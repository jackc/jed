//! Engine snapshot routing and host configuration (mirrors impl/go engine.go): the Engine methods that
//! resolve a name to the right store (main vs session-temp vs attached: working_mut/temp_read_snap/
//! attach_*_snap, the scoped lookup/write helpers) and the host config/reference-data accessors. The
//! Engine/SessionState/ActiveTx structs stay in mod.rs with the other type definitions.

use super::*;

impl Engine {
    pub fn new() -> Self {
        Engine::with_page_size(DEFAULT_PAGE_SIZE)
    }

    /// An in-memory handle that serializes at `page_size`. The page-backed B-tree's fan-out tracks
    /// the page size (spec/fileformat/format.md), so the in-memory tree must be built at the size it
    /// will serialize to — this builds fixtures / tests a non-default page size; a normal in-memory
    /// database uses [`Engine::new`] (the default page size).
    pub fn with_page_size(page_size: u32) -> Self {
        Engine {
            committed: Snapshot::default(),
            path: None,
            spill_dir: None,
            page_size,
            page_count: 0,
            free_pages: Vec::new(),
            live_at_compaction: 0,
            free_gen_txid: 0,
            paging: None,
            read_only: false,
            session: SessionState::new(),
            temp_storage: None,
            open_streams: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            core: None,
            attached_committed: HashMap::new(),
        }
    }

    /// Build an in-memory handle whose committed state **is** `snap` (no file backing). The
    /// thread-safe shared layer ([`crate::shared`]) uses this to run the unchanged executor against
    /// a snapshot it has pinned from the shared committed cell: a read handle keeps one of these
    /// with no open transaction (reads hit `committed` = the pinned snapshot); a write handle keeps
    /// one with an open READ WRITE block and publishes its working set back to the shared cell.
    pub(crate) fn from_snapshot(snap: Snapshot) -> Self {
        Engine {
            committed: snap,
            path: None,
            spill_dir: None,
            page_size: DEFAULT_PAGE_SIZE,
            page_count: 0,
            free_pages: Vec::new(),
            live_at_compaction: 0,
            free_gen_txid: 0,
            paging: None,
            read_only: false,
            session: SessionState::new(),
            temp_storage: None,
            open_streams: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            core: None,
            attached_committed: HashMap::new(),
        }
    }

    /// The snapshot a read sees: the **read pin** if one is set (a data-modifying `WITH` statement
    /// pins the pre-statement snapshot so every sub-statement reads it — writable-cte.md §2), else
    /// the open transaction's `working` (read-your-writes for a writable tx; the pinned snapshot for
    /// a read-only tx), else the committed snapshot.
    pub(crate) fn read_snap(&self) -> &Snapshot {
        if let Some(pin) = &self.session.read_pin {
            return pin;
        }
        match &self.session.tx {
            Some(tx) => &tx.working,
            None => &self.committed,
        }
    }

    /// Resolve each column's frozen collation (`Column::collation`, the name) to its baked table,
    /// indexed by column ordinal — `None` for a `C` / non-text column (the fast path). The key
    /// encoders (§2.12) consult `colls[ci]` to pick a text column's key form. Returns owned `Arc`
    /// clones (cheap), so the result outlives the snapshot borrow and composes with the mutable
    /// store borrow that phase-2 writes hold (collations are immutable within a statement).
    pub(crate) fn column_collations(
        &self,
        columns: &[Column],
    ) -> Vec<Option<std::sync::Arc<Collation>>> {
        let snap = self.read_snap();
        columns
            .iter()
            .map(|c| c.collation.as_ref().and_then(|n| snap.resolve_collation(n)))
            .collect()
    }

