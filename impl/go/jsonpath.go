package jed

import (
	"sort"
	"strconv"
	"strings"
	"unicode/utf8"
)

// The jsonpath type's compiler, canonical renderer, and evaluator (spec/design/jsonpath.md).
//
// The compiled surface: lax/strict mode; a primary ($, @ inside a filter, a scalar literal, last
// inside a subscript, or a parenthesized expression or predicate) followed by accessor steps (.key,
// .*, [subscripts] of any expression, [*], to slices, ?(filter), and the item methods of §3.1);
// arithmetic (+ - * / %, unary + -, §3.2); a top-level comparison predicate. Still deferred (a
// valid-PG path using one → 0A000 at compile): like_regex / starts with / is unknown / exists
// predicates, .**, and the datetime methods. Path variables ($name, $"name") are bound by the caller's
// jpVars. A malformed path is 42601. The
// compiled program is a pure function of the source — kept byte-identical cross-core by the
// conformance suite (CLAUDE.md §5: a hand-written parser, never codegenned). Mirrors
// impl/rust/src/jsonpath.rs.

// jsonPath is a compiled jsonpath. The body is EITHER an accessor path (produces a sequence) or a
// top-level boolean predicate (`$.a == 1`, for jsonb_path_match / @@; jsonpath.md §6).
type jsonPath struct {
	Strict bool
	// Path holds the accessor path when Pred == nil; Pred holds a top-level predicate otherwise.
	// Exactly one is meaningful (Go has no sum types).
	Path jpPathExpr
	Pred *jpPred
}

// jpPrimaryKind tags where a path starts.
type jpPrimaryKind int

const (
	// jpRoot is `$` — the context item.
	jpRoot jpPrimaryKind = iota
	// jpCurrent is `@` — the item a filter is testing (filter context only).
	jpCurrent
	// jpLiteral is a scalar literal — a JSON number / string / boolean / null.
	jpLiteral
	// jpLast is `last` — the index of the innermost subscripted array's last element (subscript
	// expressions only).
	jpLast
	// jpVar is `$name` / `$"name"` — a path variable, bound by the caller's jpVars (§5).
	jpVar
	// jpUnary is `+e` / `-e` — unary arithmetic over each item of e (§3.2).
	jpUnary
	// jpBinary is `l op r` — binary arithmetic over two singleton numbers (§3.2).
	jpBinary
	// jpPredHead is `(predicate)` followed by accessors — the predicate's truth value as an item
	// (true, false, or null when unknown).
	jpPredHead
)

// jpPathExpr is a primary followed by accessor steps — the top-level path body, every comparison
// operand, and every arithmetic operand. A literal primary with no steps is a plain literal operand.
type jpPathExpr struct {
	primary jpPrimaryKind
	// lit holds the literal when primary == jpLiteral.
	lit JsonNode
	// name holds the variable name when primary == jpVar.
	name string
	// unaryOp / arithOp hold the operator when primary == jpUnary / jpBinary; left is the unary
	// operand or the binary left operand, right the binary right operand.
	unaryOp     jpUnaryOp
	arithOp     jpArithOp
	left, right *jpPathExpr
	// pred holds the predicate when primary == jpPredHead.
	pred  *jpPred
	steps []jpStep
}

// jpUnaryOp is a unary arithmetic operator.
type jpUnaryOp int

const (
	jpUnaryPlus jpUnaryOp = iota
	jpUnaryMinus
)

func (op jpUnaryOp) symbol() string {
	if op == jpUnaryPlus {
		return "+"
	}
	return "-"
}

// jpArithOp is a binary arithmetic operator.
type jpArithOp int

const (
	jpAdd jpArithOp = iota
	jpSub
	jpMul
	jpDiv
	jpMod
)

// symbol is the operator as written and as PostgreSQL names it in an error message.
func (op jpArithOp) symbol() string {
	return [...]string{"+", "-", "*", "/", "%"}[op]
}

// priority is PostgreSQL's operationPriority: `+ -` bind looser than `* / %`.
func (op jpArithOp) priority() int {
	if op == jpAdd || op == jpSub {
		return 3
	}
	return 4
}

// jpStepKind tags an accessor step.
type jpStepKind int

const (
	// jpMember is `.key` — a member accessor (the key, unescaped).
	jpMember jpStepKind = iota
	// jpWildcardMember is `.*` — the wildcard member accessor.
	jpWildcardMember
	// jpSubscripts is `[s, …]` — one or more subscripts.
	jpSubscripts
	// jpWildcardElement is `[*]` — the wildcard element accessor.
	jpWildcardElement
	// jpFilter is `?(predicate)` — keep only items for which the predicate is TRUE (§4).
	jpFilter
	// jpMethodStep is `.name(…)` — an item method (§3.1).
	jpMethodStep
)

// jpStep is one accessor step.
type jpStep struct {
	kind jpStepKind
	// key holds the member name when kind == jpMember.
	key string
	// subs holds the subscript list when kind == jpSubscripts.
	subs []jpSubscript
	// pred holds the filter predicate when kind == jpFilter.
	pred *jpPred
	// method holds the item method when kind == jpMethodStep.
	method jpMethod
}

// jpMethodKind tags an item method (jsonpath.md §3.1). The datetime family is still deferred (0A000).
type jpMethodKind int

const (
	jmType jpMethodKind = iota
	jmSize
	jmDouble
	jmCeiling
	jmFloor
	jmAbs
	jmKeyValue
	jmBigint
	jmBoolean
	jmInteger
	jmNumber
	jmString
	jmDecimal
)

// jpMethodNames is each method's name as PostgreSQL spells it in an error message (jspOperationName),
// indexed by jpMethodKind.
var jpMethodNames = [...]string{
	"type", "size", "double", "ceiling", "floor", "abs", "keyvalue",
	"bigint", "boolean", "integer", "number", "string", "decimal",
}

// jpMethod is an item method. precision/scale hold `.decimal(p [, s])`'s optional integer arguments,
// as written.
type jpMethod struct {
	kind             jpMethodKind
	precision, scale *Decimal
}

func (m jpMethod) name() string { return jpMethodNames[m.kind] }

// jpPredKind tags a filter predicate (jsonpath.md §4, the comparison subset). 3-valued — Not/And/Or
// follow SQL/JSON's Kleene logic, but a filter keeps an item only when the predicate is definitely
// TRUE.
type jpPredKind int

const (
	// jpPredOr is `a || b`.
	jpPredOr jpPredKind = iota
	// jpPredAnd is `a && b`.
	jpPredAnd
	// jpPredNot is `!(a)`.
	jpPredNot
	// jpPredCompare is `lhs cmp rhs` — an existential comparison (true if SOME pair compares true).
	jpPredCompare
)

// jpPred is a filter predicate. kind selects which fields are meaningful (Go has no sum types).
type jpPred struct {
	kind jpPredKind
	// left/right hold operand predicates for Or/And; left holds the inner predicate for Not.
	left, right *jpPred
	// cmpLeft/cmpRight/cmpOp hold the comparison operands and operator when kind == jpPredCompare.
	cmpLeft, cmpRight jpPathExpr
	cmpOp             jpCmpOp
}

// jpCmpOp is a jsonpath comparison operator (`==`, `!=`/`<>`, `<`, `<=`, `>`, `>=`).
type jpCmpOp int

const (
	jpCmpEq jpCmpOp = iota
	jpCmpNe
	jpCmpLt
	jpCmpLe
	jpCmpGt
	jpCmpGe
)

// jpSubscript is one subscript: a single index or an `i to j` slice. slice == false ⇒ a single
// index (only a is meaningful); slice == true ⇒ the `a to b` form.
type jpSubscript struct {
	a, b  jpIndex
	slice bool
}

// jpIndex is a subscript index: a number literal (as written — evaluation truncates it and rejects
// one outside the integer range), the `last` sentinel, or any other expression (§3.2).
type jpIndex struct {
	// last is true for the `last` sentinel; expr holds any other expression; otherwise number is the
	// literal value.
	last   bool
	expr   *jpPathExpr
	number Decimal
}

// jpUnsupported is a jsonpath construct that is valid in PostgreSQL but not yet supported by jed (a
// deferred follow-on): 0A000, a documented divergence.
func jpUnsupported(what string) *EngineError {
	return newError(FeatureNotSupported, "jsonpath "+what+" is not supported yet")
}

// jpMalformed is a malformed jsonpath literal: 42601 (PostgreSQL's syntax-error class for a bad
// path literal).
func jpMalformed(detail string) *EngineError {
	return newError(SyntaxError, "invalid jsonpath: "+detail)
}

// compile compiles a jsonpath source string. Malformed → 42601; a valid-PG but unsupported construct
// → 0A000.
func compile(src string) (jsonPath, error) {
	path, err := parseJsonPath(src)
	if err != nil {
		return jsonPath{}, err
	}
	// A path is evaluated from its canonical text, whose parenthesization can nest deeper than the
	// source (`1 + 1 + 1` renders `((1 + 1) + 1)`), so that text must compile too (§8).
	if _, err := parseJsonPath(path.Render()); err != nil {
		return jsonPath{}, err
	}
	return path, nil
}

// parseJsonPath parses a jsonpath source string (see compile).
func parseJsonPath(src string) (jsonPath, error) {
	p := &jpParser{s: []byte(src)}
	p.skipWs()
	// Optional mode word: strict / lax (default lax).
	strict := false
	if p.eatKeyword("strict") {
		strict = true
	} else {
		p.eatKeyword("lax")
	}
	// The body is an expression (an accessor path, arithmetic) or a top-level predicate
	// (`$.a == 1`, for jsonb_path_match / @@) — PostgreSQL's expr_or_predicate.
	body, err := p.parseOr()
	if err != nil {
		return jsonPath{}, err
	}
	p.skipWs()
	if _, ok := p.peek(); ok {
		return jsonPath{}, jpMalformed("unexpected trailing input")
	}
	if body.pred != nil {
		return jsonPath{Strict: strict, Pred: body.pred}, nil
	}
	return jsonPath{Strict: strict, Path: *body.expr}, nil
}

// Render is the canonical render (spec/design/jsonpath.md §2.2): strict kept / lax omitted; member
// keys quoted; [*], [i], [i to j] subscripts; methods lowercase; operators spaced and parenthesized
// by PostgreSQL's jsonpath_out rules.
func (jp jsonPath) Render() string {
	var out strings.Builder
	if jp.Strict {
		out.WriteString("strict ")
	}
	// The top level parenthesizes an operator (`($."a" + 1)`, `($."a" == 1)`), as PostgreSQL prints
	// it.
	if jp.Pred != nil {
		writePred(jp.Pred, true, &out)
	} else {
		writeExpr(&jp.Path, true, &out)
	}
	return out.String()
}

// jpExprPriority is PostgreSQL's operationPriority of an expression's head: `||` 0, `&&` 1,
// comparisons 2, `+ -` 3, `* / %` 4, unary `+ -` 5, anything else 6.
func jpExprPriority(e *jpPathExpr) int {
	switch e.primary {
	case jpUnary:
		return 5
	case jpBinary:
		return e.arithOp.priority()
	case jpPredHead:
		return jpPredPriority(e.pred)
	default:
		return 6
	}
}

func jpPredPriority(pred *jpPred) int {
	switch pred.kind {
	case jpPredOr:
		return 0
	case jpPredAnd:
		return 1
	case jpPredCompare:
		return 2
	default:
		return 6
	}
}

// writeExpr renders an expression — PostgreSQL's printJsonPathItem. An operator is parenthesized
// when brackets is set: at the top level, and for an operand that binds no tighter than its parent
// (`((1 + 2) + 3)`, `(1 - (2 - 3))`). jed also parenthesizes an operator followed by accessors,
// which PostgreSQL prints bare in an operand (`$."a" + 1.type()`), a text that does not read back as
// the same path (§2.2). A number literal followed by a step is parenthesized (`(1.5).type()`).
func writeExpr(e *jpPathExpr, brackets bool, out *strings.Builder) {
	brackets = brackets || len(e.steps) > 0
	open := func() {
		if brackets {
			out.WriteByte('(')
		}
	}
	closeB := func() {
		if brackets {
			out.WriteByte(')')
		}
	}
	switch e.primary {
	case jpRoot:
		out.WriteByte('$')
	case jpCurrent:
		out.WriteByte('@')
	case jpLast:
		out.WriteString("last")
	case jpVar:
		out.WriteByte('$')
		writeQuoted(e.name, out)
	case jpUnary:
		open()
		out.WriteString(e.unaryOp.symbol())
		writeExpr(e.left, jpExprPriority(e.left) <= 5, out)
		closeB()
	case jpBinary:
		open()
		writeExpr(e.left, jpExprPriority(e.left) <= e.arithOp.priority(), out)
		out.WriteByte(' ')
		out.WriteString(e.arithOp.symbol())
		out.WriteByte(' ')
		writeExpr(e.right, jpExprPriority(e.right) <= e.arithOp.priority(), out)
		closeB()
	case jpPredHead:
		writePred(e.pred, brackets, out)
	default:
		if e.lit.Kind == JNumber && len(e.steps) > 0 {
			out.WriteByte('(')
			out.WriteString(jsonCompactOut(&e.lit))
			out.WriteByte(')')
		} else {
			out.WriteString(jsonCompactOut(&e.lit))
		}
	}
	writeSteps(e.steps, out)
}

