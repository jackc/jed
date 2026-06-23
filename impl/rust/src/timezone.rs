//! Time zones: the `JTZ` bundle codec + an RFC 8536 TZif reader + the engine-global loaded zone
//! set. The cross-core contract for time-zone conversion (spec/design/timezones.md §4/§5): the
//! reader is hand-written per core (CLAUDE.md §5 forbids codegenning it) and **byte-identical given
//! identical input** — but it reads the *standardized* TZif layout, so cores agree by construction
//! (§3.4). The byte formats are pinned in spec/tz/README.md (the TZif subset §2, the `JTZ` bundle
//! §3, the reader contract §4, the POSIX footer §5).
//!
//! This is the data path mirroring collation's host-load model: the bare binary carries **no** tz
//! data (`UTC` + fixed offsets excepted — built-in, table-free, the `C` analogue); a host hands the
//! engine a `JTZ` bundle's bytes via [`load_time_zone_data`] (`db.LoadTimeZoneData`) and the named
//! zones become usable. `timestamptz` is a UTC `i64`, so this adds **no** on-disk format change and
//! **no** version-skew verdict (the base type is tz-immune, §2) — the collation-style skew machinery
//! is latent until a tz-*derived* key can be stored (§8).

use crate::error::{EngineError, Result, SqlState};
use crate::format::crc32_ieee;
use crate::lz4;
use crate::timestamp::{civil_from_days, days_from_civil};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};

fn corrupt(msg: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::DataCorrupted, msg)
}

const SECS_PER_DAY: i64 = 86_400;

// ============================================================================================
// In-memory zone representation (the parsed TZif file, spec/tz/README.md §2)
// ============================================================================================

/// One local-time type (RFC 8536 §3.2): the offset to add to UT to get local time (east-positive),
/// the DST flag, and the abbreviation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalTimeType {
    pub utoff: i32,
    pub is_dst: bool,
    pub abbrev: String,
}

/// A parsed TZif file's tables: the transition table + local-time types + the optional POSIX footer
/// (§2). What the reader (§4) runs on and what the cross-core `tzif.toml` vectors pin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TzData {
    /// UT seconds of each transition, strictly ascending.
    pub trans: Vec<i64>,
    /// index into `types[]` in effect at/after `trans[i]`.
    pub trans_type: Vec<u8>,
    pub types: Vec<LocalTimeType>,
    /// the POSIX TZ string governing instants at/after the last transition (§5).
    pub footer: Option<PosixTz>,
}

/// A loaded named zone: its tables plus the bundle metadata it arrived with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Zone {
    pub name: String,
    pub tzdata_version: String,
    pub data: TzData,
}

/// The local-time type in effect at an instant (the reader output, §4). `AT TIME ZONE` uses only
/// `utoff`; `abbrev`/`is_dst` round out the reader contract and the `tzif.toml` vectors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offset {
    pub utoff: i32,
    pub abbrev: String,
    pub is_dst: bool,
}

/// A resolved zone reference: a built-in fixed offset (`UTC` / `±HH:MM`, table-free, §3.2) or a
/// loaded named zone.
#[derive(Clone, Debug)]
pub enum ZoneRef {
    Fixed(i32),
    Zone(Arc<Zone>),
}

/// Introspection row for `db.LoadedTimeZones` (timezones.md §3.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeZoneInfo {
    pub name: String,
    pub tzdata_version: String,
}

// ============================================================================================
// The POSIX TZ footer rule (spec/tz/README.md §5)
// ============================================================================================

/// A DST transition rule. The first cut supports only the near-universal `Mm.w.d` form (§5); the
/// rare `Jn` / `n` julian-day forms are a documented follow-on (timezones.md §14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PosixRule {
    /// month `m` (1–12), week `w` (1–5, 5 = "last"), day `d` (0–6, 0 = Sunday).
    MonthWeekDay { m: u8, w: u8, d: u8 },
}

