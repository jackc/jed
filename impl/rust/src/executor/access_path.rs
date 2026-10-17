//! Index/key access-path execution — the Engine methods that turn a resolved access-plan bound into
//! rows: index_bound_rows and the point/range/keyset probe helpers over the PK and secondary indexes
//! (mirrors part of impl/go access_path.go). The filter-analysis that DETECTS a bound is in mod.rs.

use super::*;
use crate::gin_jsonpath::GinTree;

impl Engine {
    /// Execute an index equality bound (cost.md §3 "index-bounded scan"): fetch the rows the
    /// equality admits, in index-entry order (= storage-key order among equal values), and
    /// return them with the scan's up-front units `(pages, slabs)` — the index-tree nodes
    /// overlapping the prefix range plus, per admitted entry, the table-tree nodes of that
    /// row's point lookup and its touched-column decompress slabs. The caller feeds the rows
    /// through the same ScanSource as any bounded scan (page_read block + per-row
    /// storage_row_read). A provably empty bound (NULL / contradictory equalities /
    /// out-of-range) returns nothing and charges nothing.
    pub(crate) fn index_bound_rows(
        &self,
        table_name: &str,
        ib: &IndexBound,
        params: &[Value],
        outer: &[&[Value]],
        mask: &[bool],
        left: &[Value],
    ) -> Result<(Vec<Row>, (usize, usize))> {
        let (entries, units) =
            self.index_bound_entries(table_name, ib, params, outer, mask, left)?;
        Ok((entries.into_iter().map(|(_, row)| row).collect(), units))
    }

    /// Key-preserving form of [`Self::index_bound_rows`]. SELECT's wrapper drops the storage keys;
    /// mutation consumers keep them for phase-2 writes.
    pub(crate) fn index_bound_entries(
        &self,
        table_name: &str,
        ib: &IndexBound,
        params: &[Value],
        outer: &[&[Value]],
        mask: &[bool],
        left: &[Value],
    ) -> Result<(Vec<(Vec<u8>, Row)>, (usize, usize))> {
        let Some((bound, prefix_len)) = build_index_bound(ib, params, outer, left) else {
            return Ok((Vec::new(), (0, 0))); // provably empty — read nothing, charge nothing
        };
        self.index_scan_bound_entries(
            table_name,
            &ib.name_key,
            &ib.suffix,
            &bound,
            prefix_len,
            mask,
        )
    }

    /// Key-preserving core of the ordered-index gather. Candidate order and units match the existing
    /// SELECT contract; the already-recovered storage key is retained for mutation consumers.
    pub(crate) fn index_scan_bound_entries(
        &self,
        table_name: &str,
        name_key: &str,
        suffix: &[KeySlot],
        bound: &KeyBound,
        prefix_byte_len: usize,
        mask: &[bool],
    ) -> Result<(Vec<(Vec<u8>, Row)>, (usize, usize))> {
        index_store_bound_entries(
            self.index_store(name_key),
            self.store(table_name),
            suffix,
            bound,
            prefix_byte_len,
            mask,
        )
    }

