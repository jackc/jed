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
use std::sync::{Mutex, PoisonError};

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
/// (excluding group 0, the whole match), plus the shape of its capture-slot trees ([`cap_shape`])
/// and the dedup set ([`Seen`]) its searches reuse.
#[derive(Clone, Debug)]
pub struct Program {
    pub insts: Vec<Inst>,
    pub classes: Vec<CharClass>,
    pub ngroups: usize,
    cap_depth: usize,
    cap_leaf_len: usize,
    seen_pool: SeenPool,
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
    let (cap_depth, cap_leaf_len) = cap_shape(2 * (ngroups + 1));
    Ok(Program {
        insts: c.insts,
        classes: c.classes,
        ngroups,
        cap_depth,
        cap_leaf_len,
        seen_pool: SeenPool::default(),
    })
}

// ---------------------------------------------------------------------------
// Capture slots — a persistent array (regex.md §4.1)
// ---------------------------------------------------------------------------

/// A capture-slot tree's leaves hold `CAP_FAN` slots and its interior nodes `CAP_FAN` subtrees.
/// Every group emits two Saves, so a program's `2·(ngroups+1)` slots never exceed
/// [`MAX_REGEX_PROGRAM`] and fit in four levels.
const CAP_FAN_BITS: usize = 4;
const CAP_FAN: usize = 1 << CAP_FAN_BITS;

/// A match's capture slots (code-point offsets; `-1` = unset), a persistent array (regex.md §4.1): a
/// Save makes a new version and leaves the old one intact for the threads still holding it. A pattern
/// whose slots fit in one `CAP_FAN`-slot leaf keeps them flat, and a Save copies all of them. A larger
/// pattern keeps a tree of [`CapNode`]s, and a Save copies only the path to the slot it sets, so its
/// work is bounded however many groups the pattern has.
#[derive(Clone, Debug)]
pub enum Captures {
    Flat(Rc<Vec<i64>>),
    Tree(CapNode),
}

/// A node of a capture-slot tree. A leaf holds slot values, an interior node its subtrees; a missing
/// node means every slot under it is unset. Each node is one shared slice of exactly its entries, so
/// a level of a Save is one allocation.
#[derive(Clone, Debug)]
pub enum CapNode {
    Leaf(Rc<[i64]>),
    Inner(Rc<[Option<CapNode>]>),
}

/// The entries of a node no Save has reached: every slot unset, every subtree missing.
const UNSET_SLOTS: [i64; CAP_FAN] = [-1; CAP_FAN];
const NO_SUBTREES: [Option<CapNode>; CAP_FAN] = [const { None }; CAP_FAN];

/// The depth of the capture-slot tree for `nslots` slots and the length of its leaves: a one-level
/// tree is a single leaf of exactly `nslots` slots, kept flat; a deeper one has full leaves.
fn cap_shape(nslots: usize) -> (usize, usize) {
    let mut depth = 1;
    let mut span = CAP_FAN;
    while span < nslots {
        depth += 1;
        span *= CAP_FAN;
    }
    if depth == 1 {
        (1, nslots)
    } else {
        (depth, CAP_FAN)
    }
}

impl Program {
    /// Capture slot `i` of a match's slots (`-1` = unset).
    pub fn slot(&self, caps: &Captures, i: usize) -> i64 {
        match caps {
            Captures::Flat(flat) => flat[i],
            Captures::Tree(tree) => self.slot_in(Some(tree), i),
        }
    }

    fn slot_in(&self, caps: Option<&CapNode>, i: usize) -> i64 {
        let mut node = caps;
        let mut shift = CAP_FAN_BITS * (self.cap_depth - 1);
        while shift > 0 {
            match node {
                Some(CapNode::Inner(kids)) => node = kids[(i >> shift) & (CAP_FAN - 1)].as_ref(),
                _ => return -1,
            }
            shift -= CAP_FAN_BITS;
        }
        match node {
            Some(CapNode::Leaf(vals)) => vals[i & (CAP_FAN - 1)],
            _ => -1,
        }
    }

