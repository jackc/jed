//go:build jed_specialization

package jed

import (
	"reflect"
	"testing"
)

func TestSpecializationCompositionPrefixes(t *testing.T) {
	cols := []catColumn{{Type: dataType{Scalar: scalarBool}}, {Type: dataType{Scalar: scalarBool}}, {Type: dataType{Scalar: scalarInt32}}}
	col := func(i int) *rExpr { return &rExpr{kind: reColumn, index: i} }
	null := &rExpr{kind: reConstNull}
	trap := &rExpr{kind: reArith, op: opDiv, result: scalarInt32, lhs: &rExpr{kind: reConstInt, cInt: 1}, rhs: col(2)}
	cmp := &rExpr{kind: reCompare, op: opGt, lhs: trap, rhs: &rExpr{kind: reConstInt}}
	and := &rExpr{kind: reAnd, lhs: col(0), rhs: col(1)}
	or := &rExpr{kind: reOr, lhs: col(0), rhs: cmp}
	roots := []*rExpr{
		and, or,
		{kind: reNot, operand: and},
		{kind: reIsNull, operand: or},
		{kind: reAnd, lhs: col(0), rhs: cmp},
		{kind: reAnd, lhs: and, rhs: or},
		{kind: reCase, caseArms: []rCaseArm{{cond: and, result: trap}}, caseEls: col(2)},
		{kind: reCase, caseArms: []rCaseArm{{cond: or, result: col(0)}}, caseEls: col(1)},
		{kind: reCoalesce, sargs: []*rExpr{col(2), trap}},
		{kind: reCoalesce, sargs: []*rExpr{null, trap}},
		{kind: reCoalesce, sargs: []*rExpr{and, or}},
	}
	for _, e := range roots {
		_, ty := bindComposedKernel(e, cols, nil)
		k := bindScalarKernel(e, cols, nil)
		if k == nil {
			t.Fatal("composition rejected", e.kind)
		}
		for _, a := range []Value{NullValue(), BoolValue(false), BoolValue(true)} {
			for _, b := range []Value{NullValue(), BoolValue(false), BoolValue(true)} {
				for _, n := range []Value{NullValue(), IntValue(0), IntValue(1), IntValue(-1)} {
					for limit := int64(0); limit <= 12; limit++ {
						for lifeLimit := int64(0); lifeLimit <= 15; lifeLimit++ {
							for _, filter := range []bool{false, true} {
								// Integer roots are values only. Filter context would not be resolved.
								if filter && ty != rtBool {
									continue
								}
								life1, life2 := int64(3), int64(3)
								m1, m2 := newMeterWithLimit(limit), newMeterWithLimit(limit)
								m1.lifetimeTotal, m2.lifetimeTotal = &life1, &life2
								m1.lifetimeLimit, m2.lifetimeLimit = lifeLimit, lifeLimit
								row := storedRow{a, b, n}
								env := &evalEnv{}
								var v1, v2 Value
								var err1, err2 error
								if filter {
									var b1, b2 bool
									b1, err1 = e.evalFilter(row, env, m1)
									b2, err2 = k.evalFilter(row, env, m2)
									v1, v2 = BoolValue(b1), BoolValue(b2)
								} else {
									v1, err1 = e.eval(row, env, m1)
									v2, err2 = k.eval(row, env, m2)
								}
								if !reflect.DeepEqual(v1, v2) || !reflect.DeepEqual(err1, err2) || m1.Accrued != m2.Accrued || life1 != life2 || m1.ceilingHit != m2.ceilingHit {
									t.Fatalf("kind=%v filter=%v row=%v limit=%d/%d: %v/%v %v/%v costs=%d/%d", e.kind, filter, row, limit, lifeLimit, v1, v2, err1, err2, m1.Accrued, m2.Accrued)
								}
							}
						}
					}
				}
			}
		}
	}
}

func TestSpecializationCompositionBounds(t *testing.T) {
	leaf := func() *rExpr { return &rExpr{kind: reConstBool, cBool: true} }
	e := leaf()
	for depth := 1; depth <= specializationMaxDepth+1; depth++ {
		k := bindScalarKernel(e, nil, nil)
		if (k != nil) != (depth <= specializationMaxDepth) {
			t.Fatalf("depth %d", depth)
		}
		if k != nil {
			v, err := k.eval(nil, &evalEnv{}, newMeter())
			if err != nil || v.Kind != ValBool {
				t.Fatal(v, err)
			}
		}
		e = &rExpr{kind: reNot, operand: e}
	}
	var tree func(int) *rExpr
	tree = func(nodes int) *rExpr {
		if nodes == 1 {
			return leaf()
		}
		if nodes%2 == 0 {
			return &rExpr{kind: reNot, operand: tree(nodes - 1)}
		}
		left := (nodes - 1) / 2
		return &rExpr{kind: reAnd, lhs: tree(left), rhs: tree(nodes - 1 - left)}
	}
	for _, nodes := range []int{191, 192, 193, 1537} {
		e := tree(nodes)
		k := bindScalarKernel(e, nil, nil)
		if (k != nil) != (nodes <= specializationMaxNodes) {
			t.Fatalf("nodes %d", nodes)
		}
		if k == nil {
			if got := testing.AllocsPerRun(10, func() { bindScalarKernel(e, nil, nil) }); got != 0 {
				t.Fatalf("oversize allocates %g", got)
			}
		} else {
			env, m := &evalEnv{}, newMeter()
			if got := testing.AllocsPerRun(10, func() { _, _ = k.eval(nil, env, m) }); got != 0 {
				t.Fatalf("composition allocates %g per row", got)
			}
		}
	}
}
