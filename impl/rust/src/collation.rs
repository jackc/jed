//! Collation: a hand-written Unicode Collation Algorithm (UTS #10) — a *compiler* (a canonical
//! definition → jed's own compiled table) and an *executor* (table + string → a `memcmp`-ordered
//! sort key), plus the portable `.coll` artifact codec. The cross-core contract for linguistic
//! ordering (spec/design/collation.md §2/§6): both routines are hand-written per core (CLAUDE.md
//! §5 forbids codegenning them) and **byte-identical given identical input**, pinned by the
//! vectors in spec/collation/vectors/. The byte formats are fixed in spec/collation/README.md
//! (the definition §1, the compiled table §2, the artifact §3, the sort key §4).
//!
//! Slice 1b: host-free — `compile_collation` (DUCET `allkeys.txt` root + LDML tailoring),
//! `sort_key` (the executor), and `save_collation`/`open_collation` (the artifact round-trip).
//! No SQL surface, no persistence, no host seam (`ExtractHostCollation`) — those land in later
//! slices (collation.md §14). Only deterministic collations and `non-ignorable` variable
//! weighting in this slice (§6).

use crate::error::{EngineError, Result, SqlState};
use crate::format::crc32_ieee;
use crate::lz4;

/// One collation element — a weight triple plus a flags byte. 7 bytes on disk
/// (spec/collation/README.md §2): `u8 flags`, `u16 l1`, `u16 l2`, `u16 l3` (big-endian).
/// A `0x0000` weight is *ignorable* at that level (skipped in the sort key, §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ce {
    /// bit0 = variable (space/punctuation); bits 1-7 reserved (0). Treated `non-ignorable`
    /// in slice 1 (the bit is recorded but not acted on until the `shifted` refinement, §6).
    pub flags: u8,
    pub l1: u16,
    pub l2: u16,
    pub l3: u16,
}

/// `flags` bit0 — a variable collation element (DUCET marker `*`).
pub const CE_VARIABLE: u8 = 0x01;

impl Ce {
    fn new(l1: u16, l2: u16, l3: u16) -> Ce {
        Ce {
            flags: 0,
            l1,
            l2,
            l3,
        }
    }
}

/// A compiled, fully-resolved collation: jed's own table plus its metadata. Database-independent
/// (the `Collation` value of collation.md §4). The arrays are kept sorted (the §2 contract) so the
/// serialized bytes are deterministic and no iteration order can leak (CLAUDE.md §8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collation {
    pub name: String,
    /// from the definition's `@version` record (`""` if none).
    pub unicode_version: String,
    /// CLDR version (`""` — root-only / unset in slice 1b).
    pub cldr_version: String,
    /// optional human-readable provenance; **excluded from the content hash** (§3). Only
    /// `ExtractHostCollation` (a later slice) generates one; `CompileCollation` leaves it `""`.
    pub description: String,
    /// single-code-point mappings, **ascending by code point**.
    pub singles: Vec<(u32, Vec<Ce>)>,
    /// multi-code-point (contraction) mappings, **lexicographic by the code-point sequence**.
    pub contractions: Vec<(Vec<u32>, Vec<Ce>)>,
}

// --- the dev tailoring weight allocator (spec/collation/README.md §1.2) ----------------------
// Fixed constants so every core allocates identical weights from identical rules. A real,
// ICU-faithful allocator (with weight-byte expansion for dense insertions) replaces this with the
// version-pinned DUCET follow-on (collation.md §14); for the dev slice these gaps suffice.
const BASE_L2: u16 = 0x0020; // a fresh element's secondary weight
const BASE_L3: u16 = 0x0002; // a fresh element's tertiary weight (lowercase base)
const PRIMARY_GAP: u16 = 0x0200; // appended after the current max primary
const SECONDARY_GAP: u16 = 0x0020;
const TERTIARY_GAP: u16 = 0x0006; // 0x0002 + 0x0006 = 0x0008, the dev uppercase tertiary

fn feature(msg: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::FeatureNotSupported, msg)
}
fn syntax(msg: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::SyntaxError, msg)
}
fn corrupt(msg: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::DataCorrupted, msg)
}

// ============================================================================================
// Compiler: a canonical definition (DUCET allkeys root + LDML tailorings) → a Collation.
// ============================================================================================

