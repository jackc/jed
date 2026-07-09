//! Regular-expression engine — a hand-written RE2-style **Pike VM** (spec/design/regex.md).
//!
//! jed's own RE2-able regex flavor (NOT PostgreSQL-compatible): a pattern compiles to a flat NFA
//! bytecode program (the `Program` below), which a thread-list simulation matches over the input in
//! **linear time with no backtracking** — immune to catastrophic-backtracking (ReDoS) attacks
//! independent of the cost meter (CLAUDE.md §13). Both the compilation and the per-step cost are
//! part of the **cross-core contract** (no reference impl, §2): all three cores emit the byte-
//! identical program (spec/regex/program_vectors.toml) and accrue identical `regex_compile` /
//! `regex_step` cost (spec/regex/match_vectors.toml). The lowering here follows regex.md §3 exactly.

use crate::cost::Meter;
use crate::costs::COSTS;
use crate::error::{EngineError, Result};
use crate::sqlstate::SqlState;
use std::rc::Rc;

/// Maximum compiled-program size, in instructions (regex.md §6, cost.md §7c). A fixed cross-core
/// constant — a pattern whose program would exceed it aborts `54001` at compile (the structural cap
/// that protects an unlimited handle, where the `regex_compile` cost ceiling cannot reach).
pub const MAX_REGEX_PROGRAM: usize = 32768;

/// The largest Unicode scalar value (for class complement). Input chars are valid scalars, so the
/// surrogate gap D800..DFFF never matters at match time.
const MAX_CP: u32 = 0x10FFFF;

// ---------------------------------------------------------------------------
// Bytecode
// ---------------------------------------------------------------------------

/// One NFA instruction. Jump targets are absolute instruction indices (regex.md §3.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inst {
    Char(char),
    Any,
    Class(usize), // index into `Program::classes`
    Split(usize, usize),
    Jmp(usize),
    Save(usize),
    AssertStart,
    AssertEnd,
    Match,
}

/// A character class: positive, sorted, merged code-point ranges plus a `negated` flag applied at
/// match time (regex.md §3.4 — never by complementing the range list).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CharClass {
    pub negated: bool,
    pub ranges: Vec<(u32, u32)>,
}

impl CharClass {
    fn admits(&self, c: char) -> bool {
        let cp = c as u32;
        let inside = self.ranges.iter().any(|&(lo, hi)| cp >= lo && cp <= hi);
        inside != self.negated
    }
}

/// A compiled pattern: the instruction array, the class table, and the capturing-group count
/// (excluding group 0, the whole match).
#[derive(Clone, Debug)]
pub struct Program {
    pub insts: Vec<Inst>,
    pub classes: Vec<CharClass>,
    pub ngroups: usize,
}

impl Program {
    /// The instruction count = the `regex_compile` cost (one unit per emitted instruction).
    pub fn ninst(&self) -> usize {
        self.insts.len()
    }

    /// Canonical instruction listing (the `program_vectors.toml` contract, regex.md §9). One string
    /// per instruction, in emission order; `classes()` renders the class table in parallel.
    pub fn listing(&self) -> Vec<String> {
        self.insts
            .iter()
            .map(|i| match i {
                Inst::Char(c) => format!("char {}", *c as u32),
                Inst::Any => "any".to_string(),
                Inst::Class(k) => format!("class {k}"),
                Inst::Split(a, b) => format!("split {a} {b}"),
                Inst::Jmp(a) => format!("jmp {a}"),
                Inst::Save(n) => format!("save {n}"),
                Inst::AssertStart => "assertstart".to_string(),
                Inst::AssertEnd => "assertend".to_string(),
                Inst::Match => "match".to_string(),
            })
            .collect()
    }