/// The DST half of a POSIX TZ string: the daylight abbrev/offset (east-positive) + the start/end
/// rules with their local time-of-day (seconds, default 02:00:00).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PosixDst {
    pub abbr: String,
    pub utoff: i32,
    pub start: PosixRule,
    pub start_time: i32,
    pub end: PosixRule,
    pub end_time: i32,
}

/// A parsed POSIX TZ string (§5). Offsets are stored **east-positive** (TZif's convention), negated
/// from POSIX's west-positive source on parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PosixTz {
    pub std_abbr: String,
    pub std_utoff: i32,
    pub dst: Option<PosixDst>,
}

/// Parse a POSIX TZ string (§5): `std offset[dst[offset][,start[/time],end[/time]]]`. A malformed
/// rule is an error string (surfaced as `XX001` on bundle load — the footer is committed TZif).
fn parse_posix_tz(s: &str) -> std::result::Result<PosixTz, String> {
    let b = s.as_bytes();
    let mut i = 0usize;
    let std_abbr = parse_posix_abbr(b, &mut i).ok_or("posix: missing std abbreviation")?;
    let std_posix = parse_posix_offset(b, &mut i).ok_or("posix: missing std offset")?;
    let std_utoff = -std_posix;

    if i >= b.len() {
        return Ok(PosixTz {
            std_abbr,
            std_utoff,
            dst: None,
        });
    }

    // DST abbreviation, then an optional offset (default: one hour east of std).
    let abbr = parse_posix_abbr(b, &mut i).ok_or("posix: malformed dst abbreviation")?;
    let dst_utoff = if i < b.len() && b[i] != b',' {
        -parse_posix_offset(b, &mut i).ok_or("posix: malformed dst offset")?
    } else {
        std_utoff + 3600
    };

    if i >= b.len() || b[i] != b',' {
        return Err("posix: dst without transition rules".into());
    }
    i += 1;
    let (start, start_time) = parse_posix_rule(b, &mut i)?;
    if i >= b.len() || b[i] != b',' {
        return Err("posix: missing dst end rule".into());
    }
    i += 1;
    let (end, end_time) = parse_posix_rule(b, &mut i)?;

    Ok(PosixTz {
        std_abbr,
        std_utoff,
        dst: Some(PosixDst {
            abbr,
            utoff: dst_utoff,
            start,
            start_time,
            end,
            end_time,
        }),
    })
}

fn parse_posix_abbr(b: &[u8], i: &mut usize) -> Option<String> {
    if b.get(*i) == Some(&b'<') {
        *i += 1;
        let start = *i;
        while *i < b.len() && b[*i] != b'>' {
            *i += 1;
        }
        if *i >= b.len() {
            return None;
        }
        let s = std::str::from_utf8(&b[start..*i]).ok()?.to_string();
        *i += 1; // consume '>'
        if s.is_empty() { None } else { Some(s) }
    } else {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_alphabetic() {
            *i += 1;
        }
        if *i == start {
            None
        } else {
            Some(String::from_utf8_lossy(&b[start..*i]).into_owned())
        }
    }
}

/// Parse `[+|-]hh[:mm[:ss]]` → seconds (POSIX raw value, west-positive). Caller negates to east.
fn parse_posix_offset(b: &[u8], i: &mut usize) -> Option<i32> {
    let neg = match b.get(*i) {
        Some(b'-') => {
            *i += 1;
            true
        }
        Some(b'+') => {
            *i += 1;
            false
        }
        _ => false,
    };
    let hh = parse_uint(b, i)?;
    let mut secs = hh as i32 * 3600;
    if b.get(*i) == Some(&b':') {
        *i += 1;
        secs += parse_uint(b, i)? as i32 * 60;
        if b.get(*i) == Some(&b':') {
            *i += 1;
            secs += parse_uint(b, i)? as i32;
        }
    }
    Some(if neg { -secs } else { secs })
}