// writeSteps renders an accessor-step sequence.
func writeSteps(steps []jpStep, out *strings.Builder) {
	for _, step := range steps {
		switch step.kind {
		case jpMember:
			out.WriteByte('.')
			writeQuoted(step.key, out)
		case jpWildcardMember:
			out.WriteString(".*")
		case jpWildcardElement:
			out.WriteString("[*]")
		case jpSubscripts:
			out.WriteByte('[')
			for n, s := range step.subs {
				if n > 0 {
					out.WriteByte(',')
				}
				if s.slice {
					writeIndex(s.a, out)
					out.WriteString(" to ")
					writeIndex(s.b, out)
				} else {
					writeIndex(s.a, out)
				}
			}
			out.WriteByte(']')
		case jpFilter:
			out.WriteString("?(")
			writePred(step.pred, false, out)
			out.WriteByte(')')
		case jpMethodStep:
			out.WriteByte('.')
			out.WriteString(step.method.name())
			out.WriteByte('(')
			if step.method.precision != nil {
				out.WriteString(step.method.precision.Render())
			}
			if step.method.scale != nil {
				out.WriteByte(',')
				out.WriteString(step.method.scale.Render())
			}
			out.WriteByte(')')
		}
	}
}

// writePred renders a predicate (PostgreSQL's form: `&&`/`||` and `a op b` spaced, `!(…)`),
// parenthesized when brackets is set and its operands by priority, like writeExpr.
func writePred(pred *jpPred, brackets bool, out *strings.Builder) {
	open := func() {
		if brackets {
			out.WriteByte('(')
		}
	}
	closeB := func() {
		if brackets {
			out.WriteByte(')')
		}
	}
	switch pred.kind {
	case jpPredOr, jpPredAnd:
		prio := jpPredPriority(pred)
		open()
		writePred(pred.left, jpPredPriority(pred.left) <= prio, out)
		if pred.kind == jpPredOr {
			out.WriteString(" || ")
		} else {
			out.WriteString(" && ")
		}
		writePred(pred.right, jpPredPriority(pred.right) <= prio, out)
		closeB()
	case jpPredNot:
		out.WriteString("!(")
		writePred(pred.left, false, out)
		out.WriteByte(')')
	default: // jpPredCompare
		open()
		writeExpr(&pred.cmpLeft, jpExprPriority(&pred.cmpLeft) <= 2, out)
		out.WriteByte(' ')
		switch pred.cmpOp {
		case jpCmpEq:
			out.WriteString("==")
		case jpCmpNe:
			out.WriteString("!=")
		case jpCmpLt:
			out.WriteString("<")
		case jpCmpLe:
			out.WriteString("<=")
		case jpCmpGt:
			out.WriteString(">")
		case jpCmpGe:
			out.WriteString(">=")
		}
		out.WriteByte(' ')
		writeExpr(&pred.cmpRight, jpExprPriority(&pred.cmpRight) <= 2, out)
		closeB()
	}
}

// ---------------------------------------------------------------------------------------------
// Evaluation (jsonpath.md §3-4) — the lax/strict ordered SQL/JSON item sequence.
// ---------------------------------------------------------------------------------------------

// jpEvalOpts is how a caller consumes the sequence (jsonpath.md §5).
type jpEvalOpts struct {
	// exists is an existence test (jsonb_path_exists, @?, JSON_EXISTS): a lax path stops at its first
	// item, so a later item's error is never reached. A strict path evaluates fully (§4.2).
	exists bool
	// vars is the variables the path's $name references read (§5).
	vars jpVars
}

// jpVarsKind tags where a path's variables come from.
type jpVarsKind int

const (
	// jpVarsNone is no variables: the @? / @@ operators. Counts no base object.
	jpVarsNone jpVarsKind = iota
	// jpVarsObject is the jsonb_path_* functions' vars object (already checked to be an object by
	// checkVarsObject). It is one base object, number 1.
	jpVarsObject
	// jpVarsPassing is the SQL/JSON functions' PASSING values, as passingVars orders them. Each is a
	// base object numbered by its position in the clause; count is the clause's length.
	jpVarsPassing
)

// jpVars is the variables a path's $name references are bound to (jsonpath.md §5), and the variable
// base objects they count for .keyvalue() ids (§3.1).
type jpVars struct {
	kind jpVarsKind
	// object is the vars object (jpVarsObject).
	object *JsonNode
	// passing is the ordered PASSING bindings and count the clause's length (jpVarsPassing).
	passing []jpPassingVar
	count   int64
}

// jpObjectVars is the jsonb_path_* functions' vars object as a path's variables.
func jpObjectVars(object *JsonNode) jpVars { return jpVars{kind: jpVarsObject, object: object} }

// jpPassingVarsOf is a PASSING clause's ordered bindings (count = the clause's length) as a path's
// variables.
func jpPassingVarsOf(passing []jpPassingVar, count int64) jpVars {
	return jpVars{kind: jpVarsPassing, passing: passing, count: count}
}

// baseCount is how many variable base objects the caller supplies (PostgreSQL's countVars): the
// first .keyvalue() object is numbered one past them.
func (v jpVars) baseCount() int64 {
	switch v.kind {
	case jpVarsObject:
		return 1
	case jpVarsPassing:
		return v.count
	default:
		return 0
	}
}

// jpPassingVar is one `PASSING value AS name` binding (jsonpath.md §5): the value as a jsonb item and
// its base object number (its 1-based position in the clause).
type jpPassingVar struct {
	name  string
	value JsonNode
	id    int64
}

// jpBinding is one evaluated PASSING value with the name it binds, in clause order.
type jpBinding struct {
	name  string
	value JsonNode
}

// passingVars orders a PASSING clause's bindings for lookup: sorted by name, and of two bindings with
// the same name only the first kept, as PostgreSQL's lookup finds the first. Each keeps its clause
// position as its base object number.
func passingVars(bindings []jpBinding) []jpPassingVar {
	vars := make([]jpPassingVar, len(bindings))
	for i, b := range bindings {
		vars[i] = jpPassingVar{name: b.name, value: b.value, id: int64(i) + 1}
	}
	// A stable sort keeps the first of equal names first.
	sort.SliceStable(vars, func(i, j int) bool { return vars[i].name < vars[j].name })
	out := vars[:0]
	for i := range vars {
		if len(out) > 0 && out[len(out)-1].name == vars[i].name {
			continue
		}
		out = append(out, vars[i])
	}
	return out
}

// checkVarsObject checks a jsonb_path_* function's vars argument (PostgreSQL's
// countVariablesFromJsonb): it must be an object, else 22023. Checked before evaluation, so it is
// raised even when the path uses no variable and even in silent mode.
func checkVarsObject(vars *JsonNode) error {
	if vars.Kind == JObject {
		return nil
	}
	return newError(InvalidParameterValue, `"vars" argument is not an object`).
		withDetail(`Jsonpath parameters should be encoded as key-value pairs of "vars" object.`)
}

// compileMetered compiles a path for evaluation, charging jsonpath_compile for each byte of the text
// first (jsonpath.md §7). Every evaluation compiles its path; there is no cache.
func compileMetered(src string, m *costMeter) (jsonPath, error) {
	m.Charge(costs.JsonpathCompile * int64(len(src)))
	if err := m.Guard(); err != nil {
		return jsonPath{}, err
	}
	// The text is a jsonpath value's canonical text (or a JSON_TABLE path), already checked by
	// compile at resolve.
	return parseJsonPath(src)
}

// Eval evaluates a compiled path over a jsonb context item → the ordered SQL/JSON sequence
// (jsonpath.md §3). Generated items (method results, .keyvalue() objects, literals) are built; any
// error propagates. Metered by jsonpath_step (§7).
func (jp jsonPath) Eval(ctx JsonNode, opts jpEvalOpts, m *costMeter) ([]JsonNode, error) {
	var items []jpItem
	if err := jp.evalItems(&ctx, opts, m, &items); err != nil {
		return nil, err
	}
	return jpMaterializeAll(items), nil
}

// EvalSilent is Eval in PostgreSQL's silent mode (@?, @@): a SQL/JSON (class 22) error ends the
// evaluation, returning the items found before it and errored = true. Any other error propagates.
func (jp jsonPath) EvalSilent(ctx JsonNode, opts jpEvalOpts, m *costMeter) (seq []JsonNode, errored bool, err error) {
	var items []jpItem
	if err := jp.evalItems(&ctx, opts, m, &items); err != nil {
		if !isSQLJSONError(err) {
			return nil, false, err
		}
		errored = true
	}
	return jpMaterializeAll(items), errored, nil
}

func jpMaterializeAll(items []jpItem) []JsonNode {
	out := make([]JsonNode, len(items))
	for i := range items {
		out[i] = items[i].materialize()
	}
	return out
}

func (jp jsonPath) evalItems(ctx *JsonNode, opts jpEvalOpts, m *costMeter, out *[]jpItem) error {
	cx := &jpCtx{root: ctx, vars: opts.vars, strict: jp.Strict, m: m, nextGen: 1 + opts.vars.baseCount()}
	if jp.Pred != nil {
		// A top-level predicate → a single item: its truth value as a boolean, or JSON null when
		// unknown (§4.4).
		truth, err := cx.evalPred(jp.Pred, jpDocItem(ctx), jpBase{}, -1)
		if err != nil {
			return err
		}
		if truth == nil {
			*out = append(*out, jpValItem(JsonNode{Kind: JNull}))
		} else {
			*out = append(*out, jpValItem(JsonNode{Kind: JBool, B: *truth}))
		}
		return nil
	}
	// @ is not allowed outside a filter, so the root expression's current item is unused.
	return cx.runExpr(&jp.Path, jpDocItem(ctx), jpBase{}, -1, opts.exists && !jp.Strict, out)
}

// jpItemKind tags an item of the SQL/JSON sequence.
type jpItemKind int

const (
	// jpItemDoc is a node of the context document.
	jpItemDoc jpItemKind = iota
	// jpItemVal is a scalar the evaluation produced: a path literal or an item-method result.
	jpItemVal
	// jpItemKv is a .keyvalue() object {"id": …, "key": …, "value": …}, materialized only on output.
	jpItemKv
)

// jpItem is one item of the SQL/JSON sequence. node is the document node (jpItemDoc) or the owned
// generated node (jpItemVal); kv the pair (jpItemKv).
type jpItem struct {
	kind jpItemKind
	node *JsonNode
	kv   *jpKvPair
}

// jpKvPair is one .keyvalue() pair object (jsonpath.md §3.1).
type jpKvPair struct {
	key   string
	value jpItem
	// id is the "id" member: the source object's id.
	id int64
	// generated is this object's own base-object number — the id prefix of a .keyvalue() applied
	// within it.
	generated int64
	// tree is the base object (the document or a variable's) that holds a document value, for the
	// value's layout offsets.
	tree *JsonNode
}

func jpDocItem(n *JsonNode) jpItem { return jpItem{kind: jpItemDoc, node: n} }

func jpValItem(n JsonNode) jpItem { return jpItem{kind: jpItemVal, node: &n} }

// scalarNode is the node of a document or generated item (nil for a .keyvalue() object).
func (it jpItem) scalarNode() *JsonNode {
	if it.kind == jpItemKv {
		return nil
	}
	return it.node
}

// doc is the document node (nil for a generated item).
func (it jpItem) doc() *JsonNode {
	if it.kind == jpItemDoc {
		return it.node
	}
	return nil
}

func (it jpItem) isDocKind(k JsonNodeKind) bool {
	return it.kind == jpItemDoc && it.node.Kind == k
}

func (it jpItem) typeName() string {
	if it.kind == jpItemKv {
		return "object"
	}
	return jsonTypeofName(it.node)
}

// materialize is the item as a jsonb value; a .keyvalue() object is built.
func (it jpItem) materialize() JsonNode {
	if it.kind != jpItemKv {
		return *it.node
	}
	return JsonNode{Kind: JObject, Obj: []JsonMember{
		{Key: "id", Val: JsonNode{Kind: JNumber, Num: decimalFromInt64(it.kv.id)}},
		{Key: "key", Val: JsonNode{Kind: JString, S: it.kv.key}},
		{Key: "value", Val: it.kv.value.materialize()},
	}}
}

// jpBase is the base object a .keyvalue() id is measured against (jsonpath.md §3.1): the context
// item (kv == nil, varTree == nil) while a $ path runs, a variable's base object (varTree, numbered
// varID: the vars object, number 1, or a PASSING value, its position) while a $name path runs, or
// the .keyvalue() object (kv) whose continuation is running.
type jpBase struct {
	kv      *jpKvPair
	varTree *JsonNode
	varID   int64
}

// jpCtx is the evaluation context shared by a path and the filter operands nested in it.
type jpCtx struct {
	root   *JsonNode
	vars   jpVars
	strict bool
	m      *costMeter
	// nextGen is the next .keyvalue() object number, assigned in evaluation order.
	nextGen int64
	// offsets is, per base object laid out so far (the document or a variable's), each container's
	// byte offset in PostgreSQL's binary jsonb layout of that base object, computed for a base object
	// on the first .keyvalue() that needs one.
	offsets map[*JsonNode]map[*JsonNode]int64
}

// charge charges n units of jsonpath_step and guards (jsonpath.md §7).
func (cx *jpCtx) charge(n int64) error {
	cx.m.Charge(costs.JsonpathStep * n)
	return cx.m.Guard()
}

