//! On-disk single-file format: serialize / load (spec/fileformat/format.md).
//!
//! Whole-image model (step-5b): a commit serializes the entire database to one byte
//! image; loading reconstructs it. The byte layout is the canonical contract
//! (spec/fileformat/format.md) and is verified byte-for-byte against shared goldens
//! so a file written by this core is byte-identical to one written by the Go core
//! (CLAUDE.md §8). All multi-byte integers are big-endian.

use crate::catalog::{Column, Table};
use crate::decimal::Decimal;
use crate::encoding::{decode_int, encode_nullable};
use crate::error::{EngineError, Result, SqlState};
use crate::executor::Database;
use crate::storage::Row;
use crate::types::{DecimalTypmod, ScalarType};
use crate::value::Value;

/// File magic — ASCII "JEDB" (the engine is named `jed`).
const MAGIC: [u8; 4] = *b"JEDB";
/// On-disk format version.
const FORMAT_VERSION: u16 = 1;
/// Bytes of the page header on catalog / data pages.
const PAGE_HEADER: usize = 12;
/// `page_type` for a catalog page.
const PAGE_CATALOG: u8 = 1;
/// `page_type` for a data page.
const PAGE_DATA: u8 = 2;
/// Catalog root page index (pages 0,1 are the meta slots).
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
    }
}

fn corrupt(msg: &str) -> EngineError {
    EngineError::new(SqlState::DataCorrupted, msg)
}

impl Database {
    /// Serialize the whole database to a single on-disk image
    /// (spec/fileformat/format.md). `page_size` is recorded in the meta page;
    /// `txid` is the commit counter written into both meta slots.
    pub fn to_image(&self, page_size: u32, txid: u64) -> Result<Vec<u8>> {
        let ps = page_size as usize;
        if ps < PAGE_HEADER + 36 {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "page size too small for the format",
            ));
        }
        let cap = ps - PAGE_HEADER;

        // Tables in ascending lowercased-name order (no hash-map order leak).
        let mut tables = self.catalog_and_stores();
        tables.sort_by(|a, b| a.0.cmp(b.0));

        // Per-table record bytes, in key order.
        let mut records: Vec<Vec<Vec<u8>>> = Vec::with_capacity(tables.len());
        for (_, table, store) in &tables {
            let recs = store
                .iter_entries()
                .map(|(key, row)| encode_record(table, key, row))
                .collect();
            records.push(recs);
        }

        // Catalog page grouping depends only on entry sizes, which are independent of
        // root_data_page values — so group once, fill values later.
        let entry_sizes: Vec<usize> = tables
            .iter()
            .map(|(_, t, _)| table_entry_bytes(t, 0).len())
            .collect();
        let cat_groups = pack(&entry_sizes, cap)?;
        let num_cat_pages = cat_groups.len() as u32;

        // Assign data page chains after the catalog; record each table's root page.
        let mut next_index = ROOT_PAGE + num_cat_pages;
        let mut root_data_page = vec![0u32; tables.len()];
        let mut data_groups: Vec<Vec<Vec<usize>>> = Vec::with_capacity(tables.len());
        for (ti, recs) in records.iter().enumerate() {
            if recs.is_empty() {
                data_groups.push(Vec::new());
                continue;
            }
            let sizes: Vec<usize> = recs.iter().map(Vec::len).collect();
            let groups = pack(&sizes, cap)?;
            root_data_page[ti] = next_index;
            next_index += groups.len() as u32;
            data_groups.push(groups);
        }
        let page_count = next_index;

        let mut image = vec![0u8; page_count as usize * ps];

        // Meta: both slots hold the current meta (a fresh whole-image commit has no
        // distinct prior version — spec/fileformat/format.md).
        write_meta(&mut image, ps, 0, page_size, txid, ROOT_PAGE, page_count);
        write_meta(&mut image, ps, 1, page_size, txid, ROOT_PAGE, page_count);

        // Catalog pages.
        for (gi, group) in cat_groups.iter().enumerate() {
            let index = ROOT_PAGE + gi as u32;
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

        // Data pages, one chain per non-empty table.
        for (ti, groups) in data_groups.iter().enumerate() {
            for (gi, group) in groups.iter().enumerate() {
                let index = root_data_page[ti] + gi as u32;
                let next = if gi + 1 < groups.len() { index + 1 } else { 0 };
                let mut payload = Vec::new();
                for &ri in group {
                    payload.extend_from_slice(&records[ti][ri]);
                }
                write_page(
                    &mut image,
                    ps,
                    index,
                    PAGE_DATA,
                    group.len() as u32,
                    next,
                    &payload,
                );
            }
        }

        Ok(image)
    }

    /// Reconstruct a database from an on-disk image (inverse of `to_image`). Returns
    /// a structured `data_corrupted` (XX001) error for any malformed input.
    pub fn from_image(image: &[u8]) -> Result<Database> {
        if image.len() < 12 {
            return Err(corrupt("image smaller than a meta header"));
        }
        let page_size = read_u32_at(image, 8)? as usize;
        if page_size < PAGE_HEADER + 36 || image.len() < page_size * 2 {
            return Err(corrupt("invalid page size"));
        }
        let meta = select_meta(image, page_size)?;

        let mut db = Database::new();
        let mut cat_page = meta.root_page;
        while cat_page != 0 {
            let page = read_page(image, page_size, cat_page)?;
            if page.page_type != PAGE_CATALOG {
                return Err(corrupt("expected a catalog page"));
            }
            let mut pos = 0usize;
            for _ in 0..page.item_count {
                let (table, root_data_page) = decode_table_entry(page.payload, &mut pos)?;
                let name = table.name.clone();
                let col_types: Vec<ScalarType> = table.columns.iter().map(|c| c.ty).collect();
                let has_pk = table.primary_key_index().is_some();
                db.put_table(table);
                read_data_chain(
                    image,
                    page_size,
                    root_data_page,
                    &col_types,
                    has_pk,
                    &name,
                    &mut db,
                )?;
            }
            cat_page = page.next_page;
        }
        Ok(db)
    }
}