/// Parse a collation **definition** (spec/collation/README.md §1) and compile it into a jed table.
/// The definition is a single stream, line-dispatched: `@…` records, allkeys mapping lines
/// (`codepoints ; elements`), and LDML rule lines (`&anchor < x …`). Deterministic and host-free.
pub fn compile_collation(name: &str, definition: &str) -> Result<Collation> {
    let mut unicode_version = String::new();
    let cldr_version = String::new();
    // working map: code-point sequence → CEs. A single code point has len-1 key.
    let mut map: Vec<(Vec<u32>, Vec<Ce>)> = Vec::new();

    for raw in definition.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('@') {
            // records: only @version is consumed; @implicitweights et al. are deferred (§1.1).
            let mut it = rest.split_whitespace();
            if let Some(rec) = it.next() {
                if rec == "version" {
                    unicode_version = it.next().unwrap_or("").to_string();
                }
                // other records: ignored in slice 1b.
            }
            continue;
        }
        if line.starts_with('&') {
            apply_tailoring(&mut map, line)?;
            continue;
        }
        // an allkeys mapping line: `codepoints ; elements`.
        parse_mapping(&mut map, line)?;
    }

    // split into singles + contractions, each sorted (the §2 contract).
    let mut singles: Vec<(u32, Vec<Ce>)> = Vec::new();
    let mut contractions: Vec<(Vec<u32>, Vec<Ce>)> = Vec::new();
    for (seq, ces) in map {
        if seq.len() == 1 {
            singles.push((seq[0], ces));
        } else {
            contractions.push((seq, ces));
        }
    }
    singles.sort_by_key(|(cp, _)| *cp);
    contractions.sort_by(|a, b| a.0.cmp(&b.0));

    Ok(Collation {
        name: name.to_string(),
        unicode_version,
        cldr_version,
        description: String::new(),
        singles,
        contractions,
    })
}

/// Strip a trailing `# comment` (the whole rest of the line). Definitions have no string literals,
/// so a bare `#` always starts a comment.
fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    }
}

fn parse_mapping(map: &mut Vec<(Vec<u32>, Vec<Ce>)>, line: &str) -> Result<()> {
    let (cps_part, ces_part) = line
        .split_once(';')
        .ok_or_else(|| syntax(format!("collation: mapping line has no ';': {line}")))?;
    let mut seq = Vec::new();
    for tok in cps_part.split_whitespace() {
        seq.push(parse_hex(tok)?);
    }
    if seq.is_empty() {
        return Err(syntax(format!(
            "collation: mapping with no code point: {line}"
        )));
    }
    let ces = parse_elements(ces_part.trim())?;
    if ces.is_empty() {
        return Err(syntax(format!(
            "collation: mapping with no element: {line}"
        )));
    }
    set_mapping(map, seq, ces, false)?;
    Ok(())
}

/// Parse `[*0209.0020.0002][.0000.0047.0002]…` into collation elements.
fn parse_elements(s: &str) -> Result<Vec<Ce>> {
    let mut ces = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if bytes[i] != b'[' {
            return Err(syntax(format!("collation: expected '[' in elements: {s}")));
        }
        let end = s[i..]
            .find(']')
            .ok_or_else(|| syntax(format!("collation: unterminated element: {s}")))?
            + i;
        let inner = &s[i + 1..end];
        let (marker, rest) =
            inner.split_at(inner.char_indices().nth(1).map(|(p, _)| p).unwrap_or(0));
        let mut flags = 0u8;
        match marker {
            "." => {}
            "*" => flags |= CE_VARIABLE,
            _ => return Err(syntax(format!("collation: bad element marker: {inner}"))),
        }
        let mut parts = rest.split('.');
        let l1 = parse_hex16(parts.next().unwrap_or(""))?;
        let l2 = parse_hex16(parts.next().unwrap_or(""))?;
        let l3 = parse_hex16(parts.next().unwrap_or(""))?;
        // DUCET's optional 4th (quaternary) weight is ignored (§1.1).
        ces.push(Ce { flags, l1, l2, l3 });
        i = end + 1;
    }
    Ok(ces)
}

/// Insert or replace a mapping. `replace` distinguishes a tailoring redefinition (allowed) from a
/// duplicate in the root (an error).
fn set_mapping(
    map: &mut Vec<(Vec<u32>, Vec<Ce>)>,
    seq: Vec<u32>,
    ces: Vec<Ce>,
    replace: bool,
) -> Result<()> {
    if let Some(slot) = map.iter_mut().find(|(s, _)| *s == seq) {
        if !replace {
            return Err(syntax(format!("collation: duplicate mapping for {seq:?}")));
        }
        slot.1 = ces;
    } else {
        map.push((seq, ces));
    }
    Ok(())
}

