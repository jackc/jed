package jed

// Blocking operators retain the eager executor's logical phases while moving
// their input, keyed state and output through bounded scratch owners. Existing
// gather/window operators remain separate owners; spilling their downstream
// aggregate or join state does not claim a whole-query memory limit.

import (
	"bytes"
	"errors"
)

func (db *engine) boundedBlockingEligible(p *selectPlan) bool {
	if db.spillDir == "" || db.session.workMem == 0 {
		return false
	}
	if len(p.rels) == 0 && !p.isAgg && !p.distinct {
		return false
	}
	if streamingScanEligible(p) {
		return false
	}
	if len(p.rels) == 1 && !p.isAgg && !p.distinct {
		return false
	}
	// A join takes the bounded lane only when it hashes or is a multi-step physical join — the same
	// predicate as the Rust core's blocking_spill_eligible. The lane choice must be mirrored across
	// cores because a lane determines which rows are query-memory row buffers (memory.md §5.4).
	return p.isAgg || p.distinct || p.phys.hashJoin != nil || len(p.phys.joinSteps) > 0
}

func (db *engine) scanBlockingRel(p *selectPlan, i int, env *evalEnv, m *costMeter, out *rowSpool) error {
	r := p.rels[i]
	if r.srf != nil || r.cte != nil || r.derived != nil || p.phys.relBounds[i].needsEagerScan() {
		rows, err := db.materializeRel(p, i, env.params, env.outer, nil, env.rng, env.ctes, m)
		if err != nil {
			return err
		}
		// Spool residency is operator state bounded by work_mem (memory.md §4 Q2): the rows leave the
		// query-memory row account as they enter the spool.
		releaseRowsMasked(m, rows, p.relMasks[i])
		for _, row := range rows {
			if err := out.push(row); err != nil {
				return err
			}
		}
		return nil
	}
	store := db.lkpStoreScoped(r.db, r.tableName)
	b := unboundedBound()
	empty := false
	if sb := p.phys.relBounds[i]; sb != nil && sb.pk != nil {
		b, empty = db.buildKeyBound(sb.pk, env.params, env.outer, nil)
	}
	if empty {
		return nil
	}
	pages, slabs, err := store.OverlapScanUnits(b, p.relMasks[i])
	if err != nil {
		return err
	}
	m.Charge(costs.PageRead*int64(pages) + costs.ValueDecompress*int64(slabs))
	return store.ScanRange(b, func(_ []byte, row storedRow) (bool, error) {
		if err := m.Guard(); err != nil {
			return false, err
		}
		m.Charge(costs.StorageRowRead)
		row, err := store.resolveColumns(row, p.relMasks[i])
		if err != nil {
			return false, err
		}
		// Untouched lazy values must not pin source-page payloads in the spool.
		return true, out.push(topKPruneUntouched(row, p.relMasks[i]))
	})
}

type boundedJoinTable struct {
	mem          map[uint64][]hashJoinEntry
	disk         *diskBuckets
	budget, used int
	dir          string
}

func (t *boundedJoinTable) close() {
	if t.disk != nil {
		t.disk.close()
	}
	t.mem = nil
}

func (t *boundedJoinTable) add(key []byte, row storedRow) error {
	h := hashJoinFNV1a(key)
	if t.disk == nil {
		t.mem[h] = append(t.mem[h], hashJoinEntry{key: key, row: row})
		t.used += len(key) + rowBytes(row) + 32
		if t.used <= t.budget {
			return nil
		}
		var err error
		t.disk, err = newDiskBuckets(t.dir)
		if err != nil {
			return err
		}
		for hash, bucket := range t.mem {
			for _, entry := range bucket {
				if err = t.disk.append(hash, entry.key, entry.row); err != nil {
					return err
				}
			}
		}
		t.mem = nil
		t.used = 0
		return nil
	}
	return t.disk.append(h, key, row)
}

func (t *boundedJoinTable) each(key []byte, fn func([]byte, storedRow) error) error {
	h := hashJoinFNV1a(key)
	if t.disk != nil {
		return t.disk.bucket(h, fn)
	}
	for _, e := range t.mem[h] {
		if err := fn(e.key, e.row); err != nil {
			return err
		}
	}
	return nil
}

