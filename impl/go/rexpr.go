package jed

// Resolved-expression and plan-node type definitions (mirrors impl/rust executor.rs). This file is
// the type vocabulary the resolver produces and the executor consumes: the resolved-type lattice
// (rtKind/resolvedType/compositeRType and promote/assignableTo/rtName), the resolved-expression node
// rExpr with its operation enums (rExprKind, scalarFunc/arrayFunc/rangeFunc/regexFunc, the JSON op
// kinds), and the plan-node structs (queryPlan/selectPlan/setOpPlan/valuesPlan, cteBinding/planRel/
// planJoin/orderSlot, srfPlan/jtPlan, evalEnv). Behaviour lives in resolve*.go and eval.go. (Named
// rexpr.go, not types.go — the latter already holds the scalar type system.)

// ============================================================================
// Resolved expression layer (mirrors impl/rust executor.rs).
//
// Parse → Expr (names) → resolve → rExpr (column indices, known result types, folded
// constants) → eval per row → Value. The resolver is where all type-checking and the
// literal range-check live; the evaluator is a pure tree-walk.
// ============================================================================

// rtKind tags the static type of a resolved expression.
type rtKind int

const (
	rtNull rtKind = iota // an untyped NULL literal
	rtInt                // integer; intTy carries the ScalarType
	rtBool
	rtText        // text (one family, collation C); does not promote
	rtDecimal     // decimal (one family; the per-column typmod is carried separately)
	rtBytea       // bytea (one family, raw bytes); does not promote
	rtUuid        // uuid (one family, fixed 16 bytes); does not promote. First non-integer key.
	rtTimestamp   // timestamp (zoneless instant); does not compare/cast to timestamptz
	rtTimestamptz // timestamptz (UTC instant); does not compare/cast to timestamp
	rtDate        // date (calendar date, i32 days); strict island, no compare/cast to timestamp
	rtInterval    // interval (a span); compares only with itself, by the canonical span
	rtFloat32     // f32 / real (binary32); promotes to f64; a strict island vs int/decimal
	rtFloat64     // f64 / double precision (binary64)
	// rtComposite is a composite (row) type (spec/design/composite.md §5): comp carries the
	// (optional) name and resolved field list. A named catalog type's name drives the `# types:`
	// output; an anonymous ROW(...) result has a nil name (rendered "record").
	rtComposite
	// rtArray is an array type (spec/design/array.md §2): elem carries the resolved element type.
	// Two arrays are comparable iff their element types are comparable; assignable to an array
	// column of the same element type.
	rtArray
	// rtRange is a range type (spec/design/ranges.md §2): elem carries the resolved element
	// (subtype) type. Two ranges are comparable iff their elements are equal; the element is one of
	// the six scalar subtypes that have a range.
	rtRange
	// rtJson is the json family (verbatim text — spec/design/json.md §4). NOT comparable (PG ships no
	// btree/hash opclass — §5): a comparison/ORDER BY/DISTINCT on json resolves to 42883.
	rtJson
	// rtJsonb is the jsonb family (canonical binary — spec/design/json.md §2). Comparable with itself
	// by PG's total btree order (§5).
	rtJsonb
	// rtJsonPath is the jsonpath type (spec/design/jsonpath.md, P1a). NOT comparable (42883);
	// literal-only.
	rtJsonPath
)

// isFloatKind reports whether a resolvedType is one of the two float kinds.
func isFloatKind(k rtKind) bool { return k == rtFloat32 || k == rtFloat64 }

// promoteFloat is the float promotion tower (compare.toml max-rank): a mixed-width pair widens to
// f64; same-width stays. Caller guarantees both kinds are float.
func promoteFloat(a, b rtKind) scalarType {
	if a == rtFloat64 || b == rtFloat64 {
		return scalarFloat64
	}
	return scalarFloat32
}

type resolvedType struct {
	kind  rtKind
	intTy scalarType      // valid when kind == rtInt
	comp  *compositeRType // valid when kind == rtComposite
	elem  *resolvedType   // valid when kind == rtArray (the element type)
}

// compositeRType is the resolved shape of a composite type — its (optional) name and resolved field
// list (spec/design/composite.md §5). name is "" (named=false) for an anonymous ROW(...) result, set
// for a named catalog type. fields are the resolved (field-name, type) pairs in declaration order.
type compositeRType struct {
	named  bool
	name   string
	fields []compositeRField
}

// compositeRField is one resolved (name, type) field of a compositeRType.
type compositeRField struct {
	name string
	ty   resolvedType
}

func intType(t resolvedType) (scalarType, bool) {
	if t.kind == rtInt {
		return t.intTy, true
	}
	return 0, false
}

// resolvedOfColumn is the resolved type of a stored column of ty — the output type of a bare
// column projection (SELECT * / SELECT col). A column always has a concrete type, never rtNull.
func resolvedOfColumn(ty scalarType) resolvedType {
	if ty.IsInteger() {
		return resolvedType{kind: rtInt, intTy: ty}
	}
	switch {
	case ty.IsBool():
		return resolvedType{kind: rtBool}
	case ty.IsText():
		return resolvedType{kind: rtText}
	case ty.IsDecimal():
		return resolvedType{kind: rtDecimal}
	case ty.IsBytea():
		return resolvedType{kind: rtBytea}
	case ty.IsTimestamp():
		return resolvedType{kind: rtTimestamp}
	case ty.IsTimestamptz():
		return resolvedType{kind: rtTimestamptz}
	case ty.IsDate():
		return resolvedType{kind: rtDate}
	case ty.IsInterval():
		return resolvedType{kind: rtInterval}
	case ty.IsFloat32():
		return resolvedType{kind: rtFloat32}
	case ty.IsFloat64():
		return resolvedType{kind: rtFloat64}
	default: // uuid
		return resolvedType{kind: rtUuid}
	}
}

// assignableTo reports whether a projected value of type t is assignable to a colTy column for
// storage — the FAMILY-level gate INSERT ... SELECT applies up front (spec/design/grammar.md
// §24), before any row is produced (so it fires even over an empty source). It is the
// family-level subset of storeValue and MUST agree with it: an integer assigns to an integer
// or decimal column (int→decimal widens), a decimal only to a decimal column (decimal→int is
// explicit-CAST only), text to text/uuid/bytea/timestamp/timestamptz (the documented text
// adaptation — the per-row store then parses, trapping 22P02/22007 on malformed input),
// boolean→boolean, uuid→uuid, bytea→bytea, a timestamp only to a timestamp column and a
// timestamptz only to a timestamptz column (the two never cross — they do not even compare,
// timestamp.md), and a NULL-typed projection to any column (a NOT NULL target then traps 23502
// per row). A non-assignable pair is a 42804.
func assignableTo(t resolvedType, colTy scalarType) bool {
	switch t.kind {
	case rtNull:
		return true
	case rtInt:
		return colTy.IsInteger() || colTy.IsDecimal()
	case rtDecimal:
		return colTy.IsDecimal()
	case rtBool:
		return colTy.IsBool()
	case rtText:
		return colTy.IsText() || colTy.IsUuid() || colTy.IsBytea() ||
			colTy.IsTimestamp() || colTy.IsTimestamptz() || colTy.IsInterval() || colTy.IsDate()
	case rtBytea:
		return colTy.IsBytea()
	case rtUuid:
		return colTy.IsUuid()
	case rtTimestamp:
		return colTy.IsTimestamp()
	case rtTimestamptz:
		return colTy.IsTimestamptz()
	case rtDate:
		return colTy.IsDate()
	case rtInterval:
		return colTy.IsInterval()
	case rtFloat32:
		// f32 assigns to a f32 OR a f64 column (the implicit, lossless widen — §2).
		return colTy.IsFloat32() || colTy.IsFloat64()
	case rtFloat64:
		// f64 assigns only to a f64 column (f64→f32 is explicit-CAST only).
		return colTy.IsFloat64()
	default:
		return false
	}
}

// rtName is t's type name, for a 42804 assignability message (the integer width is exact).
// typeNames renders a projection's resolved types as their canonical names for the public
// outcome.ColumnTypes — the `# types:` directive's assertion surface (spec/design/conformance.md
// §7). Same names as the 42804 message (rtName): the exact integer width, the unconstrained
// "decimal".
func typeNames(ts []resolvedType) []string {
	out := make([]string, len(ts))
	for i, t := range ts {
		out[i] = rtName(t)
	}
	return out
}

func rtName(t resolvedType) string {
	switch t.kind {
	case rtInt:
		return t.intTy.CanonicalName()
	case rtBool:
		return "boolean"
	case rtText:
		return "text"
	case rtDecimal:
		return "decimal"
	case rtBytea:
		return "bytea"
	case rtUuid:
		return "uuid"
	case rtTimestamp:
		return "timestamp"
	case rtTimestamptz:
		return "timestamptz"
	case rtDate:
		return "date"
	case rtInterval:
		return "interval"
	case rtFloat32:
		return "f32"
	case rtFloat64:
		return "f64"
	case rtJson:
		return "json"
	case rtJsonb:
		return "jsonb"
	case rtJsonPath:
		return "jsonpath"
	case rtComposite:
		// A named composite is its type name; an anonymous ROW(...) is "record" (PG).
		if t.comp != nil && t.comp.named {
			return t.comp.name
		}
		return "record"
	case rtArray:
		if t.elem != nil {
			return rtName(*t.elem) + "[]"
		}
		return "array"
	case rtRange:
		// A range names itself by its element subtype (i32 → i32range — spec/design/ranges.md).
		if t.elem != nil {
			if s, ok := resolvedRangeElementScalar(t.elem); ok {
				if name, ok2 := rangeNameForElement(s); ok2 {
					return name
				}
			}
			return "range<" + rtName(*t.elem) + ">"
		}
		return "range"
	default:
		return "unknown"
	}
}

// resolvedRangeElementScalar returns the scalar element type of a resolved range element. A range's
// element is always one of the six scalar subtypes; ok is false for anything else (never a valid
// range). Used to name a range and to build its codec.
func resolvedRangeElementScalar(elem *resolvedType) (scalarType, bool) {
	switch elem.kind {
	case rtInt:
		return elem.intTy, true
	case rtDecimal:
		return scalarDecimal, true
	case rtTimestamp:
		return scalarTimestamp, true
	case rtTimestamptz:
		return scalarTimestamptz, true
	case rtDate:
		return scalarDate, true
	default:
		return 0, false
	}
}