/// Parse a transition rule `Mm.w.d` with an optional `/time` (§5). The `Jn` / `n` julian forms are a
/// deferred follow-on and are an explicit error here.
fn parse_posix_rule(b: &[u8], i: &mut usize) -> std::result::Result<(PosixRule, i32), String> {
    match b.get(*i) {
        Some(b'M') => {
            *i += 1;
            let m = parse_uint(b, i).ok_or("posix: bad month")? as u8;
            if b.get(*i) != Some(&b'.') {
                return Err("posix: expected '.' after month".into());
            }
            *i += 1;
            let w = parse_uint(b, i).ok_or("posix: bad week")? as u8;
            if b.get(*i) != Some(&b'.') {
                return Err("posix: expected '.' after week".into());
            }
            *i += 1;
            let d = parse_uint(b, i).ok_or("posix: bad day")? as u8;
            if !(1..=12).contains(&m) || !(1..=5).contains(&w) || d > 6 {
                return Err("posix: Mm.w.d out of range".into());
            }
            let time = if b.get(*i) == Some(&b'/') {
                *i += 1;
                parse_posix_offset(b, i).ok_or("posix: bad transition time")?
            } else {
                7200
            };
            Ok((PosixRule::MonthWeekDay { m, w, d }, time))
        }
        Some(b'J') | Some(_) => {
            Err("posix: Jn/n julian-day transition rules are not yet supported".into())
        }
        None => Err("posix: missing transition rule".into()),
    }
}

fn parse_uint(b: &[u8], i: &mut usize) -> Option<u32> {
    let start = *i;
    let mut v: u32 = 0;
    while *i < b.len() && b[*i].is_ascii_digit() {
        v = v.checked_mul(10)?.checked_add((b[*i] - b'0') as u32)?;
        *i += 1;
    }
    if *i == start { None } else { Some(v) }
}

// ============================================================================================
// The reader (spec/tz/README.md §4)
// ============================================================================================

/// The day-of-month of the `w`-th weekday `d` (0=Sun) of month `m` in `year` (`w==5` = last).
fn mwd_day(year: i64, m: u8, w: u8, d: u8) -> i64 {
    let m = m as i64;
    let first = days_from_civil(year, m, 1);
    let first_dow = (first + 4).rem_euclid(7); // 0 = Sunday (1970-01-01 was Thursday)
    let offset = (d as i64 - first_dow).rem_euclid(7);
    let mut day = 1 + offset + (w as i64 - 1) * 7;
    let next = if m == 12 {
        days_from_civil(year + 1, 1, 1)
    } else {
        days_from_civil(year, m + 1, 1)
    };
    let dim = next - first;
    if day > dim {
        day -= 7;
    }
    day
}

/// UT seconds of a footer transition: the `rule` at `local_time` in `year`, interpreted in local
/// time using `utoff_before` (the offset in effect just before the transition).
fn rule_instant(rule: &PosixRule, local_time: i32, year: i64, utoff_before: i32) -> i64 {
    let PosixRule::MonthWeekDay { m, w, d } = rule;
    let day = mwd_day(year, *m, *w, *d);
    let local_epoch = days_from_civil(year, *m as i64, day) * SECS_PER_DAY + local_time as i64;
    local_epoch - utoff_before as i64
}

/// Evaluate the POSIX footer (§5) at `instant_secs`.
fn eval_posix(tz: &PosixTz, instant_secs: i64) -> Offset {
    let dst = match &tz.dst {
        None => {
            return Offset {
                utoff: tz.std_utoff,
                abbrev: tz.std_abbr.clone(),
                is_dst: false,
            };
        }
        Some(d) => d,
    };
    let (year, _, _) = civil_from_days(instant_secs.div_euclid(SECS_PER_DAY));
    let start_ut = rule_instant(&dst.start, dst.start_time, year, tz.std_utoff);
    let end_ut = rule_instant(&dst.end, dst.end_time, year, dst.utoff);
    let in_dst = if start_ut < end_ut {
        instant_secs >= start_ut && instant_secs < end_ut
    } else {
        // Southern hemisphere: the DST interval wraps the year boundary.
        instant_secs >= start_ut || instant_secs < end_ut
    };
    if in_dst {
        Offset {
            utoff: dst.utoff,
            abbrev: dst.abbr.clone(),
            is_dst: true,
        }
    } else {
        Offset {
            utoff: tz.std_utoff,
            abbrev: tz.std_abbr.clone(),
            is_dst: false,
        }
    }
}

