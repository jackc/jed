//! The integrity check (spec/design/integrity-check.md): a host operation that walks one database's
//! pinned generation and reports what is wrong with it as structured findings.
//!
//! The walk follows the contract order of §4 — the generation (meta, manifest, catalog, sequences),
//! then each table in name byte order (its tree, then each index's tree and, under `full`, the index's
//! agreement with the table), then the space accounting — so every core reports the same findings in
//! the same order. `spec/fileformat/integrity.rb` is the reference for that order and for what makes
//! a tree unreadable (§5.2).
//!
//! Memory is bounded independent of row count (§5.4, CLAUDE.md §9): one reachability bitmap of
//! `page_count` bits, the pending siblings of one root-to-leaf path per tree, one decoded leaf and one
//! row's values at a time. Index agreement re-walks the table and probes the index once per derived
//! entry through the index's own lookup, counting the index's entries during its structural walk, so
//! neither side is materialized.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::cancel::CancellationToken;
use crate::catalog::{ColType, IndexDef, IndexKind, Table};
use crate::collation::Collation;
use crate::error::{EngineError, Result};
use crate::executor::{Snapshot, encode_pk_key, index_entry_keys_columns};
use crate::format::{Cap, IntegrityFree, IntegrityGeneration, PageRef, TreeRead};
use crate::integrity_kinds as kinds;
use crate::paging::SharedPaging;
use crate::pmap::Node;
use crate::sqlstate::SqlState;
use crate::types::Type;
use crate::value::Value;

/// How much an integrity check covers (spec/design/integrity-check.md §3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IntegrityLevel {
    /// Everything except agreement between an index and its table (SQLite's `quick_check`).
    Quick,
    /// `Quick`, plus every index holds exactly the entries its table derives.
    #[default]
    Full,
}

/// The options of [`Database::integrity_check`](crate::Database::integrity_check)
/// (spec/design/integrity-check.md §2).
#[derive(Clone)]
pub struct IntegrityCheckOptions {
    /// `Full` by default.
    pub level: IntegrityLevel,
    /// Stop after this many findings and set [`IntegrityReport::truncated`]; `0` means no limit.
    /// `100` by default.
    pub max_findings: u32,
    /// Cancels the check (`57014`) when cancelled; polled at every page and row.
    pub cancel: Option<CancellationToken>,
}

impl Default for IntegrityCheckOptions {
    fn default() -> Self {
        IntegrityCheckOptions {
            level: IntegrityLevel::Full,
            max_findings: 100,
            cancel: None,
        }
    }
}

/// One thing wrong with the database (spec/design/integrity-check.md §2, §4). Everything but
/// `detail` is the cross-core contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrityFinding {
    /// A registered kind (`spec/integrity/findings.toml`; [`crate::integrity_kinds`]).
    pub kind: &'static str,
    /// The database checked (`main` or the attachment's name).
    pub database: String,
    /// The table, index, or sequence the finding is about, when there is one.
    pub object: Option<String>,
    /// The page the finding is about, when there is one.
    pub page: Option<u32>,
    /// The row's storage key, when the finding is about one row.
    pub key: Option<Vec<u8>>,
    /// Human-readable; not part of the contract.
    pub detail: String,
}

/// An object a requested check could not cover, and why (spec/design/integrity-check.md §3.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegritySkip {
    /// The object whose check was skipped.
    pub object: String,
    /// A registered skip reason ([`crate::integrity_kinds`]`::SKIP_*`).
    pub reason: &'static str,
}

/// The result of an integrity check (spec/design/integrity-check.md §2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IntegrityReport {
    /// The findings, in the contract order of §4; empty when the check found nothing.
    pub findings: Vec<IntegrityFinding>,
    /// `max_findings` was reached; more findings may exist.
    pub truncated: bool,
    /// The objects a requested check could not cover.
    pub skipped: Vec<IntegritySkip>,
    /// The distinct pages the walk reached, plus the free-list pages it read.
    pub pages: u64,
    /// The records visited in table leaves.
    pub rows: u64,
}

impl IntegrityReport {
    /// Whether the check found nothing: no findings, and not truncated.
    pub fn ok(&self) -> bool {
        self.findings.is_empty() && !self.truncated
    }
}

/// The deepest a tree page may lie (the root is depth 0): a page at this depth is reached and
/// reported `page_corrupt` without being read, since no valid tree comes near it
/// (integrity-check.md §5.2).
const MAX_TREE_DEPTH: usize = 256;

/// Why the walk stopped early: `max_findings` was reached, or the call fails.
enum Halt {
    Truncated,
    Error(EngineError),
}

impl From<EngineError> for Halt {
    fn from(e: EngineError) -> Halt {
        Halt::Error(e)
    }
}

type Step<T> = std::result::Result<T, Halt>;