// ctxOf returns the type a sibling operand offers an adaptable operand. For an integer literal
// this is the integer width it adopts; for a string literal, bytea/uuid/text (so it can decode
// the hex/uuid input); a bind parameter additionally adopts a decimal/boolean sibling (a literal
// ignores those — its arm keeps i64/text — so widening the mapping is safe). Only a bare NULL
// offers no context (spec/design/api.md §5).
func ctxOf(t resolvedType) *scalarType {
	switch t.kind {
	case rtInt:
		ty := t.intTy
		return &ty
	case rtBytea:
		ty := scalarBytea
		return &ty
	case rtUuid:
		ty := scalarUuid
		return &ty
	case rtText:
		ty := scalarText
		return &ty
	case rtBool:
		ty := scalarBool
		return &ty
	case rtDecimal:
		ty := scalarDecimal
		return &ty
	case rtTimestamp:
		ty := scalarTimestamp
		return &ty
	case rtTimestamptz:
		ty := scalarTimestamptz
		return &ty
	case rtDate:
		ty := scalarDate
		return &ty
	case rtInterval:
		ty := scalarInterval
		return &ty
	case rtFloat32:
		ty := scalarFloat32
		return &ty
	case rtFloat64:
		ty := scalarFloat64
		return &ty
	case rtJson:
		// A json/jsonb sibling offers its type so a string literal parses as that type.
		ty := scalarJson
		return &ty
	case rtJsonb:
		ty := scalarJsonb
		return &ty
	case rtJsonPath:
		ty := scalarJsonPath
		return &ty
	default:
		return nil
	}
}

// rExprKind tags a resolved expression node.
type rExprKind int

const (
	reColumn rExprKind = iota
	// reParam is a bind parameter, by 0-based index into the bound-values slice passed to eval.
	// Its static type was inferred from context at resolve (spec/design/api.md §5); the value is
	// supplied (and coerced) before evaluation.
	reParam
	reConstInt
	reConstBool
	reConstText
	reConstDecimal
	reConstBytea
	reConstUuid
	reConstTimestamp
	reConstTimestamptz
	reConstDate
	reConstInterval
	reConstFloat32
	reConstFloat64
	// reConstJson is a json constant — JSON text stored VERBATIM (spec/design/json.md §4), validated
	// well-formed at resolve. Held in cText (the verbatim text).
	reConstJson
	// reConstJsonb is a jsonb constant — the canonical tagged-node tree (spec/design/json.md §2),
	// parsed + canonicalized at resolve. Held in cJsonb.
	reConstJsonb
	// reConstJsonPath is a jsonpath constant — the canonical normalized source text
	// (spec/design/jsonpath.md, P1a), compiled + rendered at resolve. Held in cText.
	reConstJsonPath
	reConstNull
	reCast
	// reArrayCast is a cast that INVOLVES an array type (spec/design/array.md §7), none expressible
	// by the scalar reCast node (whose `result` is a ScalarType): runtime text → T[] (array_in per
	// row), array → text (array_out per row), and element-wise array → other-element-array (each
	// element through the scalar cast). `castElem` is the target element ColType for the two
	// array-producing casts and nil for array → text; the eval branches on the runtime value.
	reArrayCast
	reNeg
	reNot
	reArith
	reCompare
	reAnd
	reOr
	reIsNull
	// reIsJson is `operand IS [NOT] JSON …` (json-sql-functions.md §5): well-formedness + optional
	// kind / unique-keys test over a string / json / jsonb operand. A NULL operand → NULL; else a
	// definite boolean (NOT-negated when `negated`). `jpKind` selects the kind word; `jpUnique`
	// selects WITH UNIQUE KEYS.
	reIsJson
	// reJsonCtor is `JSON(text [(WITH|WITHOUT) UNIQUE [KEYS]])` (json-sql-functions.md §5): validate a
	// string as a `json` value (verbatim). The operand reuses `operand`; `jpUnique` carries WITH UNIQUE
	// KEYS. A NULL operand → NULL; a malformed string → 22P02; a duplicate key under jpUnique → 22030.
	reJsonCtor
	reDistinct
	reLike
	// reRegex is `lhs ~ rhs` / `~*` / `!~` / `!~*` — a regular-expression match (regex.md). Matched
	// by the hand-written Pike VM (regex.go); negated carries `!~`/`!~*`, insensitive carries
	// `~*`/`!~*` (both sides simple-lowercased like ILIKE). A constant pattern is precompiled once.
	reRegex
	// reCasing is upper(text)/lower(text) — Unicode case folding (collation.md §16). casingUpper
	// selects the direction; folds via the engine-global property table or the ASCII baseline.
	reCasing
	// reAtTimeZone is `value AT TIME ZONE zone` (grammar.md §49, timezones.md §6), desugared from the
	// operator and a bare timezone(zone, value) call. lhs is the zone (text), rhs the value;
	// atTzToTimestamptz selects the direction (false: timestamptz→timestamp; true: timestamp→
	// timestamptz). Reads the engine-global loaded zone set; unknown zone 22023, NULL propagates,
	// ±infinity passes through.
	reAtTimeZone
	// reDateTrunc is date_trunc(unit, value[, zone]) (timezones.md §9.1). sargs is [unit, value] or
	// [unit, value, zone]; for a timestamptz value the truncation is in zone (3-arg) or the session
	// zone (2-arg), charging the timezone unit. The result family is the value family.
	reDateTrunc
	// reExtract is EXTRACT(field FROM value) (timezones.md §9.2). cText is the lowercased field
	// (validated at resolve); operand is the value. For a timestamptz value every field but `epoch` is
	// computed in the session zone (charging timezone).
	reExtract
	// reDateConvert is a cross-family datetime cast (timezones.md §9.3): operand cast to `result`
	// (timestamp/timestamptz/date) from another datetime family — or the runtime text → date cast
	// (date.md §6). The casts crossing the timestamptz boundary consult the session zone (charging
	// timezone); ±infinity and NULL pass through.
	reDateConvert
	// reDateClock is a clock-relative date literal — 'today' / 'now' (0), 'tomorrow' (+1),
	// 'yesterday' (−1) — resolved to a STABLE node, never folded (date.md §6): at eval it reads the
	// STATEMENT clock (once per statement, like now()) and takes its day in the SESSION zone
	// (charging timezone), shifted by cInt days. Flagged non-immutable at birth (42P17 in an index
	// expression). 'epoch' is not this node — it folds to the constant 1970-01-01.
	reDateClock
	reCase
	// reCoalesce is COALESCE(a, b, …) (spec/design/grammar.md §51) — lazy like reCase: arguments
	// (in `sargs`) are evaluated left to right, each at most once, stopping at the first non-NULL
	// (the second sanctioned short-circuit, cost.md §3). Argument types unify exactly like CASE
	// result arms; `caseDecimal` is reused for the widen-to-decimal flag.
	reCoalesce
	// reGreatestLeast is GREATEST(a, b, …) / LEAST(a, b, …) (spec/design/grammar.md §52) — the
	// variadic max/min. EAGER (unlike reCoalesce): every argument (in `sargs`) is evaluated. NULL
	// arguments are ignored; the result is NULL only when every argument is NULL. `greatest`
	// selects max vs min; the winner is chosen by the unified type's total order (valueCmp).
	// `caseDecimal` is reused for the widen-to-decimal flag.
	reGreatestLeast
	// reScalarFunc is a scalar-function call (abs/round, spec/design/functions.md §9),
	// evaluated per row in any context.
	reScalarFunc
	// reArrayFunc is a polymorphic array-function call (spec/design/array-functions.md §3),
	// evaluated per row. Distinct from reScalarFunc: it resolves over anyarray/anyelement (§2) and
	// its builders return an array; NULL handling is per-kernel (the introspectors propagate, the
	// builders are non-strict), so there is no blanket NULL short-circuit at eval.
	reArrayFunc
	// reRangeFunc is a polymorphic range accessor call (spec/design/range-functions.md §1 — lower/
	// upper/isempty/lower_inc/upper_inc/lower_inf/upper_inf), evaluated per row. Like reArrayFunc it
	// resolves over a pseudo-family (anyrange, binding ELEM := the element type) and reuses `sargs`
	// for its single range argument; `rfunc` selects the kernel. All are STRICT (a NULL range → NULL,
	// handled in the kernel). The result type lives in the surrounding resolvedType.
	reRangeFunc
	// reRegexFunc is a regex scalar function call (spec/design/regex.md §8 — regexp_replace → text,
	// regexp_match → text[]). Like reArrayFunc the result type lives in the surrounding resolvedType;
	// `rxFunc` selects the kernel and its arg nodes reuse `sargs`. STRICT (a NULL arg → NULL). A
	// constant pattern is precompiled into rxProgram (regex.md §5), charged once via rxCompileCharged.
	reRegexFunc
	// reRangeCtor is a range CONSTRUCTOR call (spec/design/range-functions.md §2 — i32range(lo, hi[,
	// bounds]) and the five siblings), evaluated per row. `relem` is the range's element scalar (the
	// result range type is recovered from it, a bijection); its 2 bound nodes plus an optional
	// bounds-flags TEXT node reuse `sargs`. Non-strict (null = "none"): a NULL bound is an infinite
	// bound, handled in the kernel — there is no blanket NULL short-circuit. The kernel coerces each
	// bound to relem (assignment-style), reads the bounds flags, and finalizes (canonicalize /
	// order-check / empty-normalize).
	reRangeCtor
	// reRangeOp is a range BOOLEAN operator (spec/design/range-functions.md §3 — @> <@ && << >> &< &>
	// -|-), evaluated per row. Its two operand nodes reuse `sargs`; `rop` selects the kernel. STRICT —
	// a NULL operand → NULL (handled in the eval arm). `relem` carries the range's element scalar, used
	// only by the roContainsElem/roElemContainedBy element overloads to coerce the bare-element operand
	// to the range's element type at eval; unused (but carried) for the range-against-range operators.
	reRangeOp
	// reRangeSetOp is a range SET operator (spec/design/range-functions.md §4 — `+` union, `-`
	// difference, `*` intersection, and range_merge), evaluated per row. Its two range operand nodes
	// reuse `sargs`; `rsop` selects the kernel. STRICT — a NULL operand → NULL (handled in the eval
	// arm). Unlike reRangeOp it carries no element scalar — the kernels work off the self-describing
	// operand values, and the result range type is fixed at resolve. The kernels (rangeUnion/
	// rangeIntersect/rangeMinus) live in range.go; `+`/`-` raise 22000 on a non-contiguous result.
	reRangeSetOp
	// reVariadic is a VARIADIC argument-counting call (spec/design/array-functions.md §12 —
	// num_nulls/num_nonnulls). Non-strict (null = "none"): no blanket NULL short-circuit. Its
	// argument nodes reuse `sargs`; `variadicArray` records the call shape (false = the spread form,
	// counting sargs' null-ness directly; true = the VARIADIC-array form, one sargs operand whose
	// flattened elements are counted, a NULL whole-array → NULL). Result is always i32.
	reVariadic
	// reJsonBuild is a VARIADIC json/jsonb builder (json-sql-functions.md §2 — json[b]_build_array /
	// _object). Non-strict: a NULL argument is included as JSON null (array) or a value (object).
	// `jbKind` selects array vs object; `jbJson` selects the json (compact / PG builder-spacing) vs
	// jsonb (canonical) render; `variadicArray` records the VARIADIC-array call shape (the lone array
	// operand is spread; a NULL whole-array → NULL). Argument nodes reuse `sargs`. The result type
	// (json/jsonb) is fixed at resolve from the catalog.
	reJsonBuild
	// reJsonSetInsert is `jsonb_set` / `jsonb_insert` (json-sql-functions.md §2): a jsonb path
	// mutation. `sargs` is `[target jsonb, path text[], value jsonb, flag boolean]` — STRICT (any
	// NULL → SQL NULL, including a NULL path element). `psMode` selects replace-or-create (Set) vs
	// insert (Insert); the boolean flag is create_if_missing (Set) / insert_after (Insert).
	reJsonSetInsert
	// reJsonObject is `json_object` / `jsonb_object` (json-sql-functions.md §2): build an object from
	// text array(s). `sargs` is one `text[]` of alternating keys/values, or two `text[]` (keys,
	// values). Every VALUE becomes a JSON string (a NULL value → JSON null); a NULL key → 22004; an
	// odd one-array / mismatched two-array length → 2202E. STRICT in the whole array argument(s) (a
	// NULL array → SQL NULL). `jbJson` true ⇒ the json result (insertion order + dups + " : "
	// spacing); false ⇒ the jsonb result (canonical: last-wins dedup + sorted keys).
	reJsonObject
	// reJsonPathFn is a scalar jsonpath query function (P2, jsonpath.md §5): jsonb_path_exists /
	// jsonb_path_query_first / jsonb_path_query_array. `sargs` = [ctx jsonb, path jsonpath]; STRICT
	// (any NULL → SQL NULL). `jpFnKind` selects which function. The path is recompiled from its
	// canonical text at eval.
	reJsonPathFn
	// reJsonSqlFn is a SQL/JSON query function JSON_EXISTS / JSON_VALUE / JSON_QUERY (json-sql-functions.md
	// §5, S2). `sargs` = [ctx, path]: ctx produces the context jsonb (or json/text, coerced), path the
	// jsonpath; a NULL ctx/path → SQL NULL. `jsKind` selects which function; `result`/`typmod` the
	// RETURNING type; `jsWrapper`/`jsKeepQuotes`/`jsOnEmpty`/`jsOnError` drive the result. A SQL/JSON
	// (class-22) error honors ON ERROR; anything else propagates.
	reJsonSqlFn
	// reOuterColumn is a correlated column reference (spec/design/grammar.md §26): the column
	// `index` of the enclosing row `level` hops out (1 = immediate parent). A leaf.
	reOuterColumn
	// reSubquery is a CORRELATED subquery, re-executed per outer row at eval (uncorrelated ones
	// are folded to a constant / reInValues before exec).
	reSubquery
	// reInValues is a folded uncorrelated `IN (subquery)`: the subquery ran once yielding `list`;
	// per row it tests `lhs` for three-valued membership.
	reInValues
	// reQuantified is a quantified array comparison `lhs op ANY/ALL(array)`
	// (spec/design/array-functions.md §11) — the array spelling of IN. `lhs` is the scalar, `rhs`
	// the array node, `op` the comparison, `quantAll` true for ALL. At eval the three-valued fold
	// over the array's flattened elements reuses the IN-list membership semantics, charging per
	// element like reInValues.
	reQuantified
	// reRow is a ROW(...) composite constructor (spec/design/composite.md §1): its field nodes are
	// held in sargs (so the existing fold / references-outer / touched-set walks recurse into them
	// for free). Evaluates to a ValComposite; one operator_eval per node (cost.md §9).
	reRow
	// reArray is an ARRAY[...] constructor (spec/design/array.md §1): its element nodes are held in
	// sargs (so the fold / references-outer / touched-set walks recurse for free). Evaluates to a
	// ValArray; one operator_eval per node. `nested` stacks sub-arrays into a higher dimension (§4).
	reArray
	// reConstArray is a folded array constant (the value_to_rexpr equivalent), preserving its shape;
	// it evaluates to its ValArray directly (cArray).
	reConstArray
	// reConstRange is a folded range constant ('[1,5)'::i32range, already canonicalized at resolve);
	// it evaluates to its ValRange directly (cRange).
	reConstRange
	// reField is field selection `(composite).field` (spec/design/composite.md §S4): evaluate
	// `operand` (the base) to a composite value and return its `index`-th field (the field ordinal,
	// fixed at resolve). A whole-value-NULL composite yields NULL for any field. One operator_eval
	// per node (cost.md §9).
	reField
	// reSubscript is array element subscript `operand[sub]` (spec/design/array.md §6): evaluate
	// `operand` (the base array) and `sub` (the index) and return the 1-based element. A NULL array,
	// a NULL index, or an out-of-bounds index yields NULL (PG — never an error). One operator_eval
	// per node.
	reSubscript
	// reJsonGet is a jsonb accessor operator (`-> ->> #> #>>`, spec/design/json-sql-functions.md §1, J4).
	// `jgop` selects field/index vs path and text-vs-jsonb; `lhs` evaluates to a jsonb document; `rhs` is
	// the key (text), array index (integer), or path (`text[]`). The result is jsonb (`-> #>`) or text
	// (`->> #>>`), and is SQL NULL when the access misses (or when base/arg is NULL — the operators are
	// strict).
	reJsonGet
	// reJsonContains is `a @> b` jsonb deep containment (spec/design/json-sql-functions.md §1, J5):
	// does `a` contain `b`. `<@` resolves to this with the operands swapped (`lhs`=a, `rhs`=b).
	// Boolean; strict (a NULL operand → NULL).
	reJsonContains
	// reJsonHasKey is `jsonb ? text` / `?| text[]` / `?& text[]` key-existence
	// (spec/design/json-sql-functions.md §1, J5). `hasKey` selects one-key / any-key / all-keys;
	// `lhs` is the jsonb base, `rhs` the text key or text[] of keys. Boolean; strict.
	reJsonHasKey
	// reJsonConcat is `a || b` jsonb concatenate / shallow-merge (spec/design/json-sql-functions.md
	// §1, J6). `lhs`/`rhs` are the two jsonb operands. Result jsonb; strict (a NULL operand → NULL).
	reJsonConcat
	// reJsonDelete is `jsonb - text|int|text[]` (delete key/index/keys) and `jsonb #- text[]` (delete
	// at path) — the J6 mutation deletes (spec/design/json-sql-functions.md §1). `delKind` selects the
	// form; `lhs` is the jsonb document, `rhs` the key/index/key-array/path. Result jsonb; strict; a
	// delete from a scalar (or an integer index into an object) is `22023`.
	reJsonDelete
)

