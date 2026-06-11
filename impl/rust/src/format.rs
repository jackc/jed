//! On-disk single-file format: serialize / load (spec/fileformat/format.md).
//!
//! `format_version` 2 — the page-backed copy-on-write B-tree (Phase 6, P6.1): each table's rows are
//! an on-disk B-tree (leaf + interior node pages), the catalog is a relocatable page chain, and
//! `to_image` lays the whole tree out post-order (the from-scratch image the goldens pin; the
//! incremental dirty-page commit reuses the same node codec — storage.md §4). The byte layout is the
//! canonical contract (spec/fileformat/format.md), verified byte-for-byte against shared goldens so a
//! file written by this core is byte-identical to the Go, TS, and Ruby reference output (CLAUDE.md
//! §8). All multi-byte integers are big-endian.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::catalog::{Column, Table};
use crate::decimal::Decimal;
use crate::encoding::{decode_int, encode_nullable};
use crate::error::{EngineError, Result, SqlState};
use crate::executor::{Database, Snapshot};
use crate::pager::Pager;
use crate::paging::SharedPaging;
use crate::pmap::{Child, Node};
use crate::storage::Row;
use crate::types::{DecimalTypmod, ScalarType};
use crate::value::{Unfetched, Value};

/// File magic — ASCII "JEDB" (the engine is named `jed`).
const MAGIC: [u8; 4] = *b"JEDB";
/// On-disk format version — 3 = + out-of-line overflow pages for large values (large-values.md §12).
const FORMAT_VERSION: u16 = 3;
/// Bytes of the page header on catalog / B-tree pages.
const PAGE_HEADER: usize = 12;
/// Smallest valid page size: the 36-byte meta header (plus the page header) must fit
/// (spec/fileformat/format.md *Page model*). Below it a file cannot hold its own meta.
const MIN_PAGE_SIZE: usize = PAGE_HEADER + 36;
/// Largest valid page size — 64 KiB (`MAX_PAGE_SIZE`, format.md *Page model*). The cap bounds the
/// largest single page allocation: without it a corrupt or hostile file could record a
/// multi-gigabyte `page_size` and force that allocation before its content is validated (CLAUDE.md
/// §13).
const MAX_PAGE_SIZE: usize = 65536;
/// `page_type` for a catalog page.
const PAGE_CATALOG: u8 = 1;
/// `page_type` for a B-tree leaf node.
const PAGE_LEAF: u8 = 2;
/// `page_type` for a B-tree interior node.
const PAGE_INTERIOR: u8 = 3;
/// `page_type` for an overflow page — a slab of an out-of-line (external) value's payload, chained
/// by `next_page` (spec/design/large-values.md §4/§12). Large values spill here so their record
/// stays ≤ `RECORD_MAX`.
const PAGE_OVERFLOW: u8 = 4;

/// Value-codec presence tags beyond `0x00` present-inline-plain / `0x01` NULL
/// (spec/design/large-values.md §12/§13; spec/fileformat/format.md "Large values"):
/// `0x02` external-plain — the body is a fixed pointer (`u32 first_page` + `u32 payload_len`)
/// into an overflow chain; `0x03` inline-compressed — `u32 raw_len` + `u16 comp_len` + the
/// LZ4 block (lz4.md); `0x04` external-compressed — `u32 first_page` + `u32 stored_len` +
/// `u32 raw_len`, the chain carrying the COMPRESSED block.
const TAG_EXTERNAL: u8 = 0x02;
const TAG_INLINE_COMP: u8 = 0x03;
const TAG_EXTERNAL_COMP: u8 = 0x04;
/// On-disk size of an external-plain pointer in a record: `tag(1) + first_page(u32) + len(u32)`.
const EXTERNAL_PTR_LEN: usize = 1 + 4 + 4;
/// In-record overhead of the inline-compressed form: `tag(1) + raw_len(u32) + comp_len(u16)`.
const INLINE_COMP_OVERHEAD: usize = 1 + 4 + 2;
/// On-disk size of an external-compressed pointer: `tag + first_page + stored_len + raw_len`.
const EXTERNAL_COMP_PTR_LEN: usize = 1 + 4 + 4 + 4;
/// Content payloads below this many bytes are never fed to the LZ4 encoder (header overhead
/// dominates; PostgreSQL pglz's default min_input_size — large-values.md §13).
const S_COMPRESS: usize = 32;
/// Catalog root page index of a *fresh empty* database (pages 0,1 are the meta slots). The catalog
/// root is **relocatable** thereafter — a reader follows `meta.root_page`, never assumes `2`.
const ROOT_PAGE: u32 = 2;

/// Stable on-disk type code for a scalar type — independent of the in-memory enum
/// discriminant (which may be reordered). See spec/fileformat/format.md.
fn type_code_for_scalar(ty: ScalarType) -> u8 {
    match ty {
        ScalarType::Int16 => 1,
        ScalarType::Int32 => 2,
        ScalarType::Int64 => 3,
        ScalarType::Text => 4,
        ScalarType::Bool => 5,
        ScalarType::Decimal => 6,
        ScalarType::Bytea => 7,
        ScalarType::Uuid => 8,
        ScalarType::Timestamp => 9,
        ScalarType::Timestamptz => 10,
    }
}

/// Inverse of `type_code_for_scalar`; None for an unknown code.
fn scalar_for_type_code(code: u8) -> Option<ScalarType> {
    match code {
        1 => Some(ScalarType::Int16),
        2 => Some(ScalarType::Int32),
        3 => Some(ScalarType::Int64),
        4 => Some(ScalarType::Text),
        5 => Some(ScalarType::Bool),
        6 => Some(ScalarType::Decimal),
        7 => Some(ScalarType::Bytea),
        8 => Some(ScalarType::Uuid),
        9 => Some(ScalarType::Timestamp),
        10 => Some(ScalarType::Timestamptz),
        _ => None,
    }
}

/// CRC-32/IEEE (reflected, poly 0xEDB88320, init/final 0xFFFFFFFF) — the standard
/// zlib CRC32, hand-rolled so no runtime dependency is needed. Pinned by the vector
/// `crc32("123456789") == 0xCBF43926`.
fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// The value codec (spec/fileformat/format.md): a 1-byte presence tag (`0x01` = NULL),
/// then the type's present-value body. Integers reuse the order-preserving key encoding;
/// `text` is where the seam diverges — a stored text value needs no ordering, so it is a
/// compact `u16` byte-length + UTF-8 bytes (collation `C`, verbatim). A text value whose
/// UTF-8 length exceeds `u16::MAX` is unsupported; in practice it also exceeds a page and
/// is caught by the oversized-item rule in `pack` (0A000), so the cast here is sound for
/// every supported page size (spec/fileformat/format.md). `boolean` is a single
/// `bool-byte` body — `0x00` false, `0x01` true (types.md §9).
fn encode_value(ty: ScalarType, v: &Value) -> Vec<u8> {
    match v {
        Value::Null => encode_nullable(ty, None),
        Value::Int(n) => encode_nullable(ty, Some(*n)),
        // Timestamps store their int64 microsecond instant via the same fixed-width codec as
        // int64 (the sentinels are ordinary extreme values; spec/design/timestamp.md).
        Value::Timestamp(m) | Value::Timestamptz(m) => encode_nullable(ty, Some(*m)),
        Value::Text(s) => {
            let bytes = s.as_bytes();
            let mut out = Vec::with_capacity(3 + bytes.len());
            out.push(0x00); // present
            out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
            out.extend_from_slice(bytes);
            out
        }
        // Bytea: same compact length-prefixed body as text, but the raw bytes verbatim
        // (no UTF-8) — spec/fileformat/format.md.
        Value::Bytea(b) => {
            let mut out = Vec::with_capacity(3 + b.len());
            out.push(0x00); // present
            out.extend_from_slice(&(b.len() as u16).to_be_bytes());
            out.extend_from_slice(b);
            out
        }
        // Uuid: a fixed 16-byte body, NO length prefix (the first fixed-width non-integer
        // value) — spec/fileformat/format.md.
        Value::Uuid(u) => {
            let mut out = Vec::with_capacity(1 + 16);
            out.push(0x00); // present
            out.extend_from_slice(u);
            out
        }
        Value::Bool(b) => vec![0x00, u8::from(*b)], // present tag + bool-byte (0x00 false, 0x01 true)
        // Decimal value codec (spec/fileformat/format.md): tag, flags (sign), u16 scale,
        // u16 ndigits, then that many big-endian base-10^4 coefficient groups (MS-first).
        Value::Decimal(d) => {
            let (neg, scale, groups) = d.to_codec();
            let mut out = Vec::with_capacity(6 + groups.len() * 2);
            out.push(0x00); // present
            out.push(if neg { 1 } else { 0 }); // flags: bit0 = sign
            out.extend_from_slice(&(scale as u16).to_be_bytes());
            out.extend_from_slice(&(groups.len() as u16).to_be_bytes());
            for g in groups {
                out.extend_from_slice(&g.to_be_bytes());
            }
            out
        }
        // An unfetched reference is resolved before any encode/plan (the scan layer for reads,
        // the mutation path for stores, `resolve_for_encode` at commit — large-values.md §14).
        Value::Unfetched(_) => panic!("BUG: encoding an unfetched large value"),
    }
}

/// Whether a value of this type can spill out-of-line (a variable-length type). Fixed-width types
/// (`int*`/`boolean`/`uuid`/`timestamp*`) are tiny and always stay inline
/// (spec/design/large-values.md §12).
fn is_spillable(ty: ScalarType) -> bool {
    ty.is_text() || ty.is_bytea() || ty.is_decimal()
}

/// Whether any column of this row shape can ever spill — the cheap gate that keeps the
/// overflow-page cost walk (`overflow_page_count`) off tables that cannot have chains.
pub(crate) fn any_spillable(col_types: &[ScalarType]) -> bool {
    col_types.iter().any(|&ty| is_spillable(ty))
}

/// Like [`any_spillable`], but only over the columns a query's touched set selects — the gate for
/// the masked scan-units walk (cost.md §3 "The touched set"): if no *touched* column can spill,
/// the whole walk yields zero and is skipped.
pub(crate) fn any_spillable_masked(col_types: &[ScalarType], mask: &[bool]) -> bool {
    col_types
        .iter()
        .zip(mask.iter())
        .any(|(&ty, &m)| m && is_spillable(ty))
}

/// The largest a single record may serialize to and still satisfy the B-tree split contract —
/// `RECORD_MAX = (C-12)/2` where `C = cap` is the page payload (spec/fileformat/format.md
/// "Why the record cap"). The spill planner reduces a record to ≤ this by externalizing values.
fn record_max(cap: usize) -> usize {
    cap.saturating_sub(PAGE_HEADER) / 2
}

