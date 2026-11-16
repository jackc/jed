package jed

// Plan-time constant folding (spec/design/planner.md §3.7). A resolved expression's maximal
// closed subtrees — an admitted interior node whose every operand is already a constant — are
// evaluated once, when the expression is resolved, and replaced by the constant they produce. The
// boolean connectives additionally drop a constant operand whose value cannot change the result.
//
// The pass is results-identical and never raises: an evaluation that fails (an error, the fold
// meter's ceiling) leaves its subtree as written, so the error still surfaces only where execution
// reaches it. It reads no row, parameter, session setting, clock, entropy, or catalog state: the
// admitted node kinds and value domain below exclude every kernel that could.

// foldCostLimit bounds one fold attempt's evaluation work (planner.md §3.7). An attempt that
// reaches it leaves its subtree unfolded.
const foldCostLimit = 10000

// foldConstants folds e in place and returns it (nil stays nil).
func foldConstants(s *scope, e *rExpr) *rExpr {
	if e == nil {
		return nil
	}
	foldNode(s.catalog, e)
	return e
}

// foldNode folds e's children first (bottom-up), then e itself.
func foldNode(db *engine, e *rExpr) {
	switch e.kind {
	case reSubquery:
		// A subquery body is resolved — and folded — by its own planning; its IN operand is not.
		if e.lhs != nil {
			foldNode(db, e.lhs)
		}
		return
	}
	if e.operand != nil {
		foldNode(db, e.operand)
	}
	if e.lhs != nil {
		foldNode(db, e.lhs)
	}
	if e.rhs != nil {
		foldNode(db, e.rhs)
	}
	for _, a := range e.sargs {
		foldNode(db, a)
	}
	for _, arm := range e.caseArms {
		foldNode(db, arm.cond)
		foldNode(db, arm.result)
	}
	if e.caseEls != nil {
		foldNode(db, e.caseEls)
	}
	for i := range e.subs {
		for _, b := range []*rExpr{e.subs[i].index, e.subs[i].lower, e.subs[i].upper} {
			if b != nil {
				foldNode(db, b)
			}
		}
	}
	if !foldAdmitted(e) {
		return
	}
	if foldOperandsConstant(e) {
		env := &evalEnv{exec: db}
		v, err := e.eval(nil, env, newMeterWithLimit(foldCostLimit))
		if err == nil && foldValueInDomain(v) {
			*e = *valueToRExpr(v)
			return
		}
	}
	foldConnective(e)
}

// foldConnective drops a constant operand of AND/OR that cannot change the value (planner.md §3.7):
// a left operand that decides the connective replaces it (the right operand is never evaluated, at
// run time either); a left operand that does not decide it leaves the right operand; a right
// operand that is the identity leaves the left. Each is exact under three-valued logic.
func foldConnective(e *rExpr) {
	if e.kind != reAnd && e.kind != reOr {
		return
	}
	decides := e.kind == reOr // TRUE decides OR, FALSE decides AND
	if e.lhs.kind == reConstBool {
		if e.lhs.cBool == decides {
			*e = rExpr{kind: reConstBool, cBool: decides}
		} else {
			*e = *e.rhs
		}
		return
	}
	if e.rhs.kind == reConstBool && e.rhs.cBool != decides {
		*e = *e.lhs
	}
}

// foldAdmitted is the admitted interior node set (planner.md §3.7).
func foldAdmitted(e *rExpr) bool {
	switch e.kind {
	case reCast, reNeg, reNot, reArith, reCompare, reAnd, reOr, reIsNull, reDistinct,
		reCase, reCoalesce, reGreatestLeast, reArray:
		return true
	}
	return false
}

// foldOperandsConstant reports whether every operand of an admitted node is a constant in the
// fold value domain.
func foldOperandsConstant(e *rExpr) bool {
	for _, c := range []*rExpr{e.operand, e.lhs, e.rhs, e.caseEls} {
		if c != nil && !foldConstant(c) {
			return false
		}
	}
	for _, a := range e.sargs {
		if !foldConstant(a) {
			return false
		}
	}
	for _, arm := range e.caseArms {
		if !foldConstant(arm.cond) || !foldConstant(arm.result) {
			return false
		}
	}
	return true
}

// foldConstant reports whether e is a constant node in the fold value domain.
func foldConstant(e *rExpr) bool {
	switch e.kind {
	case reConstNull, reConstInt, reConstBool, reConstText, reConstDecimal, reConstBytea,
		reConstUuid, reConstFloat32, reConstFloat64:
		return true
	case reConstArray:
		return foldArrayInDomain(e.cArray)
	}
	return false
}

// foldValueInDomain is the fold value domain (planner.md §3.7): NULL, the integers, decimal,
// f32/f64, boolean, text, bytea, uuid, and arrays of those.
func foldValueInDomain(v Value) bool {
	switch v.Kind {
	case ValNull, ValInt, ValBool, ValText, ValDecimal, ValBytea, ValUuid, ValFloat32, ValFloat64:
		return true
	case ValArray:
		return foldArrayInDomain(v.arrayVal())
	}
	return false
}

func foldArrayInDomain(a *ArrayVal) bool {
	for _, el := range a.Elements {
		if el.Kind == ValArray || !foldValueInDomain(el) {
			return false
		}
	}
	return true
}