// --- LDML tailoring ---------------------------------------------------------------------------

/// Apply one LDML rule line: `&anchor REL target (REL target)*` where REL ∈ `<` `<<` `<<<` `=`.
/// Single-character anchor/targets only in slice 1b (multi-char contractions in rules deferred).
fn apply_tailoring(map: &mut Vec<(Vec<u32>, Vec<Ce>)>, line: &str) -> Result<()> {
    let body = line.strip_prefix('&').unwrap().trim();
    let toks = tokenize_rule(body)?;
    // toks alternate: char, op, char, op, char, … — first is the anchor.
    let mut it = toks.into_iter();
    let anchor = match it.next() {
        Some(Tok::Char(cp)) => cp,
        _ => {
            return Err(syntax(format!(
                "collation: rule must start with an anchor: {line}"
            )));
        }
    };
    let mut cur = single_ce(map, anchor).ok_or_else(|| {
        syntax(format!(
            "collation: rule anchor U+{anchor:04X} not a single element"
        ))
    })?;
    loop {
        let op = match it.next() {
            None => break,
            Some(Tok::Op(o)) => o,
            Some(Tok::Char(_)) => {
                return Err(syntax(format!(
                    "collation: expected a relation operator: {line}"
                )));
            }
        };
        let target = match it.next() {
            Some(Tok::Char(cp)) => cp,
            _ => {
                return Err(syntax(format!(
                    "collation: relation needs a target: {line}"
                )));
            }
        };
        let ce = alloc_after(map, cur, op)?;
        set_mapping(map, vec![target], vec![ce], true)?;
        cur = ce;
    }
    Ok(())
}

enum Tok {
    Char(u32),
    Op(Rel),
}
#[derive(Clone, Copy)]
enum Rel {
    Primary,
    Secondary,
    Tertiary,
    Identical,
}

fn tokenize_rule(s: &str) -> Result<Vec<Tok>> {
    let mut out = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '<' {
            let mut n = 0;
            while i < chars.len() && chars[i] == '<' {
                n += 1;
                i += 1;
            }
            out.push(Tok::Op(match n {
                1 => Rel::Primary,
                2 => Rel::Secondary,
                3 => Rel::Tertiary,
                _ => return Err(syntax("collation: '<<<<' (quaternary) not supported")),
            }));
        } else if c == '=' {
            out.push(Tok::Op(Rel::Identical));
            i += 1;
        } else {
            // a target/anchor character (single code point in slice 1b).
            out.push(Tok::Char(c as u32));
            i += 1;
        }
    }
    Ok(out)
}

/// The CE of a single-code-point mapping with exactly one element (a tailoring anchor must be one).
fn single_ce(map: &[(Vec<u32>, Vec<Ce>)], cp: u32) -> Option<Ce> {
    map.iter()
        .find(|(s, _)| s.as_slice() == [cp])
        .and_then(|(_, ces)| if ces.len() == 1 { Some(ces[0]) } else { None })
}

/// Allocate a fresh CE placed *after* `cur` at the given relation level, using the dev allocator.
fn alloc_after(map: &[(Vec<u32>, Vec<Ce>)], cur: Ce, rel: Rel) -> Result<Ce> {
    match rel {
        Rel::Identical => Ok(cur),
        Rel::Primary => {
            let succ = min_weight_above(map, |ce| if ce.l1 > cur.l1 { Some(ce.l1) } else { None });
            let l1 = fresh(cur.l1, succ)?;
            Ok(Ce::new(l1, BASE_L2, BASE_L3))
        }
        Rel::Secondary => {
            let succ = min_weight_above(map, |ce| {
                if ce.l1 == cur.l1 && ce.l2 > cur.l2 {
                    Some(ce.l2)
                } else {
                    None
                }
            });
            let l2 = fresh_gap(cur.l2, succ, SECONDARY_GAP)?;
            Ok(Ce::new(cur.l1, l2, BASE_L3))
        }
        Rel::Tertiary => {
            let succ = min_weight_above(map, |ce| {
                if ce.l1 == cur.l1 && ce.l2 == cur.l2 && ce.l3 > cur.l3 {
                    Some(ce.l3)
                } else {
                    None
                }
            });
            let l3 = fresh_gap(cur.l3, succ, TERTIARY_GAP)?;
            Ok(Ce::new(cur.l1, cur.l2, l3))
        }
    }
}

