package jed

// Regular-expression engine — a hand-written RE2-style Pike VM (spec/design/regex.md).
//
// jed's own RE2-able regex flavor (NOT PostgreSQL-compatible): a pattern compiles to a flat NFA
// bytecode program (regexProgram), matched over the input by a thread-list simulation in LINEAR TIME
// with no backtracking — immune to catastrophic-backtracking (ReDoS) attacks independent of the cost
// meter (CLAUDE.md §13). Both the compilation and the per-step cost are part of the cross-core
// contract (no reference impl, §2): all three cores emit the byte-identical program
// (spec/regex/program_vectors.toml) and accrue identical regex_compile / regex_step cost
// (spec/regex/match_vectors.toml). The lowering follows regex.md §3 exactly.

import (
	"fmt"
	"strings"
	"sync"
	"unicode/utf8"
)

// maxRegexProgram is the maximum compiled-program size, in instructions (regex.md §6, cost.md §7c).
// A fixed cross-core constant — a pattern whose program would exceed it aborts 54001 at compile.
const maxRegexProgram = 32768

// maxRegexCP is the largest Unicode scalar value (for class complement).
const maxRegexCP = 0x10FFFF

// ---------------------------------------------------------------------------
// Bytecode
// ---------------------------------------------------------------------------

type regexOp uint8

const (
	opChar regexOp = iota
	opAny
	// opAnyChar is any code point, newline included: the unanchored prefix's consume step
	// (regex.md §3.2), so a match may start after a newline.
	opAnyChar
	opClass
	opSplit
	opJmp
	opSave
	opAssertStart
	opAssertEnd
	// opAssertLineStart is `^` in the line-anchor mode (regex.md §8d): the subject start or just
	// after a newline. opAssertLineEnd is `$` there: the subject end or just before a newline.
	opAssertLineStart
	opAssertLineEnd
	opMatch
)

// regexInst is one NFA instruction. x/y hold jump targets (absolute instruction indices) for
// Split/Jmp, the slot for Save, the class index for Class; ch holds the code point for Char.
type regexInst struct {
	op regexOp
	x  int
	y  int
	ch rune
}

// regexClass is a character class: positive, sorted, merged code-point ranges plus a negated flag
// applied at match time (regex.md §3.4 — never by complementing the range list).
type regexClass struct {
	negated bool
	ranges  [][2]rune
}

func (c *regexClass) admits(r rune) bool {
	inside := false
	for _, rg := range c.ranges {
		if r >= rg[0] && r <= rg[1] {
			inside = true
			break
		}
	}
	return inside != c.negated
}

// regexProgram is a compiled pattern: the instruction array, the class table, and the capturing-
// group count (excluding group 0, the whole match), plus the shape of its capture-slot trees (capShape)
// and a pool of dedup sets (regexSeen) that its searches reuse.
type regexProgram struct {
	insts      []regexInst
	classes    []regexClass
	ngroups    int
	capDepth   int
	capLeafLen int
	seenPool   sync.Pool
}

func (p *regexProgram) ninst() int { return len(p.insts) }

// listing renders the canonical instruction listing (the program_vectors.toml contract, regex.md §9).
func (p *regexProgram) listing() []string {
	out := make([]string, len(p.insts))
	for i, in := range p.insts {
		switch in.op {
		case opChar:
			out[i] = fmt.Sprintf("char %d", in.ch)
		case opAny:
			out[i] = "any"
		case opAnyChar:
			out[i] = "anychar"
		case opClass:
			out[i] = fmt.Sprintf("class %d", in.x)
		case opSplit:
			out[i] = fmt.Sprintf("split %d %d", in.x, in.y)
		case opJmp:
			out[i] = fmt.Sprintf("jmp %d", in.x)
		case opSave:
			out[i] = fmt.Sprintf("save %d", in.x)
		case opAssertStart:
			out[i] = "assertstart"
		case opAssertEnd:
			out[i] = "assertend"
		case opAssertLineStart:
			out[i] = "assertlinestart"
		case opAssertLineEnd:
			out[i] = "assertlineend"
		case opMatch:
			out[i] = "match"
		}
	}
	return out
}

// classListing renders the canonical class table: `lo-hi` ranges joined by `,`, prefixed `^` when
// negated.
func (p *regexProgram) classListing() []string {
	out := make([]string, len(p.classes))
	for i, c := range p.classes {
		parts := make([]string, len(c.ranges))
		for j, rg := range c.ranges {
			parts[j] = fmt.Sprintf("%d-%d", rg[0], rg[1])
		}
		body := strings.Join(parts, ",")
		if c.negated {
			out[i] = "^" + body
		} else {
			out[i] = body
		}
	}
	return out
}

func regexInvalid(detail string) error {
	return newError(InvalidRegularExpression, "invalid regular expression: "+detail)
}

func regexTooComplex() error {
	return newError(StatementTooComplex,
		fmt.Sprintf("regular expression compiles to more than %d instructions", maxRegexProgram))
}

// ---------------------------------------------------------------------------
// Pattern AST
// ---------------------------------------------------------------------------

type regexNodeKind uint8

const (
	nEmpty regexNodeKind = iota
	nChar
	nAny
	nClass
	nConcat
	nAlt
	nStar
	nPlus
	nQuest
	nRepeat
	nGroup
	nAnchorStart
	nAnchorEnd
	nLineStart
	nLineEnd
)

type regexNode struct {
	kind     regexNodeKind
	ch       rune
	class    regexClass
	subs     []*regexNode // nConcat children; nAlt holds [left, right]; quantifiers/groups hold [sub]
	greedy   bool         // nStar / nPlus / nQuest / nRepeat
	repMin   int          // nRepeat
	repMax   int          // nRepeat; -1 means unbounded ({n,})
	groupIdx int          // nGroup: >0 capturing (1-based), 0 non-capturing
}

// ---------------------------------------------------------------------------
// Parser: pattern text -> AST
// ---------------------------------------------------------------------------

type regexParser struct {
	chars   []rune
	pos     int
	ngroups int
	mode    regexMode
}

// regexMode is a compile mode of jsonpath like_regex (regex.md §8d); the zero value is the SQL
// operators' mode.
type regexMode struct {
	// nlStop keeps a negated bracket class from matching a newline (PostgreSQL's REG_NLSTOP).
	nlStop bool
	// dotAll lets `.` match a newline (the XQuery `s` flag).
	dotAll bool
	// nlAnchor lets `^`/`$` also anchor just after / before a newline (the XQuery `m` flag,
	// PostgreSQL's REG_NLANCH). `\A`/`\z` stay string-anchored.
	nlAnchor bool
}