/// A read failure's message when it is a statement about the bytes (`XX001`, a finding); any other
/// error (I/O, cancellation, a lost owner) fails the call.
fn corrupt_message(e: EngineError) -> Step<String> {
    if e.state == SqlState::DataCorrupted {
        Ok(e.message.clone())
    } else {
        Err(Halt::Error(e))
    }
}

/// One bit per page of the generation: the pages the walk reached (§5.4).
struct PageSet {
    words: Vec<u64>,
    count: u64,
}

impl PageSet {
    fn new(pages: u32) -> PageSet {
        PageSet {
            words: vec![0; (pages as usize).div_ceil(64)],
            count: 0,
        }
    }

    fn contains(&self, page: u32) -> bool {
        self.words
            .get(page as usize / 64)
            .is_some_and(|w| w & (1 << (page % 64)) != 0)
    }

    /// Add `page` (which must lie below the set's size); `false` when it was already present.
    fn insert(&mut self, page: u32) -> bool {
        let word = &mut self.words[page as usize / 64];
        let bit = 1u64 << (page % 64);
        if *word & bit != 0 {
            return false;
        }
        *word |= bit;
        self.count += 1;
        true
    }
}

/// A pending B-tree visit: the page, its depth, and the bounds its parent's separators give it.
struct Visit {
    page: PageRef,
    depth: usize,
    lo: Option<Arc<[u8]>>,
    hi: Option<Arc<[u8]>>,
}

/// What one table's rows need to be checked: its columns' types and collations, and its key.
struct TableCtx<'t> {
    table: &'t Table,
    col_types: Arc<Vec<ColType>>,
    colls: Vec<Option<Arc<Collation>>>,
    pk: Vec<(usize, Type)>,
}

struct Checker<'a> {
    database: &'a str,
    snap: &'a Snapshot,
    paging: &'a Arc<SharedPaging>,
    cap: Cap,
    first: u32,
    page_count: u32,
    full: bool,
    max: usize,
    cancel: Option<&'a CancellationToken>,
    report: IntegrityReport,
    reached: RefCell<PageSet>,
    /// Free-list chain pages read and verified.
    chain_pages: u64,
    /// Some tree was unreadable (§5.2): the live page count is not compared.
    damaged: bool,
}

/// Check `snap`, the pinned generation `generation` of database `database` read through `paging`
/// (spec/design/integrity-check.md §3–§5). Shared by [`Database::integrity_check`](crate::Database::integrity_check)
/// and the simulation's image checker. Fails only for a non-corruption read error or cancellation;
/// a page that does not verify is a finding.
pub(crate) fn run(
    snap: &Snapshot,
    paging: &Arc<SharedPaging>,
    generation: IntegrityGeneration,
    database: &str,
    opts: &IntegrityCheckOptions,
) -> Result<IntegrityReport> {
    let cap = paging.cap();
    let mut c = Checker {
        database,
        snap,
        paging,
        cap,
        first: cap.first_body(),
        page_count: generation.page_count,
        full: opts.level == IntegrityLevel::Full,
        max: opts.max_findings as usize,
        cancel: opts.cancel.as_ref(),
        report: IntegrityReport::default(),
        reached: RefCell::new(PageSet::new(generation.page_count)),
        chain_pages: 0,
        damaged: false,
    };
    match c.check(generation) {
        Ok(()) => {}
        Err(Halt::Truncated) => c.report.truncated = true,
        Err(Halt::Error(e)) => return Err(e),
    }
    c.report.pages = c.reached.borrow().count + c.chain_pages;
    Ok(c.report)
}