func joinKeyParts(p *hashJoinPlan, offset int, build bool) ([]int, []dataType) {
	indices := make([]int, len(p.keys))
	types := make([]dataType, len(p.keys))
	for i, k := range p.keys {
		indices[i] = k.left - offset
		if build {
			indices[i] = k.right - offset
		}
		types[i] = k.ty
	}
	return indices, types
}

func (db *engine) boundedJoin(p *selectPlan, rels []*rowSpool, out *rowSpool, env *evalEnv, m *costMeter) error {
	outer, inner := physicalRelOrdinal(p, 0), physicalRelOrdinal(p, 1)
	table := &boundedJoinTable{mem: make(map[uint64][]hashJoinEntry), budget: db.session.workMem, dir: db.spillDir}
	defer table.close()
	indices, types := joinKeyParts(p.phys.hashJoin, p.rels[inner].offset, true)
	if err := rels[inner].each(func(row storedRow) error {
		key, present, err := hashJoinRowKey(row, indices, types, costs.HashBuild, m)
		if err != nil || !present {
			return err
		}
		return table.add(key, row)
	}); err != nil {
		return err
	}
	indices, types = joinKeyParts(p.phys.hashJoin, p.rels[outer].offset, false)
	return rels[outer].each(func(left storedRow) error {
		key, present, err := hashJoinRowKey(left, indices, types, costs.HashProbe, m)
		if err != nil {
			return err
		}
		matched := false
		if present {
			// The original hash probe charges every collision check before evaluating ON.
			// Two bounded passes preserve that visitation without a resident match vector.
			if err := table.each(key, func(k []byte, _ storedRow) error {
				if err := m.Guard(); err != nil {
					return err
				}
				n := min(len(k), len(key))
				m.Charge(costs.HashProbe * int64(max(1, n)))
				return nil
			}); err != nil {
				return err
			}
			if err := table.each(key, func(k []byte, right storedRow) error {
				if !bytes.Equal(k, key) {
					return nil
				}
				row := combinePhysicalRelationRows(p, outer, left, inner, right)
				if on := p.joins[0].on; on != nil {
					v, err := on.eval(row, env, m)
					if err != nil {
						return err
					}
					if !v.IsTrue() {
						return nil
					}
				}
				matched = true
				return out.push(row)
			}); err != nil {
				return err
			}
		}
		if !matched && p.joins[0].kind == joinLeft {
			return out.push(placePhysicalRelationRow(p, outer, left))
		}
		return nil
	})
}

// Accumulator state is a private row-codec payload; it preserves the running
// decimal scale and the float special flags, never finalizing/recombining partials.
func blockingAccRow(a *acc) storedRow {
	r := storedRow{IntValue(a.count), IntValue(a.sumInt), DecimalValue(a.sumDec), BoolValue(a.seen), a.cur, BoolValue(a.hasCur)}
	if a.floatSum != nil {
		f := a.floatSum
		r = append(r, BoolValue(f.is32), BoolValue(f.sawNaN), BoolValue(f.sawPosInf), BoolValue(f.sawNegInf), Float64Value(f.total), IntValue(f.count))
	}
	return r
}

func blockingAccFromRow(spec aggSpec, r storedRow) *acc {
	a := newAccFromSpec(spec)
	a.count = r[0].Int
	a.sumInt = r[1].Int
	a.sumDec = *r[2].decimal()
	a.seen = r[3].boolVal()
	a.cur = r[4]
	a.hasCur = r[5].boolVal()
	if a.floatSum != nil {
		a.floatSum = &floatSumAcc{is32: r[6].boolVal(), sawNaN: r[7].boolVal(), sawPosInf: r[8].boolVal(), sawNegInf: r[9].boolVal(), total: r[10].F64(), count: r[11].Int}
	}
	return a
}

