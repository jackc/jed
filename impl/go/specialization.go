//go:build jed_specialization

package jed

// G1 patterns are terminals of the bounded G2 composition. All descriptors
// are immutable before the existing atomic plan-cache publication.
const specializationMaxRoots = 8

type specializationPlan struct {
	kernels *scanKernels
}

type scanKernels struct {
	filter  *scalarKernel
	project [specializationMaxRoots]*scalarKernel
	// Reasons are bounded diagnostics, including rejection (no retry per row).
	reasons [specializationMaxRoots + 1]string
}

type kernelLeaf struct {
	kind  rExprKind
	index int
	n     int64
}

type kernelOperand struct {
	arith       bool
	op          binaryOp
	width       scalarType
	charge      int64
	left, right kernelLeaf
}

type scalarKernel struct {
	children    [3]*scalarKernel
	run         kernelFunc
	leaf        bool
	kind        rExprKind
	op          binaryOp
	width       scalarType
	charge      int64
	negated     bool
	left, right kernelOperand
}

func primitiveKernelType(t colType) rtKind {
	if t.Composite || t.Elem != nil || t.RangeElem != nil {
		return rtText
	}
	if t.Scalar.IsInteger() {
		return rtInt
	}
	if t.Scalar.IsBool() {
		return rtBool
	}
	return rtText // unsupported sentinel, never admitted
}

// Read declared column and finalized parameter types, never runtime values. In
// particular date-date has an integer result but is not integer arithmetic.
func bindKernelLeaf(e *rExpr, columns []catColumn, params []colType) (kernelLeaf, rtKind, bool) {
	if e == nil {
		return kernelLeaf{}, rtNull, false
	}
	l := kernelLeaf{kind: e.kind, index: e.index}
	var ty rtKind
	switch e.kind {
	case reColumn:
		if e.index < 0 || e.index >= len(columns) {
			return l, rtNull, false
		}
		t := columns[e.index].Type
		if !t.isScalar() {
			return l, rtNull, false
		}
		ty = primitiveKernelType(colType{Scalar: t.ScalarTy()})
	case reParam:
		if e.index < 0 || e.index >= len(params) {
			return l, rtNull, false
		}
		ty = primitiveKernelType(params[e.index])
	case reConstInt:
		l.n, ty = e.cInt, rtInt
	case reConstBool:
		if e.cBool {
			l.n = 1
		}
		ty = rtBool
	case reConstNull:
		ty = rtNull
	default:
		return l, rtNull, false
	}
	return l, ty, ty == rtInt || ty == rtBool || ty == rtNull
}

func kernelArithmetic(op binaryOp) bool {
	return op == opAdd || op == opSub || op == opMul || op == opDiv || op == opMod
}

func bindKernelOperand(e *rExpr, columns []catColumn, params []colType) (kernelOperand, rtKind, bool) {
	l, ty, ok := bindKernelLeaf(e, columns, params)
	if ok {
		return kernelOperand{left: l}, ty, true
	}
	if e == nil || e.kind != reArith || !e.result.IsInteger() || !kernelArithmetic(e.op) {
		return kernelOperand{}, rtNull, false
	}
	l, lt, lok := bindKernelLeaf(e.lhs, columns, params)
	r, rt, rok := bindKernelLeaf(e.rhs, columns, params)
	if !lok || !rok || (lt != rtInt && lt != rtNull) || (rt != rtInt && rt != rtNull) {
		return kernelOperand{}, rtNull, false
	}
	return kernelOperand{arith: true, op: e.op, width: e.result, charge: operatorCost(e.op.catalogName()), left: l, right: r}, rtInt, true
}

func bindStaticKernel(e *rExpr, columns []catColumn, params []colType) *scalarKernel {
	if e == nil {
		return nil
	}
	k := scalarKernel{kind: e.kind, op: e.op, width: e.result, negated: e.negated}
	switch e.kind {
	case reArith, reCompare:
		if e.kind == reArith && (!e.result.IsInteger() || !kernelArithmetic(e.op)) {
			return nil
		}
		if e.kind == reCompare && (e.collation != nil || e.op < opEq || e.op > opGe) {
			return nil
		}
		l, lt, lok := bindKernelOperand(e.lhs, columns, params)
		r, rt, rok := bindKernelOperand(e.rhs, columns, params)
		if !lok || !rok || (lt != rtInt && lt != rtNull) || (rt != rtInt && rt != rtNull) {
			return nil
		}
		k.left, k.right, k.charge = l, r, operatorCost(e.op.catalogName())
	case reIsNull, reNot:
		l, ty, ok := bindKernelLeaf(e.operand, columns, params)
		if !ok || (e.kind == reNot && ty != rtBool && ty != rtNull) {
			return nil
		}
		k.left = kernelOperand{left: l}
		k.charge = costs.OperatorEval
		if e.kind == reNot {
			k.charge = operatorCost("not")
		}
	default:
		return nil // composed forms are handled by the bounded G2 binder
	}
	return &k
}