// quoteRegex escapes every metacharacter of a pattern so it compiles to the literal string (the
// XQuery `q` flag, regex.md §8d).
func quoteRegex(pattern string) string {
	var b strings.Builder
	for _, c := range pattern {
		if isRegexMeta(c) {
			b.WriteByte('\\')
		}
		b.WriteRune(c)
	}
	return b.String()
}

func (p *regexParser) peek() (rune, bool) {
	if p.pos < len(p.chars) {
		return p.chars[p.pos], true
	}
	return 0, false
}

func (p *regexParser) peekAt(k int) (rune, bool) {
	if p.pos+k < len(p.chars) {
		return p.chars[p.pos+k], true
	}
	return 0, false
}

func (p *regexParser) bump() (rune, bool) {
	c, ok := p.peek()
	if ok {
		p.pos++
	}
	return c, ok
}

// parseAlt: concat ('|' concat)*, right-folded (`a|b|c` == `a|(b|c)`).
func (p *regexParser) parseAlt() (*regexNode, error) {
	left, err := p.parseConcat()
	if err != nil {
		return nil, err
	}
	if c, ok := p.peek(); ok && c == '|' {
		p.bump()
		right, err := p.parseAlt()
		if err != nil {
			return nil, err
		}
		return &regexNode{kind: nAlt, subs: []*regexNode{left, right}}, nil
	}
	return left, nil
}

func (p *regexParser) parseConcat() (*regexNode, error) {
	var nodes []*regexNode
	for {
		c, ok := p.peek()
		if !ok || c == '|' || c == ')' {
			break
		}
		n, err := p.parseQuant()
		if err != nil {
			return nil, err
		}
		nodes = append(nodes, n)
	}
	switch len(nodes) {
	case 0:
		return &regexNode{kind: nEmpty}, nil
	case 1:
		return nodes[0], nil
	default:
		return &regexNode{kind: nConcat, subs: nodes}, nil
	}
}

func (p *regexParser) parseQuant() (*regexNode, error) {
	atom, err := p.parseAtom()
	if err != nil {
		return nil, err
	}
	var kind regexNodeKind
	repMin, repMax := 0, 0
	have := true
	switch c, _ := p.peek(); c {
	case '*':
		p.bump()
		kind = nStar
	case '+':
		p.bump()
		kind = nPlus
	case '?':
		p.bump()
		kind = nQuest
	case '{':
		ok, lo, hi, err := p.tryInterval()
		if err != nil {
			return nil, err
		}
		if !ok {
			return atom, nil
		}
		kind, repMin, repMax = nRepeat, lo, hi
	default:
		have = false
	}
	if !have {
		return atom, nil
	}
	// An optional trailing `?` makes the quantifier lazy.
	greedy := true
	if c, ok := p.peek(); ok && c == '?' {
		p.bump()
		greedy = false
	}
	// A second quantifier with no atom between (`a**`, `a*+`) is invalid (regex.md §2).
	if c, ok := p.peek(); ok && (c == '*' || c == '+' || c == '?') {
		return nil, regexInvalid("quantifier operand missing")
	}
	n := &regexNode{kind: kind, subs: []*regexNode{atom}, greedy: greedy}
	if kind == nRepeat {
		n.repMin, n.repMax = repMin, repMax
	}
	return n, nil
}

// tryInterval reads a `{n}`, `{n,}`, or `{n,m}` at the cursor. On a non-interval `{` the cursor is
// unmoved and ok=false (the `{` is later read as a literal — the PCRE lenient-brace rule). `{n,m}`
// with m<n is 2201B. max=-1 means unbounded.
func (p *regexParser) tryInterval() (ok bool, lo, hi int, err error) {
	start := p.pos
	p.bump() // '{'
	min, haveMin := p.readCount()
	if !haveMin {
		p.pos = start
		return false, 0, 0, nil
	}
	c, _ := p.peek()
	switch c {
	case '}':
		p.bump()
		return true, min, min, nil
	case ',':
		p.bump()
		if c2, _ := p.peek(); c2 == '}' {
			p.bump()
			return true, min, -1, nil
		}
		hiv, haveHi := p.readCount()
		if !haveHi {
			p.pos = start
			return false, 0, 0, nil
		}
		if c2, _ := p.peek(); c2 != '}' {
			p.pos = start
			return false, 0, 0, nil
		}
		p.bump() // '}'
		if hiv < min {
			return false, 0, 0, regexInvalid("invalid repetition count")
		}
		return true, min, hiv, nil
	default:
		p.pos = start
		return false, 0, 0, nil
	}
}

// readCount reads ASCII digits as a count, saturating at maxRegexProgram+1 so a giant interval
// cannot overflow and is rejected 54001 at emit.
func (p *regexParser) readCount() (int, bool) {
	any := false
	n := 0
	for {
		c, ok := p.peek()
		if !ok || c < '0' || c > '9' {
			break
		}
		any = true
		p.bump()
		n = n*10 + int(c-'0')
		if n > maxRegexProgram {
			n = maxRegexProgram + 1
		}
	}
	return n, any
}

func (p *regexParser) parseAtom() (*regexNode, error) {
	c, _ := p.peek()
	switch c {
	case '(':
		return p.parseGroup()
	case '[':
		return p.parseClass()
	case '.':
		p.bump()
		if p.mode.dotAll {
			// An empty negated class admits every code point (regex.md §8d).
			return &regexNode{kind: nClass, class: regexClass{negated: true}}, nil
		}
		return &regexNode{kind: nAny}, nil
	case '^':
		p.bump()
		if p.mode.nlAnchor {
			return &regexNode{kind: nLineStart}, nil
		}
		return &regexNode{kind: nAnchorStart}, nil
	case '$':
		p.bump()
		if p.mode.nlAnchor {
			return &regexNode{kind: nLineEnd}, nil
		}
		return &regexNode{kind: nAnchorEnd}, nil
	case '\\':
		p.bump()
		return p.parseEscape()
	case '*', '+', '?':
		// A quantifier where an atom is expected (`*ab`, `a|*`) is invalid (regex.md §2).
		return nil, regexInvalid("quantifier operand missing")
	default:
		// `{`, `}`, `]` are literals here (a `{` starting a valid interval is consumed by parseQuant
		// before reaching parseAtom — the lenient-brace rule, regex.md §2).
		p.bump()
		return &regexNode{kind: nChar, ch: c}, nil
	}
}

