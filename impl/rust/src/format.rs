//! On-disk single-file format: serialize / load (spec/fileformat/format.md).
//!
//! `format_version` 8 — the page-backed copy-on-write B-tree (Phase 6, P6.1; v3 added large-value
//! overflow/compression, v4 the catalog CHECK-constraint list, v5 the secondary-index catalog
//! reshape, v6 the per-index unique flags byte, v7 a per-page CRC-32 on every body page — the
//! header grows 12→16 bytes, v8 the per-column expression-default flag bit3 + expr-text,
//! spec/fileformat/format.md *Version scope*): each table's rows are
//! an on-disk B-tree (leaf + interior node pages), the catalog is a relocatable page chain, and
//! `to_image` lays the whole tree out post-order (the from-scratch image the goldens pin; the
//! incremental dirty-page commit reuses the same node codec — storage.md §4). The byte layout is the
//! canonical contract (spec/fileformat/format.md), verified byte-for-byte against shared goldens so a
//! file written by this core is byte-identical to the Go, TS, and Ruby reference output (CLAUDE.md
//! §8). All multi-byte integers are big-endian.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::catalog::{
    CheckConstraint, ColField, ColType, Column, CompositeField, CompositeType, DefaultExpr,
    FkAction, ForeignKeyConstraint, IdentityKind, IndexDef, IndexKind, SequenceDef, Table,
};
use crate::collation::Collation;
use crate::decimal::Decimal;
use crate::encoding::{decode_int, encode_nullable};
use crate::error::{EngineError, Result, SqlState};
use crate::executor::{Database, Snapshot};
use crate::interval::Interval;
use crate::json::JsonNode;
use crate::pager::Pager;
use crate::paging::SharedPaging;
use crate::pmap::{Child, Node};
use crate::storage::{Row, TableStore};
use crate::types::{DecimalTypmod, ScalarType, Type};
use crate::value::{ArrayVal, RangeVal, Unfetched, Value};

/// File magic — ASCII "JEDB" (the engine is named `jed`).
const MAGIC: [u8; 4] = *b"JEDB";
/// On-disk format version — 18 = reference-only collations (the reference-only pivot,
/// spec/design/collation.md §2/§3/§5): the `entry_kind = 3` collation entry is now **metadata only**
/// — a flags byte (`is_default`) + name + `(unicode_version, cldr_version)` version pin + description,
/// with **no compiled table**. The table is vendored into the binary (§9) and resolved by name on
/// open; the recorded version is the pin a future graded verdict checks (compatibility.md §7). The
/// per-column collation (flags-byte bit 6 `has_collation` + a trailing name) is unchanged. This
/// supersedes v17's baked snapshot (the LZ4-compressed `.coll` artifact is gone). v16 = range columns
/// (type_code 17 + an inline element-type descriptor in
/// the catalog, spec/design/ranges.md §3, and the compact range value body — a flags byte
/// EMPTY/LB_INF/UB_INF/LB_INC/UB_INC followed by the present bound bodies, §4). v15 = IDENTITY columns
/// (the column-entry flags byte gains bit4 `is_identity` + bit5 `identity_always`; an identity column
/// desugars like `serial` plus those two bits, spec/design/sequences.md §13). v14 = the `serial`
/// owned-sequence link (the sequence-entry flags byte gains a `has_owner` bit + a trailing owner
/// table-name/column-ordinal, spec/design/sequences.md §12). v13 = GIN indexes (a per-index
/// `index_kind` byte after `index_flags`, spec/design/gin.md §7). v12 = sequences (a kind-tagged
/// sequence catalog section) + the `date` scalar. v11 = FOREIGN KEY constraints (a per-table catalog
/// foreign-key list after the index list, spec/design/constraints.md §6). v10 = array (`T[]`) columns
/// (type_code 15 + an element-type descriptor in the catalog, spec/design/array.md §3, and the compact
/// array value body, §4); v9 = composite (row) types (kind-tagged catalog entries); v8 = a per-column
/// expression-default flag; v7 added a per-page CRC-32 (header grew 12→16 bytes). Each bump is atomic
/// across the Rust/Go/TS cores + the Ruby golden reference (every `.jed` golden's version byte + CRC
/// changed together).
///
/// v19 = **storable `json` / `jsonb` columns** (spec/design/json.md, slice J1/J1b): a column type
/// can be `json` (type_code 18) or `jsonb` (type_code 19) — plain scalar catalog entries with no
/// extra descriptor (the `has_jsonb_dict` door §3.2 stays clear, zero bytes). A `json` value's body
/// is the verbatim text, length-prefixed like `text` (§4); a `jsonb` value's body is the
/// self-delimiting tagged-node tree (§2 — node tags + LEB128 varint counts, numbers as the decimal
/// body), riding the large-value overflow + LZ4 path. No catalog-shape change, so a file with no
/// json/jsonb column still moves to v19 only by its version byte.
///
/// v20 = **GiST indexes** (spec/design/gist.md, slice GX1): a per-index `index_kind = 2` selects the
/// GiST access method, and the index's on-disk form is a persisted **R-tree** of bounding-predicate
/// nodes — two new `page_type`s `5` (GiST leaf) / `6` (GiST interior) (§4.1). A leaf entry is
/// `bound_len u16 ‖ encode_range_body(bound) ‖ skey_len u16 ‖ skey`; an interior entry is
/// `bound_len u16 ‖ encode_range_body(union) ‖ child_page u32`. The catalog index entry is unchanged
/// (the `index_root_page` slot points at the R-tree root, `0` for an empty index); only the node
/// pages it reaches differ. A file with no GiST index still moves to v20 only by its version byte.
const FORMAT_VERSION: u16 = 20;
/// Bytes of the page header on catalog / B-tree / overflow pages (v7): the 12-byte v6 header
/// (`page_type`, `item_count`, `next_page`) plus a 4-byte per-page `crc32` (offset 12).
const PAGE_HEADER: usize = 16;
/// Bytes reserved inside `RECORD_MAX` for a two-key interior node's three child pointers (`4·3`)
/// — **independent of `PAGE_HEADER`** (spec/fileformat/format.md "Why the record cap"). Both were
/// 12 through v6; v7 widened the header to 16 but this reserve stays 12.
const INTERIOR_RESERVE: usize = 12;
/// Smallest valid page size (spec/fileformat/format.md *Page model*). A chosen floor of 256,
/// comfortably above the structural minimum `PAGE_HEADER + 36 = 52` (below which the 36-byte meta
/// header would not fit); sub-256 sizes have only ever served tiny test fixtures.
const MIN_PAGE_SIZE: usize = 256;
/// Largest valid page size — 64 KiB (`MAX_PAGE_SIZE`, format.md *Page model*). The cap bounds the
/// largest single page allocation: without it a corrupt or hostile file could record a
/// multi-gigabyte `page_size` and force that allocation before its content is validated (CLAUDE.md
/// §13).
const MAX_PAGE_SIZE: usize = 65536;

/// Whether `ps` is a legal page size: a **power of two** within `[MIN_PAGE_SIZE, MAX_PAGE_SIZE]`
/// (format.md *Page model* — the nine values `{256, 512, … 65536}`). Power-of-two keeps every page
/// boundary sector-aligned (the SSD target, CLAUDE.md §9) and shrinks the legal set; `is_power_of_two()`
/// also excludes `0`, so the pager's `page_size` divisor is never zero.
fn page_size_valid(ps: usize) -> bool {
    ps.is_power_of_two() && (MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&ps)
}
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
        ScalarType::Interval => 11,
        ScalarType::Float64 => 12,
        ScalarType::Float32 => 13,
        ScalarType::Date => 16,
        // 14 (composite) / 15 (array) / 17 (range) are container element-type codes, not scalars.
        ScalarType::Json => 18,
        ScalarType::Jsonb => 19,
        // `jsonpath` reserves type code 20, but is literal-only this slice (no storable column), so
        // this code is never written to disk yet — a storable jsonpath column is a P1a follow-on.
        ScalarType::JsonPath => 20,
    }
}

/// Append an array column's **element type descriptor** (spec/design/array.md §3): the element's
/// type code, then (for a composite element) its name. v1 element types are scalars; a composite
/// element is handled for forward-compat, a nested array element is rejected (multidimensionality
/// is a value property, not array-of-array — §2).
fn push_array_element_type(out: &mut Vec<u8>, elem: &Type) {
    match elem {
        Type::Scalar(s) => out.push(type_code_for_scalar(*s)),
        Type::Composite(r) => {
            out.push(14);
            let tn = r.name.as_bytes();
            out.extend_from_slice(&(tn.len() as u16).to_be_bytes());
            out.extend_from_slice(tn);
        }
        Type::Array(_) => {
            unreachable!("nested array element (array-of-array) is not a jed type — §2")
        }
        Type::Range(_) => {
            unreachable!("array-of-range is not storable yet (range columns land in R2)")
        }
    }
}

/// Decode an array column's element type descriptor (inverse of [`push_array_element_type`]).
fn read_array_element_type(buf: &[u8], pos: &mut usize) -> Result<Type> {
    let code = read_u8(buf, pos)?;
    if code == 14 {
        let name = read_string(buf, pos)?;
        Ok(Type::Composite(crate::types::CompositeRef { name }))
    } else {
        let s = scalar_for_type_code(code).ok_or_else(|| corrupt("invalid array element code"))?;
        Ok(Type::Scalar(s))
    }
}

/// Append a range column's **element type descriptor** (spec/design/ranges.md §3): a single `u8`
/// scalar type code. A range element is always one of the six scalar subtypes (`i32`/`i64`/
/// `decimal`/`timestamp`/`timestamptz`/`date`) — never composite, array, or nested range — and
/// `numrange`'s element is the *unconstrained* `decimal`, so no typmod is stored (the type name
/// fully determines the element). The element descriptor is self-describing: it identifies which of
/// the six ranges the column is.
fn push_range_element_type(out: &mut Vec<u8>, elem: &Type) {
    match elem {
        Type::Scalar(s) => out.push(type_code_for_scalar(*s)),
        _ => unreachable!("a range element is always a scalar subtype (ranges.md §2)"),
    }
}

/// Decode a range column's element type descriptor (inverse of [`push_range_element_type`]): one
/// scalar code, validated to be one of the six range element subtypes (else `XX001`).
fn read_range_element_type(buf: &[u8], pos: &mut usize) -> Result<Type> {
    let code = read_u8(buf, pos)?;
    let s = scalar_for_type_code(code).ok_or_else(|| corrupt("invalid range element code"))?;
    if crate::range::range_for_element(s).is_none() {
        return Err(corrupt("type code is not a valid range element subtype"));
    }
    Ok(Type::Scalar(s))
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
        11 => Some(ScalarType::Interval),
        12 => Some(ScalarType::Float64),
        13 => Some(ScalarType::Float32),
        16 => Some(ScalarType::Date),
        18 => Some(ScalarType::Json),
        19 => Some(ScalarType::Jsonb),
        // `jsonpath` reserves code 20 (non-storable this slice, so never actually decoded off disk).
        20 => Some(ScalarType::JsonPath),
        _ => None,
    }
}

/// Fold `data` into a running CRC-32/IEEE register (reflected, poly 0xEDB88320), **without** the
/// final XOR — so it composes: `crc32_update(crc32_update(0xFFFF_FFFF, a), b)` over a split buffer
/// equals folding `a ‖ b`. Both `crc32_ieee` and the split [`page_crc`] build on it.
fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    crc
}

/// CRC-32/IEEE (reflected, poly 0xEDB88320, init/final 0xFFFFFFFF) — the standard
/// zlib CRC32, hand-rolled so no runtime dependency is needed. Pinned by the vector
/// `crc32("123456789") == 0xCBF43926`. Also the collation `.coll` artifact's
/// `content_hash` (spec/collation/README.md §3), hence `pub`.
pub fn crc32_ieee(data: &[u8]) -> u32 {
    !crc32_update(0xFFFF_FFFF, data)
}

/// The per-page checksum (v7, spec/fileformat/format.md *Page header*): CRC-32/IEEE over a body
/// page's bytes **excluding its own 4-byte `crc32` field** at `[12, 16)` — i.e. `[0, 12)` then
/// `[16, page_size)`, covering the header, payload, and zero-fill tail. `make_page` writes it;
/// `parse_page` re-verifies it (mismatch → `XX001`). `page` is one full page (`page_size` bytes).
fn page_crc(page: &[u8]) -> u32 {
    !crc32_update(
        crc32_update(0xFFFF_FFFF, &page[0..12]),
        &page[PAGE_HEADER..],
    )
}

