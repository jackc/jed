//go:build expression_vm

package jed

// This is deliberately an experiment, not an engine setting or a persisted format.
// See spec/design/expression-vm-experiment.md. Its cache is owned by the experiment
// process and reset between workloads; entries retain source pointers, preventing
// address reuse. Programs are immutable and execution registers belong to the meter.
import (
	"math"
	"sync"
	"sync/atomic"
)

type expressionVMType struct{ vmLeafType uint8 }
type expressionVMState struct {
	vmRegisters []Value
	vmTree      bool
}

func (e *rExpr) setExpressionVMType(t resolvedType) {
	switch t.kind {
	case rtInt:
		e.vmLeafType = 1
	case rtBool:
		e.vmLeafType = 2
	case rtNull:
		e.vmLeafType = 3
	}
}

var expressionVMRun atomic.Bool
var expressionVMCache sync.Map
var expressionVMCompiled atomic.Int64
var expressionVMFallbacks atomic.Int64
var expressionVMInstructions atomic.Int64

// SetExpressionVMExperimentForTooling selects and resets the opt-in evaluator.
// It exists only in expression_vm builds; the host harness calls it between
// workloads with no active executions. Ordinary builds expose no experiment API.
func SetExpressionVMExperimentForTooling(enabled bool) {
	expressionVMRun.Store(enabled)
	resetExpressionVM()
}

func (m *costMeter) expressionVMActive() bool { return expressionVMRun.Load() && !m.vmTree }

// Reset is called only when the serial experiment driver has no active executions.
func resetExpressionVM() {
	expressionVMCache.Clear()
	expressionVMCompiled.Store(0)
	expressionVMFallbacks.Store(0)
	expressionVMInstructions.Store(0)
}

type expressionVMKey struct {
	expr   *rExpr
	filter bool
}
type expressionVMOp uint8

const (
	vmEnter expressionVMOp = iota
	vmColumn
	vmParam
	vmConstant
	vmArith
	vmCompare
	vmNeg
	vmCast
	vmNot
	vmIsNull
	vmAnd
	vmOr
	vmMove
	vmJump
	vmJumpFalse
	vmJumpTrue
	vmJumpNotTrue
	vmJumpNotNull
)

type expressionVMInstruction struct {
	op        expressionVMOp
	dst, a, b int
	arg       int64
	value     Value
	operator  binaryOp
	ty        scalarType
}
type expressionVMProgram struct {
	code      []expressionVMInstruction
	registers int
	supported bool
}