// jpFrame is a step application in progress: step `step` applied to `item`, producing its outputs
// lazily so each output's continuation runs before the next output is produced (PostgreSQL's
// depth-first order, which fixes the error raised first, the early stop of an existence test, and
// the .keyvalue() numbering).
type jpFrame struct {
	step int
	item jpItem
	base jpBase
	// unwrap is whether lax auto-unwrapping applies to this item (§4.1); false for the elements of
	// an unwrapped array, which are not unwrapped again.
	unwrap  bool
	started bool
	// a, b are output cursors.
	a, b int
	// hasRange/from/to are the current subscript's resolved element range.
	hasRange bool
	to       int64
	// kvID is the .keyvalue() id of this frame's object.
	kvID int64
	// last is the size of the innermost array being subscripted, which `last` refers to (in a filter
	// of this step); -1 for none. It is set by an enclosing subscript step, for as long as its outputs'
	// continuations run, as in PostgreSQL.
	last int64
	// size is a subscript step's array size: its own outputs' last.
	size int64
}

// jpOutKind is what a frame produces next.
type jpOutKind int

const (
	// jpOutItem is an item for the following step (or the result), with its continuation's base.
	jpOutItem jpOutKind = iota
	// jpOutSub is one element of a lax-unwrapped array, to which the frame's step applies in turn.
	jpOutSub
	jpOutDone
)

// runExpr runs an expression depth-first (§3): its head produces items one at a time (§3.2), and each
// runs through the accessor steps on an explicit stack (no native recursion per step) before the head
// produces the next. current is the item @ names and last the size of the innermost array being
// subscripted (-1 for none). Each final item is appended to out; stopAtFirst ends the run at the
// first one — PostgreSQL's lax existence test, which also lets a unary operator skip a non-number.
func (cx *jpCtx) runExpr(e *jpPathExpr, current jpItem, base jpBase, last int64, stopAtFirst bool, out *[]jpItem) error {
	steps := e.steps
	unwrap := !cx.strict
	head := &jpHead{}
	var stack []*jpFrame
	for {
		if len(stack) == 0 {
			item, itemBase, ok, err := cx.headNext(e, head, current, base, last, stopAtFirst)
			if err != nil || !ok {
				return err
			}
			if len(steps) == 0 {
				*out = append(*out, item)
				if stopAtFirst {
					return nil
				}
			} else {
				stack = append(stack, &jpFrame{step: 0, item: item, base: itemBase, unwrap: unwrap, last: last})
			}
			continue
		}
		top := stack[len(stack)-1]
		kind, item, itemBase, sub, err := cx.nextOut(steps, current, top)
		if err != nil {
			return err
		}
		switch kind {
		case jpOutItem:
			next := top.step + 1
			if next == len(steps) {
				*out = append(*out, item)
				if stopAtFirst {
					return nil
				}
			} else {
				// A subscript step's outputs run their continuations with last naming its array.
				childLast := top.last
				if steps[top.step].kind == jpSubscripts {
					childLast = top.size
				}
				stack = append(stack, &jpFrame{step: next, item: item, base: itemBase, unwrap: unwrap, last: childLast})
			}
		case jpOutSub:
			stack = append(stack, sub)
		default:
			stack = stack[:len(stack)-1]
		}
	}
}

// jpHead is the state of an expression's head: whether it has started, and a unary operator's
// operand items with the next one's position.
type jpHead struct {
	started bool
	items   []jpItem
	at      int
}

// headNext produces the next item of an expression's head ($, @, a literal, last, an operator, or a
// parenthesized predicate), with the base its continuation runs under; ok=false when there is none.
func (cx *jpCtx) headNext(e *jpPathExpr, head *jpHead, current jpItem, base jpBase, last int64, stopAtFirst bool) (jpItem, jpBase, bool, error) {
	if e.primary == jpUnary {
		// Each item of the operand (lax: an array item unwrapped one level) must be a number: one
		// application each, one emitted item for each number. A lax existence test with nothing after
		// the operator skips a non-number, as PostgreSQL does.
		if !head.started {
			head.started = true
			items, err := cx.evalSeq(e.left, current, base, last, true)
			if err != nil {
				return jpItem{}, jpBase{}, false, err
			}
			head.items = items
		}
		for head.at < len(head.items) {
			item := head.items[head.at]
			head.at++
			if err := cx.charge(1); err != nil {
				return jpItem{}, jpBase{}, false, err
			}
			n := item.scalarNode()
			if n == nil || n.Kind != JNumber {
				if stopAtFirst && len(e.steps) == 0 {
					continue
				}
				return jpItem{}, jpBase{}, false, newError(SqlJsonNumberNotFound,
					"operand of unary jsonpath operator "+e.unaryOp.symbol()+" is not a numeric value")
			}
			if err := cx.charge(1); err != nil {
				return jpItem{}, jpBase{}, false, err
			}
			if e.unaryOp == jpUnaryMinus {
				item = jpValItem(JsonNode{Kind: JNumber, Num: n.Num.Negate()})
			}
			return item, base, true, nil
		}
		return jpItem{}, jpBase{}, false, nil
	}
	if head.started {
		return jpItem{}, jpBase{}, false, nil
	}
	head.started = true
	switch e.primary {
	case jpRoot:
		return jpDocItem(cx.root), jpBase{}, true, nil
	case jpVar:
		item, varBase, err := cx.variable(e.name)
		if err != nil {
			return jpItem{}, jpBase{}, false, err
		}
		return item, varBase, true, nil
	case jpCurrent:
		return current, base, true, nil
	case jpLast:
		return jpValItem(JsonNode{Kind: JNumber, Num: decimalFromInt64(max(last, 0) - 1)}), base, true, nil
	case jpBinary:
		item, err := cx.evalBinary(e, current, base, last)
		if err != nil {
			return jpItem{}, jpBase{}, false, err
		}
		return item, base, true, nil
	case jpPredHead:
		// (predicate) followed by accessors: the truth value as an item (JSON null for unknown).
		truth, err := cx.evalPred(e.pred, current, base, last)
		if err != nil {
			return jpItem{}, jpBase{}, false, err
		}
		if truth == nil {
			return jpValItem(JsonNode{Kind: JNull}), base, true, nil
		}
		return jpValItem(JsonNode{Kind: JBool, B: *truth}), base, true, nil
	default: // jpLiteral
		return jpValItem(e.lit), base, true, nil
	}
}

// evalSeq evaluates an expression to its whole item sequence. With unwrap in lax mode an array item
// is unwrapped one level (§4.1 rule 4) — free: it only changes which items the caller sees.
func (cx *jpCtx) evalSeq(e *jpPathExpr, current jpItem, base jpBase, last int64, unwrap bool) ([]jpItem, error) {
	var seq []jpItem
	if err := cx.runExpr(e, current, base, last, false, &seq); err != nil {
		return nil, err
	}
	if cx.strict || !unwrap {
		return seq, nil
	}
	out := make([]jpItem, 0, len(seq))
	for _, item := range seq {
		if item.isDocKind(JArray) {
			for k := range item.node.Arr {
				out = append(out, jpDocItem(&item.node.Arr[k]))
			}
		} else {
			out = append(out, item)
		}
	}
	return out, nil
}

// evalBinary is `l op r` (§3.2): both operands are evaluated (lax unwraps an array item one level),
// then each must be a single number (22038). One application, the size-scaled decimal_work of the
// SQL operator, and one emitted item; the arithmetic is decimal's, so `/` takes PostgreSQL's
// select_div_scale and a zero divisor raises 22012.
func (cx *jpCtx) evalBinary(e *jpPathExpr, current jpItem, base jpBase, last int64) (jpItem, error) {
	ls, err := cx.evalSeq(e.left, current, base, last, true)
	if err != nil {
		return jpItem{}, err
	}
	rs, err := cx.evalSeq(e.right, current, base, last, true)
	if err != nil {
		return jpItem{}, err
	}
	if err := cx.charge(1); err != nil {
		return jpItem{}, err
	}
	single := func(seq []jpItem, side string) (Decimal, error) {
		if len(seq) == 1 {
			if n := seq[0].scalarNode(); n != nil && n.Kind == JNumber {
				return n.Num, nil
			}
		}
		return Decimal{}, newError(SingletonSqlJsonItemRequired,
			side+" operand of jsonpath operator "+e.arithOp.symbol()+" is not a single numeric value")
	}
	a, err := single(ls, "left")
	if err != nil {
		return jpItem{}, err
	}
	b, err := single(rs, "right")
	if err != nil {
		return jpItem{}, err
	}
	var work int64
	switch e.arithOp {
	case jpAdd, jpSub:
		work = workLinear(a, b)
	case jpMul:
		work = workMul(a, b)
	case jpDiv:
		work = workDiv(a, b)
	default:
		work = workMod(a, b)
	}
	cx.m.Charge(costs.DecimalWork * (work - 1))
	if err := cx.m.Guard(); err != nil {
		return jpItem{}, err
	}
	var v Decimal
	switch e.arithOp {
	case jpAdd:
		v, err = a.Add(b)
	case jpSub:
		v, err = a.Sub(b)
	case jpMul:
		v, err = a.Mul(b)
	case jpDiv:
		v, err = a.Div(b)
	default:
		v, err = a.Rem(b)
	}
	if err != nil {
		return jpItem{}, err
	}
	if err := cx.charge(1); err != nil {
		return jpItem{}, err
	}
	return jpValItem(JsonNode{Kind: JNumber, Num: v}), nil
}

// jpUnwraps is whether lax mode unwraps an array before applying this step (§4.1 rule 1). The
// subscripts handle arrays themselves, and .type() / .size() describe the array.
func jpUnwraps(step *jpStep) bool {
	switch step.kind {
	case jpMember, jpWildcardMember, jpFilter:
		return true
	case jpMethodStep:
		return step.method.kind != jmType && step.method.kind != jmSize
	default:
		return false
	}
}

// nextOut produces a frame's next output, charging its step application and each emitted item (§7).
func (cx *jpCtx) nextOut(steps []jpStep, current jpItem, f *jpFrame) (jpOutKind, jpItem, jpBase, *jpFrame, error) {
	step := &steps[f.step]
	done := func() (jpOutKind, jpItem, jpBase, *jpFrame, error) {
		return jpOutDone, jpItem{}, jpBase{}, nil, nil
	}
	fail := func(err error) (jpOutKind, jpItem, jpBase, *jpFrame, error) {
		return jpOutDone, jpItem{}, jpBase{}, nil, err
	}
	if f.unwrap && jpUnwraps(step) && f.item.isDocKind(JArray) {
		// lax: apply the step to each element instead (one level — an element array is not
		// unwrapped again).
		elems := f.item.node.Arr
		if f.a < len(elems) {
			e := &elems[f.a]
			f.a++
			return jpOutSub, jpItem{}, jpBase{}, &jpFrame{step: f.step, item: jpDocItem(e), base: f.base, last: f.last}, nil
		}
		return done()
	}
	strict := cx.strict
	var child jpItem
	found := false
	switch step.kind {
	case jpMember:
		if f.started {
			return done()
		}
		f.started = true
		if err := cx.charge(1); err != nil {
			return fail(err)
		}
		c, ok, err := cx.memberOf(f.item, step.key)
		if err != nil {
			return fail(err)
		}
		child, found = c, ok
	case jpWildcardMember:
		if !f.started {
			f.started = true
			if err := cx.charge(1); err != nil {
				return fail(err)
			}
			if f.item.typeName() != "object" {
				if strict {
					return fail(newError(SqlJsonObjectNotFound,
						"jsonpath wildcard member accessor can only be applied to an object"))
				}
				return done()
			}
		}
		i := f.a
		f.a++
		if d := f.item.doc(); d != nil {
			if i < len(d.Obj) {
				child, found = jpDocItem(&d.Obj[i].Val), true
			}
		} else if f.item.kind == jpItemKv {
			child, found = jpKvMember(f.item.kv, i)
		}
	case jpSubscripts:
		if !f.started {
			f.started = true
			if err := cx.charge(1); err != nil {
				return fail(err)
			}
			if strict && !f.item.isDocKind(JArray) {
				return fail(newError(SqlJsonArrayNotFound, "jsonpath array accessor can only be applied to an array"))
			}
		}
		// A non-array is a singleton array in lax (§4.1 rule 2).
		isArray := f.item.isDocKind(JArray)
		size := int64(1)
		if isArray {
			size = int64(len(f.item.node.Arr))
		}
		f.size = size
		for !found {
			if !f.hasRange {
				if f.a >= len(step.subs) {
					return done()
				}
				from, to, err := cx.subscriptRange(&step.subs[f.a], size, current, f.base)
				if err != nil {
					return fail(err)
				}
				f.hasRange, f.to, f.b = true, to, int(from)
			}
			if int64(f.b) <= f.to {
				i := f.b
				f.b++
				if isArray {
					child = jpDocItem(&f.item.node.Arr[i])
				} else {
					child = f.item
				}
				found = true
			} else {
				f.hasRange = false
				f.a++
			}
		}
	case jpWildcardElement:
		if !f.started {
			f.started = true
			if err := cx.charge(1); err != nil {
				return fail(err)
			}
			if !f.item.isDocKind(JArray) {
				if strict {
					return fail(newError(SqlJsonArrayNotFound,
						"jsonpath wildcard array accessor can only be applied to an array"))
				}
				// lax: the item is a singleton array.
				if err := cx.charge(1); err != nil {
					return fail(err)
				}
				return jpOutItem, f.item, f.base, nil, nil
			}
		}
		if f.item.isDocKind(JArray) && f.a < len(f.item.node.Arr) {
			child, found = jpDocItem(&f.item.node.Arr[f.a]), true
			f.a++
		}
	case jpFilter:
		// ?(predicate) — keep the item when the predicate is definitely TRUE (§4). The predicate's @
		// is the item; $ is the context item.
		if f.started {
			return done()
		}
		f.started = true
		if err := cx.charge(1); err != nil {
			return fail(err)
		}
		truth, err := cx.evalPred(step.pred, f.item, f.base, f.last)
		if err != nil {
			return fail(err)
		}
		if isTrue(truth) {
			child, found = f.item, true
		}
	default: // jpMethodStep
		if step.method.kind == jmKeyValue {
			return cx.nextKeyValue(f)
		}
		if f.started {
			return done()
		}
		f.started = true
		if err := cx.charge(1); err != nil {
			return fail(err)
		}
		c, err := jpApplyMethod(step.method, f.item, strict)
		if err != nil {
			return fail(err)
		}
		child, found = c, true
	}
	if !found {
		return done()
	}
	if err := cx.charge(1); err != nil {
		return fail(err)
	}
	return jpOutItem, child, f.base, nil, nil
}

