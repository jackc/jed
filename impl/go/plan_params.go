package jed

import (
	"errors"
	"fmt"
)

// Parameter-sensitive ("custom") planning (spec/design/estimator.md §6.1).
//
// Planning runs inside a frame that says what the estimator may know about each $N:
//
//   - probe: the values are not bound yet. A parameter is a plan-time literal of unknown value: the
//     estimator follows its literal path, and the first read of a parameter's VALUE records that the
//     statement is parameter-sensitive and takes the generic (unbound) estimate. A NULL test of a
//     parameter answers "not NULL" and records that the plan assumed so.
//   - custom: the bound values, as the constants a literal of each parameter's resolved type would
//     resolve to. Every estimator read sees the value, so the plan is the custom plan.
//
// A probe plan that recorded nothing never consulted a value, so it is the custom plan for every
// non-NULL binding (and for NULL ones too when no NULL test fired). Each plan→bind boundary plans in
// a probe frame, binds, and plans again in a custom frame only when the probe was not value-blind.
//
// The frame stack lives on the engine, which one goroutine drives through a statement. A parameter
// node captures the frame current when it was resolved (Go has no goroutine-local state), so the
// estimator reads the frame through the node. A frame stops answering once its boundary ends, and
// a frame shared through a cached plan is never written after it is bound.

type planFrame struct {
	custom bool
	ended  bool
	values []Value
	// probe observations
	sensitive bool
	nullRead  bool
	// every type a $N was given while probing (the EXPLAIN typing pass reads it)
	types []*colType
	// a restarted statement adopts this custom frame at its own begin instead of probing again
	adopt bool
}

// value is the plan-time constant of bound parameter i, or nil when the value is unavailable (a
// probe frame — which records the statement as parameter-sensitive — no frame, or a value with no
// literal form).
func (f *planFrame) value(i int) *rExpr {
	if f == nil || f.ended {
		return nil
	}
	if !f.custom {
		f.sensitive = true
		return nil
	}
	if i >= len(f.values) {
		return nil
	}
	c := valueToRExpr(f.values[i])
	if !estimatorLiteral(c) {
		return nil
	}
	return c
}

// isNull reports whether bound parameter i is NULL. An unbound parameter is assumed non-NULL; a
// probe frame records the assumption so a NULL binding is planned again with its value.
func (f *planFrame) isNull(i int) bool {
	if f == nil || f.ended {
		return false
	}
	if !f.custom {
		f.nullRead = true
		return false
	}
	return i < len(f.values) && f.values[i].Kind == ValNull
}

// noteType records the type a probe frame's resolution gave $(idx0+1) (nil = no context here,
// which still makes the slot exist).
func (f *planFrame) noteType(idx0 int, ty *colType) {
	if f == nil || f.ended || f.custom {
		return
	}
	for idx0 >= len(f.types) {
		f.types = append(f.types, nil)
	}
	if ty == nil {
		return
	}
	t := *ty
	if f.types[idx0] != nil {
		if u, err := unifyParamType(*f.types[idx0], t, idx0); err == nil {
			t = u
		}
	}
	f.types[idx0] = &t
}

// currentPlanFrame is the frame a parameter resolved now belongs to (nil outside any boundary).
func (db *engine) currentPlanFrame() *planFrame {
	if db == nil || len(db.planFrames) == 0 {
		return nil
	}
	return db.planFrames[len(db.planFrames)-1]
}

// newParamTypes starts a parameter-type collection that also reports to the current frame (the
// EXPLAIN typing pass).
func (db *engine) newParamTypes() *paramTypes {
	return &paramTypes{frame: db.currentPlanFrame()}
}

// planning is one plan→bind boundary's frame.
type planning struct {
	db     *engine
	frame  *planFrame
	pushed bool
}

// beginPlanning opens a boundary: adopt a restart's pending custom frame, else push a probe frame.
func (db *engine) beginPlanning() *planning {
	if top := db.currentPlanFrame(); top != nil && top.custom && top.adopt {
		top.adopt = false
		return &planning{db: db, frame: top}
	}
	f := &planFrame{}
	db.planFrames = append(db.planFrames, f)
	return &planning{db: db, frame: f, pushed: true}
}