    /// The version of the tree `caps` with slot `i` set to `v`, sharing every node off the path to it.
    fn with_slot(&self, caps: Option<&CapNode>, i: usize, v: i64) -> CapNode {
        self.with_slot_at(caps, CAP_FAN_BITS * (self.cap_depth - 1), i, v)
    }

    fn with_slot_at(&self, node: Option<&CapNode>, shift: usize, i: usize, v: i64) -> CapNode {
        if shift == 0 {
            let mut vals: Rc<[i64]> = match node {
                Some(CapNode::Leaf(vals)) => Rc::from(&vals[..]),
                _ => Rc::from(&UNSET_SLOTS[..self.cap_leaf_len]),
            };
            Rc::get_mut(&mut vals).expect("a fresh leaf is unshared")[i & (CAP_FAN - 1)] = v;
            return CapNode::Leaf(vals);
        }
        let old = match node {
            Some(CapNode::Inner(kids)) => &kids[..],
            _ => &NO_SUBTREES[..],
        };
        let k = (i >> shift) & (CAP_FAN - 1);
        let child = self.with_slot_at(old[k].as_ref(), shift - CAP_FAN_BITS, i, v);
        let mut kids: Rc<[Option<CapNode>]> = Rc::from(old);
        Rc::get_mut(&mut kids).expect("a fresh node is unshared")[k] = Some(child);
        CapNode::Inner(kids)
    }

    /// Every capture slot of a match as a flat array.
    pub fn slots(&self, caps: &Captures) -> Vec<i64> {
        (0..2 * (self.ngroups + 1))
            .map(|i| self.slot(caps, i))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Pike VM (regex.md §4)
// ---------------------------------------------------------------------------

/// How the VM keeps a thread's capture slots (regex.md §4.1): [`FlatCaps`] for a pattern whose slots
/// fit in one leaf, [`TreeCaps`] for a larger one. The VM is compiled once per representation, so a
/// small pattern's thread is a pc and one pointer to its array.
trait CapRep {
    type Caps: Clone;
    /// The version of `caps` (`None` = every slot unset) with slot `i` set to `v`.
    fn with(prog: &Program, caps: Option<&Self::Caps>, i: usize, v: i64) -> Self::Caps;
    fn result(caps: Self::Caps) -> Captures;
}

/// The slots as one array, copied whole on a Save: at most `CAP_FAN` slots.
struct FlatCaps;

impl CapRep for FlatCaps {
    type Caps = Rc<Vec<i64>>;

    fn with(prog: &Program, caps: Option<&Rc<Vec<i64>>>, i: usize, v: i64) -> Rc<Vec<i64>> {
        let mut flat = match caps {
            Some(caps) => (**caps).clone(),
            None => vec![-1; prog.cap_leaf_len],
        };
        flat[i] = v;
        Rc::new(flat)
    }

    fn result(caps: Rc<Vec<i64>>) -> Captures {
        Captures::Flat(caps)
    }
}

/// The slots as a capture-slot tree; a Save copies the path to the slot it sets.
struct TreeCaps;

impl CapRep for TreeCaps {
    type Caps = CapNode;

    fn with(prog: &Program, caps: Option<&CapNode>, i: usize, v: i64) -> CapNode {
        prog.with_slot(caps, i, v)
    }

    fn result(caps: CapNode) -> Captures {
        Captures::Tree(caps)
    }
}

struct Thread<C> {
    pc: usize,
    caps: Option<C>,
}

/// The per-position dedup set (regex.md §4): `marks[pc] == generation` means `pc` was visited at the
/// current position, so moving to the next position is an increment, not a clear. A program pools
/// its sets and reuses them across searches and rows, so a search does no setup work in proportion to
/// the program, only the states it explores, which `regex_step` meters (§4.2).
#[derive(Debug)]
struct Seen {
    marks: Vec<u32>,
    generation: u32,
}

impl Seen {
    /// Start a new generation. When the counter wraps, the stale marks could alias the new
    /// generation, so they are cleared.
    fn next(&mut self) -> u32 {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.marks.fill(0);
            self.generation = 1;
        }
        self.generation
    }
}

/// The dedup set a program keeps between calls (see [`Seen`]). A clone of the program starts empty.
#[derive(Debug, Default)]
struct SeenPool(Mutex<Option<Seen>>);

impl Clone for SeenPool {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl Program {
    /// Run `f` with a dedup set for one call's searches, taken from the program's pool when it has
    /// one and returned to it afterwards.
    fn with_seen<T>(&self, f: impl FnOnce(&mut Seen) -> T) -> T {
        let pooled = self
            .seen_pool
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let mut seen = pooled.unwrap_or_else(|| Seen {
            marks: vec![0; self.insts.len()],
            generation: 0,
        });
        let out = f(&mut seen);
        *self
            .seen_pool
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(seen);
        out
    }

