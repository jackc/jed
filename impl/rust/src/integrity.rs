//! The integrity check (spec/design/integrity-check.md): a host operation that walks one database's
//! pinned generation and reports what is wrong with it as structured findings.
//!
//! The walk follows the contract order of §4 — the generation (meta, manifest, catalog, sequences),
//! then each table in name byte order (its tree, then each index's tree and, under `full`, the index's
//! agreement with the table), then the space accounting — so every core reports the same findings in
//! the same order. `spec/fileformat/integrity.rb` is the reference for that order and for what makes
//! a tree unreadable (§5.2). Under the `constraints` option a third stage, after every table's trees,
//! re-checks each table's declared constraints against its stored rows (§3.4).
//!
//! Memory is bounded independent of row count (§5.4, CLAUDE.md §9): one reachability bitmap of
//! `page_count` bits, the pending siblings of one root-to-leaf path per tree, one decoded leaf and one
//! row's values at a time. Index agreement re-walks the table and probes the index once per derived
//! entry through the index's own lookup, counting the index's entries during its structural walk, so
//! neither side is materialized. The constraint stage likewise re-walks a table once per constraint,
//! reads a unique index's entries keeping only the previous entry's key prefix, and looks each
//! foreign key up through the referenced key's tree.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::cancel::CancellationToken;
use crate::catalog::{
    CheckConstraint, ColType, ForeignKeyConstraint, IndexDef, IndexKey, IndexKind, Table,
};
use crate::collation::Collation;
use crate::encoding::KeyOrder;
use crate::error::{EngineError, Result};
use crate::executor::{
    Engine, IndexDeriveSkip, ResolvedIndex, ResolvedKey, Snapshot, Typmod, encode_pk_key,
    fk_match_full_violated, fk_probe, key_body_len, store_value,
};
use crate::format::{Cap, IntegrityFree, IntegrityGeneration, PageRef, TreeRead};
use crate::gist::{DecodedBound, GistOpclass};
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
    /// Also re-check declared constraints against the stored rows — `NOT NULL`, column typmods,
    /// `CHECK`, uniqueness, and foreign keys (§3.4) — as a stage after every table's trees.
    /// Independent of `level`; `false` by default.
    pub constraints: bool,
    /// Cancels the check (`57014`) when cancelled; polled at every page and row.
    pub cancel: Option<CancellationToken>,
}