// jsonGetOp selects which jsonb accessor operator an reJsonGet node applies
// (spec/design/json-sql-functions.md §1).
type jsonGetOp int

const (
	jgArrow         jsonGetOp = iota // `->` — field by key (text arg) or element by index (integer arg); result jsonb.
	jgArrowText                      // `->>` — same access, rendered as text.
	jgHashArrow                      // `#>` — get at a `text[]` path; result jsonb.
	jgHashArrowText                  // `#>>` — get at a `text[]` path, rendered as text.
)

// hasKeyKind selects which jsonb key-existence operator an reJsonHasKey node applies
// (spec/design/json-sql-functions.md §1, J5).
type hasKeyKind int

const (
	hkOne hasKeyKind = iota // `?` — a single key (text) exists.
	hkAny                   // `?|` — any key of a `text[]` exists.
	hkAll                   // `?&` — all keys of a `text[]` exist.
)

// deleteKind selects which jsonb delete form an reJsonDelete node applies
// (spec/design/json-sql-functions.md §1, J6).
type deleteKind int

const (
	dkKey   deleteKind = iota // `jsonb - text` — delete a key (object) or matching string elements (array).
	dkIndex                   // `jsonb - int` — delete the array element at an index.
	dkKeys                    // `jsonb - text[]` — delete each key.
	dkPath                    // `jsonb #- text[]` — delete the element at a path.
)

// subqueryKind selects which subquery form an reSubquery node is (spec/design/grammar.md §26).
type subqueryKind int

const (
	sqScalar subqueryKind = iota
	sqExists
	sqIn
	// sqQuantified is `lhs op ANY/ALL(SELECT …)` (array-functions.md §11.6): the node carries the
	// comparison `op` and `quantAll`, and the body's single column folds through quantifiedMembership
	// exactly like the array form. Survives as an reSubquery node only when CORRELATED; an
	// uncorrelated one is folded to a constant-array reQuantified.
	sqQuantified
)

// scalarFunc selects a scalar function (kind = "function"). The overload (integer vs decimal)
// is recovered at eval from the argument's runtime value.
type scalarFunc int