    /// Execute canonical logical intervals over the row's own B-tree. Storage keys are retained for
    /// mutation consumers, and each disjoint interval's page/slab block is summed.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn pk_key_set_rows(
        &self,
        store: &TableStore,
        ks: &PkKeySet,
        params: &[Value],
        outer: &[&[Value]],
        mask: &[bool],
        left: &[Value],
        masked: bool,
    ) -> Result<(Vec<(Vec<u8>, Row)>, (usize, usize))> {
        let mut entries: Vec<(Vec<u8>, Row)> = Vec::new();
        let mut pages = 0usize;
        let mut slabs = 0usize;
        for b in ks.intervals(params, outer, left) {
            let (es, p, s) = if masked {
                store.range_scan_with_units(&b, mask)?
            } else {
                store.range_scan_with_units(&b, mask)?
            };
            entries.extend(es);
            pages += p;
            slabs += s;
        }
        Ok((entries, (pages, slabs)))
    }

    /// Probe the set's canonical intervals in the secondary index's key space. Each admitted index
    /// entry point-looks-up the table row in deterministic byte-key order.
    pub(crate) fn index_key_set_rows(
        &self,
        table_name: &str,
        ks: &IndexKeySet,
        params: &[Value],
        outer: &[&[Value]],
        mask: &[bool],
        left: &[Value],
    ) -> Result<(Vec<Row>, (usize, usize))> {
        let (entries, units) =
            self.index_key_set_entries(table_name, ks, params, outer, mask, left)?;
        Ok((entries.into_iter().map(|(_, row)| row).collect(), units))
    }

    pub(crate) fn index_key_set_entries(
        &self,
        table_name: &str,
        ks: &IndexKeySet,
        params: &[Value],
        outer: &[&[Value]],
        mask: &[bool],
        left: &[Value],
    ) -> Result<(Vec<(Vec<u8>, Row)>, (usize, usize))> {
        let mut rows: Vec<(Vec<u8>, Row)> = Vec::new();
        let mut pages = 0usize;
        let mut slabs = 0usize;
        for probe in ks.probes(params, outer, left) {
            let (r, (p, s)) = self.index_scan_bound_entries(
                table_name,
                &ks.name_key,
                &probe.suffix,
                &probe.bound,
                probe.prefix_len,
                mask,
            )?;
            rows.extend(r);
            pages += p;
            slabs += s;
        }
        Ok((rows, (pages, slabs)))
    }

    /// Execute a planned UPDATE/DELETE access path into the normalized keyed-row batch. This owns
    /// the access-method switch that used to be duplicated inline in both DML executors; per-row
    /// guards, residual evaluation, and phase-2 writes remain with the caller.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn execute_mutation_scan(
        &self,
        plan: &MutationScanPlan,
        table_name: &str,
        filter: Option<&RExpr>,
        params: &[Value],
        env: &EvalEnv,
        meter: &mut Meter,
        mask: &[bool],
    ) -> Result<MutationScanBatch> {
        // A WHERE contradiction (planner.md §3.1) reads no page and no row, and so charges nothing.
        if plan.contradiction {
            return Ok(MutationScanBatch {
                entries: Vec::new(),
                pages: 0,
                slabs: 0,
            });
        }
        let store = self.store_scoped(plan.db.as_deref(), table_name);
        let (entries, (pages, slabs)) = match plan.bound.as_ref() {
            None => {
                let (entries, pages, slabs) = store.scan_with_units(mask)?;
                (entries, (pages, slabs))
            }
            Some(ScanBound::Pk(bp)) => match build_key_bound(bp, params, &[], &[]) {
                Some(bound) => {
                    let (entries, pages, slabs) = store.range_scan_with_units(&bound, mask)?;
                    (entries, (pages, slabs))
                }
                None => (Vec::new(), (0, 0)),
            },
            Some(ScanBound::Index(ib)) => {
                self.index_bound_entries(table_name, ib, params, &[], mask, &[])?
            }
            Some(ScanBound::Gin(gb)) => {
                let query = filter.and_then(|f| gin_bound_query(f, gb));
                self.gin_bound_rows(table_name, gb, query, &[], env, meter, mask, false)?
            }
            Some(ScanBound::Gist(gb)) => {
                let query = filter.and_then(|f| gist_query_operand(f, gb));
                self.gist_bound_rows(table_name, gb, query, &[], env, meter, mask, false)?
            }
            Some(ScanBound::PkSet(ks)) => {
                self.pk_key_set_rows(store, ks, params, &[], mask, &[], false)?
            }
            Some(ScanBound::IndexSet(ks)) => {
                let (mut entries, units) =
                    self.index_key_set_entries(table_name, ks, params, &[], mask, &[])?;
                // Retain first-probe order while guaranteeing that phase 2 can never receive the
                // same row twice if a future index-key generalization makes probe sets overlap.
                let mut seen = HashSet::with_capacity(entries.len());
                entries.retain(|(key, _)| seen.insert(key.clone()));
                (entries, units)
            }
        };
        Ok(MutationScanBatch {
            entries,
            pages,
            slabs,
        })
    }

    /// Execute a GIN-bounded scan (spec/design/gin.md §6, cost.md §3). Evaluates the
    /// query operand, extracts its terms + mode via the `array_ops` opclass (an array for `@>`/`&&`;
    /// a single scalar term for `= ANY` — `Member`; the array's distinct non-NULL terms for `=` —
    /// `Equal`), gathers each term's posting list (a prefix range scan of the GIN entry tree),
    /// combines them by mode (`@>`, `= ANY`, and `=` → intersection, `&&` → union) into the
    /// candidate storage-key set, and point-looks-up each candidate in storage-key order. The
    /// original predicate stays the residual WHERE filter (re-applied downstream), so the result is
    /// always correct — the bound only narrows which rows are fetched. Returns the candidate rows +
    /// the scan's up-front units `(pages, slabs)` (entry-tree overlap nodes per term + each
    /// candidate's table point-lookup); `gin_entry` (per posting entry visited) is charged on
    /// `meter` directly. Degenerate constant queries (gin.md §6): a NULL `Q`, an `@>` whose `Q`
    /// holds a NULL element, an `&&` with no non-NULL term, and a NULL `= ANY` scalar are provably
    /// empty (read nothing); `@> '{}'` and array `=` with no non-NULL term fall back to the full scan.
    /// Gather a GIN-bounded scan's candidate rows as `(storage_key, row)` pairs (the candidate
    /// set *is* the storage keys), with the up-front `(page_read nodes, value_decompress slabs)`
    /// block. SELECT drops the keys; UPDATE/DELETE keep them to rewrite/remove the rows
    /// (gin.md §6). `gin_entry` is charged inside (during the gather); the caller charges the
    /// returned block.
    pub(crate) fn gin_bound_rows(
        &self,
        table_name: &str,
        gb: &GinBound,
        query: Option<&RExpr>,
        query_row: &[Value],
        env: &EvalEnv,
        meter: &mut Meter,
        mask: &[bool],
        keys_only: bool,
    ) -> Result<(Vec<(Vec<u8>, Row)>, (usize, usize))> {
        let store = self.store(table_name);
        // Extract the query's distinct terms. This (the opclass `extract_query_terms`) is a pure
        // planning step, NOT metered (cost.md §3) — evaluate `Q` on a scratch meter. `Q` is a
        // `query_row` is empty for a constant bound and the combined left row for a sibling INL.
        let Some(query) = query else {
            return Ok((Vec::new(), (0, 0)));
        };
        // A disjunctive bound's query is its `OR` node (gin.md §6.3): every arm's operand is
        // extracted, in source order, before any posting list is read.
        let arms: Vec<(GinStrategy, &RExpr)> = if gb.or_arms.is_empty() {
            vec![(gb.strategy, query)]
        } else {
            gin_or_arms(query)
                .into_iter()
                .map(|arm| {
                    gin_match(arm, gb.col_global, gb.opclass)
                        .expect("a disjunctive GIN bound's arms each match (plan-time detection)")
                })
                .collect()
        };
        let mut queries: Vec<GinTree> = Vec::with_capacity(arms.len());
        for (strategy, operand) in arms {
            let qv = operand.eval(query_row, env, &mut env.exec.session.scratch_meter())?;
            match gin_query_terms(gb.opclass, strategy, &qv) {
                GinQuery::Empty => {}
                // Rows the index cannot enumerate (they carry no terms) may match: scan them all and
                // let the residual filter keep the right ones (gin.md §6 / §2.1). One such arm makes
                // the whole disjunction fall back.
                GinQuery::FullScan => {
                    let (entries, pages, slabs) = store.scan_with_units(mask)?;
                    return Ok((entries, (pages, slabs)));
                }
                GinQuery::Terms(terms) => queries.push(
                    GinTree::from_terms(strategy.is_any(), terms)
                        .map_terms(&|term| gin_tagged_term(gb.tag, term)),
                ),
                GinQuery::Tree(tree) => {
                    queries.push(tree.map_terms(&|term| gin_tagged_term(gb.tag, term)))
                }
            }
        }

        // Gather each term's posting list: the entry range [term, successor) of the GIN tree
        // (gin.md §4). The entry is `term ‖ storage_key` and every term self-delimits — an
        // `array_ops` element key is fixed-width or terminated (text, §4.2), a `jsonb_ops` term is a
        // flag byte plus a fixed or terminated body (§4.1) — so the range holds exactly this term's entries
        // and the storage key is the suffix after the term's own length.
        let istore = self.index_store(&gb.name_key);
        let mut gather = GinGather::default();
        // The candidate storage keys, in ascending byte (= storage-key) order, so the point lookups
        // and the emitted rows follow storage order exactly as a full scan would (gin.md §6/§8).
        let candidates: Vec<Vec<u8>> = if queries.len() == 1 {
            gather.eval(istore, &queries[0])?
        } else {
            // A disjunction: each arm's candidates in source order, unioned (gin.md §6.3).
            let mut union = BTreeSet::new();
            for tree in &queries {
                union.extend(gather.eval(istore, tree)?);
            }
            union.into_iter().collect()
        };
        meter.charge(COSTS.gin_entry * gather.entries as i64);
        let mut pages = gather.pages;

        let mut slabs = 0usize;
        let mut rows = Vec::with_capacity(candidates.len());
        for key in candidates {
            if keys_only {
                rows.push((key, Vec::new()));
                continue;
            }
            let (row, n, s) = store.get_with_units(&key, mask)?;
            pages += n;
            slabs += s;
            rows.push((
                key,
                row.ok_or_else(|| {
                    crate::error::EngineError::data_corrupted("a GIN entry references a stored row")
                })?,
            ));
        }
        Ok((rows, (pages, slabs)))
    }

    /// Gather a GiST-bounded scan's candidate rows (spec/design/gist.md §5). Evaluates the
    /// query operand, then **descends the index's resident R-tree** visiting only children
    /// `consistent` with the query, collecting candidate storage keys at the leaves; each candidate
    /// row is point-looked-up in storage-key order. The original `&&`/`@>` predicate stays the
    /// residual WHERE filter (always-recheck, re-applied downstream), so the result is exactly the
    /// full-scan result — the bound only narrows which rows are fetched. Returns the candidate
    /// `(storage_key, row)` pairs + the up-front `(page_read, value_decompress)` block (tree nodes
    /// visited + each candidate's point-lookup); `gist_descent` (per interior node) is charged on
    /// `meter` directly here. Degenerate constant queries (gist.md §5): a NULL `Q` and an empty
    /// `&&` query match nothing (read nothing); an empty `@>` query (`col @> 'empty'`) matches every
    /// row and falls back to the full scan (the empty bound is invisible to the overlap-descend).
    pub(crate) fn gist_bound_rows(
        &self,
        table_name: &str,
        gb: &GistBound,
        query: Option<&RExpr>,
        query_row: &[Value],
        env: &EvalEnv,
        meter: &mut Meter,
        mask: &[bool],
        keys_only: bool,
    ) -> Result<(Vec<(Vec<u8>, Row)>, (usize, usize))> {
        use crate::gist::{GistQuery, GistStrategy};
        let store = self.store(table_name);
        // Extracting a constant or once-per-outer sibling query is a planning step, NOT metered
        // (cost.md §3), and uses a scratch meter.
        let qv = match query {
            Some(q) => q.eval(query_row, env, &mut env.exec.session.scratch_meter())?,
            None => return Ok((Vec::new(), (0, 0))),
        };
        // Form the resident-tree search query from the constant, handling the strategy-specific
        // degenerate cases. A NULL query is 3VL-unknown for every row → never TRUE (all strategies).
        let gquery = match gb.strategy {
            GistStrategy::Equal => {
                // scalar `=` (gist.md §6): encode the constant to its order-preserving key bytes.
                match qv {
                    Value::Null => return Ok((Vec::new(), (0, 0))),
                    v => {
                        let s = gb
                            .scalar_type
                            .expect("a scalar GiST bound carries its column scalar type");
                        let key = encode_key_value(s, &v, None)
                            .expect("a fixed-width GiST scalar key is infallible (no collation)");
                        GistQuery::Scalar(key)
                    }
                }
            }
            GistStrategy::Overlaps | GistStrategy::Contains => {
                let qrange = match qv {
                    Value::Range(rv) => rv,
                    Value::Null => return Ok((Vec::new(), (0, 0))),
                    _ => return Ok((Vec::new(), (0, 0))), // not a range (impossible post-resolve)
                };
                if qrange.empty {
                    return match gb.strategy {
                        // `col @> 'empty'` is TRUE for every row (the empty range is contained in
                        // every range), but an empty bound is absorbed by `range_merge`, so it is
                        // invisible to the overlap-descend (a false-negative trap, gist.md §5). Fall
                        // back to the full scan; the residual `@>` keeps every row.
                        GistStrategy::Contains => {
                            let (entries, pages, slabs) = store.scan_with_units(mask)?;
                            Ok((entries, (pages, slabs)))
                        }
                        // `col && 'empty'` overlaps nothing.
                        _ => Ok((Vec::new(), (0, 0))),
                    };
                }
                GistQuery::Range(qrange)
            }
        };
        // Descend the resident R-tree (rebuilt at each mutating statement, gist.md §3/§4.1), so the
        // gather visits only consistent nodes — no per-query build. An index with no tree yet (never
        // populated) yields no candidates. `page_read` per node touched + `gist_descent` per interior.
        let (mut skeys, nodes, interior) = match self.gist_tree(&gb.name_key) {
            Some(tree) => tree.search(std::slice::from_ref(&gquery), &[gb.strategy]),
            None => (Vec::new(), 0, 0),
        };
        meter.charge(COSTS.gist_descent * interior as i64);
        let mut pages = nodes;
        // Point-look-up each candidate in storage-key order (the candidates ARE storage keys), so
        // the lookups and emitted rows follow storage order exactly as a full scan would.
        skeys.sort_unstable();
        skeys.dedup();
        let mut slabs = 0usize;
        let mut rows = Vec::with_capacity(skeys.len());
        for key in skeys {
            if keys_only {
                rows.push((key, Vec::new()));
                continue;
            }
            let (row, n, s) = store.get_with_units(&key, mask)?;
            pages += n;
            slabs += s;
            rows.push((
                key,
                row.ok_or_else(|| {
                    crate::error::EngineError::data_corrupted(
                        "a GiST entry references a stored row",
                    )
                })?,
            ));
        }
        Ok((rows, (pages, slabs)))
    }
}