// nextKeyValue produces a .keyvalue() frame's next pair object: one per member, each the base of its
// own continuation.
func (cx *jpCtx) nextKeyValue(f *jpFrame) (jpOutKind, jpItem, jpBase, *jpFrame, error) {
	if !f.started {
		f.started = true
		if err := cx.charge(1); err != nil {
			return jpOutDone, jpItem{}, jpBase{}, nil, err
		}
		if f.item.typeName() != "object" {
			return jpOutDone, jpItem{}, jpBase{}, nil, newError(SqlJsonObjectNotFound,
				"jsonpath item method .keyvalue() can only be applied to an object")
		}
		f.kvID = cx.keyvalueID(f.item, f.base)
	}
	i := f.a
	f.a++
	var key string
	var value jpItem
	if d := f.item.doc(); d != nil {
		if i >= len(d.Obj) {
			return jpOutDone, jpItem{}, jpBase{}, nil, nil
		}
		key, value = d.Obj[i].Key, jpDocItem(&d.Obj[i].Val)
	} else {
		v, ok := jpKvMember(f.item.kv, i)
		if !ok {
			return jpOutDone, jpItem{}, jpBase{}, nil, nil
		}
		key, value = jpKvKeys[i], v
	}
	kv := &jpKvPair{key: key, value: value, id: f.kvID, generated: cx.nextGen, tree: cx.baseTree(f.base)}
	cx.nextGen++
	if err := cx.charge(1); err != nil {
		return jpOutDone, jpItem{}, jpBase{}, nil, err
	}
	return jpOutItem, jpItem{kind: jpItemKv, kv: kv}, jpBase{kv: kv}, nil, nil
}

// jpKvKeys is the member names of a .keyvalue() object, in canonical key order.
var jpKvKeys = [3]string{"id", "key", "value"}

// jpKvMember is the i-th member value of a .keyvalue() object.
func jpKvMember(kv *jpKvPair, i int) (jpItem, bool) {
	switch i {
	case 0:
		return jpValItem(JsonNode{Kind: JNumber, Num: decimalFromInt64(kv.id)}), true
	case 1:
		return jpValItem(JsonNode{Kind: JString, S: kv.key}), true
	case 2:
		return kv.value, true
	default:
		return jpItem{}, false
	}
}

// memberOf is `.key` on one item (§7 rule 3): one unit per member examined (stored order, up to and
// including the first match). A missing member or a non-object raises in strict and yields nothing
// in lax.
func (cx *jpCtx) memberOf(item jpItem, key string) (jpItem, bool, error) {
	var child jpItem
	found := false
	switch {
	case item.isDocKind(JObject):
		members := item.node.Obj
		at := -1
		for i := range members {
			if members[i].Key == key {
				at = i
				break
			}
		}
		examined := len(members)
		if at >= 0 {
			examined = at + 1
			child, found = jpDocItem(&members[at].Val), true
		}
		if err := cx.charge(int64(examined)); err != nil {
			return jpItem{}, false, err
		}
	case item.kind == jpItemKv:
		at := -1
		for i, k := range jpKvKeys {
			if k == key {
				at = i
				break
			}
		}
		examined := len(jpKvKeys)
		if at >= 0 {
			examined = at + 1
			child, found = jpKvMember(item.kv, at)
		}
		if err := cx.charge(int64(examined)); err != nil {
			return jpItem{}, false, err
		}
	case cx.strict:
		return jpItem{}, false, newError(SqlJsonMemberNotFound, "jsonpath member accessor can only be applied to an object")
	default:
		return jpItem{}, false, nil
	}
	if !found && cx.strict {
		return jpItem{}, false, newError(SqlJsonMemberNotFound, "JSON object does not contain key \""+key+"\"")
	}
	return child, found, nil
}

// subscriptRange resolves one subscript against an array of size elements → its inclusive (from, to)
// range, clamped. An index is truncated toward zero; one outside the integer range raises in both
// modes, and strict raises on an out-of-bounds or reversed range, which lax just leaves (partly)
// empty. A non-literal index (`last - 1`, `$.i`) is evaluated with last naming this array and must be
// a single number (22033).
func (cx *jpCtx) subscriptRange(sub *jpSubscript, size int64, current jpItem, base jpBase) (int64, int64, error) {
	from, err := cx.indexValue(sub.a, size, current, base)
	if err != nil {
		return 0, 0, err
	}
	to := from
	if sub.slice {
		if to, err = cx.indexValue(sub.b, size, current, base); err != nil {
			return 0, 0, err
		}
	}
	if cx.strict && (from < 0 || from > to || to >= size) {
		return 0, 0, newError(InvalidSqlJsonSubscript, "jsonpath array subscript is out of bounds")
	}
	return max(from, 0), min(to, size-1), nil
}

// indexValue is one subscript index's value (PostgreSQL's getArrayIndex).
func (cx *jpCtx) indexValue(i jpIndex, size int64, current jpItem, base jpBase) (int64, error) {
	if i.last {
		return size - 1, nil
	}
	n := i.number
	if i.expr != nil {
		seq, err := cx.evalSeq(i.expr, current, base, size, false)
		if err != nil {
			return 0, err
		}
		var node *JsonNode
		if len(seq) == 1 {
			node = seq[0].scalarNode()
		}
		if node == nil || node.Kind != JNumber {
			return 0, newError(InvalidSqlJsonSubscript, "jsonpath array subscript is not a single numeric value")
		}
		n = node.Num
	}
	v, ok := n.TruncPlaces(0).ToInt64Round()
	if !ok || v < -2147483648 || v > 2147483647 {
		return 0, newError(InvalidSqlJsonSubscript, "jsonpath array subscript is out of integer range")
	}
	return v, nil
}

func isTrue(b *bool) bool { return b != nil && *b }

func boolPtr(b bool) *bool { return &b }

// evalPred evaluates a filter predicate to a Kleene truth value (&true / &false / nil = unknown).
// Both operands of &&/|| are always evaluated (no short-circuit), so their cost is fixed (§7).
func (cx *jpCtx) evalPred(pred *jpPred, current jpItem, base jpBase, last int64) (*bool, error) {
	switch pred.kind {
	case jpPredOr, jpPredAnd:
		x, err := cx.evalPred(pred.left, current, base, last)
		if err != nil {
			return nil, err
		}
		y, err := cx.evalPred(pred.right, current, base, last)
		if err != nil {
			return nil, err
		}
		if pred.kind == jpPredOr {
			switch {
			case isTrue(x) || isTrue(y):
				return boolPtr(true), nil
			case x != nil && y != nil:
				return boolPtr(false), nil
			}
			return nil, nil
		}
		switch {
		case (x != nil && !*x) || (y != nil && !*y):
			return boolPtr(false), nil
		case isTrue(x) && isTrue(y):
			return boolPtr(true), nil
		}
		return nil, nil
	case jpPredNot:
		v, err := cx.evalPred(pred.left, current, base, last)
		if err != nil || v == nil {
			return nil, err
		}
		return boolPtr(!*v), nil
	default:
		return cx.evalCompare(&pred.cmpLeft, pred.cmpOp, &pred.cmpRight, current, base, last)
	}
}

// evalCompare is an existential comparison (§4.4) over every pair (a in lhs-seq, b in rhs-seq), left
// outer. An operand error → nil (unknown); the right operand is not evaluated when the left one
// fails. lax: any true pair → true, else any incomparable pair → unknown, else false (so an empty
// operand is false). strict: any incomparable pair → unknown, even beside a true pair. Each tested
// pair charges its unit plus a SQL comparison's size extra (§7 rule 4).
func (cx *jpCtx) evalCompare(l *jpPathExpr, op jpCmpOp, r *jpPathExpr, current jpItem, base jpBase, last int64) (*bool, error) {
	ls, ok, err := cx.evalOperand(l, current, base, last)
	if err != nil || !ok {
		return nil, err
	}
	rs, ok, err := cx.evalOperand(r, current, base, last)
	if err != nil || !ok {
		return nil, err
	}
	strict := cx.strict
	found := false
	anyUnknown := false
	for i := range ls {
		for j := range rs {
			if err := cx.charge(1); err != nil {
				return nil, err
			}
			a, b := ls[i].scalarNode(), rs[j].scalarNode()
			switch {
			case a != nil && b != nil && a.Kind == JString && b.Kind == JString:
				w := min(utf8.RuneCountInString(a.S), utf8.RuneCountInString(b.S))
				cx.m.Charge(costs.VarlenCompare * int64(max(w, 1)-1))
				if err := cx.m.Guard(); err != nil {
					return nil, err
				}
			case a != nil && b != nil && a.Kind == JNumber && b.Kind == JNumber:
				cx.m.Charge(costs.DecimalWork * (workLinear(a.Num, b.Num) - 1))
				if err := cx.m.Guard(); err != nil {
					return nil, err
				}
			}
			c := jpCompareItems(ls[i], op, rs[j])
			switch {
			case c == nil && strict:
				return nil, nil
			case c == nil:
				anyUnknown = true
			case *c && !strict:
				return boolPtr(true), nil
			case *c:
				found = true
			}
		}
	}
	switch {
	case found:
		return boolPtr(true), nil
	case anyUnknown:
		return nil, nil
	default:
		return boolPtr(false), nil
	}
}

// evalOperand evaluates a comparison operand to its item sequence, an array item unwrapped one level
// in lax (§4.1 rule 4). ok=false is a SQL/JSON error, which makes the comparison unknown rather than
// propagating (§4.2: filter operands never raise). A cost, budget, or cancellation error still
// propagates (§7).
func (cx *jpCtx) evalOperand(e *jpPathExpr, current jpItem, base jpBase, last int64) ([]jpItem, bool, error) {
	seq, err := cx.evalSeq(e, current, base, last, true)
	if err != nil {
		if isSQLJSONError(err) {
			return nil, false, nil
		}
		return nil, false, err
	}
	return seq, true, nil
}

// jpCompareItems compares two items under a jsonpath operator (§4.4; a *bool: &v / nil = unknown).
// Same-type scalars compare by their order (numbers numerically, strings by code point,
// false < true, null = null) under every operator. null against any other item is `!=` and nothing
// else. Any other pair (mixed non-null types, or two arrays/objects) is nil (unknown). A .keyvalue()
// object is an object.
func jpCompareItems(x jpItem, op jpCmpOp, y jpItem) *bool {
	a, b := x.scalarNode(), y.scalarNode()
	if a == nil || b == nil {
		// An object against null is `!=`; against anything else it is not comparable.
		if (a != nil && a.Kind == JNull) || (b != nil && b.Kind == JNull) {
			return boolPtr(op == jpCmpNe)
		}
		return nil
	}
	var ord int
	switch {
	case a.Kind == JNumber && b.Kind == JNumber:
		ord = a.Num.CmpValue(b.Num)
	case a.Kind == JString && b.Kind == JString:
		ord = strings.Compare(a.S, b.S)
	case a.Kind == JBool && b.Kind == JBool:
		ord = cmpInt(boolRank(a.B), boolRank(b.B))
	case a.Kind == JNull && b.Kind == JNull:
		ord = 0
	case a.Kind == JNull || b.Kind == JNull:
		return boolPtr(op == jpCmpNe)
	default:
		return nil // mixed types and containers are not comparable
	}
	switch op {
	case jpCmpEq:
		return boolPtr(ord == 0)
	case jpCmpNe:
		return boolPtr(ord != 0)
	case jpCmpLt:
		return boolPtr(ord < 0)
	case jpCmpLe:
		return boolPtr(ord <= 0)
	case jpCmpGt:
		return boolPtr(ord > 0)
	default: // jpCmpGe
		return boolPtr(ord >= 0)
	}
}

// ---------------------------------------------------------------------------------------------
// Item methods (jsonpath.md §3.1) — PostgreSQL's executeItemOptUnwrapTarget method arms.
// ---------------------------------------------------------------------------------------------

// jpNonNumeric is 22036 non_numeric_sql_json_item — the class every method coercion failure raises.
func jpNonNumeric(msg string) error { return newError(NonNumericSqlJsonItem, msg) }

// jpWrongKind is the "can only be applied to …" error of a method applied to the wrong kind of item.
func jpWrongKind(m jpMethod, what string) error {
	return jpNonNumeric("jsonpath item method ." + m.name() + "() can only be applied to " + what)
}

// jpBadArgument is the "argument … is invalid for type …" error of a failed conversion.
func jpBadArgument(m jpMethod, arg, ty string) error {
	return jpNonNumeric("argument \"" + arg + "\" of jsonpath item method ." + m.name() +
		"() is invalid for type " + ty)
}

func jpNaNOrInfinity(m jpMethod) error {
	return jpNonNumeric("NaN or Infinity is not allowed for jsonpath item method ." + m.name() + "()")
}