const (
	sfAbs scalarFunc = iota
	sfRound
	// Float scalar functions (spec/design/float.md §8). The exact / correctly-rounded set
	// (in-contract): sfFloatAbs, sfCeil, sfFloor, sfTrunc, sfFloatRound (1- and 2-arg), sfSqrt.
	// The transcendental set (exempted, native libm): sfExp, sfLn, sfLog10, sfPow, sfSin, sfCos,
	// sfTan. The width of the call is recorded in `result` (Float32/Float64).
	sfFloatAbs
	sfCeil
	sfFloor
	sfTrunc
	sfFloatRound
	sfSqrt
	sfExp
	sfLn
	sfLog10
	sfPow
	// sfLog — base-10 (1-arg) / arbitrary-base (2-arg) logarithm over decimal (decimal.md §8).
	// Decimal-only (no float `log`); the EXACT-numeric kernel, IN-CONTRACT.
	sfLog
	sfSin
	sfCos
	sfTan
	// sfCbrt is the real cube root (float.md §8) — transcendental, exempted; no domain
	// restriction (cbrt of a negative is the negative real root).
	sfCbrt
	// sfPi is the constant π as f64 (float.md §8) — zero-arg, IN-CONTRACT (same f64 literal in
	// every core), not in the transcendental ledger.
	sfPi
	// sfRadians is degrees → radians (float.md §8): x · RADIANS_PER_DEGREE. A single
	// correctly-rounded IEEE multiply, IN-CONTRACT (not ledgered).
	sfRadians
	// sfDegrees is radians → degrees (float.md §8): x / RADIANS_PER_DEGREE. A single
	// correctly-rounded IEEE divide, IN-CONTRACT (not ledgered).
	sfDegrees
	// sfAsin is the inverse sine in radians (float.md §8) — transcendental, exempted; domain
	// [-1, 1], |x| > 1 (or ±Inf) → 22003, NaN propagates.
	sfAsin
	// sfAcos is the inverse cosine in radians (float.md §8) — transcendental, exempted; same
	// domain [-1, 1] as asin.
	sfAcos
	// sfAtan is the inverse tangent in radians (float.md §8) — transcendental, exempted; no
	// domain restriction (atan(±Inf) = ±π/2).
	sfAtan
	// sfAtan2 is the quadrant-aware inverse tangent of y/x (float.md §8) — transcendental,
	// exempted; two float operands (widened to f64), no domain trap.
	sfAtan2
	// sfCot is the cotangent, 1/tan(x) (float.md §8) — transcendental, exempted; cot(0) =
	// +Infinity (no trap).
	sfCot
	// Hyperbolic functions (float.md §8) — transcendental, exempted. sinh/cosh/tanh/asinh have no
	// domain trap (sinh/cosh overflow to ±Inf, PG-faithful); acosh traps below 1, atanh outside
	// [-1, 1] (atanh(±1) = ±Inf is admissible).
	sfSinh
	sfCosh
	sfTanh
	sfAsinh
	sfAcosh
	sfAtanh
	// sfSign is sign(x) → -1 / 0 / +1 (float.md §8). Decimal → numeric (scale 0), float → f64
	// (EXACT/in-contract; sign(NaN) = sign(±0) = 0, sign(±Inf) = ±1). Dispatches on the operand.
	sfSign
	// sfDiv is div(a, b) → numeric — the TRUNCATED (toward zero) integer quotient at scale 0
	// (PG div(numeric, numeric)). Computed exactly as (a − a%b)/b. Resolver-routed (the catalog
	// name "div" belongs to the `/` operator); integers promote; 22012 on a zero divisor.
	sfDiv
	// sfGcd is gcd(a, b) → the greatest common divisor (non-negative), EXACT/in-contract. Integer
	// operands → the promoted integer type (Euclid; an overflowing-magnitude result → 22003); a
	// decimal operand → numeric at scale max(sₐ, s_b). gcd(0, 0) = 0. Resolver-routed.
	sfGcd
	// sfLcm is lcm(a, b) → the least common multiple (non-negative), EXACT/in-contract, |a/gcd·b|.
	// lcm(_, 0) = 0. Integer → the promoted type (overflow → 22003); decimal → numeric. Resolver-routed.
	sfLcm
	// sfFactorial is factorial(n) → numeric — n! at scale 0 (PG factorial(bigint)). A negative
	// operand → 22003. The O(n) multiply loop is metered per step (decimal_work, guarded) so the
	// cost ceiling bounds a large factorial before its limb work runs (§13).
	sfFactorial
	// sfWidthBucket is width_bucket(op, low, high, count) → i32 — the equi-width histogram bucket.
	// Two overloads (numeric exact, float in f64); dispatches on the operand. 2201G on a bad count /
	// equal bounds (and, for float, a NaN operand / infinite bound); a result past int4 → 22003.
	sfWidthBucket
	// sfScale is scale(numeric) → i32 — the decimal's display (fractional-digit) scale (decimal.md).
	sfScale
	// sfMinScale is min_scale(numeric) → i32 — the smallest scale that represents the value exactly
	// (trailing fractional zeros dropped); zero has min_scale 0 (decimal.md).
	sfMinScale
	// sfTrimScale is trim_scale(numeric) → numeric — the value re-scaled down to its min_scale
	// (trailing zeros removed), value-identical (decimal.md).
	sfTrimScale
	// sfMakeInterval builds an interval from its (named/defaulted) integer components plus the
	// f64 secs (spec/design/functions.md §11). The one scalar function returning interval.
	sfMakeInterval
	// sfMakeTimestamp builds a zoneless timestamp from the named (un-defaulted) date/time fields
	// plus the f64 sec — the make_interval sibling (spec/design/functions.md §11).
	sfMakeTimestamp
	// sfMakeTimestamptz builds a timestamptz: as sfMakeTimestamp, then interprets the wall clock in
	// the session zone (6-arg) or the explicit timezone text (7-arg), charging one timezone unit (§11).
	sfMakeTimestamptz
	// sfMakeDate builds a date from (year, month, day) — the make_timestamp sibling
	// (spec/design/functions.md §11); a negative year is BC, year zero / bad fields trap 22008.
	sfMakeDate
	// sfCurrentDate is the SQL-standard niladic CURRENT_DATE (parser-desugared, also callable):
	// the statement clock's day in the session zone — the 'today' literal as a function
	// (spec/design/date.md §6). STABLE; charges one timezone unit beyond operator_eval.
	sfCurrentDate
	// sfDatePart is date_part(field, source) — the float8-returning EXTRACT twin
	// (spec/design/timezones.md §9.2): the shared extract kernel, then decimal → f64. The field is
	// a RUNTIME text value validated per row; a date source WIDENS TO MIDNIGHT (the timestamp
	// matrix applies — PG's own definition); a timestamptz source decomposes in the session zone.
	sfDatePart
	// uuid extractors (spec/design/functions.md §12): pure inspectors of a uuid's bits.
	// sfUuidExtractVersion → i16 (NULL off-RFC-variant); sfUuidExtractTimestamp → timestamptz
	// (the embedded instant for v1/v7, else NULL).
	sfUuidExtractVersion
	sfUuidExtractTimestamp
	// uuid generators (spec/design/entropy.md §3): volatile. sfUuidv4 → random; sfUuidv7 → ms
	// timestamp + monotonic counter + random, with an optional interval shift.
	sfUuidv4
	sfUuidv7
	// current-time functions (spec/design/entropy.md §5): sfNow → timestamptz, the statement clock
	// read ONCE and reused (STABLE; current_timestamp is parser sugar for it); sfClockTimestamp →
	// timestamptz, the clock seam read on EVERY call (VOLATILE).
	sfNow
	sfClockTimestamp
	// sequence value functions (spec/design/sequences.md §4/§6): sfNextval(text) → i64 advances
	// the named sequence and MUTATES the per-statement pending state (write path); sfCurrval(text)
	// → i64 is a pure session-state read. sfSetval(text, i64[, bool]) → i64 sets the counter
	// (also a write); sfLastval() → i64 reads the most-recent-nextval session value (pure read).
	sfNextval
	sfCurrval
	sfSetval
	sfLastval
	// session-variable read (spec/design/session.md §6.1): sfCurrentSetting(text[, bool]) → text reads
	// the named session variable from the session's variable map. STABLE; 42704 on an unset name unless
	// the two-arg missing_ok is true (→ NULL).
	sfCurrentSetting
	// json/jsonb processing functions (B1, spec/design/json-sql-functions.md §2). The json* and
	// jsonb* variants share a kernel; the only difference is the json overload parses the verbatim
	// text first. All propagate a SQL NULL input.
	// json[b]_typeof → the JSON type name (object/array/string/number/boolean/null).
	sfJsonbTypeof
	sfJsonTypeof
	// json[b]_array_length → the array element count; a non-array is 22023.
	sfJsonbArrayLength
	sfJsonArrayLength
	// json[b]_strip_nulls → recursively remove object members whose value is JSON null.
	sfJsonbStripNulls
	sfJsonStripNulls
	// jsonb_pretty → an indented multi-line render.
	sfJsonbPretty
	// to_jsonb(anyelement) → the JSON image of any value (the valueToNode kernel). STRICT.
	sfToJsonb
	// to_json(anyelement) → the JSON image as `json` (the valueToNode kernel rendered per elemJsonText:
	// a jsonb input canonical-spaced, a json input verbatim, everything else compact). STRICT.
	sfToJson
	// json_scalar(anyelement) → the value's JSON scalar as `json` (number/boolean/string). STRICT.
	// Other source types (date/timestamp/uuid/bytea/interval/float) are a deferred 0A000.
	sfJsonScalar
	// json_serialize(json|jsonb) → the value's text serialization (json verbatim, jsonb canonical).
	sfJsonSerialize
	// --- string / text functions (spec/design/string-functions.md). All STRICT (NULL propagates via
	// the generic scalarFunc short-circuit). Character functions count Unicode code points (Go strings
	// are UTF-8, so `range`/utf8.RuneCountInString); octet/bit functions count UTF-8 bytes.
	// length(text) → i32 — the number of characters (code points). length('héllo') = 5.
	sfLength
	// octet_length(text) → i32 — the number of UTF-8 bytes. octet_length('héllo') = 6.
	sfOctetLength
	// bit_length(text) → i32 — the number of UTF-8 bits = octet_length × 8. bit_length('héllo') = 48.
	sfBitLength
	// substr(text, start[, count]) → text — the function form of SUBSTRING (1-based, code-point
	// indexed). A negative count is 22011 (string-functions.md §3).
	sfSubstr
	// left(text, n) → text — the first n characters; a negative n drops the last |n| (§3).
	sfLeft
	// right(text, n) → text — the last n characters; a negative n drops the first |n| (§3).
	sfRight
	// lpad(text, length[, fill]) → text — left-pad to `length` chars with `fill` (default space);
	// a longer string truncates; an over-large length traps 54000 (§3).
	sfLpad
	// rpad(text, length[, fill]) → text — the right-hand mirror of lpad (§3).
	sfRpad
	// btrim(text[, chars]) → text — trim characters in the `chars` set from both ends (§3).
	sfBtrim
	// ltrim(text[, chars]) → text — trim the `chars` set from the LEADING end only (§3).
	sfLtrim
	// rtrim(text[, chars]) → text — trim the `chars` set from the TRAILING end only (§3).
	sfRtrim
	// replace(text, from, to) → text — replace every occurrence of substring `from` with `to` (§3).
	sfReplace
	// translate(text, from, to) → text — per-character map/delete by position in `from`/`to` (§3).
	sfTranslate
	// repeat(text, n) → text — the string concatenated n times; over-large result traps 54000 (§3).
	sfRepeat
	// reverse(text) → text — the code points in reverse order (§3).
	sfReverse
	// strpos(text, substring) → i32 — 1-based code-point position of the first match, else 0 (§3).
	sfStrpos
	// split_part(text, delimiter, n) → text — the n-th field of the split; n=0 traps 22023 (§3).
	sfSplitPart
	// starts_with(text, prefix) → boolean — true iff the string begins with `prefix` (§3).
	sfStartsWith
	// ascii(text) → i32 — the Unicode code point of the first character; empty → 0 (§3).
	sfAscii
	// chr(int) → text — the one-character string for a Unicode code point; bad point traps (§3).
	sfChr
	// initcap(text) → text — titlecase each word (ASCII word boundaries + ASCII case fold, §3).
	sfInitcap
	// to_hex(int) → text — lowercase hex of the value's 64-bit two's-complement pattern (§3).
	sfToHex
	// encode(bytea, format) → text — render bytes as hex / base64 / escape (§3).
	sfEncode
	// decode(text, format) → bytea — parse hex / base64 / escape back to binary (§3).
	sfDecode
	// quote_literal(text) → text — wrap as a SQL string literal (§3).
	sfQuoteLiteral
	// quote_ident(text) → text — wrap as a SQL identifier (§3).
	sfQuoteIdent
	// quote_nullable(text) → text — like quote_literal but NON-STRICT (NULL → 'NULL', §3).
	sfQuoteNullable
)