impl Default for IntegrityCheckOptions {
    fn default() -> Self {
        IntegrityCheckOptions {
            level: IntegrityLevel::Full,
            max_findings: 100,
            constraints: false,
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
    constraints: bool,
    max: usize,
    cancel: Option<&'a CancellationToken>,
    report: IntegrityReport,
    reached: RefCell<PageSet>,
    /// Free-list chain pages read and verified.
    chain_pages: u64,
    /// Some tree was unreadable (§5.2): the live page count is not compared.
    damaged: bool,
    /// Whether each table's tree was read whole, by name (the constraint stage's skips, §3.5).
    tables_ok: HashMap<String, bool>,
    /// Whether each index's structural walk reported nothing, by name (§3.5).
    index_clean: HashMap<String, bool>,
    /// The write path's index-entry derivation, over the pinned snapshot and the handle's
    /// environment (§3.4). Maintenance evaluation is unmetered.
    engine: Engine,
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
        constraints: opts.constraints,
        max: opts.max_findings as usize,
        cancel: opts.cancel.as_ref(),
        report: IntegrityReport::default(),
        reached: RefCell::new(PageSet::new(generation.page_count)),
        chain_pages: 0,
        damaged: false,
        tables_ok: HashMap::new(),
        index_clean: HashMap::new(),
        engine: Engine::from_snapshot(snap.clone()),
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
        let tables = self.snap.table_names();
        for name in &tables {
            self.check_table(name, &gist_roots)?;
        }
        if self.constraints {
            for name in &tables {
                self.check_constraints(name)?;
            }
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
        let ctx = table_ctx(snap, table);
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
        self.tables_ok.insert(table.name.clone(), table_ok);
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

    /// Walk a GiST R-tree (gist.md §4.1): page verification, node types, uniform leaf depth, every
    /// bound decoding under the index's opclasses, and each interior bound containing every bound in
    /// the child it names (§3.2). Returns whether every page was read, and the leaf entry count.
    fn walk_gist(&mut self, object: &str, root: PageRef, ops: &[GistOpclass]) -> Step<(bool, u64)> {
        let mut readable = true;
        let mut entries = 0u64;
        let mut leaf_depth: Option<usize> = None;
        let mut pending: Vec<(PageRef, usize, Option<Arc<DecodedBound>>)> = Vec::new();
        if root.page != 0 {
            pending.push((root, 0, None));
        }
        while let Some((page, depth, parent)) = pending.pop() {
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
                .and_then(|raw| Ok((raw.page_type, gist_items(&raw, ops, self.cap)?)));
            let (page_type, items) = match parsed {
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
            let Some(items) = items else {
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
            let leaf = page_type == crate::gist::PAGE_GIST_LEAF;
            if leaf {
                self.check_leaf_depth(object, page.page, depth, &mut leaf_depth)?;
            }
            if let Some(parent) = &parent
                && !items.iter().all(|item| parent.contains(&item.bound))
            {
                let detail = "a bound lies outside its parent's bound for this page".to_string();
                self.add(
                    kinds::GIST_BOUNDS_VIOLATION,
                    Some(object),
                    Some(page.page),
                    None,
                    detail,
                )?;
            }
            if leaf {
                entries += items.len() as u64;
                continue;
            }
            // Pushed last child first, so the walk visits them in child order.
            for item in items.into_iter().rev() {
                if let GistTarget::Child(child) = item.target {
                    pending.push((child, depth + 1, Some(Arc::new(item.bound))));
                }
            }
        }
        Ok((readable, entries))
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
        let ops = gist_ops(ctx.table, idx);
        let gist_root = gist_roots.get(&idx.name).copied();
        let (readable, entries) = match idx.kind {
            IndexKind::Gist => match gist_root {
                Some(root) => self.walk_gist(&idx.name, root, &ops)?,
                // The catalog chain could not be read, so the tree's root is not known.
                None => (false, 0),
            },
            kind => {
                let root = snap
                    .index_store(&idx.name)
                    .tree_root()
                    .map_or(PageRef::NONE, |n| n.page_ref());
                let no_columns = Arc::new(Vec::new());
                let gin = kind == IndexKind::Gin;
                let mut entries = 0u64;
                let mut runs_ok = true;
                let walked =
                    self.walk_btree(&idx.name, root, &no_columns, true, &mut |c, page, node| {
                        if !gin {
                            entries += node.len() as u64;
                            return Ok(());
                        }
                        // A GIN record is a posting run: count its postings from its tail (§3.2).
                        let mut postings = 0u64;
                        for i in 0..node.len() {
                            match crate::gin_runs::run_postings(node.key_at(i)) {
                                Ok(n) => postings += n,
                                Err(e) => {
                                    runs_ok = false;
                                    let object = Some(idx.name.as_str());
                                    return c.add(
                                        kinds::PAGE_CORRUPT,
                                        object,
                                        Some(page),
                                        None,
                                        e.message.clone(),
                                    );
                                }
                            }
                        }
                        entries += postings;
                        Ok(())
                    })?;
                (walked && runs_ok, entries)
            }
        };
        self.damaged |= !readable;
        self.index_clean.insert(
            idx.name.clone(),
            readable && self.report.findings.len() == before,
        );
        if !self.full {
            return Ok(());
        }
        // The definition and the environment decide the first skips, before any row is read (§3.5).
        let rindex = match self.engine.resolve_index_for_check(ctx.table, idx)? {
            Ok(rindex) => rindex,
            Err(skip) => {
                self.skip(&idx.name, skip_reason(skip));
                return Ok(());
            }
        };
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
        let lookup = match idx.kind {
            IndexKind::Btree => Lookup::Btree,
            IndexKind::Gin => Lookup::Gin,
            IndexKind::Gist => Lookup::Gist(gist_root.unwrap_or(PageRef::NONE), ops),
        };
        self.check_agreement(ctx, idx, &rindex, &lookup, entries)
    }

    /// Every entry the index derives from each table row, in storage-key order, is found through
    /// the index's own lookup; and the index holds no other entries (§3.4). The entries are the
    /// write path's: expression keys, the partial predicate as a filter, GIN terms, the GiST bound.
    fn check_agreement(
        &mut self,
        ctx: &TableCtx<'_>,
        idx: &IndexDef,
        rindex: &ResolvedIndex,
        lookup: &Lookup,
        entries: u64,
    ) -> Step<()> {
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
                    // Each row's derivation has a write's scalar allowance, as one INSERT would.
                    c.engine.session.scalar_bytes.set(0);
                    let derived = c.engine.index_entries(
                        &ctx.table.columns,
                        &ctx.col_types,
                        &ctx.colls,
                        rindex,
                        key,
                        &row,
                    );
                    let derived = match derived {
                        Ok(derived) => derived,
                        Err(e) if e.state == SqlState::QueryCanceled => return Err(Halt::Error(e)),
                        Err(e) => {
                            let detail = format!("{}: {}", e.code(), e.message);
                            c.add(
                                kinds::INDEX_ENTRY_ERROR,
                                Some(&idx.name),
                                None,
                                Some(key),
                                detail,
                            )?;
                            continue;
                        }
                    };
                    for entry in derived {
                        // GIN and GiST entries are `term ‖ k` and `bound ‖ k`.
                        let head = &entry[..entry.len() - key.len()];
                        let present = match lookup {
                            Lookup::Btree => istore.get(&entry)?.is_some(),
                            Lookup::Gin => {
                                match crate::gin_runs::run_at_or_after(&istore, head, key)? {
                                    // A run that does not decode holds nothing (§3.4).
                                    Some(run) => crate::gin_runs::decode_run(head.len(), &run)
                                        .is_ok_and(|keys| keys.iter().any(|k| k == key)),
                                    None => false,
                                }
                            }
                            Lookup::Gist(root, ops) => c.gist_holds(*root, ops, head, key)?,
                        };
                        if present {
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

    /// The GiST lookup (§3.4): descend from `root` into every child whose bound contains `bound`;
    /// present when a leaf holds exactly `bound` with storage key `key`. The tree was walked
    /// cleanly, so a page that no longer reads fails the call.
    fn gist_holds(
        &self,
        root: PageRef,
        ops: &[GistOpclass],
        bound: &[u8],
        key: &[u8],
    ) -> Step<bool> {
        let want = DecodedBound::decode(ops, bound)?;
        let mut pending = Vec::new();
        if root.page != 0 {
            pending.push(root);
        }
        while let Some(page) = pending.pop() {
            self.poll()?;
            let raw = crate::format::integrity_read_page(self.paging, page)?;
            let Some(items) = gist_items(&raw, ops, self.cap)? else {
                continue;
            };
            for item in items {
                match item.target {
                    GistTarget::Leaf(skey) => {
                        if item.raw == bound && skey == key {
                            return Ok(true);
                        }
                    }
                    GistTarget::Child(child) => {
                        if item.bound.contains(&want) {
                            pending.push(child);
                        }
                    }
                }
            }
        }
        Ok(false)
    }

    /// Each table row in storage-key order, decoded, for a pass after the table's tree read whole.
    fn for_each_row(
        &mut self,
        ctx: &TableCtx<'_>,
        f: &mut dyn FnMut(&mut Self, &[u8], &crate::storage::Row) -> Step<()>,
    ) -> Step<()> {
        let root = self
            .snap
            .store(&ctx.table.name)
            .tree_root()
            .map_or(PageRef::NONE, |n| n.page_ref());
        self.walk_btree(
            &ctx.table.name,
            root,
            &ctx.col_types,
            false,
            &mut |c, page, node| {
                for i in 0..node.len() {
                    c.poll()?;
                    let Some(row) = c.decode_row(ctx, page, node, i, false)? else {
                        continue;
                    };
                    f(c, node.key_at(i), &row)?;
                }
                Ok(())
            },
        )?;
        Ok(())
    }

    /// One table's constraints (§3.4): the column pass, then each named constraint in name byte
    /// order, ties by kind (`CHECK`, uniqueness, foreign key).
    fn check_constraints(&mut self, name: &str) -> Step<()> {
        let snap = self.snap;
        let Some(table) = snap.table(name) else {
            return Ok(());
        };
        let mut named: Vec<(&str, u8, Named<'_>)> = Vec::new();
        named.extend(
            table
                .checks
                .iter()
                .map(|c| (c.name.as_str(), 0, Named::Check(c))),
        );
        named.extend(
            table
                .indexes
                .iter()
                .filter(|i| i.unique && i.kind == IndexKind::Btree)
                .map(|i| (i.name.as_str(), 1, Named::Unique(i))),
        );
        named.extend(
            table
                .foreign_keys
                .iter()
                .map(|f| (f.name.as_str(), 2, Named::ForeignKey(f))),
        );
        named.sort_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(&b.1)));
        if !self.tables_ok.get(name).copied().unwrap_or(false) {
            self.skip(name, kinds::SKIP_TABLE_UNREADABLE);
            for (object, ..) in &named {
                self.skip(object, kinds::SKIP_TABLE_UNREADABLE);
            }
            return Ok(());
        }
        let ctx = table_ctx(snap, table);
        self.check_columns(&ctx)?;
        for (_, _, c) in named {
            match c {
                Named::Check(c) => self.check_check(&ctx, c)?,
                Named::Unique(i) => self.check_unique(&ctx, i)?,
                Named::ForeignKey(f) => self.check_foreign_key(&ctx, f)?,
            }
        }
        Ok(())
    }

    /// The column pass (§3.4): per row, per column in ordinal order, a NULL in a `NOT NULL` column,
    /// or a value its column's write-time typmod coercion would reject or change.
    fn check_columns(&mut self, ctx: &TableCtx<'_>) -> Step<()> {
        let table = ctx.table;
        self.for_each_row(ctx, &mut |c, key, row| {
            for (ci, col) in table.columns.iter().enumerate() {
                let v = &row[ci];
                if matches!(v, Value::Null) {
                    if col.not_null || col.primary_key {
                        let detail = format!("column {} is NULL", col.name);
                        let object = Some(table.name.as_str());
                        c.add(kinds::NOT_NULL_VIOLATION, object, None, Some(key), detail)?;
                    }
                } else if !c.typmod_holds(col, &ctx.col_types[ci], v) {
                    let detail = format!("column {} does not fit its typmod", col.name);
                    let object = Some(table.name.as_str());
                    c.add(kinds::TYPMOD_VIOLATION, object, None, Some(key), detail)?;
                }
            }
            Ok(())
        })
    }

    /// Whether storing `v` into `col` through the write path's typmod coercion leaves its stored
    /// bytes unchanged (and does not fail). Top-level scalar columns only (§8).
    fn typmod_holds(&self, col: &crate::catalog::Column, ct: &ColType, v: &Value) -> bool {
        let tm = Typmod::of_column(col);
        let ColType::Scalar(s) = ct else {
            return true;
        };
        if tm == Typmod::NONE {
            return true;
        }
        let iz = self.engine.input_zone();
        match store_value(v.clone(), *s, tm, false, &col.name, &iz) {
            Ok(stored) => {
                crate::format::encode_value(ct, v) == crate::format::encode_value(ct, &stored)
            }
            Err(_) => false,
        }
    }

    /// A `CHECK` (§3.4): each row evaluated as at write time; FALSE is a violation, an error a
    /// `check_error`. Skipped when this environment cannot evaluate it (§3.5).
    fn check_check(&mut self, ctx: &TableCtx<'_>, check: &CheckConstraint) -> Step<()> {
        let rexpr = match self.engine.resolve_check_for_check(ctx.table, check)? {
            Ok(rexpr) => rexpr,
            Err(skip) => {
                self.skip(&check.name, skip_reason(skip));
                return Ok(());
            }
        };
        let object = Some(check.name.as_str());
        self.for_each_row(ctx, &mut |c, key, row| {
            // Each row has a write's scalar allowance, as one INSERT would.
            c.engine.session.scalar_bytes.set(0);
            match c.engine.eval_check_for_check(&rexpr, row) {
                Ok(Value::Bool(false)) => {
                    let detail = "the row fails the check".to_string();
                    c.add(kinds::CHECK_VIOLATION, object, None, Some(key), detail)
                }
                Ok(_) => Ok(()),
                Err(e) if e.state == SqlState::QueryCanceled => Err(Halt::Error(e)),
                Err(e) => {
                    let detail = format!("{}: {}", e.code(), e.message);
                    c.add(kinds::CHECK_ERROR, object, None, Some(key), detail)
                }
            }
        })
    }

    /// Uniqueness (§3.4): the unique index's entries in order, an entry whose key prefix equals the
    /// previous entry's and holds no NULL element repeating it. Only the previous prefix is kept.
    fn check_unique(&mut self, ctx: &TableCtx<'_>, idx: &IndexDef) -> Step<()> {
        if !self.index_clean.get(&idx.name).copied().unwrap_or(false) {
            self.skip(&idx.name, kinds::SKIP_INDEX_UNREADABLE);
            return Ok(());
        }
        // Each key element's resolved type and whether it keys by a collation's sort key; an
        // expression element's come from resolving it.
        let forms: Vec<(ColType, bool)> = if idx.keys.iter().any(|k| matches!(k, IndexKey::Expr(_)))
        {
            let rindex = match self.engine.resolve_index_for_check(ctx.table, idx)? {
                Ok(rindex) => rindex,
                Err(skip) => {
                    self.skip(&idx.name, skip_reason(skip));
                    return Ok(());
                }
            };
            let mut forms = Vec::with_capacity(rindex.keys.len());
            for k in &rindex.keys {
                forms.push(match k {
                    ResolvedKey::Column(ci) => (
                        ctx.col_types[*ci].clone(),
                        ctx.table.columns[*ci].collation.is_some(),
                    ),
                    ResolvedKey::Expr(_, ty, coll) => match key_col_type(ty) {
                        Some(ct) => (ct, coll.is_some()),
                        None => return Ok(()),
                    },
                });
            }
            forms
        } else {
            idx.keys
                .iter()
                .map(|k| match k {
                    IndexKey::Column(ci) => (
                        ctx.col_types[*ci].clone(),
                        ctx.table.columns[*ci].collation.is_some(),
                    ),
                    IndexKey::Expr(_) => unreachable!("a column-only index"),
                })
                .collect()
        };
        let orders: Vec<KeyOrder> = (0..forms.len()).map(|i| idx.key_order(i)).collect();
        let root = self
            .snap
            .index_store(&idx.name)
            .tree_root()
            .map_or(PageRef::NONE, |n| n.page_ref());
        let no_columns = Arc::new(Vec::new());
        let mut prev: Option<Vec<u8>> = None;
        self.walk_btree(&idx.name, root, &no_columns, false, &mut |c, _, node| {
            for i in 0..node.len() {
                c.poll()?;
                let entry = node.key_at(i);
                let Some((len, has_null)) = split_index_entry(entry, &forms, &orders) else {
                    // An entry that does not split names no key to compare.
                    prev = None;
                    continue;
                };
                let prefix = &entry[..len];
                if !has_null && prev.as_deref() == Some(prefix) {
                    let detail = "the key repeats the previous entry's".to_string();
                    let object = Some(idx.name.as_str());
                    c.add(
                        kinds::UNIQUE_VIOLATION,
                        object,
                        None,
                        Some(&entry[len..]),
                        detail,
                    )?;
                }
                prev = (!has_null).then(|| prefix.to_vec());
            }
            Ok(())
        })?;
        Ok(())
    }

    /// A foreign key (§3.4): each referencing row, unless its `MATCH` rule exempts it, is found
    /// through the referenced key exactly as the write path's enforcement looks it up.
    fn check_foreign_key(&mut self, ctx: &TableCtx<'_>, fk: &ForeignKeyConstraint) -> Step<()> {
        let snap = self.snap;
        // A referenced column whose collation is not loaded at the pinned version would encode the
        // probe under the wrong collation (§3.5); decided before the referenced tree's skips.
        if let Some(parent) = snap.table(&fk.ref_table)
            && fk.ref_columns.iter().any(|&ci| {
                parent
                    .columns
                    .get(ci)
                    .is_some_and(|c| self.engine.collation_unavailable(c.collation.as_deref()))
            })
        {
            self.skip(&fk.name, kinds::SKIP_MISSING_LOADED_DATA);
            return Ok(());
        }
        let parent = match snap.table(&fk.ref_table) {
            Some(parent) if self.tables_ok.get(&fk.ref_table).copied().unwrap_or(false) => parent,
            _ => {
                self.skip(&fk.name, kinds::SKIP_TABLE_UNREADABLE);
                return Ok(());
            }
        };
        if let Some(via) = fk_referenced_index(parent, fk)
            && !self.index_clean.get(&via.name).copied().unwrap_or(false)
        {
            self.skip(&fk.name, kinds::SKIP_INDEX_UNREADABLE);
            return Ok(());
        }
        let parent_colls: Vec<Option<Arc<Collation>>> = parent
            .columns
            .iter()
            .map(|c| c.collation.as_ref().and_then(|n| snap.resolve_collation(n)))
            .collect();
        let object = Some(fk.name.as_str());
        self.for_each_row(ctx, &mut |c, key, row| {
            let found = if fk_match_full_violated(fk, row) {
                Err("MATCH FULL with some but not all key columns NULL".to_string())
            } else {
                match fk_probe(fk, parent, &parent_colls, row, &fk.columns) {
                    // MATCH SIMPLE: a NULL key column exempts the row.
                    Ok(None) => Ok(()),
                    Ok(Some(probe)) => {
                        if c.engine.fk_probe_hits(&probe, &fk.ref_table, false)? {
                            Ok(())
                        } else {
                            Err("no referenced row".to_string())
                        }
                    }
                    Err(e) if e.state == SqlState::QueryCanceled => return Err(Halt::Error(e)),
                    Err(e) => Err(format!("{}: {}", e.code(), e.message)),
                }
            };
            match found {
                Ok(()) => Ok(()),
                Err(detail) => c.add(
                    kinds::FOREIGN_KEY_VIOLATION,
                    object,
                    None,
                    Some(key),
                    detail,
                ),
            }
        })
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

/// How agreement finds a derived entry in its index (§3.4).
enum Lookup {
    /// A point `get` of the entry key.
    Btree,
    /// The first run at or after `term ‖ k`, decoded with the term's length.
    Gin,
    /// A descent of the stored R-tree from its root, under the index's opclasses.
    Gist(PageRef, Vec<GistOpclass>),
}

/// A GiST index's opclasses, one per indexed column (gist.md §6, §7); empty for another kind.
fn gist_ops(table: &Table, idx: &IndexDef) -> Vec<GistOpclass> {
    if idx.kind != IndexKind::Gist {
        return Vec::new();
    }
    idx.column_ordinals()
        .unwrap_or_default()
        .iter()
        .map(|&ci| crate::gist::opclass_for(&table.columns[ci].ty))
        .collect()
}

/// What one table's rows need to be checked, from the pinned snapshot.
fn table_ctx<'t>(snap: &'t Snapshot, table: &'t Table) -> TableCtx<'t> {
    TableCtx {
        table,
        col_types: Arc::new(snap.store(&table.name).col_types().to_vec()),
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
    }
}

/// A named constraint of the constraint stage (§3.4), in its kind order.
enum Named<'t> {
    Check(&'t CheckConstraint),
    Unique(&'t IndexDef),
    ForeignKey(&'t ForeignKeyConstraint),
}

fn skip_reason(skip: IndexDeriveSkip) -> &'static str {
    match skip {
        IndexDeriveSkip::Unparsable => kinds::SKIP_UNPARSABLE_DEFINITION,
        IndexDeriveSkip::HostFunction => kinds::SKIP_MISSING_HOST_FUNCTION,
        IndexDeriveSkip::LoadedData => kinds::SKIP_MISSING_LOADED_DATA,
    }
}

/// An expression key's result type as a resolved column type; `None` for one with no structural
/// form (a composite result, which an expression key never has).
fn key_col_type(ty: &Type) -> Option<ColType> {
    Some(match ty {
        Type::Scalar(s) => ColType::Scalar(*s),
        Type::Array(elem) => ColType::Array(Box::new(key_col_type(elem)?)),
        Type::Range(elem) => ColType::Range(Box::new(key_col_type(elem)?)),
        _ => return None,
    })
}

/// The length of an ordered index entry's key prefix (every element's §2.16 slot) and whether an
/// element is NULL; the rest of the entry is its storage key. `None` for an entry that does not
/// split.
fn split_index_entry(
    entry: &[u8],
    forms: &[(ColType, bool)],
    orders: &[KeyOrder],
) -> Option<(usize, bool)> {
    let mut at = 0;
    let mut has_null = false;
    for ((ct, collated), order) in forms.iter().zip(orders) {
        let tag = *entry.get(at)?;
        at += 1;
        if tag == order.null_tag() {
            has_null = true;
            continue;
        }
        if tag != order.present_tag() {
            return None;
        }
        let m = if order.desc { 0xFF } else { 0x00 };
        at += key_body_len(ct, *collated, entry.get(at..)?, m)?;
    }
    Some((at, has_null))
}

/// The unique index a foreign key's write-path lookup goes through (constraints.md §6.8, as
/// [`fk_probe`] chooses it), or `None` when it names the referenced table's primary key.
fn fk_referenced_index<'t>(parent: &'t Table, fk: &ForeignKeyConstraint) -> Option<&'t IndexDef> {
    let sorted = |cols: &[usize]| {
        let mut v = cols.to_vec();
        v.sort_unstable();
        v.dedup();
        v
    };
    let refs = sorted(&fk.ref_columns);
    if !parent.pk.is_empty() && sorted(&parent.pk) == refs {
        return None;
    }
    parent
        .indexes
        .iter()
        .find(|i| i.unique && i.column_ordinals().is_some_and(|c| sorted(&c) == refs))
}

/// What one GiST entry names: a leaf's storage key, or an interior's child page.
enum GistTarget {
    Leaf(Vec<u8>),
    Child(PageRef),
}

/// One GiST node entry: its bound decoded under the index's opclasses and as stored, and its target.
struct GistItem {
    bound: DecodedBound,
    raw: Vec<u8>,
    target: GistTarget,
}

/// A GiST page's entries (gist.md §4.1), every bound decoded exactly under `ops` (`XX001`
/// otherwise); `None` for another page type.
fn gist_items(
    raw: &crate::format::RawPage,
    ops: &[GistOpclass],
    cap: Cap,
) -> Result<Option<Vec<GistItem>>> {
    let leaf = match raw.page_type {
        crate::gist::PAGE_GIST_LEAF => true,
        crate::gist::PAGE_GIST_INTERIOR => false,
        _ => return Ok(None),
    };
    let payload = &raw.content;
    let mut pos = 0usize;
    let take = |pos: &mut usize| -> Result<Vec<u8>> {
        let at = *pos;
        let len = payload
            .get(at..at + 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
            .ok_or_else(truncated)?;
        let bytes = payload.get(at + 2..at + 2 + len).ok_or_else(truncated)?;
        *pos = at + 2 + len;
        Ok(bytes.to_vec())
    };
    let mut items = Vec::with_capacity(raw.item_count as usize);
    for _ in 0..raw.item_count {
        let bound_raw = take(&mut pos)?;
        let bound = DecodedBound::decode(ops, &bound_raw)?;
        let target = if leaf {
            GistTarget::Leaf(take(&mut pos)?)
        } else {
            GistTarget::Child(crate::format::integrity_read_ptr(payload, &mut pos, cap)?)
        };
        items.push(GistItem {
            bound,
            raw: bound_raw,
            target,
        });
    }
    Ok(Some(items))
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
            kinds::GIST_BOUNDS_VIOLATION,
            kinds::LIVE_COUNT_MISMATCH,
            kinds::FREE_LIST_CORRUPT,
            kinds::FREE_PAGE_OUT_OF_RANGE,
            kinds::FREE_PAGE_DUPLICATE,
            kinds::FREE_PAGE_LIVE,
            kinds::FREE_PAGE_MANIFEST_DEPENDENCY,
            kinds::INDEX_ENTRY_MISSING,
            kinds::INDEX_ENTRY_COUNT_MISMATCH,
            kinds::INDEX_ENTRY_ERROR,
            kinds::NOT_NULL_VIOLATION,
            kinds::TYPMOD_VIOLATION,
            kinds::CHECK_VIOLATION,
            kinds::CHECK_ERROR,
            kinds::UNIQUE_VIOLATION,
            kinds::FOREIGN_KEY_VIOLATION,
        ];
        for kind in emitted {
            assert!(kinds::FINDING_KINDS.contains(&kind), "{kind} unregistered");
        }
        for reason in [
            kinds::SKIP_UNPARSABLE_DEFINITION,
            kinds::SKIP_MISSING_HOST_FUNCTION,
            kinds::SKIP_MISSING_LOADED_DATA,
            kinds::SKIP_TABLE_UNREADABLE,
            kinds::SKIP_INDEX_UNREADABLE,
        ] {
            assert!(
                kinds::SKIP_REASONS.contains(&reason),
                "{reason} unregistered"
            );
        }
    }

    /// An index entry splits into its key slots and its storage key for every keyable shape, under
    /// either sort order and NULL placement: scalars, the range, array, and composite containers, and
    /// NULL elements (the uniqueness pass, integrity-check.md §3.4).
    #[test]
    fn index_entries_split_at_their_storage_key() {
        use crate::catalog::ColField;
        use crate::types::ScalarType;
        use crate::value::{ArrayVal, RangeVal};
        let field = |ty: ColType| ColField {
            name: "f".into(),
            ty,
            typmod: None,
            varchar_len: None,
            temporal: None,
            not_null: false,
        };
        let int = |n: i64| Value::Int(n);
        let range = RangeVal {
            empty: false,
            lower: Some(Box::new(int(3))),
            upper: None,
            lower_inc: true,
            upper_inc: false,
        };
        let cases: Vec<(ColType, Value)> = vec![
            (ColType::Scalar(ScalarType::Int32), int(-7)),
            (
                ColType::Scalar(ScalarType::Text),
                Value::Text("a\0b".into()),
            ),
            (
                ColType::Scalar(ScalarType::Decimal),
                Value::Decimal(crate::decimal::Decimal::from_i64(-12345)),
            ),
            (
                ColType::Range(Box::new(ColType::Scalar(ScalarType::Int64))),
                Value::Range(range),
            ),
            (
                ColType::Array(Box::new(ColType::Scalar(ScalarType::Text))),
                Value::Array(ArrayVal {
                    dims: vec![3],
                    lbounds: vec![1],
                    elements: vec![Value::Text("x".into()), Value::Null, Value::Text("".into())],
                }),
            ),
            (
                ColType::Composite {
                    name: "pair".into(),
                    fields: vec![
                        field(ColType::Scalar(ScalarType::Int64)),
                        field(ColType::Scalar(ScalarType::Text)),
                    ],
                },
                Value::Composite(vec![int(1), Value::Null]),
            ),
        ];
        let storage_key = [0x80, 0x00, 0x00, 0x2A];
        for (ct, v) in &cases {
            for desc in [false, true] {
                for nulls_first in [false, true] {
                    let order = KeyOrder { desc, nulls_first };
                    let body = crate::executor::encode_key_ct(ct, v, None).unwrap();
                    // Two elements: the value, then a NULL.
                    let mut entry = Vec::new();
                    order.push_present(&mut entry, &body);
                    entry.push(order.null_tag());
                    let prefix = entry.len();
                    entry.extend_from_slice(&storage_key);
                    let forms = [(ct.clone(), false), (ct.clone(), false)];
                    assert_eq!(
                        split_index_entry(&entry, &forms, &[order, order]),
                        Some((prefix, true)),
                        "{ct:?} {order:?}"
                    );
                    let one = &entry[..1 + body.len()];
                    let mut single = one.to_vec();
                    single.extend_from_slice(&storage_key);
                    assert_eq!(
                        split_index_entry(&single, &forms[..1], &[order]),
                        Some((one.len(), false)),
                        "{ct:?} {order:?}"
                    );
                }
            }
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