fn first_std_offset(data: &TzData) -> Offset {
    let ty = data
        .types
        .iter()
        .find(|t| !t.is_dst)
        .or_else(|| data.types.first());
    match ty {
        Some(t) => Offset {
            utoff: t.utoff,
            abbrev: t.abbrev.clone(),
            is_dst: t.is_dst,
        },
        None => Offset {
            utoff: 0,
            abbrev: String::new(),
            is_dst: false,
        },
    }
}

/// The reader (§4): the local-time type in effect at `instant_secs` (UT seconds). Pure and total.
pub fn offset_at(data: &TzData, instant_secs: i64) -> Offset {
    let n = data.trans.len();
    if n == 0 {
        return match &data.footer {
            Some(f) => eval_posix(f, instant_secs),
            None => first_std_offset(data),
        };
    }
    if instant_secs < data.trans[0] {
        return first_std_offset(data);
    }
    if let Some(f) = &data.footer {
        if instant_secs >= data.trans[n - 1] {
            return eval_posix(f, instant_secs);
        }
    }
    // largest i with trans[i] <= instant_secs
    let i = data.trans.partition_point(|&t| t <= instant_secs) - 1;
    let ty = &data.types[data.trans_type[i] as usize];
    Offset {
        utoff: ty.utoff,
        abbrev: ty.abbrev.clone(),
        is_dst: ty.is_dst,
    }
}

// ============================================================================================
// TZif parsing (RFC 8536 / spec/tz/README.md §2)
// ============================================================================================