    /// Boolean match — TRUE iff the pattern matches somewhere in `input` (the `~` operator).
    pub fn is_match(&self, input: &[char], m: &mut Meter) -> Result<bool> {
        Ok(self.run(input, m)?.is_some())
    }

    /// Run the Pike VM from the start of the input (regex.md §4). Returns the winning thread's
    /// capture slots on a match, or `None`.
    pub fn run(&self, input: &[char], m: &mut Meter) -> Result<Option<Captures>> {
        self.with_seen(|seen| self.search(seen, input, 0, m))
    }

    /// Run the Pike VM, considering only matches that START at code-point position `start` or later
    /// (the unanchored search seeds its lazy `.*?` prefix at `start`). `^`/`$` still anchor at the
    /// true input bounds (absolute `sp == 0` / `sp == len`). Used by `regexp_replace`'s global loop.
    /// Charges `regex_step` per explored state and guards once per input position.
    // `sp` is a logical input POSITION that must reach `len` (one past the end) so the final
    // epsilon-closure can fire `AssertEnd`/`Match` — enumerate() cannot express `start..=len`.
    fn search(
        &self,
        seen: &mut Seen,
        input: &[char],
        start: usize,
        m: &mut Meter,
    ) -> Result<Option<Captures>> {
        if self.cap_depth == 1 {
            self.pike_search::<FlatCaps>(seen, input, start, m)
        } else {
            self.pike_search::<TreeCaps>(seen, input, start, m)
        }
    }

    /// [`Program::search`] over the capture representation `R`.
    #[allow(clippy::needless_range_loop)]
    fn pike_search<R: CapRep>(
        &self,
        seen: &mut Seen,
        input: &[char],
        start: usize,
        m: &mut Meter,
    ) -> Result<Option<Captures>> {
        let len = input.len();
        let mut clist: Vec<Thread<R::Caps>> = Vec::new();
        let mut nlist: Vec<Thread<R::Caps>> = Vec::new();
        let mut matched: Option<R::Caps> = None;

        let generation = seen.next();
        self.add_thread::<R>(&mut clist, seen, generation, 0, None, start, len, m)?;

        for sp in start..=len {
            let generation = seen.next();
            nlist.clear();
            let mut i = 0;
            while i < clist.len() {
                let pc = clist[i].pc;
                let advances = match &self.insts[pc] {
                    Inst::Char(c) => sp < len && input[sp] == *c,
                    Inst::Any => sp < len && input[sp] != '\n',
                    Inst::Class(k) => sp < len && self.classes[*k].admits(input[sp]),
                    Inst::Match => {
                        matched = clist[i].caps.clone();
                        break; // cut lower-priority threads (leftmost-first, regex.md §4)
                    }
                    _ => unreachable!("epsilon instructions are resolved inside add_thread"),
                };
                if advances {
                    let caps = clist[i].caps.clone();
                    self.add_thread::<R>(
                        &mut nlist,
                        seen,
                        generation,
                        pc + 1,
                        caps,
                        sp + 1,
                        len,
                        m,
                    )?;
                }
                i += 1;
            }
            std::mem::swap(&mut clist, &mut nlist);
            m.guard()?; // §6 ceiling, once per input position
            if clist.is_empty() {
                break;
            }
        }
        Ok(matched.map(R::result))
    }