func (p *regexParser) parseGroup() (*regexNode, error) {
	p.bump() // '('
	capturing := true
	if c, ok := p.peek(); ok && c == '?' {
		// `(?:...)` is non-capturing; any other `(?...)` is an excluded construct (regex.md §2).
		if c2, _ := p.peekAt(1); c2 == ':' {
			p.bump()
			p.bump()
			capturing = false
		} else {
			return nil, regexInvalid("unsupported group syntax")
		}
	}
	idx := 0
	if capturing {
		p.ngroups++
		idx = p.ngroups
	}
	inner, err := p.parseAlt()
	if err != nil {
		return nil, err
	}
	if c, ok := p.peek(); !ok || c != ')' {
		return nil, regexInvalid("unbalanced parenthesis")
	}
	p.bump() // ')'
	return &regexNode{kind: nGroup, subs: []*regexNode{inner}, groupIdx: idx}, nil
}

func (p *regexParser) parseEscape() (*regexNode, error) {
	c, ok := p.bump()
	if !ok {
		return nil, regexInvalid("trailing backslash")
	}
	// String-boundary anchors (regex.md §2): `\A` = start of subject, `\z` = absolute end of
	// subject. jed has no multiline mode, so these are exactly `^`/`$` today; they are spelled
	// separately so they stay string-anchored once a line mode lands. PCRE's `\Z` (end, or before
	// a trailing newline) is deliberately NOT accepted — its only distinguishing behavior is
	// trailing-newline leniency jed does nowhere, so it stays an invalid escape.
	switch c {
	case 'A':
		return &regexNode{kind: nAnchorStart}, nil
	case 'z':
		return &regexNode{kind: nAnchorEnd}, nil
	}
	if ranges, negated, ok := predefClass(c); ok {
		return &regexNode{kind: nClass, class: regexClass{negated: negated, ranges: ranges}}, nil
	}
	if ctrl, ok := controlEscape(c); ok {
		return &regexNode{kind: nChar, ch: ctrl}, nil
	}
	if isRegexMeta(c) {
		return &regexNode{kind: nChar, ch: c}, nil
	}
	return nil, regexInvalid(fmt.Sprintf("invalid escape \\%c", c))
}

func (p *regexParser) parseClass() (*regexNode, error) {
	p.bump() // '['
	negated := false
	if c, ok := p.peek(); ok && c == '^' {
		p.bump()
		negated = true
	}
	var ranges [][2]rune
	first := true
	for {
		c, ok := p.peek()
		if !ok {
			return nil, regexInvalid("unbalanced bracket expression")
		}
		if c == ']' && !first {
			p.bump()
			break
		}
		lo, set, isSet, err := p.classItem()
		if err != nil {
			return nil, err
		}
		if isSet {
			ranges = append(ranges, set...)
			first = false
			continue
		}
		// `lo-hi` is a range only when `-` is followed by a real high end (not `]`).
		if c2, ok2 := p.peek(); ok2 && c2 == '-' {
			if c3, ok3 := p.peekAt(1); ok3 && c3 != ']' {
				p.bump() // '-'
				hi, hset, hIsSet, err := p.classItem()
				if err != nil {
					return nil, err
				}
				if hIsSet {
					// `[\d-a]` etc. — lenient: the `-` is a literal and the set is added.
					ranges = append(ranges, [2]rune{lo, lo}, [2]rune{'-', '-'})
					ranges = append(ranges, hset...)
				} else {
					if lo > hi {
						return nil, regexInvalid("invalid range in bracket expression")
					}
					ranges = append(ranges, [2]rune{lo, hi})
				}
				first = false
				continue
			}
		}
		ranges = append(ranges, [2]rune{lo, lo})
		first = false
	}
	if negated && p.mode.nlStop {
		// Newline-sensitive: `[^…]` does not match a newline (regex.md §8d).
		ranges = append(ranges, [2]rune{'\n', '\n'})
	}
	return &regexNode{kind: nClass, class: regexClass{negated: negated, ranges: normalizeRanges(ranges)}}, nil
}

// classItem parses one item inside a `[...]`: a predefined class becomes a set; anything else a
// single char (escapes resolved).
func (p *regexParser) classItem() (lo rune, set [][2]rune, isSet bool, err error) {
	c, _ := p.bump()
	if c != '\\' {
		return c, nil, false, nil
	}
	e, ok := p.bump()
	if !ok {
		return 0, nil, false, regexInvalid("trailing backslash")
	}
	if ranges, negated, ok := predefClass(e); ok {
		if negated {
			return 0, complementRanges(normalizeRanges(ranges)), true, nil
		}
		return 0, ranges, true, nil
	}
	if ctrl, ok := controlEscape(e); ok {
		return ctrl, nil, false, nil
	}
	if isRegexMeta(e) || e == '-' || e == ']' {
		return e, nil, false, nil
	}
	return 0, nil, false, regexInvalid(fmt.Sprintf("invalid escape \\%c", e))
}

func isRegexMeta(c rune) bool {
	switch c {
	case '.', '*', '+', '?', '(', ')', '[', ']', '{', '}', '|', '^', '$', '\\':
		return true
	}
	return false
}

func controlEscape(c rune) (rune, bool) {
	switch c {
	case 'n':
		return '\n', true
	case 't':
		return '\t', true
	case 'r':
		return '\r', true
	case 'f':
		return '\f', true
	case 'v':
		return '\v', true
	}
	return 0, false
}

// predefClass returns the predefined classes \d \w \s (and their negations): positive ranges plus
// whether the letter was the negated (uppercase) form. ASCII baseline for Slice 1.
func predefClass(c rune) ([][2]rune, bool, bool) {
	switch c {
	case 'd':
		return [][2]rune{{48, 57}}, false, true
	case 'D':
		return [][2]rune{{48, 57}}, true, true
	case 'w':
		return [][2]rune{{48, 57}, {65, 90}, {95, 95}, {97, 122}}, false, true
	case 'W':
		return [][2]rune{{48, 57}, {65, 90}, {95, 95}, {97, 122}}, true, true
	case 's':
		return [][2]rune{{9, 13}, {32, 32}}, false, true
	case 'S':
		return [][2]rune{{9, 13}, {32, 32}}, true, true
	}
	return nil, false, false
}

