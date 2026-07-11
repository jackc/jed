//! The committed-state snapshot's catalog accessors/mutators (mirrors impl/go snapshot.go): impl Snapshot
//! — table/index/composite-type/sequence/collation/GiST-tree lookup and put/remove, the writer's
//! copy-on-write clone(), dependency analysis, and composite-type validation. The Snapshot struct itself
//! stays in mod.rs with the other type definitions.

use super::*;

impl Snapshot {
    /// Look up a table definition by name (case-insensitive).
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.get(&name.to_ascii_lowercase())
    }

    /// The canonical name of every table in this snapshot, sorted ascending by lowercased name (the
    /// catalog's standing order — no map-iteration order may leak, CLAUDE.md §8). Secondary indexes
    /// are not tables and are excluded (api.md §6).
    pub fn table_names(&self) -> Vec<String> {
        let mut named: Vec<(&str, &str)> = self
            .tables
            .iter()
            .map(|(key, t)| (key.as_str(), t.name.as_str()))
            .collect();
        named.sort_by(|a, b| a.0.cmp(b.0));
        named
            .into_iter()
            .map(|(_, name)| name.to_string())
            .collect()
    }

    /// All tables in ascending lowercased-name order — a deterministic order with no map-iteration
    /// leak (CLAUDE.md §8); the jed_tables / jed_columns generation order
    /// (spec/design/introspection.md §5).
    pub(crate) fn tables_sorted(&self) -> Vec<&Table> {
        let mut keys: Vec<&String> = self.tables.keys().collect();
        keys.sort();
        keys.into_iter().map(|k| &self.tables[k]).collect()
    }

    /// Look up a composite type definition by name (case-insensitive).
    pub fn composite_type(&self, name: &str) -> Option<&CompositeType> {
        self.types.get(&name.to_ascii_lowercase())
    }

    /// Advance the catalog generation — called by every schema mutator (see `cat_gen`). A SELECT
    /// plan cached against a prior generation is thereby invalidated on the next execute.
    pub(crate) fn bump_cat_gen(&mut self) {
        self.cat_gen += 1;
    }

    /// Bind this snapshot's NEW stores to a per-domain `MemoryBlockStore` paging context (the temp seam
    /// — spec/design/temp-tables.md §6, attached-databases.md §6). Set on a host-attached in-memory
    /// database's committed root at attach time (shared.rs) so its tables/indexes ride the same pager +
    /// packed-leaf path as an in-memory database. NEVER serialized (an attachment snapshot never is).
    pub(crate) fn set_store_paging(&mut self, paging: std::sync::Arc<crate::paging::SharedPaging>) {
        self.store_paging = Some(paging);
    }

    /// Register a composite type (CREATE TYPE). Lower-cased name is the key. The caller has
    /// already resolved field types and checked for a duplicate.
    pub(crate) fn put_type(&mut self, ty: CompositeType) {
        self.bump_cat_gen();
        std::sync::Arc::make_mut(&mut self.types).insert(ty.name.to_ascii_lowercase(), ty);
    }

    /// Remove a composite type (DROP TYPE). The caller has checked there are no dependents.
    pub(crate) fn remove_type(&mut self, key: &str) {
        self.bump_cat_gen();
        std::sync::Arc::make_mut(&mut self.types).remove(key);
    }

    /// All composite types in ascending lowercased-name order — the on-disk emission order
    /// (spec/fileformat/format.md) and a deterministic order with no hash-iteration leak (§8).
    pub(crate) fn composite_types_sorted(&self) -> Vec<&CompositeType> {
        let mut keys: Vec<&String> = self.types.keys().collect();
        keys.sort();
        keys.into_iter().map(|k| &self.types[k]).collect()
    }

    /// Look up a sequence by name (case-insensitive).
    pub fn sequence(&self, name: &str) -> Option<&SequenceDef> {
        self.sequences.get(&name.to_ascii_lowercase())
    }

    /// Register a sequence (CREATE SEQUENCE). Lower-cased name is the key. The caller has already
    /// validated the option set and checked the relation namespace for a collision.
    pub(crate) fn put_sequence(&mut self, seq: SequenceDef) {
        std::sync::Arc::make_mut(&mut self.sequences).insert(seq.name.to_ascii_lowercase(), seq);
    }

    /// Remove a sequence (DROP SEQUENCE). The caller has checked it exists.
    pub(crate) fn remove_sequence(&mut self, key: &str) {
        std::sync::Arc::make_mut(&mut self.sequences).remove(key);
    }

    /// Resolve a collation name for USE — query resolution and key encoding (spec/design/collation.md
    /// §2/§9). The collations the database has resolved (a cache populated on open from the file's
    /// reference entries, carrying their version pin) first, then the engine-global **loaded** set
    /// (`db.LoadUnicodeData`, §4). `None` ⇒ neither has it (the resolver raises 42704). `C` is handled
    /// by the caller (built-in). This is the reference-only read path: a collation is never baked into
    /// the file — the file references it by name and the table comes from a loaded bundle.
    pub(crate) fn resolve_collation(&self, name: &str) -> Option<std::sync::Arc<Collation>> {
        self.collations
            .get(name)
            .cloned()
            .or_else(|| crate::collation::loaded_collation(name))
    }

    /// Record a collation resolved from a file reference entry on open (its file metadata + the
    /// vendored table), keyed by name, so later resolution preserves the file's version pin.
    pub(crate) fn put_collation(&mut self, coll: std::sync::Arc<Collation>) {
        std::sync::Arc::make_mut(&mut self.collations).insert(coll.name.clone(), coll);
    }

    /// The slice-2d version-skew verdict for a referenced collation (spec/design/collation.md §12):
    /// `Some((file_unicode, file_cldr, loaded_unicode, loaded_cldr))` if this database's keys were
    /// built under a different `(unicode, cldr)` than the loaded bundle provides — the object that
    /// uses it is read-only (`XX002` on write). `None` ⇒ `Full` (same version, or this collation has
    /// no catalog-local file pin so it is freshly the loaded version — an in-memory-only database).
    /// A pure comparison of the file pin already in the catalog (§5) vs the engine-global loaded set;
    /// `loaded_collation` is `Some` post-open (open refuses an absent reference), so a missing loaded
    /// table is not skew. The `Snapshot`-level wiring of `collation::version_skew`.
    pub(crate) fn collation_skew(&self, name: &str) -> Option<(String, String, String, String)> {
        let cat = self.collations.get(name)?;
        crate::collation::version_skew(name, &cat.unicode_version, &cat.cldr_version).map(
            |(lu, lc)| {
                (
                    cat.unicode_version.clone(),
                    cat.cldr_version.clone(),
                    lu,
                    lc,
                )
            },
        )
    }

    /// The collations the database **schema references** — every column's frozen collation plus the
    /// per-database default — resolved (catalog-local set, then the binary's vendored set) and sorted
    /// by exact name. Under the reference-only model (spec/design/collation.md §2/§5) these, not an
    /// imported set, are what earn a metadata entry on disk: a collation is recorded because the
    /// schema uses it, regardless of whether it was ever passed to a (now-removed) import call. `C`
    /// columns (`collation == None`) reference nothing. A referenced name this build does not vendor
    /// is a bug surfaced here (the precursor to the slice-2d open-time verdict).
    pub(crate) fn referenced_collations(&self) -> Result<Vec<std::sync::Arc<Collation>>> {
        let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for t in self.tables.values() {
            for col in &t.columns {
                if let Some(n) = &col.collation {
                    names.insert(n.clone());
                }
            }
        }
        if let Some(d) = &self.default_collation {
            names.insert(d.clone());
        }
        names
            .into_iter()
            .map(|name| {
                self.resolve_collation(&name).ok_or_else(|| {
                    EngineError::new(
                        SqlState::UndefinedObject,
                        format!(
                            "collation \"{name}\" referenced by the schema is not provided by a loaded bundle"
                        ),
                    )
                })
            })
            .collect()
    }

    /// The REINDEX / COLLATION UPGRADE migration (spec/design/collation.md §12): rebuild every
    /// collated key stored under a version-**skewed** collation against the **loaded** table and
    /// advance that collation's pin to the loaded version — clearing the skew so the affected tables
    /// are read-write again and their collated indexes regain pushdown (a `Full` index,
    /// encoding.md §2.12). Returns the number of collations re-pinned (`0` ⇒ nothing was skewed, a
    /// no-op).
    ///
    /// **Whole-database, per-collation pin.** The pin is **one entry per collation NAME** (§5), so a
    /// collation's pin may advance only once **every** key under it (across all tables) is rebuilt —
    /// else a not-yet-rebuilt table would falsely read as `Full` (silent corruption). This rebuilds
    /// all skewed collations' keys and re-pins them together; the caller swaps the result in atomically
    /// (one root publish). Adoption is **explicit** — never automatic on open (§12).
    ///
    /// `resolve_collation` already yields the loaded table data (the file entry carries the file
    /// *pin* but the loaded singles/contractions — `decode_collation_entry`), so re-encoding a key
    /// produces **loaded-version** sort keys; the re-pin only realigns the version label.
    pub(crate) fn upgrade_collations(&mut self, page_size: u32) -> Result<usize> {
        // 1. The skewed set: referenced collations whose file pin differs from the loaded version.
        let skewed: std::collections::BTreeSet<String> = self
            .referenced_collations()?
            .into_iter()
            .filter(|c| self.collation_skew(&c.name).is_some())
            .map(|c| c.name.clone())
            .collect();
        if skewed.is_empty() {
            return Ok(0);
        }
        let is_skewed = |coll: &Option<String>| coll.as_ref().is_some_and(|n| skewed.contains(n));

        // 2. Rebuild each affected table's collated trees under the loaded collations. Sorted table
        // order so no HashMap iteration order leaks (CLAUDE.md §8); the per-table rebuilds are
        // independent and the re-pin is order-free, so the result is order-invariant regardless, but
        // the sort keeps it manifestly so.
        let mut table_keys: Vec<String> = self.tables.keys().cloned().collect();
        table_keys.sort();
        for key in table_keys {
            let table = self
                .tables
                .get(&key)
                .expect("table key from this map")
                .clone();
            // A collated PK key is re-encoded ⇒ every row's storage key moves ⇒ a full table rewrite,
            // and since an index entry carries the storage key as its suffix (indexes.md §3) every
            // index of the table must be rebuilt too. Otherwise only the indexes whose own key
            // columns use a skewed collation are rebuilt (the table store keeps its keys). A skewed
            // collation used ONLY by a non-key column needs no rebuild — values are version-independent.
            let pk_skewed = table
                .pk
                .iter()
                .any(|&i| is_skewed(&table.columns[i].collation));
            let indexes: Vec<IndexDef> = table
                .indexes
                .iter()
                .filter(|idx| {
                    pk_skewed
                        || idx.column_ordinals().is_some_and(|cols| {
                            cols.iter().any(|&c| is_skewed(&table.columns[c].collation))
                        })
                })
                .cloned()
                .collect();
            if !pk_skewed && indexes.is_empty() {
                continue;
            }
            // The per-column collations resolved against the LOADED set (the table data is loaded;
            // only the pin label is the file version) — what re-encodes each key to the loaded version.
            let colls: Vec<Option<std::sync::Arc<Collation>>> = table
                .columns
                .iter()
                .map(|c| c.collation.as_ref().and_then(|n| self.resolve_collation(n)))
                .collect();
            let pk: Vec<(usize, Type)> = table
                .pk
                .iter()
                .map(|&i| (i, table.columns[i].ty.clone()))
                .collect();
            // Read every (storage key, row) pair, fully materialized (a spilled non-key value must
            // survive a table rewrite). A collated key column never spills (§2.12 narrowing b), so
            // the keys are always inline.
            let mut entries: Vec<(Vec<u8>, Row)> = {
                let store = self.store(&key);
                let mut es = store.iter_entries()?;
                for (_, row) in &mut es {
                    store.resolve_all(row)?;
                }
                es
            };
            // The NEW storage key per row: re-encoded under the loaded collation if the PK moved,
            // else the existing key (unchanged — includes a synthetic rowid table, which has no PK).
            for (k, row) in &mut entries {
                if pk_skewed {
                    *k = encode_pk_key(&pk, &colls, row)?;
                }
            }
            // 2a. Re-key the table store (fresh empty store via `put_table`, then re-insert).
            if pk_skewed {
                self.put_table(table.clone(), page_size);
                for (k, row) in &entries {
                    self.store_mut(&key).insert(k.clone(), row.clone())?;
                }
            }
            // 2b. Rebuild each affected index store from the (re-keyed) rows.
            let cap = crate::format::page_payload(page_size);
            for def in &indexes {
                // The realign runs on a Snapshot with no Engine to evaluate an expression key; an
                // expression index is C-collated so its keys never change on a collation upgrade,
                // but a pk_skewed re-key moves its suffix — that (rare) rebuild is unsupported here
                // (0A000; drop the expression index, upgrade, recreate — indexes.md §7). A PARTIAL
                // index likewise needs the Engine to evaluate its predicate per row, so the realign
                // bails the same way (indexes.md §9).
                if def.column_ordinals().is_none() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "collation upgrade of a table with an expression index is not supported yet"
                            .to_string(),
                    ));
                }
                if def.predicate.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "collation upgrade of a table with a partial index is not supported yet"
                            .to_string(),
                    ));
                }
                let mut ekeys: Vec<Vec<u8>> = Vec::new();
                for (k, row) in &entries {
                    ekeys.extend(index_entry_keys_columns(
                        &table.columns,
                        &colls,
                        def,
                        k,
                        row,
                    )?);
                }
                ekeys.sort_unstable();
                let mut fresh = TableStore::new(cap, Vec::new());
                for ek in ekeys {
                    fresh.insert(ek, Vec::new())?;
                }
                self.put_index_store(def.name.to_ascii_lowercase(), fresh);
            }
        }

        // 3. Advance each skewed collation's pin to the loaded version (realign the label to the
        // table data already in use). `referenced_collations` then resolves the advanced pin and the
        // commit persists it; `collation_skew` now returns `None` (Full) for each.
        for name in &skewed {
            if let Some(loaded) = crate::collation::loaded_collation(name) {
                self.put_collation(loaded);
            }
        }
        Ok(skewed.len())
    }

    /// The per-database default collation name, or `None` for `C` (spec/design/collation.md §1).
    pub(crate) fn default_collation(&self) -> Option<&str> {
        self.default_collation.as_deref()
    }

    /// Set the per-database default collation (`db.set_default_collation`). `None` ⇒ `C`. The caller
    /// has validated the name is loaded.
    pub(crate) fn set_default_collation(&mut self, name: Option<String>) {
        self.default_collation = name;
    }

    /// All sequences in ascending lowercased-name order — the on-disk emission order
    /// (spec/fileformat/format.md) and a deterministic order with no hash-iteration leak (§8).
    pub(crate) fn sequences_sorted(&self) -> Vec<&SequenceDef> {
        let mut keys: Vec<&String> = self.sequences.keys().collect();
        keys.sort();
        keys.into_iter().map(|k| &self.sequences[k]).collect()
    }

    /// The lowercased keys of every sequence **owned by** the table `name` (case-insensitive) — the
    /// `serial`-created sequences `DROP TABLE` must auto-drop (spec/design/sequences.md §12). Returned
    /// in ascending key order so the auto-drop is deterministic (no hash-iteration leak, §8).
    pub(crate) fn sequences_owned_by(&self, name: &str) -> Vec<String> {
        let mut keys: Vec<String> = self
            .sequences
            .iter()
            .filter(|(_, s)| {
                s.owned_by
                    .as_ref()
                    .is_some_and(|o| o.table.eq_ignore_ascii_case(name))
            })
            .map(|(k, _)| k.clone())
            .collect();
        keys.sort();
        keys
    }

    /// Whether any table column or composite-type field still references the composite type
    /// `name` (case-insensitive) — the `DROP TYPE ... RESTRICT` dependency check (2BP01). Returns
    /// the first dependent's description for the error detail, or `None` if there are no dependents.
    pub(crate) fn composite_dependent(&self, name: &str) -> Option<String> {
        let key = name.to_ascii_lowercase();
        // `composite_ref` looks through one array level, so an `addr[]` column / field counts as a
        // dependent of `addr` exactly as a bare `addr` one does (spec/design/array.md §12).
        for t in self.tables.values() {
            for c in &t.columns {
                if c.ty
                    .composite_ref()
                    .is_some_and(|r| r.name.eq_ignore_ascii_case(&key))
                {
                    return Some(format!("column {} of table {}", c.name, t.name));
                }
            }
        }
        for ct in self.types.values() {
            for f in &ct.fields {
                if f.ty
                    .composite_ref()
                    .is_some_and(|r| r.name.eq_ignore_ascii_case(&key))
                {
                    return Some(format!("field {} of type {}", f.name, ct.name));
                }
            }
        }
        None
    }

    /// Every FK on a table **not** in `dropping` (a set of lowercased table keys) that references
    /// a table that **is** in `dropping` — the dependency scan for a multi-table `DROP TABLE`
    /// (spec/design/grammar.md §13, constraints.md §6.10). A dependent whose referencing table is
    /// itself being dropped does not count (the drop-set exclusion), so a FK between two tables
    /// both named in the same statement never blocks. Referencing tables are scanned in ascending
    /// lowercased key order (each table's `foreign_keys` is already name-ordered) for determinism
    /// (§8). `RESTRICT` raises 2BP01 on the first entry; `CASCADE` removes every entry's FK.
    pub(crate) fn foreign_key_dependents_excluding(
        &self,
        dropping: &BTreeSet<String>,
    ) -> Vec<FkDependent> {
        let mut out = Vec::new();
        let mut tkeys: Vec<&String> = self.tables.keys().collect();
        tkeys.sort();
        for tk in tkeys {
            if dropping.contains(tk) {
                continue; // the referencing table is itself being dropped — no dependency
            }
            let t = &self.tables[tk];
            for fk in &t.foreign_keys {
                let ref_key = fk.ref_table.to_ascii_lowercase();
                if dropping.contains(&ref_key) {
                    let dropped_name = self
                        .tables
                        .get(&ref_key)
                        .map_or_else(|| fk.ref_table.clone(), |d| d.name.clone());
                    out.push(FkDependent {
                        ref_table_key: tk.clone(),
                        fk_name: fk.name.clone(),
                        ref_table_name: t.name.clone(),
                        dropped_name,
                    });
                }
            }
        }
        out
    }

    /// Remove the named FK constraint from `table_key` in place, preserving the table's store and
    /// rows — `DROP TABLE … CASCADE`'s removal of a dependent FK on a table that *survives* the
    /// drop (spec/design/grammar.md §13). Only the catalog `foreign_keys` list changes; an FK
    /// owns no B-tree (constraints.md §6), so there is nothing else to remove.
    pub(crate) fn remove_foreign_key(&mut self, table_key: &str, fk_name: &str) {
        if let Some(table) = std::sync::Arc::make_mut(&mut self.tables).get_mut(table_key) {
            self.cat_gen += 1;
            table
                .foreign_keys
                .retain(|fk| !fk.name.eq_ignore_ascii_case(fk_name));
        }
    }

    /// Validate the loaded composite-type catalog (the on-disk two-pass load —
    /// spec/design/composite.md §3): every composite a field references must exist, the reference
    /// graph must be acyclic, and no type may nest deeper than [`MAX_COMPOSITE_DEPTH`]. A dangling,
    /// cyclic, or over-deep reference is a malformed file (`XX001`). Called once after the whole
    /// catalog is read, and **before** any store is built — so the subsequent `resolve_col_type`
    /// walks (and every later value-codec/comparator walk) recurse over a depth-bounded catalog and
    /// stay stack-safe (CLAUDE.md §13; cost.md §7b).
    pub(crate) fn validate_composite_types(&self) -> Result<()> {
        // Existence: every composite a field references (directly, or as an array element —
        // `composite_ref` looks through one array level) names a registered type.
        for ct in self.types.values() {
            for f in &ct.fields {
                if let Some(r) = f.ty.composite_ref() {
                    if self.composite_type(&r.name).is_none() {
                        return Err(EngineError::new(
                            SqlState::DataCorrupted,
                            format!(
                                "composite type {} references unknown type {}",
                                ct.name, r.name
                            ),
                        ));
                    }
                }
            }
        }
        // One DFS over the type → referenced-types graph that enforces BOTH acyclicity and the
        // nesting-depth bound (color: 0 unvisited, 1 on-stack, 2 done; `cache` memoizes each done
        // type's absolute nesting depth). Two guards make it stack-safe AND sound regardless of
        // visitation order: `levels_above >= MAX` bounds the native recursion on a fresh descent,
        // and the post-compute `depth > MAX` check catches an over-deep type reached via a memoized
        // (color-2) shortcut — which the descent guard alone would miss when the catalog is colored
        // bottom-up. Existence ran first, so every referenced type is present.
        fn visit(
            snap: &Snapshot,
            key: &str,
            levels_above: usize,
            color: &mut HashMap<String, u8>,
            cache: &mut HashMap<String, usize>,
        ) -> Result<usize> {
            if levels_above >= MAX_COMPOSITE_DEPTH {
                return Err(EngineError::new(
                    SqlState::DataCorrupted,
                    format!(
                        "composite type nesting exceeds the maximum depth of {MAX_COMPOSITE_DEPTH}"
                    ),
                ));
            }
            match color.get(key).copied().unwrap_or(0) {
                1 => {
                    return Err(EngineError::new(
                        SqlState::DataCorrupted,
                        format!("composite type definition cycle through {key}"),
                    ));
                }
                2 => return Ok(*cache.get(key).unwrap_or(&1)),
                _ => {}
            }
            color.insert(key.to_string(), 1);
            let mut child = 0;
            if let Some(ct) = snap.types.get(key) {
                for f in &ct.fields {
                    if let Some(r) = f.ty.composite_ref() {
                        let ck = r.name.to_ascii_lowercase();
                        child = child.max(visit(snap, &ck, levels_above + 1, color, cache)?);
                    }
                }
            }
            let depth = 1 + child;
            if depth > MAX_COMPOSITE_DEPTH {
                return Err(EngineError::new(
                    SqlState::DataCorrupted,
                    format!(
                        "composite type nesting exceeds the maximum depth of {MAX_COMPOSITE_DEPTH}"
                    ),
                ));
            }
            color.insert(key.to_string(), 2);
            cache.insert(key.to_string(), depth);
            Ok(depth)
        }
        let mut color: HashMap<String, u8> = HashMap::new();
        let mut cache: HashMap<String, usize> = HashMap::new();
        let keys: Vec<String> = self.types.keys().cloned().collect();
        for k in keys {
            if color.get(&k).copied().unwrap_or(0) == 0 {
                visit(self, &k, 0, &mut color, &mut cache)?;
            }
        }
        Ok(())
    }

    /// The composite-type nesting depth of `ty` against this snapshot's type catalog, memoized in
    /// `cache` (lowercased name → depth): a scalar is 0, `T[]` is `depth(T)` (array levels are not
    /// composite levels — `composite_ref` looks through one array level the same way), and a
    /// composite is `1 + max(field depths)` (an empty composite is 1). The `CREATE TYPE` gate uses
    /// this against the *existing* catalog, every type of which already satisfies depth ≤
    /// [`MAX_COMPOSITE_DEPTH`] (the load + create invariant), so the recursion is bounded by the
    /// limit; memoization keeps a diamond-shaped reference graph linear (spec/design/cost.md §7b).
    pub(crate) fn composite_type_depth(
        &self,
        ty: &Type,
        cache: &mut HashMap<String, usize>,
    ) -> usize {
        let r = match ty.composite_ref() {
            Some(r) => r,
            None => return 0, // a scalar (or a scalar array) adds no composite level
        };
        let key = r.name.to_ascii_lowercase();
        if let Some(&d) = cache.get(&key) {
            return d;
        }
        let depth = match self.types.get(&key) {
            Some(def) => {
                1 + def
                    .fields
                    .iter()
                    .map(|f| self.composite_type_depth(&f.ty, cache))
                    .max()
                    .unwrap_or(0)
            }
            None => 1,
        };
        cache.insert(key, depth);
        depth
    }

    /// The store for a table (panics if absent — callers resolve the table first).
    pub(crate) fn store(&self, name: &str) -> &TableStore {
        self.stores
            .get(&name.to_ascii_lowercase())
            .expect("store exists for a resolved table")
    }

    /// The store for a table, mutable (panics if absent).
    pub(crate) fn store_mut(&mut self, name: &str) -> &mut TableStore {
        std::sync::Arc::make_mut(&mut self.stores)
            .get_mut(&name.to_ascii_lowercase())
            .expect("store exists for a resolved table")
    }

    /// All rows of a table in primary-key (encoded byte) order, or None if the table is absent. A
    /// test/debug convenience (the SELECT path scans through `iter_in_key_order` directly, propagating
    /// I/O errors); every value is fully materialized — the helper's callers compare whole rows, so
    /// no unfetched reference may escape (large-values.md §14). The fault-`Result` is unwrapped here.
    pub(crate) fn rows_in_key_order(&self, name: &str) -> Option<Vec<Row>> {
        self.stores.get(&name.to_ascii_lowercase()).map(|s| {
            let mut rows = s.iter_in_key_order().expect("test-helper read failed");
            for row in &mut rows {
                s.resolve_all(row).expect("test-helper resolve failed");
            }
            rows
        })
    }

    /// Register a new table and its (empty) store. Lower-cased name is the key. The store carries
    /// the page payload `cap` (= `page_size − 16`) and the column types so the page-backed B-tree
    /// can weigh records for its size-driven split (spec/fileformat/format.md).
    pub(crate) fn put_table(&mut self, table: Table, page_size: u32) {
        // Resolve each column's `ColType` against the (already-registered) composite-type catalog
        // — the codec/coercion tree the store keeps so neither re-walks the type catalog per row
        // (spec/design/composite.md §4). Composite types are registered before any table (the
        // types-first catalog order / `CREATE TYPE`-before-`CREATE TABLE` rule), so the lookup
        // inside `resolve_col_type` always resolves.
        let col_types: Vec<ColType> = table
            .columns
            .iter()
            .map(|c| resolve_col_type(&c.ty, &self.types))
            .collect();
        self.put_table_resolved(table, col_types, page_size);
    }

    /// Register a table whose column `ColType`s are **already resolved** — used when staging a TEMP
    /// table (spec/design/temp-tables.md §8): a temp table's composite columns must resolve against
    /// the MAIN snapshot's type catalog (composites are never temp — `CREATE TYPE` is persistent),
    /// not this (temp) snapshot's empty `types` map. The resolved [`ColType`] tree is fully
    /// self-contained (spec/design/composite.md §4), so the store needs nothing from the catalog
    /// thereafter. The plain [`put_table`](Snapshot::put_table) resolves against `self.types` and
    /// delegates here.
    pub(crate) fn put_table_resolved(
        &mut self,
        table: Table,
        col_types: Vec<ColType>,
        page_size: u32,
    ) {
        self.bump_cat_gen();
        let key = table.name.to_ascii_lowercase();
        let cap = crate::format::page_payload(page_size);
        let mut st = TableStore::new(cap, col_types);
        // Bind the domain's pager (`Snapshot::store_paging`) so the new store demand-pages like a
        // loaded one: its committed leaves demote at each commit (`demote_clean_leaves`) and fault
        // back through the pool, instead of staying fully-resident decoded for the handle's lifetime.
        // `None` only on a bare scratch engine that never persists.
        if let Some(paging) = &self.store_paging {
            st.attach_paging(paging.clone());
        }
        std::sync::Arc::make_mut(&mut self.stores).insert(key.clone(), st);
        std::sync::Arc::make_mut(&mut self.tables).insert(key, table);
    }

    /// Remove a table's definition, its store, and its indexes' stores (DROP TABLE — the
    /// indexes have no independent life, spec/design/indexes.md §2).
    pub(crate) fn remove_table(&mut self, key: &str) {
        self.bump_cat_gen();
        if let Some(t) = self.tables.get(key) {
            // Disjoint field borrows: `t` reads `self.tables` while we mutate `self.index_stores`.
            let index_stores = std::sync::Arc::make_mut(&mut self.index_stores);
            for idx in &t.indexes {
                index_stores.remove(&idx.name.to_ascii_lowercase());
            }
        }
        std::sync::Arc::make_mut(&mut self.tables).remove(key);
        std::sync::Arc::make_mut(&mut self.stores).remove(key);
    }

    /// The store of a secondary index (panics if absent — callers resolve the index first).
    pub(crate) fn index_store(&self, name_key: &str) -> &TableStore {
        self.index_stores
            .get(name_key)
            .expect("store exists for a resolved index")
    }

    /// The store of a secondary index, mutable (panics if absent).
    pub(crate) fn index_store_mut(&mut self, name_key: &str) -> &mut TableStore {
        std::sync::Arc::make_mut(&mut self.index_stores)
            .get_mut(name_key)
            .expect("store exists for a resolved index")
    }

    /// Whether this snapshot holds a store for the named index (lowercased key). Used to route
    /// index access to the session temp snapshot vs the main snapshot (temp-tables.md §2).
    pub(crate) fn has_index_store(&self, name_key: &str) -> bool {
        self.index_stores.contains_key(name_key)
    }

    /// Total on-disk record bytes of every table store + index store in this snapshot — the temp
    /// budget's deterministic footprint measure (spec/design/temp-tables.md §7), summed over the
    /// session temp snapshot. Iteration order does not matter (it is a sum).
    pub(crate) fn storage_bytes(&self) -> u64 {
        let tables: u64 = self.stores.values().map(|s| s.stored_bytes()).sum();
        let indexes: u64 = self.index_stores.values().map(|s| s.stored_bytes()).sum();
        tables + indexes
    }

    /// Register a new (empty) secondary index on `table_key`: insert its definition into the
    /// table's `indexes` in ascending lowercased-name order (the catalog/planner order —
    /// spec/design/indexes.md §6) and create its zero-column store.
    pub(crate) fn put_index(&mut self, table_key: &str, def: IndexDef, page_size: u32) {
        self.bump_cat_gen();
        let name_key = def.name.to_ascii_lowercase();
        let cap = crate::format::page_payload(page_size);
        let mut fresh = TableStore::new(cap, Vec::new());
        if let Some(paging) = &self.store_paging {
            // Bind the domain pager, like put_table_resolved / put_index_store.
            fresh.attach_paging(paging.clone());
        }
        std::sync::Arc::make_mut(&mut self.index_stores).insert(name_key.clone(), fresh);
        let table = std::sync::Arc::make_mut(&mut self.tables)
            .get_mut(table_key)
            .expect("table exists");
        let pos = table
            .indexes
            .iter()
            .position(|i| i.name.to_ascii_lowercase() > name_key)
            .unwrap_or(table.indexes.len());
        table.indexes.insert(pos, def);
    }

    /// Replace a table column's expression default **in place**, leaving the table's rows and store
    /// untouched — used by `ALTER SEQUENCE … RENAME` of an owned sequence to rewrite the owning
    /// column's `nextval` default (spec/design/sequences.md §15.3). `put_table` cannot be used here:
    /// it rebuilds a fresh empty store. A no-op if the table or column ordinal is absent.
    pub(crate) fn set_column_default_expr(
        &mut self,
        table_key: &str,
        column: usize,
        default_expr: DefaultExpr,
    ) {
        if let Some(table) = std::sync::Arc::make_mut(&mut self.tables).get_mut(table_key) {
            if let Some(col) = table.columns.get_mut(column) {
                col.default_expr = Some(default_expr);
                self.cat_gen += 1;
            }
        }
    }

    /// Publish one validated ALTER TABLE catalog entry without touching row bytes.
    pub(crate) fn alter_table_catalog(
        &mut self,
        old_key: &str,
        table: Table,
        rename_table: bool,
        index_rename: Option<(&str, &str)>,
    ) {
        self.bump_cat_gen();
        let new_key = if rename_table {
            table.name.to_ascii_lowercase()
        } else {
            old_key.to_string()
        };
        if rename_table {
            std::sync::Arc::make_mut(&mut self.tables).remove(old_key);
            if let Some(store) = std::sync::Arc::make_mut(&mut self.stores).remove(old_key) {
                std::sync::Arc::make_mut(&mut self.stores).insert(new_key.clone(), store);
            }
        }
        std::sync::Arc::make_mut(&mut self.tables).insert(new_key.clone(), table.clone());
        if let Some((old, new)) = index_rename {
            let old = old.to_ascii_lowercase();
            let new = new.to_ascii_lowercase();
            if let Some(store) = std::sync::Arc::make_mut(&mut self.index_stores).remove(&old) {
                std::sync::Arc::make_mut(&mut self.index_stores).insert(new.clone(), store);
            }
            if let Some(tree) = std::sync::Arc::make_mut(&mut self.gist_trees).remove(&old) {
                std::sync::Arc::make_mut(&mut self.gist_trees).insert(new, tree);
            }
        }
        if !rename_table {
            return;
        }
        let keys: Vec<String> = self.tables.keys().cloned().collect();
        for key in keys {
            let Some(old) = self.tables.get(&key) else {
                continue;
            };
            if !old
                .foreign_keys
                .iter()
                .any(|fk| fk.ref_table.eq_ignore_ascii_case(old_key))
            {
                continue;
            }
            let mut changed = old.clone();
            for fk in &mut changed.foreign_keys {
                if fk.ref_table.eq_ignore_ascii_case(old_key) {
                    fk.ref_table = table.name.clone();
                }
            }
            std::sync::Arc::make_mut(&mut self.tables).insert(key, changed);
        }
        let seq_keys: Vec<String> = self.sequences.keys().cloned().collect();
        for key in seq_keys {
            let Some(seq) = self.sequences.get(&key) else {
                continue;
            };
            if !seq
                .owned_by
                .as_ref()
                .is_some_and(|o| o.table.eq_ignore_ascii_case(old_key))
            {
                continue;
            }
            let mut changed = seq.clone();
            changed.owned_by.as_mut().unwrap().table = table.name.clone();
            std::sync::Arc::make_mut(&mut self.sequences).insert(key, changed);
        }
    }

    pub(crate) fn sync_alter_constraint_indexes(
        &mut self,
        old: &Table,
        next: &Table,
        entries: &std::collections::HashMap<String, Vec<Vec<u8>>>,
        page_size: u32,
    ) -> Result<()> {
        let live: std::collections::HashSet<String> = next
            .indexes
            .iter()
            .map(|i| i.name.to_ascii_lowercase())
            .collect();
        for i in &old.indexes {
            let k = i.name.to_ascii_lowercase();
            if !live.contains(&k) {
                std::sync::Arc::make_mut(&mut self.index_stores).remove(&k);
                std::sync::Arc::make_mut(&mut self.gist_trees).remove(&k);
            }
        }
        let prior: std::collections::HashSet<String> = old
            .indexes
            .iter()
            .map(|i| i.name.to_ascii_lowercase())
            .collect();
        for i in &next.indexes {
            let k = i.name.to_ascii_lowercase();
            if prior.contains(&k) {
                continue;
            }
            let mut s = TableStore::new(crate::format::page_payload(page_size), Vec::new());
            if let Some(p) = &self.store_paging {
                s.attach_paging(p.clone());
            }
            for e in entries.get(&k).into_iter().flatten() {
                s.insert(e.clone(), Vec::new())?;
            }
            std::sync::Arc::make_mut(&mut self.index_stores).insert(k, s);
        }
        Ok(())
    }

    pub(crate) fn cascade_dropped_unique_fks(&mut self, parent: &str, dropped: &[Vec<usize>]) {
        if dropped.is_empty() {
            return;
        }
        for child in std::sync::Arc::make_mut(&mut self.tables).values_mut() {
            if child.name.eq_ignore_ascii_case(parent) {
                continue;
            }
            child.foreign_keys.retain(|fk| {
                !fk.ref_table.eq_ignore_ascii_case(parent)
                    || !dropped.iter().any(|c| *c == sorted_unique(&fk.ref_columns))
            });
        }
    }

    /// Register a loaded index store under its (lowercased) name — the file loader's hook
    /// (format.rs): the owning table's `indexes` list came from its catalog entry, so only
    /// the store is registered here.
    pub(crate) fn put_index_store(&mut self, name_key: String, mut store: TableStore) {
        // An index store created in-session binds the domain's pager like a table store
        // (put_table_resolved) so it joins the post-commit residency flip; a store loaded from a
        // file already attached it.
        if let Some(paging) = &self.store_paging {
            if !store.is_file_backed() {
                store.attach_paging(paging.clone());
            }
        }
        std::sync::Arc::make_mut(&mut self.index_stores).insert(name_key, store);
    }

    /// Iterate every table data store — the store-page reachability walk (format.rs `reachable_pages`,
    /// the within-session compaction basis) reads each store's tree root + column types.
    pub(crate) fn stores_iter(&self) -> impl Iterator<Item = &TableStore> {
        self.stores.values()
    }

    /// Iterate every secondary/unique index store (empty-payload trees, never spillable).
    pub(crate) fn index_stores_iter(&self) -> impl Iterator<Item = &TableStore> {
        self.index_stores.values()
    }

    /// The resident GiST R-tree of the named index (lowercased key), or `None` if the index is not
    /// GiST / not present (spec/design/gist.md §4.1). The planner descends it for a `&&`/`@>` bound.
    pub(crate) fn gist_tree(
        &self,
        name_key: &str,
    ) -> Option<&std::sync::Arc<crate::gist::GistTree>> {
        self.gist_trees.get(name_key)
    }

    /// Rebuild **every** GiST index's resident R-tree from its leaf-key store (spec/design/gist.md
    /// §3/§4.1). Called after any statement that may have changed a GiST index's leaf set (the
    /// mutating-statement hook), so the working snapshot always carries a fresh tree a subsequent
    /// read descends — and after publish, the committed snapshot does too. Each tree is built in
    /// **canonical** order (`build_from_leaf_keys`: `range_total_cmp`, ties by storage key), making
    /// it a pure function of the leaf SET — content-deterministic, cross-core identical, and
    /// identical to the on-disk persisted R-tree. Trees whose index has been dropped are removed.
    /// A whole-tree rewrite, the §4.1(b) commit-rewrite narrowing extended to in-memory writes; the
    /// O(rows)-per-mutation cost is unmetered structure maintenance on the (trusted) write path —
    /// the untrusted surface is SELECT-only and never triggers it (gist.md §9, CLAUDE.md §13).
    pub(crate) fn rebuild_gist_trees(&mut self) -> Result<()> {
        // Collect (index name key, opclass) for every GiST index, dropping the borrow on
        // `self.tables` before mutating `self.gist_trees`.
        let mut specs: Vec<(String, Vec<crate::gist::GistOpclass>)> = Vec::new();
        for table in self.tables.values() {
            for idx in &table.indexes {
                if idx.kind != IndexKind::Gist {
                    continue;
                }
                // One opclass per indexed column (gist.md §7): a single-column GX1/GX2 index has
                // one; an EXCLUDE backing index has one per `WITH` column.
                let ops: Vec<crate::gist::GistOpclass> = idx
                    .column_ordinals()
                    .expect("a GiST index is plain-column")
                    .iter()
                    .map(|&ci| crate::gist::opclass_for(&table.columns[ci].ty))
                    .collect();
                specs.push((idx.name.to_ascii_lowercase(), ops));
            }
        }
        let live: std::collections::HashSet<&str> = specs.iter().map(|(k, _)| k.as_str()).collect();
        // Disjoint field borrows: hold the mutable `gist_trees` while reading `self.index_stores`.
        let gist_trees = std::sync::Arc::make_mut(&mut self.gist_trees);
        gist_trees.retain(|k, _| live.contains(k.as_str()));
        for (name_key, ops) in &specs {
            let keys: Vec<Vec<u8>> = match self.index_stores.get(name_key) {
                Some(store) => store.iter_entries()?.into_iter().map(|(k, _)| k).collect(),
                None => Vec::new(),
            };
            let tree = crate::gist::build_from_leaf_keys(ops, keys.iter().map(|k| k.as_slice()))?;
            gist_trees.insert(name_key.clone(), std::sync::Arc::new(tree));
        }
        Ok(())
    }

    /// Remove one secondary index (DROP INDEX): its definition from the owning table and
    /// its store.
    pub(crate) fn remove_index(&mut self, table_key: &str, name_key: &str) {
        self.bump_cat_gen();
        if let Some(t) = std::sync::Arc::make_mut(&mut self.tables).get_mut(table_key) {
            t.indexes
                .retain(|i| i.name.to_ascii_lowercase() != name_key);
        }
        std::sync::Arc::make_mut(&mut self.index_stores).remove(name_key);
    }

    /// Find the table owning the named index (case-insensitive): `(table_key, &IndexDef)`.
    pub(crate) fn find_index(&self, name: &str) -> Option<(&str, &IndexDef)> {
        let key = name.to_ascii_lowercase();
        self.tables.iter().find_map(|(tk, t)| {
            t.indexes
                .iter()
                .find(|i| i.name.to_ascii_lowercase() == key)
                .map(|i| (tk.as_str(), i))
        })
    }

    /// Every table with its store, as `(lowercased key, table, store)` tuples, for the on-disk
    /// serializer (spec/fileformat/format.md). The serializer sorts by the lowercased key so
    /// hash-map iteration order never leaks (CLAUDE.md §8).
    /// Demote every store's clean, persisted resident leaves to `OnDisk` references — the
    /// post-commit residency flip over the whole snapshot (bplus-reshape.md B4), run after a
    /// successful persist so the published committed tree is the skeletal `interiors + OnDisk
    /// leaves` shape on every host. Table stores and btree/GIN index stores flip; a GiST leaf-key
    /// store's nodes are never persisted (its on-disk form is the R-tree), so it no-ops naturally.
    pub(crate) fn demote_clean_leaves(&mut self) {
        for store in std::sync::Arc::make_mut(&mut self.stores).values_mut() {
            store.demote_clean_leaves();
        }
        for store in std::sync::Arc::make_mut(&mut self.index_stores).values_mut() {
            store.demote_clean_leaves();
        }
    }

    pub(crate) fn catalog_and_stores(&self) -> Vec<(&str, &Table, &TableStore)> {
        self.tables
            .iter()
            .map(|(k, t)| (k.as_str(), t, self.stores.get(k).expect("store exists")))
            .collect()
    }
}