/// The value codec (spec/fileformat/format.md): a 1-byte presence tag (`0x01` = NULL),
/// then the type's present-value body. Integers reuse the order-preserving key encoding;
/// `text` is where the seam diverges — a stored text value needs no ordering, so it is a
/// compact `u16` byte-length + UTF-8 bytes (collation `C`, verbatim). A text value whose
/// UTF-8 length exceeds `u16::MAX` is unsupported; in practice it also exceeds a page and
/// is caught by the oversized-item rule in `pack` (0A000), so the cast here is sound for
/// every supported page size (spec/fileformat/format.md). `boolean` is a single
/// `bool-byte` body — `0x00` false, `0x01` true (types.md §9).
/// A **composite** value (spec/design/composite.md §4) is the shared presence tag then a body of
/// `null-bitmap ‖ each present field's value-codec body` (no per-field tag — the bitmap carries
/// presence): see [`encode_composite_body`]. Recurses for nested composites.
fn encode_value(ty: &ColType, v: &Value) -> Vec<u8> {
    match ty {
        ColType::Scalar(s) => encode_scalar(*s, v),
        ColType::Composite { fields, .. } => match v {
            Value::Null => vec![0x01],
            Value::Composite(vals) => {
                let mut out = vec![0x00]; // present
                out.extend_from_slice(&encode_composite_body(fields, vals));
                out
            }
            _ => panic!("BUG: a non-composite value in a composite column"),
        },
        ColType::Array(elem) => match v {
            Value::Null => vec![0x01],
            Value::Array(arr) => {
                let mut out = vec![0x00]; // present
                out.extend_from_slice(&encode_array_body(elem, arr));
                out
            }
            _ => panic!("BUG: a non-array value in an array column"),
        },
        ColType::Range(elem) => match v {
            Value::Null => vec![0x01],
            Value::Range(rv) => {
                let mut out = vec![0x00]; // present
                out.extend_from_slice(&encode_range_body(elem, rv));
                out
            }
            _ => panic!("BUG: a non-range value in a range column"),
        },
    }
}

/// A range value's **body** (after the `0x00` present tag, spec/design/ranges.md §4): a single
/// `flags u8` then the present bound bodies. The flags bits are `EMPTY` (0), `LB_INF` (1),
/// `UB_INF` (2), `LB_INC` (3), `UB_INC` (4); bits 5–7 are reserved 0. An empty range is the lone
/// flags byte `0x01` (no bounds follow). Otherwise a finite lower bound (`!LB_INF`) then a finite
/// upper bound (`!UB_INF`) each contribute the element's value-codec body **minus the presence
/// tag** (the same tag-byte+body split array/composite use). The stored value is canonical (§4) —
/// canonicalization happens at parse/cast, not here.
pub(crate) fn encode_range_body(elem: &ColType, rv: &RangeVal) -> Vec<u8> {
    if rv.empty {
        return vec![0x01]; // RANGE_EMPTY
    }
    let mut flags = 0u8;
    if rv.lower.is_none() {
        flags |= 0x02; // LB_INF
    }
    if rv.upper.is_none() {
        flags |= 0x04; // UB_INF
    }
    if rv.lower_inc {
        flags |= 0x08; // LB_INC
    }
    if rv.upper_inc {
        flags |= 0x10; // UB_INC
    }
    let mut out = vec![flags];
    if let Some(lo) = &rv.lower {
        out.extend_from_slice(&encode_value(elem, lo)[1..]); // body only (no presence tag)
    }
    if let Some(hi) = &rv.upper {
        out.extend_from_slice(&encode_value(elem, hi)[1..]);
    }
    out
}

/// An array value's **body** (after the `0x00` present tag, spec/design/array.md §4):
/// `ndim u8 ‖ flags u8 ‖ per-dim (len u32 BE, lb i32 BE) ‖ [null_bitmap if HAS_NULLS] ‖ element
/// bodies`. An empty array is `ndim = 0` (no dims/bitmap/elements); otherwise `ndim` is the
/// dimension count and each dimension records its length and lower bound (multidim + custom lower
/// bounds — spec/design/array.md §12). The bitmap (MSB-first, like composite) is present iff any
/// element is NULL (the `HAS_NULLS` flag bit); a NULL element contributes zero body bytes, a
/// present element its value-codec body minus the presence tag (row-major).
fn encode_array_body(elem: &ColType, arr: &ArrayVal) -> Vec<u8> {
    let mut out = Vec::new();
    if arr.elements.is_empty() {
        out.push(0); // ndim = 0 (empty array)
        out.push(0); // flags
        return out;
    }
    let has_nulls = arr.elements.iter().any(|e| matches!(e, Value::Null));
    out.push(arr.ndim() as u8);
    out.push(if has_nulls { 0x01 } else { 0x00 }); // flags: bit 0 = HAS_NULLS
    for d in 0..arr.ndim() {
        out.extend_from_slice(&(arr.dims[d] as u32).to_be_bytes()); // dim length
        out.extend_from_slice(&arr.lbounds[d].to_be_bytes()); // lower bound (i32 BE)
    }
    if has_nulls {
        let nbytes = arr.elements.len().div_ceil(8);
        let mut bitmap = vec![0u8; nbytes];
        for (i, e) in arr.elements.iter().enumerate() {
            if matches!(e, Value::Null) {
                bitmap[i / 8] |= 0x80 >> (i % 8);
            }
        }
        out.extend_from_slice(&bitmap);
    }
    for e in &arr.elements {
        if !matches!(e, Value::Null) {
            out.extend_from_slice(&encode_value(elem, e)[1..]); // body only (no presence tag)
        }
    }
    out
}

/// A composite value's **body** (after the `0x00` present tag, spec/design/composite.md §4): a null
/// bitmap of `ceil(field_count/8)` bytes (MSB-first — field *i* is bit `0x80 >> (i%8)` of byte
/// `i/8`; a set bit = NULL) followed by each **present** field's value-codec body in declaration
/// order. A NULL field contributes zero body bytes; a present field's body is its `encode_value`
/// minus the leading presence tag (a nested composite recurses).
fn encode_composite_body(fields: &[ColField], vals: &[Value]) -> Vec<u8> {
    let nbytes = fields.len().div_ceil(8);
    let mut bitmap = vec![0u8; nbytes];
    let mut bodies = Vec::new();
    for (i, (f, val)) in fields.iter().zip(vals.iter()).enumerate() {
        if matches!(val, Value::Null) {
            bitmap[i / 8] |= 0x80 >> (i % 8);
        } else {
            bodies.extend_from_slice(&encode_value(&f.ty, val)[1..]);
        }
    }
    bitmap.extend_from_slice(&bodies);
    bitmap
}

// --- jsonb value codec (the tagged-node tree, spec/design/json.md §2) -------------------------
//
// A `jsonb` value's BODY (after the `0x00` present tag) is a self-delimiting depth-first
// serialization of the canonical node tree: every node leads with a one-byte tag (low nibble =
// kind, high nibble = flags, reserved 0). Like array/range, there is NO outer length prefix — the
// tree walks itself, so a large `jsonb` body rides the large-value overflow + LZ4 path opaquely
// (§2). The node tags are NTAG_* below; counts/string lengths are an unsigned LEB128 varint
// (§2.2's single-byte examples). A `json` value's body is the text VERBATIM, length-prefixed
// exactly like `text` (§4).

const NTAG_NULL: u8 = 0x0;
const NTAG_FALSE: u8 = 0x1;
const NTAG_TRUE: u8 = 0x2;
const NTAG_NUMBER: u8 = 0x3;
const NTAG_STRING: u8 = 0x4;
const NTAG_STRING_DICT: u8 = 0x5; // reserved — the dictionary door (§3); a reader rejects it XX001
const NTAG_ARRAY: u8 = 0x6;
const NTAG_OBJECT: u8 = 0x7;

/// Append an unsigned LEB128 varint (7 bits/byte, high bit = continuation) — the count/length codec
/// for the `jsonb` node bodies (spec/design/json.md §2.1).
fn write_uvarint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Read an unsigned LEB128 varint (inverse of [`write_uvarint`]). `XX001` on a truncated or
/// over-64-bit value.
fn read_uvarint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let byte = read_u8(buf, pos)?;
        if shift >= 64 || (shift == 63 && byte > 1) {
            return Err(corrupt("jsonb varint overflows u64"));
        }
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
    }
}

/// A decimal value's BODY (no presence tag): `flags(sign) ‖ u16 scale ‖ u16 ndigits ‖ groups`
/// (base-10⁴, MS-first) — the `NTAG_NUMBER` payload and the inverse of [`decode_decimal_body`].
fn encode_decimal_body(d: &Decimal, out: &mut Vec<u8>) {
    let (neg, scale, groups) = d.to_codec();
    out.push(if neg { 1 } else { 0 });
    out.extend_from_slice(&(scale as u16).to_be_bytes());
    out.extend_from_slice(&(groups.len() as u16).to_be_bytes());
    for g in groups {
        out.extend_from_slice(&g.to_be_bytes());
    }
}

/// Serialize a `jsonb` node tree into `out` (the body bytes — spec/design/json.md §2.1). Object
/// members are already in canonical key order (the canonicalizer's invariant); each member's key is
/// itself a string node (`NTAG_STRING`), so the dictionary door covers keys and values uniformly.
fn encode_jsonb_body(node: &JsonNode, out: &mut Vec<u8>) {
    match node {
        JsonNode::Null => out.push(NTAG_NULL),
        JsonNode::Bool(false) => out.push(NTAG_FALSE),
        JsonNode::Bool(true) => out.push(NTAG_TRUE),
        JsonNode::Number(d) => {
            out.push(NTAG_NUMBER);
            encode_decimal_body(d, out);
        }
        JsonNode::String(s) => {
            out.push(NTAG_STRING);
            write_uvarint(out, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
        JsonNode::Array(elems) => {
            out.push(NTAG_ARRAY);
            write_uvarint(out, elems.len() as u64);
            for e in elems {
                encode_jsonb_body(e, out);
            }
        }
        JsonNode::Object(members) => {
            out.push(NTAG_OBJECT);
            write_uvarint(out, members.len() as u64);
            for (k, v) in members {
                out.push(NTAG_STRING);
                write_uvarint(out, k.len() as u64);
                out.extend_from_slice(k.as_bytes());
                encode_jsonb_body(v, out);
            }
        }
    }
}

/// Deserialize a `jsonb` node from `buf` at `pos` (inverse of [`encode_jsonb_body`]). A nonzero
/// flag nibble, the reserved `NTAG_STRING_DICT` (no dictionary slice yet), or an unknown kind is
/// `XX001` data_corrupted (spec/design/json.md §3.1/§6.3).
fn decode_jsonb_body(buf: &[u8], pos: &mut usize) -> Result<JsonNode> {
    let tag = read_u8(buf, pos)?;
    if tag & 0xf0 != 0 {
        return Err(corrupt("jsonb node tag has a reserved flag bit set"));
    }
    match tag & 0x0f {
        x if x == NTAG_NULL => Ok(JsonNode::Null),
        x if x == NTAG_FALSE => Ok(JsonNode::Bool(false)),
        x if x == NTAG_TRUE => Ok(JsonNode::Bool(true)),
        x if x == NTAG_NUMBER => match decode_decimal_body(buf, pos)? {
            Value::Decimal(d) => Ok(JsonNode::Number(d)),
            _ => unreachable!("decode_decimal_body returns a decimal"),
        },
        x if x == NTAG_STRING => Ok(JsonNode::String(decode_jsonb_string(buf, pos)?)),
        x if x == NTAG_STRING_DICT => Err(corrupt(
            "jsonb string-dictionary reference before the dictionary slice",
        )),
        x if x == NTAG_ARRAY => {
            let count = read_uvarint(buf, pos)? as usize;
            let mut elems = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                elems.push(decode_jsonb_body(buf, pos)?);
            }
            Ok(JsonNode::Array(elems))
        }
        x if x == NTAG_OBJECT => {
            let count = read_uvarint(buf, pos)? as usize;
            let mut members = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                // Each member's key is a string node (NTAG_STRING / reserved NTAG_STRING_DICT).
                let ktag = read_u8(buf, pos)?;
                if ktag & 0xf0 != 0 {
                    return Err(corrupt("jsonb object key tag has a reserved flag bit set"));
                }
                let key = match ktag & 0x0f {
                    x if x == NTAG_STRING => decode_jsonb_string(buf, pos)?,
                    x if x == NTAG_STRING_DICT => {
                        return Err(corrupt(
                            "jsonb string-dictionary reference before the dictionary slice",
                        ));
                    }
                    _ => return Err(corrupt("jsonb object key is not a string node")),
                };
                let val = decode_jsonb_body(buf, pos)?;
                members.push((key, val));
            }
            Ok(JsonNode::Object(members))
        }
        _ => Err(corrupt("unknown jsonb node tag")),
    }
}

/// Read a `NTAG_STRING` payload (`varint len ‖ UTF-8 bytes`) after its tag has been consumed.
fn decode_jsonb_string(buf: &[u8], pos: &mut usize) -> Result<String> {
    let len = read_uvarint(buf, pos)? as usize;
    let bytes = take(buf, pos, len)?.to_vec();
    String::from_utf8(bytes).map_err(|_| corrupt("non-UTF-8 jsonb string"))
}