// arrayFunc selects a polymorphic array function (spec/design/array-functions.md §3). Each name is
// single-arity, so the name alone picks the kernel; the eval recovers everything else from the
// operand values (the array's own shape header).
type arrayFunc int

const (
	afNdims       arrayFunc = iota // array_ndims(anyarray) → i32; NULL for the empty array
	afLength                       // array_length(anyarray, integer) → i32; NULL if empty / out of range
	afLower                        // array_lower(anyarray, integer) → i32
	afUpper                        // array_upper(anyarray, integer) → i32
	afCardinality                  // cardinality(anyarray) → i32; 0 for the empty array
	afDims                         // array_dims(anyarray) → text; NULL for the empty array
	afAppend                       // array_append(anyarray, anyelement) → anyarray; non-strict; 22000 if multidim
	afPrepend                      // array_prepend(anyelement, anyarray) → anyarray
	afCat                          // array_cat(anyarray, anyarray) → anyarray; 2202E on incompatible dims
	afRemove                       // array_remove(anyarray, anyelement) → anyarray; 1-D/empty only (0A000); lb preserved
	afReplace                      // array_replace(anyarray, anyelement, anyelement) → anyarray; any dim, shape preserved
	afPosition                     // array_position(anyarray, anyelement[, integer]) → i32; 1-D/empty (0A000); NULL start 22004
	afPositions                    // array_positions(anyarray, anyelement) → i32[]; 1-D/empty only (0A000)
	afToJson                       // array_to_json(anyarray) → json; compact to_jsonb kernel; STRICT; multidim 0A000
	afContains                     // a @> b (anyarray, anyarray) → boolean; does a contain b; strict eq; any dim (§10)
	afContainedBy                  // a <@ b (anyarray, anyarray) → boolean; is a contained by b (b @> a) (§10)
	afOverlaps                     // a && b (anyarray, anyarray) → boolean; do a and b share an element; strict eq (§10)
)

// rangeFunc selects a polymorphic range accessor (spec/design/range-functions.md §1, RF1). Like
// arrayFunc each is single-arity, so the name alone picks the kernel; the eval recovers everything
// else from the operand range value (self-describing). All are STRICT (a NULL range → NULL).
type rangeFunc int

const (
	rfLower    rangeFunc = iota // lower(anyrange) → anyelement; NULL if empty / unbounded below
	rfUpper                     // upper(anyrange) → anyelement; NULL if empty / unbounded above
	rfIsEmpty                   // isempty(anyrange) → boolean
	rfLowerInc                  // lower_inc(anyrange) → boolean (false for empty / an infinite lower bound)
	rfUpperInc                  // upper_inc(anyrange) → boolean (false for empty / an infinite upper bound)
	rfLowerInf                  // lower_inf(anyrange) → boolean (false for the empty range)
	rfUpperInf                  // upper_inf(anyrange) → boolean (false for the empty range)
)

// regexFunc selects a regex scalar function kernel (spec/design/regex.md §8). Kernels in regex.go.
type regexFunc int

const (
	rxReplace regexFunc = iota // regexp_replace(source, pattern, replacement [, flags]) → text
	rxMatch                    // regexp_match(source, pattern [, flags]) → text[]
	rxLike                     // regexp_like(string, pattern [, flags]) → boolean (regex.md §8b)
	rxCount                    // regexp_count(string, pattern [, start [, flags]]) → integer
	rxSubstr                   // regexp_substr(string, pattern [, start [, N [, flags [, subexpr]]]]) → text
	rxInstr                    // regexp_instr(string, pattern [, start [, N [, endoption [, flags [, subexpr]]]]]) → integer
)

// rangeOp selects a range BOOLEAN operator (spec/design/range-functions.md §3, RF3). Each is a binary
// infix operator returning a definite boolean (a NULL operand short-circuits to NULL at eval, like the
// array containment operators). roContainsElem/roElemContainedBy are the element overloads of @>/<@
// (the other operand is a bare element coerced to the range's element type); the rest are
// range-against-range. The kernels live in range.go.
type rangeOp int

const (
	roContains        rangeOp = iota // a @> b — range a contains range b
	roContainsElem                   // r @> e — range r contains element e (the element overload of @>)
	roContainedBy                    // a <@ b — range a is contained by range b
	roElemContainedBy                // e <@ r — element e is contained by range r (the element overload of <@)
	roOverlaps                       // a && b — ranges a and b overlap
	roBefore                         // a << b — a is strictly left of b
	roAfter                          // a >> b — a is strictly right of b
	roOverleft                       // a &< b — a does not extend to the right of b
	roOverright                      // a &> b — a does not extend to the left of b
	roAdjacent                       // a -|- b — a and b are adjacent
)

// rangeSetOp selects a range SET operator (spec/design/range-functions.md §4, RF4). Each combines two
// ranges over a common element type into a new range. rsoUnion/rsoDifference raise 22000 on a
// non-contiguous result; rsoIntersect/rsoMerge never error. The kernels live in range.go.
type rangeSetOp int

const (
	rsoUnion      rangeSetOp = iota // a + b — union: the smallest single range covering both (22000 on a gap)
	rsoIntersect                    // a * b — intersection: the overlap (empty when the ranges are disjoint)
	rsoDifference                   // a - b — difference: the part of `a` not in `b` (22000 if `b` splits `a`)
	rsoMerge                        // range_merge(a, b) — like union but spans any gap silently (never errors)
)

// variadicFunc selects a VARIADIC argument-counting function (spec/design/array-functions.md §12).
// Both return i32; the call form (spread vs VARIADIC-array) lives on the rExpr node.
type variadicFunc int

const (
	vfNumNulls    variadicFunc = iota // num_nulls(VARIADIC "any") → i32 — count of NULL args/elements
	vfNumNonnulls                     // num_nonnulls(VARIADIC "any") → i32 — count of non-NULL args/elements
)

// jsonBuildKind selects which VARIADIC json/jsonb builder an reJsonBuild node is
// (json-sql-functions.md §2). The `json` flag (on the node) selects the json vs jsonb render.
type jsonBuildKind int

const (
	jbArray  jsonBuildKind = iota // json[b]_build_array — every argument is one array element (NULL → JSON null)
	jbObject                      // json[b]_build_object — alternating key/value args (odd count / NULL key → 22023)
)

// jsonSqlKind selects which SQL/JSON query function an reJsonSqlFn node is (json-sql-functions.md §5).
type jsonSqlKind int

const (
	// jsExists is JSON_EXISTS → boolean (non-empty sequence); errors honor ON ERROR (default FALSE).
	jsExists jsonSqlKind = iota
	// jsValue is JSON_VALUE → a single scalar coerced to the RETURNING type (default text).
	jsValue
	// jsQuery is JSON_QUERY → a json/jsonb value (wrapper / quotes controlled).
	jsQuery
)

// jsonPathFnKind selects which scalar jsonpath query function an reJsonPathFn node is (jsonpath.md §5).
type jsonPathFnKind int

const (
	// jpfExists is jsonb_path_exists → boolean (the sequence is non-empty).
	jpfExists jsonPathFnKind = iota
	// jpfQueryFirst is jsonb_path_query_first → the first sequence item, or NULL if empty.
	jpfQueryFirst
	// jpfQueryArray is jsonb_path_query_array → the sequence wrapped in a JSON array.
	jpfQueryArray
	// jpfMatch is jsonb_path_match → the single boolean the path/predicate produces (22038 if not
	// exactly one boolean item). jpfMatchSilent is the PostgreSQL @@ operator: the same match, but a
	// non-singleton/non-boolean result is suppressed to SQL NULL.
	jpfMatch
	jpfMatchSilent
)

