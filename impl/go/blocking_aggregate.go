package jed

import (
	"math"
	"strings"
)

// blockingAccRow packs an accumulator's running state into a private state tuple — the fixed-size
// running value only. Inputs an accumulator retains (JSON elements, ordered-set values) live in the
// group's collection rows instead, so a state replacement never grows with input. The tuple shape is
// part of the bounded lane's residency measure (key_bytes(state), memory.md §6.6), so it mirrors the
// Rust core's pack_acc field for field: it preserves the running decimal scale and the float special
// flags, never finalizing/recombining partials.
func blockingAccRow(a *acc) storedRow {
	// An ordered aggregate (aggregates.md §22) spools its tuples in the collection rows too.
	if a.ordered {
		return storedRow{}
	}
	switch a.plan {
	case planCountStar, planCount, planRegrCount:
		return storedRow{IntValue(a.count)}
	case planSumInt:
		return storedRow{IntValue(a.sumInt), BoolValue(a.seen)}
	case planSumDecimal:
		return storedRow{DecimalValue(a.sumDec), BoolValue(a.seen)}
	case planAvg:
		return storedRow{DecimalValue(a.sumDec), IntValue(a.count)}
	case planSumFloat32, planSumFloat64, planAvgFloat32, planAvgFloat64:
		f := a.floatSum
		return storedRow{Float64Value(f.total), IntValue(f.count), BoolValue(f.sawNaN), BoolValue(f.sawPosInf), BoolValue(f.sawNegInf)}
	case planMin, planMax, planBoolAnd, planBoolOr, planBitAnd, planBitOr, planBitXor:
		if !a.hasCur {
			return storedRow{NullValue()}
		}
		return storedRow{a.cur}
	case planJsonbAgg, planJsonAgg, planJsonbAggStrict, planJsonAggStrict,
		planJsonbObjectAgg, planJsonObjectAgg, planJsonbObjectAggUnique, planJsonObjectAggUnique,
		planArrayAgg, planStringAgg:
		return storedRow{BoolValue(a.seen)}
	case planVarPop, planVarSamp, planStddevSamp, planStddevPop:
		return storedRow{IntValue(a.exact.n), DecimalValue(a.exact.sx), DecimalValue(a.exact.sxx)}
	case planVarPopFloat, planVarSampFloat, planStddevSampFloat, planStddevPopFloat:
		return storedRow{Float64Value(a.float.n), Float64Value(a.float.sx), Float64Value(a.float.sxx)}
	case planCovarPop, planCovarSamp, planCorr, planRegrAvgx, planRegrAvgy, planRegrSxx, planRegrSyy, planRegrSxy, planRegrSlope, planRegrIntercept, planRegrR2:
		s := a.pairs
		return storedRow{Float64Value(s.n), Float64Value(s.sx), Float64Value(s.sxx), Float64Value(s.sy), Float64Value(s.syy), Float64Value(s.sxy)}
	case planArrayAggArray:
		// The seen flag, then the first input's shape: its dimension count and each (length, lower
		// bound) pair, so a later input is checked against it.
		row := storedRow{BoolValue(a.seen), IntValue(int64(len(a.arrDims)))}
		for i := range a.arrDims {
			row = append(row, IntValue(int64(a.arrDims[i])), IntValue(int64(a.arrLbounds[i])))
		}
		return row
	default: // ordered-set and hypothetical-set: everything lives in the collection rows
		return storedRow{}
	}
}