/// A value's planned on-disk disposition (spec/design/large-values.md §2/§12/§13). The compressed
/// variants carry the LZ4 block the plan produced, so the serializer never re-compresses.
enum Disp {
    Inline,
    InlineComp(Vec<u8>),
    External,
    ExternalComp(Vec<u8>),
}

/// A record's resolved disposition plan: per-column form, the on-disk record size (the B-tree
/// split weight), and the `value_compress` slabs the plan's pass-1 attempts cost (cost.md §3).
struct RecordPlan {
    disp: Vec<Disp>,
    size: usize,
    compress_units: usize,
}

/// Decide each column's on-disk disposition for a record (spec/design/large-values.md §3/§12/§13;
/// spec/fileformat/format.md "Large values"). **Spill only when forced:** if the all-inline-plain
/// record already fits `RECORD_MAX`, nothing is compressed or spilled. Otherwise two passes, each
/// visiting largest encoded size first, ties by ascending column index — deterministic, a §8
/// contract: (1) **compress** eligible values (payload ≥ `S_COMPRESS`), adopting iff the encoded
/// compressed form is strictly smaller (store-smaller); (2) **externalize** values whose current
/// encoded size still beats their pointer, moving the bytes pass 1 chose (compressed → a `0x04`
/// chain of the compressed block) until the record fits. The size is the **weight** the
/// page-backed B-tree splits on, shared by the serializer and `record_size`: in-memory node
/// boundaries must match the serialized pages.
fn plan_dispositions(
    col_types: &[ScalarType],
    key: &[u8],
    row: &[Value],
    cap: usize,
) -> RecordPlan {
    let inline: Vec<usize> = col_types
        .iter()
        .zip(row.iter())
        .map(|(ty, v)| encode_value(*ty, v).len())
        .collect();
    let mut disp: Vec<Disp> = (0..row.len()).map(|_| Disp::Inline).collect();
    let mut cur = inline.clone();
    let mut size = 2 + key.len() + inline.iter().sum::<usize>();
    let max = record_max(cap);
    let mut compress_units = 0usize;
    if size <= max {
        return RecordPlan {
            disp,
            size,
            compress_units,
        };
    }
    // Pass 1 — compress (lz4.md): spillable, non-NULL, payload ≥ S_COMPRESS; largest
    // inline-plain encoded size first, ties by ascending index. Every attempt is metered
    // (ceil(raw/cap) value_compress slabs) whether or not store-smaller adopts it.
    let mut cand: Vec<usize> = (0..row.len())
        .filter(|&i| {
            is_spillable(col_types[i])
                && !matches!(row[i], Value::Null)
                && value_payload(col_types[i], &row[i]).len() >= S_COMPRESS
        })
        .collect();
    cand.sort_by(|&a, &b| inline[b].cmp(&inline[a]).then(a.cmp(&b)));
    for i in cand {
        if size <= max {
            break;
        }
        let payload = value_payload(col_types[i], &row[i]);
        compress_units += payload.len().div_ceil(cap);
        let comp = crate::lz4::compress(&payload);
        if INLINE_COMP_OVERHEAD + comp.len() < inline[i] {
            size = size - cur[i] + INLINE_COMP_OVERHEAD + comp.len();
            cur[i] = INLINE_COMP_OVERHEAD + comp.len();
            disp[i] = Disp::InlineComp(comp);
        }
    }
    if size <= max {
        return RecordPlan {
            disp,
            size,
            compress_units,
        };
    }
    // Pass 2 — externalize: anything whose current encoded size beats its pointer, largest
    // current size first, ties by ascending index. (A NULL is 1 byte and never qualifies.)
    let mut cand: Vec<usize> = (0..row.len())
        .filter(|&i| {
            is_spillable(col_types[i])
                && cur[i]
                    > match disp[i] {
                        Disp::InlineComp(_) => EXTERNAL_COMP_PTR_LEN,
                        _ => EXTERNAL_PTR_LEN,
                    }
        })
        .collect();
    cand.sort_by(|&a, &b| cur[b].cmp(&cur[a]).then(a.cmp(&b)));
    for i in cand {
        if size <= max {
            break;
        }
        let (ptr, next) = match std::mem::replace(&mut disp[i], Disp::Inline) {
            Disp::InlineComp(c) => (EXTERNAL_COMP_PTR_LEN, Disp::ExternalComp(c)),
            _ => (EXTERNAL_PTR_LEN, Disp::External),
        };
        disp[i] = next;
        size = size - cur[i] + ptr;
        cur[i] = ptr;
    }
    RecordPlan {
        disp,
        size,
        compress_units,
    }
}

/// The on-disk size of a record — the **weight** the page-backed B-tree splits on
/// (spec/fileformat/format.md). Accounts for compression and out-of-line spill: a compressed
/// value contributes its compressed inline form, an externalized one its fixed pointer size
/// (spec/design/large-values.md §12/§13). Must equal the length the serializer produces, so
/// in-memory node boundaries match the serialized pages.
pub(crate) fn record_size(col_types: &[ScalarType], key: &[u8], row: &Row, cap: usize) -> usize {
    plan_dispositions(col_types, key, row, cap).size
}

/// The per-record units a scan's up-front cost block charges for this record beyond the B-tree
/// nodes (cost.md §3; spec/design/large-values.md §8/§12/§14): for every column in the query's
/// **touched set** (`mask`), `pages` = one `page_read` per overflow chain page (the chain carries
/// the payload for external-plain, the COMPRESSED block for external-compressed) and
/// `decompress` = `ceil(raw_len / cap)` `value_decompress` slabs per compressed stored value
/// (inline- or external-). Zero/zero for a fully-inline-plain record or an untouched column.
pub(crate) struct ScanUnits {
    pub pages: usize,
    pub decompress: usize,
}

pub(crate) fn record_scan_units(
    col_types: &[ScalarType],
    key: &[u8],
    row: &Row,
    cap: usize,
    mask: &[bool],
) -> ScanUnits {
    let mut units = ScanUnits {
        pages: 0,
        decompress: 0,
    };
    // A lazily-loaded row carries its on-disk forms as unfetched references (large-values.md
    // §14): read the units straight off them — no disposition re-plan, which would need the
    // unfetched bytes. The numbers equal the resident plan below by construction (the
    // references ARE that plan's stored output), so a paged and an in-memory database charge
    // identically (cost.md §3, logical cost).
    if row.iter().any(|v| matches!(v, Value::Unfetched(_))) {
        for (i, v) in row.iter().enumerate() {
            if !mask[i] {
                continue;
            }
            if let Value::Unfetched(u) = v {
                match u {
                    Unfetched::External { len, .. } => {
                        units.pages += (*len as usize).div_ceil(cap);
                    }
                    Unfetched::InlineComp { raw_len, .. } => {
                        units.decompress += (*raw_len as usize).div_ceil(cap);
                    }
                    Unfetched::ExternalComp {
                        stored_len,
                        raw_len,
                        ..
                    } => {
                        units.pages += (*stored_len as usize).div_ceil(cap);
                        units.decompress += (*raw_len as usize).div_ceil(cap);
                    }
                }
            }
        }
        return units;
    }
    let plan = plan_dispositions(col_types, key, row, cap);
    for (i, d) in plan.disp.iter().enumerate() {
        if !mask[i] {
            continue; // an untouched column's chain/slabs are never read (cost.md §3)
        }
        match d {
            Disp::Inline => {}
            Disp::External => {
                units.pages += value_payload(col_types[i], &row[i]).len().div_ceil(cap);
            }
            Disp::InlineComp(_) => {
                units.decompress += value_payload(col_types[i], &row[i]).len().div_ceil(cap);
            }
            Disp::ExternalComp(c) => {
                units.pages += c.len().div_ceil(cap);
                units.decompress += value_payload(col_types[i], &row[i]).len().div_ceil(cap);
            }
        }
    }
    units
}

/// The `value_compress` slabs storing this record costs — one `ceil(raw_len / cap)` block per
/// pass-1 compression attempt, adopted or not (cost.md §3; large-values.md §13). Charged once
/// per stored row version at the statement's write site, never for B-tree re-encodes.
pub(crate) fn record_compress_units(
    col_types: &[ScalarType],
    key: &[u8],
    row: &Row,
    cap: usize,
) -> usize {
    plan_dispositions(col_types, key, row, cap).compress_units
}

/// A value's **content payload** `P(v)` — the bytes stored in the overflow chain when the value is
/// externalized (spec/design/large-values.md §12): raw UTF-8 for `text`, raw bytes for `bytea`, the
/// decimal body (`flags | scale | ndigits | groups`) for `decimal`. Only spillable types reach here.
fn value_payload(ty: ScalarType, v: &Value) -> Vec<u8> {
    match v {
        Value::Text(s) => s.as_bytes().to_vec(),
        Value::Bytea(b) => b.clone(),
        // The decimal inline body is the encoding minus its leading presence tag.
        Value::Decimal(_) => encode_value(ty, v)[1..].to_vec(),
        _ => unreachable!("only spillable values are externalized"),
    }
}

/// Reconstruct a value from the `P(v)` content payload gathered from its overflow chain (inverse of
/// [`value_payload`]) — spec/design/large-values.md §12.
fn value_from_payload(ty: ScalarType, payload: &[u8]) -> Result<Value> {
    if ty.is_text() {
        let s = String::from_utf8(payload.to_vec()).map_err(|_| corrupt("non-UTF-8 text value"))?;
        Ok(Value::Text(s))
    } else if ty.is_bytea() {
        Ok(Value::Bytea(payload.to_vec()))
    } else if ty.is_decimal() {
        let mut pos = 0usize;
        decode_decimal_body(payload, &mut pos)
    } else {
        Err(corrupt("a non-spillable type was stored external"))
    }
}

fn corrupt(msg: &str) -> EngineError {
    EngineError::new(SqlState::DataCorrupted, msg)
}

