package jed

import (
	"slices"
	"strconv"
	"strings"
)

// Structural validation of CHECK and column DEFAULT expressions (spec/design/constraints.md). A
// CHECK/DEFAULT expression is parsed as a general expression, then screened here for the constructs
// PostgreSQL forbids in that position (subqueries, aggregates/window calls, volatile references) via
// rejectCheckStructure/rejectDefaultStructure, and checkReferencedColumns validates that a CHECK only
// touches columns of its table. Invoked from DDL (CREATE TABLE) in ddl.go.

// rejectCheckStructure applies the structural CHECK-expression rejections
// (spec/design/constraints.md §4.1) in a single depth-first pre-order walk before
// resolution: a subquery is 0A000, an aggregate call 42803, a bind parameter 42P02 — PG's
// codes and messages (oracle-probed; PG interleaves these with resolution in parse order,
// a documented micro-order divergence).
func rejectCheckStructure(e exprNode) error {
	switch e.Kind {
	case exprScalarSubquery, exprExists, exprInSubquery, exprQuantifiedSubquery:
		return newError(FeatureNotSupported, "cannot use subquery in check constraint")
	case exprParam:
		return newError(UndefinedParameter,
			"there is no parameter $"+strconv.FormatUint(e.Param, 10))
	case exprFuncCall:
		if isAggregateName(e.FuncCall.Name) {
			return newError(GroupingError,
				"aggregate functions are not allowed in check constraints")
		}
		for _, a := range e.FuncCall.Args {
			if err := rejectCheckStructure(*a); err != nil {
				return err
			}
		}
		return nil
	case exprCast:
		return rejectCheckStructure(e.Cast.Inner)
	case exprExtract:
		return rejectCheckStructure(e.Extract.Source)
	case exprCollate:
		return rejectCheckStructure(e.Collate.Inner)
	case exprUnary:
		return rejectCheckStructure(e.Unary.Operand)
	case exprIsNull:
		return rejectCheckStructure(e.IsNullOf.Operand)
	case exprIsJson:
		return rejectCheckStructure(e.IsJsonOf.Operand)
	case exprJsonCtor:
		return rejectCheckStructure(e.JsonCtorOf.Operand)
	case exprJsonExists:
		if err := rejectCheckStructure(e.JsonExists.Ctx); err != nil {
			return err
		}
		return rejectCheckStructure(e.JsonExists.Path)
	case exprJsonValue:
		if err := rejectCheckStructure(e.JsonValue.Ctx); err != nil {
			return err
		}
		return rejectCheckStructure(e.JsonValue.Path)
	case exprJsonQuery:
		if err := rejectCheckStructure(e.JsonQuery.Ctx); err != nil {
			return err
		}
		return rejectCheckStructure(e.JsonQuery.Path)
	case exprBinary:
		if err := rejectCheckStructure(e.Binary.Lhs); err != nil {
			return err
		}
		return rejectCheckStructure(e.Binary.Rhs)
	case exprIsDistinct:
		if err := rejectCheckStructure(e.IsDistinct.Lhs); err != nil {
			return err
		}
		return rejectCheckStructure(e.IsDistinct.Rhs)
	case exprLike:
		if err := rejectCheckStructure(e.Like.Lhs); err != nil {
			return err
		}
		return rejectCheckStructure(e.Like.Rhs)
	case exprRegex:
		if err := rejectCheckStructure(e.Regex.Lhs); err != nil {
			return err
		}
		return rejectCheckStructure(e.Regex.Rhs)
	case exprIn:
		if err := rejectCheckStructure(e.In.Lhs); err != nil {
			return err
		}
		for _, elem := range e.In.List {
			if err := rejectCheckStructure(elem); err != nil {
				return err
			}
		}
		return nil
	case exprBetween:
		if err := rejectCheckStructure(e.Between.Lhs); err != nil {
			return err
		}
		if err := rejectCheckStructure(e.Between.Lo); err != nil {
			return err
		}
		return rejectCheckStructure(e.Between.Hi)
	case exprCase:
		if e.Case.Operand != nil {
			if err := rejectCheckStructure(*e.Case.Operand); err != nil {
				return err
			}
		}
		for _, w := range e.Case.Whens {
			if err := rejectCheckStructure(w.Cond); err != nil {
				return err
			}
			if err := rejectCheckStructure(w.Result); err != nil {
				return err
			}
		}
		if e.Case.Els != nil {
			return rejectCheckStructure(*e.Case.Els)
		}
		return nil
	case exprCoalesce:
		for _, a := range e.Coalesce {
			if err := rejectCheckStructure(a); err != nil {
				return err
			}
		}
		return nil
	case exprGreatestLeast:
		for _, a := range e.GreatestLeast {
			if err := rejectCheckStructure(a); err != nil {
				return err
			}
		}
		return nil
	case exprFieldAccess, exprFieldStar:
		// Recurse into the composite base (spec/design/composite.md §S4) so a forbidden
		// subquery/aggregate/parameter hidden there is still rejected.
		return rejectCheckStructure(*e.Base)
	case exprQualifiedStar:
		return nil // cannot syntactically reach a CHECK (select-item-only); accept structurally
	case exprSubscript:
		// Recurse into the array base and every subscript bound.
		if err := rejectCheckStructure(*e.Base); err != nil {
			return err
		}
		for _, s := range e.Subscripts {
			for _, x := range subscriptSpecExprs(s) {
				if err := rejectCheckStructure(*x); err != nil {
					return err
				}
			}
		}
		return nil
	case exprQuantified:
		if err := rejectCheckStructure(e.Quantified.Lhs); err != nil {
			return err
		}
		return rejectCheckStructure(e.Quantified.Array)
	default: // ExprColumn, ExprQualifiedColumn, ExprLiteral
		return nil
	}
}