    /// Epsilon-closure: follow Jmp/Split/Save/Assert from `pc`, appending consuming/Match threads to
    /// `list`, deduping by pc within this generation. Iterative (explicit stack) so a long
    /// Jmp/Split chain cannot overflow the native stack; the `y` arm of a Split is pushed before
    /// `x` so `x` is processed first (higher priority). Charges `regex_step` per explored state.
    #[allow(clippy::too_many_arguments)]
    fn add_thread<R: CapRep>(
        &self,
        list: &mut Vec<Thread<R::Caps>>,
        seen: &mut Seen,
        generation: u32,
        pc0: usize,
        caps0: Option<R::Caps>,
        sp: usize,
        len: usize,
        m: &mut Meter,
    ) -> Result<()> {
        let mut stack: Vec<Thread<R::Caps>> = vec![Thread {
            pc: pc0,
            caps: caps0,
        }];
        while let Some(Thread { pc, caps }) = stack.pop() {
            if seen.marks[pc] == generation {
                continue;
            }
            seen.marks[pc] = generation;
            m.charge(COSTS.regex_step);
            match &self.insts[pc] {
                Inst::Jmp(x) => stack.push(Thread { pc: *x, caps }),
                Inst::Split(x, y) => {
                    // Push y first, then x, so x pops first = higher priority.
                    stack.push(Thread {
                        pc: *y,
                        caps: caps.clone(),
                    });
                    stack.push(Thread { pc: *x, caps });
                }
                Inst::Save(n) => {
                    let caps = R::with(self, caps.as_ref(), *n, sp as i64);
                    stack.push(Thread {
                        pc: pc + 1,
                        caps: Some(caps),
                    });
                }
                Inst::AssertStart => {
                    if sp == 0 {
                        stack.push(Thread { pc: pc + 1, caps });
                    }
                }
                Inst::AssertEnd => {
                    if sp == len {
                        stack.push(Thread { pc: pc + 1, caps });
                    }
                }
                // Char / Any / Class / Match — parked for the consume loop.
                _ => list.push(Thread { pc, caps }),
            }
        }
        Ok(())
    }

    /// The start and end of a match's whole span (group 0) as positions.
    fn span(&self, caps: &Captures) -> (usize, usize) {
        (self.slot(caps, 0) as usize, self.slot(caps, 1) as usize)
    }

    /// The matches `regexp_replace(source, pattern, replacement, …)` (regex.md §8) replaces: the first
    /// match, or every non-overlapping match when `global`. Each entry is a match's capture slots. The
    /// VM matches over `match_input` (possibly case-folded); this is the kernel's whole search, so it
    /// charges every `regex_step` the replacement costs. [`Program::replace_size`] and
    /// [`Program::replace_build`] then size and splice from these slots without searching again
    /// (cost.md §8.1). `regexp_match` (never global) and `regexp_matches` run the same search, then
    /// size and slice their capture arrays from the slots ([`Program::capture_array_size`],
    /// [`Program::capture_array`]).
    pub fn replace_matches(
        &self,
        match_input: &[char],
        global: bool,
        m: &mut Meter,
    ) -> Result<Vec<Captures>> {
        self.with_seen(|seen| {
            let mut found = Vec::new();
            let mut pos = 0usize;
            loop {
                let Some(caps) = self.search(seen, match_input, pos, m)? else {
                    break;
                };
                let (s, e) = self.span(&caps);
                found.push(caps);
                if !global {
                    break;
                }
                // Empty match: the next search starts past the char at `e`, so a pattern that can
                // match empty (`a*`) cannot loop forever — the PG global rule.
                pos = if e > s { e } else { e + 1 };
                if pos > match_input.len() {
                    break;
                }
            }
            Ok(found)
        })
    }

    /// Count the non-overlapping matches at or after code-point position `start` (`regexp_count`,
    /// regex.md §8b). The non-overlapping advance is `regexp_replace`'s global rule: after a match
    /// `[s,e)` continue at `e`, or at `e+1` for an EMPTY match so a nullable pattern terminates.
    /// `start` may be up to `len` (a match of the empty string at the very end still counts);
    /// `start > len` (the caller clamps it to `len+1`) yields 0.
    pub fn regexp_count(&self, input: &[char], start: usize, m: &mut Meter) -> Result<i64> {
        self.with_seen(|seen| {
            let len = input.len();
            let mut pos = start;
            let mut count = 0i64;
            while pos <= len {
                let Some(caps) = self.search(seen, input, pos, m)? else {
                    break;
                };
                count += 1;
                let (s, e) = self.span(&caps);
                pos = if e > s { e } else { e + 1 };
            }
            Ok(count)
        })
    }

