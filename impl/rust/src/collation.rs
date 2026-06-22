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
    // working map: code-point sequence → CEs (a single code point is a len-1 key). A hash map keeps
    // compilation O(n) over the ~39k mappings of the real DUCET root; the output is sorted below, so
    // no iteration order leaks into the table bytes (CLAUDE.md §8).
    let mut map: std::collections::HashMap<Vec<u32>, Vec<Ce>> = std::collections::HashMap::new();

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

/// Compile the compact casing source (spec/collation/README.md §5, spec/collation/17.0.0/casing.txt)
/// into a `PropertyTable` — **build-time tooling** (the builder + the vector generator), like
/// `compile_collation`; the production cores never call it (they load the compiled property section
/// from a bundle, §4.2). Two line-dispatched sections: simple 1:1 mappings (`CP ; UPPER ; LOWER ;
/// TITLE`), then `@special` full (multi-code-point) **unconditional** mappings (`CP ; UPPER… ; LOWER…
/// ; TITLE…`). `-` is the identity mapping. Deterministic and host-free.
pub fn compile_casing(text: &str) -> Result<PropertyTable> {
    let mut simple: Vec<(u32, u32, u32, u32)> = Vec::new();
    let mut special: Vec<SpecialCasing> = Vec::new();
    let mut in_special = false;
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('@') {
            // @special switches sections; @version is the bundle header's, not the table's.
            if rest.split_whitespace().next() == Some("special") {
                in_special = true;
            }
            continue;
        }
        let mut fields = line.split(';');
        let cp = parse_hex(fields.next().unwrap_or(""))?;
        if in_special {
            let upper = casing_seq_field(fields.next(), cp)?;
            let lower = casing_seq_field(fields.next(), cp)?;
            let title = casing_seq_field(fields.next(), cp)?;
            special.push(SpecialCasing {
                cp,
                upper,
                lower,
                title,
            });
        } else {
            let upper = casing_simple_field(fields.next(), cp)?;
            let lower = casing_simple_field(fields.next(), cp)?;
            let title = casing_simple_field(fields.next(), cp)?;
            simple.push((cp, upper, lower, title));
        }
    }
    // Sort so the serialized property bytes are deterministic regardless of source order (README §5).
    simple.sort_by_key(|(cp, ..)| *cp);
    special.sort_by_key(|sc| sc.cp);
    Ok(PropertyTable { simple, special })
}

/// One simple-mapping field: `-` (or absent) is the identity (the code point itself), else a hex CP.
fn casing_simple_field(tok: Option<&str>, cp: u32) -> Result<u32> {
    match tok.map(str::trim) {
        Some("-") | None | Some("") => Ok(cp),
        Some(s) => parse_hex(s),
    }
}

/// One full-mapping field: `-` (or absent) is the identity (`[cp]`), else a space-separated CP list.
fn casing_seq_field(tok: Option<&str>, cp: u32) -> Result<Vec<u32>> {
    match tok.map(str::trim) {
        Some("-") | None | Some("") => Ok(vec![cp]),
        Some(s) => s.split_whitespace().map(parse_hex).collect(),
    }
}

/// Strip a trailing `# comment` (the whole rest of the line). Definitions have no string literals,
/// so a bare `#` always starts a comment.
fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    }
}

fn parse_mapping(map: &mut std::collections::HashMap<Vec<u32>, Vec<Ce>>, line: &str) -> Result<()> {
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
    map: &mut std::collections::HashMap<Vec<u32>, Vec<Ce>>,
    seq: Vec<u32>,
    ces: Vec<Ce>,
    replace: bool,
) -> Result<()> {
    if map.contains_key(&seq) && !replace {
        return Err(syntax(format!("collation: duplicate mapping for {seq:?}")));
    }
    map.insert(seq, ces);
    Ok(())
}

// --- LDML tailoring ---------------------------------------------------------------------------