func (db *engine) boundedAggregate(p *selectPlan, input, output *rowSpool, env *evalEnv, m *costMeter) error {
	for gsi := range p.groupSets {
		gs := &p.groupSets[gsi]
		groups := newBoundedMap(db)
		first := newRowSpool(db)
		seen := newBoundedMap(db)
		values := &boundedJoinTable{mem: make(map[uint64][]hashJoinEntry), budget: db.session.workMem, dir: db.spillDir}
		err := func() error {
			defer groups.close()
			defer first.close()
			defer seen.close()
			defer values.close()
			ordinal := int64(0)
			create := func(keys storedRow) (storedRow, error) {
				r := storedRow{IntValue(ordinal), CompositeValue(keys)}
				ordinal++
				for _, spec := range p.aggSpecs {
					r = append(r, CompositeValue(blockingAccRow(newAccFromSpec(spec))))
				}
				if err := first.push(keys); err != nil {
					return nil, err
				}
				return r, nil
			}
			if len(gs.keyCols) == 0 {
				r, err := create(nil)
				if err != nil {
					return err
				}
				if err = groups.put("", r); err != nil {
					return err
				}
			}
			if err := input.each(func(row storedRow) error {
				if err := m.Guard(); err != nil {
					return err
				}
				keys := make(storedRow, len(gs.keyCols))
				for i, c := range gs.keyCols {
					keys[i] = row[c]
				}
				key := distinctRowKey(keys)
				state, ok, err := groups.get(key)
				if err != nil {
					return err
				}
				if !ok {
					state, err = create(keys)
					if err != nil {
						return err
					}
				}
				state = append(storedRow(nil), state...)
				for i, spec := range p.aggSpecs {
					if spec.filter != nil {
						v, err := spec.filter.eval(row, env, m)
						if err != nil {
							return err
						}
						if !v.IsTrue() {
							continue
						}
					}
					m.Charge(costs.AggregateAccumulate)
					collectionKey := []byte(distinctRowKey([]Value{state[0], IntValue(int64(i))}))
					if spec.hypo != nil {
						tuple := make(storedRow, len(spec.hypo.keys))
						for j, e := range spec.hypo.keys {
							v, err := e.eval(row, env, m)
							if err != nil {
								return err
							}
							tuple[j] = v
						}
						a := blockingAccFromRow(spec, *state[i+2].composite())
						a.count++
						state[i+2] = CompositeValue(blockingAccRow(a))
						if err := values.add(collectionKey, tuple); err != nil {
							return err
						}
						continue
					}
					v := NullValue()
					if spec.operand != nil {
						v, err = spec.operand.eval(row, env, m)
						if err != nil {
							return err
						}
					}
					if spec.distinct {
						if v.IsNull() {
							continue
						}
						dk := distinctRowKey([]Value{state[0], IntValue(int64(i)), v})
						_, had, err := seen.get(dk)
						if err != nil {
							return err
						}
						if had {
							continue
						}
						if err = seen.put(dk, nil); err != nil {
							return err
						}
					}
					a := blockingAccFromRow(spec, *state[i+2].composite())
					handled, err := blockingFoldCollection(a, spec, v, collectionKey, values, seen, m)
					if err != nil {
						return err
					}
					if !handled {
						if err = a.fold(v, m); err != nil {
							return err
						}
					}
					state[i+2] = CompositeValue(blockingAccRow(a))
				}
				return groups.put(key, state)
			}); err != nil {
				return err
			}
			return first.each(func(keys storedRow) error {
				state, _, err := groups.get(distinctRowKey(keys))
				if err != nil {
					return err
				}
				row := make(storedRow, 0, len(p.groupKeys)+len(p.aggSpecs)+len(p.groupingSpecs))
				for _, src := range gs.slotSrc {
					if src < 0 {
						row = append(row, NullValue())
					} else {
						row = append(row, keys[src])
					}
				}
				for i, spec := range p.aggSpecs {
					a := blockingAccFromRow(spec, *state[i+2].composite())
					if spec.osaFrac != nil {
						v, err := spec.osaFrac.eval(row, env, env.exec.session.scratchMeter())
						if err != nil {
							return err
						}
						a.osaFrac = &v
					}
					collectionKey := []byte(distinctRowKey([]Value{state[0], IntValue(int64(i))}))
					v, err := db.blockingFinalizeCollection(a, spec, collectionKey, values, seen, row, env)
					if err != nil {
						return err
					}
					row = append(row, v)
				}
				for _, positions := range p.groupingSpecs {
					row = append(row, IntValue(groupingValue(positions, gs.mask)))
				}
				return output.push(row)
			})
		}()
		if err != nil {
			return err
		}
	}
	return nil
}