/// Store-explicit core of [`Engine::index_scan_bound_entries`]: gather the entries `istore` admits
/// within `bound` and point-look-up each one's row in `store`. A caller that must not resolve names
/// through the temp-first walk (the keyed referential-action gather, constraints.md §6.6) passes
/// the main snapshot's stores directly.
pub(crate) fn index_store_bound_entries(
    istore: &TableStore,
    store: &TableStore,
    suffix: &[KeySlot],
    bound: &KeyBound,
    prefix_byte_len: usize,
    mask: &[bool],
) -> Result<(Vec<(Vec<u8>, Row)>, (usize, usize))> {
    // The index store has no payload columns, so its mask is empty and its fused scan
    // contributes only the index-tree page_read count (no spill/compress units).
    let (entries, mut pages, _) = istore.range_scan_with_units(bound, &[])?;
    let mut slabs = 0usize;
    let mut rows = Vec::with_capacity(entries.len());
    for (ekey, _) in entries {
        // Skip the equality prefix by its known byte length, then each remaining key component by
        // its self-delimiting slot (indexes.md §5.1); the suffix after them is the row's storage
        // key (indexes.md §3). A corrupt entry yields an empty storage key (no row).
        let row_key = index_entry_row_key(&ekey, prefix_byte_len, suffix);
        let (row, n, s) = store.get_with_units(row_key, mask)?;
        pages += n;
        slabs += s;
        rows.push((
            row_key.to_vec(),
            row.ok_or_else(|| {
                crate::error::EngineError::data_corrupted("an index entry references a stored row")
            })?,
        ));
    }
    Ok((rows, (pages, slabs)))
}

