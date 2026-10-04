package jed

import (
	"bytes"
	"math"
	"strings"
)

func blockingValues(values *boundedJoinTable, key []byte, fn func(storedRow) error) error {
	return values.each(key, func(k []byte, row storedRow) error {
		if !bytes.Equal(k, key) {
			return nil
		}
		return fn(row)
	})
}

func blockingObjectKey(key []byte, name string) string {
	return distinctRowKey([]Value{ByteaValue(key), TextValue(name)})
}

func blockingFoldCollection(a *acc, spec aggSpec, v Value, key []byte, values *boundedJoinTable, seen *boundedMap, m *costMeter) (bool, error) {
	switch a.plan {
	case planMode, planPercentileDisc, planPercentileCont, planPercentileContInterval:
		if v.IsNull() {
			return true, nil
		}
		if a.plan == planPercentileCont {
			f, err := percentileInputF64(v)
			if err != nil {
				return true, err
			}
			v = Float64Value(f)
		}
		a.count++
		return true, values.add(key, storedRow{v})
	case planJsonbAgg, planJsonAgg, planJsonbAggStrict, planJsonAggStrict, planJsonbObjectAgg, planJsonObjectAgg, planJsonbObjectAggUnique, planJsonObjectAggUnique:
		one := newAccFromSpec(spec)
		if err := one.fold(v, m); err != nil {
			return true, err
		}
		a.seen = a.seen || one.seen
		if one.objAgg {
			pair := one.objPairs[0]
			dk := blockingObjectKey(key, pair.key)
			_, had, err := seen.get(dk)
			if err != nil {
				return true, err
			}
			if one.objUnique && had {
				return true, newError(DuplicateJsonObjectKeyValue, "duplicate JSON object key value")
			}
			if one.objUnique || !one.jsonAsJSON {
				if err := seen.put(dk, storedRow{pair.val}); err != nil {
					return true, err
				}
			}
			// JSONB keeps last-wins values in bounded state. The replay log still
			// carries duplicates so finalization validates them in input order.

			return true, values.add(key, storedRow{TextValue(pair.key), pair.val})
		}
		if len(one.jsonNodes) == 0 {
			return true, nil
		}
		return true, values.add(key, storedRow{JsonbValue(one.jsonNodes[0])})
	}
	return false, nil
}

func (db *engine) blockingFinalizeCollection(a *acc, spec aggSpec, key []byte, values *boundedJoinTable, seen *boundedMap, group storedRow, env *evalEnv) (Value, error) {
	if spec.hypo != nil {
		hyp := make(storedRow, len(spec.hypo.args))
		for i, e := range spec.hypo.args {
			v, err := e.eval(group, env, env.exec.session.scratchMeter())
			if err != nil {
				return NullValue(), err
			}
			hyp[i] = v
		}
		var before, le, unique int64
		distinct := newBoundedMap(db)
		defer distinct.close()
		err := blockingValues(values, key, func(row storedRow) error {
			c, err := hypoCmp(row, hyp, spec.hypo.sorts)
			if err != nil {
				return err
			}
			if c < 0 {
				before++
				le++
				if a.plan == planHypoDenseRank {
					dk := distinctRowKey(row)
					_, had, err := distinct.get(dk)
					if err != nil {
						return err
					}
					if !had {
						unique++
						return distinct.put(dk, nil)
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
			if a.count == 0 {
				return Float64Value(0), nil
			}
			return Float64Value(float64(before) / float64(a.count)), nil
		default:
			return Float64Value(float64(le+1) / float64(a.count+1)), nil
		}
	}
	switch a.plan {
	case planMode, planPercentileDisc, planPercentileCont, planPercentileContInterval:
		return db.blockingOrderedSet(a, key, values)
	case planJsonbAgg, planJsonAgg, planJsonbAggStrict, planJsonAggStrict:
		if !a.seen {
			return NullValue(), nil
		}
		if a.jsonAsJSON {
			var out strings.Builder
			out.WriteByte('[')
			first := true
			err := blockingValues(values, key, func(row storedRow) error {
				if !first {
					out.WriteString(", ")
				}
				first = false
				out.WriteString(jsonbOut(row[0].jsonb()))
				return nil
			})
			if err != nil {
				return NullValue(), err
			}
			out.WriteByte(']')
			return JsonValue(out.String()), nil
		}
		var nodes []JsonNode
		err := blockingValues(values, key, func(row storedRow) error { nodes = append(nodes, *row[0].jsonb()); return nil })
		if err != nil {
			return NullValue(), err
		}
		return JsonbValue(JsonNode{Kind: JArray, Arr: nodes}), nil
	case planJsonbObjectAgg, planJsonObjectAgg, planJsonbObjectAggUnique, planJsonObjectAggUnique:
		if !a.seen {
			return NullValue(), nil
		}
		if a.jsonAsJSON {
			var out strings.Builder
			out.WriteString("{ ")
			first := true
			err := blockingValues(values, key, func(row storedRow) error {
				img, err := elemJsonText(row[1])
				if err != nil {
					return err
				}
				if !first {
					out.WriteString(", ")
				}
				first = false
				n := JsonNode{Kind: JString, S: row[0].str()}
				out.WriteString(jsonCompactOut(&n))
				out.WriteString(" : ")
				out.WriteString(img)
				return nil
			})
			if err != nil {
				return NullValue(), err
			}
			out.WriteString(" }")
			return JsonValue(out.String()), nil
		}
		// Validate every source value in input order, including overwritten
		// duplicates, before reducing last-wins JSONB members. Validation errors
		// retain the old finalize order without retaining duplicate nodes.
		if err := blockingValues(values, key, func(row storedRow) error { _, err := valueToNode(row[1]); return err }); err != nil {
			return NullValue(), err
		}
		emitted := newBoundedMap(db)
		defer emitted.close()
		var members []JsonMember
		err := blockingValues(values, key, func(row storedRow) error {
			dk := row[0].str()
			_, had, err := emitted.get(dk)
			if err != nil {
				return err
			}
			if had {
				return nil
			}
			if err = emitted.put(dk, nil); err != nil {
				return err
			}
			last, _, err := seen.get(blockingObjectKey(key, row[0].str()))
			if err != nil {
				return err
			}
			node, err := valueToNode(last[0])
			if err != nil {
				return err
			}
			members = append(members, JsonMember{Key: row[0].str(), Val: node})
			return nil
		})
		if err != nil {
			return NullValue(), err
		}
		return JsonbValue(makeObject(members)), nil
	}
	return a.finalize()
}

func (db *engine) blockingOrderedSet(a *acc, key []byte, values *boundedJoinTable) (Value, error) {
	input := newRowSpool(db)
	defer input.close()
	if err := blockingValues(values, key, func(row storedRow) error { return input.push(row) }); err != nil {
		return NullValue(), err
	}
	sorted, err := db.blockingSort(input, []orderSlot{{idx: 0, descending: a.osaDesc, collation: a.osaCollation}})
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
		if err := ordered.push(storedRow{row[0]}); err != nil {
			return NullValue(), err
		}
	}
	return finalizePercentile(a.osaFrac, a.count == 0, func(p float64) (Value, error) {
		first, second := int64(0), int64(0)
		position := float64(0)
		if a.plan == planPercentileDisc {
			first = max(0, int64(math.Ceil(p*float64(a.count)))-1)
			first = min(first, a.count-1)
			second = first
		} else {
			position = p * float64(a.count-1)
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