// rejectDefaultStructure is the structural pre-walk for a DEFAULT expression (constraints.md
// §2), run before name/type resolution (the same micro-order divergence from PG that
// rejectCheckStructure carries). A default extends the CHECK rejections with one more: it may
// NOT reference a column (it is computed before the row exists). Codes match PostgreSQL
// (oracle-probed): a column reference / subquery is 0A000, an aggregate 42803, a parameter 42P02.
func rejectDefaultStructure(e exprNode) error {
	switch e.Kind {
	case exprColumn, exprQualifiedColumn:
		return newError(FeatureNotSupported, "cannot use column reference in DEFAULT expression")
	case exprScalarSubquery, exprExists, exprInSubquery, exprQuantifiedSubquery:
		return newError(FeatureNotSupported, "cannot use subquery in DEFAULT expression")
	case exprParam:
		return newError(UndefinedParameter,
			"there is no parameter $"+strconv.FormatUint(e.Param, 10))
	case exprFuncCall:
		if isAggregateName(e.FuncCall.Name) {
			return newError(GroupingError,
				"aggregate functions are not allowed in DEFAULT expressions")
		}
		for _, a := range e.FuncCall.Args {
			if err := rejectDefaultStructure(*a); err != nil {
				return err
			}
		}
		return nil
	case exprCast:
		return rejectDefaultStructure(e.Cast.Inner)
	case exprExtract:
		return rejectDefaultStructure(e.Extract.Source)
	case exprCollate:
		return rejectDefaultStructure(e.Collate.Inner)
	case exprUnary:
		return rejectDefaultStructure(e.Unary.Operand)
	case exprIsNull:
		return rejectDefaultStructure(e.IsNullOf.Operand)
	case exprIsJson:
		return rejectDefaultStructure(e.IsJsonOf.Operand)
	case exprJsonCtor:
		return rejectDefaultStructure(e.JsonCtorOf.Operand)
	case exprJsonExists:
		if err := rejectDefaultStructure(e.JsonExists.Ctx); err != nil {
			return err
		}
		return rejectDefaultStructure(e.JsonExists.Path)
	case exprJsonValue:
		if err := rejectDefaultStructure(e.JsonValue.Ctx); err != nil {
			return err
		}
		return rejectDefaultStructure(e.JsonValue.Path)
	case exprJsonQuery:
		if err := rejectDefaultStructure(e.JsonQuery.Ctx); err != nil {
			return err
		}
		return rejectDefaultStructure(e.JsonQuery.Path)
	case exprBinary:
		if err := rejectDefaultStructure(e.Binary.Lhs); err != nil {
			return err
		}
		return rejectDefaultStructure(e.Binary.Rhs)
	case exprIsDistinct:
		if err := rejectDefaultStructure(e.IsDistinct.Lhs); err != nil {
			return err
		}
		return rejectDefaultStructure(e.IsDistinct.Rhs)
	case exprLike:
		if err := rejectDefaultStructure(e.Like.Lhs); err != nil {
			return err
		}
		return rejectDefaultStructure(e.Like.Rhs)
	case exprRegex:
		if err := rejectDefaultStructure(e.Regex.Lhs); err != nil {
			return err
		}
		return rejectDefaultStructure(e.Regex.Rhs)
	case exprIn:
		if err := rejectDefaultStructure(e.In.Lhs); err != nil {
			return err
		}
		for _, elem := range e.In.List {
			if err := rejectDefaultStructure(elem); err != nil {
				return err
			}
		}
		return nil
	case exprBetween:
		if err := rejectDefaultStructure(e.Between.Lhs); err != nil {
			return err
		}
		if err := rejectDefaultStructure(e.Between.Lo); err != nil {
			return err
		}
		return rejectDefaultStructure(e.Between.Hi)
	case exprCase:
		if e.Case.Operand != nil {
			if err := rejectDefaultStructure(*e.Case.Operand); err != nil {
				return err
			}
		}
		for _, w := range e.Case.Whens {
			if err := rejectDefaultStructure(w.Cond); err != nil {
				return err
			}
			if err := rejectDefaultStructure(w.Result); err != nil {
				return err
			}
		}
		if e.Case.Els != nil {
			return rejectDefaultStructure(*e.Case.Els)
		}
		return nil
	case exprCoalesce:
		for _, a := range e.Coalesce {
			if err := rejectDefaultStructure(a); err != nil {
				return err
			}
		}
		return nil
	case exprGreatestLeast:
		for _, a := range e.GreatestLeast {
			if err := rejectDefaultStructure(a); err != nil {
				return err
			}
		}
		return nil
	case exprFieldAccess, exprFieldStar:
		// Recurse into the composite base (spec/design/composite.md §S4).
		return rejectDefaultStructure(*e.Base)
	case exprQualifiedStar:
		return nil // cannot syntactically reach a DEFAULT (select-item-only); accept structurally
	case exprSubscript:
		// Recurse into the array base and every subscript bound.
		if err := rejectDefaultStructure(*e.Base); err != nil {
			return err
		}
		for _, s := range e.Subscripts {
			for _, x := range subscriptSpecExprs(s) {
				if err := rejectDefaultStructure(*x); err != nil {
					return err
				}
			}
		}
		return nil
	case exprQuantified:
		if err := rejectDefaultStructure(e.Quantified.Lhs); err != nil {
			return err
		}
		return rejectDefaultStructure(e.Quantified.Array)
	default: // ExprLiteral, ExprTypedLiteral
		return nil
	}
}

