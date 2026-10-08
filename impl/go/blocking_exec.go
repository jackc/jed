package jed

// Blocking operators retain the eager executor's logical phases while moving
// their input, keyed state and output through bounded scratch owners. Existing
// gather/window operators remain separate owners; spilling their downstream
// aggregate or join state does not claim a whole-query memory limit.
//
// Every spill structure charges its resident elements to the statement's query-memory account
// (memory.md §6.6) and returns them on spill or when discarded. Go has no destructors, so each
// structure is released (or closed) explicitly at the point it is discarded — the same points, in
// the same order relative to the lane's reservations, as the Rust core's scope ends — and the lane's
// owner closes whatever is left (scratch files included) when the lane returns.

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

// blockingOwner owns the lane's scratch structures, so an error path never leaks a scratch file or a
// query-memory charge: whatever is still open when the lane returns is closed, except the output
// spool that escapes into the emitter.
type blockingOwner struct {
	db     *engine
	spools []*rowSpool
	maps   []*stateMap
	tables []*hashRows
}

func (o *blockingOwner) spool() *rowSpool {
	s := newRowSpool(o.db)
	o.spools = append(o.spools, s)
	return s
}

func (o *blockingOwner) adopt(s *rowSpool) *rowSpool {
	o.spools = append(o.spools, s)
	return s
}

func (o *blockingOwner) stateMap() *stateMap {
	m := newStateMap(o.db)
	o.maps = append(o.maps, m)
	return m
}

func (o *blockingOwner) hashRows() *hashRows {
	h := newHashRows(o.db)
	o.tables = append(o.tables, h)
	return h
}

func (o *blockingOwner) closeAll(escaped *rowSpool) {
	for _, s := range o.spools {
		if s != escaped {
			s.close()
		}
	}
	for _, m := range o.maps {
		m.close()
	}
	for _, h := range o.tables {
		h.close()
	}
}

// spoolMaterialized spools a materialized relation's rows: they leave the Q1 row account and enter
// spool residency with their untouched slots NULLed (memory.md §5.3/§6.1). The caller owns (and
// closes) the returned spool.
func (db *engine) spoolMaterialized(rows []storedRow, mask []bool, m *costMeter) (*rowSpool, error) {
	releaseRowsMasked(m, rows, mask)
	spool := newRowSpool(db)
	for _, row := range rows {
		if err := spool.push(topKPruneUntouched(row, mask)); err != nil {
			spool.close()
			return nil, err
		}
	}
	return spool, nil
}