impl Snapshot {
    /// Serialize this snapshot's whole state to a single, clean **from-scratch** on-disk image
    /// (spec/fileformat/format.md, *Allocation & incremental commit*): every table's B-tree is
    /// laid out post-order from page 2, then the catalog chain, then both meta slots at `txid`.
    /// This is the special case where every node is dirty — the golden fixtures pin it, and it
    /// backs `create`'s initial image and (this slice) whole-image commit. (Incremental dirty-page
    /// commit reuses `serialize_node` but writes only the dirty path; storage.md §4.)
    pub fn to_image(&self, page_size: u32, txid: u64) -> Result<Vec<u8>> {
        let ps = page_size as usize;
        if ps < MIN_PAGE_SIZE {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "page size too small for the format",
            ));
        }
        if ps > MAX_PAGE_SIZE {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "page size too large for the format",
            ));
        }
        let cap = ps - PAGE_HEADER;

        // Tables in ascending lowercased-name order (no hash-map order leak).
        let mut tables = self.catalog_and_stores();
        tables.sort_by(|a, b| a.0.cmp(b.0));

        // Serialize each table's B-tree post-order, body pages allocated from page 2. Each entry
        // is `(index, page_type, item_count, next_page, payload)`; children precede their parent so
        // parent child-pointers reference already-allocated pages (format.md). `next_page` is `0`
        // for B-tree nodes and the chain link for overflow pages (large-values.md §12).
        let mut body: Vec<(u32, u8, u32, u32, Vec<u8>)> = Vec::new();
        let mut root_data_page = vec![0u32; tables.len()];
        let mut next_index = ROOT_PAGE;
        for (ti, (_, table, store)) in tables.iter().enumerate() {
            if let Some(root) = store.tree_root() {
                root_data_page[ti] = serialize_node(root, table, cap, &mut next_index, &mut body)?;
            }
        }

        // The catalog chain follows the data; its head is the relocatable `root_page`.
        let cat_root = next_index;
        let entry_sizes: Vec<usize> = tables
            .iter()
            .map(|(_, t, _)| table_entry_bytes(t, 0).len())
            .collect();
        let cat_groups = pack(&entry_sizes, cap)?;
        let page_count = cat_root + cat_groups.len() as u32;

        let mut image = vec![0u8; page_count as usize * ps];

        // Meta: both slots hold the current meta (a fresh from-scratch image has no distinct prior
        // version; slot alternation is the live incremental-commit path — format.md).
        write_meta(&mut image, ps, 0, page_size, txid, cat_root, page_count);
        write_meta(&mut image, ps, 1, page_size, txid, cat_root, page_count);

        // B-tree node + overflow pages.
        for (index, page_type, item_count, next_page, payload) in &body {
            write_page(
                &mut image,
                ps,
                *index,
                *page_type,
                *item_count,
                *next_page,
                payload,
            );
        }

        // Catalog chain.
        for (gi, group) in cat_groups.iter().enumerate() {
            let index = cat_root + gi as u32;
            let next = if gi + 1 < cat_groups.len() {
                index + 1
            } else {
                0
            };
            let mut payload = Vec::new();
            for &ti in group {
                payload.extend_from_slice(&table_entry_bytes(tables[ti].1, root_data_page[ti]));
            }
            write_page(
                &mut image,
                ps,
                index,
                PAGE_CATALOG,
                group.len() as u32,
                next,
                &payload,
            );
        }

        Ok(image)
    }
}

/// Serialize one B-tree node and its subtree post-order, appending `(index, page_type, item_count,
/// payload)` for each node to `body` and returning this node's assigned page index. A leaf's payload
/// is its records; an interior's payload is its `N+1` child pointers (big-endian `u32`) then its `N`
/// records (format.md). A node whose payload would exceed the page is an oversized record (one over
/// `RECORD_MAX`) — `feature_not_supported` (`0A000`), matching the v1 oversized-item rule.
fn serialize_node(
    node: &Arc<Node>,
    table: &Table,
    cap: usize,
    next_index: &mut u32,
    body: &mut Vec<(u32, u8, u32, u32, Vec<u8>)>,
) -> Result<u32> {
    let mut child_pages = Vec::with_capacity(node.children.len());
    for child in &node.children {
        // Whole-image serialize renumbers pages from scratch and runs only on a fully-resident
        // in-memory database (create's empty image, the golden generator) — a paged file commits
        // incrementally via `serialize_dirty`. An `OnDisk` child would carry a page id from a
        // different layout, so it must not appear here.
        let cp = match child {
            Child::Resident(n) => serialize_node(n, table, cap, next_index, body)?,
            Child::OnDisk(p) => {
                unreachable!("whole-image serialize hit an OnDisk leaf (page {p})")
            }
        };
        child_pages.push(cp);
    }
    let index = *next_index;
    *next_index += 1;

    let n = node.keys.len() as u32;
    let mut payload = Vec::new();
    let page_type = if node.children.is_empty() {
        PAGE_LEAF
    } else {
        for &cp in &child_pages {
            payload.extend_from_slice(&cp.to_be_bytes());
        }
        PAGE_INTERIOR
    };
    // Encode records, spilling over-large values to overflow pages allocated after this node's
    // index (post-order traversal + column order → deterministic, golden-pinnable layout).
    let mut ovf = Vec::new();
    let mut take = || {
        let i = *next_index;
        *next_index += 1;
        i
    };
    for i in 0..node.keys.len() {
        payload.extend_from_slice(&encode_record(
            table,
            &node.keys[i],
            &node.vals[i],
            cap,
            &mut take,
            &mut ovf,
        ));
    }
    if payload.len() > cap {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "a record larger than the per-row limit is not supported",
        ));
    }
    body.push((index, page_type, n, 0, payload));
    for o in ovf {
        body.push((o.index, PAGE_OVERFLOW, o.item_count, o.next_page, o.payload));
    }
    Ok(index)
}

/// Materialize any unfetched values in `row` for re-encoding at commit
/// (spec/design/large-values.md §14): a dirty leaf may carry rows the lazy load left as
/// references; the serializer needs their bytes to re-plan and rewrite the record. Unmetered,
/// like all commit work. Returns `None` when nothing is unfetched (the common case — no clone).
fn resolve_for_encode(
    row: &Row,
    table: &Table,
    paging: Option<&SharedPaging>,
) -> Result<Option<Row>> {
    if !row.iter().any(|v| matches!(v, Value::Unfetched(_))) {
        return Ok(None);
    }
    let paging = paging.ok_or_else(|| corrupt("unfetched large value with no pager at commit"))?;
    let fetch = |p: u32| paging.pager().read_block(p);
    let mut out = row.clone();
    for (i, v) in out.iter_mut().enumerate() {
        if let Value::Unfetched(u) = v {
            let resolved = resolve_unfetched(table.columns[i].ty, u, &fetch)?;
            *v = resolved;
        }
    }
    Ok(Some(out))
}

/// The pages an incremental commit must write durably, plus the new catalog root and high-water for
/// the meta slot (spec/fileformat/format.md, P6.1 part B). Each `pages` entry is a full `page_size`
/// image keyed by its page index; `file.rs` pwrites them, then publishes `root_page`/`page_count` in
/// the alternate meta slot.
pub(crate) struct IncrementalWrite {
    pub(crate) pages: Vec<(u32, Vec<u8>)>,
    pub(crate) root_page: u32,
    pub(crate) page_count: u32,
    /// The free-list entries this commit did **not** consume — the new free-list (P6.2). `file.rs`
    /// stores it back on the handle for the next commit (spec/fileformat/format.md *Reclamation*).
    pub(crate) free_remaining: Vec<u32>,
}

/// Allocates page indices for an incremental commit: the **free-list** first (lowest index, the
/// pages a prior root abandoned — spec/fileformat/format.md *Reclamation*), then fresh indices at
/// the high-water once the free-list is exhausted. The free-list is pre-sorted ascending, so
/// lowest-first allocation is deterministic and the bytes stay cross-core identical. Reusing a free
/// page is torn-write-safe: it left the free-list only here, becoming part of the new committed
/// version, so it is reachable from no fallback snapshot.
struct PageAlloc<'a> {
    free: &'a [u32],
    cursor: usize,
    next: u32,
}

impl PageAlloc<'_> {
    fn take(&mut self) -> u32 {
        if self.cursor < self.free.len() {
            let p = self.free[self.cursor];
            self.cursor += 1;
            p
        } else {
            let p = self.next;
            self.next += 1;
            p
        }
    }
}

impl Snapshot {
    /// Assemble the dirty body pages + freshly-rewritten catalog for an **incremental** commit,
    /// appending page allocation from `start_page` (the on-disk high-water) — the write path's
    /// counterpart to the whole-image `to_image` (spec/fileformat/format.md, *Allocation & incremental
    /// commit*). Only **dirty** nodes are emitted (clean subtrees keep their pages — the incremental
    /// win); the catalog chain is always rewritten (it carries each table's possibly-moved root). The
    /// dirty nodes' set-once page ids are assigned here. The page size was validated at file
    /// creation, so no size check is repeated.
    pub(crate) fn incremental_image(
        &self,
        page_size: u32,
        start_page: u32,
        free: &[u32],
        paging: Option<&SharedPaging>,
    ) -> Result<IncrementalWrite> {
        let ps = page_size as usize;
        let cap = ps - PAGE_HEADER;

        let mut tables = self.catalog_and_stores();
        tables.sort_by(|a, b| a.0.cmp(b.0));

        // Allocate from the free-list first (reclaiming dead pages), then extend the file.
        let mut alloc = PageAlloc {
            free,
            cursor: 0,
            next: start_page,
        };

        let mut pages: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut root_data_page = vec![0u32; tables.len()];
        for (ti, (_, table, store)) in tables.iter().enumerate() {
            if let Some(root) = store.tree_root() {
                root_data_page[ti] =
                    serialize_dirty(root, table, cap, ps, &mut alloc, &mut pages, paging)?;
            }
        }

        // The catalog chain is rewritten to fresh pages every commit (table roots move). Allocate
        // its page indices up front — they may be reused free pages, hence not contiguous — so each
        // page can point at the next (`pack` always returns ≥ 1 group, so `cat_pages` is non-empty).
        let entry_sizes: Vec<usize> = tables
            .iter()
            .map(|(_, t, _)| table_entry_bytes(t, 0).len())
            .collect();
        let cat_groups = pack(&entry_sizes, cap)?;
        let cat_pages: Vec<u32> = (0..cat_groups.len()).map(|_| alloc.take()).collect();
        let cat_root = cat_pages[0];
        for (gi, group) in cat_groups.iter().enumerate() {
            let next_page = if gi + 1 < cat_groups.len() {
                cat_pages[gi + 1]
            } else {
                0
            };
            let mut payload = Vec::new();
            for &ti in group {
                payload.extend_from_slice(&table_entry_bytes(tables[ti].1, root_data_page[ti]));
            }
            pages.push((
                cat_pages[gi],
                make_page(ps, PAGE_CATALOG, group.len() as u32, next_page, &payload),
            ));
        }

        Ok(IncrementalWrite {
            pages,
            root_page: cat_root,
            page_count: alloc.next,
            free_remaining: alloc.free[alloc.cursor..].to_vec(),
        })
    }
}

