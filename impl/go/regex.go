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
	opClass
	opSplit
	opJmp
	opSave
	opAssertStart
	opAssertEnd
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
// group count (excluding group 0, the whole match).
type regexProgram struct {
	insts   []regexInst
	classes []regexClass
	ngroups int
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
	return NewError(InvalidRegularExpression, "invalid regular expression: "+detail)
}

func regexTooComplex() error {
	return NewError(StatementTooComplex,
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
		return &regexNode{kind: nAny}, nil
	case '^':
		p.bump()
		return &regexNode{kind: nAnchorStart}, nil
	case '$':
		p.bump()
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

// compileRegex compiles a pattern to a program (regex.md §3). Raises 2201B on a malformed pattern
// and 54001 on a well-formed-but-too-large one. Does NOT meter — the caller charges
// regex_compile × program.ninst() (the precompilation contract, regex.md §5). For ~* the pattern
// must already be case-folded by the caller.
func compileRegex(pattern string) (*regexProgram, error) {
	p := &regexParser{chars: []rune(pattern)}
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
	if _, err := c.push(regexInst{op: opAny}); err != nil {
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
	return &regexProgram{insts: c.insts, classes: c.classes, ngroups: p.ngroups}, nil
}

// ---------------------------------------------------------------------------
// Pike VM (regex.md §4)
// ---------------------------------------------------------------------------

type regexThread struct {
	pc    int
	saves []int64
}

// isMatch reports whether the pattern matches somewhere in input (the `~` operator).
func (p *regexProgram) isMatch(input []rune, m *Meter) (bool, error) {
	caps, err := p.run(input, m)
	return caps != nil, err
}

// run executes the Pike VM from the start of the input (regex.md §4). Returns the winning thread's
// capture slots on a match (code-point offsets; -1 = unset), or nil.
func (p *regexProgram) run(input []rune, m *Meter) ([]int64, error) {
	return p.search(input, 0, m)
}

// search runs the Pike VM, considering only matches that START at code-point position `start` or
// later (the unanchored search seeds its lazy `.*?` prefix at `start`); ^/$ still anchor at the true
// input bounds. Used by regexp_replace's global loop. Charges regex_step per explored state and
// guards once per input position.
func (p *regexProgram) search(input []rune, start int, m *Meter) ([]int64, error) {
	nslots := 2 * (p.ngroups + 1)
	length := len(input)
	seen := make([]uint32, len(p.insts))
	var generation uint32
	clist := []regexThread{}
	nlist := []regexThread{}
	var matched []int64

	generation++
	initSaves := make([]int64, nslots)
	for i := range initSaves {
		initSaves[i] = -1
	}
	if err := p.addThread(&clist, seen, generation, 0, initSaves, start, length, m); err != nil {
		return nil, err
	}

	for sp := start; sp <= length; sp++ {
		generation++
		nlist = nlist[:0]
	inner:
		for i := 0; i < len(clist); i++ {
			th := clist[i]
			in := p.insts[th.pc]
			switch in.op {
			case opChar:
				if sp < length && input[sp] == in.ch {
					if err := p.addThread(&nlist, seen, generation, th.pc+1, th.saves, sp+1, length, m); err != nil {
						return nil, err
					}
				}
			case opAny:
				if sp < length && input[sp] != '\n' {
					if err := p.addThread(&nlist, seen, generation, th.pc+1, th.saves, sp+1, length, m); err != nil {
						return nil, err
					}
				}
			case opClass:
				if sp < length && p.classes[in.x].admits(input[sp]) {
					if err := p.addThread(&nlist, seen, generation, th.pc+1, th.saves, sp+1, length, m); err != nil {
						return nil, err
					}
				}
			case opMatch:
				matched = th.saves
				break inner // cut lower-priority threads (leftmost-first, regex.md §4)
			}
		}
		clist, nlist = nlist, clist
		if err := m.Guard(); err != nil { // §6 ceiling, once per input position
			return nil, err
		}
		if len(clist) == 0 {
			break
		}
	}
	return matched, nil
}

// addThread performs the epsilon-closure: follow Jmp/Split/Save/Assert from pc, appending
// consuming/Match threads to *list, deduping by pc within this generation. Iterative (explicit
// stack) so a long Jmp/Split chain cannot overflow the native stack; the y arm of a Split is pushed
// before x so x is processed first (higher priority). Charges regex_step per explored state.
func (p *regexProgram) addThread(list *[]regexThread, seen []uint32, generation uint32, pc0 int, saves0 []int64, sp, length int, m *Meter) error {
	type frame struct {
		pc    int
		saves []int64
	}
	stack := []frame{{pc0, saves0}}
	for len(stack) > 0 {
		top := stack[len(stack)-1]
		stack = stack[:len(stack)-1]
		if seen[top.pc] == generation {
			continue
		}
		seen[top.pc] = generation
		m.Charge(Costs.RegexStep)
		in := p.insts[top.pc]
		switch in.op {
		case opJmp:
			stack = append(stack, frame{in.x, top.saves})
		case opSplit:
			// Push y first, then x, so x pops first = higher priority.
			stack = append(stack, frame{in.y, top.saves})
			stack = append(stack, frame{in.x, top.saves})
		case opSave:
			s := make([]int64, len(top.saves))
			copy(s, top.saves)
			s[in.x] = int64(sp)
			stack = append(stack, frame{top.pc + 1, s})
		case opAssertStart:
			if sp == 0 {
				stack = append(stack, frame{top.pc + 1, top.saves})
			}
		case opAssertEnd:
			if sp == length {
				stack = append(stack, frame{top.pc + 1, top.saves})
			}
		default:
			// Char / Any / Class / Match — parked for the consume loop.
			*list = append(*list, regexThread{pc: top.pc, saves: top.saves})
		}
	}
	return nil
}

// regexpMatch is regexp_match(source, …) capture extraction (regex.md §8). Searches once; on a match
// returns the capture group strings (groups 1..n, or a 1-element whole-match list when the pattern
// has no group — the PG rule), an unset group being a nil pointer. Returns ok=false on no match.
// matchInput is the (possibly case-folded) subject the VM matches; origInput is the ORIGINAL-case
// subject the returned substrings are sliced from (same length).
func (p *regexProgram) regexpMatch(matchInput, origInput []rune, m *Meter) (groups []*string, ok bool, err error) {
	saves, err := p.search(matchInput, 0, m)
	if err != nil {
		return nil, false, err
	}
	if saves == nil {
		return nil, false, nil
	}
	if p.ngroups == 0 {
		return []*string{sliceGroup(origInput, saves[0], saves[1])}, true, nil
	}
	groups = make([]*string, p.ngroups)
	for g := 1; g <= p.ngroups; g++ {
		groups[g-1] = sliceGroup(origInput, saves[2*g], saves[2*g+1])
	}
	return groups, true, nil
}

// regexpReplace is regexp_replace(source, pattern, replacement, …) (regex.md §8). Replaces the first
// match (or all when global) by the replacement TEMPLATE (\1..\9 = capture group, \& = whole match,
// \\ = literal backslash). Non-matched text and captured substrings come from origInput (original
// case); the VM matches over matchInput (possibly case-folded).
func (p *regexProgram) regexpReplace(matchInput, origInput, replacement []rune, global bool, m *Meter) (string, error) {
	var out []rune
	pos := 0
	for {
		saves, err := p.search(matchInput, pos, m)
		if err != nil {
			return "", err
		}
		if saves == nil {
			break
		}
		s := int(saves[0])
		e := int(saves[1])
		out = append(out, origInput[pos:s]...)
		out = spliceReplacement(out, replacement, saves, origInput)
		if !global {
			out = append(out, origInput[e:]...)
			return string(out), nil
		}
		if e > s {
			pos = e
		} else {
			// Empty match: emit the char at `e` (if any) and advance past it (the PG global rule).
			if e < len(origInput) {
				out = append(out, origInput[e])
			}
			pos = e + 1
		}
		if pos > len(origInput) {
			return string(out), nil
		}
	}
	out = append(out, origInput[pos:]...)
	return string(out), nil
}

// regexpCount counts the non-overlapping matches at or after code-point position `start`
// (regexp_count, regex.md §8b). The advance is regexpReplace's global rule: after a match [s,e)
// continue at e, or at e+1 for an EMPTY match so a nullable pattern terminates. `start` may be up to
// len (an empty match at the very end still counts); start > len (clamped to len+1 by the caller)
// yields 0.
func (p *regexProgram) regexpCount(input []rune, start int, m *Meter) (int64, error) {
	length := len(input)
	pos := start
	var count int64
	for pos <= length {
		saves, err := p.search(input, pos, m)
		if err != nil {
			return 0, err
		}
		if saves == nil {
			break
		}
		count++
		s, e := int(saves[0]), int(saves[1])
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
func (p *regexProgram) nthMatch(input []rune, start int, n int64, m *Meter) ([]int64, error) {
	length := len(input)
	pos := start
	var count int64
	for pos <= length {
		saves, err := p.search(input, pos, m)
		if err != nil {
			return nil, err
		}
		if saves == nil {
			break
		}
		count++
		if count == n {
			return saves, nil
		}
		s, e := int(saves[0]), int(saves[1])
		if e > s {
			pos = e
		} else {
			pos = e + 1
		}
	}
	return nil, nil
}

// sliceGroup slices orig[start:end] to a string pointer, or nil for an unset (-1) group.
func sliceGroup(orig []rune, start, end int64) *string {
	if start < 0 || end < 0 {
		return nil
	}
	s := string(orig[start:end])
	return &s
}

// spliceReplacement appends a replacement template to out, expanding \1..\9 (capture group), \&
// (whole match), \\ (literal backslash), and \<other> (the literal <other>). A trailing lone \ is
// literal.
func spliceReplacement(out, repl []rune, saves []int64, orig []rune) []rune {
	for i := 0; i < len(repl); i++ {
		c := repl[i]
		if c == '\\' && i+1 < len(repl) {
			n := repl[i+1]
			switch {
			case n >= '0' && n <= '9':
				g := int(n - '0')
				if 2*g+1 < len(saves) {
					if grp := sliceGroup(orig, saves[2*g], saves[2*g+1]); grp != nil {
						out = append(out, []rune(*grp)...)
					}
				}
			case n == '&':
				if grp := sliceGroup(orig, saves[0], saves[1]); grp != nil {
					out = append(out, []rune(*grp)...)
				}
			default:
				out = append(out, n) // \\ -> \, and \<other> -> <other>
			}
			i++
		} else {
			out = append(out, c)
		}
	}
	return out
}
