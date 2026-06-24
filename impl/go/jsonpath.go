package jed

import (
	"strconv"
	"strings"
	"unicode/utf8"
)

// The jsonpath type's compiler + canonical renderer (spec/design/jsonpath.md, slice P1a).
//
// P1a is the LITERAL-ONLY surface (like J0 for json): the jsonpath scalar type, the
// '…'::jsonpath / jsonpath '…' literal cast (compiled at resolve), and the canonical render
// ($.a → $."a", lax omitted, strict kept). The structural-accessor subset is parsed here
// ($, .key, .*, [subscripts], [*], numeric / last indices, to slices, lax/strict mode);
// the eval engine, filters, item methods, arithmetic, like_regex, and $name variables are a
// deferred P1b follow-on (a valid-PG path using one → 0A000 at compile). A malformed path is
// 42601. The compiled program is a pure function of the source — kept byte-identical cross-core
// by the conformance suite (CLAUDE.md §5: a hand-written parser, never codegenned).

// JsonPath is a compiled jsonpath (the structural-accessor subset, P1a).
type JsonPath struct {
	Strict bool
	Steps  []jpStep
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
}

// jpPredKind tags a filter predicate (jsonpath.md §4, the P1b comparison subset). 3-valued —
// Not/And/Or follow SQL/JSON's Kleene logic, but a filter keeps an item only when the predicate is
// definitely TRUE.
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
	cmpLeft, cmpRight jpFiltExpr
	cmpOp             jpCmpOp
}

// jpFiltExprKind tags a comparison operand inside a filter: a `@`/`$`-rooted accessor path, or a
// scalar literal.
type jpFiltExprKind int

const (
	// jpFiltPath is a `@`-rooted (fromRoot == false) or `$`-rooted (true) accessor path.
	jpFiltPath jpFiltExprKind = iota
	// jpFiltLit is a scalar literal — a JSON number / string / boolean / null.
	jpFiltLit
)

// jpFiltExpr is a comparison operand inside a filter.
type jpFiltExpr struct {
	kind jpFiltExprKind
	// fromRoot/steps hold the accessor path when kind == jpFiltPath.
	fromRoot bool
	steps    []jpStep
	// lit holds the literal node when kind == jpFiltLit.
	lit JsonNode
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

// jpIndex is a subscript index: a non-negative integer literal or the `last` sentinel.
type jpIndex struct {
	// last is true for the `last` sentinel; otherwise number is the integer value.
	last   bool
	number int64
}

// jpUnsupported is a jsonpath construct that is valid in PostgreSQL but not yet supported by jed (a
// deferred P1b follow-on): 0A000, a documented divergence.
func jpUnsupported(what string) *EngineError {
	return NewError(FeatureNotSupported, "jsonpath "+what+" is not supported yet")
}

// jpMalformed is a malformed jsonpath literal: 42601 (PostgreSQL's syntax-error class for a bad
// path literal).
func jpMalformed(detail string) *EngineError {
	return NewError(SyntaxError, "invalid jsonpath: "+detail)
}

// Compile compiles a jsonpath source string (P1a structural subset). Malformed → 42601; a valid-PG
// but unsupported construct → 0A000.
func Compile(src string) (JsonPath, error) {
	p := &jpParser{s: []byte(src)}
	p.skipWs()
	// Optional mode word: strict / lax (default lax).
	strict := false
	if p.eatKeyword("strict") {
		strict = true
	} else {
		p.eatKeyword("lax")
	}
	p.skipWs()
	if !p.eat('$') {
		// @, a variable, or a bare literal as a top-level path expression — the filter / scalar
		// path-expression surface (a P1b follow-on).
		return JsonPath{}, jpUnsupported("expressions other than a `$`-rooted accessor path")
	}
	// $name — a path variable (the $ immediately followed by a name char / quote) is a P1b
	// follow-on (the bound-variable vars surface).
	if c, ok := p.peek(); ok && (isMemberStart(c) || c == '"') {
		return JsonPath{}, jpUnsupported("path variables `$name`")
	}
	steps, err := p.parseSteps()
	if err != nil {
		return JsonPath{}, err
	}
	p.skipWs()
	// After the accessor path, anything left is a TOP-LEVEL predicate operator (`$.a == 1`, for
	// jsonb_path_match / @@) or arithmetic — both a P1b follow-on this slice.
	if c, ok := p.peek(); ok {
		switch c {
		case '=', '<', '>', '!', '&', '|':
			return JsonPath{}, jpUnsupported("top-level predicate expressions")
		case '+', '-', '*', '/', '%':
			return JsonPath{}, jpUnsupported("path arithmetic")
		default:
			return JsonPath{}, jpMalformed("unexpected trailing input in path")
		}
	}
	// `$` alone is valid (the root document) — steps is empty in that case.
	return JsonPath{Strict: strict, Steps: steps}, nil
}

// Render is the canonical render (spec/design/jsonpath.md §2): strict kept / lax omitted; member
// keys quoted; [*], [i], [i to j] subscripts; matches PostgreSQL's jsonpath_out.
func (jp JsonPath) Render() string {
	var out strings.Builder
	if jp.Strict {
		out.WriteString("strict ")
	}
	out.WriteByte('$')
	writeSteps(jp.Steps, &out)
	return out.String()
}

// writeSteps renders an accessor-step sequence (shared by the path render and a filter's `@`/`$`
// operand).
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
			writePred(step.pred, out)
			out.WriteByte(')')
		}
	}
}