/// The posting reads of one GIN gather and their units: `page_read` nodes and `gin_entry` entries.
#[derive(Default)]
struct GinGather {
    pages: usize,
    entries: usize,
}

impl GinGather {
    /// The posting range of `term`: every entry `term ‖ storage_key` (gin.md §4).
    fn posting_bound(term: &[u8]) -> KeyBound {
        KeyBound {
            lo: Some(term.to_vec()),
            lo_inc: true,
            hi: prefix_successor(term),
            hi_inc: false,
        }
    }

    /// Read `bound` of the entry tree, charging its overlap nodes and entries, and return the
    /// storage keys of its entries (the suffix after the term) in ascending order.
    fn read(&mut self, istore: &TableStore, bound: &KeyBound, term: &[u8]) -> Result<Vec<Vec<u8>>> {
        let (es, p, _) = istore.range_scan_with_units(bound, &[])?;
        self.pages += p;
        self.entries += es.len();
        Ok(es
            .into_iter()
            .map(|(ekey, _)| ekey[term.len()..].to_vec())
            .collect())
    }

    /// The candidates of a query tree (gin.md §2.2/§6), ascending. A term is its whole posting list.
    /// An `Or` unions its leaves' lists (in byte order), then its children's candidates (in order). An
    /// `And` intersects its leaves by the skip-scan (§6.2) — or, with no leaf, starts from its first
    /// child — then intersects each following child's candidates in order, stopping once none
    /// survives. Mode ALL is an `And` of terms and mode ANY an `Or`, so `= ANY` and `?` (one term) read
    /// that lone list, array `=` gathers the same superset as `@>` (the residual `=` makes it exact),
    /// and the `jsonb_ops` gathers are supersets the residual rechecks (§2.1).
    fn eval(&mut self, istore: &TableStore, tree: &GinTree) -> Result<Vec<Vec<u8>>> {
        match tree {
            GinTree::Term(term) => self.posting_list(istore, term),
            GinTree::Or(leaves, rest) => {
                let mut union = BTreeSet::new();
                for term in leaves {
                    union.extend(self.posting_list(istore, term)?);
                }
                for child in rest {
                    union.extend(self.eval(istore, child)?);
                }
                Ok(union.into_iter().collect())
            }
            GinTree::And(leaves, rest) => {
                let mut rest = rest.iter();
                let mut candidates = if leaves.is_empty() {
                    match rest.next() {
                        Some(first) => self.eval(istore, first)?,
                        None => return Ok(Vec::new()),
                    }
                } else {
                    self.intersect(istore, leaves.clone())?
                };
                for child in rest {
                    if candidates.is_empty() {
                        break;
                    }
                    let found: BTreeSet<Vec<u8>> = self.eval(istore, child)?.into_iter().collect();
                    candidates.retain(|key| found.contains(key));
                }
                Ok(candidates)
            }
        }
    }