/// Read every record in a table's data-page chain into the store under `name`. For a
/// table with no primary key, the keys are synthetic int64 rowids; advance the
/// store's rowid counter past the largest so future inserts don't collide with a
/// loaded key (spec/fileformat/format.md). No format change — keys are stored
/// verbatim.
fn read_data_chain(
    image: &[u8],
    ps: usize,
    root_data_page: u32,
    col_types: &[ScalarType],
    has_pk: bool,
    name: &str,
    db: &mut Database,
) -> Result<()> {
    let mut dp = root_data_page;
    while dp != 0 {
        let page = read_page(image, ps, dp)?;
        if page.page_type != PAGE_DATA {
            return Err(corrupt("expected a data page"));
        }
        let mut pos = 0usize;
        for _ in 0..page.item_count {
            let (key, row) = decode_record(col_types, page.payload, &mut pos)?;
            if !has_pk && key.len() == ScalarType::Int64.width_bytes() {
                db.store_mut(name)
                    .bump_rowid_to(decode_int(ScalarType::Int64, &key) + 1);
            }
            if !db.store_mut(name).insert(key, row) {
                return Err(corrupt("duplicate key in data page"));
            }
        }
        dp = page.next_page;
    }
    Ok(())
}

/// One record's bytes: `key_len(u16) | key | payload(each column value)`.
fn encode_record(table: &Table, key: &[u8], row: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(key.len() as u16).to_be_bytes());
    out.extend_from_slice(key);
    for (col, val) in table.columns.iter().zip(row.iter()) {
        out.extend_from_slice(&encode_value(col.ty, val));
    }
    out
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