// jpApplyMethod applies a single-result item method (every method except .keyvalue()) to one item.
func jpApplyMethod(m jpMethod, item jpItem, strict bool) (jpItem, error) {
	node := item.scalarNode()
	kindIs := func(k JsonNodeKind) bool { return node != nil && node.Kind == k }
	switch m.kind {
	case jmType:
		return jpValItem(JsonNode{Kind: JString, S: item.typeName()}), nil
	case jmSize:
		n := int64(1) // lax: a non-array is a singleton array (§4.1 rule 3)
		if kindIs(JArray) {
			n = int64(len(node.Arr))
		} else if strict {
			return jpItem{}, newError(SqlJsonArrayNotFound, "jsonpath item method .size() can only be applied to an array")
		}
		return jpValItem(JsonNode{Kind: JNumber, Num: decimalFromInt64(n)}), nil
	case jmAbs, jmFloor, jmCeiling:
		if !kindIs(JNumber) {
			return jpItem{}, jpWrongKind(m, "a numeric value")
		}
		var v Decimal
		var err error
		switch m.kind {
		case jmAbs:
			v = node.Num.Abs()
		case jmFloor:
			v, err = node.Num.Floor()
		default:
			v, err = node.Num.Ceil()
		}
		if err != nil {
			return jpItem{}, err
		}
		return jpValItem(JsonNode{Kind: JNumber, Num: v}), nil
	case jmDouble:
		// A number is only checked to be a valid double (the item itself is the result); a string is
		// read as a double and converted back the way PostgreSQL's float8_numeric does.
		switch {
		case kindIs(JNumber):
			if _, err := jpFloat8In(m, node.Num.Render()); err != nil {
				return jpItem{}, err
			}
			return item, nil
		case kindIs(JString):
			v, err := jpFloat8In(m, node.S)
			if err != nil {
				return jpItem{}, err
			}
			return jpValItem(JsonNode{Kind: JNumber, Num: decimalFromFloat64DblDig(v)}), nil
		}
		return jpItem{}, jpWrongKind(m, "a string or numeric value")
	case jmBigint, jmInteger:
		ty, st := "bigint", scalarInt64
		if m.kind == jmInteger {
			ty, st = "integer", scalarInt32
		}
		var v int64
		switch {
		case kindIs(JNumber):
			// A number rounds half away from zero (numeric → int) and must fit the type.
			r, ok := node.Num.ToInt64Round()
			if !ok || !st.InRange(r) {
				return jpItem{}, jpBadArgument(m, node.Num.Render(), ty)
			}
			v = r
		case kindIs(JString):
			r, err := parseIntLiteral(node.S, st)
			if err != nil {
				return jpItem{}, jpBadArgument(m, node.S, ty)
			}
			v = r
		default:
			return jpItem{}, jpWrongKind(m, "a string or numeric value")
		}
		return jpValItem(JsonNode{Kind: JNumber, Num: decimalFromInt64(v)}), nil
	case jmBoolean:
		var b bool
		switch {
		case kindIs(JBool):
			return item, nil
		case kindIs(JNumber):
			// A number must read as an integer (its text through int4 input): nonzero is true.
			text := node.Num.Render()
			r, err := parseIntLiteral(text, scalarInt32)
			if err != nil {
				return jpItem{}, jpBadArgument(m, text, "boolean")
			}
			b = r != 0
		case kindIs(JString):
			v, ok := jpParseBool(node.S)
			if !ok {
				return jpItem{}, jpBadArgument(m, node.S, "boolean")
			}
			b = v
		default:
			return jpItem{}, jpWrongKind(m, "a boolean, string, or numeric value")
		}
		return jpValItem(JsonNode{Kind: JBool, B: b}), nil
	case jmNumber, jmDecimal:
		var num Decimal
		var numstr string
		switch {
		case kindIs(JNumber):
			num, numstr = node.Num, node.Num.Render()
		case kindIs(JString):
			d, err := jpNumericIn(m, node.S)
			if err != nil {
				return jpItem{}, err
			}
			num, numstr = d, node.S
		default:
			return jpItem{}, jpWrongKind(m, "a string or numeric value")
		}
		if m.kind == jmDecimal && m.precision != nil {
			d, err := jpApplyTypmod(m, num, numstr)
			if err != nil {
				return jpItem{}, err
			}
			return jpValItem(JsonNode{Kind: JNumber, Num: d}), nil
		}
		if kindIs(JNumber) {
			return item, nil
		}
		return jpValItem(JsonNode{Kind: JNumber, Num: num}), nil
	default: // jmString
		var s string
		switch {
		case kindIs(JString):
			return item, nil
		case kindIs(JNumber):
			s = node.Num.Render()
		case kindIs(JBool):
			s = "false"
			if node.B {
				s = "true"
			}
		default:
			return jpItem{}, jpWrongKind(m, "a boolean, string, numeric, or datetime value")
		}
		return jpValItem(JsonNode{Kind: JString, S: s}), nil
	}
}

// jpFloat8In is PostgreSQL's float8in over a method argument: surrounding whitespace, a sign, digits
// with an optional point and exponent, or the special spellings. A result that overflows binary64, or
// a nonzero mantissa that underflows to zero, is out of range. NaN and ±Infinity are refused.
func jpFloat8In(m jpMethod, text string) (float64, error) {
	t := strings.Trim(text, litWhitespace)
	v, err := parseFloatLiteral(t, scalarFloat64)
	if err != nil {
		return 0, jpBadArgument(m, text, "double precision")
	}
	if v != v || v > 1.7976931348623157e308 || v < -1.7976931348623157e308 {
		return 0, jpNaNOrInfinity(m)
	}
	mantissa := t
	if i := strings.IndexAny(t, "eE"); i >= 0 {
		mantissa = t[:i]
	}
	if v == 0 && strings.ContainsAny(mantissa, "123456789") {
		return 0, jpBadArgument(m, text, "double precision")
	}
	return v, nil
}

// jpNumericIn is PostgreSQL's numeric_in over a method argument (jed's own decimal grammar,
// grammar.md §36), refusing NaN and ±Infinity with their own message.
func jpNumericIn(m jpMethod, text string) (Decimal, error) {
	if d, err := parseDecimalLiteral(text); err == nil {
		if d, err = d.CheckCap(); err == nil {
			return d, nil
		}
	}
	word := toLowerASCII(strings.Trim(text, litWhitespace))
	body := strings.TrimPrefix(strings.TrimPrefix(word, "+"), "-")
	if len(word)-len(body) > 1 {
		body = word
	}
	if word == "nan" || body == "infinity" || body == "inf" {
		return Decimal{}, jpNaNOrInfinity(m)
	}
	return Decimal{}, jpBadArgument(m, text, "numeric")
}

// jpApplyTypmod is `.decimal(p [, s])`: round to numeric(p, s) (PostgreSQL's typmod rules —
// precision 1..1000, scale −1000..1000, rounding half away from zero) and require fewer than p − s
// integer digits.
func jpApplyTypmod(m jpMethod, num Decimal, numstr string) (Decimal, error) {
	asI32 := func(d Decimal) (int64, bool) {
		v, ok := d.ToInt64Round()
		return v, ok && v >= -2147483648 && v <= 2147483647
	}
	precision, ok := asI32(*m.precision)
	if !ok {
		return Decimal{}, jpNonNumeric("precision of jsonpath item method .decimal() is out of range for type integer")
	}
	scale := int64(0)
	if m.scale != nil {
		if scale, ok = asI32(*m.scale); !ok {
			return Decimal{}, jpNonNumeric("scale of jsonpath item method .decimal() is out of range for type integer")
		}
	}
	const maxP = int64(maxPrecision)
	if precision < 1 || precision > maxP {
		return Decimal{}, newError(InvalidParameterValue, "NUMERIC precision "+strconv.FormatInt(precision, 10)+
			" must be between 1 and "+strconv.FormatInt(maxP, 10))
	}
	if scale < -maxP || scale > maxP {
		return Decimal{}, newError(InvalidParameterValue, "NUMERIC scale "+strconv.FormatInt(scale, 10)+
			" must be between "+strconv.FormatInt(-maxP, 10)+" and "+strconv.FormatInt(maxP, 10))
	}
	rounded, err := num.RoundPlaces(scale)
	if err != nil {
		return Decimal{}, jpBadArgument(m, numstr, "numeric")
	}
	// The digits from the point to the most significant nonzero digit must fit p − s.
	intDigits := int64(rounded.Precision()) - int64(rounded.Scale)
	if !rounded.IsZero() && intDigits > precision-scale {
		return Decimal{}, jpBadArgument(m, numstr, "numeric")
	}
	return rounded, nil
}

// jpParseBool is PostgreSQL's parse_bool: case-insensitive true/false/yes/no and their prefixes, on,
// off (or of), 1, 0 — with no surrounding whitespace.
func jpParseBool(s string) (bool, bool) {
	lower := toLowerASCII(s)
	prefixOf := func(word string) bool { return lower != "" && strings.HasPrefix(word, lower) }
	if lower == "" {
		return false, false
	}
	switch lower[0] {
	case 't':
		if prefixOf("true") {
			return true, true
		}
	case 'f':
		if prefixOf("false") {
			return false, true
		}
	case 'y':
		if prefixOf("yes") {
			return true, true
		}
	case 'n':
		if prefixOf("no") {
			return false, true
		}
	case 'o':
		if lower == "on" {
			return true, true
		}
		if len(lower) >= 2 && prefixOf("off") {
			return false, true
		}
	case '1':
		if len(lower) == 1 {
			return true, true
		}
	case '0':
		if len(lower) == 1 {
			return false, true
		}
	}
	return false, false
}

// ---------------------------------------------------------------------------------------------
// .keyvalue() ids — byte offsets in PostgreSQL's binary jsonb layout (jsonpath.md §3.1).
// ---------------------------------------------------------------------------------------------

// keyvalueID is the id of a .keyvalue() object: 10^10 × base number + the object's byte offset inside
// the base object, where the base is the context item (number 0), a variable's base object (§5), or
// the .keyvalue() object whose continuation is running. PostgreSQL measures the offset in its binary
// jsonb layout, which is a pure function of the value, so jed computes the same layout
// arithmetically.
func (cx *jpCtx) keyvalueID(item jpItem, base jpBase) int64 {
	const baseStride = int64(10_000_000_000)
	if base.kv == nil && base.varTree == nil {
		if d := item.doc(); d != nil {
			return cx.treeOffset(cx.root, d)
		}
		return 0
	}
	if base.kv == nil {
		var offset int64
		if d := item.doc(); d != nil {
			offset = cx.treeOffset(base.varTree, d)
		}
		return base.varID*baseStride + offset
	}
	kv := base.kv
	var offset int64
	switch {
	case item.kind == jpItemKv && item.kv == kv:
		offset = 0
	case item.kind == jpItemDoc && kv.value.kind == jpItemDoc:
		// A container inside the pair's value: the value's offset in the pair object plus the
		// container's offset inside the value (both are 4-byte aligned, so the value's internal
		// layout does not depend on where it sits). Both lie in the base object kv.tree.
		valueAt := jpAlign4(40 + pgNumericSize(decimalFromInt64(kv.id)) + int64(len(kv.key)))
		offset = valueAt + cx.treeOffset(kv.tree, item.node) - cx.treeOffset(kv.tree, kv.value.node)
	}
	return kv.generated*baseStride + offset
}

// baseTree is the base object (the document or a variable's) whose document items a continuation
// under base reaches.
func (cx *jpCtx) baseTree(base jpBase) *JsonNode {
	switch {
	case base.kv != nil:
		return base.kv.tree
	case base.varTree != nil:
		return base.varTree
	default:
		return cx.root
	}
}

// treeOffset is a container's offset in the binary layout of the base object tree that holds it,
// laying the base object out first if no .keyvalue() has yet. Each base object keeps its own offsets:
// two base objects may share nodes (Go jsonb values share their backing arrays), at different offsets.
func (cx *jpCtx) treeOffset(tree, n *JsonNode) int64 {
	if cx.offsets == nil {
		cx.offsets = map[*JsonNode]map[*JsonNode]int64{}
	}
	offsets, ok := cx.offsets[tree]
	if !ok {
		offsets = map[*JsonNode]int64{}
		pgLayout(tree, 0, offsets)
		cx.offsets[tree] = offsets
	}
	return offsets[n]
}

// variable is the value of the path variable $name with the base object its continuation runs under
// (PostgreSQL's getJsonPathVariable): a member of the vars object, found by a binary search of its
// canonical key order, or the first PASSING binding of that name. A name with no binding is 42704,
// which neither lax mode, silent mode, nor a comparison operand suppresses.
func (cx *jpCtx) variable(name string) (jpItem, jpBase, error) {
	switch cx.vars.kind {
	case jpVarsObject:
		tree := cx.vars.object
		members := tree.Obj
		i := sort.Search(len(members), func(i int) bool { return jsonKeyCmp(members[i].Key, name) >= 0 })
		if tree.Kind == JObject && i < len(members) && members[i].Key == name {
			return jpDocItem(&members[i].Val), jpBase{varTree: tree, varID: 1}, nil
		}
	case jpVarsPassing:
		vars := cx.vars.passing
		i := sort.Search(len(vars), func(i int) bool { return vars[i].name >= name })
		if i < len(vars) && vars[i].name == name {
			v := &vars[i]
			return jpDocItem(&v.value), jpBase{varTree: &v.value, varID: v.id}, nil
		}
	}
	return jpItem{}, jpBase{}, newError(UndefinedObject, `could not find jsonpath variable "`+name+`"`)
}