func blockingAccFromRow(spec aggSpec, r storedRow, zone ZoneRef) *acc {
	a := newAccFromSpec(spec, zone)
	if a.ordered {
		return a
	}
	switch a.plan {
	case planCountStar, planCount, planRegrCount:
		a.count = r[0].Int
	case planSumInt:
		a.sumInt = r[0].Int
		a.seen = r[1].boolVal()
	case planSumDecimal:
		a.sumDec = *r[0].decimal()
		a.seen = r[1].boolVal()
	case planAvg:
		a.sumDec = *r[0].decimal()
		a.count = r[1].Int
	case planSumFloat32, planSumFloat64, planAvgFloat32, planAvgFloat64:
		f := a.floatSum
		f.total, f.count = r[0].F64(), r[1].Int
		f.sawNaN, f.sawPosInf, f.sawNegInf = r[2].boolVal(), r[3].boolVal(), r[4].boolVal()
	case planMin, planMax, planBoolAnd, planBoolOr, planBitAnd, planBitOr, planBitXor:
		if !r[0].IsNull() {
			a.cur, a.hasCur = r[0], true
		}
	case planJsonbAgg, planJsonAgg, planJsonbAggStrict, planJsonAggStrict,
		planJsonbObjectAgg, planJsonObjectAgg, planJsonbObjectAggUnique, planJsonObjectAggUnique,
		planArrayAgg, planStringAgg:
		a.seen = r[0].boolVal()
	case planVarPop, planVarSamp, planStddevSamp, planStddevPop:
		a.exact = momentsExact{n: r[0].Int, sx: *r[1].decimal(), sxx: *r[2].decimal()}
	case planVarPopFloat, planVarSampFloat, planStddevSampFloat, planStddevPopFloat:
		a.float = momentsFloat{n: r[0].F64(), sx: r[1].F64(), sxx: r[2].F64()}
	case planCovarPop, planCovarSamp, planCorr, planRegrAvgx, planRegrAvgy, planRegrSxx, planRegrSyy, planRegrSxy, planRegrSlope, planRegrIntercept, planRegrR2:
		a.pairs = pairsFloat{n: r[0].F64(), sx: r[1].F64(), sxx: r[2].F64(), sy: r[3].F64(), syy: r[4].F64(), sxy: r[5].F64()}
	case planArrayAggArray:
		a.seen = r[0].boolVal()
		if n := int(r[1].Int); n > 0 {
			a.arrDims, a.arrLbounds = make([]int, n), make([]int32, n)
			for i := 0; i < n; i++ {
				a.arrDims[i], a.arrLbounds[i] = int(r[2+2*i].Int), int32(r[3+2*i].Int)
			}
		}
	}
	return a
}