// Registers are a stack of reusable scalar slots. Each subtree writes the caller's
// destination; its right operand uses the next slot. CASE/COALESCE arms reuse it.
func compileExpressionVM(e *rExpr, filter bool) *expressionVMProgram {
	p := &expressionVMProgram{}
	p.supported = p.lower(e, 0, filter)
	if !p.supported {
		p.code = nil
		p.registers = 0
	}
	return p
}
func (p *expressionVMProgram) emit(i expressionVMInstruction) int {
	pc := len(p.code)
	p.code = append(p.code, i)
	return pc
}
func (p *expressionVMProgram) patch(pc int) { p.code[pc].b = len(p.code) }
func (p *expressionVMProgram) lower(e *rExpr, dst int, filter bool) bool {
	if e == nil {
		return false
	}
	if dst+1 > p.registers {
		p.registers = dst + 1
	}
	enter := p.emit(expressionVMInstruction{op: vmEnter})
	cost := costs.OperatorEval
	op := vmConstant
	switch e.kind {
	case reColumn, reParam:
		if e.vmLeafType == 0 {
			return false
		}
		if e.kind == reColumn {
			op = vmColumn
		} else {
			op = vmParam
		}
		p.emit(expressionVMInstruction{op: op, dst: dst, a: e.index})
		return true
	case reConstInt:
		p.emit(expressionVMInstruction{op: vmConstant, dst: dst, value: IntValue(e.cInt)})
		return true
	case reConstBool:
		p.emit(expressionVMInstruction{op: vmConstant, dst: dst, value: BoolValue(e.cBool)})
		return true
	case reConstNull:
		p.emit(expressionVMInstruction{op: vmConstant, dst: dst, value: NullValue()})
		return true
	case reArith:
		if !e.result.IsInteger() {
			return false
		}
		cost, op = operatorCost(e.op.catalogName()), vmArith
	case reCompare:
		if e.collation != nil {
			return false
		}
		cost, op = operatorCost(e.op.catalogName()), vmCompare
	case reNeg:
		if !e.result.IsInteger() {
			return false
		}
		cost, op = operatorCost("neg"), vmNeg
	case reCast:
		if !e.result.IsInteger() && !e.result.IsBool() {
			return false
		}
		op = vmCast
	case reNot:
		cost, op = operatorCost("not"), vmNot
	case reIsNull:
		op = vmIsNull
	case reAnd, reOr:
		if e.kind == reAnd {
			cost, op = operatorCost("and"), vmAnd
		} else {
			cost, op = operatorCost("or"), vmOr
		}
	case reCase, reCoalesce:
		if e.caseDecimal {
			return false
		}
	default:
		return false
	}
	p.code[enter].arg = cost
	switch e.kind {
	case reArith, reCompare:
		if !p.lower(e.lhs, dst, false) || !p.lower(e.rhs, dst+1, false) {
			return false
		}
		p.emit(expressionVMInstruction{op: op, dst: dst, a: dst + 1, operator: e.op, ty: e.result})
	case reNeg, reCast, reNot, reIsNull:
		if !p.lower(e.operand, dst, false) {
			return false
		}
		neg := int64(0)
		if e.negated {
			neg = 1
		}
		p.emit(expressionVMInstruction{op: op, dst: dst, ty: e.result, arg: neg})
	case reAnd, reOr:
		isFilterAnd := filter && e.kind == reAnd
		if !p.lower(e.lhs, dst, isFilterAnd) {
			return false
		}
		jump := vmJumpFalse
		if isFilterAnd {
			jump = vmJumpNotTrue
		} else if e.kind == reOr {
			jump = vmJumpTrue
		}
		branch := p.emit(expressionVMInstruction{op: jump, a: dst})
		if !p.lower(e.rhs, dst+1, isFilterAnd) {
			return false
		}
		p.emit(expressionVMInstruction{op: op, dst: dst, a: dst + 1})
		p.patch(branch)
	case reCase:
		var ends []int
		for _, arm := range e.caseArms {
			if !p.lower(arm.cond, dst, false) {
				return false
			}
			branch := p.emit(expressionVMInstruction{op: vmJumpNotTrue, a: dst})
			if !p.lower(arm.result, dst, false) {
				return false
			}
			ends = append(ends, p.emit(expressionVMInstruction{op: vmJump}))
			p.patch(branch)
		}
		if !p.lower(e.caseEls, dst, false) {
			return false
		}
		for _, pc := range ends {
			p.patch(pc)
		}
	case reCoalesce:
		var ends []int
		for _, a := range e.sargs {
			if !p.lower(a, dst, false) {
				return false
			}
			ends = append(ends, p.emit(expressionVMInstruction{op: vmJumpNotNull, a: dst}))
		}
		p.emit(expressionVMInstruction{op: vmConstant, dst: dst, value: NullValue()})
		for _, pc := range ends {
			p.patch(pc)
		}
	}
	return true
}