func (db *engine) execBoundedBlocking(p *selectPlan, env *evalEnv, m *costMeter) (emitter, error) {
	if len(p.rels) >= 3 && (len(p.phys.relationOrder) != len(p.rels) || len(p.phys.joinSteps)+1 != len(p.rels)) {
		copyPlan := *p
		p = &copyPlan
		p.phys.relationOrder = make([]int, len(p.rels))
		for i := range p.rels {
			p.phys.relationOrder[i] = i
		}
		p.phys.joinSteps = make([]physicalJoinStep, len(p.joins))
		for i := range p.joins {
			p.phys.joinSteps[i] = physicalJoinStep{onIndices: []int{i}}
			if i == 0 {
				p.phys.joinSteps[i].hashJoin = p.phys.hashJoin
			}
		}
	}

	profileStart := m.Accrued
	var owned []*rowSpool
	var escaped *rowSpool
	spool := func() *rowSpool { s := newRowSpool(db); owned = append(owned, s); return s }
	defer func() {
		for _, s := range owned {
			if s != escaped {
				s.close()
			}
		}
	}()
	rels := make([]*rowSpool, len(p.rels))
	work := make([]int64, len(rels))
	scanOrder := make([]int, len(rels))
	for i := range scanOrder {
		scanOrder[i] = i
	}
	if p.phys.joinPkOrdered && len(rels) == 2 {
		scanOrder = []int{physicalRelOrdinal(p, 0), physicalRelOrdinal(p, 1)}
	}
	for _, i := range scanOrder {
		if p.rels[i].lateral || p.phys.relINLBounds[i] != nil {
			continue
		}
		rels[i] = spool()
		before := m.Accrued
		if err := db.scanBlockingRel(p, i, env, m, rels[i]); err != nil {
			return emitter{}, err
		}
		work[i] = m.Accrued - before
	}
	if p.phys.joinPkOrdered {
		result, err := db.boundedJoinTopN(p, rels, work, spool, env, m, profileStart)
		if err != nil {
			return emitter{}, err
		}
		stream, err := result.output()
		if err != nil {
			return emitter{}, err
		}
		escaped = result
		return emitter{sorted: stream, mode: emitSorted, end: result.count, identity: true, precharged: true}, nil
	}
	var rows *rowSpool
	if len(rels) == 0 {
		rows = spool()
		if err := rows.push(storedRow{}); err != nil {
			return emitter{}, err
		}
	} else {
		rows = rels[0]
	}
	if len(rels) > 2 {
		rows = spool()
		driver := p.phys.relationOrder[0]
		if err := rels[driver].each(func(row storedRow) error { return rows.push(placePhysicalRelationRow(p, driver, row)) }); err != nil {
			return emitter{}, err
		}
		for i, step := range p.phys.joinSteps {
			next := spool()
			inner := p.phys.relationOrder[i+1]
			if err := db.boundedJoinStep(p, step, inner, rows, rels[inner], next, env, m, work); err != nil {
				return emitter{}, err
			}
			rows = next
			if i+1 < len(p.phys.joinSteps) {
				node := "Nested Loop"
				if step.hashJoin != nil {
					node = "Hash Join"
				}
				db.explainActual.recordParent(node, m.Accrued)
			}
		}
	} else if len(rels) == 2 {
		rows = spool()
		if p.phys.hashJoin != nil {
			if err := db.boundedJoin(p, rels, rows, env, m); err != nil {
				return emitter{}, err
			}
		} else {
			outer, inner := physicalRelOrdinal(p, 0), physicalRelOrdinal(p, 1)
			lefts := spool()
			if err := rels[outer].each(func(row storedRow) error { return lefts.push(placePhysicalRelationRow(p, outer, row)) }); err != nil {
				return emitter{}, err
			}
			if err := db.boundedJoinStep(p, physicalJoinStep{onIndices: []int{0}}, inner, lefts, rels[inner], rows, env, m, work); err != nil {
				return emitter{}, err
			}
		}
	}
	order := make([]int, len(rels))
	for i := range order {
		order[i] = i
	}
	if len(p.phys.relationOrder) == len(rels) {
		copy(order, p.phys.relationOrder)
	}
	for _, i := range order {
		node := selectActualRelNode(p.rels[i])
		if selectActualRootNode(p) != node {
			db.explainActual.record(node, work[i])
		}
	}
	if len(rels) >= 2 {
		node := "Nested Loop"
		if len(rels) == 2 && p.phys.hashJoin != nil || len(rels) > 2 && p.phys.joinSteps[len(p.phys.joinSteps)-1].hashJoin != nil {
			node = "Hash Join"
		}
		if selectActualRootNode(p) != node {
			db.explainActual.recordParent(node, m.Accrued)
		}
	}
	if p.filter != nil {
		filtered := spool()
		err := rows.each(func(row storedRow) error {
			v, err := p.filter.eval(row, env, m)
			if err != nil {
				return err
			}
			if v.IsTrue() {
				return filtered.push(row)
			}
			return nil
		})
		if err != nil {
			return emitter{}, err
		}
		rows = filtered
		if selectActualRootNode(p) != "Filter" {
			db.explainActual.recordParent("Filter", m.Accrued)
		}
	}
	window := func(input *rowSpool) (*rowSpool, error) {
		// While materialized the window input is a query-memory row buffer (memory.md §5.1):
		// pre-projection rows under the touched mask, or projected-shape group rows for a grouped
		// window.
		var mask []bool
		if !p.isAgg {
			mask = p.memoryMask(m)
		}
		var rs []storedRow
		if err := input.each(func(r storedRow) error {
			if err := m.admitRowMasked(r, mask); err != nil {
				return err
			}
			rs = append(rs, r)
			return nil
		}); err != nil {
			return nil, err
		}
		if err := applyWindowStage(rs, p.windowSpecs, p.windowKeys, env, m); err != nil {
			return nil, err
		}
		releaseRowsMasked(m, rs, mask)
		out := spool()
		for _, r := range rs {
			if err := out.push(r); err != nil {
				return nil, err
			}
		}
		if selectActualRootNode(p) != "Window" {
			db.explainActual.recordParent("Window", m.Accrued)
		}
		return out, nil
	}
	if p.hasWindow && !p.isAgg {
		var err error
		rows, err = window(rows)
		if err != nil {
			return emitter{}, err
		}
	}
	if p.isAgg {
		if len(p.groupExprs) > 0 {
			extended := spool()
			err := rows.each(func(row storedRow) error {
				if err := m.Guard(); err != nil {
					return err
				}
				row = append(storedRow(nil), row...)
				for _, e := range p.groupExprs {
					v, err := e.eval(row, env, m)
					if err != nil {
						return err
					}
					row = append(row, v)
				}
				return extended.push(row)
			})
			if err != nil {
				return emitter{}, err
			}
			rows = extended
		}
		grouped := spool()
		if err := db.boundedAggregate(p, rows, grouped, env, m); err != nil {
			return emitter{}, err
		}
		rows = grouped
		if p.having != nil {
			kept := spool()
			err := rows.each(func(row storedRow) error {
				v, err := p.having.eval(row, env, m)
				if err != nil {
					return err
				}
				if v.IsTrue() {
					return kept.push(row)
				}
				return nil
			})
			if err != nil {
				return emitter{}, err
			}
			rows = kept
		}
		if selectActualRootNode(p) != "Aggregate" {
			db.explainActual.recordParent("Aggregate", m.Accrued)
		}
	}
	if p.hasWindow && p.isAgg {
		var err error
		rows, err = window(rows)
		if err != nil {
			return emitter{}, err
		}
	}
	var sorted *sortedRows
	total := rows.count
	if len(p.order) > 0 {
		extended := spool()
		err := rows.each(func(row storedRow) error {
			row = append(storedRow(nil), row...)
			for _, e := range p.orderExprs {
				v, err := e.eval(row, env, m)
				if err != nil {
					return err
				}
				row = append(row, v)
			}
			return extended.push(row)
		})
		if err != nil {
			return emitter{}, err
		}
		sorted, err = db.blockingSort(extended, p.order)
		if err != nil {
			return emitter{}, err
		}
		if selectActualRootNode(p) != "Sort" {
			db.explainActual.recordParent("Sort", m.Accrued)
		}
	}
	if p.distinct {
		distinct := spool()
		seen := newBoundedMap(db)
		defer seen.close()
		add := func(row storedRow) error {
			projected := make(storedRow, len(p.projections))
			for i, e := range p.projections {
				v, err := e.eval(row, env, m)
				if err != nil {
					return err
				}
				projected[i] = v
			}
			key := distinctRowKey(projected)
			_, ok, err := seen.get(key)
			if err != nil {
				return err
			}
			if ok {
				return nil
			}
			if err = seen.put(key, nil); err != nil {
				return err
			}
			return distinct.push(projected)
		}
		var err error
		if sorted != nil {
			func() {
				defer sorted.close()
				for {
					row, ok, e := sorted.next()
					if e != nil {
						err = e
						return
					}
					if !ok {
						return
					}
					if e = add(row); e != nil {
						err = e
						return
					}
				}
			}()
			sorted = nil
		} else {
			err = rows.each(add)
		}
		if err != nil {
			return emitter{}, err
		}
		rows = distinct
		total = rows.count
		if selectActualRootNode(p) != "Distinct" {
			db.explainActual.recordParent("Distinct", m.Accrued)
		}
	}
	if sorted == nil {
		var err error
		sorted, err = rows.output()
		if err != nil {
			return emitter{}, err
		}
		escaped = rows
	}
	start := int64(0)
	if p.offset != nil {
		start = min(*p.offset, total)
	}
	count := total - start
	if p.limit != nil {
		count = min(count, *p.limit)
	}
	for i := int64(0); i < start; i++ {
		if _, _, err := sorted.next(); err != nil {
			sorted.close()
			return emitter{}, err
		}
	}
	return emitter{sorted: sorted, mode: emitSorted, end: count, identity: p.distinct}, nil
}