/// The six RFC 8536 header counts, in file order.
struct Counts {
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

fn read_header(r: &mut Reader) -> Result<(u8, Counts)> {
    if r.take(4)? != b"TZif" {
        return Err(corrupt("tzif: bad magic"));
    }
    let version = r.u8()?;
    r.skip(15)?;
    let isutcnt = r.u32()? as usize;
    let isstdcnt = r.u32()? as usize;
    let leapcnt = r.u32()? as usize;
    let timecnt = r.u32()? as usize;
    let typecnt = r.u32()? as usize;
    let charcnt = r.u32()? as usize;
    Ok((
        version,
        Counts {
            isutcnt,
            isstdcnt,
            leapcnt,
            timecnt,
            typecnt,
            charcnt,
        },
    ))
}

/// Byte size of a data block for the given counts and transition-time width (4 or 8).
fn block_size(c: &Counts, time_size: usize) -> usize {
    c.timecnt * time_size
        + c.timecnt
        + c.typecnt * 6
        + c.charcnt
        + c.leapcnt * (time_size + 4)
        + c.isstdcnt
        + c.isutcnt
}

fn abbrev_at(desig: &[u8], idx: u8) -> Result<String> {
    let start = idx as usize;
    if start > desig.len() {
        return Err(corrupt("tzif: designation index out of range"));
    }
    let end = desig[start..]
        .iter()
        .position(|&b| b == 0)
        .map(|p| start + p)
        .unwrap_or(desig.len());
    Ok(String::from_utf8_lossy(&desig[start..end]).into_owned())
}

/// Read one data block (no footer). `time_size` is 4 (v1) or 8 (v2+).
fn read_block(r: &mut Reader, time_size: usize, c: &Counts) -> Result<TzData> {
    let mut trans = Vec::with_capacity(c.timecnt);
    for _ in 0..c.timecnt {
        trans.push(if time_size == 8 {
            r.i64()?
        } else {
            r.i32()? as i64
        });
    }
    let mut trans_type = Vec::with_capacity(c.timecnt);
    for _ in 0..c.timecnt {
        trans_type.push(r.u8()?);
    }
    let mut raw_types = Vec::with_capacity(c.typecnt);
    for _ in 0..c.typecnt {
        let utoff = r.i32()?;
        let is_dst = r.u8()? != 0;
        let idx = r.u8()?;
        raw_types.push((utoff, is_dst, idx));
    }
    let desig = r.take(c.charcnt)?.to_vec();
    // leap seconds (occ width = time_size, corr = 4) + std/wall + ut/local indicators — skipped (§2).
    r.skip(c.leapcnt * (time_size + 4))?;
    r.skip(c.isstdcnt)?;
    r.skip(c.isutcnt)?;

    let mut types = Vec::with_capacity(c.typecnt);
    for (utoff, is_dst, idx) in raw_types {
        types.push(LocalTimeType {
            utoff,
            is_dst,
            abbrev: abbrev_at(&desig, idx)?,
        });
    }
    if types.is_empty() {
        return Err(corrupt("tzif: no local time types"));
    }
    for &t in &trans_type {
        if t as usize >= types.len() {
            return Err(corrupt("tzif: transition type index out of range"));
        }
    }
    Ok(TzData {
        trans,
        trans_type,
        types,
        footer: None,
    })
}

/// Parse a TZif file (§2): a v1 block is read directly; a v2+ file skips the v1 block, reads the
/// 64-bit block, then the POSIX footer. A malformed file is `XX001` (data_corrupted).
pub fn parse_tzif(bytes: &[u8]) -> Result<TzData> {
    let mut r = Reader { b: bytes, i: 0 };
    let (version, c1) = read_header(&mut r)?;
    if version == 0 {
        // v1-only: a single 32-bit block, no footer.
        return read_block(&mut r, 4, &c1);
    }
    // v2+: skip the legacy 32-bit block, re-read the header, read the 64-bit block + footer.
    r.skip(block_size(&c1, 4))?;
    let (_v2, c2) = read_header(&mut r)?;
    let mut data = read_block(&mut r, 8, &c2)?;
    data.footer = read_footer(&mut r)?;
    Ok(data)
}

/// The v2+ footer (§2.3): a newline, the POSIX TZ string, a newline. Empty ⇒ no rule.
fn read_footer(r: &mut Reader) -> Result<Option<PosixTz>> {
    let rest = &r.b[r.i.min(r.b.len())..];
    let s = match std::str::from_utf8(rest) {
        Ok(s) => s.trim_matches('\n'),
        Err(_) => return Err(corrupt("tzif: non-UTF-8 footer")),
    };
    if s.is_empty() {
        Ok(None)
    } else {
        parse_posix_tz(s).map(Some).map_err(corrupt)
    }
}

// ============================================================================================
// The JTZ bundle codec (spec/tz/README.md §3)
// ============================================================================================

/// A parsed `JTZ` bundle (README §3): the tzdata version axis, the zone TZif sections, and the
/// alias links.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TzBundle {
    pub tzdata_version: String,
    pub description: String,
    /// (zone name, raw TZif bytes), ascending by name.
    pub zones: Vec<(String, Vec<u8>)>,
    /// (alias, target), ascending by alias.
    pub links: Vec<(String, String)>,
}

const TZ_BUNDLE_MAGIC: &[u8; 6] = b"JTZ\0\0\0";