/// Write a meta slot's bytes (and its CRC) into `image`.
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
    image[off..off + 4].copy_from_slice(&MAGIC);
    image[off + 4..off + 6].copy_from_slice(&FORMAT_VERSION.to_be_bytes());
    image[off + 8..off + 12].copy_from_slice(&page_size.to_be_bytes());
    image[off + 12..off + 20].copy_from_slice(&txid.to_be_bytes());
    image[off + 20..off + 24].copy_from_slice(&root_page.to_be_bytes());
    image[off + 24..off + 28].copy_from_slice(&page_count.to_be_bytes());
    let crc = crc32_ieee(&image[off..off + 32]);
    image[off + 32..off + 36].copy_from_slice(&crc.to_be_bytes());
}

/// Write a catalog / data page's header and payload into `image`.
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
    image[off] = page_type;
    image[off + 4..off + 8].copy_from_slice(&item_count.to_be_bytes());
    image[off + 8..off + 12].copy_from_slice(&next_page.to_be_bytes());
    image[off + PAGE_HEADER..off + PAGE_HEADER + payload.len()].copy_from_slice(payload);
}

/// A validated meta slot's salient fields.
struct Meta {
    txid: u64,
    root_page: u32,
}

/// Validate one meta slot; None if it is not a valid meta.
fn read_meta(image: &[u8], ps: usize, slot: usize) -> Option<Meta> {
    let off = slot * ps;
    if off + ps > image.len() {
        return None;
    }
    let m = &image[off..off + ps];
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
    })
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

fn read_page(image: &[u8], ps: usize, index: u32) -> Result<Page<'_>> {
    let off = index as usize * ps;
    if off + ps > image.len() {
        return Err(corrupt("page index out of range"));
    }
    let p = &image[off..off + ps];
    Ok(Page {
        page_type: p[0],
        item_count: u32::from_be_bytes([p[4], p[5], p[6], p[7]]),
        next_page: u32::from_be_bytes([p[8], p[9], p[10], p[11]]),
        payload: &p[PAGE_HEADER..],
    })
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
        let default = if flags & 0b100 != 0 {
            Some(read_value(ty, buf, pos)?)
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

fn decode_record(col_types: &[ScalarType], buf: &[u8], pos: &mut usize) -> Result<(Vec<u8>, Row)> {
    let key_len = read_u16(buf, pos)? as usize;
    let key = take(buf, pos, key_len)?.to_vec();
    let mut row = Vec::with_capacity(col_types.len());
    for &ty in col_types {
        row.push(read_value(ty, buf, pos)?);
    }
    Ok((key, row))
}

/// Read one value via the value codec (inverse of `encode_value`). The presence tag is
/// read first; for a present value the body is the column type's: a fixed-width integer,
/// a `u16` length + that many UTF-8 bytes for `text`, or a single `bool-byte` for `boolean`.
fn read_value(ty: ScalarType, buf: &[u8], pos: &mut usize) -> Result<Value> {
    match read_u8(buf, pos)? {
        0x00 => {
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
                // flags (sign), u16 scale, u16 ndigits, then that many base-10^4 groups.
                let flags = read_u8(buf, pos)?;
                let neg = flags & 1 != 0;
                let scale = read_u16(buf, pos)? as u32;
                let ndigits = read_u16(buf, pos)? as usize;
                let mut groups = Vec::with_capacity(ndigits);
                for _ in 0..ndigits {
                    groups.push(read_u16(buf, pos)?);
                }
                Ok(Value::Decimal(Decimal::from_codec(neg, scale, &groups)))
            } else if ty.is_bytea() {
                let len = read_u16(buf, pos)? as usize;
                let bytes = take(buf, pos, len)?.to_vec();
                Ok(Value::Bytea(bytes))
            } else {
                let w = ty.width_bytes();
                let vb = take(buf, pos, w)?;
                Ok(Value::Int(decode_int(ty, vb)))
            }
        }
        0x01 => Ok(Value::Null),
        _ => Err(corrupt("invalid value presence tag")),
    }
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
        assert_eq!(scalar_for_type_code(9), None);
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
}