// blockingAggregate folds every grouping set through bounded state: a keyed group map (key → state
// tuple [ordinal, packed accumulators…]), a first-occurrence key spool that owns emission order, the
// DISTINCT-aggregate and _unique-key membership maps, and the retained-input collection rows keyed by
// (group, aggregate). A set's structures are discarded once its group rows are spooled; the input is
// discarded when every set is done.
func (db *engine) blockingAggregate(p *selectPlan, rows *rowSpool, env *evalEnv, m *costMeter, own *blockingOwner) (*rowSpool, error) {
	if len(p.groupExprs) > 0 {
		decorated := own.spool()
		err := rows.each(func(row storedRow) error {
			if err := m.Step(); err != nil {
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
			return decorated.push(row)
		})
		if err != nil {
			return nil, err
		}
		rows.close()
		rows = decorated
	}
	output := own.spool()
	nspecs := uint64(len(p.aggSpecs))
	fresh := func(ordinal int64) storedRow {
		state := storedRow{IntValue(ordinal)}
		for _, spec := range p.aggSpecs {
			state = append(state, CompositeValue(blockingAccRow(newAccFromSpec(spec, env.exec.session.timeZone))))
		}
		return state
	}
	for gsi := range p.groupSets {
		gs := &p.groupSets[gsi]
		err := func() error {
			groups := newStateMap(db)
			keys := newRowSpool(db)
			seen := newStateMap(db)
			unique := newStateMap(db)
			collections := newHashRows(db)
			// The set completed (or failed): its structures are discarded.
			defer func() {
				keys.close()
				collections.close()
				unique.close()
				seen.close()
				groups.close()
			}()
			if len(gs.keyCols) == 0 {
				if err := groups.put(nil, fresh(0)); err != nil {
					return err
				}
				if err := keys.push(storedRow{}); err != nil {
					return err
				}
			}
			if err := rows.each(func(row storedRow) error {
				if err := m.Step(); err != nil {
					return err
				}
				key := make([]Value, len(gs.keyCols))
				for i, c := range gs.keyCols {
					key[i] = row[c]
				}
				if err := chargeHashKeys(m, key); err != nil {
					return err
				}
				state, ok, err := groups.get(key)
				if err != nil {
					return err
				}
				if ok {
					state = append(storedRow(nil), state...)
				} else {
					state = fresh(keys.count)
					if err := keys.push(key); err != nil {
						return err
					}
				}
				group := uint64(state[0].Int)
				for si, spec := range p.aggSpecs {
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
					owner := group*nspecs + uint64(si)
					if spec.hypo != nil {
						tuple := make(storedRow, len(spec.hypo.keys))
						for j, e := range spec.hypo.keys {
							v, err := e.eval(row, env, m)
							if err != nil {
								return err
							}
							if err := chargeSortKey(m, v, spec.hypo.sorts[j].collation); err != nil {
								return err
							}
							tuple[j] = v
						}
						if err := collections.push(owner, tuple); err != nil {
							return err
						}
						continue
					}
					v := NullValue()
					if spec.operand != nil {
						if v, err = spec.operand.eval(row, env, m); err != nil {
							return err
						}
					}
					// An in-aggregate ORDER BY's keys follow the argument (aggregates.md §22).
					var tuple []Value
					if spec.order != nil {
						if tuple, err = aggOrderTuple(spec.order, v, row, env, m); err != nil {
							return err
						}
					}
					if spec.distinct {
						if v.IsNull() && !spec.plan.distinctKeepsNull() {
							continue
						}
						if err := chargeHashKey(m, v); err != nil {
							return err
						}
						inserted, err := seen.insert([]Value{state[0], IntValue(int64(si)), v})
						if err != nil {
							return err
						}
						if !inserted {
							continue
						}
					}
					// An ordered aggregate spools its [keys…, argument] tuple; finalize sorts and folds
					// them.
					if tuple != nil {
						if err := collections.push(owner, tuple); err != nil {
							return err
						}
						continue
					}
					a := blockingAccFromRow(spec, *state[si+1].composite(), env.exec.session.timeZone)
					if err := blockingFold(a, v, m, unique, []Value{state[0], IntValue(int64(si))}, collections, owner); err != nil {
						return err
					}
					state[si+1] = CompositeValue(blockingAccRow(a))
				}
				return groups.put(key, state)
			}); err != nil {
				return err
			}
			return keys.each(func(key storedRow) error {
				state, _, err := groups.get(key)
				if err != nil {
					return err
				}
				group := uint64(state[0].Int)
				out := make(storedRow, 0, len(p.groupKeys)+len(p.aggSpecs)+len(p.groupingSpecs))
				for _, src := range gs.slotSrc {
					if src < 0 {
						out = append(out, NullValue())
					} else {
						out = append(out, key[src])
					}
				}
				for si, spec := range p.aggSpecs {
					a := blockingAccFromRow(spec, *state[si+1].composite(), env.exec.session.timeZone)
					v, err := db.finalizeSpilledAcc(a, spec, out, collections, group*nspecs+uint64(si), env, m)
					if err != nil {
						return err
					}
					out = append(out, v)
				}
				for _, positions := range p.groupingSpecs {
					out = append(out, IntValue(groupingValue(positions, gs.mask)))
				}
				return output.push(out)
			})
		}()
		if err != nil {
			return nil, err
		}
	}
	rows.close()
	return output, nil
}

// blockingFold folds one argument into a spilled aggregate's accumulator, then moves whatever the
// fold retained into the collection rows under owner, so the in-memory accumulator stays O(1). A
// _unique object aggregate's key is checked against unique (keyed by prefix + the key) — 22030 on a
// repeat.
func blockingFold(a *acc, v Value, m *costMeter, unique *stateMap, prefix []Value, collections *hashRows, owner uint64) error {
	if err := a.fold(v, m); err != nil {
		return err
	}
	if a.objAgg && a.objUnique && len(a.objPairs) > 0 {
		entry := append(append([]Value(nil), prefix...), TextValue(a.objPairs[0].key))
		inserted, err := unique.insert(entry)
		if err != nil {
			return err
		}
		if !inserted {
			return newError(DuplicateJsonObjectKeyValue, "duplicate JSON object key value")
		}
	}
	if err := blockingRetain(a, owner, collections); err != nil {
		return err
	}
	// The retained input now lives in the collection rows; drop it from the accumulator so a
	// long-lived one (an ordered aggregate's finalize fold) never re-retains it.
	a.jsonElems, a.objPairs, a.osaVals, a.osaFloats, a.arrElems, a.strRows = nil, nil, nil, nil, nil, nil
	return nil
}

// blockingRetain moves what a fold retained (JSON elements, object pairs, ordered-set values) out of
// the unpacked accumulator into its group's collection rows.
func blockingRetain(a *acc, owner uint64, collections *hashRows) error {
	for i := range a.jsonElems {
		if err := collections.push(owner, storedRow{a.jsonElems[i]}); err != nil {
			return err
		}
	}
	for _, pair := range a.objPairs {
		if err := collections.push(owner, storedRow{TextValue(pair.key), pair.val}); err != nil {
			return err
		}
	}
	for _, v := range a.arrElems {
		if err := collections.push(owner, storedRow{v}); err != nil {
			return err
		}
	}
	for _, v := range a.strRows {
		if err := collections.push(owner, storedRow(*v.composite())); err != nil {
			return err
		}
	}
	for _, v := range a.osaVals {
		if err := collections.push(owner, storedRow{v}); err != nil {
			return err
		}
	}
	for _, f := range a.osaFloats {
		if err := collections.push(owner, storedRow{Float64Value(f)}); err != nil {
			return err
		}
	}
	return nil
}

// finalizeSpilledAcc finalizes one group's accumulator, reading what it retained from its collection
// rows (owner). Ordered-set and object aggregates sort through the external sorter; a hypothetical
// DENSE_RANK dedups through a bounded map. Each is discarded once the value is computed.
func (db *engine) finalizeSpilledAcc(a *acc, spec aggSpec, srow storedRow, collections *hashRows, owner uint64, env *evalEnv, m *costMeter) (Value, error) {
	// An in-aggregate ORDER BY (aggregates.md §22): stable-sort the spooled [keys…, argument] tuples
	// by the keys (collated keys were spooled as sort-key bytes), fold the arguments in that order into
	// the plan's own accumulator — spooling its retained input into a fresh collection exactly as the
	// unordered fold does — and finalize that.
	if a.ordered {
		slots := make([]orderSlot, len(spec.order.sorts))
		for i, ks := range spec.order.sorts {
			slots[i] = orderSlot{idx: i, descending: ks.desc, nullsFirst: ks.nullsFirst}
		}
		s := db.newSorterFor(slots)
		defer s.close()
		if err := collections.each(owner, func(row storedRow) error { return s.push(row) }); err != nil {
			return NullValue(), err
		}
		sorted, err := s.finish()
		if err != nil {
			return NullValue(), err
		}
		defer sorted.close()
		inner := newAccFromSpecUnordered(spec, env.exec.session.timeZone)
		unique := newStateMap(db)
		defer unique.close()
		folded := newHashRows(db)
		defer folded.close()
		for {
			row, ok, err := sorted.next()
			if err != nil {
				return NullValue(), err
			}
			if !ok {
				break
			}
			if err := m.Step(); err != nil {
				return NullValue(), err
			}
			// A _unique key entry keeps the per-group map's (group, aggregate, key) shape, so its
			// measured bytes match every core's (memory.md §6.6).
			if err := blockingFold(inner, row[len(row)-1], m, unique, []Value{IntValue(0), IntValue(0)}, folded, 0); err != nil {
				return NullValue(), err
			}
		}
		return db.finalizeSpilledAcc(inner, spec, srow, folded, 0, env, m)
	}
	if spec.hypo != nil {
		hyp := make(storedRow, len(spec.hypo.args))
		for i, e := range spec.hypo.args {
			v, err := e.eval(srow, env, env.exec.session.scratchMeter())
			if err != nil {
				return NullValue(), err
			}
			hyp[i] = v
		}
		distinct := newStateMap(db)
		defer distinct.close()
		var n, before, le, unique int64
		err := collections.each(owner, func(row storedRow) error {
			n++
			c, err := hypoCmp(row, hyp, spec.hypo.sorts)
			if err != nil {
				return err
			}
			if c < 0 {
				before++
				le++
				if a.plan == planHypoDenseRank {
					inserted, err := distinct.insert(row)
					if err != nil {
						return err
					}
					if inserted {
						unique++
					}
				}
			} else if c == 0 {
				le++
			}
			return nil
		})
		if err != nil {
			return NullValue(), err
		}
		switch a.plan {
		case planHypoRank:
			return IntValue(before + 1), nil
		case planHypoDenseRank:
			return IntValue(unique + 1), nil
		case planHypoPercentRank:
			if n == 0 {
				return Float64Value(0), nil
			}
			return Float64Value(float64(before) / float64(n)), nil
		default:
			return Float64Value(float64(le+1) / float64(n+1)), nil
		}
	}
	switch a.plan {
	case planMode, planPercentileDisc, planPercentileCont, planPercentileContInterval:
		var fraction *Value
		if spec.osaFrac != nil {
			v, err := spec.osaFrac.eval(srow, env, env.exec.session.scratchMeter())
			if err != nil {
				return NullValue(), err
			}
			fraction = &v
		}
		return db.blockingOrderedSet(a, fraction, collections, owner)
	case planStringAgg:
		if !a.seen {
			return NullValue(), nil
		}
		var rows []Value
		if err := collections.each(owner, func(row storedRow) error { rows = append(rows, CompositeValue(row)); return nil }); err != nil {
			return NullValue(), err
		}
		return finalizeStringAgg(rows), nil
	case planArrayAgg, planArrayAggArray:
		if !a.seen {
			return NullValue(), nil
		}
		var elems []Value
		if err := collections.each(owner, func(row storedRow) error { elems = append(elems, row[0]); return nil }); err != nil {
			return NullValue(), err
		}
		return finalizeArrayAgg(a.plan, elems)
	case planJsonbAgg, planJsonAgg, planJsonbAggStrict, planJsonAggStrict:
		if !a.seen {
			return NullValue(), nil
		}
		if a.jsonAsJSON {
			breaks := jsonAggBreaksLine(a.jsonTypes[0])
			var out strings.Builder
			out.WriteByte('[')
			i := 0
			err := collections.each(owner, func(row storedRow) error {
				writeJSONAggElem(&out, i, row[0], breaks)
				i++
				return nil
			})
			if err != nil {
				return NullValue(), err
			}
			out.WriteByte(']')
			// The aggregate nests each input one level (json.md §6.4).
			if err := checkJSONTextDepth(out.String()); err != nil {
				return NullValue(), err
			}
			return JsonValue(out.String()), nil
		}
		var nodes []JsonNode
		err := collections.each(owner, func(row storedRow) error { nodes = append(nodes, *row[0].jsonb()); return nil })
		if err != nil {
			return NullValue(), err
		}
		arr := JsonNode{Kind: JArray, Arr: nodes}
		if err := checkJSONDepth(&arr); err != nil {
			return NullValue(), err
		}
		return JsonbValue(arr), nil
	case planJsonbObjectAgg, planJsonObjectAgg, planJsonbObjectAggUnique, planJsonObjectAggUnique:
		if !a.seen {
			return NullValue(), nil
		}
		if a.jsonAsJSON {
			var out strings.Builder
			out.WriteString("{ ")
			i := 0
			err := collections.each(owner, func(row storedRow) error {
				writeJSONObjectAggPair(&out, i, row[0].str(), row[1])
				i++
				return nil
			})
			if err != nil {
				return NullValue(), err
			}
			out.WriteString(" }")
			if err := checkJSONTextDepth(out.String()); err != nil {
				return NullValue(), err
			}
			return JsonValue(out.String()), nil
		}
		// The values were converted as they were folded, in input order. The sort orders keys
		// canonically (length, then bytes); within a key it is stable, so the last value wins.
		s := db.newSorterFor([]orderSlot{{idx: 0}, {idx: 1}})
		defer s.close()
		err := collections.each(owner, func(row storedRow) error {
			key := row[0].str()
			return s.push(storedRow{IntValue(int64(len(key))), TextValue(key), row[1]})
		})
		if err != nil {
			return NullValue(), err
		}
		sorted, err := s.finish()
		if err != nil {
			return NullValue(), err
		}
		defer sorted.close()
		var members []JsonMember
		for {
			row, ok, err := sorted.next()
			if err != nil {
				return NullValue(), err
			}
			if !ok {
				break
			}
			key := row[1].str()
			if len(members) > 0 && members[len(members)-1].Key == key {
				members[len(members)-1].Val = *row[2].jsonb()
				continue
			}
			members = append(members, JsonMember{Key: key, Val: *row[2].jsonb()})
		}
		out := makeObject(members)
		if err := checkJSONDepth(&out); err != nil {
			return NullValue(), err
		}
		return JsonbValue(out), nil
	}
	return a.finalize()
}

// blockingOrderedSet sorts a group's retained ordered-set inputs through the external sorter (a
// collated key decorated with its sort key), then computes mode in one pass or spools the sorted
// values for the percentile lookups.
func (db *engine) blockingOrderedSet(a *acc, fraction *Value, collections *hashRows, owner uint64) (Value, error) {
	keySlot := 0
	if a.osaCollation != nil {
		keySlot = 1
	}
	s := db.newSorterFor([]orderSlot{{idx: keySlot, descending: a.osaDesc}})
	defer s.close()
	err := collections.each(owner, func(row storedRow) error {
		if a.osaCollation != nil {
			b, err := collationSortKey(a.osaCollation, row[0])
			if err != nil {
				return err
			}
			row = storedRow{row[0], ByteaValue(b)}
		}
		return s.push(row)
	})
	if err != nil {
		return NullValue(), err
	}
	sorted, err := s.finish()
	if err != nil {
		return NullValue(), err
	}
	defer sorted.close()
	if a.plan == planMode {
		best, current := NullValue(), NullValue()
		var bestCount, run int64
		for {
			row, ok, err := sorted.next()
			if err != nil {
				return NullValue(), err
			}
			if !ok {
				break
			}
			v := row[0]
			if run > 0 && valueCmp(v, current) == 0 {
				run++
			} else {
				current = v
				run = 1
			}
			if run > bestCount {
				best, bestCount = current, run
			}
		}
		return best, nil
	}
	ordered := newRowSpool(db)
	defer ordered.close()
	for {
		row, ok, err := sorted.next()
		if err != nil {
			return NullValue(), err
		}
		if !ok {
			break
		}
		if err := ordered.push(row); err != nil {
			return NullValue(), err
		}
	}
	n := ordered.count
	return finalizePercentile(fraction, n == 0, func(p float64) (Value, error) {
		first, second := int64(0), int64(0)
		position := float64(0)
		if a.plan == planPercentileDisc {
			first = max(0, int64(math.Ceil(p*float64(n)))-1)
			first = min(first, n-1)
			second = first
		} else {
			position = p * float64(n-1)
			first = int64(math.Floor(position))
			second = int64(math.Ceil(position))
		}
		lo, hi := NullValue(), NullValue()
		var index int64
		err := ordered.each(func(row storedRow) error {
			if index == first {
				lo = row[0]
			}
			if index == second {
				hi = row[0]
				return errBlockingWindowDone
			}
			index++
			return nil
		})
		if err != nil && err != errBlockingWindowDone {
			return NullValue(), err
		}
		if first == second || a.plan == planPercentileDisc {
			return lo, nil
		}
		if a.plan == planPercentileContInterval {
			v, err := intervalLerp(expectInterval(lo), expectInterval(hi), position-float64(first))
			if err != nil {
				return NullValue(), err
			}
			return IntervalValue(v), nil
		}
		return Float64Value(lo.F64() + (position-float64(first))*(hi.F64()-lo.F64())), nil
	})
}