    /// The capture groups `regexp_match` / `regexp_matches` report for a match (regex.md §8):
    /// groups 1..n, or group 0 (the whole match) when the pattern has no group — the PG rule.
    fn reported_groups(&self) -> std::ops::RangeInclusive<usize> {
        if self.ngroups == 0 {
            0..=0
        } else {
            1..=self.ngroups
        }
    }

    /// The exact `payload` (memory.md §3) of the one-dimensional `text[]` capture array for a
    /// match's slots, computed from the spans without building it (cost.md §8.1): `ARRAY_DIM`, plus
    /// `VALUE` and the span's UTF-8 bytes per reported group (an unset group is a bare `VALUE`).
    /// `utf8_at` is [`utf8_offsets`] of the subject, so each span is measured in O(1).
    pub fn capture_array_size(&self, caps: &Captures, utf8_at: &[i64]) -> i64 {
        let mut size = crate::costs::MEMORY_ARRAY_DIM;
        for g in self.reported_groups() {
            let (s, e) = (self.slot(caps, 2 * g), self.slot(caps, 2 * g + 1));
            size += crate::costs::MEMORY_VALUE;
            if s >= 0 && e >= 0 {
                size += utf8_at[e as usize] - utf8_at[s as usize];
            }
        }
        size
    }

    /// The capture array for a match's slots, sliced from the ORIGINAL-case subject: the reported
    /// groups' substrings, an unset group `None`.
    pub fn capture_array(&self, caps: &Captures, orig: &[char]) -> Vec<Option<String>> {
        self.reported_groups()
            .map(|g| slice_group(orig, self.slot(caps, 2 * g), self.slot(caps, 2 * g + 1)))
            .collect()
    }

    /// The pieces of `regexp_split_to_array` / `regexp_split_to_table` (regex.md §8c): the text
    /// between consecutive matches, found with the global advance, then the text after the last.
    /// PostgreSQL's DEGENERATE matches split nothing: an empty match at the start or end of the
    /// subject, or one that ends no later than the previous match — so `''` splits into characters
    /// and a split of the empty string is one empty piece. The pieces are sliced from the
    /// ORIGINAL-case subject (same length as the matched one).
    pub fn split_pieces(
        &self,
        match_input: &[char],
        orig_input: &[char],
        m: &mut Meter,
    ) -> Result<Vec<String>> {
        self.with_seen(|seen| {
            let len = match_input.len();
            let mut pieces = Vec::new();
            let mut pos = 0usize;
            let mut prev_end = 0usize; // the end of the previous match, degenerate or not
            let mut piece_start = 0usize; // the end of the previous SPLITTING match
            while pos <= len {
                let Some(caps) = self.search(seen, match_input, pos, m)? else {
                    break;
                };
                let (s, e) = self.span(&caps);
                if s < len && e > prev_end {
                    pieces.push(orig_input[piece_start..s].iter().collect());
                    piece_start = e;
                }
                prev_end = e;
                pos = if e > s { e } else { e + 1 };
            }
            pieces.push(orig_input[piece_start..len].iter().collect());
            Ok(pieces)
        })
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
    ) -> Result<Option<Captures>> {
        self.with_seen(|seen| {
            let len = input.len();
            let mut pos = start;
            let mut count = 0i64;
            while pos <= len {
                let Some(caps) = self.search(seen, input, pos, m)? else {
                    break;
                };
                count += 1;
                if count == n {
                    return Ok(Some(caps));
                }
                let (s, e) = self.span(&caps);
                pos = if e > s { e } else { e + 1 };
            }
            Ok(None)
        })
    }