// rExpr is a resolved expression over fixed column indices, ready to evaluate against a
// row. Arithmetic/neg nodes carry their (promotion-tower) result type in `result` so the
// computed value can be range-checked against it.
type rExpr struct {
	kind     rExprKind
	index    int            // reColumn
	cInt     int64          // reConstInt
	cBool    bool           // reConstBool
	cText    string         // reConstText
	cDec     Decimal        // reConstDecimal
	cBytea   []byte         // reConstBytea
	cIv      Interval       // reConstInterval
	cFloat   float64        // reConstFloat32 / reConstFloat64 (a f32 const is held as the f64 of its value)
	op       binaryOp       // reArith, reCompare
	result   scalarType     // reCast target; reNeg / reArith result type
	typmod   *decimalTypmod // reCast: a decimal target's numeric(p,s) typmod
	varchar  *uint32        // reCast: a varchar(n) text target's max length — truncate (types.md §15)
	castElem *colType       // reArrayCast: the target element ColType (nil ⇒ array → text)
	lhs      *rExpr         // reArith, reCompare, reAnd, reOr, reDistinct
	rhs      *rExpr         // reArith, reCompare, reAnd, reOr, reDistinct
	operand  *rExpr         // reCast, reNeg, reNot, reIsNull, reCasing
	negated  bool           // reIsNull, reDistinct
	// insensitive carries ILIKE (reLike) / ~* (reRegex); casingUpper selects upper vs lower
	// (reCasing) — both collation.md §16.
	insensitive bool
	casingUpper bool
	// atTzToTimestamptz selects the AT TIME ZONE direction (reAtTimeZone): false is timestamptz→
	// timestamp, true is timestamp→timestamptz (timezones.md §6).
	atTzToTimestamptz bool
	// reRegex / reRegexFunc: rxProgram is the precompiled NFA for a CONSTANT pattern (compiled once at
	// resolve, the `col ~ 'literal'` case — regex.md §5); nil means the pattern is non-constant
	// (compiled per row at eval). rxCompileCharged is the one-shot flag charging a precompiled
	// program's regex_compile cost once per statement execution (on first eval), not per row. rxFunc
	// selects the reRegexFunc kernel (regexp_replace / regexp_match); its arg nodes reuse `sargs`.
	rxProgram        *regexProgram
	rxCompileCharged bool
	rxFunc           regexFunc
	// collation is the derived collation of a reCompare (spec/design/collation.md §7). nil is the
	// C / default byte order (the unchanged fast path); non-nil is a loaded collation that orders the
	// ORDERING comparisons (< <= > >=) by its UCA sort key. =/<> stay byte-equality regardless
	// (deterministic-collation equality IS byte-identity), but it is derived + conflict-checked
	// (42P21) for every comparison op.
	collation *Collation

	// reCase: (condition, result) arms, the ELSE result (constNull for an implicit ELSE), and
	// whether the unified result type is decimal (so integer results widen to decimal at eval).
	// reCoalesce and reGreatestLeast reuse caseDecimal (their arguments live in `sargs`).
	caseArms    []rCaseArm
	caseEls     *rExpr
	caseDecimal bool
	// reGreatestLeast: true for GREATEST (max), false for LEAST (min).
	greatest bool

	// reScalarFunc: the scalar function (abs/round) and its argument nodes. `result` holds the
	// static result type — for abs over an integer it is the operand's integer type, so the
	// magnitude is range-checked at that boundary; otherwise decimal.
	sfunc scalarFunc
	sargs []*rExpr
	// reArrayFunc: the polymorphic array function; its argument nodes reuse `sargs`. The result
	// type lives in the surrounding resolvedType (carried out of resolve), not on the node — the
	// kernel produces the result value from the operands (an array value is self-describing).
	afunc arrayFunc
	// reRangeFunc: the polymorphic range accessor; its single range argument reuses `sargs`. Like
	// reArrayFunc the result type lives in the surrounding resolvedType (carried out of resolve), not
	// on the node — the kernel produces the result value from the operand (a range is self-describing).
	rfunc rangeFunc
	// reRangeCtor: the element scalar of the range being built (i32range → i32). The result range type
	// is recovered from it (a bijection); the bound/flags argument nodes reuse `sargs`. reRangeOp also
	// uses `relem` — the range's element scalar, for the element-overload coercion at eval.
	relem scalarType
	// reRangeOp: the range boolean operator kernel. Its two operand nodes reuse `sargs`.
	rop rangeOp
	// reRangeSetOp: the range set operator kernel (+ union, - difference, * intersection, range_merge).
	// Its two range operand nodes reuse `sargs`; no element scalar is carried (the kernels work off the
	// self-describing operand values).
	rsop rangeSetOp
	// reVariadic: the VARIADIC counting function and its call shape. Argument nodes reuse `sargs`;
	// `variadicArray` true ⇒ the VARIADIC-array form (one array operand), false ⇒ the spread form.
	vfunc         variadicFunc
	variadicArray bool
	// reJsonBuild: which json/jsonb builder + render. `jbKind` selects array vs object; `jbJson` true ⇒
	// the `json` (compact / builder-spacing) render, false ⇒ the `jsonb` (canonical) render. Argument
	// nodes reuse `sargs`; `variadicArray` (above) records the VARIADIC-array call shape.
	jbKind jsonBuildKind
	jbJson bool

	// reJsonPathFn: which scalar jsonpath query function (jsonb_path_exists / _query_first /
	// _query_array). Argument nodes are in `sargs` = [ctx jsonb, path jsonpath] (jsonpath.md §5).
	jpFnKind jsonPathFnKind

	// reJsonSqlFn: a SQL/JSON query function JSON_EXISTS / JSON_VALUE / JSON_QUERY (json-sql-functions.md
	// §5, S2). `sargs` = [ctx, path]; `result`/`typmod` hold the RETURNING scalar type + decimal typmod.
	// jsKind selects the function; jsWrapper/jsKeepQuotes/jsOnEmpty/jsOnError drive the result.
	jsKind       jsonSqlKind
	jsWrapper    jsonWrapper
	jsKeepQuotes bool
	jsOnEmpty    jsonOnBehavior
	jsOnError    jsonOnBehavior

	// reJsonSetInsert: which path mutation (jsonb_set vs jsonb_insert). Argument nodes are in
	// `sargs` = [target, path, value, flag] (json-sql-functions.md §2).
	psMode pathSetMode

	// reArray: `nested` marks a multidim-stacking constructor (its element nodes evaluate to
	// arrays, stacked into one higher dimension — spec/design/array.md §4).
	nested bool
	// reSubscript: the subscript specs applied to `operand`, and whether any is a slice (so the
	// whole access is a slice — spec/design/array.md §6).
	subs    []rSubscript
	isSlice bool
	// reConstArray: a folded array constant (its full shape preserved).
	cArray *ArrayVal
	// reConstRange: a folded range constant (already canonicalized).
	cRange *RangeVal
	// reConstJsonb: a folded jsonb constant — the canonical node tree (parsed + canonicalized at
	// resolve). A reConstJson holds its verbatim text in cText (no extra field).
	cJsonb *JsonNode

	// reIsJson: the optional kind word (json-sql-functions.md §5) and the WITH UNIQUE KEYS flag. The
	// operand reuses `operand`; `negated` carries IS NOT JSON.
	jpKind   jsonPredicateKind
	jpUnique bool

	// reJsonGet: the jsonb accessor operator (`-> ->> #> #>>`). `lhs` is the jsonb base, `rhs` the
	// key/index/path argument (spec/design/json-sql-functions.md §1).
	jgop jsonGetOp

	// reJsonHasKey: which key-existence operator (`?`/`?|`/`?&`) — one-key / any-key / all-keys.
	// `lhs` is the jsonb base, `rhs` the text key (`?`) or text[] of keys (`?|`/`?&`).
	hasKey hasKeyKind

	// reJsonDelete: which delete form (`-` key/index/keys, `#-` path). `lhs` is the jsonb base,
	// `rhs` the key (text) / index (int) / keys-or-path (text[]) argument.
	delKind deleteKind

	// reQuantified: `lhs` is the scalar, `rhs` the array node, `op` the comparison, `quantAll`
	// selects ALL (true) vs ANY/SOME (false) (spec/design/array-functions.md §11).
	quantAll bool

	// reOuterColumn: the number of frames out (`index` reuses the column index field).
	level int
	// reSubquery: the resolved inner plan, which form, and (for sqIn) the resolved lhs (`lhs`)
	// + the NOT flag (`negated`). reInValues: `lhs` + the constant `list` + `negated`.
	subPlan *queryPlan
	subKind subqueryKind
	list    []Value
}

// rSubscript is one resolved subscript spec in a reSubscript (spec/design/array.md §6): an index
// `a[i]` (isSlice false), or a slice `a[m:n]` whose bounds may be nil (omitted: `a[:n]`/`a[m:]`/`a[:]`).
type rSubscript struct {
	isSlice bool
	index   *rExpr
	lower   *rExpr
	upper   *rExpr
}

// ============================================================================
// Query plans — the resolved, owned form of a query, executable repeatedly (a correlated
// subquery is re-run once per outer row). planQuery (the resolve half of the old runSelect)
// produces a queryPlan; execQueryPlan (the execute half) consumes it against an outer-row
// environment. The split lets a subquery be resolved ONCE — so its structural/type errors fire
// even over an empty outer — yet executed many times (spec/design/grammar.md §26).
// ============================================================================

// queryPlan is a resolved query expression: a SELECT plan or a set-op plan (mirrors QueryExpr).
// Exactly one of sel / setop is non-nil.
type queryPlan struct {
	sel    *selectPlan
	setop  *setOpPlan
	values *valuesPlan
	with   *withPlan
}

// withPlan is a planned nested `WITH … query_expr` (spec/design/cte.md §7): the nested CTE bindings
// + their inline/materialize modes, and the inner query plan that references them. At execution the
// bindings are materialized once and body runs against a fresh CTE context (they establish their
// own scope — the enclosing context is NOT chained in, the documented narrowing §7).
type withPlan struct {
	bindings []*cteBinding
	modes    []cteMode
	body     queryPlan
}

// columnTypes returns the plan's output column types (for a subquery's plan-time column-count
// check + element type).
func (p *queryPlan) columnTypes() []resolvedType {
	if p.sel != nil {
		return p.sel.columnTypes
	}
	if p.values != nil {
		return p.values.columnTypes
	}
	if p.with != nil {
		return p.with.body.columnTypes()
	}
	return p.setop.columnTypes
}

// columnNames returns the plan's output column names — the basis for a CTE's synthetic relation
// when there is no column-rename list (spec/design/cte.md §1).
func (p *queryPlan) columnNames() []string {
	if p.sel != nil {
		return p.sel.columnNames
	}
	if p.values != nil {
		return p.values.columnNames
	}
	if p.with != nil {
		return p.with.body.columnNames()
	}
	return p.setop.columnNames
}

// valuesPlan is a resolved VALUES-body relation (spec/design/grammar.md §42), executable to its
// literal rows — the FROM-position sibling of INSERT … VALUES. rows[r][c] is row r, column c, each
// resolved as a CONSTANT (the body is non-LATERAL, planned parent=nil, so it reads no row).
// columnTypes is the per-column type unified across the rows like a set operation (§25), and
// columnNames is column1, column2, … (PostgreSQL; the derived table's optional column-rename list
// overrides them at the synthetic relation). All rows have len(columnTypes) values.
type valuesPlan struct {
	rows        [][]*rExpr
	columnTypes []resolvedType
	columnNames []string
}

// cteMode is how a referenced CTE is evaluated (spec/design/cte.md §3, cost.md §3). Decided per CTE
// from its reference count and [NOT] MATERIALIZED hint: a single-reference CTE is cteInline, a
// multi-reference (or MATERIALIZED) one is cteMaterialize.
type cteMode int

const (
	// cteInline runs the body in place at each reference (re-evaluates per outer row under
	// correlation, matching PostgreSQL); charges the body's intrinsic cost, no cte_scan_row.
	cteInline cteMode = iota
	// cteMaterialize runs the body once, buffers the rows; each reference scans the buffer,
	// charging cte_scan_row per buffered row.
	cteMaterialize
)

// cteBinding is a planned common table expression, owned by runWith for the whole statement
// (spec/design/cte.md). name is lowercased for case-insensitive FROM matching; table is the
// synthetic relation exposing the body's output columns; source is the planned body (a query plan,
// or — spec/design/writable-cte.md — a data-modifying statement); hint is the [NOT] MATERIALIZED
// override (nil = default); refs counts the FROM references resolved to it during planning (the
// inline-vs-materialize decision — cost.md §3).
//
// For a RECURSIVE CTE (spec/design/recursive-cte.md) source holds the non-recursive (anchor) term
// (its column types fix the synthetic relation's) and recursive carries the recursive term + the
// UNION ALL flag; the binding is in scope inside its own recursive term, so the self-reference
// resolves to it (refs then counts the self-reference too).
type cteBinding struct {
	name  string
	table *catTable
	// source is what this binding evaluates to (cte.md, writable-cte.md): a planned query body, or
	// a data-modifying statement (dm non-nil). Exactly one of plan/dm is meaningful (selected by dm).
	// A data-modifying CTE is always materialized (writable-cte.md §3), so the inline-execution path
	// never touches a dm binding.
	plan      queryPlan // valid when dm == nil
	dm        *dmCte    // non-nil for a data-modifying CTE binding
	recursive *recursiveTerm
	hint      *bool
	refs      int
}