/// Apply one LDML rule line: `&anchor REL target (REL target)*` where REL ∈ `<` `<<` `<<<` `=`.
/// Single-character anchor/targets only in slice 1b (multi-char contractions in rules deferred).
fn apply_tailoring(
    map: &mut std::collections::HashMap<Vec<u32>, Vec<Ce>>,
    line: &str,
) -> Result<()> {
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
fn single_ce(map: &std::collections::HashMap<Vec<u32>, Vec<Ce>>, cp: u32) -> Option<Ce> {
    map.get(&vec![cp])
        .and_then(|ces| if ces.len() == 1 { Some(ces[0]) } else { None })
}

/// Allocate a fresh CE placed *after* `cur` at the given relation level, using the dev allocator.
fn alloc_after(
    map: &std::collections::HashMap<Vec<u32>, Vec<Ce>>,
    cur: Ce,
    rel: Rel,
) -> Result<Ce> {
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
fn min_weight_above(
    map: &std::collections::HashMap<Vec<u32>, Vec<Ce>>,
    f: impl Fn(&Ce) -> Option<u16>,
) -> Option<u16> {
    let mut best: Option<u16> = None;
    for ces in map.values() {
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
    serialize_entries(&coll.singles, &coll.contractions)
}

/// The §2 table-entry bytes (layout_version + singles + contractions). Shared by the `.coll`
/// artifact (a full table) and the `JUCD` bundle's root / tailoring sections (a full table or a
/// sparse override — both use this layout, spec/collation/README.md §2/§5).
fn serialize_entries(singles: &[(u32, Vec<Ce>)], contractions: &[(Vec<u32>, Vec<Ce>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(1u8); // layout_version
    out.extend_from_slice(&(singles.len() as u32).to_be_bytes());
    out.extend_from_slice(&(contractions.len() as u32).to_be_bytes());
    for (cp, ces) in singles {
        out.extend_from_slice(&cp.to_be_bytes());
        out.push(ces.len() as u8);
        for ce in ces {
            push_ce(&mut out, ce);
        }
    }
    for (seq, ces) in contractions {
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

// --- The engine-global loaded collation set (spec/design/collation.md §4/§9) --------------------
//
// The bare binary carries **no** Unicode data — no embedded `.coll`, no casing tables (§9/§16, the
// SQLite model). All collations arrive at runtime: a host hands the engine a `JUCD` bundle's bytes
// via [`load_unicode_data`] (`db.LoadUnicodeData`), the engine merges its root + per-locale deltas
// (§5.1) and adds the resulting collations here. This set is **process-global** — a property of the
// running engine, not of one `Database` handle (the spec's "loaded set available to any database on
// this handle", §4.2). Global is what lets a file *referencing* a collation be opened after the
// bundle is loaded: open resolves the referenced table from here (format.rs), and `open` mints the
// handle, so the data cannot live on the handle. `C` is never here (table-free, built in).
//
// The bytes are jed's **own pinned** tables (byte-identical across cores, §9/§10), so loading
// restores no nondeterminism — a *use* stays pure regardless of where the host sourced the bytes
// (file / fetch / compiled-in asset). The real version-pinned production bundle is
// spec/collation/fixtures/unicode.jucd (`unicode` = the CLDR-DUCET root, `es` = root + the Spanish
// ñ tailoring, UCA/UCD 17.0.0 / CLDR 48); the `dev-*` fixtures are not part of it (they only drive
// the cross-core compiler/sort-key vectors).

static LOADED: std::sync::OnceLock<
    std::sync::RwLock<std::collections::BTreeMap<String, std::sync::Arc<Collation>>>,
> = std::sync::OnceLock::new();

fn loaded_set()
-> &'static std::sync::RwLock<std::collections::BTreeMap<String, std::sync::Arc<Collation>>> {
    LOADED.get_or_init(|| std::sync::RwLock::new(std::collections::BTreeMap::new()))
}

/// The engine-global Unicode property/casing table (spec/design/collation.md §16). `None` until a
/// bundle carrying a property section is loaded; thereafter the casing functions (`upper`/`lower`/
/// `ILIKE`) fold via it. **First-wins**, like the collation set. Its presence is the binary "casing
/// regime" the `C`/ASCII baseline (§16) flips on: with `None` casing is ASCII-only, table-free, and
/// version-independent.
static LOADED_PROPERTY: std::sync::OnceLock<
    std::sync::RwLock<Option<std::sync::Arc<PropertyTable>>>,
> = std::sync::OnceLock::new();

fn property_slot() -> &'static std::sync::RwLock<Option<std::sync::Arc<PropertyTable>>> {
    LOADED_PROPERTY.get_or_init(|| std::sync::RwLock::new(None))
}

/// Load a `JUCD` Unicode-data bundle into the engine-global loaded set (§4/§9): parse the bundle,
/// merge the root + each per-locale delta (§5.1), register every collation by name, and store the
/// **property/casing** section (§16). **Additive / first-wins** — a collation name already present is
/// **not** replaced (the first bundle to provide it wins; resolution is by name in load order, §4.2),
/// and a property table is only stored if none is loaded yet, so re-loading the same bundle is an
/// idempotent no-op. A malformed bundle is `XX001` (`data_corrupted`).
///
/// This is the engine primitive behind `db.LoadUnicodeData`. Because the set is process-global it
/// may be called **before** opening any file (which is required: opening a file that references a
/// collation resolves its table from this set). It is a privileged host op — the engine reads no
/// file path and reaches no host data (§11); the host sources the bytes.
pub fn load_unicode_data(bytes: &[u8]) -> Result<()> {
    let bundle = open_bundle(bytes)?;
    let (colls, property) = load_bundle(&bundle)?;
    {
        let mut set = loaded_set()
            .write()
            .expect("loaded-collation lock poisoned");
        for c in colls {
            set.entry(c.name.clone())
                .or_insert_with(|| std::sync::Arc::new(c));
        }
    }
    if let Some(p) = property {
        let mut slot = property_slot()
            .write()
            .expect("loaded-property lock poisoned");
        if slot.is_none() {
            *slot = Some(std::sync::Arc::new(p));
        }
    }
    Ok(())
}

/// The engine-global property/casing table, if a bundle providing one has been loaded (§16). `None` ⇒
/// the ASCII-casing baseline. The casing functions look this up **once per evaluation** and pass it to
/// the pure kernels below, which is what keeps the un-loaded (ASCII) regime deterministically testable.
pub fn loaded_property() -> Option<std::sync::Arc<PropertyTable>> {
    property_slot()
        .read()
        .expect("loaded-property lock poisoned")
        .clone()
}

/// Look up a collation in the engine-global **loaded** set by its exact (case-sensitive) name
/// (spec/design/collation.md §4/§9). `None` ⇒ no loaded bundle provides it. `C` is never here
/// (table-free, built in). The resolver consults the database's referenced collations first, then
/// this set.
pub fn loaded_collation(name: &str) -> Option<std::sync::Arc<Collation>> {
    loaded_set()
        .read()
        .expect("loaded-collation lock poisoned")
        .get(name)
        .cloned()
}

// ============================================================================================
// Casing kernels (spec/design/collation.md §16) — the production `upper`/`lower`/`ILIKE` folds.
//
// Each takes the resolved property table EXPLICITLY (`None` ⇒ the ASCII baseline), so the evaluator
// does the one engine-global `loaded_property()` lookup and the kernels stay pure functions — which
// is what makes the un-loaded (ASCII) regime deterministically unit-testable despite the global set.
// Cross-core byte-identical given identical input (the casing vectors pin it, §10), including the TS
// UTF-16-vs-code-point trap (an astral cased letter like Deseret U+10428).
// ============================================================================================

/// Fold a string's case. `prop = None` is the **ASCII baseline** (fold `a–z`/`A–Z`, pass every other
/// code point through — the SQLite default, version-independent). `prop = Some(table)` folds via the
/// loaded Unicode tables — full case mappings including SpecialCasing expansions (`ß`→`SS`). `upper`
/// selects the direction. Backs the `upper(text)` / `lower(text)` functions ([functions.md §9]).
pub fn fold_case(s: &str, upper: bool, prop: Option<&PropertyTable>) -> String {
    match prop {
        None => s
            .chars()
            .map(|c| {
                if upper {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect(),
        Some(p) => {
            let mut out = String::new();
            for c in s.chars() {
                let cp = c as u32;
                if let Some(sc) = prop_lookup_special(p, cp) {
                    push_mapped(&mut out, if upper { &sc.upper } else { &sc.lower }, c);
                } else if let Some((_, up, lo, _)) = prop_lookup_simple(p, cp) {
                    let m = if upper { up } else { lo };
                    out.push(char::from_u32(m).unwrap_or(c));
                } else {
                    out.push(c);
                }
            }
            out
        }
    }
}

/// Fold to lowercase for case-insensitive matching (`ILIKE`) — **simple 1:1 mappings only** (never the
/// expanding SpecialCasing forms), so every code point stays one code point and the matcher's `_` /
/// length semantics are preserved (grammar.md §22). ASCII baseline when `prop` is `None`.
pub fn fold_lower_simple(s: &str, prop: Option<&PropertyTable>) -> String {
    match prop {
        None => s.chars().map(|c| c.to_ascii_lowercase()).collect(),
        Some(p) => s
            .chars()
            .map(|c| match prop_lookup_simple(p, c as u32) {
                Some((_, _, lo, _)) => char::from_u32(lo).unwrap_or(c),
                None => c,
            })
            .collect(),
    }
}

/// Append a mapped code-point sequence as chars (a non-char-scalar value is impossible in vetted
/// Unicode casing data; fall back to the source char if one ever appears).
fn push_mapped(out: &mut String, seq: &[u32], src: char) {
    for &cp in seq {
        out.push(char::from_u32(cp).unwrap_or(src));
    }
}

fn prop_lookup_simple(p: &PropertyTable, cp: u32) -> Option<(u32, u32, u32, u32)> {
    p.simple
        .binary_search_by_key(&cp, |(c, ..)| *c)
        .ok()
        .map(|i| p.simple[i])
}

fn prop_lookup_special(p: &PropertyTable, cp: u32) -> Option<&SpecialCasing> {
    p.special
        .binary_search_by_key(&cp, |sc| sc.cp)
        .ok()
        .map(|i| &p.special[i])
}

/// Every loaded collation, ascending by name — a deterministic order with no hash-iteration leak
/// (CLAUDE.md §8; the `BTreeMap` is already key-ordered). Backs introspection (`db.LoadedCollations`).
pub fn loaded_collation_tables() -> Vec<std::sync::Arc<Collation>> {
    loaded_set()
        .read()
        .expect("loaded-collation lock poisoned")
        .values()
        .cloned()
        .collect()
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
    /// A `u8`-length-prefixed run of `u32` code points (the JUCD property section, README §5).
    fn cps(&mut self) -> Result<Vec<u32>> {
        let n = self.u8()? as usize;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(self.u32()?);
        }
        Ok(v)
    }
}

// ============================================================================================
// The JUCD Unicode-data bundle (spec/collation/README.md §5) — the host-loaded container.
//
// A manifest-indexed container of sections: the Unicode property/casing tables, the shared DUCET
// root (a full §2 table, stored once, and itself a usable collation under its name), and per-locale
// tailoring sections (sparse overrides merged onto the root at load — §5.1). The host hands these
// bytes to `db.LoadUnicodeData`; the engine never reads a file path (collation.md §4/§9/§11). Build
// is the inverse: pack a root + per-locale diffs (+ property) into a bundle. Reuses the `.coll`
// conventions verbatim — big-endian, u16-len strings, CRC-32/IEEE, LZ4 bodies.
// ============================================================================================

/// Unicode property/casing data (README §5). First cut: case mappings only (normalization is a
/// reserved later sub-table). `simple` is ascending by code point; `special` (full / SpecialCasing)
/// likewise.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PropertyTable {
    /// `(codepoint, upper, lower, title)` simple 1:1 mappings (a field equal to the code point is
    /// the identity mapping).
    pub simple: Vec<(u32, u32, u32, u32)>,
    /// Full (multi-code-point) case mappings — `ß` → `SS`, etc.
    pub special: Vec<SpecialCasing>,
}

/// One SpecialCasing (full case mapping) entry (README §5). Conditional/locale context is reserved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpecialCasing {
    pub cp: u32,
    pub upper: Vec<u32>,
    pub lower: Vec<u32>,
    pub title: Vec<u32>,
}

/// The §2 table entries (singles + contractions) — a full table for a root, a sparse override for a
/// tailoring — plus the collation `name` the manifest records.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entries {
    pub name: String,
    pub singles: Vec<(u32, Vec<Ce>)>,
    pub contractions: Vec<(Vec<u32>, Vec<Ce>)>,
}

/// One bundle section (README §5): the property tables, the shared root, or a per-locale override.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Section {
    /// `kind 0` — Unicode property/casing tables (no collation name).
    Property(PropertyTable),
    /// `kind 1` — the shared DUCET root: a full §2 table and a usable collation under its name.
    Root(Entries),
    /// `kind 2` — a per-locale sparse override against the root, merged at load (§5.1).
    Tailoring(Entries),
}

/// A parsed `JUCD` bundle (README §5): the shared header version axis + its sections.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bundle {
    pub unicode_version: String,
    pub cldr_version: String,
    pub description: String,
    pub sections: Vec<Section>,
}

const BUNDLE_MAGIC: &[u8; 6] = b"JUCD\0\0";

/// Serialize a `JUCD` bundle (README §5): header, manifest (a table of contents with per-section
/// offsets), the LZ4-compressed section bodies, and a trailing CRC-32 over everything before it.
pub fn save_bundle(b: &Bundle) -> Vec<u8> {
    struct Packed {
        kind: u8,
        name: String,
        hash: u32,
        raw_len: u32,
        comp: Vec<u8>,
    }
    let packed: Vec<Packed> = b
        .sections
        .iter()
        .map(|s| {
            let (kind, name, raw) = match s {
                Section::Property(p) => (0u8, String::new(), serialize_property(p)),
                Section::Root(e) => (
                    1u8,
                    e.name.clone(),
                    serialize_entries(&e.singles, &e.contractions),
                ),
                Section::Tailoring(e) => (
                    2u8,
                    e.name.clone(),
                    serialize_entries(&e.singles, &e.contractions),
                ),
            };
            Packed {
                kind,
                name,
                hash: crc32_ieee(&raw),
                raw_len: raw.len() as u32,
                comp: lz4::compress(&raw),
            }
        })
        .collect();

    // Header.
    let mut header = Vec::new();
    header.extend_from_slice(BUNDLE_MAGIC);
    header.extend_from_slice(&1u16.to_be_bytes()); // format_version
    push_str(&mut header, &b.unicode_version);
    push_str(&mut header, &b.cldr_version);
    push_str(&mut header, &b.description);

    // Manifest length is fixed once the names are known, so body offsets can be computed up front.
    // Per entry: kind(1) + name(2+len) + hash(4) + raw_len(4) + comp_len(4) + offset(4).
    let manifest_len: usize = 2 + packed
        .iter()
        .map(|p| 1 + 2 + p.name.len() + 4 + 4 + 4 + 4)
        .sum::<usize>();
    let body_start = header.len() + manifest_len;

    let mut manifest = Vec::with_capacity(manifest_len);
    manifest.extend_from_slice(&(packed.len() as u16).to_be_bytes());
    let mut off = body_start;
    for p in &packed {
        manifest.push(p.kind);
        push_str(&mut manifest, &p.name);
        manifest.extend_from_slice(&p.hash.to_be_bytes());
        manifest.extend_from_slice(&p.raw_len.to_be_bytes());
        manifest.extend_from_slice(&(p.comp.len() as u32).to_be_bytes());
        manifest.extend_from_slice(&(off as u32).to_be_bytes());
        off += p.comp.len();
    }
    debug_assert_eq!(manifest.len(), manifest_len);

    let mut out = header;
    out.extend_from_slice(&manifest);
    for p in &packed {
        out.extend_from_slice(&p.comp);
    }
    let crc = crc32_ieee(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

/// Read a `JUCD` bundle (README §5). Verifies the trailing CRC, the magic, the format version, and
/// each section's content hash; a malformed bundle is `XX001` (data_corrupted).
pub fn open_bundle(bytes: &[u8]) -> Result<Bundle> {
    if bytes.len() < 4 {
        return Err(corrupt("bundle: truncated"));
    }
    let (body, trailer) = bytes.split_at(bytes.len() - 4);
    let want = u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    if crc32_ieee(body) != want {
        return Err(corrupt("bundle: trailer checksum mismatch"));
    }

    let mut r = Reader { b: bytes, i: 0 };
    if r.take(6)? != BUNDLE_MAGIC {
        return Err(corrupt("bundle: bad magic"));
    }
    let fmt = r.u16()?;
    if fmt != 1 {
        return Err(corrupt(format!("bundle: unsupported format_version {fmt}")));
    }
    let unicode_version = r.str()?;
    let cldr_version = r.str()?;
    let description = r.str()?;
    let count = r.u16()? as usize;

    struct M {
        kind: u8,
        name: String,
        hash: u32,
        raw_len: usize,
        comp_len: usize,
        offset: usize,
    }
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        entries.push(M {
            kind: r.u8()?,
            name: r.str()?,
            hash: r.u32()?,
            raw_len: r.u32()? as usize,
            comp_len: r.u32()? as usize,
            offset: r.u32()? as usize,
        });
    }

    let mut sections = Vec::with_capacity(count);
    for m in &entries {
        if m.offset > body.len() || m.offset + m.comp_len > body.len() {
            return Err(corrupt("bundle: section body out of range"));
        }
        let raw = lz4::decompress(&bytes[m.offset..m.offset + m.comp_len], m.raw_len)?;
        if crc32_ieee(&raw) != m.hash {
            return Err(corrupt("bundle: section content hash mismatch"));
        }
        sections.push(match m.kind {
            0 => Section::Property(deserialize_property(&raw)?),
            1 | 2 => {
                let (singles, contractions) = deserialize_table(&raw)?;
                let e = Entries {
                    name: m.name.clone(),
                    singles,
                    contractions,
                };
                if m.kind == 1 {
                    Section::Root(e)
                } else {
                    Section::Tailoring(e)
                }
            }
            k => return Err(corrupt(format!("bundle: unknown section kind {k}"))),
        });
    }
    Ok(Bundle {
        unicode_version,
        cldr_version,
        description,
        sections,
    })
}

/// Load a bundle (README §5.1): the root section is a usable collation; each tailoring is **merged**
/// onto the root (byte-identical to its fully-resolved `.coll` table). Every collation takes the
/// bundle header's `(unicode, cldr)` version + description. Returns the collations and the optional
/// property table.
pub fn load_bundle(b: &Bundle) -> Result<(Vec<Collation>, Option<PropertyTable>)> {
    let root = b.sections.iter().find_map(|s| match s {
        Section::Root(e) => Some(e),
        _ => None,
    });
    let mk = |name: &str, singles, contractions| Collation {
        name: name.to_string(),
        unicode_version: b.unicode_version.clone(),
        cldr_version: b.cldr_version.clone(),
        description: b.description.clone(),
        singles,
        contractions,
    };

    let mut colls = Vec::new();
    let mut property = None;
    if let Some(r) = root {
        colls.push(mk(&r.name, r.singles.clone(), r.contractions.clone()));
    }
    for s in &b.sections {
        match s {
            Section::Property(p) => property = Some(p.clone()),
            Section::Tailoring(t) => {
                let r = root.ok_or_else(|| corrupt("bundle: tailoring without a root section"))?;
                let (singles, contractions) = merge_onto_root(r, t);
                colls.push(mk(&t.name, singles, contractions));
            }
            Section::Root(_) => {}
        }
    }
    Ok((colls, property))
}

/// Build a `JUCD` bundle (README §5) from a `root` collation, per-locale `tailorings` (each diffed
/// against the root into a sparse override), and an optional `property` table — the builder tool's
/// core. The header `(unicode, cldr)` version is the root's.
pub fn build_bundle(
    root: &Collation,
    tailorings: &[&Collation],
    property: Option<PropertyTable>,
    description: &str,
) -> Bundle {
    let mut sections = Vec::new();
    if let Some(p) = property {
        sections.push(Section::Property(p));
    }
    sections.push(Section::Root(Entries {
        name: root.name.clone(),
        singles: root.singles.clone(),
        contractions: root.contractions.clone(),
    }));
    for t in tailorings {
        let (singles, contractions) = diff_against_root(t, root);
        sections.push(Section::Tailoring(Entries {
            name: t.name.clone(),
            singles,
            contractions,
        }));
    }
    Bundle {
        unicode_version: root.unicode_version.clone(),
        cldr_version: root.cldr_version.clone(),
        description: description.to_string(),
        sections,
    }
}

/// Merge a tailoring's sparse override onto the root table (README §5.1): start from the root maps,
/// replace-or-add each override by key, re-sort. The re-sort uses `BTreeMap`, so singles come out
/// ascending by code point and contractions lexicographic by sequence — the §2 total order — making
/// the result byte-identical to the fully-resolved `.coll` table.
#[allow(clippy::type_complexity)]
fn merge_onto_root(
    root: &Entries,
    delta: &Entries,
) -> (Vec<(u32, Vec<Ce>)>, Vec<(Vec<u32>, Vec<Ce>)>) {
    use std::collections::BTreeMap;
    let mut singles: BTreeMap<u32, Vec<Ce>> = root.singles.iter().cloned().collect();
    for (cp, ces) in &delta.singles {
        singles.insert(*cp, ces.clone());
    }
    let mut contractions: BTreeMap<Vec<u32>, Vec<Ce>> = root.contractions.iter().cloned().collect();
    for (seq, ces) in &delta.contractions {
        contractions.insert(seq.clone(), ces.clone());
    }
    (
        singles.into_iter().collect(),
        contractions.into_iter().collect(),
    )
}

/// The sparse override (README §5): the `full` table's singles/contractions that the `root` lacks or
/// maps differently. The current LDML subset only adds or replaces (no removals), so applying this
/// back onto the root reproduces `full` exactly (§5.1).
#[allow(clippy::type_complexity)]
fn diff_against_root(
    full: &Collation,
    root: &Collation,
) -> (Vec<(u32, Vec<Ce>)>, Vec<(Vec<u32>, Vec<Ce>)>) {
    use std::collections::HashMap;
    let root_singles: HashMap<u32, Vec<Ce>> = root.singles.iter().cloned().collect();
    let singles = full
        .singles
        .iter()
        .filter(|entry| root_singles.get(&entry.0) != Some(&entry.1))
        .cloned()
        .collect();
    let root_contractions: HashMap<Vec<u32>, Vec<Ce>> = root.contractions.iter().cloned().collect();
    let contractions = full
        .contractions
        .iter()
        .filter(|entry| root_contractions.get(&entry.0) != Some(&entry.1))
        .cloned()
        .collect();
    (singles, contractions)
}

fn serialize_property(p: &PropertyTable) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(1u8); // layout_version
    out.extend_from_slice(&(p.simple.len() as u32).to_be_bytes());
    for (cp, u, l, t) in &p.simple {
        out.extend_from_slice(&cp.to_be_bytes());
        out.extend_from_slice(&u.to_be_bytes());
        out.extend_from_slice(&l.to_be_bytes());
        out.extend_from_slice(&t.to_be_bytes());
    }
    out.extend_from_slice(&(p.special.len() as u32).to_be_bytes());
    for sc in &p.special {
        out.extend_from_slice(&sc.cp.to_be_bytes());
        push_cps(&mut out, &sc.upper);
        push_cps(&mut out, &sc.lower);
        push_cps(&mut out, &sc.title);
    }
    out
}

fn push_cps(out: &mut Vec<u8>, cps: &[u32]) {
    out.push(cps.len() as u8);
    for cp in cps {
        out.extend_from_slice(&cp.to_be_bytes());
    }
}

fn deserialize_property(raw: &[u8]) -> Result<PropertyTable> {
    let mut r = Reader { b: raw, i: 0 };
    let layout = r.u8()?;
    if layout != 1 {
        return Err(corrupt(format!(
            "bundle: unsupported property layout_version {layout}"
        )));
    }
    let num_simple = r.u32()? as usize;
    let mut simple = Vec::with_capacity(num_simple);
    for _ in 0..num_simple {
        simple.push((r.u32()?, r.u32()?, r.u32()?, r.u32()?));
    }
    let num_special = r.u32()? as usize;
    let mut special = Vec::with_capacity(num_special);
    for _ in 0..num_special {
        special.push(SpecialCasing {
            cp: r.u32()?,
            upper: r.cps()?,
            lower: r.cps()?,
            title: r.cps()?,
        });
    }
    if r.i != r.b.len() {
        return Err(corrupt("bundle: trailing bytes after property table"));
    }
    Ok(PropertyTable { simple, special })
}

#[cfg(test)]
mod bundle_tests {
    use super::*;

    /// Load the real version-pinned production bundle (spec/collation/fixtures/unicode.jucd) into the
    /// engine-global set, then hand back its `unicode`/`es` collations — the production read path the
    /// cores now take (no embed). Idempotent (the loaded set is global + first-wins).
    fn real(name: &str) -> std::sync::Arc<Collation> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../spec/collation/fixtures/unicode.jucd"
        );
        let bytes = std::fs::read(path).expect("read unicode.jucd fixture");
        load_unicode_data(&bytes).expect("load unicode.jucd");
        loaded_collation(name).unwrap_or_else(|| panic!("loaded collation {name}"))
    }

    // A small synthetic property section so the casing codec is exercised before `lower`/`upper`
    // land (Slice 3e): ASCII 'a'<->'A' simple, plus ß -> SS full (SpecialCasing).
    fn sample_property() -> PropertyTable {
        PropertyTable {
            simple: vec![(0x61, 0x41, 0x61, 0x41), (0x41, 0x41, 0x61, 0x41)],
            special: vec![SpecialCasing {
                cp: 0x00DF, // ß
                upper: vec![0x53, 0x53],
                lower: vec![0x00DF],
                title: vec![0x53, 0x73],
            }],
        }
    }

    #[test]
    fn bundle_round_trips_byte_identically_and_merge_reproduces_the_full_table() {
        let root = real("unicode");
        let es = real("es");

        let bundle = build_bundle(&root, &[&es], Some(sample_property()), "test bundle");

        // Save -> open -> save reproduces the bytes, and the parsed bundle equals the input.
        let bytes = save_bundle(&bundle);
        let reopened = open_bundle(&bytes).expect("open_bundle");
        assert_eq!(reopened, bundle, "parsed bundle differs from the built one");
        assert_eq!(
            save_bundle(&reopened),
            bytes,
            "bundle round-trip not byte-identical"
        );

        // Load -> the root is usable, and the tailoring merges back to the full `.coll` table.
        let (colls, property) = load_bundle(&bundle).expect("load_bundle");
        let loaded_unicode = colls.iter().find(|c| c.name == "unicode").expect("unicode");
        let loaded_es = colls.iter().find(|c| c.name == "es").expect("es");
        assert_eq!(
            serialize_table(loaded_unicode),
            serialize_table(&root),
            "root table changed through the bundle"
        );
        assert_eq!(
            serialize_table(loaded_es),
            serialize_table(&es),
            "merge(root, es-delta) is not byte-identical to the full es table"
        );
        assert_eq!(property.as_ref(), Some(&sample_property()));

        // The es delta is sparse — far smaller than the full es table (root-sharing pays off).
        let es_delta_singles = match &bundle.sections[2] {
            Section::Tailoring(e) => e.singles.len(),
            _ => panic!("expected a tailoring section at index 2"),
        };
        assert!(
            es_delta_singles < es.singles.len(),
            "es delta ({es_delta_singles}) should be sparse vs the full table ({})",
            es.singles.len()
        );
    }

    #[test]
    fn open_bundle_rejects_a_tampered_trailer() {
        let root = real("unicode");
        let mut bytes = save_bundle(&build_bundle(&root, &[], None, ""));
        let n = bytes.len();
        bytes[n - 1] ^= 0xFF; // corrupt the trailing CRC
        assert!(open_bundle(&bytes).is_err());
    }
}