// normalizeRanges sorts by lo and merges touching/overlapping ranges (regex.md §3.4).
func normalizeRanges(ranges [][2]rune) [][2]rune {
	if len(ranges) == 0 {
		return nil
	}
	// insertion-friendly sort (small lists); stable, deterministic.
	for i := 1; i < len(ranges); i++ {
		for j := i; j > 0 && (ranges[j][0] < ranges[j-1][0] || (ranges[j][0] == ranges[j-1][0] && ranges[j][1] < ranges[j-1][1])); j-- {
			ranges[j], ranges[j-1] = ranges[j-1], ranges[j]
		}
	}
	out := make([][2]rune, 0, len(ranges))
	for _, rg := range ranges {
		if len(out) > 0 {
			last := &out[len(out)-1]
			if rg[0] <= last[1]+1 {
				if rg[1] > last[1] {
					last[1] = rg[1]
				}
				continue
			}
		}
		out = append(out, rg)
	}
	return out
}

// complementRanges returns the complement of normalized ranges over [0, maxRegexCP].
func complementRanges(ranges [][2]rune) [][2]rune {
	var out [][2]rune
	next := rune(0)
	for _, rg := range ranges {
		if rg[0] > next {
			out = append(out, [2]rune{next, rg[0] - 1})
		}
		next = rg[1] + 1
		if next > maxRegexCP {
			return out
		}
	}
	if next <= maxRegexCP {
		out = append(out, [2]rune{next, maxRegexCP})
	}
	return out
}

// ---------------------------------------------------------------------------
// Compiler: AST -> bytecode (the exact emission of regex.md §3)
// ---------------------------------------------------------------------------

type regexCompiler struct {
	insts   []regexInst
	classes []regexClass
}

func (c *regexCompiler) push(in regexInst) (int, error) {
	if len(c.insts) >= maxRegexProgram {
		return 0, regexTooComplex()
	}
	i := len(c.insts)
	c.insts = append(c.insts, in)
	return i, nil
}

func (c *regexCompiler) emit(n *regexNode) error {
	switch n.kind {
	case nEmpty:
		return nil
	case nChar:
		_, err := c.push(regexInst{op: opChar, ch: n.ch})
		return err
	case nAny:
		_, err := c.push(regexInst{op: opAny})
		return err
	case nClass:
		k := len(c.classes)
		c.classes = append(c.classes, n.class)
		_, err := c.push(regexInst{op: opClass, x: k})
		return err
	case nConcat:
		for _, s := range n.subs {
			if err := c.emit(s); err != nil {
				return err
			}
		}
		return nil
	case nAlt:
		// Split LX,LY ; LX: <a>; Jmp LEND ; LY: <b>; LEND:
		split, err := c.push(regexInst{op: opSplit})
		if err != nil {
			return err
		}
		lx := len(c.insts)
		if err := c.emit(n.subs[0]); err != nil {
			return err
		}
		jmp, err := c.push(regexInst{op: opJmp})
		if err != nil {
			return err
		}
		ly := len(c.insts)
		if err := c.emit(n.subs[1]); err != nil {
			return err
		}
		lend := len(c.insts)
		c.insts[split].x, c.insts[split].y = lx, ly
		c.insts[jmp].x = lend
		return nil
	case nStar:
		// L1: Split L2,L3 (greedy) / Split L3,L2 (lazy) ; L2: <sub>; Jmp L1 ; L3:
		l1, err := c.push(regexInst{op: opSplit})
		if err != nil {
			return err
		}
		l2 := len(c.insts)
		if err := c.emit(n.subs[0]); err != nil {
			return err
		}
		if _, err := c.push(regexInst{op: opJmp, x: l1}); err != nil {
			return err
		}
		l3 := len(c.insts)
		if n.greedy {
			c.insts[l1].x, c.insts[l1].y = l2, l3
		} else {
			c.insts[l1].x, c.insts[l1].y = l3, l2
		}
		return nil
	case nPlus:
		// L1: <sub>; Split L1,L3 (greedy) / Split L3,L1 (lazy) ; L3:
		l1 := len(c.insts)
		if err := c.emit(n.subs[0]); err != nil {
			return err
		}
		split, err := c.push(regexInst{op: opSplit})
		if err != nil {
			return err
		}
		l3 := len(c.insts)
		if n.greedy {
			c.insts[split].x, c.insts[split].y = l1, l3
		} else {
			c.insts[split].x, c.insts[split].y = l3, l1
		}
		return nil
	case nQuest:
		// Split L1,L2 (greedy) / Split L2,L1 (lazy) ; L1: <sub>; L2:
		split, err := c.push(regexInst{op: opSplit})
		if err != nil {
			return err
		}
		l1 := len(c.insts)
		if err := c.emit(n.subs[0]); err != nil {
			return err
		}
		l2 := len(c.insts)
		if n.greedy {
			c.insts[split].x, c.insts[split].y = l1, l2
		} else {
			c.insts[split].x, c.insts[split].y = l2, l1
		}
		return nil
	case nRepeat:
		return c.emitRepeat(n)
	case nGroup:
		if n.groupIdx > 0 {
			if _, err := c.push(regexInst{op: opSave, x: 2 * n.groupIdx}); err != nil {
				return err
			}
			if err := c.emit(n.subs[0]); err != nil {
				return err
			}
			_, err := c.push(regexInst{op: opSave, x: 2*n.groupIdx + 1})
			return err
		}
		return c.emit(n.subs[0])
	case nAnchorStart:
		_, err := c.push(regexInst{op: opAssertStart})
		return err
	case nAnchorEnd:
		_, err := c.push(regexInst{op: opAssertEnd})
		return err
	case nLineStart:
		_, err := c.push(regexInst{op: opAssertLineStart})
		return err
	case nLineEnd:
		_, err := c.push(regexInst{op: opAssertLineEnd})
		return err
	}
	return nil
}