func jpAlign4(off int64) int64 { return (off + 3) &^ 3 }

// pgLayout lays n out at byte offset off the way PostgreSQL's convertJsonbValue does, recording each
// container's start, and returns the end offset. A container is 4-byte aligned: a header word, one
// 4-byte entry per element (two per object member — keys, then values), then the keys' bytes and the
// element/value data. A string is its UTF-8 bytes, a number its aligned numeric datum, and
// null/true/false take no data bytes.
func pgLayout(n *JsonNode, off int64, offsets map[*JsonNode]int64) int64 {
	switch n.Kind {
	case JString:
		return off + int64(len(n.S))
	case JNumber:
		return jpAlign4(off) + pgNumericSize(n.Num)
	case JArray:
		start := jpAlign4(off)
		offsets[n] = start
		p := start + 4 + 4*int64(len(n.Arr))
		for i := range n.Arr {
			p = pgLayout(&n.Arr[i], p, offsets)
		}
		return p
	case JObject:
		start := jpAlign4(off)
		offsets[n] = start
		p := start + 4 + 8*int64(len(n.Obj))
		for i := range n.Obj {
			p += int64(len(n.Obj[i].Key))
		}
		for i := range n.Obj {
			p = pgLayout(&n.Obj[i].Val, p, offsets)
		}
		return p
	default: // null, boolean
		return off
	}
}

// pgNumericSize is the byte size of PostgreSQL's numeric datum for d: a 4-byte varlena header, a
// 2-byte header (short form) or 4 bytes (long form), and one 2-byte base-10000 digit per group from
// the most to the least significant nonzero group. The short form needs a display scale ≤ 63 and a
// weight (the leading group's power of 10000) in −64..=63. Zero has no digits.
func pgNumericSize(d Decimal) int64 {
	text := strings.TrimPrefix(d.Render(), "-")
	intPart, frac, _ := strings.Cut(text, ".")
	digits := intPart + frac
	first := strings.IndexFunc(digits, func(r rune) bool { return r != '0' })
	if first < 0 {
		return 6
	}
	last := strings.LastIndexFunc(digits, func(r rune) bool { return r != '0' })
	power := func(i int) int64 { return int64(len(intPart) - 1 - i) }
	floorDiv4 := func(v int64) int64 {
		if v < 0 {
			return -((-v + 3) / 4)
		}
		return v / 4
	}
	weight := floorDiv4(power(first))
	ndigits := weight - floorDiv4(power(last)) + 1
	size := int64(8)
	if d.Scale <= 63 && weight >= -64 && weight <= 63 {
		size = 6
	}
	return size + 2*ndigits
}

func writeIndex(i jpIndex, out *strings.Builder) {
	switch {
	case i.last:
		out.WriteString("last")
	case i.expr != nil:
		writeExpr(i.expr, false, out)
	default:
		out.WriteString(i.number.Render())
	}
}

// writeQuoted renders a member key as a canonical jsonpath quoted string — PostgreSQL's escape_json:
// `\"`, `\\`, `\b \f \n \r \t`, and any other control character as `\u00XX`.
func writeQuoted(k string, out *strings.Builder) {
	out.WriteByte('"')
	for _, c := range k {
		switch c {
		case '"':
			out.WriteString("\\\"")
		case '\\':
			out.WriteString("\\\\")
		case '\b':
			out.WriteString("\\b")
		case '\f':
			out.WriteString("\\f")
		case '\n':
			out.WriteString("\\n")
		case '\r':
			out.WriteString("\\r")
		case '\t':
			out.WriteString("\\t")
		default:
			if c < 0x20 {
				out.WriteString("\\u")
				h := strconv.FormatInt(int64(c), 16)
				for len(h) < 4 {
					h = "0" + h
				}
				out.WriteString(h)
			} else {
				out.WriteRune(c)
			}
		}
	}
	out.WriteByte('"')
}

// jpIsSpecial reports PostgreSQL's jsonpath scanner `special` set (jsonpath_scan.l): each is a token
// of its own, so it ends an unquoted name.
func jpIsSpecial(c byte) bool {
	return strings.IndexByte("?%$.[]{}()|&!=<>@#,*:-+/", c) >= 0
}

// jpIsBlank reports PostgreSQL's jsonpath `blank` set — whitespace.
func jpIsBlank(c byte) bool {
	return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\f'
}

// jpIsOther reports PostgreSQL's jsonpath `other` class: a byte that may appear unescaped in an
// unquoted name — not special, blank, `\`, or `"`. Every non-ASCII byte is one.
func jpIsOther(c byte) bool {
	return !jpIsSpecial(c) && !jpIsBlank(c) && c != '\\' && c != '"'
}

func jpIsOctDigit(c byte) bool { return c >= '0' && c <= '7' }

func jpIsBinDigit(c byte) bool { return c == '0' || c == '1' }

// jpNumberLen is the length of the longest PostgreSQL jsonpath number token at the start of s (which
// starts with a digit), counting the rules a name competes with: an integer (decimal, or a
// `0x`/`0o`/`0b` prefix), a decimal, a real, an incomplete exponent (`1e+`), and each with one
// trailing `other` byte (the "trailing junk" rules). `_` digit separators count, as PostgreSQL 16+
// lexes them.
func jpNumberLen(s []byte) int {
	at := func(j int) (byte, bool) {
		if j < len(s) {
			return s[j], true
		}
		return 0, false
	}
	// digitRun is the end of `(_?digit)*` from j (a first digit precedes it).
	digitRun := func(j int, ok func(byte) bool) int {
		for {
			c, has := at(j)
			switch {
			case has && ok(c):
				j++
			case has && c == '_':
				if n, hasN := at(j + 1); hasN && ok(n) {
					j += 2
				} else {
					return j
				}
			default:
				return j
			}
		}
	}
	// junk is a "trailing junk" rule: the token plus one `other` byte.
	junk := func(end int) int {
		if c, has := at(end); has && jpIsOther(c) {
			return end + 1
		}
		return end
	}
	// decinteger: `0`, or a nonzero digit and (_?digit)*.
	intEnd := 1
	if s[0] != '0' {
		intEnd = digitRun(1, isASCIIDigit)
	}
	best := junk(intEnd)
	// hexinteger / octinteger / bininteger: `0x`, `0o`, `0b` and one or more digits.
	if s[0] == '0' {
		var ok func(byte) bool
		if c, has := at(1); has {
			switch c {
			case 'x', 'X':
				ok = isHexDigit
			case 'o', 'O':
				ok = jpIsOctDigit
			case 'b', 'B':
				ok = jpIsBinDigit
			}
		}
		if c, has := at(2); ok != nil && has && ok(c) {
			best = max(best, digitRun(3, ok))
		}
	}
	// decimal: decinteger `.` decdigits?
	mantissas := []int{intEnd}
	if c, has := at(intEnd); has && c == '.' {
		end := intEnd + 1
		if f, hasF := at(end); hasF && isASCIIDigit(f) {
			end = digitRun(end+1, isASCIIDigit)
		}
		best = max(best, junk(end))
		mantissas = append(mantissas, end)
	}
	// real: (decinteger | decimal) [Ee] [-+]? decdigits; realfail: … [Ee] [-+].
	for _, m := range mantissas {
		if c, has := at(m); !has || (c != 'e' && c != 'E') {
			continue
		}
		k := m + 1
		signed := false
		if c, has := at(k); has && (c == '-' || c == '+') {
			signed = true
			k++
		}
		if c, has := at(k); has && isASCIIDigit(c) {
			best = max(best, junk(digitRun(k+1, isASCIIDigit)))
		} else if signed {
			best = max(best, k)
		}
	}
	return best
}

// jpKeywordIs reports whether PostgreSQL keyword kw matches the decoded word w: exactly for `null` /
// `true` / `false`, ASCII-case-insensitively for the rest (checkKeyword).
func jpKeywordIs(w, kw string) bool {
	if kw == "null" || kw == "true" || kw == "false" {
		return w == kw
	}
	return asciiEqualFold(w, kw)
}

// asciiEqualFold compares two strings, folding only ASCII letters (PostgreSQL's pg_strncasecmp).
func asciiEqualFold(a, b string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := 0; i < len(a); i++ {
		x, y := a[i], b[i]
		if 'A' <= x && x <= 'Z' {
			x += 'a' - 'A'
		}
		if 'A' <= y && y <= 'Z' {
			y += 'a' - 'A'
		}
		if x != y {
			return false
		}
	}
	return true
}

func isASCIIDigit(c byte) bool { return c >= '0' && c <= '9' }

func isIdentByte(c byte) bool {
	return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || isASCIIDigit(c) || c == '_'
}

type jpParser struct {
	s []byte
	i int
	// depth is the program nesting depth (spec/design/json.md §6.4): +1 per filter `?(`,
	// parenthesized group, `!`, `&&`/`||` chain step, arithmetic operator, and subscript list,
	// bounded by maxJSONDepth → 54001. Bounding it at the compiler bounds every derived walk (render,
	// evaluate).
	depth int
	// inFilter is how many filters enclose the cursor — `@` is valid only inside one.
	inFilter int
	// inSubscript is how many subscripts enclose the cursor — `last` is valid only inside one.
	inSubscript int
}

// jpParsed is a parsed operand: an expression or a predicate — PostgreSQL's expr_or_predicate.
// Exactly one is non-nil. A parenthesized predicate is an expression only when accessors follow it.
type jpParsed struct {
	expr *jpPathExpr
	pred *jpPred
}

// needExpr requires an expression where the grammar takes one (42601 for a bare predicate).
func needExpr(p jpParsed, err error) (*jpPathExpr, error) {
	if err != nil {
		return nil, err
	}
	if p.expr == nil {
		return nil, jpMalformed("expected an expression, not a predicate")
	}
	return p.expr, nil
}

// needPred requires a predicate where the grammar takes one (42601 for an expression).
func needPred(p jpParsed, err error) (*jpPred, error) {
	if err != nil {
		return nil, err
	}
	if p.pred == nil {
		return nil, jpMalformed("expected a predicate")
	}
	return p.pred, nil
}

// deepen descends one program nesting level, enforcing maxJSONDepth (json.md §6.4). The caller
// restores it on the success path (an error aborts the whole compile).
func (p *jpParser) deepen() error {
	p.depth++
	if p.depth > maxJSONDepth {
		return newError(StatementTooComplex,
			"jsonpath nesting depth exceeds the maximum of "+strconv.Itoa(maxJSONDepth))
	}
	return nil
}

func (p *jpParser) peek() (byte, bool) {
	if p.i < len(p.s) {
		return p.s[p.i], true
	}
	return 0, false
}

func (p *jpParser) peekAt(k int) (byte, bool) {
	if p.i+k < len(p.s) {
		return p.s[p.i+k], true
	}
	return 0, false
}

func (p *jpParser) eat(c byte) bool {
	if pc, ok := p.peek(); ok && pc == c {
		p.i++
		return true
	}
	return false
}

// skipWs skips jsonpath whitespace (PostgreSQL's [ \t\n\r\f]).
func (p *jpParser) skipWs() {
	for p.i < len(p.s) {
		switch p.s[p.i] {
		case ' ', '\t', '\n', '\r', '\f':
			p.i++
		default:
			return
		}
	}
}

// wordAhead reports whether PostgreSQL's scanner would lex a name (an identifier or keyword) at the
// cursor: a backslash escape or an `other` byte. A digit starts a number instead, unless the run of
// `other` bytes there is longer than every number token (flex's longest match: `1abc` is a name,
// `12`, `1a`, and `1e3` are not).
func (p *jpParser) wordAhead() bool {
	c, ok := p.peek()
	switch {
	case !ok:
		return false
	case c == '\\':
		return true
	case isASCIIDigit(c):
		rest := p.s[p.i:]
		run := 0
		for run < len(rest) && jpIsOther(rest[run]) {
			run++
		}
		return run > jpNumberLen(rest)
	default:
		return jpIsOther(c)
	}
}

// scanWord scans the name at the cursor (wordAhead) — PostgreSQL's non-quoted string state: a run of
// `other` bytes and backslash escapes (decoded as in a quoted string, jsonpath.md §2.1), ended by a
// special byte, whitespace, `"`, or the end of input.
func (p *jpParser) scanWord() (string, error) {
	var out strings.Builder
	for {
		c, ok := p.peek()
		switch {
		case ok && c == '\\':
			if err := p.parseEscape(&out); err != nil {
				return "", err
			}
		case ok && jpIsOther(c):
			p.copyChar(&out)
		default:
			return out.String(), nil
		}
	}
}

// eatKeyword consumes keyword kw if the name at the cursor is that keyword (jpKeywordIs). A name that
// is not — or whose escapes fail to decode — is left unconsumed.
func (p *jpParser) eatKeyword(kw string) bool {
	if !p.wordAhead() {
		return false
	}
	start := p.i
	if w, err := p.scanWord(); err == nil && jpKeywordIs(w, kw) {
		return true
	}
	p.i = start
	return false
}

// parseQuoted parses a `"…"` jsonpath string — a port of PostgreSQL's jsonpath scanner escapes
// (jsonpath_scan.l, jsonpath.md §2.1): `\b \f \n \r \t \v`; a run of `\uXXXX` / `\u{X…}` (1–6 hex
// digits) escapes, within which a surrogate pair combines; `\xHH`; and a backslash before any other
// character is that character. A decoded NUL is 22P05, an unpaired surrogate 22P02 (PG's Detail), and
// a malformed `\u` / `\x` or a code point above U+10FFFF 42601.
func (p *jpParser) parseQuoted() (string, error) {
	p.i++ // opening "
	var out strings.Builder
	for {
		c, ok := p.peek()
		if !ok {
			return "", jpMalformed("unterminated string")
		}
		switch c {
		case '"':
			p.i++
			return out.String(), nil
		case '\\':
			if err := p.parseEscape(&out); err != nil {
				return "", err
			}
		default:
			p.copyChar(&out)
		}
	}
}