/// The scalar value codec (the body of [`encode_value`] for a `ColType::Scalar`).
fn encode_scalar(ty: ScalarType, v: &Value) -> Vec<u8> {
    match v {
        Value::Null => encode_nullable(ty, None),
        Value::Int(n) => encode_nullable(ty, Some(*n)),
        // Timestamps store their i64 microsecond instant via the same fixed-width codec as
        // i64 (the sentinels are ordinary extreme values; spec/design/timestamp.md).
        Value::Timestamp(m) | Value::Timestamptz(m) => encode_nullable(ty, Some(*m)),
        // A date stores its i32 day count via the same fixed-width (4-byte) order-preserving
        // codec as i32 (the sentinels are ordinary extreme values; spec/design/date.md).
        Value::Date(d) => encode_nullable(ty, Some(*d as i64)),
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
        // Interval: a fixed 16-byte body — i32 months, i32 days, i64 micros, all big-endian
        // two's-complement (NO sign-flip; this is a value codec, not an order-preserving key)
        // — spec/fileformat/format.md.
        Value::Interval(iv) => {
            let mut out = Vec::with_capacity(1 + 16);
            out.push(0x00); // present
            out.extend_from_slice(&iv.months.to_be_bytes());
            out.extend_from_slice(&iv.days.to_be_bytes());
            out.extend_from_slice(&iv.micros.to_be_bytes());
            out
        }
        Value::Bool(b) => vec![0x00, u8::from(*b)], // present tag + bool-byte (0x00 false, 0x01 true)
        // Float value codec (spec/fileformat/format.md, spec/design/float.md §10): present tag,
        // then the IEEE bytes big-endian (f64 = 8, f32 = 4), no length prefix. The stored
        // bits are VERBATIM for every value EXCEPT NaN: a -0.0 keeps its sign bit and ±Inf/finite
        // keep theirs, but a NaN is canonicalized to the single quiet pattern (0x7FF8…000 /
        // 0x7FC00000). A NaN's payload is core-specific (hardware Inf-Inf is the negative 0xFFF8…),
        // so the codec re-canonicalizes it to keep a stored NaN cross-core byte-identical and stop
        // an exempt/computed NaN contaminating in-contract storage (determinism.md §4). The -0→+0
        // collapse is a comparison/key concern (§3), NOT applied here.
        Value::Float64(f) => {
            let bits = if f.is_nan() {
                0x7ff8_0000_0000_0000_u64
            } else {
                f.to_bits()
            };
            let mut out = Vec::with_capacity(1 + 8);
            out.push(0x00); // present
            out.extend_from_slice(&bits.to_be_bytes());
            out
        }
        Value::Float32(f) => {
            let bits = if f.is_nan() {
                0x7fc0_0000_u32
            } else {
                f.to_bits()
            };
            let mut out = Vec::with_capacity(1 + 4);
            out.push(0x00); // present
            out.extend_from_slice(&bits.to_be_bytes());
            out
        }
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
        // A composite value is encoded by `encode_value`'s composite arm, never here.
        Value::Composite(_) => panic!("BUG: a composite value reached the scalar codec"),
        // An array value is encoded by `encode_value`'s array arm, never here.
        Value::Array(_) => panic!("BUG: an array value reached the scalar codec"),
        // A range value is not storable yet (R2 adds the range codec); it never reaches here.
        Value::Range(_) => panic!("BUG: a range value reached the scalar codec (R2)"),
        Value::JsonPath(_) => panic!("BUG: a jsonpath value reached the scalar codec"),
        // json: the verbatim text body, length-prefixed exactly like `text` (spec/design/json.md §4).
        Value::Json(s) => {
            let bytes = s.as_bytes();
            let mut out = Vec::with_capacity(3 + bytes.len());
            out.push(0x00); // present
            out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
            out.extend_from_slice(bytes);
            out
        }
        // jsonb: present tag, then the self-delimiting tagged-node tree (spec/design/json.md §2).
        Value::Jsonb(n) => {
            let mut out = vec![0x00]; // present
            encode_jsonb_body(n, &mut out);
            out
        }
    }
}

/// Whether a value of this type can spill out-of-line (a variable-length type). Fixed-width scalars
/// (`int*`/`boolean`/`uuid`/`timestamp*`) are tiny and always stay inline
/// (spec/design/large-values.md §12). A **composite** is treated as spillable — its opaque inline
/// body spills via the same overflow + LZ4 path when a record exceeds `RECORD_MAX`
/// (spec/design/composite.md §4); a small composite is never actually chosen by the plan.
fn is_spillable(ty: &ColType) -> bool {
    match ty {
        // json/jsonb are variable-length document bodies that ride the same overflow + LZ4 path
        // as text/bytea when a record exceeds RECORD_MAX (spec/design/json.md §2/§4).
        ColType::Scalar(s) => {
            s.is_text() || s.is_bytea() || s.is_decimal() || s.is_json() || s.is_jsonb()
        }
        ColType::Composite { .. } => true,
        // An array's opaque inline body spills via the same overflow + LZ4 path
        // (spec/design/array.md §4); a small array is never actually chosen by the plan.
        ColType::Array(_) => true,
        // A range's body is its flags byte + bound bodies; a `numrange` over huge decimals could
        // exceed RECORD_MAX, so it rides the same overflow + LZ4 path. A discrete range (tiny,
        // fixed-width bounds) is never actually chosen by the plan (spec/design/ranges.md §4).
        ColType::Range(_) => true,
    }
}

/// Whether any column of this row shape can ever spill — the cheap gate that keeps the
/// overflow-page cost walk (`overflow_page_count`) off tables that cannot have chains.
pub(crate) fn any_spillable(col_types: &[ColType]) -> bool {
    col_types.iter().any(is_spillable)
}

/// Like [`any_spillable`], but only over the columns a query's touched set selects — the gate for
/// the masked scan-units walk (cost.md §3 "The touched set"): if no *touched* column can spill,
/// the whole walk yields zero and is skipped.
pub(crate) fn any_spillable_masked(col_types: &[ColType], mask: &[bool]) -> bool {
    col_types
        .iter()
        .zip(mask.iter())
        .any(|(ty, &m)| m && is_spillable(ty))
}

/// The largest a single record may serialize to and still satisfy the B-tree split contract —
/// `RECORD_MAX = (C-12)/2` where `C = cap` is the page payload (spec/fileformat/format.md
/// "Why the record cap"). The spill planner reduces a record to ≤ this by externalizing values.
fn record_max(cap: usize) -> usize {
    cap.saturating_sub(INTERIOR_RESERVE) / 2
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
fn plan_dispositions(col_types: &[ColType], key: &[u8], row: &[Value], cap: usize) -> RecordPlan {
    let inline: Vec<usize> = col_types
        .iter()
        .zip(row.iter())
        .map(|(ty, v)| encode_value(ty, v).len())
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
            is_spillable(&col_types[i])
                && !matches!(row[i], Value::Null)
                && value_payload(&col_types[i], &row[i]).len() >= S_COMPRESS
        })
        .collect();
    cand.sort_by(|&a, &b| inline[b].cmp(&inline[a]).then(a.cmp(&b)));
    for i in cand {
        if size <= max {
            break;
        }
        let payload = value_payload(&col_types[i], &row[i]);
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
            is_spillable(&col_types[i])
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
pub(crate) fn record_size(col_types: &[ColType], key: &[u8], row: &Row, cap: usize) -> usize {
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
    col_types: &[ColType],
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
                units.pages += value_payload(&col_types[i], &row[i]).len().div_ceil(cap);
            }
            Disp::InlineComp(_) => {
                units.decompress += value_payload(&col_types[i], &row[i]).len().div_ceil(cap);
            }
            Disp::ExternalComp(c) => {
                units.pages += c.len().div_ceil(cap);
                units.decompress += value_payload(&col_types[i], &row[i]).len().div_ceil(cap);
            }
        }
    }
    units
}

/// The `value_compress` slabs storing this record costs — one `ceil(raw_len / cap)` block per
/// pass-1 compression attempt, adopted or not (cost.md §3; large-values.md §13). Charged once
/// per stored row version at the statement's write site, never for B-tree re-encodes.
pub(crate) fn record_compress_units(
    col_types: &[ColType],
    key: &[u8],
    row: &Row,
    cap: usize,
) -> usize {
    plan_dispositions(col_types, key, row, cap).compress_units
}

/// A value's **content payload** `P(v)` — the bytes stored in the overflow chain when the value is
/// externalized (spec/design/large-values.md §12): raw UTF-8 for `text`, raw bytes for `bytea`, the
/// decimal body (`flags | scale | ndigits | groups`) for `decimal`. Only spillable types reach here.
fn value_payload(ty: &ColType, v: &Value) -> Vec<u8> {
    match (ty, v) {
        (ColType::Scalar(_), Value::Text(s)) => s.as_bytes().to_vec(),
        (ColType::Scalar(_), Value::Bytea(b)) => b.clone(),
        // json's payload is the verbatim UTF-8 (no length prefix — the chain tracks its own length,
        // exactly like text); jsonb's payload is the tagged-node tree body (spec/design/json.md §4/§2).
        (ColType::Scalar(_), Value::Json(s)) => s.as_bytes().to_vec(),
        (ColType::Scalar(_), Value::Jsonb(n)) => {
            let mut out = Vec::new();
            encode_jsonb_body(n, &mut out);
            out
        }
        // The decimal inline body is the encoding minus its leading presence tag.
        (ColType::Scalar(s), Value::Decimal(_)) => encode_scalar(*s, v)[1..].to_vec(),
        // A composite's payload is its body — the encoding minus the leading presence tag, i.e.
        // the null bitmap + present-field bodies (spec/design/composite.md §4).
        (ColType::Composite { fields, .. }, Value::Composite(vals)) => {
            encode_composite_body(fields, vals)
        }
        // An array's payload is its body (the ndim/flags/dims header + bitmap + element bodies);
        // a large array spills through the same overflow + LZ4 path (spec/design/array.md §4).
        (ColType::Array(elem), Value::Array(arr)) => encode_array_body(elem, arr),
        // A range's payload is its body (the flags byte + present bound bodies, spec/design/ranges.md §4).
        (ColType::Range(elem), Value::Range(rv)) => encode_range_body(elem, rv),
        _ => unreachable!("only spillable values are externalized"),
    }
}