/// Assign a page to one **dirty** node (and its dirty descendants) post-order, appending each as a
/// full `page_size` page to `pages`, and return this node's page index. A **clean** node (already
/// persisted, `page != 0`) short-circuits: its whole subtree is on disk unchanged (copy-on-write
/// only rebuilds the modified path), so nothing is written and its existing page is returned. The
/// node's set-once page id is stored here — safe on the shared tree, since a node is otherwise
/// immutable (`AtomicU32`, P5.3b). Mirrors `serialize_node` for the byte layout.
fn serialize_dirty(
    node: &Arc<Node>,
    table: &Table,
    cap: usize,
    ps: usize,
    alloc: &mut PageAlloc,
    pages: &mut Vec<(u32, Vec<u8>)>,
    paging: Option<&SharedPaging>,
) -> Result<u32> {
    let existing = node.page.load(Ordering::Acquire);
    if existing != 0 {
        return Ok(existing);
    }
    let mut child_pages = Vec::with_capacity(node.children.len());
    for child in &node.children {
        // A `Resident` child recurses (dirty descendants get pages); an `OnDisk` child is a clean
        // leaf already durable at its page — keep it, write nothing (the incremental-commit win).
        let cp = match child {
            Child::Resident(n) => serialize_dirty(n, table, cap, ps, alloc, pages, paging)?,
            Child::OnDisk(p) => *p,
        };
        child_pages.push(cp);
    }
    let n = node.keys.len() as u32;
    let mut payload = Vec::new();
    let page_type = if node.children.is_empty() {
        PAGE_LEAF
    } else {
        for &cp in &child_pages {
            payload.extend_from_slice(&cp.to_be_bytes());
        }
        PAGE_INTERIOR
    };
    // Encode records, spilling over-large values to overflow pages drawn from the same allocator
    // (free-list first, then high-water — large-values.md §12). A dirty node may carry rows the
    // lazy load left unfetched (a sibling row's mutation dirtied them): resolve those through the
    // pager first — unmetered commit work, large-values.md §14 — so the re-encode re-plans the
    // resident row exactly as an eager writer would (chains are rewritten fresh; sharing an
    // unchanged chain is the deferred byte-layout follow-on). Scoped so the `&mut alloc` borrow
    // ends before this node's own page is allocated.
    let mut ovf = Vec::new();
    {
        let mut take = || alloc.take();
        for i in 0..node.keys.len() {
            let resolved = resolve_for_encode(&node.vals[i], table, paging)?;
            let row = resolved.as_ref().unwrap_or(&node.vals[i]);
            payload.extend_from_slice(&encode_record(
                table,
                &node.keys[i],
                row,
                cap,
                &mut take,
                &mut ovf,
            ));
        }
    }
    if payload.len() > cap {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "a record larger than the per-row limit is not supported",
        ));
    }
    let index = alloc.take();
    node.page.store(index, Ordering::Release);
    pages.push((index, make_page(ps, page_type, n, 0, &payload)));
    for o in ovf {
        pages.push((
            o.index,
            make_page(ps, PAGE_OVERFLOW, o.item_count, o.next_page, &o.payload),
        ));
    }
    Ok(index)
}

impl Database {
    /// Serialize the whole committed state to a single on-disk image (spec/fileformat/format.md).
    /// A thin wrapper over [`Snapshot::to_image`] for the committed snapshot — `txid` is written
    /// into both meta slots. (The writer's working snapshot is serialized directly via
    /// `Snapshot::to_image` at commit; this serves callers/tests holding a `Database`.)
    pub fn to_image(&self, page_size: u32, txid: u64) -> Result<Vec<u8>> {
        self.committed().to_image(page_size, txid)
    }

    /// Reconstruct a database from an on-disk image (inverse of `to_image`). Returns
    /// a structured `data_corrupted` (XX001) error for any malformed input.
    pub fn from_image(image: &[u8]) -> Result<Database> {
        if image.len() < 12 {
            return Err(corrupt("image smaller than a meta header"));
        }
        let page_size = read_u32_at(image, 8)? as usize;
        if !(MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&page_size) || image.len() < page_size * 2 {
            return Err(corrupt("invalid page size"));
        }
        let meta = select_meta(image, page_size)?;

        // Build the committed snapshot from the image, then wrap it in a fresh handle that
        // adopts the file's serialization parameters (spec/design/api.md §2).
        let mut snap = Snapshot::default();
        snap.txid = meta.txid;
        // Reconstruct the free-list (P6.2): collect every page reachable from the committed root —
        // the catalog chain plus each table's B-tree nodes — as we load it; the rest of `[2,
        // page_count)` is dead space the next incremental commit may reuse
        // (spec/fileformat/format.md *Reclamation*).
        let mut reached: HashSet<u32> = HashSet::new();
        let mut cat_page = meta.root_page;
        while cat_page != 0 {
            reached.insert(cat_page);
            let page = read_page(image, page_size, cat_page)?;
            if page.page_type != PAGE_CATALOG {
                return Err(corrupt("expected a catalog page"));
            }
            let mut pos = 0usize;
            for _ in 0..page.item_count {
                let (table, root_data_page) = decode_table_entry(page.payload, &mut pos)?;
                let name = table.name.clone();
                let col_types: Vec<ScalarType> = table.columns.iter().map(|c| c.ty).collect();
                let has_pk = !table.pk_indices().is_empty();
                snap.put_table(table, page_size as u32);
                if root_data_page != 0 {
                    let (root, len) =
                        read_tree(image, page_size, root_data_page, &col_types, &mut reached)?;
                    let store = snap.store_mut(&name);
                    store.set_tree(Some(root), len);
                    // No-PK keys are synthetic int64 rowids — advance the counter past the largest
                    // (the last entry in key order) so future inserts don't collide.
                    if !has_pk {
                        // In-memory load (no paging) — `iter_entries` never faults, so `?` is inert.
                        let entries = store.iter_entries()?;
                        if let Some((k, _)) = entries.last() {
                            store.bump_rowid_to(decode_int(ScalarType::Int64, k) + 1);
                        }
                    }
                }
            }
            cat_page = page.next_page;
        }
        let mut db = Database::new();
        db.page_size = page_size as u32;
        db.page_count = meta.page_count; // the on-disk high-water for the next incremental commit
        // The free-list: every body page `[2, page_count)` the committed root does not reach
        // (P6.2). Ascending by construction (the range is), so the allocator reuses lowest-first.
        db.free_pages = (ROOT_PAGE..meta.page_count)
            .filter(|p| !reached.contains(p))
            .collect();
        db.committed = snap;
        Ok(db)
    }

    /// Open a file-backed database **demand-paged** (spec/design/pager.md, P6.4b): load only the
    /// interior B-tree **skeleton** resident, leaving each leaf an `OnDisk` page faulted through the
    /// bounded buffer pool on access — so the resident set is bounded by the pool, not the file size.
    /// The inverse of an incremental commit, reading pages through `pager` instead of a whole image.
    ///
    /// This slice reads every leaf page once (to count its rows for `len` and mark it reachable for
    /// the free-list), then discards it — memory stays bounded (only the skeleton is retained), but
    /// open is O(pages). Making open O(skeleton) needs a per-subtree row count in the format (a
    /// deferred follow-on, pager.md §6); the residency win — a bounded *resident* set — already holds.
    pub(crate) fn open_paged(pager: Pager, capacity: usize) -> Result<Database> {
        let page_size = pager.page_size() as usize;
        if !(MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&page_size) {
            return Err(corrupt("invalid page size"));
        }
        let paging = SharedPaging::new(pager, capacity);

        // Select the live meta from slots 0 and 1 (highest valid txid; the lone valid slot on a torn
        // write), read as individual blocks through the pager.
        let meta = {
            let mut pg = paging.pager();
            let b0 = pg.read_block(0)?;
            let b1 = pg.read_block(1)?;
            match (parse_meta(&b0), parse_meta(&b1)) {
                (Some(a), Some(b)) => {
                    if b.txid > a.txid {
                        b
                    } else {
                        a
                    }
                }
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => return Err(corrupt("no valid meta page")),
            }
        };

        let mut snap = Snapshot::default();
        snap.txid = meta.txid;
        // Reconstruct the free-list (P6.2) from the pages the skeleton load marks reachable — every
        // interior node, plus each leaf's page id (recorded without retaining the leaf).
        let mut reached: HashSet<u32> = HashSet::new();
        let mut cat_page = meta.root_page;
        while cat_page != 0 {
            reached.insert(cat_page);
            let block = paging.pager().read_block(cat_page)?;
            let page = parse_page(&block)?;
            if page.page_type != PAGE_CATALOG {
                return Err(corrupt("expected a catalog page"));
            }
            let mut pos = 0usize;
            for _ in 0..page.item_count {
                let (table, root_data_page) = decode_table_entry(page.payload, &mut pos)?;
                let name = table.name.clone();
                let col_types: Vec<ScalarType> = table.columns.iter().map(|c| c.ty).collect();
                let has_pk = !table.pk_indices().is_empty();
                snap.put_table(table, page_size as u32);
                snap.store_mut(&name).attach_paging(paging.clone());
                if root_data_page != 0 {
                    let (root, len) =
                        read_skeleton(&paging, root_data_page, &col_types, &mut reached)?;
                    // The skeleton leaves leaves `OnDisk` (unread), so their records' overflow
                    // chains are invisible to the reachability walk above. For a table with
                    // spillable columns, read the leaves now to collect those live chains — else
                    // the free-list would reclaim still-referenced overflow pages
                    // (spec/design/large-values.md §12; default `open` is this paged path). Dead
                    // chains still leak until the next open, matching the P6.2 orphan model.
                    if any_spillable(&col_types) {
                        collect_leaf_overflow(&paging, root_data_page, &col_types, &mut reached)?;
                    }
                    let store = snap.store_mut(&name);
                    store.set_tree(Some(root), len);
                    if !has_pk {
                        // No-PK rowid reconstruction faults the leaves to find the largest key; only
                        // for keyless tables (most have a PK), and bounded by the pool.
                        let entries = store.iter_entries()?;
                        if let Some((k, _)) = entries.last() {
                            store.bump_rowid_to(decode_int(ScalarType::Int64, k) + 1);
                        }
                    }
                }
            }
            cat_page = page.next_page;
        }

        let mut db = Database::new();
        db.page_size = page_size as u32;
        db.page_count = meta.page_count;
        db.free_pages = (ROOT_PAGE..meta.page_count)
            .filter(|p| !reached.contains(p))
            .collect();
        db.committed = snap;
        db.paging = Some(paging);
        Ok(db)
    }
}