    /// A term's whole posting list (a prefix range scan).
    fn posting_list(&mut self, istore: &TableStore, term: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.read(istore, &Self::posting_bound(term), term)
    }

    /// The ALL-mode intersection (gin.md §6.2). The terms are ordered by their posting range's
    /// skeleton overlap count (ties by bytes); the first list is read whole, and each following term
    /// is probed at the surviving candidates in leaf groups — runs of consecutive candidates whose
    /// probe keys fall in one leaf — each charging its root-to-leaf path once and one `gin_entry` per
    /// step of the cheaper leaf-local match (gin.md §6.2). Stops once no candidate survives. The spans and the grouping read only the
    /// resident skeleton and are unmetered.
    fn intersect(&mut self, istore: &TableStore, terms: Vec<Vec<u8>>) -> Result<Vec<Vec<u8>>> {
        let mut ordered: Vec<(usize, Vec<u8>)> = terms
            .into_iter()
            .map(|t| (istore.overlap_node_count(&Self::posting_bound(&t)), t))
            .collect();
        ordered.sort();
        let mut ordered = ordered.into_iter().map(|(_, t)| t);
        let Some(driver) = ordered.next() else {
            return Ok(Vec::new());
        };
        let mut candidates = self.posting_list(istore, &driver)?;
        let height = istore.height();
        let probe = |term: &[u8], key: &[u8]| [term, key].concat();
        let span = |lo: Vec<u8>, hi: Vec<u8>| KeyBound {
            lo: Some(lo),
            lo_inc: true,
            hi: Some(hi),
            hi_inc: true,
        };
        for term in ordered {
            if candidates.is_empty() {
                break;
            }
            let mut survivors = Vec::new();
            let mut i = 0;
            while i < candidates.len() {
                let first = probe(&term, &candidates[i]);
                let mut j = i;
                while j + 1 < candidates.len()
                    && istore
                        .overlap_node_count(&span(first.clone(), probe(&term, &candidates[j + 1])))
                        == height
                {
                    j += 1;
                }
                // One leaf: its root-to-leaf path once, then the cheaper leaf-local match — a probe
                // per candidate, or a merge with the term's entries between the first and last
                // candidate — one `gin_entry` per step.
                let (found, path, _) = istore
                    .range_scan_with_units(&span(first, probe(&term, &candidates[j])), &[])?;
                self.pages += path;
                self.entries += (j + 1 - i).min(found.len());
                // Both lists ascend, so a merge keeps the candidates whose entry was found.
                let mut f = found.iter().map(|(k, _)| &k[term.len()..]).peekable();
                for c in &candidates[i..=j] {
                    while f.next_if(|k| *k < c.as_slice()).is_some() {}
                    if f.next_if(|k| *k == c.as_slice()).is_some() {
                        survivors.push(c.clone());
                    }
                }
                i = j + 1;
            }
            candidates = survivors;
        }
        Ok(candidates)
    }
}