// writePred renders a filter predicate (PG's `?(…)` form: `&&`/`||` spaced, `!(…)`, `a op b` spaced).
func writePred(pred *jpPred, out *strings.Builder) {
	switch pred.kind {
	case jpPredOr:
		writePred(pred.left, out)
		out.WriteString(" || ")
		writePred(pred.right, out)
	case jpPredAnd:
		writePred(pred.left, out)
		out.WriteString(" && ")
		writePred(pred.right, out)
	case jpPredNot:
		out.WriteString("!(")
		writePred(pred.left, out)
		out.WriteByte(')')
	default: // jpPredCompare
		writeFiltExpr(&pred.cmpLeft, out)
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
		writeFiltExpr(&pred.cmpRight, out)
	}
}

func writeFiltExpr(e *jpFiltExpr, out *strings.Builder) {
	if e.kind == jpFiltPath {
		if e.fromRoot {
			out.WriteByte('$')
		} else {
			out.WriteByte('@')
		}
		writeSteps(e.steps, out)
		return
	}
	out.WriteString(jsonCompactOut(&e.lit))
}

// ---------------------------------------------------------------------------------------------
// Evaluation (jsonpath.md §3-4) — the lax/strict ordered jsonb-item sequence (P1b structural subset).
// ---------------------------------------------------------------------------------------------

// Eval evaluates a compiled path over a jsonb context item → the ordered SQL/JSON sequence
// (jsonpath.md §3). Each accessor is a `seq → seq` map applied left to right. lax (the default)
// auto-unwraps arrays (§4.1) and suppresses structural navigation failures (§4.2); strict raises.
// The P1b structural subset (no filters / item methods / arithmetic — those are still 0A000 at
// compile).
func (jp JsonPath) Eval(ctx JsonNode) ([]JsonNode, error) {
	return evalSteps(jp.Steps, &ctx, &ctx, jp.Strict)
}

// evalSteps evaluates an accessor-step sequence over a seed item, with root as the document `$` (for
// a filter's `$`-rooted operand).
func evalSteps(steps []jpStep, seed, root *JsonNode, strict bool) ([]JsonNode, error) {
	seq := []JsonNode{*seed}
	for i := range steps {
		var next []JsonNode
		for j := range seq {
			var err error
			next, err = applyStep(&steps[i], &seq[j], strict, root, next)
			if err != nil {
				return nil, err
			}
		}
		seq = next
	}
	return seq, nil
}