func (db *engine) boundedJoinStep(p *selectPlan, step physicalJoinStep, inner int, lefts, rights, out *rowSpool, env *evalEnv, m *costMeter, work []int64) error {
	var table *boundedJoinTable
	var indices []int
	var types []dataType
	if step.hashJoin != nil {
		table = &boundedJoinTable{mem: make(map[uint64][]hashJoinEntry), budget: db.session.workMem, dir: db.spillDir}
		defer table.close()
		indices, types = joinKeyParts(step.hashJoin, p.rels[inner].offset, true)
		if err := rights.each(func(row storedRow) error {
			key, present, err := hashJoinRowKey(row, indices, types, costs.HashBuild, m)
			if err != nil || !present {
				return err
			}
			return table.add(key, row)
		}); err != nil {
			return err
		}
		indices, types = joinKeyParts(step.hashJoin, 0, false)
	}
	kind := physicalStepKind(p, step)
	emitLeft := kind == joinLeft || kind == joinFull
	emitRight := kind == joinRight || kind == joinFull
	matchedRight := newBoundedMap(db)
	defer matchedRight.close()
	if err := lefts.each(func(left storedRow) error {
		matched := false
		var ordinal int64
		accept := func(right storedRow) error {
			index := ordinal
			ordinal++
			row := append(storedRow(nil), left...)
			copy(row[p.rels[inner].offset:], right)
			for _, onIndex := range step.onIndices {
				if on := p.joins[onIndex].on; on != nil {
					v, err := on.eval(row, env, m)
					if err != nil {
						return err
					}
					if !v.IsTrue() {
						return nil
					}
				}
			}
			matched = true
			if emitRight {
				if err := matchedRight.put(distinctRowKey([]Value{IntValue(index)}), nil); err != nil {
					return err
				}
			}
			return out.push(row)
		}
		if table != nil {
			key, present, err := hashJoinRowKey(left, indices, types, costs.HashProbe, m)
			if err != nil {
				return err
			}
			if present {
				if err := table.each(key, func(k []byte, _ storedRow) error {
					if err := m.Guard(); err != nil {
						return err
					}
					m.Charge(costs.HashProbe * int64(max(1, min(len(k), len(key)))))
					return nil
				}); err != nil {
					return err
				}
				if err := table.each(key, func(k []byte, right storedRow) error {
					if !bytes.Equal(k, key) {
						return nil
					}
					return accept(right)
				}); err != nil {
					return err
				}
			}
		} else {
			candidate := rights
			if candidate == nil {
				var err error
				candidate, err = db.blockingDynamicRight(p, inner, left, env, m, work)
				if err != nil {
					return err
				}
				defer candidate.close()
			}
			if err := candidate.each(accept); err != nil {
				return err
			}
		}
		if emitLeft && !matched {
			return out.push(append(storedRow(nil), left...))
		}
		return nil
	}); err != nil {
		return err
	}
	if emitRight {
		var ordinal int64
		return rights.each(func(right storedRow) error {
			_, had, err := matchedRight.get(distinctRowKey([]Value{IntValue(ordinal)}))
			ordinal++
			if err != nil {
				return err
			}
			if had {
				return nil
			}
			return out.push(placePhysicalRelationRow(p, inner, right))
		})
	}
	return nil
}