    /// Canonical class listing (`program_vectors.toml`'s `classes`): `lo-hi` ranges joined by `,`,
    /// prefixed `^` when negated.
    pub fn class_listing(&self) -> Vec<String> {
        self.classes
            .iter()
            .map(|c| {
                let body = c
                    .ranges
                    .iter()
                    .map(|&(lo, hi)| format!("{lo}-{hi}"))
                    .collect::<Vec<_>>()
                    .join(",");
                if c.negated { format!("^{body}") } else { body }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn invalid(detail: &str) -> EngineError {
    EngineError::new(
        SqlState::InvalidRegularExpression,
        format!("invalid regular expression: {detail}"),
    )
}

fn too_complex() -> EngineError {
    EngineError::new(
        SqlState::StatementTooComplex,
        format!("regular expression compiles to more than {MAX_REGEX_PROGRAM} instructions"),
    )
}

// ---------------------------------------------------------------------------
// Pattern AST
// ---------------------------------------------------------------------------

enum Node {
    Empty,
    Char(char),
    Any,
    Class(CharClass),
    Concat(Vec<Node>),
    /// Right-folded binary alternation (`a|b|c` == `a|(b|c)`), regex.md §3.3.
    Alt(Box<Node>, Box<Node>),
    Star(Box<Node>, bool), // greedy
    Plus(Box<Node>, bool),
    Quest(Box<Node>, bool),
    /// `{min,max}` — `max == None` is `{min,}`. Unrolled at emit (regex.md §3.3).
    Repeat(Box<Node>, usize, Option<usize>, bool),
    /// `Some(index)` = capturing group (1-based), `None` = non-capturing.
    Group(Box<Node>, Option<usize>),
    AnchorStart,
    AnchorEnd,
}

// ---------------------------------------------------------------------------
// Parser: pattern text -> AST
// ---------------------------------------------------------------------------

struct Parser {
    chars: Vec<char>,
    pos: usize,
    ngroups: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }
    fn peek_at(&self, k: usize) -> Option<char> {
        self.chars.get(self.pos + k).copied()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// alternation (loosest): concat ('|' concat)*, right-folded.
    fn parse_alt(&mut self) -> Result<Node> {
        let left = self.parse_concat()?;
        if self.peek() == Some('|') {
            self.bump();
            let right = self.parse_alt()?;
            Ok(Node::Alt(Box::new(left), Box::new(right)))
        } else {
            Ok(left)
        }
    }

    /// concatenation: a run of quantified atoms until `|`, `)`, or end.
    fn parse_concat(&mut self) -> Result<Node> {
        let mut nodes = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            nodes.push(self.parse_quant()?);
        }
        Ok(match nodes.len() {
            0 => Node::Empty,
            1 => nodes.pop().unwrap(),
            _ => Node::Concat(nodes),
        })
    }

    /// atom followed by at most one quantifier (`* + ? {…}`), with an optional lazy `?`.
    fn parse_quant(&mut self) -> Result<Node> {
        let atom = self.parse_atom()?;
        let quant = match self.peek() {
            Some('*') => {
                self.bump();
                Some(Quant::Star)
            }
            Some('+') => {
                self.bump();
                Some(Quant::Plus)
            }
            Some('?') => {
                self.bump();
                Some(Quant::Quest)
            }
            Some('{') => self.try_interval()?,
            _ => None,
        };
        let Some(quant) = quant else {
            return Ok(atom);
        };
        // An optional trailing `?` makes the quantifier lazy (`*?`, `+?`, `??`, `{n,m}?`).
        let greedy = if self.peek() == Some('?') {
            self.bump();
            false
        } else {
            true
        };
        // A second quantifier with no atom between (`a**`, `a*+`) is invalid (regex.md §2).
        if matches!(self.peek(), Some('*') | Some('+') | Some('?')) {
            return Err(invalid("quantifier operand missing"));
        }
        Ok(match quant {
            Quant::Star => Node::Star(Box::new(atom), greedy),
            Quant::Plus => Node::Plus(Box::new(atom), greedy),
            Quant::Quest => Node::Quest(Box::new(atom), greedy),
            Quant::Repeat(min, max) => Node::Repeat(Box::new(atom), min, max, greedy),
        })
    }

    /// Try to read a `{n}`, `{n,}`, or `{n,m}` interval at the cursor. On a non-interval `{` the
    /// cursor is **unmoved** and `None` returned, so the `{` is later read as a literal (the PCRE
    /// lenient-brace rule, regex.md §2). `{n,m}` with `m < n` is `2201B`.
    fn try_interval(&mut self) -> Result<Option<Quant>> {
        debug_assert_eq!(self.peek(), Some('{'));
        let start = self.pos;
        self.bump(); // '{'
        let Some(min) = self.read_count() else {
            self.pos = start;
            return Ok(None);
        };
        let max;
        match self.peek() {
            Some('}') => {
                self.bump();
                max = Some(min); // {n}
            }
            Some(',') => {
                self.bump();
                if self.peek() == Some('}') {
                    self.bump();
                    max = None; // {n,}
                } else if let Some(hi) = self.read_count() {
                    if self.peek() == Some('}') {
                        self.bump();
                        if hi < min {
                            return Err(invalid("invalid repetition count"));
                        }
                        max = Some(hi); // {n,m}
                    } else {
                        self.pos = start;
                        return Ok(None);
                    }
                } else {
                    self.pos = start;
                    return Ok(None);
                }
            }
            _ => {
                self.pos = start;
                return Ok(None);
            }
        }
        Ok(Some(Quant::Repeat(min, max)))
    }

    /// Read a run of ASCII digits as a count, saturating at `MAX_REGEX_PROGRAM + 1` so a giant
    /// interval (`{99999999999}`) cannot overflow and is rejected `54001` at emit. Returns `None`
    /// if no digit is present.
    fn read_count(&mut self) -> Option<usize> {
        let mut any = false;
        let mut n: usize = 0;
        while let Some(c) = self.peek() {
            if !c.is_ascii_digit() {
                break;
            }
            any = true;
            self.bump();
            n = n
                .saturating_mul(10)
                .saturating_add((c as usize) - ('0' as usize));
            if n > MAX_REGEX_PROGRAM {
                n = MAX_REGEX_PROGRAM + 1;
            }
        }
        if any { Some(n) } else { None }
    }

    fn parse_atom(&mut self) -> Result<Node> {
        let c = self
            .peek()
            .expect("parse_atom called at end (concat guards)");
        match c {
            '(' => self.parse_group(),
            '[' => self.parse_class(),
            '.' => {
                self.bump();
                Ok(Node::Any)
            }
            '^' => {
                self.bump();
                Ok(Node::AnchorStart)
            }
            '$' => {
                self.bump();
                Ok(Node::AnchorEnd)
            }
            '\\' => {
                self.bump();
                self.parse_escape()
            }
            // A quantifier where an atom is expected (`*ab`, `a|*`) is invalid (regex.md §2).
            '*' | '+' | '?' => Err(invalid("quantifier operand missing")),
            // `{`, `}`, `]` are literals here — a `{` starting a valid interval is consumed by
            // parse_quant before reaching parse_atom (the lenient-brace rule, regex.md §2).
            _ => {
                self.bump();
                Ok(Node::Char(c))
            }
        }
    }

    fn parse_group(&mut self) -> Result<Node> {
        self.bump(); // '('
        let capturing = if self.peek() == Some('?') {
            // `(?:...)` is non-capturing; any other `(?...)` is an excluded construct (regex.md §2).
            if self.peek_at(1) == Some(':') {
                self.bump();
                self.bump();
                false
            } else {
                return Err(invalid("unsupported group syntax"));
            }
        } else {
            true
        };
        let index = if capturing {
            self.ngroups += 1;
            Some(self.ngroups)
        } else {
            None
        };
        let inner = self.parse_alt()?;
        if self.peek() != Some(')') {
            return Err(invalid("unbalanced parenthesis"));
        }
        self.bump(); // ')'
        Ok(Node::Group(Box::new(inner), index))
    }

    /// Parse an escape after a consumed `\` at the top level (not inside a class).
    fn parse_escape(&mut self) -> Result<Node> {
        let Some(c) = self.bump() else {
            return Err(invalid("trailing backslash"));
        };
        // String-boundary anchors (regex.md §2): `\A` = start of subject, `\z` = absolute end of
        // subject. jed has no multiline mode, so these are exactly `^`/`$` today; they are spelled
        // separately so they stay string-anchored once a line mode lands. PCRE's `\Z` (end, or
        // before a trailing newline) is deliberately NOT accepted — its only distinguishing
        // behavior is trailing-newline leniency jed does nowhere, so it stays an invalid escape.
        match c {
            'A' => return Ok(Node::AnchorStart),
            'z' => return Ok(Node::AnchorEnd),
            _ => {}
        }
        if let Some((ranges, negated)) = predef_class(c) {
            return Ok(Node::Class(CharClass { negated, ranges }));
        }
        if let Some(ctrl) = control_escape(c) {
            return Ok(Node::Char(ctrl));
        }
        if is_meta(c) {
            return Ok(Node::Char(c));
        }
        Err(invalid(&format!("invalid escape \\{c}")))
    }

    fn parse_class(&mut self) -> Result<Node> {
        self.bump(); // '['
        let negated = if self.peek() == Some('^') {
            self.bump();
            true
        } else {
            false
        };
        let mut ranges: Vec<(u32, u32)> = Vec::new();
        let mut first = true;
        loop {
            match self.peek() {
                None => return Err(invalid("unbalanced bracket expression")),
                Some(']') if !first => {
                    self.bump();
                    break;
                }
                _ => {}
            }
            // Parse one class item: a predefined-class set, or a single code point (possibly the
            // low end of a range).
            match self.class_item()? {
                ClassItem::Set(rs) => ranges.extend(rs),
                ClassItem::Char(lo) => {
                    // `lo-hi` is a range only when `-` is followed by a real high end (not `]`).
                    if self.peek() == Some('-')
                        && self.peek_at(1).is_some()
                        && self.peek_at(1) != Some(']')
                    {
                        self.bump(); // '-'
                        match self.class_item()? {
                            ClassItem::Char(hi) => {
                                if lo > hi {
                                    return Err(invalid("invalid range in bracket expression"));
                                }
                                ranges.push((lo as u32, hi as u32));
                            }
                            // `[\d-a]` etc. — a class endpoint on a range is lenient: the `-` is a
                            // literal and the predefined set is added (regex.md §2).
                            ClassItem::Set(rs) => {
                                ranges.push((lo as u32, lo as u32));
                                ranges.push(('-' as u32, '-' as u32));
                                ranges.extend(rs);
                            }
                        }
                    } else {
                        ranges.push((lo as u32, lo as u32));
                    }
                }
            }
            first = false;
        }
        Ok(Node::Class(CharClass {
            negated,
            ranges: normalize_ranges(ranges),
        }))
    }

    /// One item inside a `[...]`: a predefined class (`\d` …) becomes a `Set`; anything else a
    /// single `Char` (escapes resolved). `^` is only special as the very first char (handled by the
    /// caller), so here it is a literal.
    fn class_item(&mut self) -> Result<ClassItem> {
        let c = self
            .bump()
            .expect("class_item called at end (loop guards None)");
        if c != '\\' {
            return Ok(ClassItem::Char(c));
        }
        let Some(e) = self.bump() else {
            return Err(invalid("trailing backslash"));
        };
        if let Some((rs, negated)) = predef_class(e) {
            // A negated predefined class inside `[...]` expands to its complement as positive
            // ranges (the outer class keeps a single `negated` flag, regex.md §3.4).
            return Ok(ClassItem::Set(if negated {
                complement_ranges(&normalize_ranges(rs))
            } else {
                rs
            }));
        }
        if let Some(ctrl) = control_escape(e) {
            return Ok(ClassItem::Char(ctrl));
        }
        if is_meta(e) || e == '-' || e == ']' {
            return Ok(ClassItem::Char(e));
        }
        Err(invalid(&format!("invalid escape \\{e}")))
    }
}

enum Quant {
    Star,
    Plus,
    Quest,
    Repeat(usize, Option<usize>),
}

enum ClassItem {
    Char(char),
    Set(Vec<(u32, u32)>),
}

fn is_meta(c: char) -> bool {
    matches!(
        c,
        '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\'
    )
}

fn control_escape(c: char) -> Option<char> {
    Some(match c {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        'f' => '\u{000C}',
        'v' => '\u{000B}',
        _ => return None,
    })
}

/// The predefined classes `\d \w \s` (and their negations): positive ranges plus whether the letter
/// was the negated (uppercase) form. ASCII baseline for Slice 1 (Unicode-property classes deferred).
fn predef_class(c: char) -> Option<(Vec<(u32, u32)>, bool)> {
    let (ranges, negated): (Vec<(u32, u32)>, bool) = match c {
        'd' => (vec![(48, 57)], false),
        'D' => (vec![(48, 57)], true),
        'w' => (vec![(48, 57), (65, 90), (95, 95), (97, 122)], false),
        'W' => (vec![(48, 57), (65, 90), (95, 95), (97, 122)], true),
        's' => (vec![(9, 13), (32, 32)], false),
        'S' => (vec![(9, 13), (32, 32)], true),
        _ => return None,
    };
    Some((ranges, negated))
}

/// Sort by `lo` and merge touching/overlapping ranges (regex.md §3.4).
fn normalize_ranges(mut ranges: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    ranges.sort_unstable();
    let mut out: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
    for (lo, hi) in ranges {
        if let Some(last) = out.last_mut() {
            // Merge when overlapping or adjacent (`last.1 + 1 >= lo`, saturating at MAX_CP).
            if lo <= last.1.saturating_add(1) {
                if hi > last.1 {
                    last.1 = hi;
                }
                continue;
            }
        }
        out.push((lo, hi));
    }
    out
}

/// Complement of normalized ranges over `[0, MAX_CP]`.
fn complement_ranges(ranges: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut next = 0u32;
    for &(lo, hi) in ranges {
        if lo > next {
            out.push((next, lo - 1));
        }
        next = hi.saturating_add(1);
        if next > MAX_CP {
            return out;
        }
    }
    if next <= MAX_CP {
        out.push((next, MAX_CP));
    }
    out
}

// ---------------------------------------------------------------------------
// Compiler: AST -> bytecode (the exact emission of regex.md §3)
// ---------------------------------------------------------------------------

struct Compiler {
    insts: Vec<Inst>,
    classes: Vec<CharClass>,
}

impl Compiler {
    fn push(&mut self, inst: Inst) -> Result<usize> {
        if self.insts.len() >= MAX_REGEX_PROGRAM {
            return Err(too_complex());
        }
        let i = self.insts.len();
        self.insts.push(inst);
        Ok(i)
    }

    fn emit(&mut self, node: &Node) -> Result<()> {
        match node {
            Node::Empty => Ok(()),
            Node::Char(c) => {
                self.push(Inst::Char(*c))?;
                Ok(())
            }
            Node::Any => {
                self.push(Inst::Any)?;
                Ok(())
            }
            Node::Class(cc) => {
                let k = self.classes.len();
                self.classes.push(cc.clone());
                self.push(Inst::Class(k))?;
                Ok(())
            }
            Node::Concat(nodes) => {
                for n in nodes {
                    self.emit(n)?;
                }
                Ok(())
            }
            Node::Alt(a, b) => {
                // Split LX, LY ; LX: <a>; Jmp LEND ; LY: <b>; LEND:
                let split = self.push(Inst::Split(0, 0))?;
                let lx = self.insts.len();
                self.emit(a)?;
                let jmp = self.push(Inst::Jmp(0))?;
                let ly = self.insts.len();
                self.emit(b)?;
                let lend = self.insts.len();
                self.insts[split] = Inst::Split(lx, ly);
                self.insts[jmp] = Inst::Jmp(lend);
                Ok(())
            }
            Node::Star(sub, greedy) => {
                // L1: Split L2,L3 (greedy) / Split L3,L2 (lazy) ; L2: <sub>; Jmp L1 ; L3:
                let l1 = self.push(Inst::Split(0, 0))?;
                let l2 = self.insts.len();
                self.emit(sub)?;
                self.push(Inst::Jmp(l1))?;
                let l3 = self.insts.len();
                self.insts[l1] = if *greedy {
                    Inst::Split(l2, l3)
                } else {
                    Inst::Split(l3, l2)
                };
                Ok(())
            }
            Node::Plus(sub, greedy) => {
                // L1: <sub>; Split L1,L3 (greedy) / Split L3,L1 (lazy) ; L3:
                let l1 = self.insts.len();
                self.emit(sub)?;
                let split = self.push(Inst::Split(0, 0))?;
                let l3 = self.insts.len();
                self.insts[split] = if *greedy {
                    Inst::Split(l1, l3)
                } else {
                    Inst::Split(l3, l1)
                };
                Ok(())
            }
            Node::Quest(sub, greedy) => {
                // Split L1,L2 (greedy) / Split L2,L1 (lazy) ; L1: <sub>; L2:
                let split = self.push(Inst::Split(0, 0))?;
                let l1 = self.insts.len();
                self.emit(sub)?;
                let l2 = self.insts.len();
                self.insts[split] = if *greedy {
                    Inst::Split(l1, l2)
                } else {
                    Inst::Split(l2, l1)
                };
                Ok(())
            }
            Node::Repeat(sub, min, max, greedy) => self.emit_repeat(sub, *min, *max, *greedy),
            Node::Group(sub, index) => {
                if let Some(i) = index {
                    self.push(Inst::Save(2 * i))?;
                    self.emit(sub)?;
                    self.push(Inst::Save(2 * i + 1))?;
                } else {
                    self.emit(sub)?;
                }
                Ok(())
            }
            Node::AnchorStart => {
                self.push(Inst::AssertStart)?;
                Ok(())
            }
            Node::AnchorEnd => {
                self.push(Inst::AssertEnd)?;
                Ok(())
            }
        }
    }

    /// `{min,max}` -> `min` mandatory copies, then either a `Star` (`{min,}`) or `(max-min)`
    /// greedy/lazy `Quest` copies. Each copy's emit checks the `MAX_REGEX_PROGRAM` cap, so a giant
    /// interval aborts `54001` after at most that many instructions (regex.md §3.3, §6).
    fn emit_repeat(
        &mut self,
        sub: &Node,
        min: usize,
        max: Option<usize>,
        greedy: bool,
    ) -> Result<()> {
        for _ in 0..min {
            self.emit(sub)?;
        }
        match max {
            None => self.emit(&clone_star(sub, greedy)),
            Some(m) => {
                for _ in 0..(m - min) {
                    self.emit(&clone_quest(sub, greedy))?;
                }
                Ok(())
            }
        }
    }
}

/// Re-wrap a borrowed sub-node as a fresh `Star`/`Quest` for the unroll tail. The AST is small and
/// rebuilt cheaply; cloning a `Node` avoids threading lifetimes through `emit_repeat`.
fn clone_star(sub: &Node, greedy: bool) -> Node {
    Node::Star(Box::new(clone_node(sub)), greedy)
}
fn clone_quest(sub: &Node, greedy: bool) -> Node {
    Node::Quest(Box::new(clone_node(sub)), greedy)
}
fn clone_node(n: &Node) -> Node {
    match n {
        Node::Empty => Node::Empty,
        Node::Char(c) => Node::Char(*c),
        Node::Any => Node::Any,
        Node::Class(cc) => Node::Class(cc.clone()),
        Node::Concat(v) => Node::Concat(v.iter().map(clone_node).collect()),
        Node::Alt(a, b) => Node::Alt(Box::new(clone_node(a)), Box::new(clone_node(b))),
        Node::Star(s, g) => Node::Star(Box::new(clone_node(s)), *g),
        Node::Plus(s, g) => Node::Plus(Box::new(clone_node(s)), *g),
        Node::Quest(s, g) => Node::Quest(Box::new(clone_node(s)), *g),
        Node::Repeat(s, lo, hi, g) => Node::Repeat(Box::new(clone_node(s)), *lo, *hi, *g),
        Node::Group(s, i) => Node::Group(Box::new(clone_node(s)), *i),
        Node::AnchorStart => Node::AnchorStart,
        Node::AnchorEnd => Node::AnchorEnd,
    }
}

/// Compile a pattern to a program (regex.md §3). Raises `2201B` on a malformed pattern and `54001`
/// on a well-formed-but-too-large one. Does NOT meter — the caller charges `regex_compile ×
/// program.ninst()` (the precompilation contract, regex.md §5). For `~*` the pattern must already be
/// case-folded by the caller.
pub fn compile(pattern: &str) -> Result<Program> {
    let mut parser = Parser {
        chars: pattern.chars().collect(),
        pos: 0,
        ngroups: 0,
    };
    let root = parser.parse_alt()?;
    if parser.pos != parser.chars.len() {
        // A leftover `)` (or anything parse_alt stopped on) is an unbalanced parenthesis.
        return Err(invalid("unbalanced parenthesis"));
    }
    let ngroups = parser.ngroups;

    let mut c = Compiler {
        insts: Vec::new(),
        classes: Vec::new(),
    };
    // Wrapper (regex.md §3.2): lazy `.*?` prefix (unanchored, leftmost) + group-0 save + Match.
    c.push(Inst::Split(3, 1))?;
    c.push(Inst::Any)?;
    c.push(Inst::Jmp(0))?;
    c.push(Inst::Save(0))?;
    c.emit(&root)?;
    c.push(Inst::Save(1))?;
    c.push(Inst::Match)?;
    Ok(Program {
        insts: c.insts,
        classes: c.classes,
        ngroups,
    })
}

// ---------------------------------------------------------------------------
// Pike VM (regex.md §4)
// ---------------------------------------------------------------------------

struct Thread {
    pc: usize,
    saves: Rc<Vec<i64>>,
}

impl Program {
    /// Boolean match — TRUE iff the pattern matches somewhere in `input` (the `~` operator).
    pub fn is_match(&self, input: &[char], m: &mut Meter) -> Result<bool> {
        Ok(self.run(input, m)?.is_some())
    }

    /// Run the Pike VM from the start of the input (regex.md §4). Returns the winning thread's
    /// capture slots on a match (code-point offsets; `-1` = unset), or `None`.
    pub fn run(&self, input: &[char], m: &mut Meter) -> Result<Option<Vec<i64>>> {
        self.search(input, 0, m)
    }

    /// Run the Pike VM, considering only matches that START at code-point position `start` or later
    /// (the unanchored search seeds its lazy `.*?` prefix at `start`). `^`/`$` still anchor at the
    /// true input bounds (absolute `sp == 0` / `sp == len`). Used by `regexp_replace`'s global loop.
    /// Charges `regex_step` per explored state and guards once per input position.
    // `sp` is a logical input POSITION that must reach `len` (one past the end) so the final
    // epsilon-closure can fire `AssertEnd`/`Match` — enumerate() cannot express `start..=len`.
    #[allow(clippy::needless_range_loop)]
    pub fn search(&self, input: &[char], start: usize, m: &mut Meter) -> Result<Option<Vec<i64>>> {
        let nslots = 2 * (self.ngroups + 1);
        let len = input.len();
        let mut seen = vec![0u32; self.insts.len()];
        let mut generation: u32 = 0;
        let mut clist: Vec<Thread> = Vec::new();
        let mut nlist: Vec<Thread> = Vec::new();
        let mut matched: Option<Vec<i64>> = None;

        generation += 1;
        self.add_thread(
            &mut clist,
            &mut seen,
            generation,
            0,
            Rc::new(vec![-1; nslots]),
            start,
            len,
            m,
        )?;

        for sp in start..=len {
            generation += 1;
            nlist.clear();
            let mut i = 0;
            while i < clist.len() {
                let pc = clist[i].pc;
                match &self.insts[pc] {
                    Inst::Char(c) => {
                        if sp < len && input[sp] == *c {
                            let saves = Rc::clone(&clist[i].saves);
                            self.add_thread(
                                &mut nlist,
                                &mut seen,
                                generation,
                                pc + 1,
                                saves,
                                sp + 1,
                                len,
                                m,
                            )?;
                        }
                    }
                    Inst::Any => {
                        if sp < len && input[sp] != '\n' {
                            let saves = Rc::clone(&clist[i].saves);
                            self.add_thread(
                                &mut nlist,
                                &mut seen,
                                generation,
                                pc + 1,
                                saves,
                                sp + 1,
                                len,
                                m,
                            )?;
                        }
                    }
                    Inst::Class(k) => {
                        if sp < len && self.classes[*k].admits(input[sp]) {
                            let saves = Rc::clone(&clist[i].saves);
                            self.add_thread(
                                &mut nlist,
                                &mut seen,
                                generation,
                                pc + 1,
                                saves,
                                sp + 1,
                                len,
                                m,
                            )?;
                        }
                    }
                    Inst::Match => {
                        matched = Some((*clist[i].saves).clone());
                        break; // cut lower-priority threads (leftmost-first, regex.md §4)
                    }
                    _ => unreachable!("epsilon instructions are resolved inside add_thread"),
                }
                i += 1;
            }
            std::mem::swap(&mut clist, &mut nlist);
            m.guard()?; // §6 ceiling, once per input position
            if clist.is_empty() {
                break;
            }
        }
        Ok(matched)
    }

    /// Epsilon-closure: follow Jmp/Split/Save/Assert from `pc`, appending consuming/Match threads to
    /// `list`, deduping by pc within this generation. Iterative (explicit stack) so a long
    /// Jmp/Split chain cannot overflow the native stack; the `y` arm of a Split is pushed before
    /// `x` so `x` is processed first (higher priority). Charges `regex_step` per explored state.
    #[allow(clippy::too_many_arguments)]
    fn add_thread(
        &self,
        list: &mut Vec<Thread>,
        seen: &mut [u32],
        generation: u32,
        pc0: usize,
        saves0: Rc<Vec<i64>>,
        sp: usize,
        len: usize,
        m: &mut Meter,
    ) -> Result<()> {
        let mut stack: Vec<(usize, Rc<Vec<i64>>)> = vec![(pc0, saves0)];
        while let Some((pc, saves)) = stack.pop() {
            if seen[pc] == generation {
                continue;
            }
            seen[pc] = generation;
            m.charge(COSTS.regex_step);
            match &self.insts[pc] {
                Inst::Jmp(x) => stack.push((*x, saves)),
                Inst::Split(x, y) => {
                    // Push y first, then x, so x pops first = higher priority.
                    stack.push((*y, Rc::clone(&saves)));
                    stack.push((*x, saves));
                }
                Inst::Save(n) => {
                    let mut s = (*saves).clone();
                    s[*n] = sp as i64;
                    stack.push((pc + 1, Rc::new(s)));
                }
                Inst::AssertStart => {
                    if sp == 0 {
                        stack.push((pc + 1, saves));
                    }
                }
                Inst::AssertEnd => {
                    if sp == len {
                        stack.push((pc + 1, saves));
                    }
                }
                // Char / Any / Class / Match — parked for the consume loop.
                _ => list.push(Thread { pc, saves }),
            }
        }
        Ok(())
    }

    /// `regexp_match(source, …)` capture extraction (regex.md §8). Searches once; on a match returns
    /// the capture group strings (groups 1..n, or a 1-element whole-match list when the pattern has
    /// no group — the PG rule), an unset group being `None`. Returns `None` (the whole result) on no
    /// match. `match_input` is the (possibly case-folded) subject the VM matches; `orig_input` is the
    /// ORIGINAL-case subject the returned substrings are sliced from (same length, regex.md §8).
    pub fn regexp_match(
        &self,
        match_input: &[char],
        orig_input: &[char],
        m: &mut Meter,
    ) -> Result<Option<Vec<Option<String>>>> {
        let Some(saves) = self.search(match_input, 0, m)? else {
            return Ok(None);
        };
        let groups = if self.ngroups == 0 {
            // No capturing group: PG returns a 1-element array of the whole match.
            vec![slice_group(orig_input, saves[0], saves[1])]
        } else {
            (1..=self.ngroups)
                .map(|g| slice_group(orig_input, saves[2 * g], saves[2 * g + 1]))
                .collect()
        };
        Ok(Some(groups))
    }

    /// `regexp_replace(source, pattern, replacement, …)` (regex.md §8). Replaces the first match (or
    /// all when `global`) by the replacement TEMPLATE (`\1`..`\9` = capture group, `\&` = whole
    /// match, `\\` = literal backslash). Non-matched text and captured substrings come from
    /// `orig_input` (original case); the VM matches over `match_input` (possibly case-folded).
    pub fn regexp_replace(
        &self,
        match_input: &[char],
        orig_input: &[char],
        replacement: &[char],
        global: bool,
        m: &mut Meter,
    ) -> Result<String> {
        let mut out = String::new();
        let mut pos = 0usize;
        loop {
            let Some(saves) = self.search(match_input, pos, m)? else {
                break;
            };
            let s = saves[0] as usize;
            let e = saves[1] as usize;
            out.extend(orig_input[pos..s].iter());
            splice_replacement(&mut out, replacement, &saves, orig_input);
            if !global {
                out.extend(orig_input[e..].iter());
                return Ok(out);
            }
            if e > s {
                pos = e;
            } else {
                // Empty match: emit the char at `e` (if any) and advance past it, so a pattern that
                // can match empty (`a*`) cannot loop forever — the PG global rule.
                if e < orig_input.len() {
                    out.push(orig_input[e]);
                }
                pos = e + 1;
            }
            if pos > orig_input.len() {
                return Ok(out);
            }
        }
        out.extend(orig_input[pos..].iter());
        Ok(out)
    }

    /// Count the non-overlapping matches at or after code-point position `start` (`regexp_count`,
    /// regex.md §8b). The non-overlapping advance is `regexp_replace`'s global rule: after a match
    /// `[s,e)` continue at `e`, or at `e+1` for an EMPTY match so a nullable pattern terminates.
    /// `start` may be up to `len` (a match of the empty string at the very end still counts);
    /// `start > len` (the caller clamps it to `len+1`) yields 0.
    pub fn regexp_count(&self, input: &[char], start: usize, m: &mut Meter) -> Result<i64> {
        let len = input.len();
        let mut pos = start;
        let mut count = 0i64;
        while pos <= len {
            let Some(saves) = self.search(input, pos, m)? else {
                break;
            };
            count += 1;
            let s = saves[0] as usize;
            let e = saves[1] as usize;
            pos = if e > s { e } else { e + 1 };
        }
        Ok(count)
    }

    /// The capture slots of the N-th (1-based) non-overlapping match at or after `start`
    /// (`regexp_substr` / `regexp_instr`, regex.md §8b), or `None` when fewer than N matches exist.
    /// Same non-overlapping advance as [`regexp_count`](Self::regexp_count).
    pub fn nth_match(
        &self,
        input: &[char],
        start: usize,
        n: i64,
        m: &mut Meter,
    ) -> Result<Option<Vec<i64>>> {
        let len = input.len();
        let mut pos = start;
        let mut count = 0i64;
        while pos <= len {
            let Some(saves) = self.search(input, pos, m)? else {
                break;
            };
            count += 1;
            if count == n {
                return Ok(Some(saves));
            }
            let s = saves[0] as usize;
            let e = saves[1] as usize;
            pos = if e > s { e } else { e + 1 };
        }
        Ok(None)
    }
}

/// Slice `orig[start..end]` to an owned `String`, or `None` for an unset (`-1`) group.
fn slice_group(orig: &[char], start: i64, end: i64) -> Option<String> {
    if start < 0 || end < 0 {
        return None;
    }
    Some(orig[start as usize..end as usize].iter().collect())
}

/// Append a replacement template to `out`, expanding `\1`..`\9` (capture group), `\&` (whole match),
/// `\\` (literal backslash), and `\<other>` (the literal `<other>`). A trailing lone `\` is literal.
fn splice_replacement(out: &mut String, repl: &[char], saves: &[i64], orig: &[char]) {
    let mut i = 0;
    while i < repl.len() {
        let c = repl[i];
        if c == '\\' && i + 1 < repl.len() {
            let n = repl[i + 1];
            if let Some(d) = n.to_digit(10) {
                let g = d as usize;
                if 2 * g + 1 < saves.len() {
                    if let Some(s) = slice_group(orig, saves[2 * g], saves[2 * g + 1]) {
                        out.push_str(&s);
                    }
                }
            } else if n == '&' {
                if let Some(s) = slice_group(orig, saves[0], saves[1]) {
                    out.push_str(&s);
                }
            } else {
                out.push(n); // \\ -> \, and \<other> -> <other>
            }
            i += 2;
        } else {
            out.push(c);
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cost_meter() -> Meter {
        Meter::new()
    }

    #[test]
    fn empty_pattern_matches_empty() {
        let p = compile("").unwrap();
        let mut m = cost_meter();
        assert!(p.is_match(&[], &mut m).unwrap());
        assert!(p.is_match(&['a'], &mut m).unwrap());
    }

    #[test]
    fn literal_unanchored() {
        let p = compile("b").unwrap();
        let mut m = cost_meter();
        assert!(p.is_match(&['a', 'b', 'c'], &mut m).unwrap());
        assert!(!compile("z").unwrap().is_match(&['a', 'b'], &mut m).unwrap());
    }

    #[test]
    fn anchors() {
        let mut m = cost_meter();
        assert!(
            compile("^a")
                .unwrap()
                .is_match(&['a', 'b'], &mut m)
                .unwrap()
        );
        assert!(
            !compile("^b")
                .unwrap()
                .is_match(&['a', 'b'], &mut m)
                .unwrap()
        );
        assert!(
            compile("b$")
                .unwrap()
                .is_match(&['a', 'b'], &mut m)
                .unwrap()
        );
        assert!(
            !compile("a$")
                .unwrap()
                .is_match(&['a', 'b'], &mut m)
                .unwrap()
        );
    }

    #[test]
    fn classes_and_quantifiers() {
        let mut m = cost_meter();
        assert!(
            compile("[0-9]+")
                .unwrap()
                .is_match(&['x', '4', '2'], &mut m)
                .unwrap()
        );
        assert!(
            compile(r"\d{3}")
                .unwrap()
                .is_match(&['1', '2', '3'], &mut m)
                .unwrap()
        );
        assert!(
            !compile(r"^\d{3}$")
                .unwrap()
                .is_match(&['1', '2'], &mut m)
                .unwrap()
        );
        assert!(
            compile("a.c")
                .unwrap()
                .is_match(&['a', 'x', 'c'], &mut m)
                .unwrap()
        );
        assert!(
            !compile("a.c")
                .unwrap()
                .is_match(&['a', '\n', 'c'], &mut m)
                .unwrap()
        );
    }

    #[test]
    fn alternation_and_groups() {
        let mut m = cost_meter();
        assert!(
            compile("(foo|bar)")
                .unwrap()
                .is_match(&"bar".chars().collect::<Vec<_>>(), &mut m)
                .unwrap()
        );
        assert!(
            !compile("^(foo|bar)$")
                .unwrap()
                .is_match(&"baz".chars().collect::<Vec<_>>(), &mut m)
                .unwrap()
        );
    }

    #[test]
    fn redos_pattern_is_linear() {
        // The classic catastrophic-backtracking pattern: a backtracker would hang, the Pike VM
        // runs it in linear time and simply returns false.
        let p = compile("(a+)+$").unwrap();
        let input: Vec<char> = std::iter::repeat_n('a', 40).chain(['!']).collect();
        let mut m = cost_meter();
        assert!(!p.is_match(&input, &mut m).unwrap());
    }

    #[test]
    fn malformed_patterns_are_2201b() {
        for pat in ["(a", "a)", "[a", r"a\", "*a", "a{2,1}", "a**", "(?=a)"] {
            let err = compile(pat).unwrap_err();
            assert_eq!(
                err.state,
                SqlState::InvalidRegularExpression,
                "pattern {pat:?}"
            );
        }
    }

    #[test]
    fn oversized_program_is_54001() {
        let err = compile("(a{1000}){1000}").unwrap_err();
        assert_eq!(err.state, SqlState::StatementTooComplex);
    }

    #[test]
    fn captures_record_spans() {
        let p = compile("a(b+)c").unwrap();
        let mut m = cost_meter();
        let caps = p
            .run(&"xabbbcx".chars().collect::<Vec<_>>(), &mut m)
            .unwrap()
            .unwrap();
        // group 0 = whole match "abbbc" at [1,6); group 1 = "bbb" at [2,5).
        assert_eq!(caps[0], 1);
        assert_eq!(caps[1], 6);
        assert_eq!(caps[2], 2);
        assert_eq!(caps[3], 5);
    }
}
