package jed

import (
	"fmt"
)

// Aggregate- and window-function resolution (spec/design/aggregates.md). This file resolves the
// aggregate/window family of function calls against a scope: plain and DISTINCT/FILTER aggregates
// (resolveAggregate), ordered-set and hypothetical-set aggregates (resolveOrderedSetAggregate/
// resolveHypotheticalSetAggregate), GROUPING(), window calls and their OVER clause
// (resolveWindowCall/resolveWindowDef), and frame-bound resolution (resolveFrame/resolveIntBound/
// resolveRangeBound). Accumulation and detection live in aggregate.go.

// resolveAggregate resolves an aggregate call into a synthetic-row reference, collecting its
// aggSpec. Valid only in collect mode; in Forbidden mode (WHERE/ON/nested) it is 42803. The
// operand resolves in a fresh Forbidden sub-context (a nested aggregate is 42803; its columns
// resolve against the real row). The result type follows the PG widening (aggregates.md §3).
func resolveAggregate(s *scope, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if !ag.collecting {
		return nil, resolvedType{}, newError(GroupingError, "aggregate functions are not allowed here")
	}
	name := toLowerASCII(fc.Name)
	sub := &aggCtx{collecting: false}
	var (
		plan    aggPlan
		operand *rExpr
		result  resolvedType
	)
	// json[b]_object_agg[_unique] take TWO operands (key, value) — resolve both and encode as a Row
	// operand for the single-operand aggregate framework (the fold splits the composite back out).
	if objPlan, ok := objectAggClassify(name); ok {
		if fc.Star || len(fc.Args) != 2 {
			return nil, resolvedType{}, noAggOverload(name)
		}
		rk, _, err := resolve(s, *fc.Args[0], nil, sub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		rv, _, err := resolve(s, *fc.Args[1], nil, sub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		operand := &rExpr{kind: reRow, sargs: []*rExpr{rk, rv}}
		result := resolvedType{kind: rtJsonb}
		if objPlan == planJsonObjectAgg || objPlan == planJsonObjectAggUnique {
			result = resolvedType{kind: rtJson}
		}
		slot := len(ag.groupKeys) + len(ag.specs)
		ag.specs = append(ag.specs, aggSpec{plan: objPlan, operand: operand})
		return &rExpr{kind: reColumn, index: slot}, result, nil
	}
	if fc.Star {
		// Only COUNT has a star overload (aggregates.md §3); SUM(*) etc. is a syntax error.
		if !aggregateHasStar(name) {
			return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		plan, operand, result = planCountStar, nil, resolvedType{kind: rtInt, intTy: scalarInt64}
	} else {
		// One operand, resolved in a fresh Forbidden sub-context. The registry validates the
		// (surface, operand-family) overload exists (else 42883) and yields its result code; the
		// plan + result type follow from it (the PG widening).
		arg, err := aggArg(fc)
		if err != nil {
			return nil, resolvedType{}, err
		}
		// An aggregate's argument may not contain a window function (PG 42803 — window.md §7): the
		// window stage runs AFTER aggregation, so a window result cannot be folded into an aggregate.
		if exprHasWindow(arg) {
			return nil, resolvedType{}, newError(GroupingError, "aggregate function calls cannot contain window function calls")
		}
		r, t, err := resolve(s, arg, nil, sub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		desc := lookupAggregateOverload(name, t)
		if desc == nil {
			return nil, resolvedType{}, noAggOverload(name)
		}
		plan, result = aggregatePlan(name, desc.Result, t)
		operand = r
	}
	// FILTER (WHERE cond): resolve the per-row predicate against the input row with aggregates
	// FORBIDDEN — an aggregate inside FILTER is 42803, matching PG (aggregates.md §11). A non-boolean
	// condition (or an untyped NULL, always unknown → folds no row) is 42804. The fold loop evaluates
	// this per row and folds only the rows for which it is TRUE.
	var filter *rExpr
	if fc.Filter != nil {
		fsub := &aggCtx{collecting: false}
		rf, ft, err := resolve(s, *fc.Filter, nil, fsub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if ft.kind != rtBool && ft.kind != rtNull {
			return nil, resolvedType{}, typeError("argument of FILTER must be type boolean")
		}
		filter = rf
	}
	// Aggregate results follow the group-key values in the synthetic row.
	slot := len(ag.groupKeys) + len(ag.specs)
	ag.specs = append(ag.specs, aggSpec{plan: plan, operand: operand, distinct: fc.Distinct, filter: filter})
	return &rExpr{kind: reColumn, index: slot}, result, nil
}

// resolveOrderedSetAggregate resolves agg(direct_args) WITHIN GROUP (ORDER BY key) — mode,
// percentile_cont, percentile_disc (spec/design/aggregates.md §13). Like resolveAggregate it is
// valid only in collect mode (else 42803) and folds into the same aggSpec list, returning a
// synthetic-row reference. The WITHIN GROUP key is the aggregate's operand (resolved with aggregates
// forbidden — a nested aggregate is 42803); the parenthesized Args are the per-group direct argument
// (the percentile fraction; empty for mode).
func resolveOrderedSetAggregate(s *scope, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if !ag.collecting {
		return nil, resolvedType{}, newError(GroupingError, "aggregate functions are not allowed here")
	}
	// DISTINCT cannot decorate an ordered-set aggregate (PG: a 42601 syntax error).
	if fc.Distinct {
		return nil, resolvedType{}, newError(SyntaxError, "DISTINCT is not allowed with ordered-set aggregates")
	}
	name := toLowerASCII(fc.Name)
	// Exactly one WITHIN GROUP sort key (PG models a second as a missing overload → 42883).
	if len(fc.WithinGroup) != 1 {
		return nil, resolvedType{}, noAggOverload(name)
	}
	key := fc.WithinGroup[0]
	// The aggregated argument: the WITHIN GROUP order key, resolved per row with aggregates FORBIDDEN
	// (a nested aggregate in the order key is 42803, matching PG). A general-expression key
	// (`ORDER BY a + b`) carries Expr; a bare/qualified column key carries Column (rebuilt here as an
	// Expr so both paths share one resolve).
	var keyExpr exprNode
	if key.Expr != nil {
		keyExpr = *key.Expr
	} else if key.Qualifier != "" {
		keyExpr = exprNode{Kind: exprQualifiedColumn, Qualifier: key.Qualifier, Column: key.Column}
	} else {
		keyExpr = exprNode{Kind: exprColumn, Column: key.Column}
	}
	sub := &aggCtx{collecting: false}
	operand, optype, err := resolve(s, keyExpr, nil, sub, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	// The WITHIN GROUP key's COLLATION drives the sort (aggregates.md §13): an explicit COLLATE on the
	// key (text operand only — else "collations are not supported by type T", like the query ORDER BY),
	// else a bare/qualified column key inherits its column's frozen collation; otherwise the default C
	// (byte) order. Resolved to the loaded Collation (42704 if not loaded). The finalize sort applies
	// it (an unmapped code point → 0A000 there).
	var collation *Collation
	if key.Collation != "" {
		if optype.kind != rtText && optype.kind != rtNull {
			return nil, resolvedType{}, typeError(fmt.Sprintf("collations are not supported by type %s", rtName(optype)))
		}
		if collation, err = resolveCollationName(s.catalog, key.Collation); err != nil {
			return nil, resolvedType{}, err
		}
	} else if key.Expr == nil {
		// A bare/qualified column key with no explicit COLLATE inherits the column's frozen collation.
		var r resolved
		if key.Qualifier != "" {
			r, err = s.resolveQualified(key.Qualifier, key.Column)
		} else {
			r, err = s.resolveBare(key.Column)
		}
		if err != nil {
			return nil, resolvedType{}, err
		}
		if cn := s.columnOf(r).Collation; cn != "" {
			if collation, err = resolveCollationName(s.catalog, cn); err != nil {
				return nil, resolvedType{}, err
			}
		}
	}
	var (
		plan   aggPlan
		frac   *rExpr
		result resolvedType
	)
	switch name {
	case "mode":
		// mode() takes no direct argument; mode(x) matches no overload (42883).
		if len(fc.Args) != 0 {
			return nil, resolvedType{}, noAggOverload(name)
		}
		plan, frac, result = planMode, nil, optype
	case "percentile_disc":
		// An ARRAY fraction (percentile_disc(ARRAY[…])) returns an array of percentiles, one per
		// element; a scalar fraction returns one value (aggregates.md §18).
		f, isArray, err := resolveOsaFraction(s, name, fc.Args, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		plan, frac, result = planPercentileDisc, f, arrayIf(optype, isArray)
	case "percentile_cont":
		// percentile_cont interpolates: over a NUMERIC input it widens to f64 and returns f64; over
		// an INTERVAL input it interpolates in the interval domain (PG interval_lerp) and returns
		// interval. Any other WITHIN GROUP type matches no overload (42883). The fraction resolves
		// first (matching Rust's order) so an arity/type error on it is raised before the operand
		// check. An ARRAY fraction makes the result an array of those percentiles (aggregates.md §18).
		f, isArray, err := resolveOsaFraction(s, name, fc.Args, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		switch optype.kind {
		case rtInt, rtDecimal, rtFloat32, rtFloat64:
			plan, frac, result = planPercentileCont, f, arrayIf(resolvedType{kind: rtFloat64}, isArray)
		case rtInterval:
			plan, frac, result = planPercentileContInterval, f, arrayIf(resolvedType{kind: rtInterval}, isArray)
		default:
			return nil, resolvedType{}, noAggOverload(name)
		}
	default:
		panic("isOrderedSetAggregateName gates the three names above")
	}
	// FILTER (WHERE cond): resolved per input row with aggregates forbidden, exactly as for an
	// ordinary aggregate (aggregates.md §11) — a non-boolean cond is 42804, a nested aggregate 42803.
	var filter *rExpr
	if fc.Filter != nil {
		fsub := &aggCtx{collecting: false}
		rf, ft, err := resolve(s, *fc.Filter, nil, fsub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if ft.kind != rtBool && ft.kind != rtNull {
			return nil, resolvedType{}, typeError("argument of FILTER must be type boolean")
		}
		filter = rf
	}
	slot := len(ag.groupKeys) + len(ag.specs)
	ag.specs = append(ag.specs, aggSpec{plan: plan, operand: operand, distinct: false, filter: filter, osaDesc: key.Descending, osaFrac: frac, osaCollation: collation})
	return &rExpr{kind: reColumn, index: slot}, result, nil
}

// resolveHypotheticalSetAggregate resolves a hypothetical-set aggregate f(direct_args) WITHIN GROUP
// (ORDER BY keys) — rank, dense_rank, percent_rank, cume_dist (spec/design/aggregates.md §19). The
// direct args are the hypothetical row; the WITHIN GROUP keys are the sort columns. Their counts
// must match (else 42883). Each key operand is buffered per row; each direct arg is evaluated per
// group (it may reference grouping columns) and coerced to the key's type. Like the other
// ordered-set aggregates, OVER is 0A000, DISTINCT is 42601, and it is valid only in a collecting
// context.
func resolveHypotheticalSetAggregate(s *scope, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if !ag.collecting {
		return nil, resolvedType{}, newError(GroupingError, "aggregate functions are not allowed here")
	}
	if fc.Distinct {
		return nil, resolvedType{}, newError(SyntaxError, "DISTINCT is not allowed with ordered-set aggregates")
	}
	name := toLowerASCII(fc.Name)
	// The number of hypothetical direct arguments must match the number of ordering columns (PG
	// models a mismatch as a missing overload → 42883).
	if len(fc.Args) == 0 || len(fc.Args) != len(fc.WithinGroup) {
		return nil, resolvedType{}, noAggOverload(name)
	}
	// Resolve each WITHIN GROUP key operand (per row, aggregates forbidden) + its sort spec, then the
	// matching direct argument (per group, in the grouped context so it may reference grouping
	// columns) coerced to the key's type.
	keyNodes := make([]*rExpr, 0, len(fc.WithinGroup))
	sorts := make([]keySort, 0, len(fc.WithinGroup))
	argNodes := make([]*rExpr, 0, len(fc.Args))
	for i := range fc.WithinGroup {
		key := fc.WithinGroup[i]
		arg := fc.Args[i]
		// The WITHIN GROUP order key, resolved per row with aggregates FORBIDDEN (a nested aggregate is
		// 42803). A general-expression key carries Expr; a bare/qualified column key carries Column.
		var keyExpr exprNode
		if key.Expr != nil {
			keyExpr = *key.Expr
		} else if key.Qualifier != "" {
			keyExpr = exprNode{Kind: exprQualifiedColumn, Qualifier: key.Qualifier, Column: key.Column}
		} else {
			keyExpr = exprNode{Kind: exprColumn, Column: key.Column}
		}
		sub := &aggCtx{collecting: false}
		knode, ktype, err := resolve(s, keyExpr, nil, sub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		// The key's collation (explicit COLLATE — text only — or a bare/qualified column's frozen
		// collation), §13. An unknown name is 42704; a COLLATE on a non-text key is 42804.
		var collation *Collation
		if key.Collation != "" {
			if ktype.kind != rtText && ktype.kind != rtNull {
				return nil, resolvedType{}, typeError(fmt.Sprintf("collations are not supported by type %s", rtName(ktype)))
			}
			if collation, err = resolveCollationName(s.catalog, key.Collation); err != nil {
				return nil, resolvedType{}, err
			}
		} else if key.Expr == nil {
			var r resolved
			if key.Qualifier != "" {
				r, err = s.resolveQualified(key.Qualifier, key.Column)
			} else {
				r, err = s.resolveBare(key.Column)
			}
			if err != nil {
				return nil, resolvedType{}, err
			}
			if cn := s.columnOf(r).Collation; cn != "" {
				if collation, err = resolveCollationName(s.catalog, cn); err != nil {
					return nil, resolvedType{}, err
				}
			}
		}
		// The hypothetical direct arg, evaluated per group (grouped context); a literal adapts to the
		// key's scalar type via the hint. Its type must match the key's family (else 42883).
		var hint *scalarType
		if t, err := typeFromResolved(ktype); err == nil && t.Comp == nil && t.Array == nil && t.Range == nil {
			st := t.Scalar
			hint = &st
		}
		anode, atype, err := resolve(s, *arg, hint, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if !hypoArgCompatible(atype, ktype) {
			return nil, resolvedType{}, noAggOverload(name)
		}
		keyNodes = append(keyNodes, knode)
		sorts = append(sorts, keySort{desc: key.Descending, nullsFirst: key.NullsFirst, collation: collation})
		argNodes = append(argNodes, anode)
	}

	// FILTER (WHERE cond): per-input-row predicate (aggregates forbidden); restricts buffered rows.
	var filter *rExpr
	if fc.Filter != nil {
		fsub := &aggCtx{collecting: false}
		rf, ft, err := resolve(s, *fc.Filter, nil, fsub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if ft.kind != rtBool && ft.kind != rtNull {
			return nil, resolvedType{}, typeError("argument of FILTER must be type boolean")
		}
		filter = rf
	}

	var (
		plan   aggPlan
		result resolvedType
	)
	switch name {
	case "rank":
		plan, result = planHypoRank, resolvedType{kind: rtInt, intTy: scalarInt64}
	case "dense_rank":
		plan, result = planHypoDenseRank, resolvedType{kind: rtInt, intTy: scalarInt64}
	case "percent_rank":
		plan, result = planHypoPercentRank, resolvedType{kind: rtFloat64}
	case "cume_dist":
		plan, result = planHypoCumeDist, resolvedType{kind: rtFloat64}
	default:
		panic("isHypotheticalSetName gates the four names above")
	}
	slot := len(ag.groupKeys) + len(ag.specs)
	ag.specs = append(ag.specs, aggSpec{plan: plan, operand: nil, distinct: false, filter: filter, hypo: &hypoParams{args: argNodes, keys: keyNodes, sorts: sorts}})
	return &rExpr{kind: reColumn, index: slot}, result, nil
}

// hypoArgCompatible reports whether a hypothetical direct argument of type arg is comparable with the
// WITHIN GROUP key of type key (aggregates.md §19). A NULL arg is always allowed; otherwise the two
// must be the same scalar family (numeric Int/Decimal/Float each only match themselves, exactly as
// the value comparator orders them), so the buffered key tuple and the hypothetical row compare
// meaningfully.
func hypoArgCompatible(arg, key resolvedType) bool {
	if arg.kind == rtNull {
		return true
	}
	switch {
	case arg.kind == rtInt && key.kind == rtInt,
		arg.kind == rtDecimal && key.kind == rtDecimal,
		isFloatKind(arg.kind) && isFloatKind(key.kind),
		arg.kind == rtText && key.kind == rtText,
		arg.kind == rtBool && key.kind == rtBool,
		arg.kind == rtBytea && key.kind == rtBytea,
		arg.kind == rtUuid && key.kind == rtUuid,
		arg.kind == rtTimestamp && key.kind == rtTimestamp,
		arg.kind == rtTimestamptz && key.kind == rtTimestamptz,
		arg.kind == rtDate && key.kind == rtDate,
		arg.kind == rtInterval && key.kind == rtInterval:
		return true
	}
	return false
}

// resolveOsaFraction resolves an ordered-set aggregate's direct argument — the percentile fraction
// (aggregates.md §13/§17/§18). The fraction is evaluated **once per group**, so it may be any
// expression over **grouping columns** (resolved here in the grouped agg context, so a grouping
// column binds its synthetic key slot and a non-grouped column is 42803 — PG's "direct arguments …
// must use only grouped columns"; a constant folds the usual way). An aggregate inside the fraction
// is 42803 (PG forbids nesting). Resolved with a float hint so a bare numeric literal folds to f64.
// The returned node is stored and evaluated per group at finalize. Returns (node, isArray) — a
// NUMERIC array fraction (percentile_cont(ARRAY[…])) computes one percentile per element and returns
// an array (§18). A non-numeric fraction or a wrong argument count matches no overload (42883); a
// NULL fraction yields a NULL result at finalize.
func resolveOsaFraction(s *scope, name string, args []*exprNode, ag *aggCtx, params *paramTypes) (*rExpr, bool, error) {
	if len(args) != 1 {
		return nil, false, noAggOverload(name) // wrong argument count
	}
	// The fraction is evaluated before the fold (it is a direct argument, not an aggregate operand),
	// so a nested aggregate is illegal — 42803, matching PG.
	if exprHasAggregate(*args[0]) {
		return nil, false, newError(GroupingError, "aggregate function calls cannot be nested")
	}
	fl := scalarFloat64
	rarg, rtype, err := resolve(s, *args[0], &fl, ag, params)
	if err != nil {
		return nil, false, err
	}
	switch rtype.kind {
	case rtNull, rtFloat32, rtFloat64, rtInt, rtDecimal:
		return rarg, false, nil
	case rtArray:
		// A NUMERIC array fraction returns an array of percentiles, one per element (§18); a
		// non-numeric element matches no overload.
		switch rtype.elem.kind {
		case rtFloat32, rtFloat64, rtInt, rtDecimal:
			return rarg, true, nil
		default:
			return nil, false, noAggOverload(name)
		}
	default:
		return nil, false, noAggOverload(name) // a non-numeric fraction matches no overload
	}
}

// arrayIf returns Array(t) when isArray, else t — the result type of an ordered-set aggregate whose
// direct argument is an array vs. a scalar fraction (aggregates.md §18).
func arrayIf(t resolvedType, isArray bool) resolvedType {
	if isArray {
		return resolvedType{kind: rtArray, elem: &t}
	}
	return t
}

// resolveGrouping resolves GROUPING(c1, …, ck) (spec/design/aggregates.md §12) — the grouping-sets
// membership function. Valid only in a grouped query's projection / HAVING (collecting); each
// argument must be one of the master grouping columns, else 42803 (matching PostgreSQL). Returns an
// integer (i32) whose bit (k-1-j) is 1 iff c_j is grouped away in the row's grouping set. The value
// is computed per group row at execution from the grouping set's mask, so the call resolves to the
// placeholder slot groupingGsBase+index (rebased to its real trailing synthetic slot afterwards).
func resolveGrouping(s *scope, fc *funcCallExpr, ag *aggCtx) (*rExpr, resolvedType, error) {
	if fc.Star {
		// GROUPING(*) — PG raises a syntax error; mirror the COUNT-only `*` message (42601).
		return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
	}
	if len(fc.Args) == 0 {
		// GROUPING() with no arguments — PG raises a syntax error (42601).
		return nil, resolvedType{}, newError(SyntaxError, "GROUPING requires at least one argument")
	}
	groupingArgErr := func() error {
		return newError(GroupingError, "arguments to GROUPING must be grouping expressions of the associated query level")
	}
	// GROUPING is meaningful only in a grouped query (ag.collecting) — including a grouped query that
	// ALSO has window functions (GROUPING SETS + window, aggregates.md §21); outside one its arguments
	// cannot be grouping expressions.
	if !ag.collecting {
		return nil, resolvedType{}, groupingArgErr()
	}
	positions := make([]int, 0, len(fc.Args))
	for _, arg := range fc.Args {
		var (
			r   resolved
			err error
		)
		switch arg.Kind {
		case exprColumn:
			r, err = s.resolveBare(arg.Column)
		case exprQualifiedColumn:
			r, err = s.resolveQualified(arg.Qualifier, arg.Column)
		default:
			// A non-column argument is never a grouping column (jed groups by columns only).
			return nil, resolvedType{}, groupingArgErr()
		}
		if err != nil {
			return nil, resolvedType{}, err
		}
		if r.level != 0 {
			return nil, resolvedType{}, groupingArgErr()
		}
		pos := -1
		for p, gk := range ag.groupKeys {
			if gk == r.index {
				pos = p
				break
			}
		}
		if pos < 0 {
			return nil, resolvedType{}, groupingArgErr()
		}
		positions = append(positions, pos)
	}
	slot := groupingGsBase + len(ag.groupingSpecs)
	ag.groupingSpecs = append(ag.groupingSpecs, positions)
	return &rExpr{kind: reColumn, index: slot}, resolvedType{kind: rtInt, intTy: scalarInt32}, nil
}

// resolveWindowCall resolves a window-function call `f(args) OVER (window_definition)`
// (spec/design/window.md §5.1). Valid only in a window query's projection (ag.windowing); anywhere
// else (WHERE / JOIN ON / HAVING / an aggregate query) it is 42P20. The call collects into a
// windowSpec and resolves to the synthetic slot windowBase+window_index. S0: only row_number().
func resolveWindowCall(s *scope, fc *funcCallExpr, filter *exprNode, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	name := toLowerASCII(fc.Name)
	// The plan + result type from the function name. S0: only row_number(); an aggregate name with
	// OVER (a window aggregate, S3) resolves to planAgg carrying the aggregate plan in wagg; any
	// other name is 42883.
	var (
		plan   windowPlan
		result resolvedType
		wagg   aggPlan // the aggregate plan, valid only when plan == planAgg
	)
	// The sub-context window ARGUMENTS resolve in (spec/design/window.md §5.1). In a grouped query
	// (ag.collecting) they resolve against the grouped row — a nested aggregate collects into the
	// query's SHARED specs and a bare column must be a grouping key (else 42803) — so `sub` is a
	// collecting context seeded with the running specs; a nested window is then 42P20 (sub is not
	// windowing). In a plain window query `sub` is Forbidden (no aggregate/window nesting). The grown
	// specs are written back into ag at the end so the next window's nested aggregates keep numbering.
	sub := &aggCtx{}
	if ag.collecting {
		// Seed with the running grouping specs too, so a GROUPING() nested in a window argument collects
		// into the query's shared grouping specs (GROUPING SETS + window, aggregates.md §21); written
		// back alongside specs below.
		sub = &aggCtx{collecting: true, groupKeys: ag.groupKeys, groupKeyExprs: ag.groupKeyExprs, specs: ag.specs, groupingSpecs: ag.groupingSpecs}
	}
	// The frame-insensitive no-argument ranking functions (S0/S1): row_number/rank/dense_rank → i64.
	noArgI64, isNoArg := map[string]windowPlan{
		"row_number": planRowNumber,
		"rank":       planRank,
		"dense_rank": planDenseRank,
	}[name]
	// The frame-insensitive no-argument ratio functions (S1): percent_rank/cume_dist → f64
	// (PG's float8 — the ratio is the IEEE correctly-rounded f64 division, window.md §4).
	noArgRatio, isNoArgRatio := map[string]windowPlan{
		"percent_rank": planPercentRank,
		"cume_dist":    planCumeDist,
	}[name]
	var wargs []*rExpr
	switch {
	case isNoArg:
		if fc.Star || len(fc.Args) != 0 {
			return nil, resolvedType{}, newError(UndefinedFunction, name+" takes no arguments")
		}
		plan = noArgI64
		result = resolvedType{kind: rtInt, intTy: scalarInt64}
	case isNoArgRatio:
		if fc.Star || len(fc.Args) != 0 {
			return nil, resolvedType{}, newError(UndefinedFunction, name+" takes no arguments")
		}
		plan = noArgRatio
		result = resolvedType{kind: rtFloat64}
	case name == "ntile":
		// ntile(n) — one integer bucket-count argument (window.md §4), resolved in a fresh
		// Forbidden sub-context (no aggregate/window nesting in a window argument).
		if fc.Star || len(fc.Args) != 1 {
			return nil, resolvedType{}, newError(UndefinedFunction, "ntile takes exactly one argument")
		}
		anode, aty, err := resolve(s, *fc.Args[0], nil, sub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if aty.kind != rtInt && aty.kind != rtNull {
			return nil, resolvedType{}, typeError("argument of ntile must be integer")
		}
		wargs = append(wargs, anode)
		plan = planNtile
		result = resolvedType{kind: rtInt, intTy: scalarInt64}
	case name == "lag" || name == "lead":
		// lag/lead(value [, offset [, default]]) — window.md §4. The value expression's type is the
		// result; offset is an integer (default 1); default (returned when the offset leaves the
		// partition) must match the value type. Args resolved in a fresh Forbidden sub-context.
		if fc.Star || len(fc.Args) == 0 || len(fc.Args) > 3 {
			return nil, resolvedType{}, newError(UndefinedFunction, name+" takes 1 to 3 arguments")
		}
		vnode, vty, err := resolve(s, *fc.Args[0], nil, sub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		// The scalar hint for resolving the default literal: the value's scalar for an int/float
		// value type, else none (mirrors Rust's Int(s) | Float(s) => Some(*s)).
		var hint *scalarType
		switch vty.kind {
		case rtInt:
			h := vty.intTy
			hint = &h
		case rtFloat32:
			h := scalarFloat32
			hint = &h
		case rtFloat64:
			h := scalarFloat64
			hint = &h
		}
		wargs = append(wargs, vnode)
		if len(fc.Args) >= 2 {
			onode, oty, err := resolve(s, *fc.Args[1], nil, sub, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			if oty.kind != rtInt && oty.kind != rtNull {
				return nil, resolvedType{}, typeError("offset of " + name + " must be integer")
			}
			wargs = append(wargs, onode)
		}
		if len(fc.Args) == 3 {
			dnode, dty, err := resolve(s, *fc.Args[2], hint, sub, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			if dty.kind != rtNull && !resolvedTypeEqual(dty, vty) {
				return nil, resolvedType{}, typeError("default of " + name + " must match the value type")
			}
			wargs = append(wargs, dnode)
		}
		if name == "lag" {
			plan = planLag
		} else {
			plan = planLead
		}
		result = vty
	case isAggregateName(name):
		// An aggregate used as a window function (S3): reuse the aggregate overload resolution to
		// get the plan + result type; applyWindowStage folds it over the default frame (running
		// with a window ORDER BY, whole-partition without — spec/design/window.md §6).
		if fc.Star {
			// Only COUNT has a star overload; SUM(*) OVER () etc. is a syntax error.
			if !aggregateHasStar(name) {
				return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
			}
			wagg = planCountStar
			result = resolvedType{kind: rtInt, intTy: scalarInt64}
		} else {
			// One operand, resolved in a fresh Forbidden sub-context (no aggregate/window nesting
			// in a window aggregate's argument). The registry validates the (surface, operand-family)
			// overload exists (else 42883); the plan + result type follow the PG widening.
			arg, err := aggArg(fc)
			if err != nil {
				return nil, resolvedType{}, err
			}
			r, t, err := resolve(s, arg, nil, sub, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			desc := lookupAggregateOverload(name, t)
			if desc == nil {
				return nil, resolvedType{}, noAggOverload(name)
			}
			wagg, result = aggregatePlan(name, desc.Result, t)
			wargs = append(wargs, r) // the aggregate operand → args[0]
		}
		plan = planAgg
	case name == "first_value" || name == "last_value" || name == "nth_value":
		// Frame-sensitive value pickers (S4, window.md §4). first/last_value take one value
		// expression (→ result type); nth_value takes the value + an integer position. Args
		// resolved in a fresh Forbidden sub-context (no aggregate/window nesting).
		if fc.Star {
			return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		want := 1
		if name == "nth_value" {
			want = 2
		}
		if len(fc.Args) != want {
			return nil, resolvedType{}, newError(UndefinedFunction,
				fmt.Sprintf("%s takes %d argument(s)", name, want))
		}
		vnode, vty, err := resolve(s, *fc.Args[0], nil, sub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		wargs = append(wargs, vnode)
		if name == "first_value" {
			plan = planFirstValue
		} else if name == "last_value" {
			plan = planLastValue
		} else {
			nnode, nty, err := resolve(s, *fc.Args[1], nil, sub, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			if nty.kind != rtInt && nty.kind != rtNull {
				return nil, resolvedType{}, typeError("position of nth_value must be integer")
			}
			wargs = append(wargs, nnode)
			plan = planNthValue
		}
		result = vty
	default:
		return nil, resolvedType{}, newError(UndefinedFunction, name+" is not a window function")
	}
	// Resolve the window definition (PARTITION BY / ORDER BY expressions → slots, explicit frame).
	// Keys resolve in `sub` (the grouped collecting ctx — a bare grouping column → its grouped-row
	// slot and an aggregate → an agg slot, else 42803; or plain Forbidden, columns → real input
	// slots); a non-column key materializes into ag.windowKeys at a windowKeyBase+k placeholder
	// (window.md §5.1).
	partition, order, frame, err := resolveWindowDef(s, fc.Over, sub, &ag.windowKeys, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	// FILTER (WHERE cond) on a window aggregate (aggregates.md §20): a per-frame-row boolean over the
	// INPUT row, resolved with aggregates forbidden (a nested aggregate is 42803, a non-boolean 42804)
	// — exactly the non-window FILTER rule (§11). The window stage folds only the frame rows it keeps.
	var rfilter *rExpr
	if filter != nil {
		fsub := &aggCtx{collecting: false}
		rf, ft, ferr := resolve(s, *filter, nil, fsub, params)
		if ferr != nil {
			return nil, resolvedType{}, ferr
		}
		if ft.kind != rtBool && ft.kind != rtNull {
			return nil, resolvedType{}, typeError("argument of FILTER must be type boolean")
		}
		rfilter = rf
	}
	// A window function is allowed only in a window query's projection. In WHERE / a JOIN ON /
	// HAVING / an aggregate-only query ag is not windowing → 42P20 (window.md §7).
	if !ag.windowing {
		return nil, resolvedType{}, newError(WindowingError, "window functions are not allowed here")
	}
	// Write back the (possibly grown) aggregate specs collected from this call's arguments AND window
	// keys so the next window's nested aggregates continue the numbering (grouped query — window.md
	// §5.1).
	if ag.collecting {
		ag.specs = sub.specs
		ag.groupingSpecs = sub.groupingSpecs
	}
	slot := ag.windowBase + len(ag.windowSpecs)
	ag.windowSpecs = append(ag.windowSpecs, windowSpec{plan: plan, partition: partition, order: order, args: wargs, aggPlan: wagg, frame: frame, filter: rfilter})
	return &rExpr{kind: reColumn, index: slot}, result, nil
}

// windowKeySlot maps a resolved window-key expression to the slot the window stage indexes
// (spec/design/window.md §5.1). A bare column / aggregate (reColumn) keeps its real row slot — the
// input slot for a plain query, the grouped-row slot for a grouped one — so a column-only window is
// byte-identical to before. Any compound expression is materialized into *windowKeys at the
// placeholder slot windowKeyBase+k (rebased once the row layout is final). A key referencing an
// enclosing query (a correlated window — clause names it) is the deferred follow-on (0A000).
func windowKeySlot(rexpr *rExpr, clause string, windowKeys *[]*rExpr) (int, error) {
	if rexprReferencesOuter(rexpr, 0) {
		return 0, newError(FeatureNotSupported, clause+" may not reference an outer query column")
	}
	if rexpr.kind == reColumn {
		return rexpr.index, nil
	}
	k := len(*windowKeys)
	*windowKeys = append(*windowKeys, rexpr)
	return windowKeyBase + k, nil
}

// resolveWindowDef resolves the PARTITION BY and within-partition ORDER BY (→ sort keys) of an
// OVER (...) clause. Each key is a general expression (spec/design/window.md §5.1) resolved against
// keyCtx: a plain window query passes a Forbidden ctx (columns → real input slots, an aggregate is
// 42803), a grouped one passes a collecting ctx sharing the query's aggregate specs (a bare column →
// its grouping-column slot or 42803, an aggregate sum(x) collects → its agg slot). A bare-column /
// aggregate key (reColumn) keeps its real slot; any compound key is materialized into windowKeys at a
// windowKeyBase+k placeholder. A key referencing an enclosing-query column (a correlated window) is
// 0A000; a window function inside a key is rejected by keyCtx (42P20).
func resolveWindowDef(s *scope, wd *windowDef, keyCtx *aggCtx, windowKeys *[]*rExpr, params *paramTypes) ([]int, []orderSlot, *resolvedFrame, error) {
	partition := make([]int, 0, len(wd.Partition))
	for _, key := range wd.Partition {
		rexpr, _, err := resolve(s, key, nil, keyCtx, params)
		if err != nil {
			return nil, nil, nil, err
		}
		slot, err := windowKeySlot(rexpr, "PARTITION BY", windowKeys)
		if err != nil {
			return nil, nil, nil, err
		}
		partition = append(partition, slot)
	}
	order := make([]orderSlot, 0, len(wd.Order))
	// The ORDER BY key types, captured in lockstep with order — a RANGE value-offset frame folds
	// key ± offset over the single ordering key, so it needs the key's type (§6).
	orderTypes := make([]dataType, 0, len(wd.Order))
	for _, key := range wd.Order {
		rexpr, ty, err := resolve(s, key.Expr, nil, keyCtx, params)
		if err != nil {
			return nil, nil, nil, err
		}
		// The sort-key collation. An explicit trailing COLLATE (rare — parseExpr usually absorbs a
		// COLLATE into the key expression) must be on a text key (42804); otherwise the collation is
		// DERIVED from the key expression (collation.md §1) — a COLLATE inside it is explicit, a bare
		// text column is its frozen implicit collation, every other shape resets to none (C). A
		// collated window ORDER BY honors the collation in both the per-partition sort and peer
		// determination (window.md §3/§5); COLLATE "C" resolves to nil (the raw-byte fast path).
		var coll *Collation
		if key.Collation != "" {
			if ty.kind != rtText && ty.kind != rtNull {
				return nil, nil, nil, typeError(fmt.Sprintf("collations are not supported by type %s", rtName(ty)))
			}
			if coll, err = resolveCollationName(s.catalog, key.Collation); err != nil {
				return nil, nil, nil, err
			}
		} else {
			d, derr := deriveCollation(s, key.Expr)
			if derr != nil {
				return nil, nil, nil, derr
			}
			if coll, err = resolveDeriv(s.catalog, d); err != nil {
				return nil, nil, nil, err
			}
		}
		slot, err := windowKeySlot(rexpr, "window ORDER BY", windowKeys)
		if err != nil {
			return nil, nil, nil, err
		}
		order = append(order, orderSlot{idx: slot, descending: key.Descending, nullsFirst: key.NullsFirst, collation: coll})
		kt, err := typeFromResolved(ty)
		if err != nil {
			return nil, nil, nil, err
		}
		orderTypes = append(orderTypes, kt)
	}
	// The explicit frame (window.md §6): ROWS / GROUPS integer-count offsets, RANGE value offsets.
	var frame *resolvedFrame
	if wd.Frame != nil {
		f, err := resolveFrame(wd.Frame, order, orderTypes)
		if err != nil {
			return nil, nil, nil, err
		}
		frame = f
	}
	return partition, order, frame, nil
}

// resolveFrame resolves an explicit frame clause (spec/design/window.md §6). GROUPS requires an
// ORDER BY (42P20); a RANGE value offset requires exactly one ORDER BY column (42P20) of an integer,
// decimal, or float type (a timestamp/date key is the deferred D4 follow-on, any other type is
// 0A000). A negative offset is 22013. Mirrors Rust's resolve_frame.
func resolveFrame(f *windowFrame, order []orderSlot, orderTypes []dataType) (*resolvedFrame, error) {
	isOffset := func(b frameBound) bool { return b.Kind == framePreceding || b.Kind == frameFollowing }
	hasOffset := isOffset(f.Start) || isOffset(f.End)
	switch f.Mode {
	case frameRows:
		start, err := resolveIntBound(f.Start)
		if err != nil {
			return nil, err
		}
		end, err := resolveIntBound(f.End)
		if err != nil {
			return nil, err
		}
		return &resolvedFrame{mode: frameRows, start: start, end: end, exclude: f.Exclude}, nil
	case frameGroups:
		if len(order) == 0 {
			return nil, newError(WindowingError, "GROUPS mode requires an ORDER BY clause")
		}
		start, err := resolveIntBound(f.Start)
		if err != nil {
			return nil, err
		}
		end, err := resolveIntBound(f.End)
		if err != nil {
			return nil, err
		}
		return &resolvedFrame{mode: frameGroups, start: start, end: end, exclude: f.Exclude}, nil
	default: // FrameRange
		if hasOffset {
			if len(order) != 1 {
				return nil, newError(WindowingError,
					"RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column")
			}
			kt := orderTypes[0]
			if !(kt.IsInteger() || kt.IsDecimal() || kt.IsFloat()) {
				return nil, newError(FeatureNotSupported, fmt.Sprintf(
					"RANGE with offset PRECEDING/FOLLOWING is not supported for column type %s", kt.CanonicalName(),
				))
			}
			start, err := resolveRangeBound(f.Start, kt)
			if err != nil {
				return nil, err
			}
			end, err := resolveRangeBound(f.End, kt)
			if err != nil {
				return nil, err
			}
			return &resolvedFrame{mode: frameRange, start: start, end: end, exclude: f.Exclude}, nil
		}
		// RANGE with only UNBOUNDED / CURRENT ROW bounds — peer/edge based, any number of ORDER BY
		// keys (or none); no key arithmetic, so it reuses the plain bound resolution.
		start, err := resolveIntBound(f.Start)
		if err != nil {
			return nil, err
		}
		end, err := resolveIntBound(f.End)
		if err != nil {
			return nil, err
		}
		return &resolvedFrame{mode: frameRange, start: start, end: end, exclude: f.Exclude}, nil
	}
}

// resolveIntBound resolves a ROWS/GROUPS frame bound: the offset of `n PRECEDING`/`n FOLLOWING` must
// be a non-negative integer literal (22013 if negative; a non-literal/non-integer offset is 0A000).
func resolveIntBound(b frameBound) (resolvedBound, error) {
	offset := func(e exprNode) (Value, error) {
		if e.Kind == exprLiteral && e.Literal != nil && e.Literal.Kind == literalInt {
			if e.Literal.Int >= 0 {
				return IntValue(e.Literal.Int), nil
			}
			return Value{}, newError(InvalidPrecedingOrFollowingSize,
				"frame starting or ending offset must not be negative")
		}
		return Value{}, newError(FeatureNotSupported, "frame offset must be a non-negative integer literal")
	}
	switch b.Kind {
	case frameUnboundedPreceding:
		return resolvedBound{kind: boundUnboundedPreceding}, nil
	case frameCurrentRow:
		return resolvedBound{kind: boundCurrentRow}, nil
	case frameUnboundedFollowing:
		return resolvedBound{kind: boundUnboundedFollowing}, nil
	case framePreceding:
		v, err := offset(b.Offset)
		if err != nil {
			return resolvedBound{}, err
		}
		return resolvedBound{kind: boundPreceding, offVal: v}, nil
	case frameFollowing:
		v, err := offset(b.Offset)
		if err != nil {
			return resolvedBound{}, err
		}
		return resolvedBound{kind: boundFollowing, offVal: v}, nil
	default:
		return resolvedBound{}, newError(FeatureNotSupported, "unsupported frame bound")
	}
}

// resolveRangeBound resolves a RANGE value-offset bound (window.md §6). The offset literal must be a
// non-negative numeric matching the ordering key type: an integer key takes an integer offset (a
// decimal offset is 0A000, matching PG); a decimal key takes an integer (widened) or decimal offset;
// a float key takes an integer or decimal offset converted to f64 (PG's in_range_float*_float8 — the
// offset is float8 for both f32 and f64 keys). The decimal→f64 conversion traps 22003 on overflow
// (jed's float-cast rule); an int offset is always finite.
func resolveRangeBound(b frameBound, kt dataType) (resolvedBound, error) {
	offset := func(e exprNode) (Value, error) {
		if e.Kind != exprLiteral || e.Literal == nil {
			return Value{}, newError(FeatureNotSupported, "frame offset must be a non-negative numeric literal")
		}
		switch e.Literal.Kind {
		case literalInt:
			if e.Literal.Int < 0 {
				return Value{}, newError(InvalidPrecedingOrFollowingSize,
					"frame starting or ending offset must not be negative")
			}
			if kt.IsFloat() {
				return Float64Value(float64(e.Literal.Int)), nil
			}
			if kt.IsDecimal() {
				return DecimalValue(decimalFromInt64(e.Literal.Int)), nil
			}
			return IntValue(e.Literal.Int), nil
		case literalDecimal:
			if e.Literal.Dec.Neg && !e.Literal.Dec.IsZero() {
				return Value{}, newError(InvalidPrecedingOrFollowingSize,
					"frame starting or ending offset must not be negative")
			}
			if kt.IsFloat() {
				f, err := decimalToFloat64(e.Literal.Dec)
				if err != nil {
					return Value{}, err
				}
				return Float64Value(f), nil
			}
			if !kt.IsDecimal() {
				return Value{}, newError(FeatureNotSupported, fmt.Sprintf(
					"RANGE with offset PRECEDING/FOLLOWING is not supported for column type %s and offset type decimal",
					kt.CanonicalName(),
				))
			}
			return DecimalValue(e.Literal.Dec), nil
		default:
			return Value{}, newError(FeatureNotSupported, "frame offset must be a non-negative numeric literal")
		}
	}
	switch b.Kind {
	case frameUnboundedPreceding:
		return resolvedBound{kind: boundUnboundedPreceding}, nil
	case frameCurrentRow:
		return resolvedBound{kind: boundCurrentRow}, nil
	case frameUnboundedFollowing:
		return resolvedBound{kind: boundUnboundedFollowing}, nil
	case framePreceding:
		v, err := offset(b.Offset)
		if err != nil {
			return resolvedBound{}, err
		}
		return resolvedBound{kind: boundPreceding, offVal: v}, nil
	case frameFollowing:
		v, err := offset(b.Offset)
		if err != nil {
			return resolvedBound{}, err
		}
		return resolvedBound{kind: boundFollowing, offVal: v}, nil
	default:
		return resolvedBound{}, newError(FeatureNotSupported, "unsupported frame bound")
	}
}

// aggArg returns the single argument of a non-star aggregate call. Each aggregate takes
// exactly one argument; a different count matches no aggregate overload and is 42883 (PG).
func aggArg(fc *funcCallExpr) (exprNode, error) {
	if len(fc.Args) != 1 {
		return exprNode{}, newError(UndefinedFunction, "no aggregate function matches the given argument count")
	}
	return *fc.Args[0], nil
}

// noAggOverload is 42883 — an aggregate over an operand family it has no overload for.
func noAggOverload(fn string) error {
	return newError(UndefinedFunction, "no "+fn+" aggregate for that argument type")
}

// noFuncOverload is 42883 — a scalar function over argument types it has no overload for.
func noFuncOverload(fn string) error {
	return newError(UndefinedFunction, "no "+fn+" function for those argument types")
}