/// The outcome of a GIN opclass's `extract_query_terms` (gin.md §2/§6): the distinct query terms to
/// gather (sorted), a provably-empty bound, or the full-scan fallback for a query whose matches the
/// index cannot enumerate.
pub(crate) enum GinQuery {
    Terms(Vec<Vec<u8>>),
    /// A jsonpath strategy's AND/OR term tree (gin.md §2.2).
    Tree(GinTree),
    Empty,
    FullScan,
}

/// A GIN query value's terms under its operator class and strategy — what the gather reads, and
/// what the planner prices from GIN term statistics (statistics.md §6.1).
pub(crate) fn gin_query_terms(opclass: GinOpclass, strategy: GinStrategy, qv: &Value) -> GinQuery {
    match opclass {
        GinOpclass::Array(elem) => array_query_terms(strategy, elem, qv),
        GinOpclass::Jsonb => jsonb_query_terms(strategy, qv),
    }
}

/// `array_ops` query terms (gin.md §2/§6). Each term is the element's order-preserving key encoding
/// (§4) — the SAME bytes the entries carry, so a term doubles as its posting-list prefix. Encoding
/// now (vs. later) lets us dedup distinct terms by bytes (the encoding is a bijection: byte-dedup ==
/// value-dedup, byte-sort == value-sort) generically over every admitted element type.
fn array_query_terms(strategy: GinStrategy, elem: ScalarType, qv: &Value) -> GinQuery {
    let mut terms: Vec<Vec<u8>> = Vec::new();
    let mut has_null = false;
    let mut is_empty = false;
    if strategy == GinStrategy::Member {
        // `c = ANY(col)`: the query operand is a SCALAR, not an array. A NULL `c` can equal no
        // element, so the bound is provably empty (gin.md §6). `c` may be a different numeric
        // type than the elements (an integer against `decimal[]`, a decimal against `i32[]`);
        // gin_query_term converts it to the element type or reports that no element can equal it.
        match qv {
            Value::Null => return GinQuery::Empty,
            v => match gin_query_term(elem, v) {
                Some(term) => terms.push(term),
                None => return GinQuery::Empty,
            },
        }
    } else {
        let arr = match qv {
            // A NULL whole-array query is 3VL-NULL for every row → never TRUE (both @> and &&).
            Value::Array(a) => a,
            _ => return GinQuery::Empty, // NULL (or not an array — impossible post-resolve)
        };
        // A query element no stored element can equal (an `f64` constant not exactly representable
        // as `f32` against an `f32[]` index — gin_query_term). It reaches no posting list, so `@>`
        // and array `=` (which need EVERY query element present) are provably empty; `&&` skips it.
        let mut unmatchable = false;
        for el in &arr.elements {
            match el {
                Value::Null => has_null = true, // a NULL element carries no term
                v => match gin_query_term(elem, v) {
                    Some(term) => terms.push(term),
                    None => unmatchable = true,
                },
            }
        }
        is_empty = arr.elements.is_empty();
        if unmatchable && strategy != GinStrategy::Overlaps {
            return GinQuery::Empty;
        }
    }
    terms.sort_unstable();
    terms.dedup();
    match strategy {
        // `@> '{}'`: every non-NULL array contains the empty array — not derivable from the
        // index (which knows only rows that HAVE terms), so fall back to the full scan.
        GinStrategy::Contains if is_empty => GinQuery::FullScan,
        // `@>` a query containing a NULL element is never TRUE (strict element equality).
        GinStrategy::Contains if has_null => GinQuery::Empty,
        // `col = Q` with NO non-NULL term — `'{}'` or an all-NULL `Q`. The rows it matches (`{}`,
        // `{NULL}`, …) carry NO index terms, so the index cannot enumerate them: fall back to the
        // full scan (gin.md §6). NOT a provably-empty bound — and a `Q` with ≥1 non-NULL element is
        // NOT caught here (it gathers, even when it also has a NULL element).
        GinStrategy::Equal if terms.is_empty() => GinQuery::FullScan,
        // `&&` with no non-NULL term (empty or all-NULL `Q`) overlaps nothing.
        GinStrategy::Overlaps if terms.is_empty() => GinQuery::Empty,
        _ => GinQuery::Terms(terms),
    }
}