/// The smallest weight strictly above `cur`, per `f`, across every CE in the table.
fn min_weight_above(map: &[(Vec<u32>, Vec<Ce>)], f: impl Fn(&Ce) -> Option<u16>) -> Option<u16> {
    let mut best: Option<u16> = None;
    for (_, ces) in map {
        for ce in ces {
            if let Some(w) = f(ce) {
                best = Some(best.map_or(w, |b| b.min(w)));
            }
        }
    }
    best
}

/// A fresh primary weight after `lo`: the midpoint to `succ` if one exists (needs room ≥ 2),
/// else `lo + PRIMARY_GAP` (append).
fn fresh(lo: u16, succ: Option<u16>) -> Result<u16> {
    fresh_gap(lo, succ, PRIMARY_GAP)
}

fn fresh_gap(lo: u16, succ: Option<u16>, gap: u16) -> Result<u16> {
    match succ {
        Some(hi) => {
            if hi.saturating_sub(lo) < 2 {
                return Err(feature(
                    "collation: tailoring weight space exhausted (dense-insertion allocator deferred)",
                ));
            }
            Ok(lo + (hi - lo) / 2)
        }
        None => lo
            .checked_add(gap)
            .ok_or_else(|| feature("collation: tailoring weight overflow (allocator deferred)")),
    }
}

fn parse_hex(s: &str) -> Result<u32> {
    u32::from_str_radix(s.trim(), 16)
        .map_err(|_| syntax(format!("collation: bad code point hex: {s:?}")))
}
fn parse_hex16(s: &str) -> Result<u16> {
    u16::from_str_radix(s.trim(), 16)
        .map_err(|_| syntax(format!("collation: bad weight hex: {s:?}")))
}

// ============================================================================================
// Executor: (Collation, &str) → sort key (spec/collation/README.md §4).
// ============================================================================================

/// The sort key: the byte string whose `memcmp` order equals the collation's logical order.
/// `L1-weights ‖ 0x0000 ‖ L2-weights ‖ 0x0000 ‖ L3-weights ‖ 0x0000 ‖ Ckey(original)`.
pub fn sort_key(coll: &Collation, s: &str) -> Result<Vec<u8>> {
    let cps: Vec<u32> = s.chars().map(|c| c as u32).collect();
    let ces = collation_elements(coll, &cps)?;

    let mut key = Vec::new();
    for ce in &ces {
        if ce.l1 != 0 {
            key.extend_from_slice(&ce.l1.to_be_bytes());
        }
    }
    key.extend_from_slice(&[0, 0]);
    for ce in &ces {
        if ce.l2 != 0 {
            key.extend_from_slice(&ce.l2.to_be_bytes());
        }
    }
    key.extend_from_slice(&[0, 0]);
    for ce in &ces {
        if ce.l3 != 0 {
            key.extend_from_slice(&ce.l3.to_be_bytes());
        }
    }
    key.extend_from_slice(&[0, 0]);
    // identical level: the §2.4 C-key of the original UTF-8 string.
    key.extend_from_slice(&crate::encoding::encode_terminated(s.as_bytes()));
    Ok(key)
}

/// Walk the code points, taking the longest matching contraction else the single mapping at each
/// position, concatenating each match's collation elements.
fn collation_elements(coll: &Collation, cps: &[u32]) -> Result<Vec<Ce>> {
    let max_contraction = coll
        .contractions
        .iter()
        .map(|(s, _)| s.len())
        .max()
        .unwrap_or(0);
    let mut out = Vec::new();
    let mut i = 0;
    while i < cps.len() {
        let mut matched = None;
        let mut clen = max_contraction.min(cps.len() - i);
        while clen >= 2 {
            if let Some(ces) = lookup_contraction(coll, &cps[i..i + clen]) {
                matched = Some((clen, ces));
                break;
            }
            clen -= 1;
        }
        if let Some((len, ces)) = matched {
            out.extend_from_slice(ces);
            i += len;
        } else if let Some(ces) = lookup_single(coll, cps[i]) {
            out.extend_from_slice(ces);
            i += 1;
        } else {
            return Err(feature(format!(
                "collation: code point U+{:04X} has no mapping (implicit weights deferred)",
                cps[i]
            )));
        }
    }
    Ok(out)
}

fn lookup_single(coll: &Collation, cp: u32) -> Option<&[Ce]> {
    coll.singles
        .binary_search_by_key(&cp, |(c, _)| *c)
        .ok()
        .map(|idx| coll.singles[idx].1.as_slice())
}