// emitRepeat unrolls `{min,max}` -> min mandatory copies, then a Star ({min,}) or (max-min)
// greedy/lazy Quest copies. Each copy's emit checks the cap, so a giant interval aborts 54001
// (regex.md §3.3, §6).
func (c *regexCompiler) emitRepeat(n *regexNode) error {
	sub := n.subs[0]
	for i := 0; i < n.repMin; i++ {
		if err := c.emit(sub); err != nil {
			return err
		}
	}
	if n.repMax < 0 {
		return c.emit(&regexNode{kind: nStar, subs: []*regexNode{sub}, greedy: n.greedy})
	}
	for i := 0; i < n.repMax-n.repMin; i++ {
		if err := c.emit(&regexNode{kind: nQuest, subs: []*regexNode{sub}, greedy: n.greedy}); err != nil {
			return err
		}
	}
	return nil
}

// foldRegexPattern is the case-insensitive pattern fold (regex.md §1): foldLowerSimple over the
// pattern's literal characters and class members, with the code point after each backslash kept as
// written. Every escape is a backslash plus one code point (§2), so this leaves escape letters
// intact — `\D \W \S` stay negated classes rather than becoming `\d \w \s`, and `\A` stays an anchor.
func foldRegexPattern(pat string, p *propertyTable) string {
	var b strings.Builder
	b.Grow(len(pat))
	start := 0
	for i := 0; i < len(pat); i++ {
		if pat[i] != '\\' {
			continue
		}
		b.WriteString(foldLowerSimple(pat[start:i], p))
		end := i + 1
		if end < len(pat) {
			_, size := utf8.DecodeRuneInString(pat[end:])
			end += size
		}
		b.WriteString(pat[i:end])
		start = end
		i = end - 1
	}
	b.WriteString(foldLowerSimple(pat[start:], p))
	return b.String()
}

// compileRegex compiles a pattern to a program (regex.md §3). Raises 2201B on a malformed pattern
// and 54001 on a well-formed-but-too-large one. Does NOT meter — the caller charges
// regex_compile × program.ninst() (the precompilation contract, regex.md §5). For ~* the pattern
// must already be case-folded by the caller (foldRegexPattern).
func compileRegex(pattern string) (*regexProgram, error) {
	return compileRegexMode(pattern, regexMode{})
}

// compileRegexMode is compileRegex under a jsonpath like_regex compile mode (regex.md §8d).
func compileRegexMode(pattern string, mode regexMode) (*regexProgram, error) {
	p := &regexParser{chars: []rune(pattern), mode: mode}
	root, err := p.parseAlt()
	if err != nil {
		return nil, err
	}
	if p.pos != len(p.chars) {
		return nil, regexInvalid("unbalanced parenthesis")
	}
	c := &regexCompiler{}
	// Wrapper (regex.md §3.2): lazy `.*?` prefix + group-0 save + Match.
	if _, err := c.push(regexInst{op: opSplit, x: 3, y: 1}); err != nil {
		return nil, err
	}
	if _, err := c.push(regexInst{op: opAnyChar}); err != nil {
		return nil, err
	}
	if _, err := c.push(regexInst{op: opJmp, x: 0}); err != nil {
		return nil, err
	}
	if _, err := c.push(regexInst{op: opSave, x: 0}); err != nil {
		return nil, err
	}
	if err := c.emit(root); err != nil {
		return nil, err
	}
	if _, err := c.push(regexInst{op: opSave, x: 1}); err != nil {
		return nil, err
	}
	if _, err := c.push(regexInst{op: opMatch}); err != nil {
		return nil, err
	}
	prog := &regexProgram{insts: c.insts, classes: c.classes, ngroups: p.ngroups}
	prog.capDepth, prog.capLeafLen = capShape(2 * (p.ngroups + 1))
	return prog, nil
}

// ---------------------------------------------------------------------------
// Capture slots — a persistent array (regex.md §4.1)
// ---------------------------------------------------------------------------

// A capture-slot tree's leaves hold capFan slots and its interior nodes capFan subtrees. Every group
// emits two Saves, so a program's 2·(ngroups+1) slots never exceed maxRegexProgram and fit in four
// levels.
const (
	capFanBits = 4
	capFan     = 1 << capFanBits
)

// regexCaps is a thread's or match's capture slots (code-point offsets; -1 = unset), a persistent
// array (regex.md §4.1): a Save makes a new version and leaves the old one intact for the threads
// still holding it. A pattern whose slots fit in one capFan-slot leaf keeps them flat, and a Save
// copies all of them; the array holds no pointers, so it is the cheapest allocation the collector
// has. A larger pattern keeps a tree, and a Save copies only the path to the slot it sets, so its
// work is bounded however many groups the pattern has. The zero value has every slot unset.
type regexCaps struct {
	flat []int64
	tree *capNode
}

// capNode is a node of a capture-slot tree. A leaf holds slot values, an interior node its subtrees;
// a nil node means every slot under it is unset.
type capNode struct {
	vals []int64
	kids []*capNode
}

// capLeaf and capInner allocate a node together with the array its slice views, so a level of a
// Save is one allocation.
type capLeaf struct {
	node capNode
	buf  [capFan]int64
}

type capInner struct {
	node capNode
	buf  [capFan]*capNode
}

// capShape returns the depth of the capture-slot tree for nslots slots and the length of its leaves:
// a one-level tree is a single leaf of exactly nslots slots, kept flat; a deeper one has full leaves.
func capShape(nslots int) (depth, leafLen int) {
	depth = 1
	for span := capFan; span < nslots; span *= capFan {
		depth++
	}
	if depth == 1 {
		return 1, nslots
	}
	return depth, capFan
}

// slot returns capture slot i of a thread's or match's slots (-1 = unset).
func (p *regexProgram) slot(caps regexCaps, i int) int64 {
	if p.capDepth == 1 {
		if caps.flat == nil {
			return -1
		}
		return caps.flat[i]
	}
	n := caps.tree
	for shift := capFanBits * (p.capDepth - 1); shift > 0 && n != nil; shift -= capFanBits {
		n = n.kids[(i>>shift)&(capFan-1)]
	}
	if n == nil {
		return -1
	}
	return n.vals[i&(capFan-1)]
}