var errBlockingWindowDone = errors.New("blocking join window complete")

func (db *engine) boundedJoinTopN(p *selectPlan, rels []*rowSpool, work []int64, spool func() *rowSpool, env *evalEnv, m *costMeter, profileStart int64) (*rowSpool, error) {
	out := spool()
	var lefts, rights *rowSpool
	var hp *hashJoinPlan
	var inner, leftOffset int
	var onIndices []int
	var combine func(storedRow, storedRow) storedRow
	if len(rels) == 2 {
		outer := physicalRelOrdinal(p, 0)
		inner = physicalRelOrdinal(p, 1)
		leftOffset = p.rels[outer].offset
		lefts, rights = rels[outer], rels[inner]
		hp = p.phys.hashJoin
		onIndices = []int{0}
		combine = func(left, right storedRow) storedRow {
			return combinePhysicalRelationRows(p, outer, left, inner, right)
		}
	} else {
		driver := p.phys.relationOrder[0]
		lefts = spool()
		if err := rels[driver].each(func(row storedRow) error { return lefts.push(placePhysicalRelationRow(p, driver, row)) }); err != nil {
			return nil, err
		}
		for i, step := range p.phys.joinSteps[:len(p.phys.joinSteps)-1] {
			next := spool()
			ri := p.phys.relationOrder[i+1]
			if err := db.boundedJoinStep(p, step, ri, lefts, rels[ri], next, env, m, work); err != nil {
				return nil, err
			}
			lefts = next
			node := "Nested Loop"
			if step.hashJoin != nil {
				node = "Hash Join"
			}
			db.explainActual.recordParent(node, m.Accrued)
		}
		inner = p.phys.relationOrder[len(rels)-1]
		step := p.phys.joinSteps[len(p.phys.joinSteps)-1]
		hp = step.hashJoin
		rights = rels[inner]
		onIndices = step.onIndices
		combine = func(left, right storedRow) storedRow {
			row := append(storedRow(nil), left...)
			copy(row[p.rels[inner].offset:], right)
			return row
		}
	}
	table := &boundedJoinTable{mem: make(map[uint64][]hashJoinEntry), budget: db.session.workMem, dir: db.spillDir}
	defer table.close()
	if hp != nil && (len(rels) > 2 || p.limit == nil || *p.limit != 0) {
		indices, types := joinKeyParts(hp, p.rels[inner].offset, true)
		if err := rights.each(func(row storedRow) error {
			key, present, err := hashJoinRowKey(row, indices, types, costs.HashBuild, m)
			if err != nil || !present {
				return err
			}
			return table.add(key, row)
		}); err != nil {
			return nil, err
		}
	}
	var filterWork, outputWork, passed int64
	offset := int64(0)
	if p.offset != nil {
		offset = *p.offset
	}
	if p.limit == nil || *p.limit > 0 {
		var indices []int
		var types []dataType
		if hp != nil {
			indices, types = joinKeyParts(hp, leftOffset, false)
		}
		err := lefts.each(func(left storedRow) error {
			accept := func(right storedRow) error {
				row := combine(left, right)
				for _, i := range onIndices {
					if on := p.joins[i].on; on != nil {
						v, err := on.eval(row, env, m)
						if err != nil {
							return err
						}
						if !v.IsTrue() {
							return nil
						}
					}
				}
				if p.filter != nil {
					before := m.Accrued
					v, err := p.filter.eval(row, env, m)
					filterWork += m.Accrued - before
					if err != nil {
						return err
					}
					if !v.IsTrue() {
						return nil
					}
				}
				passed++
				if passed <= offset {
					return nil
				}
				if err := m.Guard(); err != nil {
					return err
				}
				before := m.Accrued
				m.Charge(costs.RowProduced)
				projected := make(storedRow, len(p.projections))
				for i, e := range p.projections {
					v, err := e.eval(row, env, m)
					if err != nil {
						return err
					}
					projected[i] = v
				}
				outputWork += m.Accrued - before
				if err := out.push(projected); err != nil {
					return err
				}
				if p.limit != nil && out.count >= *p.limit {
					return errBlockingWindowDone
				}
				return nil
			}
			if hp != nil {
				key, present, err := hashJoinRowKey(left, indices, types, costs.HashProbe, m)
				if err != nil || !present {
					return err
				}
				if err = table.each(key, func(k []byte, _ storedRow) error {
					if err := m.Guard(); err != nil {
						return err
					}
					m.Charge(costs.HashProbe * int64(max(1, min(len(k), len(key)))))
					return nil
				}); err != nil {
					return err
				}
				return table.each(key, func(k []byte, right storedRow) error {
					if !bytes.Equal(k, key) {
						return nil
					}
					return accept(right)
				})
			}
			candidate := rights
			if candidate == nil {
				logical := left
				if len(rels) == 2 {
					logical = placePhysicalRelationRow(p, physicalRelOrdinal(p, 0), left)
				}
				var err error
				candidate, err = db.blockingDynamicRight(p, inner, logical, env, m, work)
				if err != nil {
					return err
				}
				defer candidate.close()
			}
			return candidate.each(accept)
		})
		if err != nil && err != errBlockingWindowDone {
			return nil, err
		}
	}
	order := p.phys.relationOrder
	if len(order) != len(rels) {
		order = []int{0, 1}
	}
	for _, i := range order {
		node := selectActualRelNode(p.rels[i])
		if len(rels) > 2 || selectActualRootNode(p) != node {
			db.explainActual.record(node, work[i])
		}
	}
	through := m.Accrued - profileStart - filterWork - outputWork
	node := "Nested Loop"
	if hp != nil {
		node = "Hash Join"
	}
	if len(rels) > 2 || selectActualRootNode(p) != node {
		db.explainActual.recordParent(node, through)
	}
	if p.filter != nil && (len(rels) > 2 || selectActualRootNode(p) != "Filter") {
		db.explainActual.recordParent("Filter", through+filterWork)
	}
	return out, nil
}