impl Checker<'_> {
    fn poll(&self) -> Step<()> {
        if let Some(cancel) = self.cancel {
            cancel.check()?;
        }
        Ok(())
    }

    fn add(
        &mut self,
        kind: &'static str,
        object: Option<&str>,
        page: Option<u32>,
        key: Option<&[u8]>,
        detail: String,
    ) -> Step<()> {
        if self.max != 0 && self.report.findings.len() == self.max {
            return Err(Halt::Truncated);
        }
        self.report.findings.push(IntegrityFinding {
            kind,
            database: self.database.to_string(),
            object: object.map(str::to_string),
            page,
            key: key.map(<[u8]>::to_vec),
            detail,
        });
        Ok(())
    }

    fn skip(&mut self, object: &str, reason: &'static str) {
        self.report.skipped.push(IntegritySkip {
            object: object.to_string(),
            reason,
        });
    }

    fn in_body(&self, page: u32) -> bool {
        page >= self.first && page < self.page_count
    }

    /// Record `page` as reached by `object`'s walk; `false` (after reporting) when the walk must not
    /// read it: outside the body range, or reached before (§3.2).
    fn reach(&mut self, object: &str, page: u32) -> Step<bool> {
        if !self.in_body(page) {
            let detail = format!("page {page} outside [{}, {})", self.first, self.page_count);
            self.add(
                kinds::PAGE_OUT_OF_RANGE,
                Some(object),
                Some(page),
                None,
                detail,
            )?;
            return Ok(false);
        }
        if !self.reached.borrow_mut().insert(page) {
            let detail = format!("page {page} is already part of the database");
            self.add(
                kinds::PAGE_REACHED_TWICE,
                Some(object),
                Some(page),
                None,
                detail,
            )?;
            return Ok(false);
        }
        Ok(true)
    }

    fn check(&mut self, generation: IntegrityGeneration) -> Step<()> {
        for &(kind, page) in &generation.findings {
            let detail = if kind == kinds::META_INVALID {
                format!("meta slot {page} no longer verifies")
            } else {
                format!("page {page} no longer carries its manifest identity")
            };
            self.add(kind, None, Some(page), None, detail)?;
        }
        let gist_roots = self.check_catalog(generation.root)?;
        self.check_sequences()?;
        for name in self.snap.table_names() {
            self.check_table(&name, &gist_roots)?;
        }
        self.check_free_list(&generation.free, &generation.protected)?;
        let reached = self.reached.borrow().count;
        if let Some(live) = generation.live_pages
            && !self.damaged
            && u64::from(live) != reached
        {
            let detail = format!("meta records {live} live pages, {reached} are reachable");
            self.add(kinds::LIVE_COUNT_MISMATCH, None, None, None, detail)?;
        }
        Ok(())
    }

    /// Walk the catalog chain (its pages reached first, §3.2) and compare it with the snapshot's
    /// catalog (§3.1). Returns each GiST index's root as the stored chain records it; an index
    /// missing from the map has no known root (the chain could not be read).
    fn check_catalog(&mut self, root: PageRef) -> Step<HashMap<String, PageRef>> {
        let mut pages = Vec::new();
        let mut p = root;
        while p.page != 0 {
            self.poll()?;
            if !self.in_body(p.page) || self.reached.borrow().contains(p.page) {
                let detail = format!("the catalog chain names page {}", p.page);
                self.damaged = true;
                self.add(
                    kinds::CATALOG_PAGE_CORRUPT,
                    None,
                    Some(p.page),
                    None,
                    detail,
                )?;
                return Ok(HashMap::new());
            }
            self.reached.borrow_mut().insert(p.page);
            let problem = match crate::format::integrity_read_page(self.paging, p) {
                Ok(raw)
                    if raw.page_type == crate::format::PAGE_CATALOG_TYPE && raw.flags & !1 == 0 =>
                {
                    let next = raw.next;
                    pages.push((p.page, raw));
                    p = next;
                    None
                }
                Ok(raw) => Some(format!("page type {} in the catalog chain", raw.page_type)),
                Err(e) => Some(corrupt_message(e)?),
            };
            if let Some(detail) = problem {
                self.damaged = true;
                self.add(
                    kinds::CATALOG_PAGE_CORRUPT,
                    None,
                    Some(p.page),
                    None,
                    detail,
                )?;
                return Ok(HashMap::new());
            }
        }
        match crate::format::integrity_catalog(self.snap, self.cap, &pages) {
            Ok((None, gist)) => Ok(gist),
            Ok((Some(page), gist)) => {
                let detail = "the stored catalog is not the encoding of the snapshot's".to_string();
                self.add(kinds::CATALOG_MISMATCH, None, Some(page), None, detail)?;
                Ok(gist)
            }
            Err(e) => {
                let page = pages.first().map_or(root.page, |p| p.0);
                self.add(
                    kinds::CATALOG_MISMATCH,
                    None,
                    Some(page),
                    None,
                    e.message.clone(),
                )?;
                Ok(HashMap::new())
            }
        }
    }

    fn check_sequences(&mut self) -> Step<()> {
        let snap = self.snap;
        for s in snap.sequences_sorted() {
            if s.last_value < s.min_value || s.last_value > s.max_value {
                let detail = format!(
                    "value {} outside [{}, {}]",
                    s.last_value, s.min_value, s.max_value
                );
                self.add(
                    kinds::SEQUENCE_OUT_OF_RANGE,
                    Some(&s.name),
                    None,
                    None,
                    detail,
                )?;
            }
        }
        Ok(())
    }

    fn check_table(&mut self, name: &str, gist_roots: &HashMap<String, PageRef>) -> Step<()> {
        let snap = self.snap;
        let Some(table) = snap.table(name) else {
            return Ok(());
        };
        let store = snap.store(name);
        let ctx = TableCtx {
            table,
            col_types: Arc::new(store.col_types().to_vec()),
            colls: table
                .columns
                .iter()
                .map(|c| c.collation.as_ref().and_then(|n| snap.resolve_collation(n)))
                .collect(),
            pk: table
                .pk
                .iter()
                .map(|&i| (i, table.columns[i].ty.clone()))
                .collect(),
        };
        let root = store.tree_root().map_or(PageRef::NONE, |n| n.page_ref());
        let rows = Cell::new(0u64);
        let stopped = Cell::new(false);
        let readable = self.walk_btree(
            &table.name,
            root,
            &ctx.col_types,
            true,
            &mut |c, page, node| {
                for i in 0..node.len() {
                    c.poll()?;
                    c.report.rows += 1;
                    rows.set(rows.get() + 1);
                    if !c.check_row(&ctx, page, node, i)? {
                        stopped.set(true);
                    }
                }
                Ok(())
            },
        )?;
        let table_ok = readable && !stopped.get();
        if table_ok && store.count() != Some(rows.get() as i64) {
            let detail = format!(
                "catalog records {} rows, the tree holds {}",
                store.count().unwrap_or(0),
                rows.get()
            );
            self.add(
                kinds::ROW_COUNT_MISMATCH,
                Some(&table.name),
                None,
                None,
                detail,
            )?;
        }
        self.damaged |= !table_ok;
        let mut indexes: Vec<&IndexDef> = table.indexes.iter().collect();
        indexes.sort_by(|a, b| a.name.cmp(&b.name));
        for idx in indexes {
            self.check_index(&ctx, idx, table_ok, gist_roots)?;
        }
        Ok(())
    }

    /// Decode row `i` of a table leaf and check it is stored under its primary-key encoding.
    /// Returns `false` when a value finding stopped the row (its table is then unreadable).
    fn check_row(&mut self, ctx: &TableCtx<'_>, page: u32, node: &Node, i: usize) -> Step<bool> {
        let key = node.key_at(i);
        let Some(row) = self.decode_row(ctx, page, node, i, true)? else {
            return Ok(false);
        };
        if !ctx.pk.is_empty() {
            let want = encode_pk_key(&ctx.pk, &ctx.col_types, &ctx.colls, &row);
            if want.as_deref().ok() != Some(key) {
                let detail = "stored key is not the row's primary-key encoding".to_string();
                self.add(
                    kinds::ROW_KEY_NOT_PRIMARY_KEY,
                    Some(&ctx.table.name),
                    Some(page),
                    Some(key),
                    detail,
                )?;
            }
        }
        Ok(true)
    }

    /// Decode every value of row `i`, following its overflow chains. On the first walk (`mark`) the
    /// chains' pages are reached, and a value that does not decode is a finding that stops the row
    /// (`None`); on a re-walk nothing is recorded and any failure fails the call.
    fn decode_row(
        &mut self,
        ctx: &TableCtx<'_>,
        page: u32,
        node: &Node,
        i: usize,
        mark: bool,
    ) -> Step<Option<Vec<Value>>> {
        let mut row = Vec::with_capacity(ctx.col_types.len());
        for (c, ty) in ctx.col_types.iter().enumerate() {
            if node.col_is_null(i, c) {
                row.push(Value::Null);
                continue;
            }
            let violation: Cell<Option<(&'static str, u32)>> = Cell::new(None);
            let decoded = match node.col_at(i, c) {
                Ok(Value::Unfetched(u)) => {
                    let fetch = |p: PageRef| -> Result<Vec<u8>> {
                        if let Some(cancel) = self.cancel {
                            cancel.check()?;
                        }
                        if mark {
                            if !self.in_body(p.page) {
                                violation.set(Some((kinds::PAGE_OUT_OF_RANGE, p.page)));
                                return Err(EngineError::new(
                                    SqlState::DataCorrupted,
                                    format!("overflow page {} out of range", p.page),
                                ));
                            }
                            if !self.reached.borrow_mut().insert(p.page) {
                                violation.set(Some((kinds::PAGE_REACHED_TWICE, p.page)));
                                return Err(EngineError::new(
                                    SqlState::DataCorrupted,
                                    format!("overflow page {} reached twice", p.page),
                                ));
                            }
                        }
                        self.paging.read_body(p)
                    };
                    crate::format::resolve_unfetched(ty, &u, &fetch, self.cap)
                }
                other => other,
            };
            let e = match decoded {
                Ok(v) => {
                    row.push(v);
                    continue;
                }
                Err(e) => e,
            };
            if !mark || e.state != SqlState::DataCorrupted {
                return Err(Halt::Error(e));
            }
            let key = node.key_at(i);
            let column = &ctx.table.columns[c].name;
            let detail = format!("column {column}: {}", e.message);
            let object = Some(ctx.table.name.as_str());
            match violation.get() {
                Some((kind, at)) => self.add(kind, object, Some(at), Some(key), detail)?,
                None => {
                    // A variable column's stored bytes lead with the value's presence tag; a
                    // fixed-width slot is untagged and never out of line.
                    let external = crate::format::fixed_value_width(ty).is_none()
                        && node
                            .col_span(i, c)
                            .flatten()
                            .and_then(|span| span.first())
                            .is_some_and(|t| *t == 0x02 || *t == 0x04);
                    let kind = if external {
                        kinds::OVERFLOW_CHAIN_CORRUPT
                    } else {
                        kinds::VALUE_UNDECODABLE
                    };
                    self.add(kind, object, Some(page), Some(key), detail)?;
                }
            }
            return Ok(None);
        }
        Ok(Some(row))
    }

    /// Walk one B+tree depth-first in child order (format.md *Interior node*), checking page
    /// verification, node types, uniform leaf depth, strict key order within a node, and that every
    /// key lies in the bounds its parent's separators give it; `leaf` sees each leaf in key order.
    /// Returns whether every page was read (§5.2). With `mark` unset (a re-walk of a tree already
    /// checked) nothing is reached or reported, and a page that fails to read fails the call.
    fn walk_btree(
        &mut self,
        object: &str,
        root: PageRef,
        col_types: &Arc<Vec<ColType>>,
        mark: bool,
        leaf: &mut dyn FnMut(&mut Self, u32, &Node) -> Step<()>,
    ) -> Step<bool> {
        let mut readable = true;
        let mut leaf_depth: Option<usize> = None;
        let mut pending: Vec<Visit> = Vec::new();
        if root.page != 0 {
            pending.push(Visit {
                page: root,
                depth: 0,
                lo: None,
                hi: None,
            });
        }
        while let Some(v) = pending.pop() {
            self.poll()?;
            let page = v.page.page;
            if mark && !self.reach(object, page)? {
                readable = false;
                continue;
            }
            if mark && v.depth >= MAX_TREE_DEPTH {
                readable = false;
                let detail = "deeper than any valid tree".to_string();
                self.add(kinds::PAGE_CORRUPT, Some(object), Some(page), None, detail)?;
                continue;
            }
            let read = match crate::format::integrity_read_tree_page(self.paging, v.page, col_types)
            {
                Ok(read) => read,
                Err(e) if mark => {
                    let detail = corrupt_message(e)?;
                    readable = false;
                    self.add(kinds::PAGE_CORRUPT, Some(object), Some(page), None, detail)?;
                    continue;
                }
                Err(e) => return Err(Halt::Error(e)),
            };
            match read {
                TreeRead::Interior { children, seps } => {
                    if mark {
                        let n = seps.len();
                        self.check_keys(object, page, n, &|i| seps[i].as_slice(), &v.lo, &v.hi)?;
                    }
                    let seps: Vec<Arc<[u8]>> = seps.into_iter().map(Arc::from).collect();
                    // Pushed last child first, so the walk visits them in child order.
                    for (i, child) in children.into_iter().enumerate().rev() {
                        pending.push(Visit {
                            page: child,
                            depth: v.depth + 1,
                            lo: if i == 0 {
                                v.lo.clone()
                            } else {
                                Some(Arc::clone(&seps[i - 1]))
                            },
                            hi: if i == seps.len() {
                                v.hi.clone()
                            } else {
                                Some(Arc::clone(&seps[i]))
                            },
                        });
                    }
                }
                TreeRead::Leaf(node) => {
                    if mark {
                        self.check_leaf_depth(object, page, v.depth, &mut leaf_depth)?;
                        let n = node.len();
                        self.check_keys(object, page, n, &|i| node.key_at(i), &v.lo, &v.hi)?;
                    }
                    leaf(self, page, &node)?;
                }
                TreeRead::LeafCorrupt(detail) => {
                    if !mark {
                        return Err(Halt::Error(EngineError::new(
                            SqlState::DataCorrupted,
                            detail,
                        )));
                    }
                    readable = false;
                    self.check_leaf_depth(object, page, v.depth, &mut leaf_depth)?;
                    self.add(kinds::PAGE_CORRUPT, Some(object), Some(page), None, detail)?;
                }
                TreeRead::Other(t) => {
                    if !mark {
                        return Err(Halt::Error(EngineError::new(
                            SqlState::DataCorrupted,
                            format!("page type {t} in a B-tree"),
                        )));
                    }
                    readable = false;
                    let detail = format!("page type {t} in a B-tree");
                    self.add(
                        kinds::PAGE_TYPE_WRONG,
                        Some(object),
                        Some(page),
                        None,
                        detail,
                    )?;
                }
            }
        }
        Ok(readable)
    }

    fn check_leaf_depth(
        &mut self,
        object: &str,
        page: u32,
        depth: usize,
        leaf_depth: &mut Option<usize>,
    ) -> Step<()> {
        let want = *leaf_depth.get_or_insert(depth);
        if depth != want {
            let detail = format!("leaf at depth {depth}, others at {want}");
            self.add(
                kinds::TREE_DEPTH_UNEVEN,
                Some(object),
                Some(page),
                None,
                detail,
            )?;
        }
        Ok(())
    }

    /// At most one `keys_unordered` and one `separator_violation` per page.
    fn check_keys<'k>(
        &mut self,
        object: &str,
        page: u32,
        n: usize,
        key: &dyn Fn(usize) -> &'k [u8],
        lo: &Option<Arc<[u8]>>,
        hi: &Option<Arc<[u8]>>,
    ) -> Step<()> {
        if (1..n).any(|i| key(i - 1) >= key(i)) {
            let detail = "keys do not ascend strictly".to_string();
            self.add(
                kinds::KEYS_UNORDERED,
                Some(object),
                Some(page),
                None,
                detail,
            )?;
        }
        let outside = (0..n).any(|i| {
            let k = key(i);
            lo.as_ref().is_some_and(|lo| k < &lo[..]) || hi.as_ref().is_some_and(|hi| k >= &hi[..])
        });
        if outside {
            let detail = "a key lies outside the bounds its parent gives it".to_string();
            self.add(
                kinds::SEPARATOR_VIOLATION,
                Some(object),
                Some(page),
                None,
                detail,
            )?;
        }
        Ok(())
    }

    /// Walk a GiST R-tree (gist.md §4.1) for page verification, node types, and uniform leaf depth.
    /// Bounding-box containment is slice I3's.
    fn walk_gist(&mut self, object: &str, root: PageRef) -> Step<bool> {
        let mut readable = true;
        let mut leaf_depth: Option<usize> = None;
        let mut pending: Vec<(PageRef, usize)> = Vec::new();
        if root.page != 0 {
            pending.push((root, 0));
        }
        while let Some((page, depth)) = pending.pop() {
            self.poll()?;
            if !self.reach(object, page.page)? {
                readable = false;
                continue;
            }
            if depth >= MAX_TREE_DEPTH {
                readable = false;
                let detail = "deeper than any valid tree".to_string();
                self.add(
                    kinds::PAGE_CORRUPT,
                    Some(object),
                    Some(page.page),
                    None,
                    detail,
                )?;
                continue;
            }
            let parsed = crate::format::integrity_read_page(self.paging, page)
                .and_then(|raw| Ok((raw.page_type, gist_children(&raw, self.cap)?)));
            let (page_type, children) = match parsed {
                Ok(parsed) => parsed,
                Err(e) => {
                    let detail = corrupt_message(e)?;
                    readable = false;
                    self.add(
                        kinds::PAGE_CORRUPT,
                        Some(object),
                        Some(page.page),
                        None,
                        detail,
                    )?;
                    continue;
                }
            };
            let Some(children) = children else {
                readable = false;
                let detail = format!("page type {page_type} in a GiST tree");
                self.add(
                    kinds::PAGE_TYPE_WRONG,
                    Some(object),
                    Some(page.page),
                    None,
                    detail,
                )?;
                continue;
            };
            if page_type == crate::gist::PAGE_GIST_LEAF {
                self.check_leaf_depth(object, page.page, depth, &mut leaf_depth)?;
                continue;
            }
            for child in children.into_iter().rev() {
                pending.push((child, depth + 1));
            }
        }
        Ok(readable)
    }

    fn check_index(
        &mut self,
        ctx: &TableCtx<'_>,
        idx: &IndexDef,
        table_ok: bool,
        gist_roots: &HashMap<String, PageRef>,
    ) -> Step<()> {
        let snap = self.snap;
        let before = self.report.findings.len();
        let mut entries = 0u64;
        let readable = if idx.kind == IndexKind::Gist {
            match gist_roots.get(&idx.name) {
                Some(&root) => self.walk_gist(&idx.name, root)?,
                // The catalog chain could not be read, so the tree's root is not known.
                None => false,
            }
        } else {
            let root = snap
                .index_store(&idx.name)
                .tree_root()
                .map_or(PageRef::NONE, |n| n.page_ref());
            let no_columns = Arc::new(Vec::new());
            self.walk_btree(&idx.name, root, &no_columns, true, &mut |_, _, node| {
                entries += node.len() as u64;
                Ok(())
            })?
        };
        self.damaged |= !readable;
        if !self.full {
            return Ok(());
        }
        if !table_ok {
            self.skip(&idx.name, kinds::SKIP_TABLE_UNREADABLE);
            return Ok(());
        }
        // Any structural finding, even one that left every entry read, skips agreement: a probe
        // through a misordered index can miss entries it holds.
        if !readable || self.report.findings.len() != before {
            self.skip(&idx.name, kinds::SKIP_INDEX_UNREADABLE);
            return Ok(());
        }
        // Agreement for the indexes the reference derives today; the other kinds are slice I3's.
        let derivable = idx.kind == IndexKind::Btree
            && idx.predicate.is_none()
            && idx.column_ordinals().is_some();
        if !derivable {
            return Ok(());
        }
        self.check_agreement(ctx, idx, entries)
    }

    /// Every entry the index derives from each table row, in storage-key order, is found through
    /// the index's own lookup; and the index holds no other entries (§3.4).
    fn check_agreement(&mut self, ctx: &TableCtx<'_>, idx: &IndexDef, entries: u64) -> Step<()> {
        let snap = self.snap;
        let istore = snap.index_store(&idx.name);
        let root = snap
            .store(&ctx.table.name)
            .tree_root()
            .map_or(PageRef::NONE, |n| n.page_ref());
        let found = Cell::new(0u64);
        self.walk_btree(
            &ctx.table.name,
            root,
            &ctx.col_types,
            false,
            &mut |c, page, node| {
                for i in 0..node.len() {
                    c.poll()?;
                    let key = node.key_at(i);
                    let Some(row) = c.decode_row(ctx, page, node, i, false)? else {
                        continue;
                    };
                    let derived = index_entry_keys_columns(
                        &ctx.table.columns,
                        &ctx.col_types,
                        &ctx.colls,
                        idx,
                        key,
                        &row,
                    );
                    match derived {
                        Ok(derived) => {
                            for entry in derived {
                                if istore.get(&entry)?.is_some() {
                                    found.set(found.get() + 1);
                                } else {
                                    let detail = "no entry for the row".to_string();
                                    c.add(
                                        kinds::INDEX_ENTRY_MISSING,
                                        Some(&idx.name),
                                        None,
                                        Some(key),
                                        detail,
                                    )?;
                                }
                            }
                        }
                        Err(e) => {
                            let detail = e.message.clone();
                            c.add(
                                kinds::INDEX_ENTRY_ERROR,
                                Some(&idx.name),
                                None,
                                Some(key),
                                detail,
                            )?;
                        }
                    }
                }
                Ok(())
            },
        )?;
        if entries != found.get() {
            let detail = format!(
                "index holds {entries} entries, {} derive from rows",
                found.get()
            );
            self.add(
                kinds::INDEX_ENTRY_COUNT_MISMATCH,
                Some(&idx.name),
                None,
                None,
                detail,
            )?;
        }
        Ok(())
    }

    /// The free list (§3.3): its chain pages first, then each entry in list order.
    fn check_free_list(&mut self, free: &IntegrityFree, protected: &HashSet<u32>) -> Step<()> {
        let mut chain: Vec<PageRef> = Vec::new();
        let mut chain_set: HashSet<u32> = HashSet::new();
        match free {
            IntegrityFree::Unknown => return Ok(()),
            IntegrityFree::List(_) => {}
            IntegrityFree::Chain(head) => {
                let mut p = *head;
                while p.page != 0 {
                    self.poll()?;
                    if !self.in_body(p.page) || chain_set.contains(&p.page) {
                        let detail = format!("free-list chain names page {}", p.page);
                        self.add(kinds::FREE_LIST_CORRUPT, None, Some(p.page), None, detail)?;
                        break;
                    }
                    if self.reached.borrow().contains(p.page) {
                        let detail = format!("free-list page {} is reachable", p.page);
                        self.add(kinds::FREE_PAGE_LIVE, None, Some(p.page), None, detail)?;
                    }
                    let problem = match crate::format::integrity_read_page(self.paging, p) {
                        Ok(raw)
                            if raw.page_type == crate::format::PAGE_FREELIST
                                && raw.item_count as usize <= raw.content.len() / 4 =>
                        {
                            chain.push(p);
                            chain_set.insert(p.page);
                            self.chain_pages += 1;
                            p = raw.next;
                            None
                        }
                        Ok(raw) if raw.page_type != crate::format::PAGE_FREELIST => {
                            Some("not a free-list page".to_string())
                        }
                        Ok(_) => Some("invalid free-list count".to_string()),
                        Err(e) => Some(corrupt_message(e)?),
                    };
                    if let Some(detail) = problem {
                        self.add(kinds::FREE_LIST_CORRUPT, None, Some(p.page), None, detail)?;
                        break;
                    }
                }
            }
        }
        let mut seen = PageSet::new(self.page_count);
        match free {
            IntegrityFree::List(list) => {
                for &p in list {
                    self.check_free_entry(p, &mut seen, protected)?;
                }
            }
            _ => {
                for &p in &chain {
                    self.poll()?;
                    let raw = crate::format::integrity_read_page(self.paging, p)?;
                    for e in raw.content[..raw.item_count as usize * 4].chunks_exact(4) {
                        let id = u32::from_be_bytes(e.try_into().unwrap());
                        self.check_free_entry(id, &mut seen, protected)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn check_free_entry(
        &mut self,
        p: u32,
        seen: &mut PageSet,
        protected: &HashSet<u32>,
    ) -> Step<()> {
        if !self.in_body(p) {
            let detail = format!(
                "free page {p} outside [{}, {})",
                self.first, self.page_count
            );
            self.add(kinds::FREE_PAGE_OUT_OF_RANGE, None, Some(p), None, detail)
        } else if !seen.insert(p) {
            let detail = format!("free page {p} listed twice");
            self.add(kinds::FREE_PAGE_DUPLICATE, None, Some(p), None, detail)
        } else if self.reached.borrow().contains(p) {
            let detail = format!("free page {p} is reachable");
            self.add(kinds::FREE_PAGE_LIVE, None, Some(p), None, detail)
        } else if protected.contains(&p) {
            let detail = format!("free page {p} is a dependency of the generation");
            self.add(
                kinds::FREE_PAGE_MANIFEST_DEPENDENCY,
                None,
                Some(p),
                None,
                detail,
            )
        } else {
            Ok(())
        }
    }
}

/// A GiST page's child pointers: `Some([])` for a leaf (its entries parsed and discarded), the
/// children of an interior, `None` for another page type (gist.md §4.1).
fn gist_children(raw: &crate::format::RawPage, cap: Cap) -> Result<Option<Vec<PageRef>>> {
    let payload = &raw.content;
    let mut pos = 0usize;
    let take_len = |pos: &mut usize| -> Result<()> {
        let at = *pos;
        let len = payload
            .get(at..at + 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
            .ok_or_else(truncated)?;
        if at + 2 + len > payload.len() {
            return Err(truncated());
        }
        *pos = at + 2 + len;
        Ok(())
    };
    match raw.page_type {
        crate::gist::PAGE_GIST_LEAF => {
            for _ in 0..raw.item_count {
                take_len(&mut pos)?; // the bound
                take_len(&mut pos)?; // the storage key
            }
            Ok(Some(Vec::new()))
        }
        crate::gist::PAGE_GIST_INTERIOR => {
            let mut children = Vec::new();
            for _ in 0..raw.item_count {
                take_len(&mut pos)?;
                children.push(crate::format::integrity_read_ptr(payload, &mut pos, cap)?);
            }
            Ok(Some(children))
        }
        _ => Ok(None),
    }
}

fn truncated() -> EngineError {
    EngineError::new(SqlState::DataCorrupted, "truncated GiST node payload")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind and skip reason the check emits is registered (spec/integrity/findings.toml).
    #[test]
    fn emitted_kinds_are_registered() {
        let emitted = [
            kinds::META_INVALID,
            kinds::MANIFEST_PAGE_MISMATCH,
            kinds::CATALOG_PAGE_CORRUPT,
            kinds::CATALOG_MISMATCH,
            kinds::SEQUENCE_OUT_OF_RANGE,
            kinds::PAGE_OUT_OF_RANGE,
            kinds::PAGE_REACHED_TWICE,
            kinds::PAGE_CORRUPT,
            kinds::PAGE_TYPE_WRONG,
            kinds::TREE_DEPTH_UNEVEN,
            kinds::KEYS_UNORDERED,
            kinds::SEPARATOR_VIOLATION,
            kinds::ROW_KEY_NOT_PRIMARY_KEY,
            kinds::VALUE_UNDECODABLE,
            kinds::OVERFLOW_CHAIN_CORRUPT,
            kinds::ROW_COUNT_MISMATCH,
            kinds::LIVE_COUNT_MISMATCH,
            kinds::FREE_LIST_CORRUPT,
            kinds::FREE_PAGE_OUT_OF_RANGE,
            kinds::FREE_PAGE_DUPLICATE,
            kinds::FREE_PAGE_LIVE,
            kinds::FREE_PAGE_MANIFEST_DEPENDENCY,
            kinds::INDEX_ENTRY_MISSING,
            kinds::INDEX_ENTRY_COUNT_MISMATCH,
            kinds::INDEX_ENTRY_ERROR,
        ];
        for kind in emitted {
            assert!(kinds::FINDING_KINDS.contains(&kind), "{kind} unregistered");
        }
        for reason in [kinds::SKIP_TABLE_UNREADABLE, kinds::SKIP_INDEX_UNREADABLE] {
            assert!(
                kinds::SKIP_REASONS.contains(&reason),
                "{reason} unregistered"
            );
        }
    }

    /// The generated constants are the registry's kinds and reasons, in registry order.
    #[test]
    fn generated_kinds_match_the_registry() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../spec/integrity/findings.toml"
        );
        let text = std::fs::read_to_string(path).expect("read findings.toml");
        let registry: toml::Value = toml::from_str(&text).expect("parse findings.toml");
        let list = |table: &str, field: &str| -> Vec<String> {
            registry[table]
                .as_array()
                .expect("an array of tables")
                .iter()
                .map(|e| e[field].as_str().expect("a string").to_string())
                .collect()
        };
        assert_eq!(list("finding", "kind"), kinds::FINDING_KINDS);
        assert_eq!(list("skip", "reason"), kinds::SKIP_REASONS);
    }
}