/// Walk a table's on-disk B-tree, reading each **leaf** and adding the overflow chain pages its
/// records reference to `reached` (spec/design/large-values.md §12). Interior separators are
/// skipped here — `read_skeleton_node` already collected their chains. Used only for tables with
/// spillable columns during the paged-open free-list reconstruction; it decodes each leaf lazily
/// and follows its chains **by headers only** (`chain_pages` — large-values.md §14), so opening a
/// file never materializes or decompresses a large value.
fn collect_leaf_overflow(
    paging: &SharedPaging,
    page_idx: u32,
    col_types: &[ScalarType],
    reached: &mut HashSet<u32>,
) -> Result<()> {
    let block = paging.pager().read_block(page_idx)?;
    let page = parse_page(&block)?;
    match page.page_type {
        PAGE_LEAF => {
            let fetch = |p: u32| paging.pager().read_block(p);
            let mut pos = 0usize;
            for _ in 0..page.item_count {
                let (_k, row, _w) = decode_record_lazy(col_types, page.payload, &mut pos)?;
                mark_chains(&row, &fetch, reached)?;
            }
            Ok(())
        }
        PAGE_INTERIOR => {
            let n = page.item_count as usize;
            let mut pos = 0usize;
            let mut cps = Vec::with_capacity(n + 1);
            for _ in 0..=n {
                cps.push(read_u32(page.payload, &mut pos)?);
            }
            for cp in cps {
                collect_leaf_overflow(paging, cp, col_types, reached)?;
            }
            Ok(())
        }
        _ => Err(corrupt("expected a B-tree node page")),
    }
}

/// Read a table's on-disk B-tree (rooted at `root_page`) into a demand-paged **skeleton**: interior
/// nodes resident, each leaf left `OnDisk`. Returns the root node and the total row count. A table
/// whose root is itself a single leaf has no interior parent to hold an `OnDisk` reference, so the
/// root leaf is faulted resident (spec/design/pager.md §1/§4).
fn read_skeleton(
    paging: &SharedPaging,
    root_page: u32,
    col_types: &[ScalarType],
    reached: &mut HashSet<u32>,
) -> Result<(Arc<Node>, usize)> {
    let (child, len) = read_skeleton_node(paging, root_page, col_types, reached)?;
    let root = match child {
        Child::Resident(node) => node,
        Child::OnDisk(page) => paging.fault_leaf(page, col_types)?,
    };
    Ok((root, len))
}

/// Read one B-tree node through the pager, **once**: a leaf becomes `Child::OnDisk` (its rows counted
/// from the header, then dropped — not retained); an interior node becomes `Child::Resident` with its
/// children resolved recursively. Returns the child reference and the subtree's row count.
fn read_skeleton_node(
    paging: &SharedPaging,
    page_idx: u32,
    col_types: &[ScalarType],
    reached: &mut HashSet<u32>,
) -> Result<(Child, usize)> {
    reached.insert(page_idx);
    let block = paging.pager().read_block(page_idx)?;
    let page = parse_page(&block)?;
    match page.page_type {
        PAGE_LEAF => Ok((Child::OnDisk(page_idx), page.item_count as usize)),
        PAGE_INTERIOR => {
            let n = page.item_count as usize;
            let mut pos = 0usize;
            let mut children = Vec::with_capacity(n + 1);
            let mut total = 0usize;
            for _ in 0..=n {
                let cp = read_u32(page.payload, &mut pos)?;
                let (child, clen) = read_skeleton_node(paging, cp, col_types, reached)?;
                children.push(child);
                total += clen;
            }
            let (mut keys, mut vals, mut weights) = (
                Vec::with_capacity(n),
                Vec::with_capacity(n),
                Vec::with_capacity(n),
            );
            // Separators decode lazily like leaves (large-values.md §14): an external value
            // stays an unfetched reference; its chain is marked reachable by headers only.
            let fetch = |p: u32| paging.pager().read_block(p);
            for _ in 0..n {
                let (key, row, w) = decode_record_lazy(col_types, page.payload, &mut pos)?;
                weights.push(w as u32);
                mark_chains(&row, &fetch, reached)?;
                keys.push(key);
                vals.push(row);
            }
            total += n;
            Ok((
                Child::Resident(Node::loaded(keys, vals, weights, children, page_idx)),
                total,
            ))
        }
        _ => Err(corrupt("expected a B-tree node page")),
    }
}

/// Read a table's on-disk B-tree (rooted at `page_idx`) into an in-memory tree, returning the root
/// node and the total row count (spec/fileformat/format.md). An interior node's payload is its
/// `N+1` child pointers then its `N` records; we recurse the pointers, then read the separators.
/// Weights are recomputed from the value codec (the exact size the writer used), so the loaded tree
/// is ready for further size-driven splits. Every node page and every overflow chain page reached
/// (an external value's chain — large-values.md §12) is added to `reached` for the free-list walk.
fn read_tree(
    image: &[u8],
    ps: usize,
    page_idx: u32,
    col_types: &[ScalarType],
    reached: &mut HashSet<u32>,
) -> Result<(Arc<Node>, usize)> {
    reached.insert(page_idx);
    let cap = ps - PAGE_HEADER;
    let page = read_page(image, ps, page_idx)?;
    let fetch = |p: u32| page_block(image, ps, p);
    match page.page_type {
        PAGE_LEAF => {
            let n = page.item_count as usize;
            let (mut keys, mut vals, mut weights) = (
                Vec::with_capacity(n),
                Vec::with_capacity(n),
                Vec::with_capacity(n),
            );
            let mut pos = 0usize;
            for _ in 0..n {
                let (key, row, ovf) =
                    decode_record(col_types, page.payload, &mut pos, Some(&fetch))?;
                weights.push(record_size(col_types, &key, &row, cap) as u32);
                reached.extend(ovf);
                keys.push(key);
                vals.push(row);
            }
            Ok((Node::loaded(keys, vals, weights, Vec::new(), page_idx), n))
        }
        PAGE_INTERIOR => {
            let n = page.item_count as usize;
            let mut pos = 0usize;
            let mut children = Vec::with_capacity(n + 1);
            let mut total = 0usize;
            for _ in 0..=n {
                let cp = read_u32(page.payload, &mut pos)?;
                let (child, clen) = read_tree(image, ps, cp, col_types, reached)?;
                // The in-memory load is fully resident (no pager to fault from); the demand-paged
                // file load (B2) is a separate path that leaves leaf children `OnDisk`.
                children.push(Child::Resident(child));
                total += clen;
            }
            let (mut keys, mut vals, mut weights) = (
                Vec::with_capacity(n),
                Vec::with_capacity(n),
                Vec::with_capacity(n),
            );
            for _ in 0..n {
                let (key, row, ovf) =
                    decode_record(col_types, page.payload, &mut pos, Some(&fetch))?;
                weights.push(record_size(col_types, &key, &row, cap) as u32);
                reached.extend(ovf);
                keys.push(key);
                vals.push(row);
            }
            total += n;
            Ok((Node::loaded(keys, vals, weights, children, page_idx), total))
        }
        _ => Err(corrupt("expected a B-tree node page")),
    }
}

/// One record's bytes: `key_len(u16) | key | payload(each column value)`.
/// One overflow page produced while serializing a record's external value (large-values.md §12).
struct OverflowPageOut {
    index: u32,
    item_count: u32,
    next_page: u32,
    payload: Vec<u8>,
}

/// Encode a record (`key_len | key | each column value`), spilling over-large values out-of-line
/// per the disposition plan (large-values.md §12). For each externalized value, allocate overflow
/// page(s) via `take`, append them to `ovf`, and write a fixed `tag | first_page | len` pointer
/// into the record instead of the inline body. `cap` is the page payload (the slab size + the
/// spill-plan input). Shared by the whole-image (`serialize_node`) and incremental (`serialize_dirty`)
/// writers, which differ only in how `take` allocates a page.
fn encode_record(
    table: &Table,
    key: &[u8],
    row: &[Value],
    cap: usize,
    take: &mut dyn FnMut() -> u32,
    ovf: &mut Vec<OverflowPageOut>,
) -> Vec<u8> {
    let col_types: Vec<ScalarType> = table.columns.iter().map(|c| c.ty).collect();
    let plan = plan_dispositions(&col_types, key, row, cap);
    let mut out = Vec::new();
    out.extend_from_slice(&(key.len() as u16).to_be_bytes());
    out.extend_from_slice(key);
    for (i, (col, val)) in table.columns.iter().zip(row.iter()).enumerate() {
        match &plan.disp[i] {
            Disp::Inline => out.extend_from_slice(&encode_value(col.ty, val)),
            Disp::External => {
                let payload = value_payload(col.ty, val);
                let first = write_overflow_chain(&payload, cap, take, ovf);
                out.push(TAG_EXTERNAL);
                out.extend_from_slice(&first.to_be_bytes());
                out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            }
            Disp::InlineComp(comp) => {
                let raw_len = value_payload(col.ty, val).len();
                out.push(TAG_INLINE_COMP);
                out.extend_from_slice(&(raw_len as u32).to_be_bytes());
                out.extend_from_slice(&(comp.len() as u16).to_be_bytes());
                out.extend_from_slice(comp);
            }
            Disp::ExternalComp(comp) => {
                // The chain carries the COMPRESSED block (its page count follows comp size).
                let raw_len = value_payload(col.ty, val).len();
                let first = write_overflow_chain(comp, cap, take, ovf);
                out.push(TAG_EXTERNAL_COMP);
                out.extend_from_slice(&first.to_be_bytes());
                out.extend_from_slice(&(comp.len() as u32).to_be_bytes());
                out.extend_from_slice(&(raw_len as u32).to_be_bytes());
            }
        }
    }
    out
}

/// Write `payload` across a chain of overflow pages (`cap`-byte slabs, in order), allocating each
/// page via `take` and linking it with `next_page` (`0` terminates). Returns the first page index
/// for the record's pointer. `payload` is always non-empty (only values larger than the pointer
/// spill — `plan_dispositions`).
fn write_overflow_chain(
    payload: &[u8],
    cap: usize,
    take: &mut dyn FnMut() -> u32,
    ovf: &mut Vec<OverflowPageOut>,
) -> u32 {
    let n = payload.len().div_ceil(cap);
    let indices: Vec<u32> = (0..n).map(|_| take()).collect();
    for (j, slab) in payload.chunks(cap).enumerate() {
        let next_page = if j + 1 < indices.len() {
            indices[j + 1]
        } else {
            0
        };
        ovf.push(OverflowPageOut {
            index: indices[j],
            item_count: slab.len() as u32,
            next_page,
            payload: slab.to_vec(),
        });
    }
    indices[0]
}