// Collated keys are decorated only after every expression key has evaluated.
func (db *engine) blockingSort(input *rowSpool, order []orderSlot) (*sortedRows, error) {
	var s *sorter
	defer func() {
		if s != nil {
			s.close()
		}
	}()
	err := input.each(func(row storedRow) error {
		row = append(storedRow(nil), row...)
		keys := append([]orderSlot(nil), order...)
		for i, k := range order {
			if k.collation != nil {
				v := NullValue()
				if row[k.idx].Kind == ValText {
					b, err := sortKey(k.collation, row[k.idx].str())
					if err != nil {
						return err
					}
					v = ByteaValue(b)
				}
				keys[i].idx = len(row)
				keys[i].collation = nil
				row = append(row, v)
			}
		}
		if s == nil {
			s = db.newSorterFor(keys)
		}
		return s.push(row)
	})
	if err != nil {
		return nil, err
	}
	if s == nil {
		return &sortedRows{}, nil
	}
	out, err := s.finish()
	if err == nil {
		s.runs = nil
	}
	return out, err
}

// INL/LATERAL gathering remains an upstream owner; transfer each gathered batch
// into scratch before the join's replay and release it after that probe.
func (db *engine) blockingDynamicRight(p *selectPlan, inner int, left storedRow, env *evalEnv, m *costMeter, work []int64) (*rowSpool, error) {
	outer := env.outer
	boundLeft := left
	if p.rels[inner].lateral {
		outer = append(append([]storedRow(nil), outer...), left)
		boundLeft = nil
	}
	before := m.Accrued
	rows, err := db.materializeRel(p, inner, env.params, outer, boundLeft, env.rng, env.ctes, m)
	work[inner] += m.Accrued - before
	if err != nil {
		return nil, err
	}
	releaseRowsMasked(m, rows, p.relMasks[inner])
	out := newRowSpool(db)
	for _, row := range rows {
		if err := out.push(row); err != nil {
			out.close()
			return nil, err
		}
	}
	return out, nil
}