/// Reconstruct a value from the `P(v)` content payload gathered from its overflow chain (inverse of
/// [`value_payload`]) — spec/design/large-values.md §12.
fn value_from_payload(ty: &ColType, payload: &[u8]) -> Result<Value> {
    match ty {
        ColType::Scalar(s) if s.is_text() => {
            let str =
                String::from_utf8(payload.to_vec()).map_err(|_| corrupt("non-UTF-8 text value"))?;
            Ok(Value::Text(str))
        }
        ColType::Scalar(s) if s.is_bytea() => Ok(Value::Bytea(payload.to_vec())),
        ColType::Scalar(s) if s.is_json() => {
            let str =
                String::from_utf8(payload.to_vec()).map_err(|_| corrupt("non-UTF-8 json value"))?;
            Ok(Value::Json(str))
        }
        ColType::Scalar(s) if s.is_jsonb() => {
            let mut pos = 0usize;
            Ok(Value::Jsonb(decode_jsonb_body(payload, &mut pos)?))
        }
        ColType::Scalar(s) if s.is_decimal() => {
            let mut pos = 0usize;
            decode_decimal_body(payload, &mut pos)
        }
        // A composite's payload is its body (bitmap + present-field bodies); decode it with a
        // fresh cursor (spec/design/composite.md §4).
        ColType::Composite { .. } => {
            let mut pos = 0usize;
            read_composite_body(ty, payload, &mut pos)
        }
        // An array's payload is its body; decode it with a fresh cursor (spec/design/array.md §4).
        ColType::Array(elem) => {
            let mut pos = 0usize;
            read_array_body(elem, payload, &mut pos)
        }
        // A range's payload is its body; decode it with a fresh cursor (spec/design/ranges.md §4).
        ColType::Range(elem) => {
            let mut pos = 0usize;
            read_range_body(elem, payload, &mut pos)
        }
        _ => Err(corrupt("a non-spillable type was stored external")),
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
        if !ps.is_power_of_two() {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "page size must be a power of two",
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
        let mut index_roots: Vec<Vec<u32>> = vec![Vec::new(); tables.len()];
        let mut next_index = ROOT_PAGE;
        for (ti, (_, table, store)) in tables.iter().enumerate() {
            if let Some(root) = store.tree_root() {
                root_data_page[ti] =
                    serialize_node(root, store.col_types(), cap, &mut next_index, &mut body)?;
            }
            // The table's index trees follow its data tree, in catalog (name) order
            // (spec/fileformat/format.md "From-scratch image"). Index records are the key alone —
            // no value columns, so they encode against an empty `col_types`.
            for idx in &table.indexes {
                let r = if idx.kind == IndexKind::Gist {
                    // GiST: the on-disk form is the R-tree (pages 5/6), not the flat leaf store
                    // (gist.md §4.1). Serialize the canonical tree, allocating from the same counter.
                    let (gpages, root) = {
                        let mut alloc = || {
                            let p = next_index;
                            next_index += 1;
                            p
                        };
                        serialize_gist_index(self, table, idx, &mut alloc)?
                    };
                    for p in gpages {
                        body.push((p.page_no, p.page_type, p.item_count, 0, p.payload));
                    }
                    root
                } else {
                    let istore = self.index_store(&idx.name.to_ascii_lowercase());
                    match istore.tree_root() {
                        Some(root) => serialize_node(root, &[], cap, &mut next_index, &mut body)?,
                        None => 0,
                    }
                };
                index_roots[ti].push(r);
            }
        }

        // The catalog chain follows the data; its head is the relocatable `root_page`. Each entry
        // is kind-tagged (v9): composite-type entries (kind 1) first in lowercased-name order,
        // then table entries (kind 0) — spec/fileformat/format.md.
        let cat_root = next_index;
        let mut cat_entries: Vec<Vec<u8>> = Vec::new();
        for ct in self.composite_types_sorted() {
            let mut e = vec![1u8];
            e.extend_from_slice(&composite_type_entry_bytes(ct));
            cat_entries.push(e);
        }
        for s in self.sequences_sorted() {
            let mut e = vec![2u8];
            e.extend_from_slice(&sequence_entry_bytes(s));
            cat_entries.push(e);
        }
        // Collation reference entries (kind 3, v18) — after sequences, before tables, so a collated
        // table entry is read after the entry it references. Reference-only: emit one metadata entry
        // per collation the SCHEMA references (columns + default), not an imported set
        // (spec/design/collation.md §2/§5).
        let default_coll = self.default_collation();
        for coll in self.referenced_collations()? {
            let mut e = vec![3u8];
            e.extend_from_slice(&collation_entry_bytes(
                &coll,
                default_coll == Some(coll.name.as_str()),
            ));
            cat_entries.push(e);
        }
        for (ti, (_, t, _)) in tables.iter().enumerate() {
            let mut e = vec![0u8];
            e.extend_from_slice(&table_entry_bytes(t, root_data_page[ti], &index_roots[ti]));
            cat_entries.push(e);
        }
        let entry_sizes: Vec<usize> = cat_entries.iter().map(|e| e.len()).collect();
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
            for &ei in group {
                payload.extend_from_slice(&cat_entries[ei]);
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
    col_types: &[ColType],
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
            Child::Resident(n) => serialize_node(n, col_types, cap, next_index, body)?,
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
            col_types,
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
    col_types: &[ColType],
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
            let resolved = resolve_unfetched(&col_types[i], u, &fetch)?;
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
        let mut index_roots: Vec<Vec<u32>> = vec![Vec::new(); tables.len()];
        for (ti, (_, table, store)) in tables.iter().enumerate() {
            if let Some(root) = store.tree_root() {
                root_data_page[ti] = serialize_dirty(
                    root,
                    store.col_types(),
                    cap,
                    ps,
                    &mut alloc,
                    &mut pages,
                    paging,
                )?;
            }
            // The table's index trees follow its data tree, in catalog (name) order — only
            // their dirty nodes are written, like any tree (spec/fileformat/format.md
            // "Allocation & incremental commit"). Index records carry no value columns (empty
            // `col_types`).
            for idx in &table.indexes {
                let r = if idx.kind == IndexKind::Gist {
                    // GiST rewrites its WHOLE R-tree every commit (gist.md §4.1(b)): fresh pages from
                    // the allocator (free-list first), the old tree's pages reclaimed on the next open.
                    let (gpages, root) = {
                        let mut a = || alloc.take();
                        serialize_gist_index(self, table, idx, &mut a)?
                    };
                    for p in gpages {
                        pages.push((
                            p.page_no,
                            make_page(ps, p.page_type, p.item_count, 0, &p.payload),
                        ));
                    }
                    root
                } else {
                    let istore = self.index_store(&idx.name.to_ascii_lowercase());
                    match istore.tree_root() {
                        Some(root) => {
                            serialize_dirty(root, &[], cap, ps, &mut alloc, &mut pages, paging)?
                        }
                        None => 0,
                    }
                };
                index_roots[ti].push(r);
            }
        }

        // The catalog chain is rewritten to fresh pages every commit (table roots move). Allocate
        // its page indices up front — they may be reused free pages, hence not contiguous — so each
        // page can point at the next (`pack` always returns ≥ 1 group, so `cat_pages` is non-empty).
        // Entries are kind-tagged (v9): composite-type entries (kind 1, name order) then table
        // entries (kind 0) — spec/fileformat/format.md.
        let mut cat_entries: Vec<Vec<u8>> = Vec::new();
        for ct in self.composite_types_sorted() {
            let mut e = vec![1u8];
            e.extend_from_slice(&composite_type_entry_bytes(ct));
            cat_entries.push(e);
        }
        for s in self.sequences_sorted() {
            let mut e = vec![2u8];
            e.extend_from_slice(&sequence_entry_bytes(s));
            cat_entries.push(e);
        }
        // Collation reference entries (kind 3, v18) — after sequences, before tables, so a collated
        // table entry is read after the entry it references. Reference-only: emit one metadata entry
        // per collation the SCHEMA references (columns + default), not an imported set
        // (spec/design/collation.md §2/§5).
        let default_coll = self.default_collation();
        for coll in self.referenced_collations()? {
            let mut e = vec![3u8];
            e.extend_from_slice(&collation_entry_bytes(
                &coll,
                default_coll == Some(coll.name.as_str()),
            ));
            cat_entries.push(e);
        }
        for (ti, (_, t, _)) in tables.iter().enumerate() {
            let mut e = vec![0u8];
            e.extend_from_slice(&table_entry_bytes(t, root_data_page[ti], &index_roots[ti]));
            cat_entries.push(e);
        }
        let entry_sizes: Vec<usize> = cat_entries.iter().map(|e| e.len()).collect();
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
            for &ei in group {
                payload.extend_from_slice(&cat_entries[ei]);
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
    col_types: &[ColType],
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
            Child::Resident(n) => serialize_dirty(n, col_types, cap, ps, alloc, pages, paging)?,
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
            let resolved = resolve_for_encode(&node.vals[i], col_types, paging)?;
            let row = resolved.as_ref().unwrap_or(&node.vals[i]);
            payload.extend_from_slice(&encode_record(
                col_types,
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
        if !page_size_valid(page_size) || image.len() < page_size * 2 {
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
                // Each catalog entry is kind-tagged (v9): 1 = a composite-type entry (registered
                // now; its nested refs are validated after the full walk), 0 = a table entry.
                let kind = read_u8(page.payload, &mut pos)?;
                if kind == 1 {
                    let ct = decode_composite_type_entry(page.payload, &mut pos)?;
                    snap.put_type(ct);
                    continue;
                }
                if kind == 2 {
                    // A sequence entry (v12): self-contained, registered directly (no two-pass).
                    let s = decode_sequence_entry(page.payload, &mut pos)?;
                    snap.put_sequence(s);
                    continue;
                }
                if kind == 3 {
                    // A collation snapshot (v17): the baked `.coll` artifact + an `is_default` flag
                    // (spec/design/collation.md §5). Registered directly; the default restores the
                    // per-database default collation.
                    let (coll, is_default) = decode_collation_entry(page.payload, &mut pos)?;
                    if is_default {
                        snap.set_default_collation(Some(coll.name.clone()));
                    }
                    snap.put_collation(std::sync::Arc::new(coll));
                    continue;
                }
                if kind != 0 {
                    return Err(corrupt("unknown catalog entry kind"));
                }
                let (table, root_data_page, index_roots) =
                    decode_table_entry(page.payload, &mut pos)?;
                let name = table.name.clone();
                let has_pk = !table.pk_indices().is_empty();
                let indexes = table.indexes.clone();
                snap.put_table(table, page_size as u32);
                // The store resolved each column's `ColType` from the (types-first) catalog at
                // `put_table`; the codec reads it back rather than re-walking the type catalog.
                let col_types = snap.store(&name).col_types().to_vec();
                if root_data_page != 0 {
                    let (root, len) =
                        read_tree(image, page_size, root_data_page, &col_types, &mut reached)?;
                    let store = snap.store_mut(&name);
                    store.set_tree(Some(root), len);
                    // No-PK keys are synthetic i64 rowids — advance the counter past the largest
                    // (the last entry in key order) so future inserts don't collide.
                    if !has_pk {
                        // In-memory load (no paging) — `iter_entries` never faults, so `?` is inert.
                        let entries = store.iter_entries()?;
                        if let Some((k, _)) = entries.last() {
                            store.bump_rowid_to(decode_int(ScalarType::Int64, k) + 1);
                        }
                    }
                }
                // The table's index trees (v5): zero-column stores of entry keys
                // (spec/design/indexes.md §3), reachable pages included in the walk.
                for (idx, &iroot) in indexes.iter().zip(&index_roots) {
                    let cap = page_size - PAGE_HEADER;
                    let mut istore = TableStore::new(cap, Vec::new());
                    if iroot != 0 {
                        if idx.kind == IndexKind::Gist {
                            // GiST: parse the persisted R-tree (pages 5/6), marking its pages
                            // reached, and recover its leaf keys to repopulate the flat leaf store.
                            // The resident R-tree is rebuilt canonically below (rebuild_gist_trees).
                            let mut keys = Vec::new();
                            read_gist_leaf_keys(
                                &|p| {
                                    let pg = read_page(image, page_size, p)?;
                                    Ok((pg.page_type, pg.item_count, pg.payload.to_vec()))
                                },
                                iroot,
                                &mut reached,
                                &mut keys,
                            )?;
                            for k in keys {
                                istore.insert(k, Vec::new())?;
                            }
                        } else {
                            let (root, len) =
                                read_tree(image, page_size, iroot, &[], &mut reached)?;
                            istore.set_tree(Some(root), len);
                        }
                    }
                    snap.put_index_store(idx.name.to_ascii_lowercase(), istore);
                }
            }
            cat_page = page.next_page;
        }
        // Two-pass: validate the composite-type catalog (existence + acyclicity) now that every
        // type entry has been read (spec/design/composite.md §3); a bad reference is XX001.
        snap.validate_composite_types()?;
        // Build each GiST index's resident R-tree from its now-loaded leaf store (gist.md §4.1).
        snap.rebuild_gist_trees()?;
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
        if !page_size_valid(page_size) {
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
                // Each catalog entry is kind-tagged (v9): 1 = a composite-type entry (registered
                // now; its nested refs are validated after the full walk), 0 = a table entry.
                let kind = read_u8(page.payload, &mut pos)?;
                if kind == 1 {
                    let ct = decode_composite_type_entry(page.payload, &mut pos)?;
                    snap.put_type(ct);
                    continue;
                }
                if kind == 2 {
                    // A sequence entry (v12): self-contained, registered directly (no two-pass).
                    let s = decode_sequence_entry(page.payload, &mut pos)?;
                    snap.put_sequence(s);
                    continue;
                }
                if kind == 3 {
                    // A collation snapshot (v17): the baked `.coll` artifact + an `is_default` flag
                    // (spec/design/collation.md §5). Registered directly; the default restores the
                    // per-database default collation.
                    let (coll, is_default) = decode_collation_entry(page.payload, &mut pos)?;
                    if is_default {
                        snap.set_default_collation(Some(coll.name.clone()));
                    }
                    snap.put_collation(std::sync::Arc::new(coll));
                    continue;
                }
                if kind != 0 {
                    return Err(corrupt("unknown catalog entry kind"));
                }
                let (table, root_data_page, index_roots) =
                    decode_table_entry(page.payload, &mut pos)?;
                let name = table.name.clone();
                let has_pk = !table.pk_indices().is_empty();
                let indexes = table.indexes.clone();
                snap.put_table(table, page_size as u32);
                snap.store_mut(&name).attach_paging(paging.clone());
                // The store resolved each column's `ColType` from the (types-first) catalog at
                // `put_table` (spec/design/composite.md §3).
                let col_types = snap.store(&name).col_types().to_vec();
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
                // The table's index trees (v5): zero-column demand-paged stores of entry
                // keys (spec/design/indexes.md §3); no spillable columns, so no overflow
                // collection is ever needed.
                for (idx, &iroot) in indexes.iter().zip(&index_roots) {
                    let cap = page_size - PAGE_HEADER;
                    let mut istore = TableStore::new(cap, Vec::new());
                    if iroot != 0 {
                        if idx.kind == IndexKind::Gist {
                            // GiST is EAGER-loaded, not demand-paged (gist.md §4.1(a)): read the
                            // whole R-tree (marking pages reached), recover its leaf keys into a
                            // fully-resident leaf store. The resident R-tree is rebuilt below.
                            let mut keys = Vec::new();
                            read_gist_leaf_keys(
                                &|p| {
                                    let block = paging.pager().read_block(p)?;
                                    let pg = parse_page(&block)?;
                                    Ok((pg.page_type, pg.item_count, pg.payload.to_vec()))
                                },
                                iroot,
                                &mut reached,
                                &mut keys,
                            )?;
                            for k in keys {
                                istore.insert(k, Vec::new())?;
                            }
                        } else {
                            istore.attach_paging(paging.clone());
                            let (root, len) = read_skeleton(&paging, iroot, &[], &mut reached)?;
                            istore.set_tree(Some(root), len);
                        }
                    } else {
                        istore.attach_paging(paging.clone());
                    }
                    snap.put_index_store(idx.name.to_ascii_lowercase(), istore);
                }
            }
            cat_page = page.next_page;
        }

        // Two-pass: validate the composite-type catalog (existence + acyclicity) — XX001 on a bad
        // reference (spec/design/composite.md §3).
        snap.validate_composite_types()?;
        // Build each GiST index's resident R-tree from its eager-loaded leaf store (gist.md §4.1).
        snap.rebuild_gist_trees()?;
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
    col_types: &[ColType],
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
    col_types: &[ColType],
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
    col_types: &[ColType],
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
    col_types: &[ColType],
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

/// Build a GiST index's canonical R-tree from its leaf-key store and serialize it to node pages
/// (spec/design/gist.md §3/§4.1). The on-disk form of a GiST index is the R-tree (page types 5/6),
/// NOT the flat leaf-key B-tree the in-memory index store holds — so the index store is never
/// serialized for a GiST index; this is. The tree is rebuilt **canonically** (`build_from_leaf_keys`)
/// from the leaf set, so its bytes are a pure function of the set — content-deterministic and
/// cross-core identical (§3); the whole tree is rewritten every commit (§4.1(b)). `alloc` hands out
/// page numbers (a counter for the whole image, the free-list allocator for an incremental commit).
/// Returns the node pages + the root page; an empty index returns no pages and root `0` (the
/// empty-index convention shared with ordinary indexes).
fn serialize_gist_index<A: FnMut() -> u32>(
    snap: &Snapshot,
    table: &Table,
    idx: &IndexDef,
    alloc: &mut A,
) -> Result<(Vec<crate::gist::GistPage>, u32)> {
    let op = crate::gist::opclass_for(&table.columns[idx.columns[0]].ty);
    let istore = snap.index_store(&idx.name.to_ascii_lowercase());
    let keys: Vec<Vec<u8>> = istore.iter_entries()?.into_iter().map(|(k, _)| k).collect();
    if keys.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let tree = crate::gist::build_from_leaf_keys(&op, keys.iter().map(|k| k.as_slice()))?;
    Ok(crate::gist::serialize_tree(&tree, &op, alloc))
}

/// Walk a persisted GiST R-tree (rooted at `root`, page types 5/6 — spec/design/gist.md §4.1),
/// marking every node page in `reached` (so the free-list keeps the live tree) and collecting each
/// leaf's **leaf key** (`bound ‖ skey` — the bound bytes concatenated with the storage key,
/// recovered by re-joining the two length-prefixed fields). The bound bytes are the opclass's
/// self-delimiting form (`range_ops`' range body / the scalar `=` opclass's `[min,max]` key blob),
/// copied verbatim — so this walk is **opclass-agnostic** (no element type needed). The leaf keys
/// repopulate the in-memory leaf-key store (the maintenance source of truth); the resident R-tree
/// the planner descends is rebuilt canonically from them afterward (`Snapshot::rebuild_gist_trees`).
/// `read` returns one page's `(page_type, item_count, payload)`, reading from the whole image or
/// through the pager.
fn read_gist_leaf_keys<F>(
    read: &F,
    page_no: u32,
    reached: &mut HashSet<u32>,
    out: &mut Vec<Vec<u8>>,
) -> Result<()>
where
    F: Fn(u32) -> Result<(u8, u32, Vec<u8>)>,
{
    reached.insert(page_no);
    let (page_type, n, payload) = read(page_no)?;
    let mut pos = 0usize;
    match page_type {
        t if t == crate::gist::PAGE_GIST_LEAF => {
            for _ in 0..n {
                let blen = read_u16(&payload, &mut pos)? as usize;
                let bound = take_bytes(&payload, &mut pos, blen)?;
                let slen = read_u16(&payload, &mut pos)? as usize;
                let skey = take_bytes(&payload, &mut pos, slen)?;
                let mut key = bound;
                key.extend_from_slice(&skey);
                out.push(key);
            }
            Ok(())
        }
        t if t == crate::gist::PAGE_GIST_INTERIOR => {
            // Collect child pages first (the payload borrow ends before recursing).
            let mut children = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let blen = read_u16(&payload, &mut pos)? as usize;
                let _ = take_bytes(&payload, &mut pos, blen)?; // skip the union bound
                children.push(read_u32(&payload, &mut pos)?);
            }
            for cp in children {
                read_gist_leaf_keys(read, cp, reached, out)?;
            }
            Ok(())
        }
        _ => Err(corrupt("expected a GiST node page")),
    }
}

/// Copy `n` bytes from `buf` at `*pos`, advancing it (bounds-checked) — the byte-slice analogue of
/// `read_u16`/`read_u32` used by the GiST node walk.
fn take_bytes(buf: &[u8], pos: &mut usize, n: usize) -> Result<Vec<u8>> {
    if *pos + n > buf.len() {
        return Err(corrupt("truncated GiST node payload"));
    }
    let v = buf[*pos..*pos + n].to_vec();
    *pos += n;
    Ok(v)
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
    col_types: &[ColType],
    key: &[u8],
    row: &[Value],
    cap: usize,
    take: &mut dyn FnMut() -> u32,
    ovf: &mut Vec<OverflowPageOut>,
) -> Vec<u8> {
    let plan = plan_dispositions(col_types, key, row, cap);
    let mut out = Vec::new();
    out.extend_from_slice(&(key.len() as u16).to_be_bytes());
    out.extend_from_slice(key);
    for (i, (ty, val)) in col_types.iter().zip(row.iter()).enumerate() {
        match &plan.disp[i] {
            Disp::Inline => out.extend_from_slice(&encode_value(ty, val)),
            Disp::External => {
                let payload = value_payload(ty, val);
                let first = write_overflow_chain(&payload, cap, take, ovf);
                out.push(TAG_EXTERNAL);
                out.extend_from_slice(&first.to_be_bytes());
                out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            }
            Disp::InlineComp(comp) => {
                let raw_len = value_payload(ty, val).len();
                out.push(TAG_INLINE_COMP);
                out.extend_from_slice(&(raw_len as u32).to_be_bytes());
                out.extend_from_slice(&(comp.len() as u16).to_be_bytes());
                out.extend_from_slice(comp);
            }
            Disp::ExternalComp(comp) => {
                // The chain carries the COMPRESSED block (its page count follows comp size).
                let raw_len = value_payload(ty, val).len();
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
fn table_entry_bytes(table: &Table, root_data_page: u32, index_roots: &[u32]) -> Vec<u8> {
    let mut out = Vec::new();
    let name = table.name.as_bytes();
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&(table.columns.len() as u16).to_be_bytes());
    for col in &table.columns {
        let cn = col.name.as_bytes();
        out.extend_from_slice(&(cn.len() as u16).to_be_bytes());
        out.extend_from_slice(cn);
        // bit0 (primary_key through v4) is RETIRED in v5 — the pk ordinal list below is the
        // single authority; the bit is reserved, written 0 (spec/fileformat/format.md).
        match &col.ty {
            Type::Composite(r) => {
                // A composite column (v9): type_code 14, then flags, then the type name in the
                // typmod slot (spec/fileformat/format.md). Composite columns carry no default this
                // slice, so flags bits 2/3 are 0.
                out.push(14);
                let flags = if col.not_null { 0b10u8 } else { 0 };
                out.push(flags);
                let tn = r.name.as_bytes();
                out.extend_from_slice(&(tn.len() as u16).to_be_bytes());
                out.extend_from_slice(tn);
            }
            Type::Array(elem) => {
                // An array column (v10): type_code 15, flags, then the element type descriptor
                // (spec/design/array.md §3). Arrays carry no default this slice (flags bits 2/3 = 0).
                out.push(15);
                let flags = if col.not_null { 0b10u8 } else { 0 };
                out.push(flags);
                push_array_element_type(&mut out, elem);
            }
            Type::Range(elem) => {
                // A range column (v16): type_code 17, flags, then the element type descriptor — one
                // scalar code (spec/design/ranges.md §3). Ranges carry no default this slice (flags
                // bits 2/3 = 0), so the entry is type_code ‖ flags ‖ element_code.
                out.push(17);
                let flags = if col.not_null { 0b10u8 } else { 0 };
                out.push(flags);
                push_range_element_type(&mut out, elem);
            }
            Type::Scalar(s) => {
                out.push(type_code_for_scalar(*s));
                let mut flags = 0u8;
                if col.not_null {
                    flags |= 0b10;
                }
                if col.default.is_some() {
                    flags |= 0b100;
                }
                if col.default_expr.is_some() {
                    // bit3 default_is_expr (v8) — mutually exclusive with bit2 (a column has at
                    // most one of a constant or an expression default — spec/fileformat/format.md).
                    flags |= 0b1000;
                }
                // bit4 is_identity + bit5 identity_always (v15) — an IDENTITY column also carries
                // not_null (bit1) + the nextval expression default (bit3) — spec/design/sequences.md §13.
                match col.identity {
                    Some(IdentityKind::Always) => flags |= 0b11_0000,
                    Some(IdentityKind::ByDefault) => flags |= 0b1_0000,
                    None => {}
                }
                // bit6 has_collation (v17) — a text column with a non-`C` effective collation
                // (spec/design/collation.md §5); the name is appended after the default.
                if col.collation.is_some() {
                    flags |= 0b100_0000;
                }
                out.push(flags);
                // A decimal column appends its typmod (precision, scale) — only for type_code 6,
                // so non-decimal entries are byte-unchanged. `precision 0` = unconstrained.
                if s.is_decimal() {
                    let (precision, scale) = match col.decimal {
                        Some(t) => (t.precision, t.scale),
                        None => (0u16, 0u16),
                    };
                    out.extend_from_slice(&precision.to_be_bytes());
                    out.extend_from_slice(&scale.to_be_bytes());
                }
                // A column with a constant DEFAULT (flags bit2) appends its pre-evaluated default
                // value via the value codec rows use — AFTER the typmod, presence-gated. A
                // `DEFAULT NULL` is one 0x01. An EXPRESSION default (flags bit3, v8) instead
                // appends its expr-text (u16 length + UTF-8) there — bit2/bit3 are exclusive.
                if let Some(d) = &col.default {
                    // A column DEFAULT is always a scalar value (composite columns carry no
                    // default this slice — composite.md §12), so encode the scalar body directly.
                    out.extend_from_slice(&encode_scalar(*s, d));
                } else if let Some(de) = &col.default_expr {
                    let et = de.expr_text.as_bytes();
                    out.extend_from_slice(&(et.len() as u16).to_be_bytes());
                    out.extend_from_slice(et);
                }
                // The effective collation name (v17, flags bit6) — last in the per-column entry, so
                // a non-collated column is byte-unchanged (spec/design/collation.md §5).
                if let Some(coll) = &col.collation {
                    let cb = coll.as_bytes();
                    out.extend_from_slice(&(cb.len() as u16).to_be_bytes());
                    out.extend_from_slice(cb);
                }
            }
        }
    }
    // The primary key (v5): count, then the member column ordinals in KEY order
    // (constraints.md §3 — the list persists an order independent of declaration order).
    out.extend_from_slice(&(table.pk.len() as u16).to_be_bytes());
    for &i in &table.pk {
        out.extend_from_slice(&(i as u16).to_be_bytes());
    }
    // CHECK constraints (v4): count, then (name, expression text) per check, in the
    // catalog's evaluation order — the text is written back VERBATIM, so the bytes are
    // stable across create → commit → load → commit (spec/fileformat/format.md
    // "Check-expression text").
    out.extend_from_slice(&(table.checks.len() as u16).to_be_bytes());
    for check in &table.checks {
        let cn = check.name.as_bytes();
        out.extend_from_slice(&(cn.len() as u16).to_be_bytes());
        out.extend_from_slice(cn);
        let ce = check.expr_text.as_bytes();
        out.extend_from_slice(&(ce.len() as u16).to_be_bytes());
        out.extend_from_slice(ce);
    }
    // Secondary indexes (v5): count, then per index the name, key-column ordinals
    // (index-key order, duplicates allowed), the v6 flags byte (bit0 unique —
    // spec/design/indexes.md §8), and its tree's root page — in the catalog's ascending
    // lowercased-name order (spec/design/indexes.md §6).
    debug_assert_eq!(table.indexes.len(), index_roots.len());
    out.extend_from_slice(&(table.indexes.len() as u16).to_be_bytes());
    for (idx, &root) in table.indexes.iter().zip(index_roots) {
        let inm = idx.name.as_bytes();
        out.extend_from_slice(&(inm.len() as u16).to_be_bytes());
        out.extend_from_slice(inm);
        out.extend_from_slice(&(idx.columns.len() as u16).to_be_bytes());
        for &c in &idx.columns {
            out.extend_from_slice(&(c as u16).to_be_bytes());
        }
        out.push(if idx.unique { 1 } else { 0 });
        // v13: index_kind byte (0 = ordered B-tree, 1 = GIN — spec/design/gin.md §7).
        out.push(idx.kind as u8);
        out.extend_from_slice(&root.to_be_bytes());
    }
    // Foreign keys (v11): count, then per FK the name, the local-column ordinals (into THIS
    // table, list order), the referenced table name, the referenced-column ordinals (into the
    // PARENT, list order), and the actions byte (bits 0-1 on_delete, bits 2-3 on_update) — in the
    // catalog's ascending lowercased-name order (spec/design/constraints.md §6.9). An FK owns no
    // B-tree (no root page).
    out.extend_from_slice(&(table.foreign_keys.len() as u16).to_be_bytes());
    for fk in &table.foreign_keys {
        let fnm = fk.name.as_bytes();
        out.extend_from_slice(&(fnm.len() as u16).to_be_bytes());
        out.extend_from_slice(fnm);
        out.extend_from_slice(&(fk.columns.len() as u16).to_be_bytes());
        for &c in &fk.columns {
            out.extend_from_slice(&(c as u16).to_be_bytes());
        }
        let rt = fk.ref_table.as_bytes();
        out.extend_from_slice(&(rt.len() as u16).to_be_bytes());
        out.extend_from_slice(rt);
        out.extend_from_slice(&(fk.ref_columns.len() as u16).to_be_bytes());
        for &c in &fk.ref_columns {
            out.extend_from_slice(&(c as u16).to_be_bytes());
        }
        out.push(fk_action_code(fk.on_delete) | (fk_action_code(fk.on_update) << 2));
    }
    out.extend_from_slice(&root_data_page.to_be_bytes());
    out
}

/// The 2-bit on-disk code for a referential action (format.md): NO ACTION = 0, RESTRICT = 1.
fn fk_action_code(a: FkAction) -> u8 {
    match a {
        FkAction::NoAction => 0,
        FkAction::Restrict => 1,
    }
}

/// Decode a 2-bit referential-action code; an unsupported code (2/3, reserved for the deferred
/// write-actions) in an otherwise-valid file is `XX001`.
fn fk_action_from_code(c: u8) -> Result<FkAction> {
    match c {
        0 => Ok(FkAction::NoAction),
        1 => Ok(FkAction::Restrict),
        _ => Err(corrupt("unsupported foreign-key action code")),
    }
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
    // The per-page checksum (v7) is computed last, over every byte but its own field at [12,16).
    let crc = page_crc(&p);
    p[12..16].copy_from_slice(&crc.to_be_bytes());
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
    // Verify the per-page checksum (v7) before trusting any header field — a mismatch is silent
    // at-rest corruption (spec/fileformat/format.md *Page header*; storage.md §6).
    let stored = u32::from_be_bytes([block[12], block[13], block[14], block[15]]);
    if page_crc(block) != stored {
        return Err(corrupt("page checksum mismatch (corrupted page)"));
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
pub(crate) fn decode_leaf_node(block: &[u8], page: u32, col_types: &[ColType]) -> Result<Node> {
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

/// Decode one catalog table entry: the `Table` (its pk list, checks, and index definitions
/// included), its `root_data_page`, and each index's root page (parallel to
/// `Table::indexes`).
/// Serialize a composite-type catalog entry's BODY (after its `entry_kind = 1` byte): name,
/// field count, then per field — name, type code, [type name when code 14 (nested composite)],
/// flags (bit0 `not_null`), [decimal typmod when code 6] (spec/fileformat/format.md
/// *Composite-type entry*).
fn composite_type_entry_bytes(ct: &CompositeType) -> Vec<u8> {
    let mut out = Vec::new();
    let name = ct.name.as_bytes();
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&(ct.fields.len() as u16).to_be_bytes());
    for f in &ct.fields {
        let fname = f.name.as_bytes();
        out.extend_from_slice(&(fname.len() as u16).to_be_bytes());
        out.extend_from_slice(fname);
        match &f.ty {
            Type::Composite(r) => {
                out.push(14);
                let tn = r.name.as_bytes();
                out.extend_from_slice(&(tn.len() as u16).to_be_bytes());
                out.extend_from_slice(tn);
            }
            Type::Scalar(s) => out.push(type_code_for_scalar(*s)),
            // An array-typed field (spec/design/array.md §12): type_code 15, then the same
            // inline element-type descriptor an array column uses (§3), placed before the flags
            // byte — mirroring where a nested-composite field's name sits.
            Type::Array(elem) => {
                out.push(15);
                push_array_element_type(&mut out, elem);
            }
            // A range field cannot occur: CREATE TYPE rejects a range field (range columns are not
            // storable yet — R2).
            Type::Range(_) => {
                unreachable!("a composite range field is rejected at CREATE TYPE (R2)")
            }
        }
        out.push(if f.not_null { 0b1 } else { 0 });
        if let Type::Scalar(s) = &f.ty {
            if s.is_decimal() {
                let (p, sc) = match f.decimal {
                    Some(t) => (t.precision, t.scale),
                    None => (0u16, 0u16),
                };
                out.extend_from_slice(&p.to_be_bytes());
                out.extend_from_slice(&sc.to_be_bytes());
            }
        }
    }
    out
}

/// Decode a composite-type catalog entry's body (inverse of `composite_type_entry_bytes`); the
/// caller has already consumed the `entry_kind` byte. Nested composite fields hold the referenced
/// type's NAME (resolved/validated after the whole catalog is read — the two-pass load).
fn decode_composite_type_entry(buf: &[u8], pos: &mut usize) -> Result<CompositeType> {
    let name = read_string(buf, pos)?;
    let field_count = read_u16(buf, pos)? as usize;
    let mut fields = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        let fname = read_string(buf, pos)?;
        let tc = read_u8(buf, pos)?;
        let (ty, mut decimal) = if tc == 14 {
            let tn = read_string(buf, pos)?;
            (
                Type::Composite(crate::types::CompositeRef { name: tn }),
                None,
            )
        } else if tc == 15 {
            // An array-typed field (spec/design/array.md §12): the element-type descriptor, then
            // (below) the flags byte — the inverse of the `Type::Array` arm above.
            let elem = read_array_element_type(buf, pos)?;
            (Type::Array(Box::new(elem)), None)
        } else {
            let s = scalar_for_type_code(tc).ok_or_else(|| corrupt("unknown field type code"))?;
            (Type::Scalar(s), None)
        };
        let flags = read_u8(buf, pos)?;
        if flags & !0b1 != 0 {
            return Err(corrupt("reserved composite field flag set"));
        }
        let not_null = flags & 0b1 != 0;
        if let Type::Scalar(s) = &ty {
            if s.is_decimal() {
                let precision = read_u16(buf, pos)?;
                let scale = read_u16(buf, pos)?;
                decimal = if precision == 0 {
                    None
                } else {
                    Some(DecimalTypmod { precision, scale })
                };
            }
        }
        fields.push(CompositeField {
            name: fname,
            ty,
            decimal,
            not_null,
        });
    }
    Ok(CompositeType { name, fields })
}

/// Serialize a sequence catalog entry's BODY (after its `entry_kind = 2` byte): name, then the six
/// fixed i64 fields (big-endian two's-complement, no sign-flip) and a flags byte — spec/fileformat/
/// format.md *Sequence entry*. Fixed-width, every field present (no presence tags).
fn sequence_entry_bytes(s: &SequenceDef) -> Vec<u8> {
    let mut out = Vec::new();
    let name = s.name.as_bytes();
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&s.increment.to_be_bytes());
    out.extend_from_slice(&s.min_value.to_be_bytes());
    out.extend_from_slice(&s.max_value.to_be_bytes());
    out.extend_from_slice(&s.start.to_be_bytes());
    out.extend_from_slice(&s.cache.to_be_bytes());
    out.extend_from_slice(&s.last_value.to_be_bytes());
    let mut flags = 0u8;
    if s.cycle {
        flags |= 0b1;
    }
    if s.is_called {
        flags |= 0b10;
    }
    if s.owned_by.is_some() {
        flags |= 0b100; // bit2 has_owner (v13)
    }
    out.push(flags);
    // The OWNED BY tail (v13): only present when has_owner — owner table name + column ordinal
    // (spec/design/sequences.md §12, format.md *Sequence entry*).
    if let Some(owner) = &s.owned_by {
        let ot = owner.table.as_bytes();
        out.extend_from_slice(&(ot.len() as u16).to_be_bytes());
        out.extend_from_slice(ot);
        out.extend_from_slice(&owner.column.to_be_bytes());
    }
    out
}

/// Decode a sequence catalog entry's body (inverse of `sequence_entry_bytes`); the caller has
/// already consumed the `entry_kind` byte.
fn decode_sequence_entry(buf: &[u8], pos: &mut usize) -> Result<SequenceDef> {
    let name = read_string(buf, pos)?;
    let increment = read_i64(buf, pos)?;
    let min_value = read_i64(buf, pos)?;
    let max_value = read_i64(buf, pos)?;
    let start = read_i64(buf, pos)?;
    let cache = read_i64(buf, pos)?;
    let last_value = read_i64(buf, pos)?;
    let flags = read_u8(buf, pos)?;
    if flags & !0b111 != 0 {
        return Err(corrupt("reserved sequence flag set"));
    }
    // The OWNED BY tail (v13): present iff bit2 (has_owner) is set.
    let owned_by = if flags & 0b100 != 0 {
        let table = read_string(buf, pos)?;
        let column = read_u16(buf, pos)?;
        Some(crate::catalog::SeqOwner { table, column })
    } else {
        None
    };
    Ok(SequenceDef {
        name,
        increment,
        min_value,
        max_value,
        start,
        cache,
        cycle: flags & 0b1 != 0,
        last_value,
        is_called: flags & 0b10 != 0,
        owned_by,
    })
}

/// Serialize a collation-snapshot catalog entry's BODY (after its `entry_kind = 3` byte, v17): a
/// flags byte (bit0 `is_default`, bit1 `reference` — deferred, always 0/baked this slice), then the
/// baked `.coll` artifact (u32 length + LZ4-compressed bytes) — spec/design/collation.md §5,
/// spec/fileformat/format.md *Collation snapshot entry*. The artifact is byte-identical to
/// `db.save_collation`, so a golden doubles as an artifact fixture.
fn collation_entry_bytes(coll: &Collation, is_default: bool) -> Vec<u8> {
    // Reference-only (format_version 18, collation.md §5): metadata ONLY — the `is_default` flag, the
    // name, the `(unicode, cldr)` version pin, and the description. The compiled table is NOT stored;
    // it is vendored into the binary (§2/§9) and resolved by name on open.
    let mut out = Vec::new();
    out.push(if is_default { 0b1u8 } else { 0 });
    push_string(&mut out, &coll.name);
    push_string(&mut out, &coll.unicode_version);
    push_string(&mut out, &coll.cldr_version);
    push_string(&mut out, &coll.description);
    out
}

/// Decode a collation reference entry's body (inverse of `collation_entry_bytes`); the caller has
/// consumed the `entry_kind` byte. Reads the metadata, then resolves the compiled table from the
/// binary's **vendored** set by name (§2/§9) — the table is no longer in the file. Returns the
/// resolved collation and whether it is the per-database default (the `is_default` flag bit).
fn decode_collation_entry(buf: &[u8], pos: &mut usize) -> Result<(Collation, bool)> {
    let flags = read_u8(buf, pos)?;
    if flags & !0b1 != 0 {
        return Err(corrupt("reserved collation flag set"));
    }
    let is_default = flags & 0b1 != 0;
    let name = read_string(buf, pos)?;
    let unicode_version = read_string(buf, pos)?;
    let cldr_version = read_string(buf, pos)?;
    let description = read_string(buf, pos)?;
    // The file records only the version PIN; the table comes from a loaded bundle (the host must have
    // loaded one providing this collation before opening — collation.md §4/§9). A name no loaded
    // bundle provides at all is the graded verdict's **legible refusal** (slice 2d, collation.md §12 /
    // compatibility.md §7): the open is refused with XX002 naming the collation + version, rather than
    // degrading the rest of the database (the conservative resolution of compatibility.md §12 open #3 —
    // a *version-skewed* collation, by contrast, opens and is enforced read-only at write time §14).
    let loaded = crate::collation::loaded_collation(&name).ok_or_else(|| {
        EngineError::new(
            SqlState::CollationVersionMismatch,
            format!(
                "collation \"{name}\" (@ {unicode_version}/{cldr_version}) is not provided by any loaded bundle"
            ),
        )
    })?;
    let coll = Collation {
        name,
        unicode_version,
        cldr_version,
        description,
        singles: loaded.singles.clone(),
        contractions: loaded.contractions.clone(),
    };
    Ok((coll, is_default))
}

fn decode_table_entry(buf: &[u8], pos: &mut usize) -> Result<(Table, u32, Vec<u32>)> {
    let name = read_string(buf, pos)?;
    let col_count = read_u16(buf, pos)? as usize;
    let mut columns = Vec::with_capacity(col_count);
    for _ in 0..col_count {
        let cname = read_string(buf, pos)?;
        let tc = read_u8(buf, pos)?;
        if tc == 14 {
            // A composite column (v9): flags, then the type name (spec/fileformat/format.md).
            // Forward-ready — composite columns are not produced this slice (composite.md §12),
            // but a reader handles the code so a later-slice file loads cleanly.
            let flags = read_u8(buf, pos)?;
            if flags & 0b01 != 0 {
                return Err(corrupt("reserved column flag bit0 set"));
            }
            let tname = read_string(buf, pos)?;
            columns.push(Column {
                name: cname,
                ty: Type::Composite(crate::types::CompositeRef { name: tname }),
                decimal: None,
                primary_key: false,
                not_null: flags & 0b10 != 0,
                default: None,
                default_expr: None,
                identity: None,
                collation: None,
            });
            continue;
        }
        if tc == 15 {
            // An array column (v10): flags, then the element type descriptor
            // (spec/design/array.md §3). Arrays carry no default this slice.
            let flags = read_u8(buf, pos)?;
            if flags & 0b01 != 0 {
                return Err(corrupt("reserved column flag bit0 set"));
            }
            let elem = read_array_element_type(buf, pos)?;
            columns.push(Column {
                name: cname,
                ty: Type::Array(Box::new(elem)),
                decimal: None,
                primary_key: false,
                not_null: flags & 0b10 != 0,
                default: None,
                default_expr: None,
                identity: None,
                collation: None,
            });
            continue;
        }
        if tc == 17 {
            // A range column (v16): flags, then the element type descriptor — one scalar code
            // (spec/design/ranges.md §3). Ranges carry no default this slice (and never identity).
            let flags = read_u8(buf, pos)?;
            if flags & 0b01 != 0 {
                return Err(corrupt("reserved column flag bit0 set"));
            }
            let elem = read_range_element_type(buf, pos)?;
            columns.push(Column {
                name: cname,
                ty: Type::Range(Box::new(elem)),
                decimal: None,
                primary_key: false,
                not_null: flags & 0b10 != 0,
                default: None,
                default_expr: None,
                identity: None,
                collation: None,
            });
            continue;
        }
        let ty = scalar_for_type_code(tc).ok_or_else(|| corrupt("unknown type code"))?;
        let flags = read_u8(buf, pos)?;
        // bit0 was the primary_key flag through v4; v5 retired it (the pk list below is the
        // authority) and reserves it as must-be-zero. bit6 is has_collation (v17); bit7 reserved.
        if flags & 0b01 != 0 {
            return Err(corrupt("reserved column flag bit0 set"));
        }
        if flags & 0b1000_0000 != 0 {
            return Err(corrupt("reserved column flag bit7 set"));
        }
        // bit4 is_identity + bit5 identity_always (v15) — identity_always is meaningful only with
        // is_identity (spec/design/sequences.md §13).
        if flags & 0b11_0000 == 0b10_0000 {
            return Err(corrupt("identity_always set without is_identity"));
        }
        let identity = if flags & 0b1_0000 != 0 {
            Some(if flags & 0b10_0000 != 0 {
                IdentityKind::Always
            } else {
                IdentityKind::ByDefault
            })
        } else {
            None
        };
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
        // The default follows the typmod (spec/fileformat/format.md): a CONSTANT default
        // (flags bit2) is a value via the same value codec rows use — never externalized, so no
        // overflow reader is needed (a `0x02` tag here would be a corrupt catalog). An
        // EXPRESSION default (flags bit3, v8) is instead the expr-text (u16 length + UTF-8),
        // re-parsed with the ordinary expression parser (XX001 if it fails, like a stored
        // check). The two bits are mutually exclusive — both set is a corrupt catalog.
        if flags & 0b1100 == 0b1100 {
            return Err(corrupt(
                "column has both a constant and an expression default",
            ));
        }
        let default = if flags & 0b100 != 0 {
            let mut sink = Vec::new();
            // A constant default is a scalar value (this branch is the scalar type path).
            Some(read_value(&ColType::Scalar(ty), buf, pos, None, &mut sink)?)
        } else {
            None
        };
        let default_expr = if flags & 0b1000 != 0 {
            let expr_text = read_string(buf, pos)?;
            let expr = crate::parser::parse_expression(&expr_text)
                .map_err(|e| corrupt(&format!("stored default expression does not parse: {e}")))?;
            Some(DefaultExpr { expr_text, expr })
        } else {
            None
        };
        // The effective collation (v17, flags bit6) — appended last; a non-collated column has the
        // bit clear and reads nothing (spec/design/collation.md §5).
        let collation = if flags & 0b100_0000 != 0 {
            Some(read_string(buf, pos)?)
        } else {
            None
        };
        columns.push(Column {
            name: cname,
            ty: Type::Scalar(ty),
            decimal,
            primary_key: false, // set from the pk list below
            not_null: flags & 0b10 != 0,
            default,
            default_expr,
            identity,
            collation,
        });
    }
    // The primary key (v5): member ordinals in KEY order. Each must name a real column,
    // once; membership sets the per-column convenience flag.
    let pk_count = read_u16(buf, pos)? as usize;
    let mut pk = Vec::with_capacity(pk_count);
    for _ in 0..pk_count {
        let ord = read_u16(buf, pos)? as usize;
        if ord >= columns.len() || pk.contains(&ord) {
            return Err(corrupt("invalid primary key ordinal"));
        }
        columns[ord].primary_key = true;
        pk.push(ord);
    }
    // CHECK constraints (v4): the stored expression text re-parses with the ordinary
    // expression parser — it was written by the token renderer, so this cannot fail for a
    // file the engine wrote; failure means the file lied (XX001, constraints.md §4.5).
    let check_count = read_u16(buf, pos)? as usize;
    let mut checks = Vec::with_capacity(check_count);
    for _ in 0..check_count {
        let check_name = read_string(buf, pos)?;
        let expr_text = read_string(buf, pos)?;
        let expr = crate::parser::parse_expression(&expr_text)
            .map_err(|e| corrupt(&format!("stored check constraint does not parse: {e}")))?;
        checks.push(CheckConstraint {
            name: check_name,
            expr_text,
            expr,
        });
    }
    // Secondary indexes (v5): name + key-column ordinals + the v6 flags byte (bit0
    // unique; the rest reserved-zero) + root page, in the catalog's (lowercased-name
    // ascending) order — a reader trusts the order. Duplicate ordinals within one index
    // are legal (indexes.md §1).
    let index_count = read_u16(buf, pos)? as usize;
    let mut indexes = Vec::with_capacity(index_count);
    let mut index_roots = Vec::with_capacity(index_count);
    for _ in 0..index_count {
        let iname = read_string(buf, pos)?;
        let kc = read_u16(buf, pos)? as usize;
        if kc == 0 {
            return Err(corrupt("index with no key columns"));
        }
        let mut cols = Vec::with_capacity(kc);
        for _ in 0..kc {
            let ord = read_u16(buf, pos)? as usize;
            if ord >= columns.len() {
                return Err(corrupt("invalid index column ordinal"));
            }
            cols.push(ord);
        }
        let iflags = read_u8(buf, pos)?;
        if iflags & !0b01 != 0 {
            return Err(corrupt("reserved index flag set"));
        }
        // v13: index_kind byte (0 = ordered B-tree, 1 = GIN — spec/design/gin.md §7);
        // v20: 2 = GiST (spec/design/gist.md §8).
        let kind = match read_u8(buf, pos)? {
            0 => IndexKind::Btree,
            1 => IndexKind::Gin,
            2 => IndexKind::Gist,
            _ => return Err(corrupt("unsupported index kind")),
        };
        index_roots.push(read_u32(buf, pos)?);
        indexes.push(IndexDef {
            name: iname,
            columns: cols,
            unique: iflags & 0b01 != 0,
            kind,
        });
    }
    // Foreign keys (v11): name + local ordinals + referenced table + referenced ordinals + the
    // actions byte, in the catalog's (lowercased-name ascending) order — a reader trusts the
    // order. The local ordinals index THIS table; the referenced ordinals index the PARENT (whose
    // entry may be decoded later, so they are not cross-checked here — the writer keeps them
    // valid; a structurally impossible FK is rejected below).
    let fk_count = read_u16(buf, pos)? as usize;
    let mut foreign_keys = Vec::with_capacity(fk_count);
    for _ in 0..fk_count {
        let fname = read_string(buf, pos)?;
        let lc = read_u16(buf, pos)? as usize;
        if lc == 0 {
            return Err(corrupt("foreign key with no columns"));
        }
        let mut cols = Vec::with_capacity(lc);
        for _ in 0..lc {
            let ord = read_u16(buf, pos)? as usize;
            if ord >= columns.len() {
                return Err(corrupt("invalid foreign-key column ordinal"));
            }
            cols.push(ord);
        }
        let ref_table = read_string(buf, pos)?;
        let rc = read_u16(buf, pos)? as usize;
        if rc != lc {
            return Err(corrupt(
                "foreign-key referencing/referenced column count mismatch",
            ));
        }
        let mut ref_cols = Vec::with_capacity(rc);
        for _ in 0..rc {
            ref_cols.push(read_u16(buf, pos)? as usize);
        }
        let actions = read_u8(buf, pos)?;
        if actions & !0b1111 != 0 {
            return Err(corrupt("reserved foreign-key action bit set"));
        }
        foreign_keys.push(ForeignKeyConstraint {
            name: fname,
            columns: cols,
            ref_table,
            ref_columns: ref_cols,
            on_delete: fk_action_from_code(actions & 0b11)?,
            on_update: fk_action_from_code((actions >> 2) & 0b11)?,
        });
    }
    let root_data_page = read_u32(buf, pos)?;
    Ok((
        Table {
            name,
            columns,
            pk,
            checks,
            indexes,
            foreign_keys,
        },
        root_data_page,
        index_roots,
    ))
}

/// Decode one record `(key, row)` and the **overflow chain pages** any external value followed
/// (for the free-list reachability walk — spec/design/large-values.md §12). `fetch` reads a page
/// block by index, used to follow overflow chains; `None` is only valid where no value can be
/// external (e.g. a catalog default).
fn decode_record(
    col_types: &[ColType],
    buf: &[u8],
    pos: &mut usize,
    fetch: Option<&dyn Fn(u32) -> Result<Vec<u8>>>,
) -> Result<(Vec<u8>, Row, Vec<u32>)> {
    let key_len = read_u16(buf, pos)? as usize;
    let key = take(buf, pos, key_len)?.to_vec();
    let mut row = Vec::with_capacity(col_types.len());
    let mut ovf = Vec::new();
    for ty in col_types {
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
    ty: &ColType,
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

/// The present-value body (after a `0x00` tag) for any [`ColType`]: a scalar via
/// [`read_inline_scalar`], or a composite via [`read_composite_body`] (spec/design/composite.md §4).
fn read_inline_body(ty: &ColType, buf: &[u8], pos: &mut usize) -> Result<Value> {
    match ty {
        ColType::Scalar(s) => read_inline_scalar(*s, buf, pos),
        ColType::Composite { .. } => read_composite_body(ty, buf, pos),
        ColType::Array(elem) => read_array_body(elem, buf, pos),
        ColType::Range(elem) => read_range_body(elem, buf, pos),
    }
}

/// A range value's present **body** (after the `0x00` tag): inverse of [`encode_range_body`]
/// (spec/design/ranges.md §4). Reads the `flags` byte; an `EMPTY` range stops there. Otherwise the
/// finite lower bound (`!LB_INF`) then the finite upper bound (`!UB_INF`) are each read as the
/// element's value-codec body (no presence tag). A reserved flag bit set is `XX001`. Note: an
/// infinite bound's inclusivity bit is canonically 0, but the body that produced the bytes already
/// enforced that — read whatever bits are present and rebuild the `RangeVal` faithfully.
pub(crate) fn read_range_body(elem: &ColType, buf: &[u8], pos: &mut usize) -> Result<Value> {
    let flags = read_u8(buf, pos)?;
    if flags & !0x1f != 0 {
        return Err(corrupt("range flags has a reserved bit set"));
    }
    if flags & 0x01 != 0 {
        return Ok(Value::Range(RangeVal::empty()));
    }
    let lb_inf = flags & 0x02 != 0;
    let ub_inf = flags & 0x04 != 0;
    let lower = if lb_inf {
        None
    } else {
        Some(Box::new(read_inline_body(elem, buf, pos)?))
    };
    let upper = if ub_inf {
        None
    } else {
        Some(Box::new(read_inline_body(elem, buf, pos)?))
    };
    Ok(Value::Range(RangeVal {
        empty: false,
        lower,
        upper,
        lower_inc: flags & 0x08 != 0,
        upper_inc: flags & 0x10 != 0,
    }))
}

/// An array value's present **body** (after the `0x00` tag): inverse of [`encode_array_body`]
/// (spec/design/array.md §4). Reads `ndim`/`flags`/per-dim `(len, lb)`, then the optional null
/// bitmap and the present element bodies (row-major). Accepts `ndim` 0 (empty) through 6 (`MAXDIM`);
/// a higher `ndim` or an element-count overflow is `XX001`.
fn read_array_body(elem: &ColType, buf: &[u8], pos: &mut usize) -> Result<Value> {
    let ndim = read_u8(buf, pos)? as usize;
    let flags = read_u8(buf, pos)?;
    if flags & !0x01 != 0 {
        return Err(corrupt("array flags has a reserved bit set"));
    }
    if ndim == 0 {
        return Ok(Value::Array(ArrayVal::empty())); // empty array
    }
    if ndim > 6 {
        return Err(corrupt("array ndim exceeds the maximum of 6"));
    }
    let mut dims = Vec::with_capacity(ndim);
    let mut lbounds = Vec::with_capacity(ndim);
    let mut n: usize = 1;
    for _ in 0..ndim {
        let len = read_u32(buf, pos)? as usize;
        let lb = read_u32(buf, pos)? as i32; // lower bound (i32 two's-complement)
        n = n
            .checked_mul(len)
            .ok_or_else(|| corrupt("array element count overflow"))?;
        dims.push(len);
        lbounds.push(lb);
    }
    let has_nulls = flags & 0x01 != 0;
    let bitmap = if has_nulls {
        take(buf, pos, n.div_ceil(8))?.to_vec()
    } else {
        Vec::new()
    };
    let mut elements = Vec::with_capacity(n);
    for i in 0..n {
        let is_null = has_nulls && (bitmap[i / 8] & (0x80 >> (i % 8)) != 0);
        if is_null {
            elements.push(Value::Null);
        } else {
            elements.push(read_inline_body(elem, buf, pos)?);
        }
    }
    Ok(Value::Array(ArrayVal {
        dims,
        lbounds,
        elements,
    }))
}

/// A composite value's present **body** (after the `0x00` tag): the null bitmap then each present
/// field's body in declaration order (inverse of [`encode_composite_body`], spec/design/composite.md
/// §4). A field whose bitmap bit is set is `Value::Null` and consumes no body bytes; otherwise its
/// body is read recursively (no per-field presence tag).
fn read_composite_body(ty: &ColType, buf: &[u8], pos: &mut usize) -> Result<Value> {
    let ColType::Composite { fields, .. } = ty else {
        return Err(corrupt("read_composite_body on a non-composite type"));
    };
    let nbytes = fields.len().div_ceil(8);
    let bitmap = take(buf, pos, nbytes)?.to_vec();
    let mut vals = Vec::with_capacity(fields.len());
    for (i, f) in fields.iter().enumerate() {
        let is_null = bitmap[i / 8] & (0x80 >> (i % 8)) != 0;
        if is_null {
            vals.push(Value::Null);
        } else {
            vals.push(read_inline_body(&f.ty, buf, pos)?);
        }
    }
    Ok(Value::Composite(vals))
}

/// The present-value body of a **scalar** (after a `0x00` tag): a fixed-width integer, a `u16`
/// length + UTF-8 bytes for `text`, a single `bool-byte`, the decimal body, etc.
/// (spec/fileformat/format.md *Value codec*).
fn read_inline_scalar(ty: ScalarType, buf: &[u8], pos: &mut usize) -> Result<Value> {
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
    } else if ty.is_json() {
        // json: verbatim text, length-prefixed exactly like text (spec/design/json.md §4).
        let len = read_u16(buf, pos)? as usize;
        let bytes = take(buf, pos, len)?.to_vec();
        let s = String::from_utf8(bytes).map_err(|_| corrupt("non-UTF-8 json value"))?;
        Ok(Value::Json(s))
    } else if ty.is_jsonb() {
        // jsonb: the self-delimiting tagged-node tree (spec/design/json.md §2).
        Ok(Value::Jsonb(decode_jsonb_body(buf, pos)?))
    } else if ty.is_uuid() {
        // Fixed 16 raw bytes, no length prefix (must branch before the integer path —
        // decode_int would sign-flip and width_bytes is 16 there too).
        let b: [u8; 16] = take(buf, pos, 16)?
            .try_into()
            .map_err(|_| corrupt("invalid uuid length"))?;
        Ok(Value::Uuid(b))
    } else if ty.is_float64() {
        // 8 IEEE bytes, big-endian; the stored bits are preserved verbatim (spec/design/float.md
        // §10). Must branch before the integer path (width_bytes is 8 there too).
        let b: [u8; 8] = take(buf, pos, 8)?
            .try_into()
            .map_err(|_| corrupt("invalid f64 length"))?;
        Ok(Value::Float64(f64::from_bits(u64::from_be_bytes(b))))
    } else if ty.is_float32() {
        // 4 IEEE bytes, big-endian. Must branch before the integer path (width_bytes is 4, which
        // would otherwise match i32 and sign-flip).
        let b: [u8; 4] = take(buf, pos, 4)?
            .try_into()
            .map_err(|_| corrupt("invalid f32 length"))?;
        Ok(Value::Float32(f32::from_bits(u32::from_be_bytes(b))))
    } else if ty.is_timestamp() {
        let vb = take(buf, pos, ty.width_bytes())?;
        Ok(Value::Timestamp(decode_int(ty, vb)))
    } else if ty.is_timestamptz() {
        let vb = take(buf, pos, ty.width_bytes())?;
        Ok(Value::Timestamptz(decode_int(ty, vb)))
    } else if ty.is_date() {
        // 4-byte i32 day count, same order-preserving codec as i32 (spec/design/date.md).
        let vb = take(buf, pos, ty.width_bytes())?;
        Ok(Value::Date(decode_int(ty, vb) as i32))
    } else if ty.is_interval() {
        // Fixed 16-byte body: i32 months + i32 days + i64 micros, big-endian (no sign-flip).
        let months = i32::from_be_bytes(
            take(buf, pos, 4)?
                .try_into()
                .map_err(|_| corrupt("invalid interval months"))?,
        );
        let days = i32::from_be_bytes(
            take(buf, pos, 4)?
                .try_into()
                .map_err(|_| corrupt("invalid interval days"))?,
        );
        let micros = i64::from_be_bytes(
            take(buf, pos, 8)?
                .try_into()
                .map_err(|_| corrupt("invalid interval micros"))?,
        );
        Ok(Value::Interval(Interval {
            months,
            days,
            micros,
        }))
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
fn read_value_lazy(ty: &ColType, buf: &[u8], pos: &mut usize) -> Result<Value> {
    match read_u8(buf, pos)? {
        // A composite's inline body has no nested overflow pointers (its fields are inline —
        // composite.md §4), so it is read eagerly even in the lazy path.
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
    col_types: &[ColType],
    buf: &[u8],
    pos: &mut usize,
) -> Result<(Vec<u8>, Row, usize)> {
    let start = *pos;
    let key_len = read_u16(buf, pos)? as usize;
    let key = take(buf, pos, key_len)?.to_vec();
    let mut row = Vec::with_capacity(col_types.len());
    for ty in col_types {
        row.push(read_value_lazy(ty, buf, pos)?);
    }
    Ok((key, row, *pos - start))
}

/// Materialize an unfetched reference into its plain [`Value`] (spec/design/large-values.md
/// §14): gather the overflow chain through `fetch` for an external form, decompress a
/// compressed one, and reconstruct by column type. Decompression errors are `data_corrupted`,
/// surfaced only when the value is actually touched.
pub(crate) fn resolve_unfetched(
    ty: &ColType,
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

/// Write a `u16`-length-prefixed UTF-8 string (the catalog's name/string encoding — the inverse of
/// `read_string`).
fn push_string(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// Read an 8-byte big-endian two's-complement i64 (the sequence-entry field encoding).
fn read_i64(buf: &[u8], pos: &mut usize) -> Result<i64> {
    let s = take(buf, pos, 8)?;
    Ok(i64::from_be_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Outcome, execute};

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn collation_open_refuses_absent_reference() {
        // A file that references a collation NO loaded bundle provides is the graded verdict's
        // legible refusal (spec/design/collation.md §12, slice 2d): decoding the reference entry
        // fails with XX002 (collation_version_mismatch) naming it + its version, rather than the old
        // bare 42704. "zz-absent-collation" is never in any bundle, so this is independent of the
        // engine-global loaded set (no bundle load needed). A *version-skewed* collation, by
        // contrast, decodes fine and is enforced read-only at write time (executor skew_tests).
        let coll = Collation {
            name: "zz-absent-collation".to_string(),
            unicode_version: "17.0.0".to_string(),
            cldr_version: "48".to_string(),
            description: String::new(),
            singles: Vec::new(),
            contractions: Vec::new(),
        };
        let bytes = collation_entry_bytes(&coll, false);
        let mut pos = 0;
        let err =
            decode_collation_entry(&bytes, &mut pos).expect_err("absent reference must refuse");
        assert_eq!(err.code(), "XX002");
        assert!(err.message.contains("zz-absent-collation"));
        assert!(err.message.contains("17.0.0/48"));
    }

    #[test]
    fn type_codes_round_trip() {
        for ty in ScalarType::all() {
            assert_eq!(scalar_for_type_code(type_code_for_scalar(ty)), Some(ty));
        }
        assert_eq!(scalar_for_type_code(0), None);
        // 12 = f64, 13 = f32 (spec/fileformat/format.md); 14 is the next unassigned code.
        assert_eq!(scalar_for_type_code(12), Some(ScalarType::Float64));
        assert_eq!(scalar_for_type_code(13), Some(ScalarType::Float32));
        assert_eq!(scalar_for_type_code(14), None);
    }

    /// The float value codec preserves the IEEE BITS verbatim for every value EXCEPT NaN — the sign
    /// bit of `-0.0`, ±Inf, and finite values all round-trip bit-for-bit — while a NaN is
    /// canonicalized to the single quiet pattern (`0x7FF8…000` / `0x7FC00000`) so a stored NaN is
    /// cross-core byte-identical (spec/design/float.md §10). Big-endian, fixed-width. The `-0 → +0`
    /// collapse is a comparison/key concern and is NOT applied by the codec.
    #[test]
    fn float_value_codec_round_trips_bits() {
        let mut sink = Vec::new();
        // f64 cases, compared by RAW BITS so -0 vs +0 and NaN payloads are distinguished.
        let f64s = [
            0.0f64,
            -0.0f64,
            1.5,
            -2.5,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            f64::from_bits(0x7ff8_0000_0000_0001), // a NaN with a payload
            f64::MIN_POSITIVE,
            std::f64::consts::PI,
        ];
        for &x in &f64s {
            let enc = encode_value(&ColType::Scalar(ScalarType::Float64), &Value::Float64(x));
            // present tag + 8 IEEE bytes, big-endian, no length prefix. A NaN canonicalizes to the
            // single quiet pattern; every other value is verbatim.
            let want_bits = if x.is_nan() {
                0x7ff8_0000_0000_0000_u64
            } else {
                x.to_bits()
            };
            assert_eq!(
                enc.len(),
                1 + 8,
                "f64 body is 8 bytes behind the presence tag"
            );
            assert_eq!(enc[0], 0x00, "present tag");
            assert_eq!(
                &enc[1..],
                &want_bits.to_be_bytes(),
                "big-endian IEEE bytes (NaN canonicalized)"
            );
            let mut pos = 0usize;
            let got = read_value(
                &ColType::Scalar(ScalarType::Float64),
                &enc,
                &mut pos,
                None,
                &mut sink,
            )
            .unwrap();
            match got {
                Value::Float64(y) => assert_eq!(y.to_bits(), want_bits, "bits round-trip"),
                other => panic!("expected Float64, got {other:?}"),
            }
        }
        // f32 cases (4-byte body).
        let f32s = [
            0.0f32,
            -0.0f32,
            1.5,
            -2.5,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            f32::from_bits(0x7fc0_0001),
        ];
        for &x in &f32s {
            let enc = encode_value(&ColType::Scalar(ScalarType::Float32), &Value::Float32(x));
            let want_bits = if x.is_nan() {
                0x7fc0_0000_u32
            } else {
                x.to_bits()
            };
            assert_eq!(
                enc.len(),
                1 + 4,
                "f32 body is 4 bytes behind the presence tag"
            );
            assert_eq!(&enc[1..], &want_bits.to_be_bytes());
            let mut pos = 0usize;
            let got = read_value(
                &ColType::Scalar(ScalarType::Float32),
                &enc,
                &mut pos,
                None,
                &mut sink,
            )
            .unwrap();
            match got {
                Value::Float32(y) => assert_eq!(y.to_bits(), want_bits),
                other => panic!("expected Float32, got {other:?}"),
            }
        }
        // NULL round-trips for both float widths (presence tag only).
        for ty in [ScalarType::Float32, ScalarType::Float64] {
            let ct = ColType::Scalar(ty);
            let enc = encode_value(&ct, &Value::Null);
            let mut pos = 0usize;
            assert!(matches!(
                read_value(&ct, &enc, &mut pos, None, &mut sink).unwrap(),
                Value::Null
            ));
        }
    }

    /// A float column written and re-read through the whole on-disk image (the cross-core
    /// round-trip path) preserves both finite and special values.
    #[test]
    fn float_table_in_memory_round_trip() {
        let mut db = Database::new();
        for s in [
            "CREATE TABLE t (id i32 PRIMARY KEY, f f64, g f32)",
            "INSERT INTO t VALUES (1, 1.5, 2.5)",
            "INSERT INTO t VALUES (2, 0.0, 0.0)",
            "INSERT INTO t VALUES (3, 0.0, 0.0)",
            // INSERT VALUES takes only plain literals; the specials enter via an UPDATE whose RHS
            // is a typed-literal expression (the resolver path that admits `float '…'`).
            "UPDATE t SET f = float 'Infinity', g = real '-Infinity' WHERE id = 2",
            "UPDATE t SET f = float 'NaN' WHERE id = 3",
        ] {
            execute(&mut db, s).expect("setup");
        }
        let image = db.to_image(8192, 1).unwrap();
        let mut db2 = Database::from_image(&image).expect("re-open");
        let out = execute(&mut db2, "SELECT id, f, g FROM t ORDER BY id").expect("query");
        let rows = match out {
            Outcome::Query { rows, .. } => rows,
            _ => panic!("expected query"),
        };
        assert_eq!(rows.len(), 3);
        // id=2 carries ±Infinity, id=3 carries NaN — they survive the image round trip.
        assert_eq!(rows[1][1].render(), "Infinity");
        assert_eq!(rows[1][2].render(), "-Infinity");
        assert_eq!(rows[2][1].render(), "NaN");
    }

    fn sample_db() -> Database {
        let mut db = Database::new();
        for s in [
            "CREATE TABLE t (id i32 PRIMARY KEY, v i16)",
            "INSERT INTO t VALUES (1, 10)",
            "INSERT INTO t VALUES (2, NULL)",
            "INSERT INTO t VALUES (3, 30)",
            "CREATE TABLE r (a i16, b i64)",
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
            "CREATE TABLE t (id i32 PRIMARY KEY, body text)".to_string(),
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