// checkReferencedColumns returns the distinct columns a CHECK expression references, as
// indices into columns — the input to PG's auto-naming rule (constraints.md §4.3: exactly
// one distinct column → <table>_<col>_check). Resolution already validated every
// reference, so an unknown name is simply skipped; a qualified reference counts its column
// like a bare one (oracle-probed).
func checkReferencedColumns(e exprNode, columns []catColumn) []int {
	var out []int
	var walk func(e exprNode)
	note := func(name string) {
		for i := range columns {
			if strings.EqualFold(columns[i].Name, name) {
				if !slices.Contains(out, i) {
					out = append(out, i)
				}
				return
			}
		}
	}
	walk = func(e exprNode) {
		switch e.Kind {
		case exprColumn, exprQualifiedColumn:
			note(e.Column)
		case exprCast:
			walk(e.Cast.Inner)
		case exprExtract:
			walk(e.Extract.Source)
		case exprCollate:
			walk(e.Collate.Inner)
		case exprUnary:
			walk(e.Unary.Operand)
		case exprIsNull:
			walk(e.IsNullOf.Operand)
		case exprIsJson:
			walk(e.IsJsonOf.Operand)
		case exprJsonCtor:
			walk(e.JsonCtorOf.Operand)
		case exprJsonExists:
			walk(e.JsonExists.Ctx)
			walk(e.JsonExists.Path)
		case exprJsonValue:
			walk(e.JsonValue.Ctx)
			walk(e.JsonValue.Path)
		case exprJsonQuery:
			walk(e.JsonQuery.Ctx)
			walk(e.JsonQuery.Path)
		case exprBinary:
			walk(e.Binary.Lhs)
			walk(e.Binary.Rhs)
		case exprIsDistinct:
			walk(e.IsDistinct.Lhs)
			walk(e.IsDistinct.Rhs)
		case exprLike:
			walk(e.Like.Lhs)
			walk(e.Like.Rhs)
		case exprRegex:
			walk(e.Regex.Lhs)
			walk(e.Regex.Rhs)
		case exprIn:
			walk(e.In.Lhs)
			for _, elem := range e.In.List {
				walk(elem)
			}
		case exprBetween:
			walk(e.Between.Lhs)
			walk(e.Between.Lo)
			walk(e.Between.Hi)
		case exprCase:
			if e.Case.Operand != nil {
				walk(*e.Case.Operand)
			}
			for _, w := range e.Case.Whens {
				walk(w.Cond)
				walk(w.Result)
			}
			if e.Case.Els != nil {
				walk(*e.Case.Els)
			}
		case exprCoalesce:
			for _, a := range e.Coalesce {
				walk(a)
			}
		case exprGreatestLeast:
			for _, a := range e.GreatestLeast {
				walk(a)
			}
		case exprFuncCall:
			for _, a := range e.FuncCall.Args {
				walk(*a)
			}
		case exprFieldAccess, exprFieldStar:
			// Field selection recurses into the composite base (spec/design/composite.md §S4).
			walk(*e.Base)
		case exprQualifiedStar:
			// `t.*` cannot appear in a CHECK expression (select-item-only); no columns to note.
		case exprSubscript:
			// `base[..]` recurses into the array base and every subscript bound.
			walk(*e.Base)
			for _, s := range e.Subscripts {
				for _, x := range subscriptSpecExprs(s) {
					walk(*x)
				}
			}
		case exprQuantified:
			walk(e.Quantified.Lhs)
			walk(e.Quantified.Array)
		}
	}
	walk(e)
	return out
}

