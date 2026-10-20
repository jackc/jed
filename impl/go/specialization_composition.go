//go:build jed_specialization

package jed

// Limits are checked before constructing any descriptor/closure. Children of a
// rejected root are never executed. No source node or execution state is retained.
const (
	specializationMaxNodes = 192
	specializationMaxDepth = 32
)

type kernelFunc func(storedRow, *evalEnv, *costMeter, bool) (Value, error)

func boundedKernelShape(e *rExpr, depth int, nodes *int) bool {
	if e == nil || depth > specializationMaxDepth || *nodes >= specializationMaxNodes {
		return false
	}
	*nodes++
	child := func(c *rExpr) bool { return boundedKernelShape(c, depth+1, nodes) }
	switch e.kind {
	case reColumn, reParam, reConstInt, reConstBool, reConstNull:
		return true
	case reArith, reCompare, reAnd, reOr:
		return child(e.lhs) && child(e.rhs)
	case reNot, reIsNull:
		return child(e.operand)
	case reCase:
		return !e.caseDecimal && len(e.caseArms) == 1 && child(e.caseArms[0].cond) && child(e.caseArms[0].result) && child(e.caseEls)
	case reCoalesce:
		return !e.caseDecimal && len(e.sargs) == 2 && child(e.sargs[0]) && child(e.sargs[1])
	}
	return false
}

func bindScalarKernel(e *rExpr, columns []catColumn, params []colType) *scalarKernel {
	nodes := 0
	if !boundedKernelShape(e, 1, &nodes) {
		return nil
	}
	k, _ := bindComposedKernel(e, columns, params)
	if k != nil && specializationClosures {
		// The closure build retains only closures, not a second descriptor graph.
		run := closeKernel(k)
		return &scalarKernel{run: run}
	}
	return k
}

func bindComposedKernel(e *rExpr, columns []catColumn, params []colType) (*scalarKernel, rtKind) {
	if k := bindStaticKernel(e, columns, params); k != nil {
		if e.kind == reArith {
			return k, rtInt
		}
		return k, rtBool
	}
	if leaf, ty, ok := bindKernelLeaf(e, columns, params); ok {
		return &scalarKernel{leaf: true, left: kernelOperand{left: leaf}}, ty
	}
	var source [3]*rExpr
	k := &scalarKernel{kind: e.kind, negated: e.negated, charge: costs.OperatorEval}
	switch e.kind {
	case reAnd, reOr:
		source[0], source[1] = e.lhs, e.rhs
		if e.kind == reAnd {
			k.charge = operatorCost("and")
		} else {
			k.charge = operatorCost("or")
		}
	case reNot, reIsNull:
		source[0] = e.operand
		if e.kind == reNot {
			k.charge = operatorCost("not")
		}
	case reCase:
		source[0], source[1], source[2] = e.caseArms[0].cond, e.caseArms[0].result, e.caseEls
	case reCoalesce:
		source[0], source[1] = e.sargs[0], e.sargs[1]
	default:
		return nil, rtNull
	}
	var types [3]rtKind
	for i, child := range source {
		if child == nil {
			continue
		}
		k.children[i], types[i] = bindComposedKernel(child, columns, params)
		if k.children[i] == nil {
			return nil, rtNull
		}
	}
	boolean := func(t rtKind) bool { return t == rtBool || t == rtNull }
	switch e.kind {
	case reAnd, reOr:
		if !boolean(types[0]) || !boolean(types[1]) {
			return nil, rtNull
		}
		return k, rtBool
	case reNot:
		if !boolean(types[0]) {
			return nil, rtNull
		}
		return k, rtBool
	case reIsNull:
		return k, rtBool
	case reCase:
		if !boolean(types[0]) {
			return nil, rtNull
		}
		types[0], types[1] = types[1], types[2]
	}
	// CASE/COALESCE have no coercion on the admitted integer/boolean surface.
	if types[0] == rtNull {
		return k, types[1]
	}
	if types[1] == rtNull || types[0] == types[1] {
		return k, types[0]
	}
	return nil, rtNull
}

func (k *scalarKernel) evalFilter(row storedRow, env *evalEnv, m *costMeter) (bool, error) {
	specializationInvoked()
	v, err := k.evalMode(row, env, m, true)
	return v.IsTrue(), err
}

func (k *scalarKernel) evalComposition(row storedRow, env *evalEnv, m *costMeter, filter bool) (Value, error) {
	if err := m.Guard(); err != nil {
		return Value{}, err
	}
	m.Charge(k.charge)
	// Only an AND in filter context propagates that context to its children.
	childFilter := filter && k.kind == reAnd
	a, err := k.children[0].evalMode(row, env, m, childFilter)
	if err != nil {
		return Value{}, err
	}
	switch k.kind {
	case reAnd:
		if childFilter && !a.IsTrue() {
			return BoolValue(false), nil
		}
		if to3(a) == False {
			return a, nil
		}
		b, err := k.children[1].evalMode(row, env, m, childFilter)
		if err != nil {
			return Value{}, err
		}
		if childFilter {
			return BoolValue(b.IsTrue()), nil
		}
		return boolAnd(a, b), nil
	case reOr:
		if to3(a) == True {
			return a, nil
		}
		b, err := k.children[1].evalMode(row, env, m, false)
		if err != nil {
			return Value{}, err
		}
		return boolOr(a, b), nil
	case reNot:
		return boolNot(a), nil
	case reIsNull:
		return BoolValue(a.IsNull() != k.negated), nil
	case reCase:
		if a.IsTrue() {
			return k.children[1].evalMode(row, env, m, false)
		}
		return k.children[2].evalMode(row, env, m, false)
	case reCoalesce:
		if !a.IsNull() {
			return a, nil
		}
		return k.children[1].evalMode(row, env, m, false)
	}
	panic("invalid composition")
}