/// `jsonb_ops` query terms (gin.md §2.1). `@>` gathers the query document's own index terms (ALL);
/// `?` the one key term; `?|` / `?&` the key terms of the array's non-NULL elements (ANY / ALL). A
/// NULL operand is provably empty; a term-less `@>` query and a key-less `?&` fall back to the full
/// scan (`{}` is contained by every object, `?& '{}'` holds for every document); a key-less `?|`
/// matches nothing.
fn jsonb_query_terms(strategy: GinStrategy, qv: &Value) -> GinQuery {
    let terms = match (strategy, qv) {
        (_, Value::Null) => return GinQuery::Empty,
        // `@?` / `@@` (gin.md §2.2): the tree PostgreSQL extracts from the path, or the full scan
        // when nothing is extractable. The path text is a jsonpath value's canonical text, already
        // compiled at resolve; a term never depends on the environment's casing table.
        (GinStrategy::JsonPathExists | GinStrategy::JsonPathMatch, Value::JsonPath(text)) => {
            let Ok(path) = crate::jsonpath::JsonPath::compile(text, None) else {
                return GinQuery::FullScan;
            };
            return match crate::gin_jsonpath::jsonpath_gin_tree(
                &path,
                strategy == GinStrategy::JsonPathExists,
            ) {
                Some(tree) => GinQuery::Tree(tree),
                None => GinQuery::FullScan,
            };
        }
        (GinStrategy::JsonContains, Value::Jsonb(doc)) => {
            let terms = crate::json::gin_jsonb_index_terms(doc);
            if terms.is_empty() {
                return GinQuery::FullScan;
            }
            terms
        }
        (GinStrategy::JsonHasKey, Value::Text(k)) => vec![crate::json::gin_jsonb_key_term(k)],
        (GinStrategy::JsonHasAnyKey | GinStrategy::JsonHasAllKeys, Value::Array(arr)) => {
            let mut terms: Vec<Vec<u8>> = arr
                .elements
                .iter()
                .filter_map(|e| match e {
                    Value::Text(k) => Some(crate::json::gin_jsonb_key_term(k)),
                    _ => None, // a NULL key is skipped (json-sql-functions.md §1.2)
                })
                .collect();
            terms.sort_unstable();
            terms.dedup();
            if terms.is_empty() {
                return if strategy == GinStrategy::JsonHasAllKeys {
                    GinQuery::FullScan
                } else {
                    GinQuery::Empty
                };
            }
            terms
        }
        _ => unreachable!("a jsonb_ops strategy's operand type is fixed by resolution"),
    };
    GinQuery::Terms(terms)
}