func applyStep(step *jpStep, item *JsonNode, strict bool, root *JsonNode, out []JsonNode) ([]JsonNode, error) {
	switch step.kind {
	case jpMember:
		// lax: a member accessor on an array unwraps it ONE level first (§4.1.1).
		if !strict && item.Kind == JArray {
			for k := range item.Arr {
				var err error
				out, err = memberAccess(&item.Arr[k], step.key, strict, out)
				if err != nil {
					return nil, err
				}
			}
			return out, nil
		}
		return memberAccess(item, step.key, strict, out)
	case jpWildcardMember:
		if !strict && item.Kind == JArray {
			for k := range item.Arr {
				var err error
				out, err = wildcardMember(&item.Arr[k], strict, out)
				if err != nil {
					return nil, err
				}
			}
			return out, nil
		}
		return wildcardMember(item, strict, out)
	case jpSubscripts:
		// [i] on a non-array: lax treats the item as a singleton array (§4.1.2); strict raises.
		var elems []JsonNode
		if item.Kind == JArray {
			elems = item.Arr
		} else if !strict {
			elems = []JsonNode{*item}
		} else {
			return nil, NewError(InvalidSqlJsonSubscript,
				"jsonpath array accessor can only be applied to an array")
		}
		for i := range step.subs {
			var err error
			out, err = subscript(elems, &step.subs[i], strict, out)
			if err != nil {
				return nil, err
			}
		}
		return out, nil
	case jpWildcardElement:
		// [*] on a non-array: lax → the singleton item; strict raises.
		if item.Kind == JArray {
			return append(out, item.Arr...), nil
		}
		if !strict {
			return append(out, *item), nil
		}
		return nil, NewError(InvalidSqlJsonSubscript,
			"jsonpath wildcard array accessor can only be applied to an array")
	default: // jpFilter
		// `?(predicate)` — keep the current item when the predicate is definitely TRUE (§4). The
		// predicate's `@` is the item, `$` is the document root.
		ok, err := evalPred(step.pred, item, root, strict)
		if err != nil {
			return nil, err
		}
		if ok != nil && *ok {
			return append(out, *item), nil
		}
		return out, nil
	}
}

// evalPred evaluates a filter predicate to a Kleene truth value (a *bool: &true / &false / nil =
// unknown — mirroring the Rust Option<bool>).
func evalPred(pred *jpPred, current, root *JsonNode, strict bool) (*bool, error) {
	switch pred.kind {
	case jpPredOr:
		x, err := evalPred(pred.left, current, root, strict)
		if err != nil {
			return nil, err
		}
		y, err := evalPred(pred.right, current, root, strict)
		if err != nil {
			return nil, err
		}
		switch {
		case isTrue(x) || isTrue(y):
			return boolPtr(true), nil
		case isFalse(x) && isFalse(y):
			return boolPtr(false), nil
		default:
			return nil, nil
		}
	case jpPredAnd:
		x, err := evalPred(pred.left, current, root, strict)
		if err != nil {
			return nil, err
		}
		y, err := evalPred(pred.right, current, root, strict)
		if err != nil {
			return nil, err
		}
		switch {
		case isFalse(x) || isFalse(y):
			return boolPtr(false), nil
		case isTrue(x) && isTrue(y):
			return boolPtr(true), nil
		default:
			return nil, nil
		}
	case jpPredNot:
		v, err := evalPred(pred.left, current, root, strict)
		if err != nil {
			return nil, err
		}
		if v == nil {
			return nil, nil
		}
		return boolPtr(!*v), nil
	default: // jpPredCompare
		return evalCompare(&pred.cmpLeft, pred.cmpOp, &pred.cmpRight, current, root, strict)
	}
}

func isTrue(b *bool) bool  { return b != nil && *b }
func isFalse(b *bool) bool { return b != nil && !*b }

func boolPtr(b bool) *bool { return &b }

// evalCompare is the existential comparison (§4): true if SOME pair (a in lhs-seq, b in rhs-seq)
// compares true. An empty operand or all-incomparable pairs → nil (unknown); else &false.
func evalCompare(l *jpFiltExpr, op jpCmpOp, r *jpFiltExpr, current, root *JsonNode, strict bool) (*bool, error) {
	ls := evalFiltExpr(l, current, root, strict)
	rs := evalFiltExpr(r, current, root, strict)
	if len(ls) == 0 || len(rs) == 0 {
		return nil, nil
	}
	anyUnknown := false
	for i := range ls {
		for j := range rs {
			c := compareNodes(&ls[i], op, &rs[j])
			switch {
			case c == nil:
				anyUnknown = true
			case *c:
				return boolPtr(true), nil
			}
		}
	}
	if anyUnknown {
		return nil, nil
	}
	return boolPtr(false), nil
}