// isDml reports whether this binding is a data-modifying CTE (its source is a statement, not a query
// plan) — writable-cte.md.
func (b *cteBinding) isDml() bool { return b.dm != nil }

// dmCte is a data-modifying CTE's body (spec/design/writable-cte.md): the INSERT/UPDATE/DELETE to run
// (cloned from the AST, executed with the statement's CTE context threaded in) and whether it has no
// RETURNING clause — in which case a FROM reference to it is 0A000 (§5). Exactly one of
// insert/update/delete is non-nil.
type dmCte struct {
	insert      *insert
	update      *update
	delete      *deleteStmt
	noReturning bool
}

// recursiveTerm is the recursive half of a WITH RECURSIVE CTE (spec/design/recursive-cte.md §4):
// the planned recursive term (the UNION's right operand, which references the CTE once) and whether
// the body is UNION ALL (keep every row) versus UNION (drop rows duplicating any already emitted).
type recursiveTerm struct {
	plan     queryPlan
	unionAll bool
}

// cteCtx is the per-statement CTE execution context, threaded through exec* and evalEnv so a FROM
// reference (any nesting depth) can deliver a CTE's rows (spec/design/cte.md §5). modes and bindings
// are fixed after planning; buffers is filled before the main query runs — one slot per CTE in list
// order, holding the materialized rows of a cteMaterialize CTE (an empty placeholder for a cteInline
// one, whose body is run in place from bindings[ci].plan instead). bindings also serves a
// data-modifying CTE's own inner queries, which resolve against the earlier bindings when the
// writable-CTE orchestrator executes them (writable-cte.md §2). The zero value (all nil) is the empty
// context — no CTEs in scope (every non-WITH execution path).
type cteCtx struct {
	modes    []cteMode
	bindings []*cteBinding
	buffers  [][]storedRow
}

// planRel is one relation in a SELECT plan: the table name (looked up in the store at exec), the
// flat offset of its first column, and its column count (for NULL-padding).
type planRel struct {
	tableName string
	// db is the relation's explicit database qualifier (attached-databases.md §3), passed to the
	// scope-aware store funnels at exec (lkpStoreScoped etc.). nil for a bare implicit-scope name → the
	// funnels fall through to the temp-first walk (behavior-neutral for every unqualified query).
	db       *string
	offset   int
	colCount int
	// srf is non-nil when this relation is a COMPUTED set-returning function (generate_series)
	// rather than a base table: tableName is then the function name (never looked up in the
	// store) and the executor generates the rows instead of scanning (functions.md §10).
	srf *srfPlan
	// cte is non-nil (pointing to the index into the statement's CTE list — spec/design/cte.md)
	// when this relation is a reference to a common-table expression rather than a base table:
	// tableName is then the CTE name (never looked up in the store) and the executor delivers its
	// rows from the per-statement cteCtx (a materialized buffer, or the inlined body run in place).
	cte *int
	// derived is non-nil when this relation is a DERIVED TABLE — `FROM (SELECT …) [AS] t`
	// (spec/design/grammar.md §42): a parenthesized subquery used as a relation, mechanically an
	// anonymous always-inlined single-reference CTE. tableName is the alias (never looked up in the
	// store); the executor runs this plan in place, charging its intrinsic cost — no cte_scan_row.
	// Non-lateral it reads no outer row; a lateral one reads the left-hand row.
	derived *queryPlan
	// lateral is true when this relation is a CORRELATED LATERAL item (spec/design/grammar.md §44):
	// its derived body / SRF args reference an earlier sibling (or an enclosing query), so the
	// executor re-materializes it ONCE PER combined left-hand row (with that row pushed as its
	// immediate outer — the correlated-subquery mechanism), rather than materializing it once. Always
	// false for the first relation. Only a srf or derived relation is ever lateral.
	lateral bool
}

// srfKind selects which set-returning function a srfPlan is, picking the row generator at exec
// (spec/design/functions.md §10, array-functions.md §9). The dispatch is hand-written per core.
type srfKind int

const (
	// srfGenerateSeries is generate_series(start, stop[, step]) — an integer series (functions.md §10).
	srfGenerateSeries srfKind = iota
	// srfUnnest is unnest(anyarray) — one row per array element, flattened row-major (array-functions.md §9).
	srfUnnest
	// srfJsonbArrayElements is jsonb_array_elements(jsonb) — one `jsonb` row per array element
	// (json-sql-functions.md §3).
	srfJsonbArrayElements
	// srfJsonbArrayElementsText is jsonb_array_elements_text(jsonb) — one `text` row per array element
	// (the `->>`-style render).
	srfJsonbArrayElementsText
	// srfJsonbObjectKeys is jsonb_object_keys(jsonb) — one `text` row per object key, in canonical key order.
	srfJsonbObjectKeys
	// srfJsonObjectKeys is json_object_keys(json) — one `text` row per object key, in INPUT order
	// (duplicates kept).
	srfJsonObjectKeys
	// srfJsonbEach is jsonb_each(jsonb) — one `(key text, value jsonb)` row per top-level object
	// member, canonical key order (json-sql-functions.md §3). A two-column SRF (the C0 multi-column
	// synthetic table).
	srfJsonbEach
	// srfJsonbEachText is jsonb_each_text(jsonb) — one `(key text, value text)` row per member (the
	// `->>`-style value).
	srfJsonbEachText
	// srfJSONRecord is json[b]_to_record(doc) (R1, json-table.md §2) — ONE record row: map the JSON
	// object's members to the C0 col-def-list columns by name, coercing each to its declared type.
	srfJSONRecord
	// srfJSONRecordset is json[b]_to_recordset(doc) (R1) — setof record: one record row per element
	// of a top-level JSON array (a non-array → 22023).
	srfJSONRecordset
	// srfJsonbPathQuery is jsonb_path_query(jsonb, jsonpath) (P2, jsonpath.md §5.2) — one `jsonb` row
	// per item of the path's evaluation sequence over the context document. `args` is `[ctx, path]`.
	srfJsonbPathQuery
	// srfJsonTable is JSON_TABLE(ctx, path COLUMNS (…)) (T1, json-table.md §3) — a multi-column
	// relation produced by the recursive default-plan expansion. `args` is `[ctx]`; the resolved column
	// tree is the srfPlan's `jsonTable` field.
	srfJsonTable
	// srfJedTables is the jed_tables catalog relation (spec/design/introspection.md §5): a read-only
	// COMPUTED relation — one row per user table of the qualified database, derived at execution from
	// its pinned catalog snapshot. Not a function (it is resolved as a table name), but it rides the
	// srf plan shape so every "computed, not scanned" gate handles it: no store, no index pushdown, no
	// PK order, excluded from the streaming/vectorized fast paths. `args` is empty; the scope is the
	// srfPlan's introspectScope.
	srfJedTables
	// srfJedColumns is the jed_columns catalog relation (introspection.md §5) — one row per column of
	// every user table of the qualified database, in (table, ordinal) order.
	srfJedColumns
	// srfJedIndexes is the jed_indexes catalog relation (introspection.md §5.1, slice I2) — one row
	// per secondary index of every user table (name, table, columns, is_unique, method).
	srfJedIndexes
	// srfJedConstraints is the jed_constraints catalog relation (introspection.md §5.1, slice I2) —
	// one row per CHECK / UNIQUE / FK / EXCLUDE constraint of every user table.
	srfJedConstraints
)

// srfPlan is a resolved set-returning-function row source (spec/design/functions.md §10,
// array-functions.md §9). kind selects the generator: generate_series(start, stop[, step]) (args =
// 2 or 3 integers) or unnest(anyarray) (args = the single array expression). Non-LATERAL, so each
// arg evaluates against the params/outer environment with no local row. The produced column's type
// lives on the synthetic relation (built in resolveSRF).
type srfPlan struct {
	kind srfKind
	args []*rExpr
	// recordCols is the declared output columns for a record-returning SRF (srfJSONRecord[set]) — the
	// C0 col-def list, used to map JSON members to columns by name + coerce. nil for every other kind.
	recordCols []catColumn
	// jsonTable is the resolved column tree for a JSON_TABLE SRF (srfJsonTable), else nil.
	jsonTable *jtPlan
	// introspectScope is the validated database scope of a catalog relation (srfJedTables /
	// srfJedColumns — introspection.md §5): "main" (also the unqualified default), "temp", or a
	// lowercased attachment name. "" for every other kind.
	introspectScope string
}

// jtPlan is a resolved JSON_TABLE plan (T1, json-table.md §3) — the compiled root path + the column
// tree + the total flattened width.
type jtPlan struct {
	// rootPath is the compiled root jsonpath (its evaluation over `ctx` yields the row items).
	rootPath string
	// width is the total number of flattened output columns.
	width int
	// columns is the top-level column tree.
	columns []jtCol
}

// jtCol is one resolved JSON_TABLE column (json-table.md §3.3). Leaf columns carry their flat output
// index; a nested column carries its child subtree. Modeled as a tagged union (one struct per kind).
type jtCol interface{ isJtCol() }

// jtColOrdinality is `FOR ORDINALITY` — the level's 1-based row counter, written to flat index `idx`.
type jtColOrdinality struct {
	idx int
}

// jtColRegular is a regular column: evaluate `path` over the row item, apply JSON_VALUE (scalar) or
// JSON_QUERY (json/jsonb) semantics, coerce to `returning`, and write it to flat index `idx`.
type jtColRegular struct {
	idx       int
	returning scalarType
	decimal   *decimalTypmod
	path      string
	// query selects JSON_QUERY semantics (json/jsonb returning) vs JSON_VALUE (scalar).
	query   bool
	wrapper jsonWrapper
	onEmpty jsonOnBehavior
	onError jsonOnBehavior
}

// jtColExists is an EXISTS column: JSON_EXISTS of `path`, coerced to `returning` (bool/int), written
// to flat index `idx`.
type jtColExists struct {
	idx       int
	returning scalarType
	path      string
	onError   jsonOnBehavior
}

// jtColNested is a NESTED PATH subtree: expanded over the row item (the default-plan LEFT OUTER /
// sibling UNION).
type jtColNested struct {
	path    string
	columns []jtCol
}