// withSlotAt returns the version of the subtree n with slot i set to v, sharing every node off the
// path to it.
func (p *regexProgram) withSlotAt(n *capNode, shift, i int, v int64) *capNode {
	if shift == 0 {
		leaf := &capLeaf{}
		vals := leaf.buf[:]
		if n != nil {
			copy(vals, n.vals)
		} else {
			for j := range vals {
				vals[j] = -1
			}
		}
		vals[i&(capFan-1)] = v
		leaf.node.vals = vals
		return &leaf.node
	}
	inner := &capInner{}
	kids := inner.buf[:]
	if n != nil {
		copy(kids, n.kids)
	}
	k := (i >> shift) & (capFan - 1)
	kids[k] = p.withSlotAt(kids[k], shift-capFanBits, i, v)
	inner.node.kids = kids
	return &inner.node
}

// slots returns every capture slot of caps as a flat array.
func (p *regexProgram) slots(caps regexCaps) []int64 {
	out := make([]int64, 2*(p.ngroups+1))
	for i := range out {
		out[i] = p.slot(caps, i)
	}
	return out
}

// ---------------------------------------------------------------------------
// Pike VM (regex.md §4)
// ---------------------------------------------------------------------------

// capRep is how the VM keeps a thread's capture slots (regex.md §4.1): flatCaps for a pattern whose
// slots fit in one leaf, treeCaps for a larger one. The VM is instantiated once per representation,
// so a thread is a pc and one field, and a small pattern's slots stay a pointer-free array.
type capRep[C any] interface {
	with(p *regexProgram, caps C, i int, v int64) C
	result(caps C) regexCaps
}

// flatCaps keeps the slots as one array, copied whole on a Save: at most capFan slots.
type flatCaps struct{}

func (flatCaps) with(p *regexProgram, caps []int64, i int, v int64) []int64 {
	flat := make([]int64, p.capLeafLen)
	if caps != nil {
		copy(flat, caps)
	} else {
		for j := range flat {
			flat[j] = -1
		}
	}
	flat[i] = v
	return flat
}

func (flatCaps) result(caps []int64) regexCaps { return regexCaps{flat: caps} }

// treeCaps keeps the slots as a capture-slot tree; a Save copies the path to the slot it sets.
type treeCaps struct{}

func (treeCaps) with(p *regexProgram, caps *capNode, i int, v int64) *capNode {
	return p.withSlotAt(caps, capFanBits*(p.capDepth-1), i, v)
}

func (treeCaps) result(caps *capNode) regexCaps { return regexCaps{tree: caps} }

type regexThread[C any] struct {
	pc   int
	caps C
}

// regexSeen is the per-position dedup set (regex.md §4): marks[pc] == generation means pc was visited
// at the current position, so moving to the next position is an increment, not a clear. A program
// pools its sets and reuses them across searches and rows, so a search does no setup work in
// proportion to the program, only the states it explores, which regex_step meters (§4.2).
type regexSeen struct {
	marks      []uint32
	generation uint32
}

// next starts a new generation. When the counter wraps, the stale marks could alias the new
// generation, so they are cleared.
func (s *regexSeen) next() uint32 {
	s.generation++
	if s.generation == 0 {
		clear(s.marks)
		s.generation = 1
	}
	return s.generation
}

// takeSeen returns a dedup set for one call's searches, from the program's pool when it has one. The
// caller returns it with p.seenPool.Put.
func (p *regexProgram) takeSeen() *regexSeen {
	if s, ok := p.seenPool.Get().(*regexSeen); ok {
		return s
	}
	return &regexSeen{marks: make([]uint32, len(p.insts))}
}

// isMatch reports whether the pattern matches somewhere in input (the `~` operator).
func (p *regexProgram) isMatch(input []rune, m *costMeter) (bool, error) {
	_, ok, err := p.run(input, m)
	return ok, err
}

// run executes the Pike VM from the start of the input (regex.md §4). Returns the winning thread's
// capture slots on a match, or nil.
func (p *regexProgram) run(input []rune, m *costMeter) (regexCaps, bool, error) {
	seen := p.takeSeen()
	defer p.seenPool.Put(seen)
	return p.search(seen, input, 0, m)
}

// search runs the Pike VM, considering only matches that START at code-point position `start` or
// later (the unanchored search seeds its lazy `.*?` prefix at `start`); ^/$ still anchor at the true
// input bounds. Used by regexp_replace's global loop. Charges regex_step per explored state and
// guards once per input position.
func (p *regexProgram) search(seen *regexSeen, input []rune, start int, m *costMeter) (regexCaps, bool, error) {
	if p.capDepth == 1 {
		return pikeSearch[[]int64, flatCaps](p, seen, input, start, m)
	}
	return pikeSearch[*capNode, treeCaps](p, seen, input, start, m)
}

// pikeSearch is search over the capture representation R.
func pikeSearch[C any, R capRep[C]](p *regexProgram, seen *regexSeen, input []rune, start int, m *costMeter) (regexCaps, bool, error) {
	var rep R
	var unset C
	length := len(input)
	clist := []regexThread[C]{}
	nlist := []regexThread[C]{}
	var matched C
	found := false

	if err := pikeAddThread[C, R](p, &clist, seen, seen.next(), 0, unset, start, input, m); err != nil {
		return regexCaps{}, false, err
	}

	for sp := start; sp <= length; sp++ {
		generation := seen.next()
		nlist = nlist[:0]
	inner:
		for i := 0; i < len(clist); i++ {
			th := clist[i]
			in := p.insts[th.pc]
			switch in.op {
			case opChar:
				if sp < length && input[sp] == in.ch {
					if err := pikeAddThread[C, R](p, &nlist, seen, generation, th.pc+1, th.caps, sp+1, input, m); err != nil {
						return regexCaps{}, false, err
					}
				}
			case opAny:
				if sp < length && input[sp] != '\n' {
					if err := pikeAddThread[C, R](p, &nlist, seen, generation, th.pc+1, th.caps, sp+1, input, m); err != nil {
						return regexCaps{}, false, err
					}
				}
			case opAnyChar:
				if sp < length {
					if err := pikeAddThread[C, R](p, &nlist, seen, generation, th.pc+1, th.caps, sp+1, input, m); err != nil {
						return regexCaps{}, false, err
					}
				}
			case opClass:
				if sp < length && p.classes[in.x].admits(input[sp]) {
					if err := pikeAddThread[C, R](p, &nlist, seen, generation, th.pc+1, th.caps, sp+1, input, m); err != nil {
						return regexCaps{}, false, err
					}
				}
			case opMatch:
				matched, found = th.caps, true
				break inner // cut lower-priority threads (leftmost-first, regex.md §4)
			}
		}
		clist, nlist = nlist, clist
		if err := m.Guard(); err != nil { // §6 ceiling, once per input position
			return regexCaps{}, false, err
		}
		if len(clist) == 0 {
			break
		}
	}
	return rep.result(matched), found, nil
}