fn lookup_contraction<'a>(coll: &'a Collation, seq: &[u32]) -> Option<&'a [Ce]> {
    coll.contractions
        .binary_search_by(|(s, _)| s.as_slice().cmp(seq))
        .ok()
        .map(|idx| coll.contractions[idx].1.as_slice())
}

// ============================================================================================
// Compiled-table bytes (spec/collation/README.md §2) and the `.coll` artifact (§3).
// ============================================================================================

/// Serialize the compiled table (§2) — the bytes the content hash covers and the artifact carries.
pub fn serialize_table(coll: &Collation) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(1u8); // layout_version
    out.extend_from_slice(&(coll.singles.len() as u32).to_be_bytes());
    out.extend_from_slice(&(coll.contractions.len() as u32).to_be_bytes());
    for (cp, ces) in &coll.singles {
        out.extend_from_slice(&cp.to_be_bytes());
        out.push(ces.len() as u8);
        for ce in ces {
            push_ce(&mut out, ce);
        }
    }
    for (seq, ces) in &coll.contractions {
        out.push(seq.len() as u8);
        for cp in seq {
            out.extend_from_slice(&cp.to_be_bytes());
        }
        out.push(ces.len() as u8);
        for ce in ces {
            push_ce(&mut out, ce);
        }
    }
    out
}

fn push_ce(out: &mut Vec<u8>, ce: &Ce) {
    out.push(ce.flags);
    out.extend_from_slice(&ce.l1.to_be_bytes());
    out.extend_from_slice(&ce.l2.to_be_bytes());
    out.extend_from_slice(&ce.l3.to_be_bytes());
}

/// The portable `.coll` artifact (§3): magic + metadata + provenance + CRC-32 + the
/// LZ4-compressed table. `open_collation` is its exact inverse; the round-trip is byte-identical
/// on every core (collation.md §10).
pub fn save_collation(coll: &Collation) -> Vec<u8> {
    let table = serialize_table(coll);
    let hash = crc32_ieee(&table);
    let comp = lz4::compress(&table);

    let mut out = Vec::new();
    out.extend_from_slice(b"JCOLL\0"); // 6-byte magic (4A 43 4F 4C 4C 00)
    out.extend_from_slice(&1u16.to_be_bytes()); // format_version
    push_str(&mut out, &coll.name);
    push_str(&mut out, &coll.unicode_version);
    push_str(&mut out, &coll.cldr_version);
    push_str(&mut out, &coll.description);
    out.extend_from_slice(&hash.to_be_bytes());
    out.extend_from_slice(&(table.len() as u32).to_be_bytes());
    out.extend_from_slice(&(comp.len() as u32).to_be_bytes());
    out.extend_from_slice(&comp);
    out
}

/// Read a `.coll` artifact (§3) back into a `Collation`. Verifies the magic, the format version,
/// and the content hash; a malformed or tampered artifact is `XX001` (data_corrupted).
pub fn open_collation(bytes: &[u8]) -> Result<Collation> {
    let mut r = Reader { b: bytes, i: 0 };
    let magic = r.take(6)?;
    if magic != b"JCOLL\0" {
        return Err(corrupt("collation: bad artifact magic"));
    }
    let fmt = r.u16()?;
    if fmt != 1 {
        return Err(corrupt(format!(
            "collation: unsupported artifact format_version {fmt}"
        )));
    }
    let name = r.str()?;
    let unicode_version = r.str()?;
    let cldr_version = r.str()?;
    let description = r.str()?;
    let hash = r.u32()?;
    let raw_len = r.u32()? as usize;
    let comp_len = r.u32()? as usize;
    let comp = r.take(comp_len)?;
    if r.i != r.b.len() {
        return Err(corrupt("collation: trailing bytes after artifact"));
    }
    let table = lz4::decompress(comp, raw_len)?;
    if crc32_ieee(&table) != hash {
        return Err(corrupt("collation: artifact content hash mismatch"));
    }
    let (singles, contractions) = deserialize_table(&table)?;
    Ok(Collation {
        name,
        unicode_version,
        cldr_version,
        description,
        singles,
        contractions,
    })
}