// parseEscape decodes the backslash escape at the cursor into out — shared by quoted strings and
// unquoted names (jsonpath.md §2.1).
func (p *jpParser) parseEscape(out *strings.Builder) error {
	if p.i+1 >= len(p.s) {
		return jpMalformed("unexpected end after backslash")
	}
	switch ec := p.s[p.i+1]; ec {
	case 'u':
		return p.parseUnicodeRun(out)
	case 'x':
		if p.i+4 > len(p.s) || !isHexDigit(p.s[p.i+2]) || !isHexDigit(p.s[p.i+3]) {
			return jpMalformed("invalid hexadecimal character sequence")
		}
		ch, _ := strconv.ParseUint(string(p.s[p.i+2:p.i+4]), 16, 32)
		if err := jpPushCodePoint(rune(ch), out); err != nil {
			return err
		}
		p.i += 4
	default:
		p.i++ // the backslash; the escaped character follows
		switch ec {
		case 'b':
			out.WriteByte('\b')
		case 'f':
			out.WriteByte('\f')
		case 'n':
			out.WriteByte('\n')
		case 'r':
			out.WriteByte('\r')
		case 't':
			out.WriteByte('\t')
		case 'v':
			out.WriteByte('\v')
		default:
			p.copyChar(out)
			return nil
		}
		p.i++
	}
	return nil
}

// copyChar copies the one UTF-8 character at the cursor.
func (p *jpParser) copyChar(out *strings.Builder) {
	start := p.i
	n := utf8Len(p.s[p.i])
	if p.i+n > len(p.s) {
		n = len(p.s) - p.i
	}
	p.i += n
	out.Write(p.s[start:p.i])
}

// parseUnicodeRun parses a run of consecutive `\u` escapes (the cursor at the first `\`) — PG's
// `{unicode}+` token, lexed whole before any is decoded, so a malformed escape anywhere in the run is
// 42601 ahead of a decode error. Decoding is PG's addUnicode: a high surrogate must be followed,
// within the run, by a low one (the pair combines); a lone or doubled surrogate is 22P02.
func (p *jpParser) parseUnicodeRun(out *strings.Builder) error {
	var units []int
	for p.i+1 < len(p.s) && p.s[p.i] == '\\' && p.s[p.i+1] == 'u' {
		p.i += 2
		braced := p.eat('{')
		start := p.i
		max := 4
		if braced {
			max = 6
		}
		for p.i-start < max && p.i < len(p.s) && isHexDigit(p.s[p.i]) {
			p.i++
		}
		digits := p.i - start
		if (braced && (digits == 0 || !p.eat('}'))) || (!braced && digits < 4) {
			return jpMalformed("invalid Unicode escape sequence")
		}
		v, _ := strconv.ParseUint(string(p.s[start:start+digits]), 16, 32)
		units = append(units, int(v))
	}
	hiSurrogate := -1
	for _, ch := range units {
		if ch >= 0xD800 && ch <= 0xDBFF {
			if hiSurrogate != -1 {
				return jsonEscapeSyntaxError("jsonpath", jsonHighSurrogateDetail)
			}
			hiSurrogate = ch
			continue
		} else if ch >= 0xDC00 && ch <= 0xDFFF {
			if hiSurrogate == -1 {
				return jsonEscapeSyntaxError("jsonpath", jsonLowSurrogateDetail)
			}
			ch = 0x10000 + (((hiSurrogate - 0xD800) << 10) | (ch - 0xDC00))
			hiSurrogate = -1
		} else if hiSurrogate != -1 {
			return jsonEscapeSyntaxError("jsonpath", jsonLowSurrogateDetail)
		}
		if err := jpPushCodePoint(rune(ch), out); err != nil {
			return err
		}
	}
	if hiSurrogate != -1 {
		return jsonEscapeSyntaxError("jsonpath", jsonLowSurrogateDetail)
	}
	return nil
}

// jpPushCodePoint appends a decoded escape's code point — PG's addUnicodeChar: NUL is 22P05 (text
// cannot hold it), a value above U+10FFFF 42601 ("invalid Unicode code point").
func jpPushCodePoint(ch rune, out *strings.Builder) error {
	if ch == 0 {
		return jsonUnicodeZeroError()
	}
	if ch > utf8.MaxRune {
		return newError(SyntaxError, "invalid Unicode code point")
	}
	out.WriteRune(ch)
	return nil
}

// parseSubscripts parses a [...] subscript list (the opening `[` consumed, not the wildcard form).
// Each subscript is `index` or `index to index`, an index any expression (`1`, `last - 1`, `$.i`),
// in which `last` is allowed.
func (p *jpParser) parseSubscripts() ([]jpSubscript, error) {
	var subs []jpSubscript
	if err := p.deepen(); err != nil {
		return nil, err
	}
	p.inSubscript++
	for {
		a, err := p.parseIndex()
		if err != nil {
			return nil, err
		}
		p.skipWs()
		var sub jpSubscript
		if p.eatKeyword("to") {
			b, err := p.parseIndex()
			if err != nil {
				return nil, err
			}
			p.skipWs()
			sub = jpSubscript{a: a, b: b, slice: true}
		} else {
			sub = jpSubscript{a: a}
		}
		subs = append(subs, sub)
		c, ok := p.peek()
		switch {
		case ok && c == ',':
			p.i++
			continue
		case ok && c == ']':
			p.i++
			p.inSubscript--
			p.depth--
			return subs, nil
		default:
			return nil, jpMalformed("expected `,` or `]` in subscript")
		}
	}
}

// parseIndex parses one subscript index: a number literal or `last` keeps its own form; anything
// else is an expression evaluated per array.
func (p *jpParser) parseIndex() (jpIndex, error) {
	p.skipWs()
	if _, ok := p.peek(); !ok {
		return jpIndex{}, jpMalformed("expected a subscript index")
	}
	e, err := needExpr(p.parseAdditive())
	if err != nil {
		return jpIndex{}, err
	}
	switch {
	case e.primary == jpLiteral && e.lit.Kind == JNumber && len(e.steps) == 0:
		return jpIndex{number: e.lit.Num}, nil
	case e.primary == jpLast && len(e.steps) == 0:
		return jpIndex{last: true}, nil
	default:
		return jpIndex{expr: e}, nil
	}
}

// parsePostfix parses a primary and its accessor steps (PostgreSQL's accessor_expr). A parenthesized
// expression followed by accessors continues it (`($.a).b` is `$.a.b`, `(1 + 2).type()`); a
// parenthesized predicate followed by accessors yields its truth value; without accessors the
// parentheses only group.
func (p *jpParser) parsePostfix() (jpParsed, error) {
	p.skipWs()
	if p.eat('(') {
		if err := p.deepen(); err != nil {
			return jpParsed{}, err
		}
		inner, err := p.parseOr()
		if err != nil {
			return jpParsed{}, err
		}
		p.depth--
		p.skipWs()
		if !p.eat(')') {
			return jpParsed{}, jpMalformed("expected `)`")
		}
		steps, err := p.parseSteps()
		if err != nil {
			return jpParsed{}, err
		}
		if len(steps) == 0 {
			return inner, nil
		}
		if inner.pred != nil {
			return jpParsed{expr: &jpPathExpr{primary: jpPredHead, pred: inner.pred, steps: steps}}, nil
		}
		inner.expr.steps = append(inner.expr.steps, steps...)
		return inner, nil
	}
	c, ok := p.peek()
	next, _ := p.peekAt(1)
	e := &jpPathExpr{}
	switch {
	case !ok:
		return jpParsed{}, jpMalformed("unexpected end of input")
	case c == '$':
		p.i++
		// $"name" / $name — a path variable. PostgreSQL's unquoted variable name is a run of `other`
		// bytes (no escapes); a quoted one takes the string escapes (§2.1).
		if nc, ok := p.peek(); ok && nc == '"' {
			name, err := p.parseQuoted()
			if err != nil {
				return jpParsed{}, err
			}
			e.primary, e.name = jpVar, name
		} else if ok && jpIsOther(nc) {
			start := p.i
			for c, ok := p.peek(); ok && jpIsOther(c); c, ok = p.peek() {
				p.i++
			}
			// A run of `other` bytes never splits a UTF-8 sequence (every non-ASCII byte is one).
			e.primary, e.name = jpVar, string(p.s[start:p.i])
		} else {
			e.primary = jpRoot
		}
	case c == '@':
		if p.inFilter == 0 {
			return jpParsed{}, jpMalformed("@ is not allowed in root expressions")
		}
		p.i++
		e.primary = jpCurrent
	case c == '"':
		s, err := p.parseQuoted()
		if err != nil {
			return jpParsed{}, err
		}
		e.primary, e.lit = jpLiteral, JsonNode{Kind: JString, S: s}
	case isASCIIDigit(c) && p.wordAhead():
		// A digit-led name (`1abc`) is not an operand.
		if _, err := p.scanWord(); err != nil {
			return jpParsed{}, err
		}
		return jpParsed{}, jpMalformed("a name is not an operand")
	case isASCIIDigit(c) || (c == '.' && isASCIIDigit(next)):
		n, err := p.parseNumber()
		if err != nil {
			return jpParsed{}, err
		}
		e.primary, e.lit = jpLiteral, n
	case p.wordAhead():
		// A name: one of the keywords that start an operand, or else not an operand at all.
		word, err := p.scanWord()
		if err != nil {
			return jpParsed{}, err
		}
		switch {
		case jpKeywordIs(word, "true"):
			e.primary, e.lit = jpLiteral, JsonNode{Kind: JBool, B: true}
		case jpKeywordIs(word, "false"):
			e.primary, e.lit = jpLiteral, JsonNode{Kind: JBool, B: false}
		case jpKeywordIs(word, "null"):
			e.primary, e.lit = jpLiteral, JsonNode{Kind: JNull}
		case jpKeywordIs(word, "last"):
			if p.inSubscript == 0 {
				return jpParsed{}, jpMalformed("LAST is allowed only in array subscripts")
			}
			e.primary = jpLast
		case jpKeywordIs(word, "exists"):
			return jpParsed{}, jpUnsupported("exists predicates")
		default:
			return jpParsed{}, jpMalformed("a name is not an operand")
		}
	default:
		return jpParsed{}, jpMalformed("unexpected token")
	}
	steps, err := p.parseSteps()
	if err != nil {
		return jpParsed{}, err
	}
	e.steps = steps
	return jpParsed{expr: e}, nil
}

// parseNumber parses a number literal (PostgreSQL's jsonpath scanner: an integer, 1.5, 1., .5, an
// exponent) → a Number node. A number running into a letter or digit is trailing junk.
func (p *jpParser) parseNumber() (JsonNode, error) {
	junk := func() (JsonNode, error) { return JsonNode{}, jpMalformed("trailing junk after numeric literal") }
	neg := p.eat('-')
	intStart := p.i
	for p.i < len(p.s) && isASCIIDigit(p.s[p.i]) {
		p.i++
	}
	intPart := string(p.s[intStart:p.i])
	if len(intPart) > 1 && intPart[0] == '0' {
		return junk()
	}
	frac := ""
	if c, ok := p.peek(); ok && c == '.' {
		nc, nok := p.peekAt(1)
		switch {
		case nok && isASCIIDigit(nc):
			p.i++
			start := p.i
			for p.i < len(p.s) && isASCIIDigit(p.s[p.i]) {
				p.i++
			}
			frac = string(p.s[start:p.i])
		case nok && (nc == 'e' || nc == 'E') && intPart != "" && p.exponentAt(2):
			// `1.e3` — an empty fraction, then an exponent.
			p.i++
		case nok && (isAsciiAlpha(nc) || nc == '_'):
			return junk()
		case nok && (nc == '*' || nc == '"' || nc == '[' || nc == '.'):
			// A step follows the point.
		case intPart != "":
			// `1.` — a number with an empty fraction.
			p.i++
		}
	}
	hasExp := false
	var exp int64
	if c, ok := p.peek(); ok && (c == 'e' || c == 'E') {
		p.i++
		eneg := false
		if p.eat('-') {
			eneg = true
		} else {
			p.eat('+')
		}
		start := p.i
		for p.i < len(p.s) && isASCIIDigit(p.s[p.i]) {
			if exp < expLimit {
				exp = min(exp*10+int64(p.s[p.i]-'0'), expLimit)
			}
			p.i++
		}
		if p.i == start {
			return junk()
		}
		if eneg {
			exp = -exp
		}
		hasExp = true
	}
	if c, ok := p.peek(); ok && isIdentByte(c) {
		return junk()
	}
	if intPart == "" && frac == "" {
		return JsonNode{}, jpMalformed("invalid number literal")
	}
	digits, scale := decimalFromParts(intPart, frac, hasExp, exp)
	d, err := decimalFromDigitsScale(neg, digits, scale).CheckCap()
	if err != nil {
		return JsonNode{}, err
	}
	return JsonNode{Kind: JNumber, Num: d}, nil
}

// exponentAt is whether exponent digits (optionally signed) start k bytes past the cursor.
func (p *jpParser) exponentAt(k int) bool {
	c, ok := p.peekAt(k)
	if ok && (c == '+' || c == '-') {
		c, ok = p.peekAt(k + 1)
	}
	return ok && isASCIIDigit(c)
}