// pikeAddThread performs the epsilon-closure: follow Jmp/Split/Save/Assert from pc, appending
// consuming/Match threads to *list, deduping by pc within this generation. Iterative (explicit
// stack) so a long Jmp/Split chain cannot overflow the native stack; the y arm of a Split is pushed
// before x so x is processed first (higher priority). Charges regex_step per explored state.
func pikeAddThread[C any, R capRep[C]](p *regexProgram, list *[]regexThread[C], seen *regexSeen, generation uint32, pc0 int, caps0 C, sp int, input []rune, m *costMeter) error {
	var rep R
	length := len(input)
	stack := []regexThread[C]{{pc0, caps0}}
	for len(stack) > 0 {
		top := stack[len(stack)-1]
		stack = stack[:len(stack)-1]
		if seen.marks[top.pc] == generation {
			continue
		}
		seen.marks[top.pc] = generation
		m.Charge(costs.RegexStep)
		in := p.insts[top.pc]
		switch in.op {
		case opJmp:
			stack = append(stack, regexThread[C]{in.x, top.caps})
		case opSplit:
			// Push y first, then x, so x pops first = higher priority.
			stack = append(stack, regexThread[C]{in.y, top.caps})
			stack = append(stack, regexThread[C]{in.x, top.caps})
		case opSave:
			stack = append(stack, regexThread[C]{top.pc + 1, rep.with(p, top.caps, in.x, int64(sp))})
		case opAssertStart:
			if sp == 0 {
				stack = append(stack, regexThread[C]{top.pc + 1, top.caps})
			}
		case opAssertEnd:
			if sp == length {
				stack = append(stack, regexThread[C]{top.pc + 1, top.caps})
			}
		case opAssertLineStart:
			if sp == 0 || input[sp-1] == '\n' {
				stack = append(stack, regexThread[C]{top.pc + 1, top.caps})
			}
		case opAssertLineEnd:
			if sp == length || input[sp] == '\n' {
				stack = append(stack, regexThread[C]{top.pc + 1, top.caps})
			}
		default:
			// Char / Any / Class / Match — parked for the consume loop.
			*list = append(*list, top)
		}
	}
	return nil
}

// reportedGroups returns the capture groups regexp_match / regexp_matches report for a match (regex.md
// §8): groups 1..n, or group 0 (the whole match) when the pattern has no group — the PG rule — as
// the inclusive range [lo, hi].
func (p *regexProgram) reportedGroups() (lo, hi int) {
	if p.ngroups == 0 {
		return 0, 0
	}
	return 1, p.ngroups
}

// captureArraySize is the exact payload (memory.md §3) of the one-dimensional text[] capture array for
// a match's slots, computed from the spans without building it (cost.md §8.1): ARRAY_DIM, plus VALUE
// and the span's UTF-8 bytes per reported group (an unset group is a bare VALUE). utf8At is
// utf8Offsets of the subject, so each span is measured in O(1).
func (p *regexProgram) captureArraySize(caps regexCaps, utf8At []int64) int64 {
	size := memoryArrayDim
	lo, hi := p.reportedGroups()
	for g := lo; g <= hi; g++ {
		s, e := p.slot(caps, 2*g), p.slot(caps, 2*g+1)
		size += memoryValue
		if s >= 0 && e >= 0 {
			size += utf8At[e] - utf8At[s]
		}
	}
	return size
}

// captureArray returns the capture array for a match's slots, sliced from the ORIGINAL-case subject:
// the reported groups' substrings, an unset group nil.
func (p *regexProgram) captureArray(caps regexCaps, orig []rune) []*string {
	lo, hi := p.reportedGroups()
	groups := make([]*string, 0, hi-lo+1)
	for g := lo; g <= hi; g++ {
		groups = append(groups, sliceGroup(orig, p.slot(caps, 2*g), p.slot(caps, 2*g+1)))
	}
	return groups
}

// utf8Offsets returns the UTF-8 byte offset of every rune of orig, plus the total at len(orig): the
// span [s, e) is offsets[e] - offsets[s] bytes.
func utf8Offsets(orig []rune) []int64 {
	offsets := make([]int64, len(orig)+1)
	var at int64
	for i, c := range orig {
		at += int64(utf8.RuneLen(c))
		offsets[i+1] = at
	}
	return offsets
}

// utf8RunesLen returns the UTF-8 byte length of runes.
func utf8RunesLen(runes []rune) int64 {
	var n int64
	for _, c := range runes {
		n += int64(utf8.RuneLen(c))
	}
	return n
}

// splitPieces returns the pieces of regexp_split_to_array / regexp_split_to_table (regex.md §8c):
// the text between consecutive matches, found with the global advance, then the text after the
// last. PostgreSQL's DEGENERATE matches split nothing: an empty match at the start or end of the
// subject, or one that ends no later than the previous match — so ” splits into characters and a
// split of the empty string is one empty piece. The pieces are sliced from the ORIGINAL-case subject
// (same length as the matched one).
func (p *regexProgram) splitPieces(matchInput, origInput []rune, m *costMeter) ([]string, error) {
	seen := p.takeSeen()
	defer p.seenPool.Put(seen)
	length := len(matchInput)
	var pieces []string
	pos := 0
	prevEnd := 0    // the end of the previous match, degenerate or not
	pieceStart := 0 // the end of the previous SPLITTING match
	for pos <= length {
		caps, ok, err := p.search(seen, matchInput, pos, m)
		if err != nil {
			return nil, err
		}
		if !ok {
			break
		}
		s, e := int(p.slot(caps, 0)), int(p.slot(caps, 1))
		if s < length && e > prevEnd {
			pieces = append(pieces, string(origInput[pieceStart:s]))
			pieceStart = e
		}
		prevEnd = e
		if e > s {
			pos = e
		} else {
			pos = e + 1
		}
	}
	pieces = append(pieces, string(origInput[pieceStart:length]))
	return pieces, nil
}