/// One table's catalog entry bytes (spec/fileformat/format.md).
fn table_entry_bytes(table: &Table, root_data_page: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let name = table.name.as_bytes();
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&(table.columns.len() as u16).to_be_bytes());
    for col in &table.columns {
        let cn = col.name.as_bytes();
        out.extend_from_slice(&(cn.len() as u16).to_be_bytes());
        out.extend_from_slice(cn);
        out.push(type_code_for_scalar(col.ty));
        let mut flags = 0u8;
        if col.primary_key {
            flags |= 0b01;
        }
        if col.not_null {
            flags |= 0b10;
        }
        if col.default.is_some() {
            flags |= 0b100;
        }
        out.push(flags);
        // A decimal column appends its typmod (precision, scale) — only for type_code 6, so
        // non-decimal entries are byte-unchanged (spec/fileformat/format.md). `precision 0`
        // = unconstrained `numeric`.
        if col.ty.is_decimal() {
            let (precision, scale) = match col.decimal {
                Some(t) => (t.precision, t.scale),
                None => (0u16, 0u16),
            };
            out.extend_from_slice(&precision.to_be_bytes());
            out.extend_from_slice(&scale.to_be_bytes());
        }
        // A column with a DEFAULT (flags bit2) appends its pre-evaluated default value via the
        // same value codec rows use — AFTER the typmod, presence-gated, so a column without a
        // default is byte-unchanged (spec/fileformat/format.md). A `DEFAULT NULL` is one 0x01.
        if let Some(d) = &col.default {
            out.extend_from_slice(&encode_value(col.ty, d));
        }
    }
    out.extend_from_slice(&root_data_page.to_be_bytes());
    out
}

/// Greedily pack item sizes into pages of capacity `cap`, returning groups of item
/// indices. An empty input yields one empty group (an empty page still exists). A
/// single item larger than `cap` is unsupported (no overflow pages in step-5b).
fn pack(sizes: &[usize], cap: usize) -> Result<Vec<Vec<usize>>> {
    let mut groups = Vec::new();
    let mut cur = Vec::new();
    let mut used = 0usize;
    for (i, &sz) in sizes.iter().enumerate() {
        if sz > cap {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "a record or table entry larger than a page is not supported",
            ));
        }
        if !cur.is_empty() && used + sz > cap {
            groups.push(std::mem::take(&mut cur));
            used = 0;
        }
        cur.push(i);
        used += sz;
    }
    groups.push(cur);
    Ok(groups)
}

/// One meta slot's full `page_size` bytes (the 36-byte header + its CRC, zero-padded): its only
/// content. `to_image` copies it into both slots; an incremental commit pwrites it to the alternate
/// slot (`file.rs`). Single-sources the meta byte layout (spec/fileformat/format.md).
pub(crate) fn meta_page(page_size: u32, txid: u64, root_page: u32, page_count: u32) -> Vec<u8> {
    let mut p = vec![0u8; page_size as usize];
    p[0..4].copy_from_slice(&MAGIC);
    p[4..6].copy_from_slice(&FORMAT_VERSION.to_be_bytes());
    p[8..12].copy_from_slice(&page_size.to_be_bytes());
    p[12..20].copy_from_slice(&txid.to_be_bytes());
    p[20..24].copy_from_slice(&root_page.to_be_bytes());
    p[24..28].copy_from_slice(&page_count.to_be_bytes());
    let crc = crc32_ieee(&p[0..32]);
    p[32..36].copy_from_slice(&crc.to_be_bytes());
    p
}

/// A catalog / B-tree page's full `page_size` bytes (header + payload, zero-padded). `to_image`
/// copies it into the image; an incremental commit pwrites it directly (`file.rs`). Single-sources
/// the page byte layout.
fn make_page(ps: usize, page_type: u8, item_count: u32, next_page: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0u8; ps];
    p[0] = page_type;
    p[4..8].copy_from_slice(&item_count.to_be_bytes());
    p[8..12].copy_from_slice(&next_page.to_be_bytes());
    p[PAGE_HEADER..PAGE_HEADER + payload.len()].copy_from_slice(payload);
    p
}

/// Write a meta slot into `image` (the whole-image path; `meta_page` is the single source).
fn write_meta(
    image: &mut [u8],
    ps: usize,
    slot: usize,
    page_size: u32,
    txid: u64,
    root_page: u32,
    page_count: u32,
) {
    let off = slot * ps;
    image[off..off + ps].copy_from_slice(&meta_page(page_size, txid, root_page, page_count));
}

/// Write a catalog / data page into `image` (the whole-image path; `make_page` is the single source).
fn write_page(
    image: &mut [u8],
    ps: usize,
    index: u32,
    page_type: u8,
    item_count: u32,
    next_page: u32,
    payload: &[u8],
) {
    let off = index as usize * ps;
    image[off..off + ps].copy_from_slice(&make_page(ps, page_type, item_count, next_page, payload));
}

/// A validated meta slot's salient fields.
struct Meta {
    txid: u64,
    root_page: u32,
    /// On-disk page high-water — the next free page an incremental commit appends at (P6.1 part B).
    page_count: u32,
}

/// Validate a standalone meta block; None if it is not a valid meta. Shared by `read_meta` (whole
/// image) and the demand-paged loader (which reads meta slots 0/1 as individual blocks).
fn parse_meta(m: &[u8]) -> Option<Meta> {
    if m.len() < 36 {
        return None;
    }
    if m[0..4] != MAGIC {
        return None;
    }
    if u16::from_be_bytes([m[4], m[5]]) != FORMAT_VERSION {
        return None;
    }
    if m[6] != 0 || m[7] != 0 || m[28..32] != [0, 0, 0, 0] {
        return None;
    }
    let stored = u32::from_be_bytes([m[32], m[33], m[34], m[35]]);
    if crc32_ieee(&m[0..32]) != stored {
        return None;
    }
    Some(Meta {
        txid: u64::from_be_bytes(m[12..20].try_into().unwrap()),
        root_page: u32::from_be_bytes(m[20..24].try_into().unwrap()),
        page_count: u32::from_be_bytes(m[24..28].try_into().unwrap()),
    })
}

/// Validate one meta slot of a whole image; None if it is not a valid meta.
fn read_meta(image: &[u8], ps: usize, slot: usize) -> Option<Meta> {
    let off = slot * ps;
    if off + ps > image.len() {
        return None;
    }
    parse_meta(&image[off..off + ps])
}

/// Pick the valid meta slot with the highest txid (tie → slot 0); the lone valid
/// slot on a torn write; error if neither is valid (spec/fileformat/format.md).
fn select_meta(image: &[u8], ps: usize) -> Result<Meta> {
    match (read_meta(image, ps, 0), read_meta(image, ps, 1)) {
        (Some(a), Some(b)) => Ok(if b.txid > a.txid { b } else { a }),
        (Some(a), None) => Ok(a),
        (None, Some(b)) => Ok(b),
        (None, None) => Err(corrupt("no valid meta page")),
    }
}

/// A parsed page: header fields + a borrowed payload slice.
struct Page<'a> {
    page_type: u8,
    item_count: u32,
    next_page: u32,
    payload: &'a [u8],
}

/// Parse one standalone page block (header + borrowed payload). The single-block reader the demand-
/// paged loader and fault path use (a page read through the pager is exactly one block); `read_page`
/// slices it out of a whole image.
fn parse_page(block: &[u8]) -> Result<Page<'_>> {
    if block.len() < PAGE_HEADER {
        return Err(corrupt("page shorter than its header"));
    }
    Ok(Page {
        page_type: block[0],
        item_count: u32::from_be_bytes([block[4], block[5], block[6], block[7]]),
        next_page: u32::from_be_bytes([block[8], block[9], block[10], block[11]]),
        payload: &block[PAGE_HEADER..],
    })
}

fn read_page(image: &[u8], ps: usize, index: u32) -> Result<Page<'_>> {
    let off = index as usize * ps;
    if off + ps > image.len() {
        return Err(corrupt("page index out of range"));
    }
    parse_page(&image[off..off + ps])
}

/// One page's full block, copied out of a whole image — the overflow-chain `fetch` for the
/// in-memory load path (`read_tree`, large-values.md §12).
fn page_block(image: &[u8], ps: usize, index: u32) -> Result<Vec<u8>> {
    let off = index as usize * ps;
    if off + ps > image.len() {
        return Err(corrupt("page index out of range"));
    }
    Ok(image[off..off + ps].to_vec())
}

/// Decode a single **leaf** page block into a resident node, for the demand-paging fault path
/// (spec/design/pager.md §4; paging.rs `fault_leaf`). `block` is one page; `page` is its page id,
/// stamped on the node so a later incremental commit keeps it clean. Decoding is **lazy**
/// (large-values.md §14): an external/compressed value becomes an [`Unfetched`] reference — no
/// chain read, no decompression — resolved later only for the columns a query touches. Each
/// weight is the bytes the record occupies on the page (exactly the writer's `record_size`).
pub(crate) fn decode_leaf_node(block: &[u8], page: u32, col_types: &[ScalarType]) -> Result<Node> {
    let parsed = parse_page(block)?;
    if parsed.page_type != PAGE_LEAF {
        return Err(corrupt("demand-paged a non-leaf page"));
    }
    let n = parsed.item_count as usize;
    let (mut keys, mut vals, mut weights) = (
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
    );
    let mut pos = 0usize;
    for _ in 0..n {
        let (key, row, w) = decode_record_lazy(col_types, parsed.payload, &mut pos)?;
        weights.push(w as u32);
        keys.push(key);
        vals.push(row);
    }
    Ok(Node::leaf_loaded(keys, vals, weights, page))
}