// indexExprHasSubquery reports whether an index-key expression contains a SUBQUERY
// (spec/design/indexes.md §2): a scalar subquery, EXISTS, IN (subquery), or a quantified subquery.
// A subquery reads other rows, so it is not a deterministic function of this row — 0A000 at CREATE
// INDEX (PostgreSQL: "cannot use subquery in index expression"). Unlike an aggregate/window
// (rejected by resolution), the resolver admits an uncorrelated subquery, so it is caught here.
func indexExprHasSubquery(e exprNode) bool {
	var walk func(e exprNode) bool
	walk = func(e exprNode) bool {
		switch e.Kind {
		case exprScalarSubquery, exprExists, exprInSubquery, exprQuantifiedSubquery:
			return true
		case exprCast:
			return walk(e.Cast.Inner)
		case exprExtract:
			return walk(e.Extract.Source)
		case exprCollate:
			return walk(e.Collate.Inner)
		case exprUnary:
			return walk(e.Unary.Operand)
		case exprIsNull:
			return walk(e.IsNullOf.Operand)
		case exprIsJson:
			return walk(e.IsJsonOf.Operand)
		case exprJsonCtor:
			return walk(e.JsonCtorOf.Operand)
		case exprJsonExists:
			return walk(e.JsonExists.Ctx) || walk(e.JsonExists.Path)
		case exprJsonValue:
			return walk(e.JsonValue.Ctx) || walk(e.JsonValue.Path)
		case exprJsonQuery:
			return walk(e.JsonQuery.Ctx) || walk(e.JsonQuery.Path)
		case exprBinary:
			return walk(e.Binary.Lhs) || walk(e.Binary.Rhs)
		case exprIsDistinct:
			return walk(e.IsDistinct.Lhs) || walk(e.IsDistinct.Rhs)
		case exprLike:
			return walk(e.Like.Lhs) || walk(e.Like.Rhs)
		case exprRegex:
			return walk(e.Regex.Lhs) || walk(e.Regex.Rhs)
		case exprIn:
			if walk(e.In.Lhs) {
				return true
			}
			for _, elem := range e.In.List {
				if walk(elem) {
					return true
				}
			}
			return false
		case exprQuantified:
			return walk(e.Quantified.Lhs) || walk(e.Quantified.Array)
		case exprBetween:
			return walk(e.Between.Lhs) || walk(e.Between.Lo) || walk(e.Between.Hi)
		case exprCase:
			if e.Case.Operand != nil && walk(*e.Case.Operand) {
				return true
			}
			for _, w := range e.Case.Whens {
				if walk(w.Cond) || walk(w.Result) {
					return true
				}
			}
			if e.Case.Els != nil && walk(*e.Case.Els) {
				return true
			}
			return false
		case exprCoalesce:
			for _, a := range e.Coalesce {
				if walk(a) {
					return true
				}
			}
			return false
		case exprGreatestLeast:
			for _, a := range e.GreatestLeast {
				if walk(a) {
					return true
				}
			}
			return false
		case exprFuncCall:
			for _, a := range e.FuncCall.Args {
				if walk(*a) {
					return true
				}
			}
			return false
		case exprRow, exprArray:
			for _, it := range e.RowItems {
				if walk(it) {
					return true
				}
			}
			return false
		case exprFieldAccess, exprFieldStar:
			return walk(*e.Base)
		case exprSubscript:
			if walk(*e.Base) {
				return true
			}
			for _, s := range e.Subscripts {
				for _, x := range subscriptSpecExprs(s) {
					if walk(*x) {
						return true
					}
				}
			}
			return false
		default:
			return false
		}
	}
	return walk(e)
}