// replaceMatches finds the matches regexp_replace(source, pattern, replacement, …) (regex.md §8)
// replaces: the first match, or every non-overlapping match when global. Each entry is a match's
// capture slots. The VM matches over matchInput (possibly case-folded); this is the kernel's whole
// search, so it charges every regex_step the replacement costs. replaceSize and replaceBuild then size
// and splice from these slots without searching again (cost.md §8.1).
// regexp_match (never global) and regexp_matches run the same search, then size and slice their
// capture arrays from the slots (captureArraySize, captureArray).
func (p *regexProgram) replaceMatches(matchInput []rune, global bool, m *costMeter) ([]regexCaps, error) {
	seen := p.takeSeen()
	defer p.seenPool.Put(seen)
	var found []regexCaps
	pos := 0
	for {
		caps, ok, err := p.search(seen, matchInput, pos, m)
		if err != nil {
			return nil, err
		}
		if !ok {
			break
		}
		s := int(p.slot(caps, 0))
		e := int(p.slot(caps, 1))
		found = append(found, caps)
		if !global {
			break
		}
		// Empty match: the next search starts past the char at e, so a pattern that can match empty
		// (a*) cannot loop forever — the PG global rule.
		if e > s {
			pos = e
		} else {
			pos = e + 1
		}
		if pos > len(matchInput) {
			break
		}
	}
	return found, nil
}

// regexpCount counts the non-overlapping matches at or after code-point position `start`
// (regexp_count, regex.md §8b). The advance is regexpReplace's global rule: after a match [s,e)
// continue at e, or at e+1 for an EMPTY match so a nullable pattern terminates. `start` may be up to
// len (an empty match at the very end still counts); start > len (clamped to len+1 by the caller)
// yields 0.
func (p *regexProgram) regexpCount(input []rune, start int, m *costMeter) (int64, error) {
	seen := p.takeSeen()
	defer p.seenPool.Put(seen)
	length := len(input)
	pos := start
	var count int64
	for pos <= length {
		caps, ok, err := p.search(seen, input, pos, m)
		if err != nil {
			return 0, err
		}
		if !ok {
			break
		}
		count++
		s, e := int(p.slot(caps, 0)), int(p.slot(caps, 1))
		if e > s {
			pos = e
		} else {
			pos = e + 1
		}
	}
	return count, nil
}

// nthMatch returns the capture slots of the N-th (1-based) non-overlapping match at or after `start`
// (regexp_substr / regexp_instr, regex.md §8b), or nil when fewer than N matches exist. Same
// non-overlapping advance as regexpCount.
func (p *regexProgram) nthMatch(input []rune, start int, n int64, m *costMeter) (regexCaps, bool, error) {
	seen := p.takeSeen()
	defer p.seenPool.Put(seen)
	length := len(input)
	pos := start
	var count int64
	for pos <= length {
		caps, ok, err := p.search(seen, input, pos, m)
		if err != nil {
			return regexCaps{}, false, err
		}
		if !ok {
			break
		}
		count++
		if count == n {
			return caps, true, nil
		}
		s, e := int(p.slot(caps, 0)), int(p.slot(caps, 1))
		if e > s {
			pos = e
		} else {
			pos = e + 1
		}
	}
	return regexCaps{}, false, nil
}

// sliceGroup slices orig[start:end] to a string pointer, or nil for an unset (-1) group.
func sliceGroup(orig []rune, start, end int64) *string {
	if start < 0 || end < 0 {
		return nil
	}
	s := string(orig[start:end])
	return &s
}

// forEachPiece walks a replacement template, expanding \1..\9 (capture group), \& (whole match), \\
// (literal backslash), and \<other> (the literal <other>). A trailing lone \ is literal, and an unset
// or absent group expands to nothing. Each piece is a literal rune (lo < 0) or the [lo, hi) span of
// the original input. Sizing and building share this walk so they cannot disagree.
func (p *regexProgram) forEachPiece(repl []rune, caps regexCaps, f func(r rune, lo, hi int)) {
	span := func(start, end int64) {
		if start >= 0 && end >= 0 {
			f(0, int(start), int(end))
		}
	}
	for i := 0; i < len(repl); i++ {
		c := repl[i]
		if c == '\\' && i+1 < len(repl) {
			n := repl[i+1]
			switch {
			case n >= '0' && n <= '9':
				g := int(n - '0')
				if g <= p.ngroups {
					span(p.slot(caps, 2*g), p.slot(caps, 2*g+1))
				}
			case n == '&':
				span(p.slot(caps, 0), p.slot(caps, 1))
			default:
				f(n, -1, -1) // \\ -> \, and \<other> -> <other>
			}
			i++
		} else {
			f(c, -1, -1)
		}
	}
}

// forEachOutput walks the pieces of a replacement in output order: unmatched source text, each match's
// expanded template, and, for an empty match, the rune after it.
func (p *regexProgram) forEachOutput(orig, repl []rune, found []regexCaps, f func(r rune, lo, hi int)) {
	pos := 0
	for _, caps := range found {
		s := int(p.slot(caps, 0))
		e := int(p.slot(caps, 1))
		f(0, pos, s)
		p.forEachPiece(repl, caps, f)
		if e > s {
			pos = e
		} else {
			if e < len(orig) {
				f(orig[e], -1, -1)
			}
			pos = e + 1
		}
	}
	if pos <= len(orig) {
		f(0, pos, len(orig))
	}
}

// replaceSize is the exact UTF-8 byte length of regexp_replace's result for the matches replaceMatches
// found, computed without building it (cost.md §8.1).
func (p *regexProgram) replaceSize(orig, repl []rune, found []regexCaps) int64 {
	var size int64
	p.forEachOutput(orig, repl, found, func(r rune, lo, hi int) {
		if lo < 0 {
			size += int64(utf8.RuneLen(r))
			return
		}
		for _, c := range orig[lo:hi] {
			size += int64(utf8.RuneLen(c))
		}
	})
	return size
}

// replaceBuild builds regexp_replace's result of size bytes from the matches replaceMatches found.
func (p *regexProgram) replaceBuild(orig, repl []rune, found []regexCaps, size int64) string {
	var b strings.Builder
	b.Grow(int(size))
	p.forEachOutput(orig, repl, found, func(r rune, lo, hi int) {
		if lo < 0 {
			b.WriteRune(r)
			return
		}
		for _, c := range orig[lo:hi] {
			b.WriteRune(c)
		}
	})
	return b.String()
}
