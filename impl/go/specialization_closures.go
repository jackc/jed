//go:build jed_specialization

package jed

// One closure per source node, selected at preparation, over exactly the static
// binder's admitted surface. Captures contain scalar descriptors/child functions
// only. Unlike the static control, G1 fused patterns expand to individual nodes.
func closeLeaf(l kernelLeaf) kernelFunc {
	return func(row storedRow, env *evalEnv, m *costMeter, _ bool) (Value, error) { return l.load(row, env, m) }
}

func closeArithmetic(op binaryOp, width scalarType, charge int64, left, right kernelFunc) kernelFunc {
	return func(row storedRow, env *evalEnv, m *costMeter, _ bool) (Value, error) {
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		m.Charge(charge)
		a, err := left(row, env, m, false)
		if err != nil {
			return Value{}, err
		}
		b, err := right(row, env, m, false)
		if err != nil {
			return Value{}, err
		}
		if a.IsNull() || b.IsNull() {
			return NullValue(), nil
		}
		return evalArith(op, a.Int, b.Int, width)
	}
}

func closeOperand(o kernelOperand) kernelFunc {
	if !o.arith {
		return closeLeaf(o.left)
	}
	return closeArithmetic(o.op, o.width, o.charge, closeLeaf(o.left), closeLeaf(o.right))
}

func closeKernel(k *scalarKernel) kernelFunc {
	if k.leaf {
		return closeLeaf(k.left.left)
	}
	kind, op, charge, negated := k.kind, k.op, k.charge, k.negated
	var left, right, third kernelFunc
	if k.children[0] != nil {
		left = closeKernel(k.children[0])
		if k.children[1] != nil {
			right = closeKernel(k.children[1])
		}
		if k.children[2] != nil {
			third = closeKernel(k.children[2])
		}
	} else {
		left = closeOperand(k.left)
		if kind == reArith || kind == reCompare {
			right = closeOperand(k.right)
		}
	}
	switch kind {
	case reArith:
		return closeArithmetic(op, k.width, charge, left, right)
	case reCompare:
		return func(row storedRow, env *evalEnv, m *costMeter, _ bool) (Value, error) {
			if err := m.Guard(); err != nil {
				return Value{}, err
			}
			m.Charge(charge)
			a, err := left(row, env, m, false)
			if err != nil {
				return Value{}, err
			}
			b, err := right(row, env, m, false)
			if err != nil {
				return Value{}, err
			}
			return compareKernel(op, a, b, m)
		}
	case reAnd:
		return func(row storedRow, env *evalEnv, m *costMeter, filter bool) (Value, error) {
			if err := m.Guard(); err != nil {
				return Value{}, err
			}
			m.Charge(charge)
			a, err := left(row, env, m, filter)
			if err != nil {
				return Value{}, err
			}
			if filter && !a.IsTrue() {
				return BoolValue(false), nil
			}
			if to3(a) == False {
				return a, nil
			}
			b, err := right(row, env, m, filter)
			if err != nil {
				return Value{}, err
			}
			if filter {
				return BoolValue(b.IsTrue()), nil
			}
			return boolAnd(a, b), nil
		}
	case reOr:
		return func(row storedRow, env *evalEnv, m *costMeter, _ bool) (Value, error) {
			if err := m.Guard(); err != nil {
				return Value{}, err
			}
			m.Charge(charge)
			a, err := left(row, env, m, false)
			if err != nil {
				return Value{}, err
			}
			if to3(a) == True {
				return a, nil
			}
			b, err := right(row, env, m, false)
			if err != nil {
				return Value{}, err
			}
			return boolOr(a, b), nil
		}
	case reNot:
		return func(row storedRow, env *evalEnv, m *costMeter, _ bool) (Value, error) {
			if err := m.Guard(); err != nil {
				return Value{}, err
			}
			m.Charge(charge)
			a, err := left(row, env, m, false)
			if err != nil {
				return Value{}, err
			}
			return boolNot(a), nil
		}
	case reIsNull:
		return func(row storedRow, env *evalEnv, m *costMeter, _ bool) (Value, error) {
			if err := m.Guard(); err != nil {
				return Value{}, err
			}
			m.Charge(charge)
			a, err := left(row, env, m, false)
			if err != nil {
				return Value{}, err
			}
			return BoolValue(a.IsNull() != negated), nil
		}
	case reCase:
		return func(row storedRow, env *evalEnv, m *costMeter, _ bool) (Value, error) {
			if err := m.Guard(); err != nil {
				return Value{}, err
			}
			m.Charge(charge)
			a, err := left(row, env, m, false)
			if err != nil {
				return Value{}, err
			}
			if a.IsTrue() {
				return right(row, env, m, false)
			}
			return third(row, env, m, false)
		}
	case reCoalesce:
		return func(row storedRow, env *evalEnv, m *costMeter, _ bool) (Value, error) {
			if err := m.Guard(); err != nil {
				return Value{}, err
			}
			m.Charge(charge)
			a, err := left(row, env, m, false)
			if err != nil {
				return Value{}, err
			}
			if !a.IsNull() {
				return a, nil
			}
			return right(row, env, m, false)
		}
	}
	panic("invalid closure kernel")
}