// rejectIndexPredicateStructure applies the structural rejections for a PARTIAL-index predicate
// (spec/design/indexes.md §9) before resolution: a subquery is 0A000 (cannot use subquery in index
// predicate) and a bind parameter $N is 42P02 (there is no parameter $N) — both admitted by the
// ordinary resolver, so caught here (the aggregate 42803 / window 42P20 / non-boolean 42804
// rejections then fall out of the Forbidden-context boolean resolve). Reuses indexExprHasSubquery
// for the subquery walk, then finds the first param.
func rejectIndexPredicateStructure(e exprNode) error {
	if indexExprHasSubquery(e) {
		return newError(FeatureNotSupported, "cannot use subquery in index predicate")
	}
	if n, ok := indexExprFirstParam(e); ok {
		return newError(UndefinedParameter, "there is no parameter $"+strconv.FormatUint(n, 10))
	}
	return nil
}

// indexExprFirstParam returns the 1-based index of the first bind parameter $N in an expression, or
// ok=false if it has none (used by rejectIndexPredicateStructure). Mirrors indexExprHasSubquery's walk.
func indexExprFirstParam(e exprNode) (uint64, bool) {
	var walk func(e exprNode) (uint64, bool)
	walk = func(e exprNode) (uint64, bool) {
		switch e.Kind {
		case exprParam:
			return e.Param, true
		case exprCast:
			return walk(e.Cast.Inner)
		case exprExtract:
			return walk(e.Extract.Source)
		case exprCollate:
			return walk(e.Collate.Inner)
		case exprUnary:
			return walk(e.Unary.Operand)
		case exprIsNull:
			return walk(e.IsNullOf.Operand)
		case exprIsJson:
			return walk(e.IsJsonOf.Operand)
		case exprJsonCtor:
			return walk(e.JsonCtorOf.Operand)
		case exprJsonExists:
			if n, ok := walk(e.JsonExists.Ctx); ok {
				return n, true
			}
			return walk(e.JsonExists.Path)
		case exprJsonValue:
			if n, ok := walk(e.JsonValue.Ctx); ok {
				return n, true
			}
			return walk(e.JsonValue.Path)
		case exprJsonQuery:
			if n, ok := walk(e.JsonQuery.Ctx); ok {
				return n, true
			}
			return walk(e.JsonQuery.Path)
		case exprBinary:
			if n, ok := walk(e.Binary.Lhs); ok {
				return n, true
			}
			return walk(e.Binary.Rhs)
		case exprIsDistinct:
			if n, ok := walk(e.IsDistinct.Lhs); ok {
				return n, true
			}
			return walk(e.IsDistinct.Rhs)
		case exprLike:
			if n, ok := walk(e.Like.Lhs); ok {
				return n, true
			}
			return walk(e.Like.Rhs)
		case exprRegex:
			if n, ok := walk(e.Regex.Lhs); ok {
				return n, true
			}
			return walk(e.Regex.Rhs)
		case exprIn:
			if n, ok := walk(e.In.Lhs); ok {
				return n, true
			}
			for _, elem := range e.In.List {
				if n, ok := walk(elem); ok {
					return n, true
				}
			}
			return 0, false
		case exprQuantified:
			if n, ok := walk(e.Quantified.Lhs); ok {
				return n, true
			}
			return walk(e.Quantified.Array)
		case exprBetween:
			if n, ok := walk(e.Between.Lhs); ok {
				return n, true
			}
			if n, ok := walk(e.Between.Lo); ok {
				return n, true
			}
			return walk(e.Between.Hi)
		case exprCase:
			if e.Case.Operand != nil {
				if n, ok := walk(*e.Case.Operand); ok {
					return n, true
				}
			}
			for _, w := range e.Case.Whens {
				if n, ok := walk(w.Cond); ok {
					return n, true
				}
				if n, ok := walk(w.Result); ok {
					return n, true
				}
			}
			if e.Case.Els != nil {
				if n, ok := walk(*e.Case.Els); ok {
					return n, true
				}
			}
			return 0, false
		case exprCoalesce:
			for _, a := range e.Coalesce {
				if n, ok := walk(a); ok {
					return n, true
				}
			}
			return 0, false
		case exprGreatestLeast:
			for _, a := range e.GreatestLeast {
				if n, ok := walk(a); ok {
					return n, true
				}
			}
			return 0, false
		case exprFuncCall:
			for _, a := range e.FuncCall.Args {
				if n, ok := walk(*a); ok {
					return n, true
				}
			}
			return 0, false
		case exprRow, exprArray:
			for _, it := range e.RowItems {
				if n, ok := walk(it); ok {
					return n, true
				}
			}
			return 0, false
		case exprFieldAccess, exprFieldStar:
			return walk(*e.Base)
		case exprSubscript:
			if n, ok := walk(*e.Base); ok {
				return n, true
			}
			for _, s := range e.Subscripts {
				for _, x := range subscriptSpecExprs(s) {
					if n, ok := walk(*x); ok {
						return n, true
					}
				}
			}
			return 0, false
		default:
			return 0, false
		}
	}
	return walk(e)
}