func (*jtColOrdinality) isJtCol() {}
func (*jtColRegular) isJtCol()    {}
func (*jtColExists) isJtCol()     {}
func (*jtColNested) isJtCol()     {}

// planJoin is one join in a SELECT plan: its kind and resolved ON predicate (nil for CROSS). The
// right relation is rels[k+1].
type planJoin struct {
	kind joinKind
	on   *rExpr
}

// orderSlot is a resolved ORDER BY key: a flat/synthetic slot + the per-key direction flags + an
// optional collation. A nil collation is the C/value order; a non-nil collation orders this key by
// its UCA sort key (spec/design/collation.md §8) via the decorate sorter — it never reaches the
// spill Sorter (collation is in-memory only this slice), which ignores the field.
type orderSlot struct {
	idx        int
	descending bool
	nullsFirst bool
	collation  *Collation
}

// selectPlan is a resolved SELECT, executable against an outer-row environment (the execute half
// of the old runSelect, lifted to a value so a correlated subquery can re-run it per outer row).
type selectPlan struct {
	rels      []planRel
	joins     []planJoin
	filter    *rExpr
	isAgg     bool
	groupKeys []int
	// groupExprs is the materialized general-expression GROUP BY keys (`GROUP BY a + b`,
	// aggregates.md §15), in synthetic-slot order. Before bucketing, each post-WHERE row evaluates
	// these and appends the values at flat slots inputWidth+k, so a master grouping key index in
	// groupKeys / groupSets may point at one — the whole-row bucket machinery stays slot-based. Empty
	// when every grouping key is a plain column (the common case, byte-identical to before).
	groupExprs []*rExpr
	// groupSets are the grouping sets to compute (spec/design/aggregates.md §12). A plain GROUP BY
	// (and the whole-table aggregate) is a single set; ROLLUP/CUBE/GROUPING SETS produce several.
	groupSets []groupSetPlan
	// groupingSpecs has one entry per GROUPING() call in the projection / HAVING, in synthetic-slot
	// order: the master-grouping-column positions of its arguments. Each call's value per group row is
	// computed from the row's grouping-set mask and appended after the aggregate results.
	groupingSpecs [][]int
	aggSpecs      []aggSpec
	// hasWindow is true when the select list has a window function — the query runs the blocking
	// WINDOW stage (after WHERE, before ORDER BY/LIMIT) and takes the eager path (never streaming).
	// Mutually exclusive with isAgg in S0 (spec/design/window.md §5.2).
	hasWindow bool
	// windowSpecs is one resolved window function per select-list OVER call (empty unless hasWindow).
	// The window stage appends each spec's per-row result after the input columns and the materialized
	// window keys, so the projection references result i as flat slot input_width+len(windowKeys)+i
	// (spec/design/window.md §5.1).
	windowSpecs []windowSpec
	// windowKeys is the materialized window-key expressions (a non-column PARTITION BY / ORDER BY key
	// — `PARTITION BY a + b`, `ORDER BY a % 2`), in synthetic-slot order. Before the window stage each
	// row evaluates these and appends the values at flat slots input_width+k, so the slot-based
	// partition / sort / frame machinery is unchanged. Empty when every window key is a bare column.
	windowKeys []*rExpr
	having     *rExpr
	order      []orderSlot
	// orderExprs is the materialized ORDER BY expression-key expressions (`ORDER BY a + 1`,
	// `ORDER BY abs(b)`), in the order their sort slots reference them. Just before the sort each row
	// evaluates these and appends the values at final_row_width+k (after any window / grouped columns),
	// so the slot-based sort stays unchanged — the window-key precedent (window.md §5.1). Empty when
	// every ORDER BY key is a bare column or ordinal (the common case, byte-identical to before).
	orderExprs  []*rExpr
	projections []*rExpr
	columnNames []string
	columnTypes []resolvedType
	distinct    bool
	limit       *int64
	offset      *int64
	// relMasks is the TOUCHED SET per relation (cost.md §3 "The touched set"; large-values.md
	// §14): which of its columns this query statically references. Drives the chain-page_read /
	// value_decompress portion of the scan's up-front cost block — an untouched spilled or
	// compressed column charges nothing, however many records the bound admits. An ANNOTATION of
	// the logical plan, not an optimization: a wrong mask is a disk-mode NULL-folding correctness
	// bug, not a slow plan — so it is computed by the resolve half (computeRelMasks), never by a
	// physical rule (spec/design/planner.md §2).
	relMasks [][]bool
	// phys is the plan's physical / access-path decisions — set ONLY by the optimizeSelect pass
	// (optimize.go); zero-valued when resolve hands the plan over (spec/design/planner.md §4).
	phys physicalPlan
}

// physicalPlan is the physical/access-path half of a selectPlan: every field is the output of one
// discrete rule of the optimizeSelect pass (spec/design/planner.md §4), applied in a fixed order
// after the resolve half has built the logical plan. A zero-valued physicalPlan is always correct —
// the executor then full-scans and eager-sorts.
type physicalPlan struct {
	// relationOrder maps physical join positions to logical FROM ordinals. P7 sets [0,1] or [1,0]
	// for eligible two-base INNER/CROSS joins; nil retains source order at every barrier. Resolved
	// expression slots never change.
	relationOrder []int
	// hashJoin is the deterministic two-input hash operator. It builds the right input and probes
	// the left using same-type bare-column equality keys in source order. nil keeps nested loop.
	hashJoin *hashJoinPlan
	// pkOrdered reports that ORDER BY is satisfied by the single base relation's PRIMARY-KEY scan
	// order — the table tree already yields rows in this order, so the sort is elided (and with a
	// LIMIT the scan short-circuits a top-N). True iff the query is a single-table, non-aggregate,
	// non-DISTINCT SELECT whose ORDER BY keys are a prefix of the PK columns, all one direction with
	// the column's stored key collation (spec/design/cost.md §3 "ORDER BY satisfied by primary-key
	// order"). Secondary-index order is a follow-on.
	pkOrdered bool
	// pkReverse is the PK scan direction when pkOrdered: true ⇒ the order is all-DESC over the full
	// PK, served by a REVERSE scan; false ⇒ all-ASC (forward). Always false when !pkOrdered.
	pkReverse bool
	// indexOrder reports that ORDER BY is satisfied by walking a B-tree SECONDARY index in key order
	// (with a LIMIT top-N) — non-nil when the PK scan does not satisfy the order but the index does
	// (cost.md §3 "secondary-index order"). Mutually exclusive with pkOrdered (the PK scan is
	// cheaper). nil keeps the eager/streaming sort.
	indexOrder *indexOrderPlan
	// joinPkOrdered reports that ORDER BY is satisfied by the OUTER relation's PK scan order in a
	// two-table INNER/CROSS join (cost.md §3 "JOIN"): the join drives/probes the outer in PK order, so
	// its output is already in order — the sort is elided and a LIMIT short-circuits the loop. Set only
	// for exactly two non-lateral base relations, a LIMIT, and a forward outer-PK ORDER BY.
	joinPkOrdered bool
	// topK is K = OFFSET + LIMIT for a blocking plain SELECT sort. The executor retains only the
	// best K rows with the exact stable ORDER BY comparator. nil means the rule did not fire (or K
	// overflowed), so the ordinary full sort remains authoritative.
	topK *int64
	// relBounds is the scan-bound pushdown, ONE entry per relation in rels: the WHERE
	// conjuncts that bound that relation's storage key, so its scan seeks/ranges instead of walking
	// the whole B-tree (spec/design/cost.md §3 "bounded scan"). nil ⇒ a full scan of that relation.
	// In a JOIN each base table is bounded independently by the WHERE predicates on its OWN primary
	// key against a CONSTANT (literal/param/outer) — a cross-relation `b.pk = a.x` is the
	// index-nested-loop case (a follow-on). The residual filter stays the WHOLE `filter`, re-applied
	// after the join — the bound only narrows which rows are scanned.
	relBounds []*scanBound
	// relEstimates is the deterministic base estimate inventory. P6b composes it into complete
	// one-base-relation access/ordering pipelines; joins retain their staged legacy policies.
	// Execution never reads this field.
	relEstimates [][]candidateEstimate
	// relINLBounds is the INDEX-NESTED-LOOP scan bounds, one per relation (cost.md §3 "JOIN").
	// Non-nil for a join inner relation whose primary key / indexed column is compared to a SIBLING
	// column of an earlier relation (`a JOIN b ON b.pk = a.x`) — a per-outer-row bound resolved from
	// the combined left-hand row. When set, that relation is NOT materialized once up front; the join
	// loop re-materializes it per left row (like a correlated LATERAL), seeking instead of
	// full-scanning — O(N·M) → O(N·log M). nil ⇒ the ordinary once-materialized relBounds path. A
	// non-nil entry takes precedence over relBounds for that relation.
	relINLBounds []*scanBound
}

type hashJoinPlan struct {
	keys []hashJoinKey
}

type hashJoinKey struct {
	left  int
	right int
	ty    dataType
}

// setOpPlan is a resolved set operation: both operands planned with the same parent scope, the
// unified output types, and the trailing ORDER BY / LIMIT / OFFSET resolved by output column.
type setOpPlan struct {
	op          setOpKind
	all         bool
	lhs         queryPlan
	rhs         queryPlan
	columnNames []string
	columnTypes []resolvedType
	order       []orderSlot
	limit       *int64
	offset      *int64
}

// evalEnv is the environment threaded into the per-row evaluator (spec/design/grammar.md §26):
// the engine (to run a correlated subquery's plan), the bound parameters, and the stack of
// enclosing rows (innermost LAST) a correlated reference reads. outer is empty at the top level;
// a correlated subquery pushes the current row before running its inner plan, so an reOuterColumn
// at frame `level` reads outer[len(outer)-level][index].
type evalEnv struct {
	exec   *engine
	params []Value
	outer  []storedRow
	// The per-statement entropy+clock state (spec/design/entropy.md §5): the uuidv7 monotonic counter
	// + the once-resolved statement clock. The injected random/clock functions live on exec.session.seam
	// (handle-scoped); only the volatile uuid generators touch any of this.
	rng *stmtRng
	// ctes is the statement's CTE execution context (spec/design/cte.md §5), so a FROM reference at
	// any nesting depth delivers a CTE's rows. The zero cteCtx for every non-WITH statement.
	ctes cteCtx
}

// rCaseArm is one resolved (condition, result) branch of a reCase node (spec/design/grammar.md
// §23). The condition is the searched boolean predicate, or the simple form's resolved
// `operand = value` equality.
type rCaseArm struct {
	cond   *rExpr
	result *rExpr
}