// end closes the boundary (deferred by every caller).
func (p *planning) end() {
	if !p.pushed {
		return
	}
	p.pushed = false
	p.frame.ended = true
	p.db.planFrames = p.db.planFrames[:len(p.db.planFrames)-1]
}

// probing reports whether the boundary is still probing (it did not adopt a restart's frame).
func (p *planning) probing() bool { return p.pushed && !p.frame.custom }

// probeFacts are the probe's two observations — read a value, assumed a parameter non-NULL — that
// a prepared statement's cache entry keeps.
func (p *planning) probeFacts() (bool, bool) {
	if !p.probing() {
		return false, false
	}
	return p.frame.sensitive, p.frame.nullRead
}

// needsCustom reports whether the probe consulted a value: it read one, or it assumed a parameter
// non-NULL that bound binds to NULL.
func (p *planning) needsCustom(bound []Value) bool {
	if !p.probing() {
		return false
	}
	if p.frame.sensitive {
		return true
	}
	if p.frame.nullRead {
		for _, v := range bound {
			if v.Kind == ValNull {
				return true
			}
		}
	}
	return false
}

// bind switches the boundary to a custom frame over bound, returning whether the probe plan must
// be planned again (needsCustom). Planning after the call sees the values.
func (p *planning) bind(bound []Value) bool {
	again := p.needsCustom(bound)
	if p.pushed {
		p.frame.custom = true
		p.frame.values = bound
	}
	return again
}

// restartPlanning runs f (a restarted statement) under a custom frame over bound that its own
// beginPlanning adopts.
func (db *engine) restartPlanning(bound []Value, f func() error) error {
	frame := &planFrame{custom: true, values: bound, adopt: true}
	db.planFrames = append(db.planFrames, frame)
	defer func() {
		frame.ended = true
		db.planFrames = db.planFrames[:len(db.planFrames)-1]
	}()
	return f()
}

// withPlanValues runs f with the bound values visible to the estimator (planning after binding with
// no probe of its own — a prepared statement's cached types).
func (db *engine) withPlanValues(bound []Value, f func() error) error {
	frame := &planFrame{custom: true, values: bound}
	db.planFrames = append(db.planFrames, frame)
	defer func() {
		frame.ended = true
		db.planFrames = db.planFrames[:len(db.planFrames)-1]
	}()
	return f()
}

// planMemoCapacity bounds how many custom plans a parameter-sensitive prepared statement remembers
// (estimator.md §6.1: any bounded policy; the memo is invisible).
const planMemoCapacity = 8

// planMemoKey is the exact identity of a binding for the custom-plan memo: each value's kind and
// its exact payload (a float's bits, a decimal's scale, a container's canonical rendering).
func planMemoKey(bound []Value) string {
	var b []byte
	for _, v := range bound {
		b = append(b, byte(v.Kind))
		b = appendMemoValue(b, v)
		b = append(b, 0xff)
	}
	return string(b)
}

func appendMemoValue(b []byte, v Value) []byte {
	switch v.Kind {
	case ValNull:
		return b
	case ValInt, ValBool, ValTimestamp, ValTimestamptz, ValDate, ValFloat32, ValFloat64:
		u := uint64(v.Int)
		for i := 0; i < 8; i++ {
			b = append(b, byte(u>>(8*i)))
		}
		return b
	case ValText, ValBytea, ValUuid, ValJson, ValJsonPath:
		return appendMemoString(b, v.str())
	case ValDecimal:
		return appendMemoString(b, v.decimal().Render())
	default:
		return appendMemoString(b, v.Render())
	}
}

func appendMemoString(b []byte, s string) []byte {
	n := uint64(len(s))
	for i := 0; i < 8; i++ {
		b = append(b, byte(n>>(8*i)))
	}
	return append(b, s...)
}