// --- Vendored collation set (spec/design/collation.md §2/§9) ------------------------------------
//
// In the reference-only model the engine reads collations from a set **vendored into the binary**,
// not from the database file (the file only *references* a collation by name + version). Production
// `OpenCollation`s these embedded `.coll` artifacts once at startup and serves every later use from
// them. This is the dev fixture set (`dev-root`, `dev-nordic`); the real version-pinned DUCET +
// curated tailorings and the embedder-chosen footprint tiers (§13) are later slices (§14, 2a/2f).
// The `.coll` bytes are the same artifact `gen_collation_vectors` writes and `db.SaveCollation`
// produces — byte-identical across cores, so every core vendors the identical table (§9/§10).

/// The `(name, .coll bytes)` pairs compiled into this binary. The artifact's own embedded name is
/// authoritative for the registry key (it always equals the label here).
const VENDORED_COLL: &[(&str, &[u8])] = &[
    (
        "dev-root",
        include_bytes!("../../../spec/collation/fixtures/dev-root.coll"),
    ),
    (
        "dev-nordic",
        include_bytes!("../../../spec/collation/fixtures/dev-nordic.coll"),
    ),
];

static VENDORED: std::sync::OnceLock<std::collections::HashMap<String, std::sync::Arc<Collation>>> =
    std::sync::OnceLock::new();

fn vendored() -> &'static std::collections::HashMap<String, std::sync::Arc<Collation>> {
    VENDORED.get_or_init(|| {
        let mut m = std::collections::HashMap::new();
        for (label, bytes) in VENDORED_COLL {
            let coll =
                open_collation(bytes).unwrap_or_else(|e| panic!("vendored collation {label}: {e}"));
            m.insert(coll.name.clone(), std::sync::Arc::new(coll));
        }
        m
    })
}

/// Look up a collation **vendored into this binary** by its exact (case-sensitive) name
/// (spec/design/collation.md §2/§9). `None` ⇒ not vendored. `C` is never here (table-free, built
/// in). The resolver consults the database's referenced collations first, then this set.
pub fn vendored_collation(name: &str) -> Option<std::sync::Arc<Collation>> {
    vendored().get(name).cloned()
}

/// Every vendored collation, ascending by name — a deterministic order with no hash-iteration leak
/// (CLAUDE.md §8). Used by introspection (`db.Collations`).
pub fn vendored_collations() -> Vec<std::sync::Arc<Collation>> {
    let mut v: Vec<std::sync::Arc<Collation>> = vendored().values().cloned().collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

#[allow(clippy::type_complexity)]
fn deserialize_table(table: &[u8]) -> Result<(Vec<(u32, Vec<Ce>)>, Vec<(Vec<u32>, Vec<Ce>)>)> {
    let mut r = Reader { b: table, i: 0 };
    let layout = r.u8()?;
    if layout != 1 {
        return Err(corrupt(format!(
            "collation: unsupported table layout_version {layout}"
        )));
    }
    let num_singles = r.u32()? as usize;
    let num_contractions = r.u32()? as usize;
    let mut singles = Vec::with_capacity(num_singles);
    for _ in 0..num_singles {
        let cp = r.u32()?;
        let n = r.u8()? as usize;
        let mut ces = Vec::with_capacity(n);
        for _ in 0..n {
            ces.push(r.ce()?);
        }
        singles.push((cp, ces));
    }
    let mut contractions = Vec::with_capacity(num_contractions);
    for _ in 0..num_contractions {
        let seq_len = r.u8()? as usize;
        let mut seq = Vec::with_capacity(seq_len);
        for _ in 0..seq_len {
            seq.push(r.u32()?);
        }
        let n = r.u8()? as usize;
        let mut ces = Vec::with_capacity(n);
        for _ in 0..n {
            ces.push(r.ce()?);
        }
        contractions.push((seq, ces));
    }
    if r.i != r.b.len() {
        return Err(corrupt("collation: trailing bytes after table"));
    }
    Ok((singles, contractions))
}

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
            return Err(corrupt("collation: artifact truncated"));
        }
        let s = &self.b[self.i..self.i + n];
        self.i += n;
        Ok(s)
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
    fn ce(&mut self) -> Result<Ce> {
        let flags = self.u8()?;
        let l1 = self.u16()?;
        let l2 = self.u16()?;
        let l3 = self.u16()?;
        Ok(Ce { flags, l1, l2, l3 })
    }
    fn str(&mut self) -> Result<String> {
        let n = self.u16()? as usize;
        let s = self.take(n)?;
        String::from_utf8(s.to_vec()).map_err(|_| corrupt("collation: artifact string not UTF-8"))
    }
}