#[cfg(test)]
mod casing_tests {
    // The casing kernels' DIVERGENCES from the PostgreSQL/glibc oracle (collation.md §16) — what the
    // oracle corpus cannot express (CLAUDE.md §10): the ASCII baseline (non-ASCII passes through, where
    // glibc folds) and full SpecialCasing (ß→SS, where glibc gives ß). Both call the pure kernels with
    // an EXPLICIT property table, so they are deterministic regardless of the engine-global loaded set.
    use super::*;

    // A small property table: ASCII a/A, é/É (simple), and ß→SS (special, no simple form). Sorted
    // ascending by code point (the §5 binary-search contract).
    fn prop() -> PropertyTable {
        PropertyTable {
            simple: vec![
                (0x41, 0x41, 0x61, 0x41), // A → a
                (0x61, 0x41, 0x61, 0x41), // a → A
                (0xC9, 0xC9, 0xE9, 0xC9), // É → é
                (0xE9, 0xC9, 0xE9, 0xC9), // é → É
            ],
            special: vec![SpecialCasing {
                cp: 0xDF, // ß: upper SS, lower identity, title Ss
                upper: vec![0x53, 0x53],
                lower: vec![0xDF],
                title: vec![0x53, 0x73],
            }],
        }
    }

    #[test]
    fn ascii_baseline_passes_non_ascii_through() {
        // No property loaded ⇒ fold ASCII only, pass the rest through (the SQLite default; a divergence
        // from glibc, which would fold é/É).
        assert_eq!(fold_case("café", true, None), "CAFé");
        assert_eq!(fold_case("CAFÉ", false, None), "cafÉ");
        assert_eq!(fold_case("Hello, World!", true, None), "HELLO, WORLD!");
        assert_eq!(fold_case("ß", true, None), "ß"); // ASCII baseline never expands
    }

