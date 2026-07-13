//! Index/key access-path execution — the Engine methods that turn a resolved access-plan bound into
//! rows: index_bound_rows and the point/range/keyset probe helpers over the PK and secondary indexes
//! (mirrors part of impl/go access_path.go). The filter-analysis that DETECTS a bound is in mod.rs.

use super::*;

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
            &ib.suffix_types,
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
        suffix_types: &[ScalarType],
        bound: &KeyBound,
        prefix_byte_len: usize,
        mask: &[bool],
    ) -> Result<(Vec<(Vec<u8>, Row)>, (usize, usize))> {
        let istore = self.index_store(name_key);
        // The index store has no payload columns, so its mask is empty and its fused scan
        // contributes only the index-tree page_read count (no spill/compress units).
        let (entries, mut pages, _) = istore.range_scan_with_units(bound, &[])?;
        let store = self.store(table_name);
        let mut slabs = 0usize;
        let mut rows = Vec::with_capacity(entries.len());
        for (ekey, _) in entries {
            // Skip the equality prefix by its known byte length, then each remaining key component by
            // width (self-delimiting — a 0x01 NULL tag alone, or 0x00 + the fixed width,
            // indexes.md §5.1); the suffix after them is the row's storage key (indexes.md §3).
            let mut at = prefix_byte_len;
            for &ty in suffix_types {
                at += match ekey.get(at) {
                    Some(0x01) => 1,
                    _ => 1 + ty.width_bytes(),
                };
            }
            let row_key = &ekey[at..];
            let (row, n, s) = store.get_with_units(row_key, mask)?;
            pages += n;
            slabs += s;
            rows.push((
                row_key.to_vec(),
                row.expect("an index entry references a stored row"),
            ));
        }
        Ok((rows, (pages, slabs)))
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
        for b in canonical_interval_set(
            ks.pk_type,
            &ks.specs,
            &ks.clip,
            params,
            outer,
            ks.coll.as_deref(),
            left,
        ) {
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

    /// Map canonical logical intervals into the secondary index's present-value key space. Each
    /// admitted index entry point-looks-up the table row in deterministic byte-key order.
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
        for logical in canonical_interval_set(
            ks.col_type,
            &ks.specs,
            &ks.clip,
            params,
            outer,
            ks.coll.as_deref(),
            left,
        ) {
            let physical = index_logical_interval(&logical);
            let point = logical.lo.is_some()
                && logical.lo == logical.hi
                && logical.lo_inc
                && logical.hi_inc;
            let mut suffix_types = ks.tail_types.clone();
            let prefix_len = if point {
                1 + logical.lo.as_ref().unwrap().len()
            } else {
                suffix_types.insert(0, ks.col_type);
                0
            };
            let (r, (p, s)) = self.index_scan_bound_entries(
                table_name,
                &ks.name_key,
                &suffix_types,
                &physical,
                prefix_len,
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
                let query = filter.and_then(|f| gin_match(f, gb.col_global).map(|(_, q)| q));
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
        let qv = match query {
            Some(q) => q.eval(query_row, env, &mut Meter::new())?,
            None => return Ok((Vec::new(), (0, 0))),
        };
        // Each term is the element's order-preserving key encoding (gin.md §4) — the SAME bytes the
        // entries carry, so a term doubles as its posting-list prefix below. Encoding now (vs. later)
        // lets us dedup distinct terms by bytes (the encoding is a bijection: byte-dedup ==
        // value-dedup, byte-sort == value-sort) generically over every admitted element type.
        let mut terms: Vec<Vec<u8>> = Vec::new();
        let mut has_null = false;
        let mut is_empty = false;
        if gb.strategy == GinStrategy::Member {
            // `c = ANY(col)`: the query operand is a SCALAR, not an array. A NULL `c` can equal no
            // element, so the bound is provably empty (gin.md §6). `c` is in the element type's
            // domain by resolution (jed coerces `c` to the element type, rejecting an out-of-range
            // integer constant 22003 before exec); the integer range check is a defensive guard
            // against silently truncating an out-of-range value into a wrong term.
            // A GIN element is fixed-width (no text), so the term encoding never collates / fails.
            let gin_term = |ty: ScalarType, v: &Value| -> Vec<u8> {
                encode_key_value(ty, v, None)
                    .expect("a GIN element key is infallible (fixed-width, no collation)")
            };
            match &qv {
                Value::Null => return Ok((Vec::new(), (0, 0))),
                Value::Int(n) if *n >= gb.elem_type.min() && *n <= gb.elem_type.max() => {
                    terms.push(gin_term(gb.elem_type, &qv))
                }
                Value::Int(_) => return Ok((Vec::new(), (0, 0))), // out-of-range guard
                v => terms.push(gin_term(gb.elem_type, v)),
            }
        } else {
            let gin_term = |ty: ScalarType, v: &Value| -> Vec<u8> {
                encode_key_value(ty, v, None)
                    .expect("a GIN element key is infallible (fixed-width, no collation)")
            };
            let arr = match &qv {
                // A NULL whole-array query is 3VL-NULL for every row → never TRUE (both @> and &&).
                Value::Null => return Ok((Vec::new(), (0, 0))),
                Value::Array(a) => a,
                _ => return Ok((Vec::new(), (0, 0))), // not an array (impossible post-resolve)
            };
            for el in &arr.elements {
                match el {
                    Value::Null => has_null = true, // a NULL element carries no term
                    v => terms.push(gin_term(gb.elem_type, v)),
                }
            }
            is_empty = arr.elements.is_empty();
        }
        terms.sort_unstable();
        terms.dedup();

        match gb.strategy {
            // `@> '{}'`: every non-NULL array contains the empty array — not derivable from the
            // index (which knows only rows that HAVE terms), so fall back to the full scan. The
            // residual filter then keeps the right rows (gin.md §6).
            GinStrategy::Contains if is_empty => {
                let (entries, pages, slabs) = store.scan_with_units(mask)?;
                return Ok((entries, (pages, slabs)));
            }
            // `@>` a query containing a NULL element is never TRUE (strict element equality).
            GinStrategy::Contains if has_null => return Ok((Vec::new(), (0, 0))),
            // `col = Q` with NO non-NULL term — `'{}'` (`is_empty`) or an all-NULL `Q` (`has_null`,
            // no non-NULL element). The rows it matches (`{}`, `{NULL}`, …) carry NO index terms,
            // so the index cannot enumerate them: fall back to the full scan and let the residual
            // `=` keep them (gin.md §6). NOT a provably-empty bound — and a `Q` with ≥1 non-NULL
            // element is NOT caught here (it gathers, even when it also has a NULL element).
            GinStrategy::Equal if terms.is_empty() => {
                let (entries, pages, slabs) = store.scan_with_units(mask)?;
                return Ok((entries, (pages, slabs)));
            }
            // `&&` with no non-NULL term (empty or all-NULL `Q`) overlaps nothing.
            GinStrategy::Overlaps if terms.is_empty() => return Ok((Vec::new(), (0, 0))),
            _ => {}
        }

        // Gather each term's posting list: the entry range [encode(term), successor) of the GIN
        // tree (gin.md §4). The entry is `encode_element(term) ‖ storage_key`; the element type is
        // fixed-width, so the storage key is the suffix after `term_width` bytes.
        let istore = self.index_store(&gb.name_key);
        let term_width = gb.elem_type.width_bytes();
        let mut pages = 0usize;
        let mut entries_visited = 0usize;
        let mut postings: Vec<Vec<Vec<u8>>> = Vec::with_capacity(terms.len());
        for prefix in &terms {
            let bound = KeyBound {
                lo: Some(prefix.clone()),
                lo_inc: true,
                hi: prefix_successor(prefix),
                hi_inc: false,
            };
            let (es, p, _) = istore.range_scan_with_units(&bound, &[])?;
            pages += p;
            entries_visited += es.len();
            postings.push(
                es.into_iter()
                    .map(|(ekey, _)| ekey[term_width..].to_vec())
                    .collect(),
            );
        }
        meter.charge(COSTS.gin_entry * entries_visited as i64);

        // Combine the posting sets by mode into the candidate storage keys, in ascending byte
        // (= storage-key) order, so the point lookups and the emitted rows follow storage order
        // exactly as a full scan would (gin.md §6/§8).
        let candidates: BTreeSet<Vec<u8>> = match gb.strategy {
            // `@>` ALL → intersection; `= ANY` (Member) is a single term, so its intersection is
            // that lone posting list; array `=` (Equal) gathers the same superset as `@>` over `Q`'s
            // distinct non-NULL terms (the residual `=` makes it exact downstream) — gin.md §6.
            GinStrategy::Contains | GinStrategy::Member | GinStrategy::Equal => {
                let mut it = postings.into_iter();
                let mut acc: BTreeSet<Vec<u8>> =
                    it.next().unwrap_or_default().into_iter().collect();
                for list in it {
                    let s: BTreeSet<Vec<u8>> = list.into_iter().collect();
                    acc.retain(|k| s.contains(k));
                }
                acc
            }
            GinStrategy::Overlaps => postings.into_iter().flatten().collect(),
        };

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
            rows.push((key, row.expect("a GIN entry references a stored row")));
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
            Some(q) => q.eval(query_row, env, &mut Meter::new())?,
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
            rows.push((key, row.expect("a GiST entry references a stored row")));
        }
        Ok((rows, (pages, slabs)))
    }
}