fn decode_table_entry(buf: &[u8], pos: &mut usize) -> Result<(Table, u32)> {
    let name = read_string(buf, pos)?;
    let col_count = read_u16(buf, pos)? as usize;
    let mut columns = Vec::with_capacity(col_count);
    for _ in 0..col_count {
        let cname = read_string(buf, pos)?;
        let tc = read_u8(buf, pos)?;
        let ty = scalar_for_type_code(tc).ok_or_else(|| corrupt("unknown type code"))?;
        let flags = read_u8(buf, pos)?;
        // A decimal column carries its typmod (precision, scale); precision 0 = unconstrained.
        let decimal = if ty.is_decimal() {
            let precision = read_u16(buf, pos)?;
            let scale = read_u16(buf, pos)?;
            if precision == 0 {
                None
            } else {
                Some(DecimalTypmod { precision, scale })
            }
        } else {
            None
        };
        // The default value follows the typmod, present iff flags bit2 (same value codec as
        // rows). Absent → no bytes consumed (spec/fileformat/format.md).
        // A default is a small evaluated literal — never externalized — so no overflow reader
        // is needed (a `0x02` tag here would be a corrupt catalog).
        let default = if flags & 0b100 != 0 {
            let mut sink = Vec::new();
            Some(read_value(ty, buf, pos, None, &mut sink)?)
        } else {
            None
        };
        columns.push(Column {
            name: cname,
            ty,
            decimal,
            primary_key: flags & 0b01 != 0,
            not_null: flags & 0b10 != 0,
            default,
        });
    }
    let root_data_page = read_u32(buf, pos)?;
    Ok((Table { name, columns }, root_data_page))
}

/// Decode one record `(key, row)` and the **overflow chain pages** any external value followed
/// (for the free-list reachability walk — spec/design/large-values.md §12). `fetch` reads a page
/// block by index, used to follow overflow chains; `None` is only valid where no value can be
/// external (e.g. a catalog default).
fn decode_record(
    col_types: &[ScalarType],
    buf: &[u8],
    pos: &mut usize,
    fetch: Option<&dyn Fn(u32) -> Result<Vec<u8>>>,
) -> Result<(Vec<u8>, Row, Vec<u32>)> {
    let key_len = read_u16(buf, pos)? as usize;
    let key = take(buf, pos, key_len)?.to_vec();
    let mut row = Vec::with_capacity(col_types.len());
    let mut ovf = Vec::new();
    for &ty in col_types {
        row.push(read_value(ty, buf, pos, fetch, &mut ovf)?);
    }
    Ok((key, row, ovf))
}

/// Read one value via the value codec (inverse of `encode_value`). The presence tag is read first:
/// `0x00` an inline body, `0x01` NULL, `0x02` an external pointer (`u32 first_page` + `u32 len`)
/// whose payload is gathered from the overflow chain via `fetch` and reconstructed by type
/// (spec/design/large-values.md §12). Pages visited while following a chain are pushed to `ovf_out`
/// for the free-list reachability walk.
fn read_value(
    ty: ScalarType,
    buf: &[u8],
    pos: &mut usize,
    fetch: Option<&dyn Fn(u32) -> Result<Vec<u8>>>,
    ovf_out: &mut Vec<u32>,
) -> Result<Value> {
    match read_u8(buf, pos)? {
        0x00 => read_inline_body(ty, buf, pos),
        0x01 => Ok(Value::Null),
        TAG_EXTERNAL => {
            let first = read_u32(buf, pos)?;
            let len = read_u32(buf, pos)? as usize;
            let fetch = fetch.ok_or_else(|| corrupt("external value with no overflow reader"))?;
            let payload = read_overflow_chain(first, len, fetch, ovf_out)?;
            value_from_payload(ty, &payload)
        }
        TAG_INLINE_COMP => {
            let raw_len = read_u32(buf, pos)? as usize;
            let comp_len = read_u16(buf, pos)? as usize;
            let comp = take(buf, pos, comp_len)?;
            let payload = crate::lz4::decompress(comp, raw_len)?;
            value_from_payload(ty, &payload)
        }
        TAG_EXTERNAL_COMP => {
            let first = read_u32(buf, pos)?;
            let stored = read_u32(buf, pos)? as usize;
            let raw_len = read_u32(buf, pos)? as usize;
            let fetch = fetch.ok_or_else(|| corrupt("external value with no overflow reader"))?;
            let comp = read_overflow_chain(first, stored, fetch, ovf_out)?;
            let payload = crate::lz4::decompress(&comp, raw_len)?;
            value_from_payload(ty, &payload)
        }
        _ => Err(corrupt("invalid value presence tag")),
    }
}

/// The present-value body (after a `0x00` tag): a fixed-width integer, a `u16` length + UTF-8 bytes
/// for `text`, a single `bool-byte`, the decimal body, etc. (spec/fileformat/format.md *Value codec*).
fn read_inline_body(ty: ScalarType, buf: &[u8], pos: &mut usize) -> Result<Value> {
    if ty.is_text() {
        let len = read_u16(buf, pos)? as usize;
        let bytes = take(buf, pos, len)?.to_vec();
        let s = String::from_utf8(bytes).map_err(|_| corrupt("non-UTF-8 text value"))?;
        Ok(Value::Text(s))
    } else if ty.is_bool() {
        match read_u8(buf, pos)? {
            0x00 => Ok(Value::Bool(false)),
            0x01 => Ok(Value::Bool(true)),
            _ => Err(corrupt("invalid boolean value byte")),
        }
    } else if ty.is_decimal() {
        decode_decimal_body(buf, pos)
    } else if ty.is_bytea() {
        let len = read_u16(buf, pos)? as usize;
        let bytes = take(buf, pos, len)?.to_vec();
        Ok(Value::Bytea(bytes))
    } else if ty.is_uuid() {
        // Fixed 16 raw bytes, no length prefix (must branch before the integer path —
        // decode_int would sign-flip and width_bytes is 16 there too).
        let b: [u8; 16] = take(buf, pos, 16)?
            .try_into()
            .map_err(|_| corrupt("invalid uuid length"))?;
        Ok(Value::Uuid(b))
    } else if ty.is_timestamp() {
        let vb = take(buf, pos, ty.width_bytes())?;
        Ok(Value::Timestamp(decode_int(ty, vb)))
    } else if ty.is_timestamptz() {
        let vb = take(buf, pos, ty.width_bytes())?;
        Ok(Value::Timestamptz(decode_int(ty, vb)))
    } else {
        let w = ty.width_bytes();
        let vb = take(buf, pos, w)?;
        Ok(Value::Int(decode_int(ty, vb)))
    }
}

/// Decode a decimal value's body — `flags` (sign), `u16` scale, `u16` ndigits, then that many
/// base-10⁴ groups (spec/fileformat/format.md). Shared by the inline path and by external
/// reconstruction (a spilled decimal's chain payload is exactly this body — large-values.md §12).
fn decode_decimal_body(buf: &[u8], pos: &mut usize) -> Result<Value> {
    let flags = read_u8(buf, pos)?;
    let neg = flags & 1 != 0;
    let scale = read_u16(buf, pos)? as u32;
    let ndigits = read_u16(buf, pos)? as usize;
    let mut groups = Vec::with_capacity(ndigits);
    for _ in 0..ndigits {
        groups.push(read_u16(buf, pos)?);
    }
    Ok(Value::Decimal(Decimal::from_codec(neg, scale, &groups)))
}

/// Gather `len` bytes of an external value's payload by following its overflow chain from
/// `first` (spec/design/large-values.md §12): each page is `page_type 4`, carries `item_count`
/// payload bytes, and chains via `next_page` (`0` terminates). Every visited page is pushed to
/// `visited` (the free-list reachability walk). `fetch` returns a page's full block by index.
fn read_overflow_chain(
    first: u32,
    len: usize,
    fetch: &dyn Fn(u32) -> Result<Vec<u8>>,
    visited: &mut Vec<u32>,
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    let mut p = first;
    while out.len() < len {
        if p == 0 {
            return Err(corrupt("overflow chain ended before the value length"));
        }
        visited.push(p);
        let block = fetch(p)?;
        let page = parse_page(&block)?;
        if page.page_type != PAGE_OVERFLOW {
            return Err(corrupt("expected an overflow page"));
        }
        let take = page.item_count as usize;
        if take == 0 || take > page.payload.len() || out.len() + take > len {
            return Err(corrupt("overflow page slab out of range"));
        }
        out.extend_from_slice(&page.payload[..take]);
        p = page.next_page;
    }
    Ok(out)
}

/// Read one value **lazily** (spec/design/large-values.md §14): inline-plain and NULL decode as
/// today, but an external/compressed form becomes an [`Unfetched`] reference holding exactly the
/// record's pointer fields — no chain read, no decompression. The scan layer resolves the
/// references for the columns a query touches ([`resolve_unfetched`]); the commit path resolves
/// the rest when a dirty leaf re-encodes (`resolve_for_encode`).
fn read_value_lazy(ty: ScalarType, buf: &[u8], pos: &mut usize) -> Result<Value> {
    match read_u8(buf, pos)? {
        0x00 => read_inline_body(ty, buf, pos),
        0x01 => Ok(Value::Null),
        TAG_EXTERNAL => {
            let first_page = read_u32(buf, pos)?;
            let len = read_u32(buf, pos)?;
            Ok(Value::Unfetched(Unfetched::External { first_page, len }))
        }
        TAG_INLINE_COMP => {
            let raw_len = read_u32(buf, pos)?;
            let comp_len = read_u16(buf, pos)? as usize;
            let comp = take(buf, pos, comp_len)?.to_vec();
            Ok(Value::Unfetched(Unfetched::InlineComp { comp, raw_len }))
        }
        TAG_EXTERNAL_COMP => {
            let first_page = read_u32(buf, pos)?;
            let stored_len = read_u32(buf, pos)?;
            let raw_len = read_u32(buf, pos)?;
            Ok(Value::Unfetched(Unfetched::ExternalComp {
                first_page,
                stored_len,
                raw_len,
            }))
        }
        _ => Err(corrupt("invalid value presence tag")),
    }
}

/// Decode one record lazily (`read_value_lazy` per column) and return `(key, row, weight)`,
/// where the weight is the bytes the record occupies on the page — exactly the
/// [`record_size`] the writer split on, read off the cursor instead of re-planned (a re-plan
/// would need the unfetched bytes).
fn decode_record_lazy(
    col_types: &[ScalarType],
    buf: &[u8],
    pos: &mut usize,
) -> Result<(Vec<u8>, Row, usize)> {
    let start = *pos;
    let key_len = read_u16(buf, pos)? as usize;
    let key = take(buf, pos, key_len)?.to_vec();
    let mut row = Vec::with_capacity(col_types.len());
    for &ty in col_types {
        row.push(read_value_lazy(ty, buf, pos)?);
    }
    Ok((key, row, *pos - start))
}