// indexNamePart is the auto-name part for one index key element (spec/design/indexes.md §2, PG's
// ChooseIndexColumnNames): a column key contributes its (lowercased) column name; a bare-function-
// call expression its function name (`lower(email)` → `lower`); any other expression the literal
// `expr`.
func indexNamePart(elem indexKeyElem) string {
	if elem.Expr == nil {
		return strings.ToLower(elem.Column)
	}
	if elem.Expr.Kind == exprFuncCall {
		return strings.ToLower(elem.Expr.FuncCall.Name)
	}
	return "expr"
}

// indexExprNonimmutableCall reports whether an index-key expression calls a non-immutable built-in
// (spec/design/indexes.md §2): the entropy/clock seam (uuidv4/uuidv7/now/clock_timestamp —
// current_timestamp desugars to now — and current_date, the bare keyword's own catalog function)
// or the sequence functions (nextval/currval/setval/lastval) / current_setting. Such a function
// would let the index drift from the table, so it is 42P17 at CREATE INDEX. The walk mirrors checkReferencedColumns (subqueries are already rejected by
// resolution). The session-timezone hazard (an expression over timestamptz) is handled separately
// by the caller, so this covers only calls.
func indexExprNonimmutableCall(e exprNode) bool {
	isNonimmutable := func(name string) bool {
		switch strings.ToLower(name) {
		case "uuidv4", "uuidv7", "now", "clock_timestamp", "current_date",
			"nextval", "currval", "setval", "lastval", "current_setting":
			return true
		}
		return false
	}
	var walk func(e exprNode) bool
	walk = func(e exprNode) bool {
		switch e.Kind {
		case exprFuncCall:
			if isNonimmutable(e.FuncCall.Name) {
				return true
			}
			for _, a := range e.FuncCall.Args {
				if walk(*a) {
					return true
				}
			}
			return false
		case exprCast:
			return walk(e.Cast.Inner)
		case exprExtract:
			return walk(e.Extract.Source)
		case exprCollate:
			return walk(e.Collate.Inner)
		case exprUnary:
			return walk(e.Unary.Operand)
		case exprIsNull:
			return walk(e.IsNullOf.Operand)
		case exprIsJson:
			return walk(e.IsJsonOf.Operand)
		case exprJsonCtor:
			return walk(e.JsonCtorOf.Operand)
		case exprJsonExists:
			return walk(e.JsonExists.Ctx) || walk(e.JsonExists.Path)
		case exprJsonValue:
			return walk(e.JsonValue.Ctx) || walk(e.JsonValue.Path)
		case exprJsonQuery:
			return walk(e.JsonQuery.Ctx) || walk(e.JsonQuery.Path)
		case exprBinary:
			return walk(e.Binary.Lhs) || walk(e.Binary.Rhs)
		case exprIsDistinct:
			return walk(e.IsDistinct.Lhs) || walk(e.IsDistinct.Rhs)
		case exprLike:
			return walk(e.Like.Lhs) || walk(e.Like.Rhs)
		case exprRegex:
			return walk(e.Regex.Lhs) || walk(e.Regex.Rhs)
		case exprIn:
			if walk(e.In.Lhs) {
				return true
			}
			for _, elem := range e.In.List {
				if walk(elem) {
					return true
				}
			}
			return false
		case exprBetween:
			return walk(e.Between.Lhs) || walk(e.Between.Lo) || walk(e.Between.Hi)
		case exprQuantified:
			return walk(e.Quantified.Lhs) || walk(e.Quantified.Array)
		case exprCase:
			if e.Case.Operand != nil && walk(*e.Case.Operand) {
				return true
			}
			for _, w := range e.Case.Whens {
				if walk(w.Cond) || walk(w.Result) {
					return true
				}
			}
			if e.Case.Els != nil && walk(*e.Case.Els) {
				return true
			}
			return false
		case exprCoalesce:
			// COALESCE is a pure combinator — immutable iff its arguments are (grammar.md §51).
			for _, a := range e.Coalesce {
				if walk(a) {
					return true
				}
			}
			return false
		case exprGreatestLeast:
			// GREATEST/LEAST is likewise a pure combinator — immutable iff its arguments are (§52).
			for _, a := range e.GreatestLeast {
				if walk(a) {
					return true
				}
			}
			return false
		case exprRow, exprArray:
			for _, it := range e.RowItems {
				if walk(it) {
					return true
				}
			}
			return false
		case exprFieldAccess, exprFieldStar:
			return walk(*e.Base)
		case exprSubscript:
			if walk(*e.Base) {
				return true
			}
			for _, s := range e.Subscripts {
				for _, x := range subscriptSpecExprs(s) {
					if walk(*x) {
						return true
					}
				}
			}
			return false
		default:
			// exprColumn / exprQualifiedColumn / exprLiteral / exprTypedLiteral / exprParam and the
			// resolution-rejected subquery/star forms carry no non-immutable call.
			return false
		}
	}
	return walk(e)
}