    /// Walk a replacement template, expanding `\1`..`\9` (capture group), `\&` (whole match), `\\`
    /// (literal backslash), and `\<other>` (the literal `<other>`). A trailing lone `\` is literal,
    /// and an unset or absent group expands to nothing. Sizing and building share this walk so they
    /// cannot disagree.
    fn for_each_piece(&self, repl: &[char], caps: &Captures, mut f: impl FnMut(Piece)) {
        // An unset (`-1`) group expands to nothing.
        let span = |start: i64, end: i64| {
            (start >= 0 && end >= 0).then(|| Piece::Span(start as usize, end as usize))
        };
        let mut i = 0;
        while i < repl.len() {
            let c = repl[i];
            if c == '\\' && i + 1 < repl.len() {
                let n = repl[i + 1];
                if let Some(d) = n.to_digit(10) {
                    let g = d as usize;
                    if g <= self.ngroups {
                        if let Some(p) = span(self.slot(caps, 2 * g), self.slot(caps, 2 * g + 1)) {
                            f(p);
                        }
                    }
                } else if n == '&' {
                    if let Some(p) = span(self.slot(caps, 0), self.slot(caps, 1)) {
                        f(p);
                    }
                } else {
                    f(Piece::Char(n));
                }
                i += 2;
            } else {
                f(Piece::Char(c));
                i += 1;
            }
        }
    }

    /// The text spans and pieces of a replacement, in output order: unmatched source text from
    /// `orig`, each match's expanded template, and, for an empty match, the char after it.
    fn for_each_output(
        &self,
        orig: &[char],
        repl: &[char],
        found: &[Captures],
        mut f: impl FnMut(Piece),
    ) {
        let mut pos = 0usize;
        for caps in found {
            let (s, e) = self.span(caps);
            f(Piece::Span(pos, s));
            self.for_each_piece(repl, caps, &mut f);
            if e > s {
                pos = e;
            } else {
                if e < orig.len() {
                    f(Piece::Char(orig[e]));
                }
                pos = e + 1;
            }
        }
        if pos <= orig.len() {
            f(Piece::Span(pos, orig.len()));
        }
    }

    /// The exact UTF-8 byte length of `regexp_replace`'s result for the matches
    /// [`Program::replace_matches`] found, computed without building it (cost.md §8.1).
    pub fn replace_size(&self, orig: &[char], repl: &[char], found: &[Captures]) -> i64 {
        let mut size = 0i64;
        self.for_each_output(orig, repl, found, |p| match p {
            Piece::Char(c) => size += c.len_utf8() as i64,
            Piece::Span(a, b) => {
                size += orig[a..b].iter().map(|c| c.len_utf8() as i64).sum::<i64>()
            }
        });
        size
    }

    /// Build `regexp_replace`'s result of `size` bytes from the matches
    /// [`Program::replace_matches`] found.
    pub fn replace_build(
        &self,
        orig: &[char],
        repl: &[char],
        found: &[Captures],
        size: i64,
    ) -> String {
        let mut out = String::with_capacity(size as usize);
        self.for_each_output(orig, repl, found, |p| match p {
            Piece::Char(c) => out.push(c),
            Piece::Span(a, b) => out.extend(orig[a..b].iter()),
        });
        out
    }
}

/// The UTF-8 byte offset of every code point of `orig`, plus the total at `orig.len()`: the
/// span `[s, e)` is `offsets[e] - offsets[s]` bytes.
pub fn utf8_offsets(orig: &[char]) -> Vec<i64> {
    let mut offsets = Vec::with_capacity(orig.len() + 1);
    let mut at = 0i64;
    offsets.push(at);
    for c in orig {
        at += c.len_utf8() as i64;
        offsets.push(at);
    }
    offsets
}

/// Slice `orig[start..end]` to an owned `String`, or `None` for an unset (`-1`) group.
fn slice_group(orig: &[char], start: i64, end: i64) -> Option<String> {
    if start < 0 || end < 0 {
        return None;
    }
    Some(orig[start as usize..end as usize].iter().collect())
}

/// One piece of an expanded replacement template: a literal char, or the `[start, end)` span of
/// `orig` a capture group or the whole match selected.
enum Piece {
    Char(char),
    Span(usize, usize),
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
        assert_eq!(p.slots(&caps), vec![1, 6, 2, 5]);
    }
}