/// Serialize a `JTZ` bundle (README §3): header, the zone manifest (a table of contents with
/// per-section offsets) + the link section, the LZ4-compressed TZif bodies, a trailing CRC.
pub fn save_bundle(b: &TzBundle) -> Vec<u8> {
    struct Packed {
        name: String,
        hash: u32,
        raw_len: u32,
        comp: Vec<u8>,
    }
    let packed: Vec<Packed> = b
        .zones
        .iter()
        .map(|(name, raw)| Packed {
            name: name.clone(),
            hash: crc32_ieee(raw),
            raw_len: raw.len() as u32,
            comp: lz4::compress(raw),
        })
        .collect();

    let mut header = Vec::new();
    header.extend_from_slice(TZ_BUNDLE_MAGIC);
    header.extend_from_slice(&1u16.to_be_bytes()); // format_version
    push_str(&mut header, &b.tzdata_version);
    push_str(&mut header, &b.description);
    header.extend_from_slice(&(packed.len() as u16).to_be_bytes());
    header.extend_from_slice(&(b.links.len() as u16).to_be_bytes());

    // Per zone entry: name(2+len) + hash(4) + raw_len(4) + comp_len(4) + offset(4).
    let zone_manifest_len: usize = packed.iter().map(|p| 2 + p.name.len() + 16).sum();
    let link_len: usize = b.links.iter().map(|(a, t)| 2 + a.len() + 2 + t.len()).sum();
    let body_start = header.len() + zone_manifest_len + link_len;

    let mut manifest = Vec::with_capacity(zone_manifest_len + link_len);
    let mut off = body_start;
    for p in &packed {
        push_str(&mut manifest, &p.name);
        manifest.extend_from_slice(&p.hash.to_be_bytes());
        manifest.extend_from_slice(&p.raw_len.to_be_bytes());
        manifest.extend_from_slice(&(p.comp.len() as u32).to_be_bytes());
        manifest.extend_from_slice(&(off as u32).to_be_bytes());
        off += p.comp.len();
    }
    for (alias, target) in &b.links {
        push_str(&mut manifest, alias);
        push_str(&mut manifest, target);
    }
    debug_assert_eq!(manifest.len(), zone_manifest_len + link_len);

    let mut out = header;
    out.extend_from_slice(&manifest);
    for p in &packed {
        out.extend_from_slice(&p.comp);
    }
    let crc = crc32_ieee(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

/// Read a `JTZ` bundle (README §3). Verifies the trailing CRC, the magic, the format version, and
/// each zone section's content hash; a malformed bundle is `XX001` (data_corrupted).
pub fn open_bundle(bytes: &[u8]) -> Result<TzBundle> {
    if bytes.len() < 4 {
        return Err(corrupt("tz bundle: truncated"));
    }
    let (body, trailer) = bytes.split_at(bytes.len() - 4);
    let want = u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    if crc32_ieee(body) != want {
        return Err(corrupt("tz bundle: trailer checksum mismatch"));
    }

    let mut r = Reader { b: bytes, i: 0 };
    if r.take(6)? != TZ_BUNDLE_MAGIC {
        return Err(corrupt("tz bundle: bad magic"));
    }
    let fmt = r.u16()?;
    if fmt != 1 {
        return Err(corrupt(format!(
            "tz bundle: unsupported format_version {fmt}"
        )));
    }
    let tzdata_version = r.str()?;
    let description = r.str()?;
    let zone_count = r.u16()? as usize;
    let link_count = r.u16()? as usize;

    struct M {
        name: String,
        hash: u32,
        raw_len: usize,
        comp_len: usize,
        offset: usize,
    }
    let mut metas = Vec::with_capacity(zone_count);
    for _ in 0..zone_count {
        metas.push(M {
            name: r.str()?,
            hash: r.u32()?,
            raw_len: r.u32()? as usize,
            comp_len: r.u32()? as usize,
            offset: r.u32()? as usize,
        });
    }
    let mut links = Vec::with_capacity(link_count);
    for _ in 0..link_count {
        let alias = r.str()?;
        let target = r.str()?;
        links.push((alias, target));
    }

    let mut zones = Vec::with_capacity(zone_count);
    for m in &metas {
        if m.offset > body.len() || m.offset + m.comp_len > body.len() {
            return Err(corrupt("tz bundle: section body out of range"));
        }
        let raw = lz4::decompress(&bytes[m.offset..m.offset + m.comp_len], m.raw_len)?;
        if crc32_ieee(&raw) != m.hash {
            return Err(corrupt("tz bundle: section content hash mismatch"));
        }
        zones.push((m.name.clone(), raw));
    }
    Ok(TzBundle {
        tzdata_version,
        description,
        zones,
        links,
    })
}

// ============================================================================================
// The engine-global loaded zone set + the load seam (timezones.md §3.3)
// ============================================================================================
//
// Mirrors collation's process-global `LOADED` set: the bare binary carries no tz data, a host hands
// the engine a `JTZ` bundle's bytes via `load_time_zone_data` (`db.LoadTimeZoneData`), and the zones
// land here. Global (not per-handle) so a host may load before opening any file. `UTC` and fixed
// offsets are never here — they are built in (resolve_zone, §3.2).

static LOADED_TZ: OnceLock<RwLock<BTreeMap<String, Arc<Zone>>>> = OnceLock::new();

fn loaded_set() -> &'static RwLock<BTreeMap<String, Arc<Zone>>> {
    LOADED_TZ.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Load a `JTZ` bundle into the engine-global loaded set (§3.3/§4): parse each zone's TZif, register
/// it by name, then resolve each link alias onto its target's tables. **Additive / first-wins** — a
/// name already present is not replaced, so re-loading the same bundle is an idempotent no-op. A
/// malformed bundle (or TZif) is `XX001`. The engine primitive behind `db.LoadTimeZoneData`; may be
/// called before opening any file, reads no path, reaches no host data (§10).
pub fn load_time_zone_data(bytes: &[u8]) -> Result<()> {
    let bundle = open_bundle(bytes)?;
    let ver = &bundle.tzdata_version;

    let mut parsed: BTreeMap<String, Arc<Zone>> = BTreeMap::new();
    for (name, raw) in &bundle.zones {
        let data = parse_tzif(raw)?;
        parsed.insert(
            name.clone(),
            Arc::new(Zone {
                name: name.clone(),
                tzdata_version: ver.clone(),
                data,
            }),
        );
    }

    let mut set = loaded_set().write().expect("loaded-timezone lock poisoned");
    for (name, z) in &parsed {
        set.entry(name.clone()).or_insert_with(|| z.clone());
    }
    for (alias, target) in &bundle.links {
        if let Some(z) = parsed.get(target) {
            let aliased = Arc::new(Zone {
                name: alias.clone(),
                tzdata_version: ver.clone(),
                data: z.data.clone(),
            });
            set.entry(alias.clone()).or_insert(aliased);
        }
    }
    Ok(())
}

/// Look up a loaded named zone by exact name (`None` ⇒ no loaded bundle provides it).
pub fn loaded_zone(name: &str) -> Option<Arc<Zone>> {
    loaded_set()
        .read()
        .expect("loaded-timezone lock poisoned")
        .get(name)
        .cloned()
}

/// Introspect the engine-global loaded zone set (`db.LoadedTimeZones`, §3.3): every zone + alias a
/// loaded bundle provides, ascending by name.
pub fn loaded_time_zones() -> Vec<TimeZoneInfo> {
    loaded_set()
        .read()
        .expect("loaded-timezone lock poisoned")
        .values()
        .map(|z| TimeZoneInfo {
            name: z.name.clone(),
            tzdata_version: z.tzdata_version.clone(),
        })
        .collect()
}

/// Resolve a zone name to a built-in fixed offset (`UTC` / `±HH[:MM[:SS]]`, §3.2) or a loaded named
/// zone. `None` ⇒ unknown (the caller raises `22023`).
pub fn resolve_zone(name: &str) -> Option<ZoneRef> {
    if name == "UTC" {
        return Some(ZoneRef::Fixed(0));
    }
    if let Some(off) = parse_fixed_offset(name) {
        return Some(ZoneRef::Fixed(off));
    }
    loaded_zone(name).map(ZoneRef::Zone)
}

/// The offset in effect at `instant_secs` for a resolved zone reference.
pub fn offset_at_ref(zr: &ZoneRef, instant_secs: i64) -> Offset {
    match zr {
        ZoneRef::Fixed(off) => Offset {
            utoff: *off,
            abbrev: fixed_abbrev(*off),
            is_dst: false,
        },
        ZoneRef::Zone(z) => offset_at(&z.data, instant_secs),
    }
}

/// `timestamptz AT TIME ZONE zone` (§4): render the UTC instant (micros) as the local wall-clock
/// reading in `zone` — `local = instant + utoff`.
pub fn instant_to_local_micros(zr: &ZoneRef, instant_micros: i64) -> i64 {
    let off = offset_at_ref(zr, instant_micros.div_euclid(1_000_000));
    instant_micros + off.utoff as i64 * 1_000_000
}

/// `timestamp AT TIME ZONE zone` (§4): interpret the wall-clock reading (micros) as local in `zone`,
/// producing the UTC instant — `instant = wall − utoff`. Two-probe resolution: guess the offset from
/// the wall clock read as if UT, then re-read at the guessed instant. Correct for every unambiguous
/// time; at a DST gap/overlap the branch matches PostgreSQL (oracle-pinned, timezones.md §6).
pub fn local_to_instant_micros(zr: &ZoneRef, wall_micros: i64) -> i64 {
    let wall_secs = wall_micros.div_euclid(1_000_000);
    let off1 = offset_at_ref(zr, wall_secs).utoff as i64;
    let off2 = offset_at_ref(zr, wall_secs - off1).utoff as i64;
    wall_micros - off2 * 1_000_000
}

/// Parse a fixed numeric offset `[+|-]HH[:MM[:SS]]` (the WHOLE string). Requires a leading sign so it
/// never swallows a zone name. `None` ⇒ not a fixed offset. **POSIX sign** (positive = *west* of UTC),
/// matching PostgreSQL's `AT TIME ZONE '+05:30'` (= UTC−5:30 — the PG foot-gun, oracle-pinned), so the
/// east-positive `utoff` is the negation of the written value.
fn parse_fixed_offset(name: &str) -> Option<i32> {
    let b = name.as_bytes();
    if b.is_empty() || (b[0] != b'+' && b[0] != b'-') {
        return None;
    }
    let mut i = 0usize;
    let posix = parse_posix_offset(b, &mut i)?;
    if i != b.len() {
        return None; // trailing junk ⇒ not a clean fixed offset
    }
    Some(-posix)
}

fn fixed_abbrev(utoff: i32) -> String {
    if utoff == 0 {
        return "UTC".to_string();
    }
    let sign = if utoff < 0 { '-' } else { '+' };
    let a = utoff.abs();
    let (h, m, s) = (a / 3600, (a % 3600) / 60, a % 60);
    if s == 0 {
        format!("{sign}{h:02}:{m:02}")
    } else {
        format!("{sign}{h:02}:{m:02}:{s:02}")
    }
}

// ============================================================================================
// Low-level byte helpers (big-endian; mirrors collation.rs's private Reader)
// ============================================================================================

fn push_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.i + n > self.b.len() {
            return Err(corrupt("tz: unexpected end of input"));
        }
        let s = &self.b[self.i..self.i + n];
        self.i += n;
        Ok(s)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        let s = self.take(2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn i32(&mut self) -> Result<i32> {
        let s = self.take(4)?;
        Ok(i32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn i64(&mut self) -> Result<i64> {
        let s = self.take(8)?;
        Ok(i64::from_be_bytes([
            s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
        ]))
    }
    fn str(&mut self) -> Result<String> {
        let n = self.u16()? as usize;
        let s = self.take(n)?;
        String::from_utf8(s.to_vec()).map_err(|_| corrupt("tz: invalid UTF-8 string"))
    }
}