func (db *engine) scanBlockingRel(p *selectPlan, i int, env *evalEnv, m *costMeter, own *blockingOwner) (*rowSpool, error) {
	r := p.rels[i]
	if r.srf != nil || r.cte != nil || r.derived != nil || p.phys.relBounds[i].needsEagerScan() {
		// Those producers retain their existing upstream materialization contract. Transfer their
		// owned rows into bounded downstream state instead of disabling spill entirely.
		rows, err := db.materializeRel(p, i, env.params, env.outer, nil, env.rng, env.ctes, m)
		if err != nil {
			return nil, err
		}
		// Spool residency is operator state bounded by work_mem (memory.md §6.6): the rows leave the
		// query-memory row account as they enter the spool.
		spool, err := db.spoolMaterialized(rows, p.relMasks[i], m)
		if err != nil {
			return nil, err
		}
		return own.adopt(spool), nil
	}
	out := own.spool()
	store := db.lkpStoreScoped(r.db, r.tableName)
	b := unboundedBound()
	empty := false
	if sb := p.phys.relBounds[i]; sb != nil && sb.pk != nil {
		b, empty = db.buildKeyBound(sb.pk, env.params, env.outer, nil)
	}
	if empty {
		return out, nil
	}
	pages, slabs, err := store.OverlapScanUnits(b, p.relMasks[i])
	if err != nil {
		return nil, err
	}
	m.Charge(costs.PageRead*int64(pages) + costs.ValueDecompress*int64(slabs))
	err = store.ScanRange(b, func(_ []byte, row storedRow) (bool, error) {
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
	if err != nil {
		return nil, err
	}
	return out, nil
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

// spillHashTable is the bounded lane's hash-join build side: each build row whose key has no NULL is
// a hashRows entry [key bytes, row] under the key's full hash, so a probe walks one hash's rows in
// build order and rechecks the full key.
type spillHashTable struct {
	db          *engine
	entries     *hashRows
	leftIndices []int
	types       []dataType
}

func (db *engine) buildSpillHashTable(hash *hashJoinPlan, buildOffset, probeOffset int, rows *rowSpool, m *costMeter, own *blockingOwner) (*spillHashTable, error) {
	leftIndices, types := joinKeyParts(hash, probeOffset, false)
	rightIndices, _ := joinKeyParts(hash, buildOffset, true)
	entries := own.hashRows()
	err := rows.each(func(row storedRow) error {
		key, present, err := hashJoinRowKey(row, rightIndices, types, costs.HashBuild, m)
		if err != nil || !present {
			return err
		}
		return entries.push(hashJoinFNV1a(key), storedRow{ByteaValue(key), CompositeValue(row)})
	})
	if err != nil {
		return nil, err
	}
	return &spillHashTable{db: db, entries: entries, leftIndices: leftIndices, types: types}, nil
}

// close discards the table (the step completed).
func (t *spillHashTable) close() {
	if t != nil {
		t.entries.close()
	}
}

// probe collects the build rows matching left's key into a spool the caller owns (and closes). The
// probe charges every collision check before the caller evaluates ON.
func (t *spillHashTable) probe(left storedRow, m *costMeter) (*rowSpool, error) {
	matches := newRowSpool(t.db)
	key, present, err := hashJoinRowKey(left, t.leftIndices, t.types, costs.HashProbe, m)
	if err != nil {
		matches.close()
		return nil, err
	}
	if present {
		err = t.entries.each(hashJoinFNV1a(key), func(entry storedRow) error {
			stored := []byte(entry[0].str())
			if err := m.Guard(); err != nil {
				return err
			}
			m.Charge(costs.HashProbe * int64(max(1, min(len(stored), len(key)))))
			if bytes.Equal(stored, key) {
				return matches.push(*entry[1].composite())
			}
			return nil
		})
		if err != nil {
			matches.close()
			return nil, err
		}
	}
	return matches, nil
}

// execBoundedBlocking is the bounded-spill lane. Its spill structures reserve without the meter in
// reach, so a rejected reservation is passed through costFirst here (memory.md §6.7).
func (db *engine) execBoundedBlocking(p *selectPlan, env *evalEnv, m *costMeter) (emitter, error) {
	em, err := db.execBoundedBlockingLane(p, env, m)
	if err != nil {
		return emitter{}, m.costFirst(err)
	}
	return em, nil
}

func (db *engine) execBoundedBlockingLane(p *selectPlan, env *evalEnv, m *costMeter) (emitter, error) {
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
	own := &blockingOwner{db: db}
	var escaped *rowSpool
	defer func() { own.closeAll(escaped) }()

	if p.phys.joinPkOrdered {
		out, err := db.boundedJoinTopN(p, own, env, m)
		if err != nil {
			return emitter{}, err
		}
		count := out.count
		stream, err := out.output()
		if err != nil {
			return emitter{}, err
		}
		escaped = out
		return emitter{sorted: stream, mode: emitSorted, end: count, identity: true, precharged: true}, nil
	}

	root := selectActualRootNode(p)
	n := len(p.rels)
	inputs := make([]*rowSpool, n)
	work := make([]int64, n)
	for i := 0; i < n; i++ {
		before := m.Accrued
		if p.rels[i].lateral || p.phys.relINLBounds[i] != nil {
			inputs[i] = own.spool()
		} else {
			s, err := db.scanBlockingRel(p, i, env, m, own)
			if err != nil {
				return emitter{}, err
			}
			inputs[i] = s
		}
		work[i] = m.Accrued - before
	}
	var rows *rowSpool
	switch {
	case n >= 2:
		var err error
		if rows, err = db.blockingJoinSteps(p, inputs, env, work, n-1, m, own); err != nil {
			return emitter{}, err
		}
	case n == 0:
		rows = own.spool()
		if err := rows.push(storedRow{}); err != nil {
			return emitter{}, err
		}
	default:
		rows = inputs[0]
	}
	order := make([]int, n)
	for i := range order {
		order[i] = i
	}
	if len(p.phys.relationOrder) == n {
		copy(order, p.phys.relationOrder)
	}
	for _, i := range order {
		node := selectActualRelNode(p.rels[i])
		if root != node {
			db.explainActual.record(node, work[i])
		}
	}
	if n >= 2 {
		node := "Nested Loop"
		if n == 2 && p.phys.hashJoin != nil || n > 2 && p.phys.joinSteps[len(p.phys.joinSteps)-1].hashJoin != nil {
			node = "Hash Join"
		}
		if root != node {
			db.explainActual.recordParent(node, m.Accrued)
		}
	}
	// The WHERE pass always moves the rows into a fresh spool; the consumed spool is discarded once
	// the pass completes.
	filtered := own.spool()
	err := rows.each(func(row storedRow) error {
		if p.filter != nil {
			v, err := p.filter.eval(row, env, m)
			if err != nil {
				return err
			}
			if !v.IsTrue() {
				return nil
			}
		}
		return filtered.push(row)
	})
	if err != nil {
		return emitter{}, err
	}
	rows.close()
	rows = filtered
	if p.filter != nil && root != "Filter" {
		db.explainActual.recordParent("Filter", m.Accrued)
	}
	if p.hasWindow && !p.isAgg {
		if rows, err = db.blockingWindow(p, rows, env, m, own); err != nil {
			return emitter{}, err
		}
	}
	if p.isAgg {
		if rows, err = db.blockingAggregate(p, rows, env, m, own); err != nil {
			return emitter{}, err
		}
		if p.having != nil {
			kept := own.spool()
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
			rows.close()
			rows = kept
		}
		if root != "Aggregate" {
			db.explainActual.recordParent("Aggregate", m.Accrued)
		}
		if p.hasWindow {
			if rows, err = db.blockingWindow(p, rows, env, m, own); err != nil {
				return emitter{}, err
			}
		}
	}
	if len(p.order) > 0 {
		if rows, err = db.blockingOrder(p, rows, env, m, own); err != nil {
			return emitter{}, err
		}
		if root != "Sort" {
			db.explainActual.recordParent("Sort", m.Accrued)
		}
	}
	if p.distinct {
		seen := own.stateMap()
		distinct := own.spool()
		err := rows.each(func(row storedRow) error {
			out := make(storedRow, len(p.projections))
			for i, e := range p.projections {
				v, err := e.eval(row, env, m)
				if err != nil {
					return err
				}
				out[i] = v
			}
			inserted, err := seen.insert(out)
			if err != nil || !inserted {
				return err
			}
			return distinct.push(out)
		})
		if err != nil {
			return emitter{}, err
		}
		// The DISTINCT pass completed: its input and its dedup set are discarded.
		rows.close()
		seen.close()
		rows = distinct
		if root != "Distinct" {
			db.explainActual.recordParent("Distinct", m.Accrued)
		}
	}
	total := rows.count
	start := int64(0)
	if p.offset != nil {
		start = min(*p.offset, total)
	}
	count := total - start
	if p.limit != nil {
		count = min(count, *p.limit)
	}
	sorted, err := rows.output()
	if err != nil {
		return emitter{}, err
	}
	escaped = rows
	for i := int64(0); i < start; i++ {
		if _, _, err := sorted.next(); err != nil {
			sorted.close()
			return emitter{}, err
		}
	}
	return emitter{sorted: sorted, mode: emitSorted, end: count, identity: p.distinct}, nil
}

// blockingOrder evaluates every ORDER BY expression before any collation decoration can fail, then
// sorts through the external sorter (each row carrying one key per ORDER BY slot) and spools the
// sorted rows; the sorter's final run is discarded once the spool is filled.
func (db *engine) blockingOrder(p *selectPlan, rows *rowSpool, env *evalEnv, m *costMeter, own *blockingOwner) (*rowSpool, error) {
	decorated := own.spool()
	err := rows.each(func(row storedRow) error {
		row = append(storedRow(nil), row...)
		for _, e := range p.orderExprs {
			v, err := e.eval(row, env, m)
			if err != nil {
				return err
			}
			row = append(row, v)
		}
		return decorated.push(row)
	})
	if err != nil {
		return nil, err
	}
	rows.close()
	var s *sorter
	defer func() {
		if s != nil {
			s.close()
		}
	}()
	err = decorated.each(func(row storedRow) error {
		row = append(storedRow(nil), row...)
		base := len(row)
		keys := make([]orderSlot, len(p.order))
		for i, k := range p.order {
			keys[i] = orderSlot{idx: base + i, descending: k.descending, nullsFirst: k.nullsFirst}
		}
		for _, k := range p.order {
			key := row[k.idx]
			if k.collation != nil && key.Kind == ValText {
				b, err := sortKey(k.collation, key.str())
				if err != nil {
					return err
				}
				key = ByteaValue(b)
			}
			row = append(row, key)
		}
		if s == nil {
			s = db.newSorterFor(keys)
		}
		return s.push(row)
	})
	if err != nil {
		return nil, err
	}
	decorated.close()
	if s == nil {
		s = db.newSorterFor(nil)
	}
	sorted, err := s.finish()
	if err != nil {
		return nil, err
	}
	defer sorted.close()
	out := own.spool()
	for {
		row, ok, err := sorted.next()
		if err != nil {
			return nil, err
		}
		if !ok {
			break
		}
		if err := out.push(row); err != nil {
			return nil, err
		}
	}
	return out, nil
}

// blockingWindow runs the window stage. Its input is materialized as a query-memory row buffer
// (memory.md §5.1) — pre-projection rows under the touched mask, or projected-shape group rows for a
// grouped window — while the window runs; the consumed input spool is discarded once the output
// spool is filled.
func (db *engine) blockingWindow(p *selectPlan, rows *rowSpool, env *evalEnv, m *costMeter, own *blockingOwner) (*rowSpool, error) {
	var mask []bool
	if !p.isAgg {
		mask = p.memoryMask(m)
	}
	var rs []storedRow
	if err := rows.each(func(r storedRow) error {
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
	out := own.spool()
	for _, r := range rs {
		if err := out.push(r); err != nil {
			return nil, err
		}
	}
	if selectActualRootNode(p) != "Window" {
		db.explainActual.recordParent("Window", m.Accrued)
	}
	rows.close()
	return out, nil
}

// blockingDynamicRight re-materializes a LATERAL / index-nested-loop inner relation for one left row
// and spools it; the caller closes the spool once that left row's join work is done.
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
	return db.spoolMaterialized(rows, p.relMasks[inner], m)
}

// blockingStepKinds reports whether a join step emits unmatched left and/or right rows.
func blockingStepKinds(p *selectPlan, onIndices []int) (emitLeft, emitRight bool) {
	for _, i := range onIndices {
		switch p.joins[i].kind {
		case joinLeft:
			emitLeft = true
		case joinRight:
			emitRight = true
		case joinFull:
			emitLeft, emitRight = true, true
		}
	}
	return emitLeft, emitRight
}

// blockingJoinSteps places the driver's rows into the full-width layout, then runs count join steps
// in physical order. Each step's left rows, unmatched-right membership, and hash table are discarded
// when the step completes; each consumed input is discarded as soon as nothing reads it again.
func (db *engine) blockingJoinSteps(p *selectPlan, inputs []*rowSpool, env *evalEnv, work []int64, count int, m *costMeter, own *blockingOwner) (*rowSpool, error) {
	n := len(p.rels)
	physical := len(p.phys.relationOrder) == n && (n == 2 || len(p.phys.joinSteps)+1 == n)
	order := make([]int, n)
	for i := range order {
		order[i] = i
	}
	if physical {
		copy(order, p.phys.relationOrder)
	}
	driver := order[0]
	rows := own.spool()
	if err := inputs[driver].each(func(row storedRow) error {
		return rows.push(placePhysicalRelationRow(p, driver, row))
	}); err != nil {
		return nil, err
	}
	inputs[driver].close()
	inputs[driver] = own.spool()
	for position := 0; position < count; position++ {
		inner := order[position+1]
		var onIndices []int
		var hash *hashJoinPlan
		if physical && n >= 3 {
			onIndices = p.phys.joinSteps[position].onIndices
			hash = p.phys.joinSteps[position].hashJoin
		} else {
			onIndices = []int{position}
			if position == 0 {
				hash = p.phys.hashJoin
			}
		}
		var table *spillHashTable
		if hash != nil {
			var err error
			if table, err = db.buildSpillHashTable(hash, p.rels[inner].offset, 0, inputs[inner], m, own); err != nil {
				return nil, err
			}
		}
		emitLeft, emitRight := blockingStepKinds(p, onIndices)
		if table != nil && !emitRight {
			inputs[inner].close()
			inputs[inner] = own.spool()
		}
		rightMatches := own.stateMap()
		next := own.spool()
		offset := p.rels[inner].offset
		err := rows.each(func(left storedRow) error {
			var candidates *rowSpool
			var err error
			if p.rels[inner].lateral || p.phys.relINLBounds[inner] != nil {
				candidates, err = db.blockingDynamicRight(p, inner, left, env, m, work)
			} else if table != nil {
				candidates, err = table.probe(left, m)
			}
			if err != nil {
				return err
			}
			// The per-left candidates are discarded once this left row's join work is done.
			defer candidates.close()
			source := candidates
			if source == nil {
				source = inputs[inner]
			}
			matched := false
			var ordinal int64
			if err := source.each(func(right storedRow) error {
				index := ordinal
				ordinal++
				combined := append(storedRow(nil), left...)
				copy(combined[offset:], right)
				for _, onIndex := range onIndices {
					if on := p.joins[onIndex].on; on != nil {
						v, err := on.eval(combined, env, m)
						if err != nil {
							return err
						}
						if !v.IsTrue() {
							return nil
						}
					}
				}
				if err := next.push(combined); err != nil {
					return err
				}
				matched = true
				if emitRight {
					if _, err := rightMatches.insert([]Value{IntValue(index)}); err != nil {
						return err
					}
				}
				return nil
			}); err != nil {
				return err
			}
			if emitLeft && !matched {
				return next.push(left)
			}
			return nil
		})
		if err != nil {
			return nil, err
		}
		if emitRight {
			var ordinal int64
			if err := inputs[inner].each(func(right storedRow) error {
				_, had, err := rightMatches.get([]Value{IntValue(ordinal)})
				ordinal++
				if err != nil || had {
					return err
				}
				return next.push(placePhysicalRelationRow(p, inner, right))
			}); err != nil {
				return nil, err
			}
		}
		old := rows
		rows = next
		inputs[inner].close()
		inputs[inner] = own.spool()
		// The step completed: its left rows, unmatched-right membership, and hash table go.
		old.close()
		rightMatches.close()
		table.close()
		if physical && n >= 3 && position+1 < len(p.phys.joinSteps) {
			node := "Nested Loop"
			if hash != nil {
				node = "Hash Join"
			}
			db.explainActual.recordParent(node, m.Accrued)
		}
	}
	return rows, nil
}

var errBlockingWindowDone = errors.New("blocking join window complete")

// boundedJoinTopN is the bounded lane's PK-ordered join top-N: the left subtree is joined in full,
// then the final step streams its left rows and stops once the OFFSET/LIMIT window fills.
func (db *engine) boundedJoinTopN(p *selectPlan, own *blockingOwner, env *evalEnv, m *costMeter) (*rowSpool, error) {
	startCost := m.Accrued
	n := len(p.rels)
	nway := n >= 3
	order := make([]int, n)
	for i := range order {
		order[i] = i
	}
	if len(p.phys.relationOrder) == n {
		copy(order, p.phys.relationOrder)
	}
	scanOrder := order
	if nway {
		scanOrder = make([]int, n)
		for i := range scanOrder {
			scanOrder[i] = i
		}
	}
	inputs := make([]*rowSpool, n)
	for i := range inputs {
		inputs[i] = own.spool()
	}
	work := make([]int64, n)
	for _, ri := range scanOrder {
		if p.rels[ri].lateral || p.phys.relINLBounds[ri] != nil {
			continue
		}
		before := m.Accrued
		s, err := db.scanBlockingRel(p, ri, env, m, own)
		if err != nil {
			return nil, err
		}
		inputs[ri] = s
		work[ri] = m.Accrued - before
	}
	rows, err := db.blockingJoinSteps(p, inputs, env, work, n-2, m, own)
	if err != nil {
		return nil, err
	}
	inner := order[n-1]
	var hash *hashJoinPlan
	onIndices := []int{0}
	if nway {
		hash = p.phys.joinSteps[n-2].hashJoin
		onIndices = p.phys.joinSteps[n-2].onIndices
	} else {
		hash = p.phys.hashJoin
	}
	var table *spillHashTable
	if hash != nil && (nway || p.limit == nil || *p.limit != 0) {
		if table, err = db.buildSpillHashTable(hash, p.rels[inner].offset, 0, inputs[inner], m, own); err != nil {
			return nil, err
		}
		inputs[inner].close()
		inputs[inner] = own.spool()
	}
	output := own.spool()
	var filterWork, outputWork, passed int64
	offset := int64(0)
	if p.offset != nil {
		offset = *p.offset
	}
	if p.limit == nil || *p.limit != 0 {
		innerOffset := p.rels[inner].offset
		err := rows.each(func(left storedRow) error {
			var candidates *rowSpool
			var err error
			if p.rels[inner].lateral || p.phys.relINLBounds[inner] != nil {
				candidates, err = db.blockingDynamicRight(p, inner, left, env, m, work)
			} else if table != nil {
				candidates, err = table.probe(left, m)
			}
			if err != nil {
				return err
			}
			// The per-left candidates are discarded once this left row's join work is done.
			defer candidates.close()
			source := candidates
			if source == nil {
				source = inputs[inner]
			}
			return source.each(func(right storedRow) error {
				combined := append(storedRow(nil), left...)
				copy(combined[innerOffset:], right)
				for _, i := range onIndices {
					if on := p.joins[i].on; on != nil {
						v, err := on.eval(combined, env, m)
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
					v, err := p.filter.eval(combined, env, m)
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
					v, err := e.eval(combined, env, m)
					if err != nil {
						return err
					}
					projected[i] = v
				}
				outputWork += m.Accrued - before
				if err := output.push(projected); err != nil {
					return err
				}
				if p.limit != nil && output.count >= *p.limit {
					return errBlockingWindowDone
				}
				return nil
			})
		})
		if err != nil && err != errBlockingWindowDone {
			return nil, err
		}
	}
	root := selectActualRootNode(p)
	for _, i := range order {
		node := selectActualRelNode(p.rels[i])
		if nway || root != node {
			db.explainActual.record(node, work[i])
		}
	}
	through := m.Accrued - startCost - filterWork - outputWork
	node := "Nested Loop"
	if hash != nil {
		node = "Hash Join"
	}
	if nway || root != node {
		db.explainActual.recordParent(node, through)
	}
	if p.filter != nil && (nway || root != "Filter") {
		db.explainActual.recordParent("Filter", through+filterWork)
	}
	return output, nil
}