    /// Refuse a WRITE that would maintain a collated B-tree under a **version-skewed** collation
    /// (the slice-2d verdict, spec/design/collation.md §12/§14): if any of `columns` carries a
    /// collation the file pinned to a different `(unicode, cldr)` than the loaded bundle provides,
    /// inserting/updating/deleting/index-building would mix two orderings in one tree and corrupt it,
    /// so the whole table is **read-only** until a REINDEX migration (deferred) rebuilds + re-pins it.
    /// `XX002`, naming the collation + both versions. Reads never call this — they recompute against
    /// the loaded table (the heap-scan fallback, compatibility.md §8). Per-table granularity: one
    /// skewed column collation makes the table read-only (finer per-index gating is a follow-on).
    pub(crate) fn ensure_collations_writable(&self, columns: &[Column]) -> Result<()> {
        let snap = self.read_snap();
        for c in columns {
            if let Some(name) = &c.collation
                && let Some((fu, fc, lu, lc)) = snap.collation_skew(name)
            {
                return Err(EngineError::new(
                    SqlState::CollationVersionMismatch,
                    format!(
                        "collation \"{name}\" version mismatch: this database's keys were built under \
                         {fu}/{fc} but the loaded bundle is {lu}/{lc}; tables using it are read-only \
                         until a REINDEX migration rebuilds them"
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Refresh the main working snapshot's resident GiST trees **iff** the current statement mutated
    /// the main image (spec/design/gist.md §3/§4.1). Run after a statement so a subsequent read —
    /// within the same transaction or, after publish, against the committed snapshot — descends a
    /// fresh, canonically-rebuilt tree. Gated on `main_dirty` (set by the statement's own
    /// `working_mut` writes): a read or a temp-only write leaves it unset, so this is a no-op and
    /// never forces a spurious main-image persist (the temp-no-file-write invariant, temp-tables.md
    /// §2). Trees on temp snapshots are out of scope this slice (GiST on a temp table is
    /// `0A000`, gist.md §11), so only the main working snapshot is refreshed.
    pub(crate) fn rebuild_main_gist_trees_if_dirty(&mut self) -> Result<()> {
        if let Some(tx) = self.session.tx.as_mut()
            && tx.main_dirty
        {
            tx.working.rebuild_gist_trees()?;
        }
        Ok(())
    }

    /// The working snapshot a write mutates — the open transaction's `working`. A write only ever
    /// runs with a transaction open (autocommit opens one implicitly), so this never panics in a
    /// correct flow.
    pub(crate) fn working_mut(&mut self) -> &mut Snapshot {
        let tx = self
            .session
            .tx
            .as_mut()
            .expect("a write statement runs within a transaction");
        // Mark the main image dirty so the commit knows to persist it; a temp-only transaction never
        // reaches here and so makes zero file writes (spec/design/temp-tables.md §2).
        tx.main_dirty = true;
        &mut tx.working
    }

    /// The session's temp-table snapshot for READS (spec/design/temp-tables.md §2): the open
    /// transaction's `temp_working`, else the session's committed temp state. The temp analogue of
    /// [`read_snap`](Engine::read_snap) (it does not consult `read_pin` — a writable-CTE pins only
    /// the main snapshot).
    pub(crate) fn temp_read_snap(&self) -> &Snapshot {
        match &self.session.tx {
            Some(tx) => &tx.temp_working,
            None => &self.session.temp_committed,
        }
    }

    /// The session's temp-table snapshot for WRITES — the open transaction's `temp_working`. A temp
    /// write opens an (implicit autocommit) transaction just like a main write, so this is present;
    /// it also flags `temp_dirty` so the commit can skip persisting the (unchanged) main image.
    pub(crate) fn temp_working_mut(&mut self) -> &mut Snapshot {
        let tx = self
            .session
            .tx
            .as_mut()
            .expect("a temp write statement runs within a transaction");
        tx.temp_dirty = true;
        &mut tx.temp_working
    }

    /// Whether `name` resolves to a SESSION-LOCAL temporary table in the visible temp snapshot
    /// (spec/design/temp-tables.md §3). Preclude-overlaps guarantees a name is temp XOR persistent,
    /// so this is the routing predicate the table/store funnels use.
    pub(crate) fn is_temp_table(&self, name: &str) -> bool {
        self.temp_read_snap().table(name).is_some()
    }

    /// Validate an optional database qualifier on a table reference against the implicit scope
    /// (spec/design/attached-databases.md §3, Slice 1a). A qualified name reaches a specific database:
    /// `main` (the file / persistent database) or `temp` (the session-local domain) — the two reserved
    /// implicit qualifiers this slice recognizes; a host-attached database arrives in Slice 1b, so any
    /// other qualifier is 42P01 "database … is not attached". Because jed precludes overlaps (a name is
    /// temp XOR persistent within a session, §3), a valid qualifier resolves to the SAME store the bare
    /// name would, so this is a VALIDATION GATE, not a routing change: it asserts the named relation
    /// lives in the claimed database (else 42P01), and the downstream temp-first funnel then resolves it
    /// to the matching scope. A `None` qualifier (a bare, implicit-scope name) always passes. The
    /// qualifier is matched case-insensitively (unquoted identifiers fold to lower case).
    pub(crate) fn check_table_qualifier(&self, qualifier: Option<&str>, name: &str) -> Result<()> {
        let Some(q) = qualifier else {
            return Ok(());
        };
        match q.to_ascii_lowercase().as_str() {
            "temp" => {
                if !self.is_temp_table(name) {
                    return Err(EngineError::new(
                        SqlState::UndefinedTable,
                        format!("relation \"temp.{name}\" does not exist"),
                    ));
                }
            }
            "main" => {
                if self.read_snap().table(name).is_none() {
                    return Err(EngineError::new(
                        SqlState::UndefinedTable,
                        format!("relation \"main.{name}\" does not exist"),
                    ));
                }
            }
            _ => {
                // A host-attached database (attached-databases.md §5): the qualifier must name an
                // attachment (else 42P01 "database … is not attached") and it must carry the table
                // (else 42P01 "relation … does not exist"). Slice 1a's default case was always 42P01;
                // Slice 1b routes it to the attachment registry.
                let scope = q.to_ascii_lowercase();
                match self.attach_read_snap(&scope) {
                    None => {
                        return Err(EngineError::new(
                            SqlState::UndefinedTable,
                            format!("database \"{q}\" is not attached"),
                        ));
                    }
                    Some(snap) => {
                        if snap.table(name).is_none() {
                            return Err(EngineError::new(
                                SqlState::UndefinedTable,
                                format!("relation \"{q}.{name}\" does not exist"),
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Reject a WRITE (DML or DDL) targeting a READ-ONLY host attachment with `25006`
    /// (attached-databases.md §4), before any I/O. A `None` scope, or `main`/`temp` (never read-only via
    /// a qualifier — the read-only *handle* path is separate), or a read-write attachment passes.
    /// Unknown attachments are caught by the qualifier gate, so this only inspects the mode.
    pub(crate) fn check_attachment_writable(&self, scope: Option<&str>) -> Result<()> {
        let Some(q) = scope else { return Ok(()) };
        let Some(core) = &self.core else {
            return Ok(());
        };
        let name = q.to_ascii_lowercase();
        if name == "main" || name == "temp" {
            return Ok(());
        }
        if core.attachment_mode(&name) == Some(crate::shared::AttachMode::ReadOnly) {
            return Err(EngineError::new(
                SqlState::ReadOnlySqlTransaction,
                format!("cannot write to read-only database \"{q}\""),
            ));
        }
        Ok(())
    }

    /// Whether this handle's MAIN database is file-backed (durable) rather than in-memory — the input to
    /// the one-durable-writer count (attached-databases.md §5). In the shared-core path the backing path
    /// lives on the core's storage; a standalone engine carries it on `self.path`.
    pub(crate) fn main_is_durable(&self) -> bool {
        match &self.core {
            Some(c) => c.is_file_backed(),
            None => self.path.is_some(),
        }
    }

    /// The page size of a host attachment's OWN page space (attached-databases.md §2) — used to build its
    /// NEW stores (CREATE TABLE / CREATE INDEX) at the size its commit serializes to. A file attachment
    /// carries its own page size, baked into the file, which may differ from main's. The attachment is
    /// known to exist (the qualifier gate passed).
    pub(crate) fn attach_page_size(&self, name: &str) -> u32 {
        self.core
            .as_ref()
            .expect("an attachment write has a shared core")
            .attachment_page_size(name)
    }

    /// The READ snapshot of a host-attached database (attached-databases.md §5) — the transaction's
    /// working clone if this tx wrote it, else the pinned committed root (`attached_committed`). `None`
    /// when no attachment is named `name` (the caller raises 42P01). `name` is expected lowercased.
    pub(crate) fn attach_read_snap(&self, name: &str) -> Option<&Snapshot> {
        if let Some(tx) = &self.session.tx
            && let Some(ws) = tx.attach_working.get(name)
        {
            return Some(ws);
        }
        self.attached_committed.get(name).map(|a| a.as_ref())
    }

    /// The WRITE snapshot of a host-attached database, cloning the pinned committed root into the
    /// transaction's per-attachment working set on first write and marking it dirty (the attachment
    /// analogue of `working_mut`/`temp_working_mut`). A write runs within a transaction, and the
    /// attachment is known to exist (the qualifier gate ran), so this never panics in a correct flow.
    /// `name` is expected lowercased.
    pub(crate) fn attach_write_snap(&mut self, name: &str) -> &mut Snapshot {
        let present = self
            .session
            .tx
            .as_ref()
            .is_some_and(|tx| tx.attach_working.contains_key(name));
        if !present {
            // Clone the committed base BEFORE borrowing `session.tx` mutably (no field-overlap borrow).
            let base = self
                .attached_committed
                .get(name)
                .expect("a write to an attached database resolves its committed root")
                .as_ref()
                .clone();
            let tx = self
                .session
                .tx
                .as_mut()
                .expect("a write statement runs within a transaction");
            tx.attach_working.insert(name.to_string(), base);
        }
        let tx = self
            .session
            .tx
            .as_mut()
            .expect("a write statement runs within a transaction");
        tx.attach_dirty.insert(name.to_string());
        tx.attach_working
            .get_mut(name)
            .expect("the working snapshot was just inserted")
    }

    /// The current READ view of every attached database — the transaction's working clone where this tx
    /// wrote it, else the pinned committed root — as one frozen map. Used to freeze a `snapshot_engine`'s
    /// attachment view (whose own tx is `None`, so it reads straight from this map). Returns
    /// `attached_committed` cloned directly when no attachment has been written this tx (the common case).
    pub(crate) fn attach_read_view(&self) -> HashMap<String, std::sync::Arc<Snapshot>> {
        match &self.session.tx {
            Some(tx) if !tx.attach_working.is_empty() => {
                let mut view = self.attached_committed.clone();
                for (k, v) in &tx.attach_working {
                    view.insert(k.clone(), std::sync::Arc::new(v.clone()));
                }
                view
            }
            _ => self.attached_committed.clone(),
        }
    }

    /// The READ snapshot for an explicit database qualifier (attached-databases.md §3): `main` / `temp`
    /// / a host attachment. Used only when a scope is present; a bare (`None`) name keeps the temp-first
    /// funnels. `None` for an unknown attachment (the qualifier gate already raised 42P01).
    ///
    /// This funnel IS where Slice 1c's "temp is an implicit in-memory attachment" reframe is realized
    /// (attached-databases.md §6): `temp`, `main`, and every host attachment resolve through one
    /// scoped-routing path, so a temp table is a citizen of the same mechanism an attachment is. What
    /// stays deliberately distinct is temp's *lifecycle* — it is SESSION-SCOPED (temp_read_snap reads
    /// session-private state; commit lands on the session's temp root with no cross-session roots
    /// publish; its reclamation watermark is the session's open-cursor count, not the Database-wide live
    /// registry). That divergence is correct, not a gap: relocating temp into the Database-scoped
    /// attachment registry would re-share it across sessions (what Slice 0 removed). So temp routes like
    /// an attachment here but keeps its own home.
    pub(crate) fn snap_for_scope(&self, scope: &str) -> Option<&Snapshot> {
        match scope.to_ascii_lowercase().as_str() {
            "temp" => Some(self.temp_read_snap()),
            "main" => Some(self.read_snap()),
            other => self.attach_read_snap(other),
        }
    }

    /// Validate a catalog relation's database qualifier and return the scope string
    /// `snap_for_scope` resolves at exec (introspection.md §5): `None` (unqualified) ⇒ `"main"`
    /// (the implicit scope); `main`/`temp` pass; any other qualifier must name a host attachment
    /// (else `42P01`, the check_table_qualifier wording). Unlike a user table there is no per-table
    /// existence half — the relation exists in EVERY valid scope, so only the scope itself is
    /// validated.
    pub(crate) fn resolve_catalog_scope(&self, qualifier: Option<&str>) -> Result<String> {
        let Some(q) = qualifier else {
            return Ok("main".to_string());
        };
        let lq = q.to_ascii_lowercase();
        if lq == "main" || lq == "temp" {
            return Ok(lq);
        }
        if self.attach_read_snap(&lq).is_none() {
            return Err(EngineError::new(
                SqlState::UndefinedTable,
                format!("database \"{q}\" is not attached"),
            ));
        }
        Ok(lq)
    }

    /// Resolve a table's catalog entry honoring an explicit database qualifier (attached-databases.md
    /// §3); a `None` scope keeps the bare temp-first walk.
    pub(crate) fn table_scoped(&self, scope: Option<&str>, name: &str) -> Option<&Table> {
        match scope {
            None => self.table(name),
            Some(q) => self.snap_for_scope(q).and_then(|s| s.table(name)),
        }
    }

    /// A table's READ store honoring an explicit database qualifier; a `None` scope keeps the bare
    /// temp-first funnel. The table is known to exist (resolved upstream).
    pub(crate) fn store_scoped(&self, scope: Option<&str>, name: &str) -> &TableStore {
        match scope {
            None => self.store(name),
            Some(q) => match q.to_ascii_lowercase().as_str() {
                "temp" => self.temp_read_snap().store(name),
                "main" => self.read_snap().store(name),
                other => self
                    .attach_read_snap(other)
                    .expect("attachment resolved upstream")
                    .store(name),
            },
        }
    }

    /// A table's WRITE store honoring an explicit database qualifier, marking the right domain dirty
    /// (main / temp / the attachment); a `None` scope keeps the bare temp-first funnel.
    pub(crate) fn store_mut_scoped(&mut self, scope: Option<&str>, name: &str) -> &mut TableStore {
        match scope {
            None => self.store_mut(name),
            Some(q) => match q.to_ascii_lowercase().as_str() {
                "temp" => self.temp_working_mut().store_mut(name),
                "main" => self.working_mut().store_mut(name),
                other => {
                    let other = other.to_string();
                    self.attach_write_snap(&other).store_mut(name)
                }
            },
        }
    }

    /// A secondary index's READ store honoring an explicit database qualifier (an index belongs to the
    /// same database as its table); a `None` scope keeps the bare temp-first funnel.
    pub(crate) fn index_store_scoped(&self, scope: Option<&str>, name_key: &str) -> &TableStore {
        match scope {
            None => self.index_store(name_key),
            Some(q) => match q.to_ascii_lowercase().as_str() {
                "temp" => self.temp_read_snap().index_store(name_key),
                "main" => self.read_snap().index_store(name_key),
                other => self
                    .attach_read_snap(other)
                    .expect("attachment resolved upstream")
                    .index_store(name_key),
            },
        }
    }

    /// A secondary index's WRITE store honoring an explicit database qualifier; a `None` scope keeps the
    /// bare temp-first funnel.
    pub(crate) fn index_store_mut_scoped(
        &mut self,
        scope: Option<&str>,
        name_key: &str,
    ) -> &mut TableStore {
        match scope {
            None => self.index_store_mut(name_key),
            Some(q) => match q.to_ascii_lowercase().as_str() {
                "temp" => self.temp_working_mut().index_store_mut(name_key),
                "main" => self.working_mut().index_store_mut(name_key),
                other => {
                    let other = other.to_string();
                    self.attach_write_snap(&other).index_store_mut(name_key)
                }
            },
        }
    }

    /// The `DROP TYPE … RESTRICT` dependency check across every visible scope (spec/design/temp-tables.md
    /// §8): the main image (tables + composite fields), then the visible session-local temp snapshot
    /// (its tables). A composite type is always persistent, but a TEMP table column may reference it, so
    /// dropping the type while such a temp table exists is 2BP01 — matching the persistent case
    /// (PostgreSQL blocks the drop). A session sees only its own session-local temp tables, so the check
    /// is scoped to what is visible (another session's private temp table is invisible by design — and
    /// its resolved [`ColType`] is self-contained, so it keeps working regardless).
    pub(crate) fn composite_dependent_any(&self, name: &str) -> Option<String> {
        self.read_snap()
            .composite_dependent(name)
            .or_else(|| self.temp_read_snap().composite_dependent(name))
    }

    /// Whether `name` is a secondary index on a SESSION-LOCAL temp table (spec/design/temp-tables.md §8)
    /// — the index analogue of [`is_temp_table`](Engine::is_temp_table), used to gate (`allow_temp_ddl`)
    /// and route a `DROP INDEX` of a temp index. Preclude-overlaps keeps an index name in one scope.
    pub(crate) fn is_temp_index(&self, name: &str) -> bool {
        self.temp_read_snap().find_index(name).is_some()
    }

    /// Resolution walk for a sequence by name (spec/design/sequences.md + temp-tables.md §8):
    /// session-local temp → persistent. Preclude-overlaps keeps a name in at most one scope (the shared
    /// relation namespace), so this is just "where the sequence lives". Every sequence READ
    /// (nextval/currval/setval resolution, DROP/ALTER SEQUENCE) goes through here, so a
    /// `serial`/IDENTITY column's OWNED temp sequence resolves exactly like a persistent one.
    pub(crate) fn sequence(&self, name: &str) -> Option<&SequenceDef> {
        if let Some(s) = self.temp_read_snap().sequence(name) {
            return Some(s);
        }
        self.read_snap().sequence(name)
    }

    /// Whether `name` is a sequence in the SESSION-LOCAL temp snapshot (temp-tables.md §8) — the
    /// sequence analogue of [`is_temp_table`](Engine::is_temp_table). A temp sequence only ever
    /// arises from a `serial`/IDENTITY temp column (standalone CREATE SEQUENCE is always persistent),
    /// so it is always owned. Routes a sequence write/gate to the session-local scope.
    pub(crate) fn is_temp_sequence(&self, name: &str) -> bool {
        self.temp_read_snap().sequence(name).is_some()
    }

    /// Stage a sequence def into whichever scope currently owns its name (flagging the matching dirty
    /// bit): session-local temp, else the main working set. A `serial`/IDENTITY temp column's owned
    /// sequence advances (`nextval` flush) into its temp snapshot, so the advance — like the table's
    /// rows — makes zero file writes (temp-tables.md §2). A brand-new persistent sequence is absent from
    /// the temp scope and lands in the main image.
    pub(crate) fn put_sequence_routed(&mut self, def: SequenceDef) {
        if self.is_temp_sequence(&def.name) {
            self.temp_working_mut().put_sequence(def);
        } else {
            self.working_mut().put_sequence(def);
        }
    }

    /// Remove a sequence from whichever scope owns its name (the routed analogue of
    /// [`put_sequence_routed`](Engine::put_sequence_routed)). Used by `DROP SEQUENCE` and
    /// `DROP TABLE`'s owned-sequence auto-drop.
    pub(crate) fn remove_sequence_routed(&mut self, name: &str) {
        let key = name.to_ascii_lowercase();
        if self.is_temp_sequence(name) {
            self.temp_working_mut().remove_sequence(&key);
        } else {
            self.working_mut().remove_sequence(&key);
        }
    }

    /// Rewrite a column's stored DEFAULT expression in whichever scope owns the table — the routed
    /// analogue used by `ALTER SEQUENCE … RENAME` of an owned sequence (temp-tables.md §8), so a
    /// renamed owned TEMP sequence's `nextval` default is rewritten in the temp snapshot.
    pub(crate) fn set_column_default_expr_routed(
        &mut self,
        table: &str,
        col: usize,
        de: DefaultExpr,
    ) {
        if self.is_temp_table(table) {
            self.temp_working_mut()
                .set_column_default_expr(table, col, de);
        } else {
            self.working_mut().set_column_default_expr(table, col, de);
        }
    }

    /// Enforce the per-session temp-table storage budget (`temp_buffers`, spec/design/temp-tables.md
    /// §7) — the §13 gate on RETAINED temp bytes. Checked after each temp-writing statement: if the
    /// session's temp footprint (byte-identical on-disk record bytes, summed over every temp table +
    /// index) **exceeds** the budget, abort `54P03`. The over-budget write is in `temp_working`, so the
    /// abort discards it (autocommit) or fails the block (rolled back at ROLLBACK) — nothing commits.
    /// `temp_buffers = 0` is unlimited; a transaction that did not touch temp cannot have grown it, so
    /// the check self-gates on `temp_dirty` and is a no-op for ordinary (persistent) statements. The
    /// WITHIN-statement bound is `max_cost` (a single huge temp write hits the cost ceiling first).
    /// The `MemoryBlockStore` paging context for the session-local temp domain (temp-tables.md §6),
    /// lazily creating the domain's storage identity ([`Storage::new_temp`] — a private in-RAM store +
    /// pinned pool with within-session compaction on) on first use.
    pub(crate) fn temp_domain_paging(&mut self) -> std::sync::Arc<crate::paging::SharedPaging> {
        if self.temp_storage.is_none() {
            self.temp_storage = Some(crate::shared::Storage::new_temp(self.page_size));
        }
        self.temp_storage.as_ref().unwrap().paging().clone()
    }

    /// Increment [`Engine::open_streams`] and return the RAII guard that decrements it on `Drop`
    /// (bundled into a streaming cursor's pin — shared.rs). While a guard is live a session-local temp
    /// compaction defers (temp-tables.md §6), so a page the cursor may still fault is never reclaimed.
    pub(crate) fn open_stream_guard(&self) -> OpenStreamGuard {
        self.open_streams
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        OpenStreamGuard {
            count: self.open_streams.clone(),
        }
    }

    pub(crate) fn check_temp_budget(&self) -> Result<()> {
        let limit = self.session.temp_buffers;
        if limit == 0 {
            return Ok(());
        }
        let temp_dirty = self.session.tx.as_ref().is_some_and(|t| t.temp_dirty);
        if !temp_dirty {
            return Ok(());
        }
        // Page-based footprint of the session-local temp domain (temp-tables.md §7, Design decision 3):
        // the committed `MemoryBlockStore` high-water × page size — the honest resident-RAM measure now
        // that temp rides a pager (a record-byte walk would skip demoted `OnDisk` leaves and undercount a
        // multi-leaf temp table, defeating the §13 bound). Deterministic and cross-core-identical:
        // `page_count` is a pure function of operations via the B+tree + within-session compaction. It
        // reflects the state one commit behind (the pending write commits at statement end), so a domain
        // already over budget aborts the NEXT temp write and rolls it back (§7).
        let used = self
            .temp_storage
            .as_ref()
            .map_or(0, |ts| ts.page_count() as u64 * self.page_size as u64);
        if used > limit as u64 {
            return Err(EngineError::new(
                SqlState::TempStorageLimitExceeded,
                format!("temporary table storage exceeded the limit of {limit} bytes"),
            ));
        }
        Ok(())
    }

    /// The committed snapshot, immutable (spec/design/transactions.md §2). Exposed for the host
    /// `Transaction`/read surfaces and for the on-disk serializer.
    pub(crate) fn committed(&self) -> &Snapshot {
        &self.committed
    }
}