// evalFiltExpr evaluates a filter operand to its jsonb-item sequence (a `@`/`$` path) or a singleton
// literal.
func evalFiltExpr(e *jpFiltExpr, current, root *JsonNode, strict bool) []JsonNode {
	if e.kind == jpFiltLit {
		return []JsonNode{e.lit}
	}
	seed := current
	if e.fromRoot {
		seed = root
	}
	// A navigation error inside a filter operand → no items (the comparison is just unknown),
	// never propagated (§4.2: filter operands never raise, even in strict).
	seq, err := evalSteps(e.steps, seed, root, strict)
	if err != nil {
		return nil
	}
	return seq
}

// compareNodes compares two jsonb scalars under a jsonpath operator (a *bool: &v / nil = unknown).
// Only same-type number/string compare by order; booleans/nulls compare only by `==`/`!=`; any other
// (mixed-type) pair is nil (unknown).
func compareNodes(a *JsonNode, op jpCmpOp, b *JsonNode) *bool {
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
	default:
		return nil // mixed types are not comparable
	}
	// Booleans / nulls support only equality; ordering on them is unknown.
	orderOK := a.Kind == JNumber || a.Kind == JString
	switch op {
	case jpCmpEq:
		return boolPtr(ord == 0)
	case jpCmpNe:
		return boolPtr(ord != 0)
	case jpCmpLt:
		if orderOK {
			return boolPtr(ord < 0)
		}
	case jpCmpLe:
		if orderOK {
			return boolPtr(ord <= 0)
		}
	case jpCmpGt:
		if orderOK {
			return boolPtr(ord > 0)
		}
	case jpCmpGe:
		if orderOK {
			return boolPtr(ord >= 0)
		}
	}
	return nil // an order comparison on bool/null is unknown
}

func memberAccess(item *JsonNode, key string, strict bool, out []JsonNode) ([]JsonNode, error) {
	if item.Kind == JObject {
		for i := range item.Obj {
			if item.Obj[i].Key == key {
				return append(out, item.Obj[i].Val), nil
			}
		}
		if strict {
			return nil, NewError(SqlJsonItemCannotBeCastToTargetType,
				"JSON object does not contain key \""+key+"\"")
		}
		// lax: a missing member contributes no item (§4.2 rule 5).
		return out, nil
	}
	if strict {
		return nil, NewError(SqlJsonObjectNotFound,
			"jsonpath member accessor can only be applied to an object")
	}
	// lax: a member accessor on a non-object/non-array contributes no item.
	return out, nil
}

func wildcardMember(item *JsonNode, strict bool, out []JsonNode) ([]JsonNode, error) {
	if item.Kind == JObject {
		for i := range item.Obj {
			out = append(out, item.Obj[i].Val)
		}
		return out, nil
	}
	if strict {
		return nil, NewError(SqlJsonObjectNotFound,
			"jsonpath wildcard member accessor can only be applied to an object")
	}
	return out, nil
}

func subscript(elems []JsonNode, sub *jpSubscript, strict bool, out []JsonNode) ([]JsonNode, error) {
	length := int64(len(elems))
	resolve := func(idx jpIndex) int64 {
		if idx.last {
			return length - 1
		}
		return idx.number
	}
	if sub.slice {
		from := resolve(sub.a)
		if from < 0 {
			from = 0
		}
		to := resolve(sub.b)
		if to > length-1 {
			to = length - 1
		}
		for i := from; i <= to; i++ {
			out = append(out, elems[i])
		}
		return out, nil
	}
	i := resolve(sub.a)
	if i >= 0 && i < length {
		out = append(out, elems[i])
	} else if strict {
		return nil, NewError(InvalidSqlJsonSubscript,
			"jsonpath array subscript is out of bounds")
	}
	// lax: an out-of-range subscript contributes no item.
	return out, nil
}