    #[test]
    fn unicode_folds_via_the_property_table() {
        // The synthetic table covers a/A and é/É (simple) + ß (special); use only those letters.
        let p = prop();
        assert_eq!(fold_case("aé", true, Some(&p)), "AÉ");
        assert_eq!(fold_case("AÉ", false, Some(&p)), "aé");
        // Full SpecialCasing expansion ß→SS — the deliberate divergence from glibc's 1:1 ß.
        assert_eq!(fold_case("ß", true, Some(&p)), "SS");
        assert_eq!(fold_case("aßa", true, Some(&p)), "ASSA");
        assert_eq!(fold_case("ß", false, Some(&p)), "ß"); // lower of ß is identity
        // A code point not in the table is identity (no mapping).
        assert_eq!(fold_case("z", true, Some(&p)), "z");
    }

    #[test]
    fn fold_lower_simple_never_expands() {
        let p = prop();
        // ILIKE folding is simple-only: ß has no simple lower, so it stays one code point (it does NOT
        // become "ss"), keeping the matcher's _/length invariant.
        assert_eq!(fold_lower_simple("ß", Some(&p)), "ß");
        assert_eq!(fold_lower_simple("É", Some(&p)), "é");
        assert_eq!(fold_lower_simple("HELLO", None), "hello");
        assert_eq!(fold_lower_simple("É", None), "É"); // ASCII baseline passthrough
    }
}