func (e *rExpr) evalExpressionVM(row storedRow, env *evalEnv, m *costMeter, filter bool) (Value, error) {
	key := expressionVMKey{e, filter}
	cached, ok := expressionVMCache.Load(key)
	if !ok {
		p := compileExpressionVM(e, filter)
		cached, ok = expressionVMCache.LoadOrStore(key, p)
		if !ok {
			if p.supported {
				expressionVMCompiled.Add(1)
				expressionVMInstructions.Add(int64(len(p.code)))
			} else {
				expressionVMFallbacks.Add(1)
			}
		}
	}
	p := cached.(*expressionVMProgram)
	if !p.supported {
		m.vmTree = true
		defer func() { m.vmTree = false }()
		if filter {
			keep, err := e.evalFilter(row, env, m)
			return BoolValue(keep), err
		}
		return e.eval(row, env, m)
	}
	if cap(m.vmRegisters) < p.registers {
		m.vmRegisters = make([]Value, p.registers)
	}
	regs := m.vmRegisters[:p.registers]
	// Only NULL, integer and boolean slots are admitted; no heap-bearing values
	// survive execution or require release/accounting beyond the scalar slots.
	for pc := 0; pc < len(p.code); pc++ {
		i := &p.code[pc]
		switch i.op {
		case vmEnter:
			if err := m.Guard(); err != nil {
				return Value{}, err
			}
			if i.arg != 0 {
				m.Charge(i.arg)
			}
		case vmColumn:
			v := row[i.a]
			if v.Kind == ValUnfetched {
				var err error
				v, err = resolveUnfetchedSelf(v.unfetched())
				if err != nil {
					return Value{}, err
				}
			}
			regs[i.dst] = v
		case vmParam:
			regs[i.dst] = env.params[i.a]
		case vmConstant:
			regs[i.dst] = i.value
		case vmArith:
			a, b := regs[i.dst], regs[i.a]
			if a.IsNull() || b.IsNull() {
				regs[i.dst] = NullValue()
				continue
			}
			v, err := evalArith(i.operator, a.Int, b.Int, i.ty)
			if err != nil {
				return Value{}, err
			}
			regs[i.dst] = v
		case vmCompare:
			// Integer/bool/NULL operands have no decimal or varlen work. The
			// evaluator nevertheless guards after each of those zero charges.
			if err := m.Guard(); err != nil {
				return Value{}, err
			}
			if err := m.Guard(); err != nil {
				return Value{}, err
			}
			a, b := regs[i.dst], regs[i.a]
			switch i.operator {
			case opEq:
				regs[i.dst] = from3(a.Eq3(b))
			case opNe:
				regs[i.dst] = from3(not3(a.Eq3(b)))
			case opLt:
				regs[i.dst] = from3(a.Lt3(b))
			case opGt:
				regs[i.dst] = from3(a.Gt3(b))
			case opLe:
				regs[i.dst] = from3(or3(a.Lt3(b), a.Eq3(b)))
			default:
				regs[i.dst] = from3(or3(a.Gt3(b), a.Eq3(b)))
			}
		case vmNeg:
			v := regs[i.dst]
			if v.IsNull() {
				continue
			}
			if v.Int == math.MinInt64 || !i.ty.InRange(-v.Int) {
				return Value{}, overflowErr(i.ty)
			}
			regs[i.dst] = IntValue(-v.Int)
		case vmCast:
			if regs[i.dst].IsNull() {
				continue
			}
			v, err := evalCast(regs[i.dst], i.ty, nil)
			if err != nil {
				return Value{}, err
			}
			regs[i.dst] = v
		case vmNot:
			regs[i.dst] = boolNot(regs[i.dst])
		case vmIsNull:
			regs[i.dst] = BoolValue(regs[i.dst].IsNullTest(i.arg != 0))
		case vmAnd:
			regs[i.dst] = boolAnd(regs[i.dst], regs[i.a])
		case vmOr:
			regs[i.dst] = boolOr(regs[i.dst], regs[i.a])
		case vmMove:
			regs[i.dst] = regs[i.a]
		case vmJump:
			pc = i.b - 1
		case vmJumpFalse:
			if to3(regs[i.a]) == False {
				pc = i.b - 1
			}
		case vmJumpTrue:
			if regs[i.a].IsTrue() {
				pc = i.b - 1
			}
		case vmJumpNotTrue:
			if !regs[i.a].IsTrue() {
				pc = i.b - 1
			}
		case vmJumpNotNull:
			if !regs[i.a].IsNull() {
				pc = i.b - 1
			}
		}
	}
	return regs[0], nil
}