func writeIndex(i jpIndex, out *strings.Builder) {
	if i.last {
		out.WriteString("last")
	} else {
		out.WriteString(strconv.FormatInt(i.number, 10))
	}
}

// writeQuoted renders a member key as a canonical jsonpath quoted string ("…" with JSON escaping).
func writeQuoted(k string, out *strings.Builder) {
	out.WriteByte('"')
	for _, c := range k {
		switch c {
		case '"':
			out.WriteString("\\\"")
		case '\\':
			out.WriteString("\\\\")
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

func isMemberStart(c byte) bool {
	return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || c == '_'
}

func isMemberCont(c byte) bool {
	return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
		c == '_' || c == '$'
}

type jpParser struct {
	s []byte
	i int
}

func (p *jpParser) peek() (byte, bool) {
	if p.i < len(p.s) {
		return p.s[p.i], true
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

func (p *jpParser) skipWs() {
	for p.i < len(p.s) && isAsciiWhitespace(p.s[p.i]) {
		p.i++
	}
}

func isAsciiWhitespace(c byte) bool {
	return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\f' || c == '\v'
}

// eatKeyword consumes kw if it appears as a whole WORD at the cursor — i.e. the following byte is
// not an identifier-continuation character (so `last]`, `to `, `strict $` all match, but `lastfoo`
// does not).
func (p *jpParser) eatKeyword(kw string) bool {
	kb := []byte(kw)
	if p.i+len(kb) <= len(p.s) && string(p.s[p.i:p.i+len(kb)]) == kw {
		after := p.i + len(kb)
		if after >= len(p.s) {
			p.i += len(kb)
			return true
		}
		c := p.s[after]
		if !((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '_') {
			p.i += len(kb)
			return true
		}
	}
	return false
}

// parseMember parses a member key after `.`: a bare identifier or a "…" quoted string.
func (p *jpParser) parseMember() (string, error) {
	if c, ok := p.peek(); ok && c == '"' {
		return p.parseQuoted()
	}
	start := p.i
	for {
		c, ok := p.peek()
		if !ok || !isMemberCont(c) {
			break
		}
		p.i++
	}
	if p.i == start {
		return "", jpMalformed("empty member name")
	}
	return string(p.s[start:p.i]), nil
}

// parseQuoted parses a "…" jsonpath string (JSON escapes).
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
			p.i++
			ec, ok := p.peek()
			if !ok {
				return "", jpMalformed("invalid escape")
			}
			switch ec {
			case '"':
				out.WriteByte('"')
			case '\\':
				out.WriteByte('\\')
			case '/':
				out.WriteByte('/')
			case 'n':
				out.WriteByte('\n')
			case 'r':
				out.WriteByte('\r')
			case 't':
				out.WriteByte('\t')
			case 'b':
				out.WriteByte('\b')
			case 'f':
				out.WriteByte('\f')
			case 'u':
				if p.i+5 > len(p.s) {
					return "", jpMalformed("invalid \\u escape")
				}
				hex := string(p.s[p.i+1 : p.i+5])
				cp, err := strconv.ParseUint(hex, 16, 32)
				if err != nil {
					return "", jpMalformed("invalid \\u escape")
				}
				r := rune(cp)
				if !utf8.ValidRune(r) {
					return "", jpMalformed("invalid \\u escape")
				}
				out.WriteRune(r)
				p.i += 4
			default:
				return "", jpMalformed("invalid escape")
			}
			p.i++
		default:
			// Copy one UTF-8 char.
			start := p.i
			n := utf8Len(p.s[p.i])
			if p.i+n > len(p.s) {
				n = len(p.s) - p.i
			}
			p.i += n
			out.Write(p.s[start:p.i])
		}
	}
}

// parseSubscripts parses a [...] subscript list (the opening `[` consumed, not the wildcard form).
// Each subscript is `index` or `index to index`; `index` is a number or `last`. Anything else →
// 0A000.
func (p *jpParser) parseSubscripts() ([]jpSubscript, error) {
	var subs []jpSubscript
	for {
		p.skipWs()
		a, err := p.parseIndex()
		if err != nil {
			return nil, err
		}
		p.skipWs()
		var sub jpSubscript
		if p.eatKeyword("to") {
			p.skipWs()
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
			return subs, nil
		default:
			return nil, jpMalformed("expected `,` or `]` in subscript")
		}
	}
}

// parseSteps parses a sequence of accessor steps (`.key`, `.*`, `[subscripts]`, `[*]`, `?(filter)`),
// stopping at the first non-accessor byte (EOF, a comparison/logical operator, `)`, etc).
func (p *jpParser) parseSteps() ([]jpStep, error) {
	var steps []jpStep
	for {
		p.skipWs()
		c, ok := p.peek()
		if !ok {
			break
		}
		switch c {
		case '.':
			p.i++
			if p.eat('*') {
				steps = append(steps, jpStep{kind: jpWildcardMember})
			} else if nc, ok := p.peek(); ok && (nc == '"' || isMemberStart(nc)) {
				m, err := p.parseMember()
				if err != nil {
					return nil, err
				}
				// `.identifier(` is an item-method call (a P1b follow-on); a bare identifier
				// is a member accessor.
				if pc, ok := p.peek(); ok && pc == '(' {
					return nil, jpUnsupported("item methods")
				}
				steps = append(steps, jpStep{kind: jpMember, key: m})
			} else {
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
			pred, err := p.parsePred()
			if err != nil {
				return nil, err
			}
			p.skipWs()
			if !p.eat(')') {
				return nil, jpMalformed("expected `)` after a filter predicate")
			}
			steps = append(steps, jpStep{kind: jpFilter, pred: pred})
		default:
			return steps, nil
		}
	}
	return steps, nil
}

// parsePred parses a filter predicate (P1b comparison subset): `||` over `&&` over `!` / `(…)` /
// comparison.
func (p *jpParser) parsePred() (*jpPred, error) {
	left, err := p.parseAnd()
	if err != nil {
		return nil, err
	}
	for {
		p.skipWs()
		if p.eatOp("||") {
			right, err := p.parseAnd()
			if err != nil {
				return nil, err
			}
			left = &jpPred{kind: jpPredOr, left: left, right: right}
		} else {
			return left, nil
		}
	}
}

func (p *jpParser) parseAnd() (*jpPred, error) {
	left, err := p.parseNot()
	if err != nil {
		return nil, err
	}
	for {
		p.skipWs()
		if p.eatOp("&&") {
			right, err := p.parseNot()
			if err != nil {
				return nil, err
			}
			left = &jpPred{kind: jpPredAnd, left: left, right: right}
		} else {
			return left, nil
		}
	}
}

func (p *jpParser) parseNot() (*jpPred, error) {
	p.skipWs()
	if p.eat('!') {
		p.skipWs()
		if !p.eat('(') {
			return nil, jpMalformed("expected `(` after `!`")
		}
		inner, err := p.parsePred()
		if err != nil {
			return nil, err
		}
		p.skipWs()
		if !p.eat(')') {
			return nil, jpMalformed("expected `)` after `!(`")
		}
		return &jpPred{kind: jpPredNot, left: inner}, nil
	}
	if c, ok := p.peek(); ok && c == '(' {
		p.i++
		inner, err := p.parsePred()
		if err != nil {
			return nil, err
		}
		p.skipWs()
		if !p.eat(')') {
			return nil, jpMalformed("expected `)` in predicate")
		}
		return inner, nil
	}
	return p.parseComparison()
}

// parseComparison parses `filter_expr cmp filter_expr` — the only leaf predicate this slice (`exists`
// / `like_regex` / `starts with` / `is unknown` are a follow-on).
func (p *jpParser) parseComparison() (*jpPred, error) {
	left, err := p.parseFilterExpr()
	if err != nil {
		return nil, err
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
		return nil, jpUnsupported(
			"filter predicates other than a comparison (exists / like_regex / starts with)",
		)
	}
	right, err := p.parseFilterExpr()
	if err != nil {
		return nil, err
	}
	return &jpPred{kind: jpPredCompare, cmpLeft: left, cmpOp: op, cmpRight: right}, nil
}

// parseFilterExpr parses a comparison operand: a `@`/`$`-rooted accessor path, or a scalar literal.
func (p *jpParser) parseFilterExpr() (jpFiltExpr, error) {
	p.skipWs()
	c, ok := p.peek()
	if !ok {
		return jpFiltExpr{}, jpMalformed("expected a comparison operand")
	}
	switch {
	case c == '@':
		p.i++
		steps, err := p.parseSteps()
		if err != nil {
			return jpFiltExpr{}, err
		}
		return jpFiltExpr{kind: jpFiltPath, fromRoot: false, steps: steps}, nil
	case c == '$':
		p.i++
		if nc, ok := p.peek(); ok && (isMemberStart(nc) || nc == '"') {
			return jpFiltExpr{}, jpUnsupported("path variables `$name`")
		}
		steps, err := p.parseSteps()
		if err != nil {
			return jpFiltExpr{}, err
		}
		return jpFiltExpr{kind: jpFiltPath, fromRoot: true, steps: steps}, nil
	case c == '"':
		s, err := p.parseQuoted()
		if err != nil {
			return jpFiltExpr{}, err
		}
		return jpFiltExpr{kind: jpFiltLit, lit: JsonNode{Kind: JString, S: s}}, nil
	case (c >= '0' && c <= '9') || c == '-':
		n, err := p.parseNumber()
		if err != nil {
			return jpFiltExpr{}, err
		}
		return jpFiltExpr{kind: jpFiltLit, lit: n}, nil
	default:
		switch {
		case p.eatKeyword("true"):
			return jpFiltExpr{kind: jpFiltLit, lit: JsonNode{Kind: JBool, B: true}}, nil
		case p.eatKeyword("false"):
			return jpFiltExpr{kind: jpFiltLit, lit: JsonNode{Kind: JBool, B: false}}, nil
		case p.eatKeyword("null"):
			return jpFiltExpr{kind: jpFiltLit, lit: JsonNode{Kind: JNull}}, nil
		default:
			return jpFiltExpr{}, jpMalformed("expected a comparison operand")
		}
	}
}

// parseNumber parses a JSON number literal in a filter (integer or decimal) → a Number node. Reuses
// the json number parser (a bare number is valid JSON).
func (p *jpParser) parseNumber() (JsonNode, error) {
	start := p.i
	if c, ok := p.peek(); ok && c == '-' {
		p.i++
	}
	for {
		c, ok := p.peek()
		if !ok {
			break
		}
		if (c >= '0' && c <= '9') || c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-' {
			p.i++
		} else {
			break
		}
	}
	text := string(p.s[start:p.i])
	n, err := jsonbIn(text)
	if err != nil || n.Kind != JNumber {
		return JsonNode{}, jpMalformed("invalid number literal")
	}
	return n, nil
}

// eatOp consumes a multi-byte operator token if it appears at the cursor.
func (p *jpParser) eatOp(op string) bool {
	if p.i+len(op) <= len(p.s) && string(p.s[p.i:p.i+len(op)]) == op {
		p.i += len(op)
		return true
	}
	return false
}

func (p *jpParser) parseIndex() (jpIndex, error) {
	if p.eatKeyword("last") {
		return jpIndex{last: true}, nil
	}
	c, ok := p.peek()
	switch {
	case !ok:
		// A truncated path (no index where one is required) is malformed.
		return jpIndex{}, jpMalformed("expected a subscript index")
	case !((c >= '0' && c <= '9') || c == '-'):
		// A non-numeric token starts an expression subscript ($.a, arithmetic) — a P1b follow-on.
		return jpIndex{}, jpUnsupported("non-literal subscript expressions")
	}
	start := p.i
	if pc, ok := p.peek(); ok && pc == '-' {
		p.i++
	}
	for {
		dc, ok := p.peek()
		if !ok || dc < '0' || dc > '9' {
			break
		}
		p.i++
	}
	if p.i == start+1 && p.s[start] == '-' {
		return jpIndex{}, jpMalformed("expected digits after `-`")
	}
	n, err := strconv.ParseInt(string(p.s[start:p.i]), 10, 64)
	if err != nil {
		return jpIndex{}, jpMalformed("subscript out of range")
	}
	return jpIndex{number: n}, nil
}
