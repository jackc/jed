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