/// The GIN term for one non-NULL query value against an index whose elements are `elem` — the
/// element's order-preserving key encoding (gin.md §4), the same bytes the entries carry — or
/// `None` when the value can equal NO stored element. A float query value may be the OTHER float
/// width (`f64 = ANY(f32[])` compares by promoting the `f32` elements, float.md §2), so it is first
/// converted to the element width: an `f32` value widens exactly; an `f64` value narrows only when
/// it is exactly an `f32` (or NaN — every NaN is one value, float.md §3), else no `f32` element can
/// equal it. The float key canonicalizes `-0 → +0` and every NaN (encoding.md §2.8), so term
/// equality is exactly SQL float `=`. Integer and decimal compare by value (`2 = 2.0`), so a
/// value of one against elements of the other is converted too (gin.md §2 *Numeric terms*): an
/// integer becomes a decimal exactly; a decimal becomes an integer only when it is integral and in
/// the element's range, else no element can equal it. An integer outside a narrower element's range
/// equals no element either. Every other value takes the shared element term ([`gin_element_term`]),
/// which never collates or fails.
fn gin_query_term(elem: ScalarType, v: &Value) -> Option<Vec<u8>> {
    match (elem, v) {
        (ScalarType::Float32, Value::Float64(x)) => {
            let narrowed = *x as f32;
            (x.is_nan() || f64::from(narrowed) == *x).then(|| encode_f32_key(narrowed))
        }
        (ScalarType::Float64, Value::Float32(x)) => Some(encode_f64_key(f64::from(*x))),
        (ScalarType::Decimal, Value::Int(n)) => Some(gin_element_term(
            elem,
            &Value::Decimal(Decimal::from_i64(*n)),
        )),
        (_, Value::Decimal(d)) if elem.is_integer() => {
            let integral = d.trunc_to_scale(0).cmp_value(d) == std::cmp::Ordering::Equal;
            let n = d.to_i64_round().filter(|_| integral)?;
            (n >= elem.min() && n <= elem.max()).then(|| gin_element_term(elem, &Value::Int(n)))
        }
        (_, Value::Int(n)) if elem.is_integer() && (*n < elem.min() || *n > elem.max()) => None,
        _ => Some(gin_element_term(elem, v)),
    }
}