func (db *engine) prepareSpecialization(p *selectPlan, params []colType) {
	// The publication sites call this only on newly planned trees. Resetting also
	// drops inherited annotations if a planner rewrite copied a selectPlan.
	p.kernels = nil
	if !pullStreamingScanEligible(p) || p.distinct || !db.planCacheable(p) {
		return
	}
	// Selective retains all bounded scans (including point lookups) as controls.
	if specializationMode != "forced" && p.phys.relBounds[0] != nil {
		return
	}
	if len(p.projections) > specializationMaxRoots-1 {
		return
	}
	table, ok := db.lkpTableScoped(p.rels[0].db, p.rels[0].tableName)
	if !ok {
		return
	}
	ks := &scanKernels{}
	bind := func(e *rExpr, slot int) *scalarKernel {
		k := bindScalarKernel(e, table.Columns, params)
		ks.reasons[slot] = "unsupported root"
		if e == nil {
			ks.reasons[slot] = "absent"
		}
		if k != nil {
			ks.reasons[slot] = "bounded G1/G2 kernel"
		}
		return k
	}
	ks.filter = bind(p.filter, 0)
	for i, e := range p.projections {
		ks.project[i] = bind(e, i+1)
	}
	p.kernels = ks
}

func (l *kernelLeaf) load(row storedRow, env *evalEnv, m *costMeter) (Value, error) {
	if err := m.Guard(); err != nil {
		return Value{}, err
	}
	switch l.kind {
	case reColumn:
		v := row[l.index]
		if v.Kind == ValUnfetched {
			return resolveUnfetchedSelf(v.unfetched())
		}
		return v, nil
	case reParam:
		return env.params[l.index], nil
	case reConstInt:
		return IntValue(l.n), nil
	case reConstBool:
		return BoolValue(l.n != 0), nil
	default:
		return NullValue(), nil
	}
}

func (o *kernelOperand) eval(row storedRow, env *evalEnv, m *costMeter) (Value, error) {
	if !o.arith {
		return o.left.load(row, env, m)
	}
	if err := m.Guard(); err != nil {
		return Value{}, err
	}
	m.Charge(o.charge)
	a, err := o.left.load(row, env, m)
	if err != nil {
		return Value{}, err
	}
	b, err := o.right.load(row, env, m)
	if err != nil {
		return Value{}, err
	}
	if a.IsNull() || b.IsNull() {
		return NullValue(), nil
	}
	return evalArith(o.op, a.Int, b.Int, o.width)
}

func (k *scalarKernel) eval(row storedRow, env *evalEnv, m *costMeter) (Value, error) {
	specializationInvoked()
	return k.evalMode(row, env, m, false)
}

func (k *scalarKernel) evalMode(row storedRow, env *evalEnv, m *costMeter, filter bool) (Value, error) {
	if specializationClosures {
		return k.run(row, env, m, filter)
	}
	if k.leaf {
		return k.left.left.load(row, env, m)
	}
	if k.children[0] != nil {
		return k.evalComposition(row, env, m, filter)
	}
	if err := m.Guard(); err != nil {
		return Value{}, err
	}
	m.Charge(k.charge)
	a, err := k.left.eval(row, env, m)
	if err != nil {
		return Value{}, err
	}
	if k.kind == reIsNull {
		return BoolValue(a.IsNull() != k.negated), nil
	}
	if k.kind == reNot {
		return boolNot(a), nil
	}
	b, err := k.right.eval(row, env, m)
	if err != nil {
		return Value{}, err
	}
	if k.kind == reArith {
		if a.IsNull() || b.IsNull() {
			return NullValue(), nil
		}
		return evalArith(k.op, a.Int, b.Int, k.width)
	}
	return compareKernel(k.op, a, b, m)
}

func compareKernel(op binaryOp, a, b Value, m *costMeter) (Value, error) {
	// Preserve BOTH zero-charge/guard sites of the ordinary comparator, including
	// ceilingHit bookkeeping. No cancellation checks occur at expression nodes.
	m.Charge(0)
	if err := m.Guard(); err != nil {
		return Value{}, err
	}
	m.Charge(0)
	if err := m.Guard(); err != nil {
		return Value{}, err
	}
	if a.IsNull() || b.IsNull() {
		return NullValue(), nil
	}
	var result bool
	switch op {
	case opEq:
		result = a.Int == b.Int
	case opNe:
		result = a.Int != b.Int
	case opLt:
		result = a.Int < b.Int
	case opGt:
		result = a.Int > b.Int
	case opLe:
		result = a.Int <= b.Int
	case opGe:
		result = a.Int >= b.Int
	}
	return BoolValue(result), nil
}

// Keep selection inline so unsupported roots pay no extra evaluator call. The
// untagged build folds these helpers to nil and retains the original row loop.
func (p *selectPlan) filterKernel() *scalarKernel {
	if specializationMode != "ordinary" && p.kernels != nil {
		return p.kernels.filter
	}
	return nil
}

func (p *selectPlan) projectionKernel(i int) *scalarKernel {
	if specializationMode != "ordinary" && p.kernels != nil {
		return p.kernels.project[i]
	}
	return nil
}
