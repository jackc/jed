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
use crate::pmap::{Child, Node};
use crate::storage::Row;
use crate::types::{DecimalTypmod, ScalarType};
use crate::value::Value;

/// File magic — ASCII "JEDB" (the engine is named `jed`).
const MAGIC: [u8; 4] = *b"JEDB";
/// On-disk format version — 2 = page-backed copy-on-write B-tree (Phase 6, P6.1).
const FORMAT_VERSION: u16 = 2;
/// Bytes of the page header on catalog / B-tree pages.
const PAGE_HEADER: usize = 12;
/// `page_type` for a catalog page.
const PAGE_CATALOG: u8 = 1;
/// `page_type` for a B-tree leaf node.
const PAGE_LEAF: u8 = 2;
/// `page_type` for a B-tree interior node.
const PAGE_INTERIOR: u8 = 3;
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
    }
}

/// The on-disk size of a record (`key_len(u16) | key | each column value`) — the **weight** the
/// page-backed B-tree splits on (spec/fileformat/format.md). It must equal the length
/// `encode_record` produces, so the in-memory node boundaries match the serialized page boundaries;
/// computed directly from the value codec to keep the two in lockstep.
pub(crate) fn record_size(col_types: &[ScalarType], key: &[u8], row: &Row) -> usize {
    let mut n = 2 + key.len();
    for (ty, v) in col_types.iter().zip(row.iter()) {
        n += encode_value(*ty, v).len();
    }
    n
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

        // Serialize each table's B-tree post-order, body pages allocated from page 2. Each entry
        // is `(index, page_type, item_count, payload)`; children precede their parent so parent
        // child-pointers reference already-allocated pages (format.md).
        let mut body: Vec<(u32, u8, u32, Vec<u8>)> = Vec::new();
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

        // B-tree node pages.
        for (index, page_type, item_count, payload) in &body {
            write_page(&mut image, ps, *index, *page_type, *item_count, 0, payload);
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
    body: &mut Vec<(u32, u8, u32, Vec<u8>)>,
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
    for i in 0..node.keys.len() {
        payload.extend_from_slice(&encode_record(table, &node.keys[i], &node.vals[i]));
    }
    if payload.len() > cap {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "a record larger than the per-row limit is not supported",
        ));
    }
    body.push((index, page_type, n, payload));
    Ok(index)
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
                root_data_page[ti] = serialize_dirty(root, table, cap, ps, &mut alloc, &mut pages)?;
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
            Child::Resident(n) => serialize_dirty(n, table, cap, ps, alloc, pages)?,
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
    for i in 0..node.keys.len() {
        payload.extend_from_slice(&encode_record(table, &node.keys[i], &node.vals[i]));
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
        if page_size < PAGE_HEADER + 36 || image.len() < page_size * 2 {
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
                let has_pk = table.primary_key_index().is_some();
                snap.put_table(table, page_size as u32);
                if root_data_page != 0 {
                    let (root, len) = read_tree(image, page_size, root_data_page, &col_types)?;
                    collect_node_pages(&root, &mut reached);
                    let store = snap.store_mut(&name);
                    store.set_tree(Some(root), len);
                    // No-PK keys are synthetic int64 rowids — advance the counter past the largest
                    // (the last entry in key order) so future inserts don't collide.
                    if !has_pk {
                        let max = store
                            .iter_entries()
                            .last()
                            .map(|(k, _)| decode_int(ScalarType::Int64, &k));
                        if let Some(m) = max {
                            store.bump_rowid_to(m + 1);
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
}

/// Collect the on-disk page index of `node` and every descendant (a loaded tree, all pages set) into
/// `reached` — the live set the free-list reconstruction subtracts from `[2, page_count)` (P6.2).
fn collect_node_pages(node: &Arc<Node>, reached: &mut HashSet<u32>) {
    reached.insert(node.page.load(Ordering::Acquire));
    for child in &node.children {
        match child {
            // An `OnDisk` leaf contributes its page without being loaded — the free-list walk reuses
            // the resident interior skeleton (pager.md §4); a `Resident` child recurses.
            Child::Resident(n) => collect_node_pages(n, reached),
            Child::OnDisk(p) => {
                reached.insert(*p);
            }
        }
    }
}

/// Read a table's on-disk B-tree (rooted at `page_idx`) into an in-memory tree, returning the root
/// node and the total row count (spec/fileformat/format.md). An interior node's payload is its
/// `N+1` child pointers then its `N` records; we recurse the pointers, then read the separators.
/// Weights are recomputed from the value codec (the exact size the writer used), so the loaded tree
/// is ready for further size-driven splits.
fn read_tree(
    image: &[u8],
    ps: usize,
    page_idx: u32,
    col_types: &[ScalarType],
) -> Result<(Arc<Node>, usize)> {
    let page = read_page(image, ps, page_idx)?;
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
                let (key, row) = decode_record(col_types, page.payload, &mut pos)?;
                weights.push(record_size(col_types, &key, &row) as u32);
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
                let (child, clen) = read_tree(image, ps, cp, col_types)?;
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
                let (key, row) = decode_record(col_types, page.payload, &mut pos)?;
                weights.push(record_size(col_types, &key, &row) as u32);
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
        page_count: u32::from_be_bytes(m[24..28].try_into().unwrap()),
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
}