// resolveRowCounts resolves a query's LIMIT / OFFSET counts to the plan's row counts
// (spec/design/grammar.md §9). A literal was checked by the parser. A parameter $n is typed i64 and
// read from its bound value at plan time (estimator.md §6.1: every execution takes its custom plan):
// NULL is no limit / no offset (PostgreSQL's LIMIT ALL / OFFSET 0), a negative value is 2201W /
// 2201X. A probe plan, which has no values, plans without the count; reading it makes the statement
// parameter-sensitive, so the plan that runs is always planned with the value.
func (db *engine) resolveRowCounts(limit, offset *rowCount, ptypes *paramTypes) (*int64, *int64, error) {
	l, err := db.resolveRowCount(limit, true, ptypes)
	if err != nil {
		return nil, nil, err
	}
	o, err := db.resolveRowCount(offset, false, ptypes)
	if err != nil {
		return nil, nil, err
	}
	return l, o, nil
}

func (db *engine) resolveRowCount(c *rowCount, isLimit bool, ptypes *paramTypes) (*int64, error) {
	if c == nil {
		return nil, nil
	}
	if c.Param == 0 {
		v := c.Value
		return &v, nil
	}
	if db.viewTracing() {
		return nil, newError(UndefinedParameter, fmt.Sprintf("there is no parameter $%d", c.Param))
	}
	idx0 := c.Param - 1
	i64 := scalarInt64
	if err := ptypes.note(idx0, &i64); err != nil {
		return nil, err
	}
	what := "OFFSET"
	if isLimit {
		what = "LIMIT"
	}
	f := db.currentPlanFrame()
	if f == nil || f.ended {
		return nil, nil
	}
	if !f.custom {
		f.sensitive = true
		return nil, nil
	}
	if idx0 >= len(f.values) {
		return nil, nil
	}
	v := f.values[idx0]
	switch v.Kind {
	case ValNull:
		return nil, nil
	case ValInt:
		n := v.Int
		if n < 0 {
			if isLimit {
				return nil, newError(InvalidRowCountInLimitClause, "LIMIT must not be negative")
			}
			return nil, newError(InvalidRowCountInOffsetClause, "OFFSET must not be negative")
		}
		return &n, nil
	}
	return nil, newError(DatatypeMismatch, fmt.Sprintf("argument of %s must be type bigint", what))
}

// stmtParams is the statement-wide parameter state of a WITH whose parts are data-modifying
// (writable-cte.md, api.md §5). Each part (a data-modifying CTE or the primary) resolves and binds on
// its own, so the WITH runs them twice: a typing pass, in which a part merges the types it inferred
// into typing and stops before executing, then — once the whole statement is bound — the execution,
// in which each part takes the statement's values instead of binding its own.
type stmtParams struct {
	typing *paramTypes
	bound  []Value
}

// errParamsTyped ends a part's typing pass at its bind point. It never leaves runWithDml.
var errParamsTyped = errors.New("statement part typed")

// take is a part's bind point under a statement-wide parameter state: in the typing pass it merges
// ptypes and returns errParamsTyped; in the execution it returns the statement's bound values. ok is
// false outside any such state.
func (sp *stmtParams) take(ptypes *paramTypes) (bound []Value, ok bool, err error) {
	if sp == nil {
		return nil, false, nil
	}
	if sp.typing == nil {
		return sp.bound, true, nil
	}
	for i, t := range ptypes.types {
		if err := sp.typing.noteCol(i, t); err != nil {
			return nil, true, err
		}
	}
	if ptypes.uncacheable {
		sp.typing.uncacheable = true
	}
	return nil, true, errParamsTyped
}

// bindPartParams finalizes and binds the parameters a statement resolved into ptypes — or, for a
// part of a WITH with data-modifying parts, defers to the statement-wide state (stmtParams.take).
func (db *engine) bindPartParams(params []Value, ptypes *paramTypes) ([]Value, error) {
	if bound, ok, err := db.stmtParams.take(ptypes); ok {
		return bound, err
	}
	ptys, err := ptypes.finalize()
	if err != nil {
		return nil, err
	}
	return bindParams(params, ptys, db.inputZone())
}