/// Materialize an unfetched reference into its plain [`Value`] (spec/design/large-values.md
/// §14): gather the overflow chain through `fetch` for an external form, decompress a
/// compressed one, and reconstruct by column type. Decompression errors are `data_corrupted`,
/// surfaced only when the value is actually touched.
pub(crate) fn resolve_unfetched(
    ty: ScalarType,
    u: &Unfetched,
    fetch: &dyn Fn(u32) -> Result<Vec<u8>>,
) -> Result<Value> {
    let mut sink = Vec::new();
    match u {
        Unfetched::External { first_page, len } => {
            let payload = read_overflow_chain(*first_page, *len as usize, fetch, &mut sink)?;
            value_from_payload(ty, &payload)
        }
        Unfetched::InlineComp { comp, raw_len } => {
            let payload = crate::lz4::decompress(comp, *raw_len as usize)?;
            value_from_payload(ty, &payload)
        }
        Unfetched::ExternalComp {
            first_page,
            stored_len,
            raw_len,
        } => {
            let comp = read_overflow_chain(*first_page, *stored_len as usize, fetch, &mut sink)?;
            let payload = crate::lz4::decompress(&comp, *raw_len as usize)?;
            value_from_payload(ty, &payload)
        }
    }
}

/// The page indices of the overflow chain carrying `len` payload bytes from `first`, following
/// `next_page` hops and reading **headers only** — no payload assembly, no decompression
/// (spec/design/large-values.md §14). The open-time reachability walk marks live chains with
/// this, so opening a file never materializes its large values.
fn chain_pages(first: u32, len: usize, fetch: &dyn Fn(u32) -> Result<Vec<u8>>) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    let mut gathered = 0usize;
    let mut p = first;
    while gathered < len {
        if p == 0 {
            return Err(corrupt("overflow chain ended before the value length"));
        }
        out.push(p);
        let block = fetch(p)?;
        let page = parse_page(&block)?;
        if page.page_type != PAGE_OVERFLOW {
            return Err(corrupt("expected an overflow page"));
        }
        let take = page.item_count as usize;
        if take == 0 || take > page.payload.len() || gathered + take > len {
            return Err(corrupt("overflow page slab out of range"));
        }
        gathered += take;
        p = page.next_page;
    }
    Ok(out)
}

/// Add the overflow chain pages a lazily-decoded row references to `reached` (the free-list
/// reachability walk), via the header-only [`chain_pages`] hop.
fn mark_chains(
    row: &Row,
    fetch: &dyn Fn(u32) -> Result<Vec<u8>>,
    reached: &mut HashSet<u32>,
) -> Result<()> {
    for v in row {
        if let Value::Unfetched(u) = v {
            match u {
                Unfetched::External { first_page, len } => {
                    reached.extend(chain_pages(*first_page, *len as usize, fetch)?);
                }
                Unfetched::ExternalComp {
                    first_page,
                    stored_len,
                    ..
                } => {
                    reached.extend(chain_pages(*first_page, *stored_len as usize, fetch)?);
                }
                Unfetched::InlineComp { .. } => {}
            }
        }
    }
    Ok(())
}

// --- bounds-checked big-endian readers over a payload cursor ---

fn take<'a>(buf: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
    if *pos + n > buf.len() {
        return Err(corrupt("unexpected end of page data"));
    }
    let s = &buf[*pos..*pos + n];
    *pos += n;
    Ok(s)
}

fn read_u8(buf: &[u8], pos: &mut usize) -> Result<u8> {
    Ok(take(buf, pos, 1)?[0])
}

fn read_u16(buf: &[u8], pos: &mut usize) -> Result<u16> {
    let s = take(buf, pos, 2)?;
    Ok(u16::from_be_bytes([s[0], s[1]]))
}

fn read_u32(buf: &[u8], pos: &mut usize) -> Result<u32> {
    let s = take(buf, pos, 4)?;
    Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn read_u32_at(buf: &[u8], at: usize) -> Result<u32> {
    if at + 4 > buf.len() {
        return Err(corrupt("truncated header"));
    }
    Ok(u32::from_be_bytes([
        buf[at],
        buf[at + 1],
        buf[at + 2],
        buf[at + 3],
    ]))
}

fn read_string(buf: &[u8], pos: &mut usize) -> Result<String> {
    let len = read_u16(buf, pos)? as usize;
    let bytes = take(buf, pos, len)?.to_vec();
    String::from_utf8(bytes).map_err(|_| corrupt("non-UTF-8 name"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute;

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn type_codes_round_trip() {
        for ty in ScalarType::all() {
            assert_eq!(scalar_for_type_code(type_code_for_scalar(ty)), Some(ty));
        }
        assert_eq!(scalar_for_type_code(0), None);
        assert_eq!(scalar_for_type_code(11), None);
    }

    fn sample_db() -> Database {
        let mut db = Database::new();
        for s in [
            "CREATE TABLE t (id int32 PRIMARY KEY, v int16)",
            "INSERT INTO t VALUES (1, 10)",
            "INSERT INTO t VALUES (2, NULL)",
            "INSERT INTO t VALUES (3, 30)",
            "CREATE TABLE r (a int16, b int64)",
            "INSERT INTO r VALUES (7, 70)",
        ] {
            execute(&mut db, s).expect("setup");
        }
        db
    }

    #[test]
    fn serialize_is_deterministic() {
        let db = sample_db();
        assert_eq!(db.to_image(8192, 1).unwrap(), db.to_image(8192, 1).unwrap());
    }

    #[test]
    fn in_memory_round_trip() {
        let db = sample_db();
        let image = db.to_image(8192, 1).unwrap();
        let loaded = Database::from_image(&image).unwrap();
        assert_eq!(loaded.to_image(8192, 1).unwrap(), image);
        assert_eq!(
            loaded.rows_in_key_order("t"),
            db.rows_in_key_order("t"),
            "PK table rows survive the round trip"
        );
        assert_eq!(loaded.rows_in_key_order("r"), db.rows_in_key_order("r"));
    }

    #[test]
    fn selects_highest_txid_and_falls_back() {
        let db = sample_db();
        let ps = 8192usize;
        let mut image = db.to_image(ps as u32, 1).unwrap();
        let pc = (image.len() / ps) as u32;
        // Two valid slots, differing txid: slot 1 (txid 7) must win over slot 0 (2).
        write_meta(&mut image, ps, 0, ps as u32, 2, ROOT_PAGE, pc);
        write_meta(&mut image, ps, 1, ps as u32, 7, ROOT_PAGE, pc);
        assert_eq!(select_meta(&image, ps).unwrap().txid, 7);
        // Corrupt slot 1's CRC: selection falls back to the valid slot 0.
        image[ps + 35] ^= 0xFF;
        assert_eq!(select_meta(&image, ps).unwrap().txid, 2);
    }

    #[test]
    fn corrupt_image_is_rejected() {
        let db = sample_db();
        let mut image = db.to_image(8192, 1).unwrap();
        // Smash both meta magics.
        image[0] ^= 0xFF;
        image[8192] ^= 0xFF;
        // (Database has no Debug impl, so match the error rather than unwrap_err.)
        match Database::from_image(&image) {
            Err(e) => assert_eq!(e.code(), "XX001"),
            Ok(_) => panic!("expected a data_corrupted error"),
        }
    }

    // --- large values / overflow pages (spec/design/large-values.md §12) ---

    /// Count the body pages of a given `page_type` in an image (meta slots start with the magic, so
    /// they never collide with a small `page_type` byte).
    fn count_page_type(image: &[u8], ps: usize, ty: u8) -> usize {
        (0..image.len() / ps)
            .filter(|&i| image[i * ps] == ty)
            .count()
    }

    /// Incompressible filler (spec/fileformat/format.md "Fixtures"): xorshift32("JEDB") over a
    /// 64-char alphabet, so Slice B's compress pass never wins store-smaller and the value
    /// deterministically stays PLAIN (this test exercises the out-of-line chain, which a
    /// compressible run would dodge by compressing inline).
    fn filler_text(n: usize) -> String {
        const ALPHA64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut x: u32 = 0x4A45_4442;
        let mut out = String::with_capacity(n);
        for _ in 0..n {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            out.push(ALPHA64[(x % 64) as usize] as char);
        }
        out
    }

    /// A table with a large incompressible `text` value that must spill out-of-line at a small
    /// page size, plus a small inline value. The big value (1250 bytes) far exceeds `RECORD_MAX`
    /// at `ps=256` (`= (256-12-12)/2 = 116`) and `cap` (`= 244`), so it spans **several**
    /// overflow pages.
    fn big_value_db() -> Database {
        let mut db = Database::new();
        let big = filler_text(1250);
        for s in [
            "CREATE TABLE t (id int32 PRIMARY KEY, body text)".to_string(),
            format!("INSERT INTO t VALUES (1, '{big}')"),
            "INSERT INTO t VALUES (2, 'tiny')".to_string(),
        ] {
            execute(&mut db, &s).expect("setup");
        }
        db
    }

    #[test]
    fn external_value_spans_overflow_chain_and_round_trips() {
        let db = big_value_db();
        let ps = 256u32;
        let image = db.to_image(ps, 1).unwrap();
        // The 1250-byte value spilled across a multi-page chain (cap 244 ⇒ ≥ 6 pages).
        assert!(
            count_page_type(&image, ps as usize, PAGE_OVERFLOW) >= 2,
            "a large value spans several overflow pages"
        );
        // It reconstructs exactly, and re-serialization is byte-identical (deterministic spill +
        // chain allocation).
        let loaded = Database::from_image(&image).unwrap();
        assert_eq!(
            loaded.rows_in_key_order("t"),
            db.rows_in_key_order("t"),
            "the external value survives the round trip"
        );
        assert_eq!(loaded.to_image(ps, 1).unwrap(), image);
    }

    #[test]
    fn small_values_never_spill() {
        // The all-integer sample table fits inline at the same small page size — "spill only when
        // forced": no overflow page is written.
        let image = sample_db().to_image(256, 1).unwrap();
        assert_eq!(
            count_page_type(&image, 256, PAGE_OVERFLOW),
            0,
            "inline-fitting values are never externalized"
        );
    }

    #[test]
    fn from_image_reclaims_only_dead_overflow_pages() {
        // The live external value's chain pages must be in the reachable set, so they are NOT on the
        // reconstructed free-list (else a later commit would reuse a still-referenced page).
        let db = big_value_db();
        let ps = 256u32;
        let loaded = Database::from_image(&db.to_image(ps, 1).unwrap()).unwrap();
        let ovf_pages =
            count_page_type(&loaded.to_image(ps, 1).unwrap(), ps as usize, PAGE_OVERFLOW);
        assert!(ovf_pages >= 2);
        assert!(
            loaded.free_pages.len() < ovf_pages,
            "live overflow pages ({ovf_pages}) are reachable, not free ({} free)",
            loaded.free_pages.len()
        );
    }
}