// parseSteps parses a sequence of accessor steps (`.key`, `.*`, `[subscripts]`, `[*]`, `?(filter)`,
// `.method(…)`), stopping at the first non-accessor byte (EOF, a comparison/logical operator, `)`,
// etc).
func (p *jpParser) parseSteps() ([]jpStep, error) {
	var steps []jpStep
	for {
		p.skipWs()
		c, ok := p.peek()
		if !ok {
			return steps, nil
		}
		switch c {
		case '.':
			p.i++
			dotEnd := p.i
			p.skipWs()
			nc, ok := p.peek()
			switch {
			case p.eat('*'):
				// `.**` — the any-level accessor — is a deferred follow-on.
				if c, ok := p.peek(); ok && c == '*' {
					return nil, jpUnsupported("the .** accessor")
				}
				steps = append(steps, jpStep{kind: jpWildcardMember})
			case ok && nc == '"':
				k, err := p.parseQuoted()
				if err != nil {
					return nil, err
				}
				steps = append(steps, jpStep{kind: jpMember, key: k})
			case ok && p.i == dotEnd && isASCIIDigit(nc):
				// PostgreSQL lexes `.5` / `.1abc` as a number token, never `.` and a name.
				return nil, jpMalformed("expected a member name after `.`")
			case p.wordAhead():
				// An unquoted name (jsonpath.md §2.1); followed by `(`, an item method.
				name, err := p.scanWord()
				if err != nil {
					return nil, err
				}
				save := p.i
				p.skipWs()
				if pc, ok := p.peek(); ok && pc == '(' {
					m, err := p.parseMethod(name)
					if err != nil {
						return nil, err
					}
					steps = append(steps, jpStep{kind: jpMethodStep, method: m})
				} else {
					p.i = save
					steps = append(steps, jpStep{kind: jpMember, key: name})
				}
			default:
				return nil, jpMalformed("expected a member name after `.`")
			}
		case '[':
			p.i++
			p.skipWs()
			if p.eat('*') {
				p.skipWs()
				if !p.eat(']') {
					return nil, jpMalformed("expected `]` after `[*`")
				}
				steps = append(steps, jpStep{kind: jpWildcardElement})
			} else {
				subs, err := p.parseSubscripts()
				if err != nil {
					return nil, err
				}
				steps = append(steps, jpStep{kind: jpSubscripts, subs: subs})
			}
		case '?':
			p.i++
			p.skipWs()
			if !p.eat('(') {
				return nil, jpMalformed("expected `(` after `?`")
			}
			if err := p.deepen(); err != nil {
				return nil, err
			}
			p.inFilter++
			pred, err := needPred(p.parseOr())
			if err != nil {
				return nil, err
			}
			p.inFilter--
			p.depth--
			p.skipWs()
			if !p.eat(')') {
				return nil, jpMalformed("expected `)` after a filter predicate")
			}
			steps = append(steps, jpStep{kind: jpFilter, pred: pred})
		default:
			return steps, nil
		}
	}
}

// parseMethod parses an item method's argument list (the cursor is at its `(`). Method names are
// matched case-insensitively, like PostgreSQL's keywords.
func (p *jpParser) parseMethod(name string) (jpMethod, error) {
	p.i++ // (
	p.skipWs()
	lower := toLowerASCII(name)
	var m jpMethod
	switch lower {
	case "datetime", "date", "time", "time_tz", "timestamp", "timestamp_tz":
		return jpMethod{}, jpUnsupported("item method ." + lower + "()")
	case "decimal":
		var args []Decimal
		if !p.eat(')') {
			for {
				d, err := p.parseIntArg()
				if err != nil {
					return jpMethod{}, err
				}
				args = append(args, d)
				p.skipWs()
				if p.eat(')') {
					break
				}
				if !p.eat(',') {
					return jpMethod{}, jpMalformed("expected `,` or `)` in .decimal() arguments")
				}
			}
		}
		if len(args) > 2 {
			return jpMethod{}, jpMalformed(".decimal() can only have an optional precision[,scale]")
		}
		m.kind = jmDecimal
		if len(args) > 0 {
			m.precision = &args[0]
		}
		if len(args) > 1 {
			m.scale = &args[1]
		}
		return m, nil
	}
	found := false
	for k, n := range jpMethodNames {
		if n == lower && jpMethodKind(k) != jmDecimal {
			m.kind, found = jpMethodKind(k), true
		}
	}
	if !found {
		return jpMethod{}, jpMalformed("unknown item method ." + name + "()")
	}
	if !p.eat(')') {
		return jpMethod{}, jpMalformed("item method ." + m.name() + "() takes no arguments")
	}
	return m, nil
}

// parseIntArg parses a .decimal() argument: an optionally signed integer literal.
func (p *jpParser) parseIntArg() (Decimal, error) {
	p.skipWs()
	neg := false
	if p.eat('-') {
		neg = true
	} else {
		p.eat('+')
	}
	p.skipWs()
	start := p.i
	for p.i < len(p.s) && isASCIIDigit(p.s[p.i]) {
		p.i++
	}
	digits := string(p.s[start:p.i])
	if c, ok := p.peek(); digits == "" || (len(digits) > 1 && digits[0] == '0') || (ok && (isIdentByte(c) || c == '.')) {
		return Decimal{}, jpMalformed("expected an integer .decimal() argument")
	}
	return decimalFromDigitsScale(neg, digits, 0), nil
}

// parseOr parses an expression or predicate: `||` over `&&` over `!` / comparison over arithmetic
// (PostgreSQL's jsonpath_gram.y precedence: `||`, `&&`, `!`, comparisons, `+ -`, `* / %`, unary
// `+ -`, accessors). `||` and `&&` take predicates.
func (p *jpParser) parseOr() (jpParsed, error) {
	left, err := p.parseAnd()
	if err != nil {
		return jpParsed{}, err
	}
	// Each `||` step deepens the left-deep tree by one level (json.md §6.4).
	steps := 0
	for {
		p.skipWs()
		if !p.eatOp("||") {
			p.depth -= steps
			return left, nil
		}
		if err := p.deepen(); err != nil {
			return jpParsed{}, err
		}
		steps++
		l, err := needPred(left, nil)
		if err != nil {
			return jpParsed{}, err
		}
		r, err := needPred(p.parseAnd())
		if err != nil {
			return jpParsed{}, err
		}
		left = jpParsed{pred: &jpPred{kind: jpPredOr, left: l, right: r}}
	}
}

func (p *jpParser) parseAnd() (jpParsed, error) {
	left, err := p.parseNot()
	if err != nil {
		return jpParsed{}, err
	}
	// Each `&&` step deepens the left-deep tree by one level (json.md §6.4).
	steps := 0
	for {
		p.skipWs()
		if !p.eatOp("&&") {
			p.depth -= steps
			return left, nil
		}
		if err := p.deepen(); err != nil {
			return jpParsed{}, err
		}
		steps++
		l, err := needPred(left, nil)
		if err != nil {
			return jpParsed{}, err
		}
		r, err := needPred(p.parseNot())
		if err != nil {
			return jpParsed{}, err
		}
		left = jpParsed{pred: &jpPred{kind: jpPredAnd, left: l, right: r}}
	}
}

// parseNot parses `!(predicate)` — the operand must be a parenthesized predicate.
func (p *jpParser) parseNot() (jpParsed, error) {
	p.skipWs()
	if c, ok := p.peek(); ok && c == '!' {
		if nc, ok := p.peekAt(1); !ok || nc != '=' {
			p.i++
			p.skipWs()
			if p.eatKeyword("exists") {
				return jpParsed{}, jpUnsupported("exists predicates")
			}
			if !p.eat('(') {
				return jpParsed{}, jpMalformed("expected `(` after `!`")
			}
			if err := p.deepen(); err != nil {
				return jpParsed{}, err
			}
			inner, err := needPred(p.parseOr())
			if err != nil {
				return jpParsed{}, err
			}
			p.depth--
			p.skipWs()
			if !p.eat(')') {
				return jpParsed{}, jpMalformed("expected `)` after `!(`")
			}
			return jpParsed{pred: &jpPred{kind: jpPredNot, left: inner}}, nil
		}
	}
	return p.parseComparison()
}

// parseComparison parses `expr cmp expr`, or a lone expression or parenthesized predicate. The word
// predicates (`like_regex`, `starts with`, `is unknown`) are a deferred follow-on.
func (p *jpParser) parseComparison() (jpParsed, error) {
	left, err := p.parseAdditive()
	if err != nil {
		return jpParsed{}, err
	}
	if left.pred != nil {
		p.skipWs()
		if p.wordAhead() {
			start := p.i
			word, err := p.scanWord()
			if err != nil {
				return jpParsed{}, err
			}
			if jpKeywordIs(word, "is") {
				return jpParsed{}, jpUnsupported("is unknown predicates")
			}
			p.i = start
		}
		return left, nil
	}
	p.skipWs()
	var op jpCmpOp
	switch {
	case p.eatOp("=="):
		op = jpCmpEq
	case p.eatOp("!=") || p.eatOp("<>"):
		op = jpCmpNe
	case p.eatOp("<="):
		op = jpCmpLe
	case p.eatOp(">="):
		op = jpCmpGe
	case p.eat('<'):
		op = jpCmpLt
	case p.eat('>'):
		op = jpCmpGt
	default:
		// A word predicate operator is a deferred follow-on. The name is lexed first, so its escape
		// error wins; any other name is trailing input.
		if p.wordAhead() {
			start := p.i
			word, err := p.scanWord()
			if err != nil {
				return jpParsed{}, err
			}
			if jpKeywordIs(word, "like_regex") || jpKeywordIs(word, "starts") {
				return jpParsed{}, jpUnsupported("predicates other than a comparison (like_regex / starts with)")
			}
			p.i = start
		}
		return left, nil
	}
	right, err := needExpr(p.parseAdditive())
	if err != nil {
		return jpParsed{}, err
	}
	return jpParsed{pred: &jpPred{kind: jpPredCompare, cmpLeft: *left.expr, cmpOp: op, cmpRight: *right}}, nil
}

// parseAdditive parses `+` and `-`, left-associative.
func (p *jpParser) parseAdditive() (jpParsed, error) {
	left, err := p.parseMultiplicative()
	if err != nil {
		return jpParsed{}, err
	}
	// Each operator deepens the left-deep tree by one level (json.md §6.4).
	steps := 0
	for {
		p.skipWs()
		c, ok := p.peek()
		var op jpArithOp
		switch {
		case ok && c == '+':
			op = jpAdd
		case ok && c == '-':
			op = jpSub
		default:
			p.depth -= steps
			return left, nil
		}
		p.i++
		if err := p.deepen(); err != nil {
			return jpParsed{}, err
		}
		steps++
		l, err := needExpr(left, nil)
		if err != nil {
			return jpParsed{}, err
		}
		r, err := needExpr(p.parseMultiplicative())
		if err != nil {
			return jpParsed{}, err
		}
		left = jpParsed{expr: &jpPathExpr{primary: jpBinary, arithOp: op, left: l, right: r}}
	}
}

// parseMultiplicative parses `*`, `/`, and `%`, left-associative.
func (p *jpParser) parseMultiplicative() (jpParsed, error) {
	left, err := p.parseUnary()
	if err != nil {
		return jpParsed{}, err
	}
	steps := 0
	for {
		p.skipWs()
		c, ok := p.peek()
		var op jpArithOp
		switch {
		case ok && c == '*':
			op = jpMul
		case ok && c == '/':
			op = jpDiv
		case ok && c == '%':
			op = jpMod
		default:
			p.depth -= steps
			return left, nil
		}
		p.i++
		if err := p.deepen(); err != nil {
			return jpParsed{}, err
		}
		steps++
		l, err := needExpr(left, nil)
		if err != nil {
			return jpParsed{}, err
		}
		r, err := needExpr(p.parseUnary())
		if err != nil {
			return jpParsed{}, err
		}
		left = jpParsed{expr: &jpPathExpr{primary: jpBinary, arithOp: op, left: l, right: r}}
	}
}

// parseUnary parses unary `+` / `-`, which bind tighter than the binary operators but looser than
// accessors (`-$.a * 2` is `(-($.a)) * 2`). Applied to a number literal they fold into it (`-1`), as
// PostgreSQL's grammar does.
func (p *jpParser) parseUnary() (jpParsed, error) {
	p.skipWs()
	c, ok := p.peek()
	var op jpUnaryOp
	switch {
	case ok && c == '+':
		op = jpUnaryPlus
	case ok && c == '-':
		op = jpUnaryMinus
	default:
		return p.parsePostfix()
	}
	p.i++
	if err := p.deepen(); err != nil {
		return jpParsed{}, err
	}
	arg, err := needExpr(p.parseUnary())
	if err != nil {
		return jpParsed{}, err
	}
	p.depth--
	if arg.primary == jpLiteral && arg.lit.Kind == JNumber && len(arg.steps) == 0 {
		if op == jpUnaryMinus {
			arg.lit.Num = arg.lit.Num.Negate()
		}
		return jpParsed{expr: arg}, nil
	}
	return jpParsed{expr: &jpPathExpr{primary: jpUnary, unaryOp: op, left: arg}}, nil
}

// eatOp consumes a multi-byte operator token if it appears at the cursor.
func (p *jpParser) eatOp(op string) bool {
	if p.i+len(op) <= len(p.s) && string(p.s[p.i:p.i+len(op)]) == op {
		p.i += len(op)
		return true
	}
	return false
}
