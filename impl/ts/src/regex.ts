// Regular-expression engine — a hand-written RE2-style Pike VM (spec/design/regex.md).
//
// jed's own RE2-able regex flavor (NOT PostgreSQL-compatible): a pattern compiles to a flat NFA
// bytecode program (RegexProgram), matched over the input by a thread-list simulation in LINEAR TIME
// with no backtracking — immune to catastrophic-backtracking (ReDoS) attacks independent of the cost
// meter (CLAUDE.md §13). Both the compilation and the per-step cost are part of the cross-core
// contract (no reference impl, §2): all three cores emit the byte-identical program
// (spec/regex/program_vectors.toml) and accrue identical regex_compile / regex_step cost
// (spec/regex/match_vectors.toml). The lowering follows regex.md §3 exactly. Iteration is by Unicode
// CODE POINT (Array.from / codePointAt — never charCodeAt), a §8 determinism surface.

import { COSTS, MEMORY_ARRAY_DIM, MEMORY_VALUE } from "./costs.ts";
import type { Meter } from "./cost.ts";
import { foldLowerSimple, type PropertyTable } from "./collation.ts";
import { engineError } from "./errors.ts";

// The maximum compiled-program size, in instructions (regex.md §6, cost.md §7c). A fixed cross-core
// constant — a pattern whose program would exceed it aborts 54001 at compile.
export const MAX_REGEX_PROGRAM = 32768;

// The largest Unicode scalar value (for class complement).
const MAX_REGEX_CP = 0x10ffff;

// ---------------------------------------------------------------------------
// Bytecode
// ---------------------------------------------------------------------------

type RegexOp =
  | "char"
  | "any"
  // Any code point, newline included: the unanchored prefix's consume step (regex.md §3.2).
  | "anyChar"
  | "class"
  | "split"
  | "jmp"
  | "save"
  | "assertStart"
  | "assertEnd"
  // `^` / `$` in the line-anchor mode (regex.md §8d): also just after / before a newline.
  | "assertLineStart"
  | "assertLineEnd"
  // Passes only at a position in the run's gate set (the gated wrappers of regex.md §8f).
  | "gate"
  | "match";

// One NFA instruction. x/y hold jump targets (absolute instruction indices) for split/jmp, the slot
// for save, the class index for class; ch holds the code point for char.
type Inst = { op: RegexOp; x: number; y: number; ch: number };

// A character class: positive, sorted, merged code-point ranges plus a negated flag applied at match
// time (regex.md §3.4 — never by complementing the range list).
type RegexClass = { negated: boolean; ranges: [number, number][] };

function classAdmits(c: RegexClass, cp: number): boolean {
  let inside = false;
  for (const [lo, hi] of c.ranges) {
    if (cp >= lo && cp <= hi) {
      inside = true;
      break;
    }
  }
  return inside !== c.negated;
}

// A compiled pattern: the instruction array, the class table, and the capturing-group count
// (excluding group 0, the whole match), plus the shape of its capture-slot trees (capShape) and the
// dedup set (RegexSeen) its searches reuse.
export type RegexProgram = {
  insts: Inst[];
  classes: RegexClass[];
  ngroups: number;
  capDepth: number;
  capLeafLen: number;
  seen: RegexSeen | null;
};

export function regexNinst(p: RegexProgram): number {
  return p.insts.length;
}

// The canonical instruction listing (the program_vectors.toml contract, regex.md §9).
export function regexListing(p: RegexProgram): string[] {
  return p.insts.map((i) => {
    switch (i.op) {
      case "char":
        return `char ${i.ch}`;
      case "any":
        return "any";
      case "anyChar":
        return "anychar";
      case "class":
        return `class ${i.x}`;
      case "split":
        return `split ${i.x} ${i.y}`;
      case "jmp":
        return `jmp ${i.x}`;
      case "save":
        return `save ${i.x}`;
      case "assertStart":
        return "assertstart";
      case "assertEnd":
        return "assertend";
      case "assertLineStart":
        return "assertlinestart";
      case "assertLineEnd":
        return "assertlineend";
      case "gate":
        return "gate";
      case "match":
        return "match";
      default: {
        const exhaustive: never = i.op;
        throw new Error(`unreachable regex op ${exhaustive}`);
      }
    }
  });
}

// The canonical class table: `lo-hi` ranges joined by `,`, prefixed `^` when negated.
export function regexClassListing(p: RegexProgram): string[] {
  return p.classes.map((c) => {
    const body = c.ranges.map(([lo, hi]) => `${lo}-${hi}`).join(",");
    return c.negated ? `^${body}` : body;
  });
}

function regexInvalid(detail: string): EngineErrorLike {
  return engineError("invalid_regular_expression", `invalid regular expression: ${detail}`);
}

function regexTooComplex(): EngineErrorLike {
  return engineError(
    "statement_too_complex",
    `regular expression compiles to more than ${MAX_REGEX_PROGRAM} instructions`,
  );
}

// engineError returns an EngineError; we only need to throw it, so a minimal alias keeps the import
// list small without re-exporting the type.
type EngineErrorLike = ReturnType<typeof engineError>;

// ---------------------------------------------------------------------------
// Pattern AST
// ---------------------------------------------------------------------------

type Node =
  | { kind: "empty" }
  | { kind: "char"; ch: number }
  | { kind: "any" }
  | { kind: "class"; class: RegexClass }
  | { kind: "concat"; subs: Node[] }
  | { kind: "alt"; left: Node; right: Node }
  | { kind: "star"; sub: Node; greedy: boolean }
  | { kind: "plus"; sub: Node; greedy: boolean }
  | { kind: "quest"; sub: Node; greedy: boolean }
  | { kind: "repeat"; sub: Node; min: number; max: number | null; greedy: boolean }
  | { kind: "group"; sub: Node; index: number } // index>0 capturing (1-based), 0 non-capturing
  | { kind: "anchorStart" }
  | { kind: "anchorEnd" }
  | { kind: "lineStart" }
  | { kind: "lineEnd" };

// ---------------------------------------------------------------------------
// Parser: pattern text -> AST (chars are code-point strings via Array.from)
// ---------------------------------------------------------------------------

// A compile mode of jsonpath like_regex (regex.md §8d); NO_MODE is the SQL operators' mode.
export type RegexMode = {
  // nlStop keeps a negated bracket class from matching a newline (PostgreSQL's REG_NLSTOP).
  nlStop: boolean;
  // dotAll lets `.` match a newline (the XQuery `s` flag).
  dotAll: boolean;
  // nlAnchor lets `^`/`$` also anchor just after / before a newline (the XQuery `m` flag,
  // PostgreSQL's REG_NLANCH). `\A`/`\z` stay string-anchored.
  nlAnchor: boolean;
};

const NO_MODE: RegexMode = { nlStop: false, dotAll: false, nlAnchor: false };

// quoteRegex escapes every metacharacter of a pattern so it compiles to the literal string (the
// XQuery `q` flag, regex.md §8d).
export function quoteRegex(pattern: string): string {
  let out = "";
  for (const c of pattern) out += isRegexMeta(c) ? "\\" + c : c;
  return out;
}

class RegexParser {
  private chars: string[];
  private pos = 0;
  private readonly mode: RegexMode;
  ngroups = 0;

  constructor(pattern: string, mode: RegexMode) {
    this.chars = Array.from(pattern);
    this.mode = mode;
  }

  private peek(): string | undefined {
    return this.chars[this.pos];
  }
  private peekAt(k: number): string | undefined {
    return this.chars[this.pos + k];
  }
  private bump(): string | undefined {
    const c = this.chars[this.pos];
    if (c !== undefined) this.pos++;
    return c;
  }
  atEnd(): boolean {
    return this.pos >= this.chars.length;
  }

  // alternation (loosest): concat ('|' concat)*, right-folded (`a|b|c` == `a|(b|c)`).
  parseAlt(): Node {
    const left = this.parseConcat();
    if (this.peek() === "|") {
      this.bump();
      const right = this.parseAlt();
      return { kind: "alt", left, right };
    }
    return left;
  }

  private parseConcat(): Node {
    const nodes: Node[] = [];
    for (;;) {
      const c = this.peek();
      if (c === undefined || c === "|" || c === ")") break;
      nodes.push(this.parseQuant());
    }
    if (nodes.length === 0) return { kind: "empty" };
    if (nodes.length === 1) return nodes[0];
    return { kind: "concat", subs: nodes };
  }

  private parseQuant(): Node {
    const atom = this.parseAtom();
    let kind: "star" | "plus" | "quest" | "repeat" | null = null;
    let repMin = 0;
    let repMax: number | null = 0;
    switch (this.peek()) {
      case "*":
        this.bump();
        kind = "star";
        break;
      case "+":
        this.bump();
        kind = "plus";
        break;
      case "?":
        this.bump();
        kind = "quest";
        break;
      case "{": {
        const iv = this.tryInterval();
        if (iv === null) return atom;
        kind = "repeat";
        repMin = iv.min;
        repMax = iv.max;
        break;
      }
    }
    if (kind === null) return atom;
    // An optional trailing `?` makes the quantifier lazy.
    let greedy = true;
    if (this.peek() === "?") {
      this.bump();
      greedy = false;
    }
    // A second quantifier with no atom between (`a**`, `a*+`) is invalid (regex.md §2).
    const next = this.peek();
    if (next === "*" || next === "+" || next === "?") {
      throw regexInvalid("quantifier operand missing");
    }
    switch (kind) {
      case "star":
        return { kind: "star", sub: atom, greedy };
      case "plus":
        return { kind: "plus", sub: atom, greedy };
      case "quest":
        return { kind: "quest", sub: atom, greedy };
      case "repeat":
        return { kind: "repeat", sub: atom, min: repMin, max: repMax, greedy };
    }
  }

  // tryInterval reads a `{n}`, `{n,}`, or `{n,m}` at the cursor. On a non-interval `{` the cursor is
  // unmoved and null is returned (the `{` is later read as a literal — the lenient-brace rule).
  // `{n,m}` with m<n is 2201B. max=null means unbounded.
  private tryInterval(): { min: number; max: number | null } | null {
    const start = this.pos;
    this.bump(); // '{'
    const min = this.readCount();
    if (min === null) {
      this.pos = start;
      return null;
    }
    const c = this.peek();
    if (c === "}") {
      this.bump();
      return { min, max: min };
    }
    if (c === ",") {
      this.bump();
      if (this.peek() === "}") {
        this.bump();
        return { min, max: null };
      }
      const hi = this.readCount();
      if (hi === null || this.peek() !== "}") {
        this.pos = start;
        return null;
      }
      this.bump(); // '}'
      if (hi < min) throw regexInvalid("invalid repetition count");
      return { min, max: hi };
    }
    this.pos = start;
    return null;
  }

  // readCount reads ASCII digits as a count, saturating at MAX_REGEX_PROGRAM+1 so a giant interval
  // cannot overflow and is rejected 54001 at emit. Returns null if no digit is present.
  private readCount(): number | null {
    let any = false;
    let n = 0;
    for (;;) {
      const c = this.peek();
      if (c === undefined || c < "0" || c > "9") break;
      any = true;
      this.bump();
      n = n * 10 + (c.charCodeAt(0) - 0x30);
      if (n > MAX_REGEX_PROGRAM) n = MAX_REGEX_PROGRAM + 1;
    }
    return any ? n : null;
  }

  private parseAtom(): Node {
    const c = this.peek() as string; // parseConcat guards against end / | / )
    switch (c) {
      case "(":
        return this.parseGroup();
      case "[":
        return this.parseClass();
      case ".":
        this.bump();
        // Dot-all: an empty negated class admits every code point (regex.md §8d).
        if (this.mode.dotAll) return { kind: "class", class: { negated: true, ranges: [] } };
        return { kind: "any" };
      case "^":
        this.bump();
        return this.mode.nlAnchor ? { kind: "lineStart" } : { kind: "anchorStart" };
      case "$":
        this.bump();
        return this.mode.nlAnchor ? { kind: "lineEnd" } : { kind: "anchorEnd" };
      case "\\":
        this.bump();
        return this.parseEscape();
      case "*":
      case "+":
      case "?":
        // A quantifier where an atom is expected (`*ab`, `a|*`) is invalid (regex.md §2).
        throw regexInvalid("quantifier operand missing");
      default:
        // `{`, `}`, `]` are literals here (a `{` starting a valid interval is consumed by parseQuant
        // before reaching parseAtom — the lenient-brace rule, regex.md §2).
        this.bump();
        return { kind: "char", ch: c.codePointAt(0) as number };
    }
  }

  private parseGroup(): Node {
    this.bump(); // '('
    let capturing = true;
    if (this.peek() === "?") {
      // `(?:...)` is non-capturing; any other `(?...)` is an excluded construct (regex.md §2).
      if (this.peekAt(1) === ":") {
        this.bump();
        this.bump();
        capturing = false;
      } else {
        throw regexInvalid("unsupported group syntax");
      }
    }
    let index = 0;
    if (capturing) {
      this.ngroups++;
      index = this.ngroups;
    }
    const inner = this.parseAlt();
    if (this.peek() !== ")") throw regexInvalid("unbalanced parenthesis");
    this.bump(); // ')'
    return { kind: "group", sub: inner, index };
  }

  private parseEscape(): Node {
    const c = this.bump();
    if (c === undefined) throw regexInvalid("trailing backslash");
    // String-boundary anchors (regex.md §2): `\A` = start of subject, `\z` = absolute end of
    // subject. jed has no multiline mode, so these are exactly `^`/`$` today; they are spelled
    // separately so they stay string-anchored once a line mode lands. PCRE's `\Z` (end, or before
    // a trailing newline) is deliberately NOT accepted — its only distinguishing behavior is
    // trailing-newline leniency jed does nowhere, so it stays an invalid escape.
    if (c === "A") return { kind: "anchorStart" };
    if (c === "z") return { kind: "anchorEnd" };
    const pc = predefClass(c);
    if (pc !== null) return { kind: "class", class: { negated: pc.negated, ranges: pc.ranges } };
    const ctrl = controlEscape(c);
    if (ctrl !== null) return { kind: "char", ch: ctrl };
    // Any other escaped code point that is not an ASCII letter or digit is itself (regex.md §2).
    if (isRegexMeta(c) || !isAsciiAlnum(c)) return { kind: "char", ch: c.codePointAt(0) as number };
    throw regexInvalid(`invalid escape \\${c}`);
  }

  private parseClass(): Node {
    this.bump(); // '['
    let negated = false;
    if (this.peek() === "^") {
      this.bump();
      negated = true;
    }
    const ranges: [number, number][] = [];
    let first = true;
    for (;;) {
      const c = this.peek();
      if (c === undefined) throw regexInvalid("unbalanced bracket expression");
      if (c === "]" && !first) {
        this.bump();
        break;
      }
      const item = this.classItem();
      if (item.set !== null) {
        ranges.push(...item.set);
        first = false;
        continue;
      }
      const lo = item.ch as number;
      // `lo-hi` is a range only when `-` is followed by a real high end (not `]`).
      if (this.peek() === "-" && this.peekAt(1) !== undefined && this.peekAt(1) !== "]") {
        this.bump(); // '-'
        const hiItem = this.classItem();
        if (hiItem.set !== null) {
          // `[\d-a]` etc. — lenient: the `-` is a literal and the set is added.
          ranges.push([lo, lo], [0x2d, 0x2d], ...hiItem.set);
        } else {
          const hi = hiItem.ch as number;
          if (lo > hi) throw regexInvalid("invalid range in bracket expression");
          ranges.push([lo, hi]);
        }
        first = false;
        continue;
      }
      ranges.push([lo, lo]);
      first = false;
    }
    // Newline-sensitive: `[^…]` does not match a newline (regex.md §8d).
    if (negated && this.mode.nlStop) ranges.push([0x0a, 0x0a]);
    return { kind: "class", class: { negated, ranges: normalizeRanges(ranges) } };
  }

  // classItem parses one item inside a `[...]`: a predefined class becomes a set; anything else a
  // single code point (escapes resolved).
  private classItem(): { ch: number | null; set: [number, number][] | null } {
    const c = this.bump() as string;
    if (c !== "\\") return { ch: c.codePointAt(0) as number, set: null };
    const e = this.bump();
    if (e === undefined) throw regexInvalid("trailing backslash");
    const pc = predefClass(e);
    if (pc !== null) {
      return {
        ch: null,
        set: pc.negated ? complementRanges(normalizeRanges(pc.ranges)) : pc.ranges,
      };
    }
    const ctrl = controlEscape(e);
    if (ctrl !== null) return { ch: ctrl, set: null };
    if (isRegexMeta(e) || !isAsciiAlnum(e)) return { ch: e.codePointAt(0) as number, set: null };
    throw regexInvalid(`invalid escape \\${e}`);
  }
}

// isAsciiAlnum reports whether c is an ASCII letter or digit, the escapes regex.md §2 reserves.
function isAsciiAlnum(c: string): boolean {
  return /^[0-9A-Za-z]$/.test(c);
}

function isRegexMeta(c: string): boolean {
  return ".*+?()[]{}|^$\\".includes(c) && c.length === 1;
}

function controlEscape(c: string): number | null {
  switch (c) {
    case "n":
      return 0x0a;
    case "t":
      return 0x09;
    case "r":
      return 0x0d;
    case "f":
      return 0x0c;
    case "v":
      return 0x0b;
  }
  return null;
}

// predefClass returns the predefined classes \d \w \s (and their negations): positive ranges plus
// whether the letter was the negated (uppercase) form. ASCII baseline for Slice 1.
function predefClass(c: string): { ranges: [number, number][]; negated: boolean } | null {
  switch (c) {
    case "d":
      return { ranges: [[48, 57]], negated: false };
    case "D":
      return { ranges: [[48, 57]], negated: true };
    case "w":
      return {
        ranges: [
          [48, 57],
          [65, 90],
          [95, 95],
          [97, 122],
        ],
        negated: false,
      };
    case "W":
      return {
        ranges: [
          [48, 57],
          [65, 90],
          [95, 95],
          [97, 122],
        ],
        negated: true,
      };
    case "s":
      return {
        ranges: [
          [9, 13],
          [32, 32],
        ],
        negated: false,
      };
    case "S":
      return {
        ranges: [
          [9, 13],
          [32, 32],
        ],
        negated: true,
      };
  }
  return null;
}

// normalizeRanges sorts by lo and merges touching/overlapping ranges (regex.md §3.4).
function normalizeRanges(ranges: [number, number][]): [number, number][] {
  const sorted = [...ranges].sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  const out: [number, number][] = [];
  for (const [lo, hi] of sorted) {
    const last = out[out.length - 1];
    if (last !== undefined && lo <= last[1] + 1) {
      if (hi > last[1]) last[1] = hi;
      continue;
    }
    out.push([lo, hi]);
  }
  return out;
}

// complementRanges returns the complement of normalized ranges over [0, MAX_REGEX_CP].
function complementRanges(ranges: [number, number][]): [number, number][] {
  const out: [number, number][] = [];
  let next = 0;
  for (const [lo, hi] of ranges) {
    if (lo > next) out.push([next, lo - 1]);
    next = hi + 1;
    if (next > MAX_REGEX_CP) return out;
  }
  if (next <= MAX_REGEX_CP) out.push([next, MAX_REGEX_CP]);
  return out;
}

// ---------------------------------------------------------------------------
// Compiler: AST -> bytecode (the exact emission of regex.md §3)
// ---------------------------------------------------------------------------

class RegexCompiler {
  insts: Inst[] = [];
  classes: RegexClass[] = [];

  pushInst(inst: Inst): number {
    if (this.insts.length >= MAX_REGEX_PROGRAM) throw regexTooComplex();
    const i = this.insts.length;
    this.insts.push(inst);
    return i;
  }

  emit(n: Node): void {
    switch (n.kind) {
      case "empty":
        return;
      case "char":
        this.pushInst({ op: "char", x: 0, y: 0, ch: n.ch });
        return;
      case "any":
        this.pushInst({ op: "any", x: 0, y: 0, ch: 0 });
        return;
      case "class": {
        const k = this.classes.length;
        this.classes.push(n.class);
        this.pushInst({ op: "class", x: k, y: 0, ch: 0 });
        return;
      }
      case "concat":
        for (const s of n.subs) this.emit(s);
        return;
      case "alt": {
        // split LX,LY ; LX: <a>; jmp LEND ; LY: <b>; LEND:
        const split = this.pushInst({ op: "split", x: 0, y: 0, ch: 0 });
        const lx = this.insts.length;
        this.emit(n.left);
        const jmp = this.pushInst({ op: "jmp", x: 0, y: 0, ch: 0 });
        const ly = this.insts.length;
        this.emit(n.right);
        const lend = this.insts.length;
        this.insts[split].x = lx;
        this.insts[split].y = ly;
        this.insts[jmp].x = lend;
        return;
      }
      case "star": {
        if (nullable(n.sub)) {
          // A star over a NULLABLE body is lowered as `(sub+)?` (RE2's rule, regex.md §3.3): the
          // plain loop's `jmp L1` re-enters a pc the epsilon closure already visited, so the thread
          // that took the body's empty iteration dies and a capture inside it stays unset (`(a*)*`
          // on '' → {NULL}); `(sub+)?` enters the body first and loops from its end, so the empty
          // iteration is recorded ({""}, as PostgreSQL/RE2/PCRE).
          const plus: Node = { kind: "plus", sub: n.sub, greedy: n.greedy };
          this.emit({ kind: "quest", sub: plus, greedy: n.greedy });
          return;
        }
        // L1: split L2,L3 (greedy) / split L3,L2 (lazy) ; L2: <sub>; jmp L1 ; L3:
        const l1 = this.pushInst({ op: "split", x: 0, y: 0, ch: 0 });
        const l2 = this.insts.length;
        this.emit(n.sub);
        this.pushInst({ op: "jmp", x: l1, y: 0, ch: 0 });
        const l3 = this.insts.length;
        this.insts[l1].x = n.greedy ? l2 : l3;
        this.insts[l1].y = n.greedy ? l3 : l2;
        return;
      }
      case "plus": {
        // L1: <sub>; split L1,L3 (greedy) / split L3,L1 (lazy) ; L3:
        const l1 = this.insts.length;
        this.emit(n.sub);
        const split = this.pushInst({ op: "split", x: 0, y: 0, ch: 0 });
        const l3 = this.insts.length;
        this.insts[split].x = n.greedy ? l1 : l3;
        this.insts[split].y = n.greedy ? l3 : l1;
        return;
      }
      case "quest": {
        // split L1,L2 (greedy) / split L2,L1 (lazy) ; L1: <sub>; L2:
        const split = this.pushInst({ op: "split", x: 0, y: 0, ch: 0 });
        const l1 = this.insts.length;
        this.emit(n.sub);
        const l2 = this.insts.length;
        this.insts[split].x = n.greedy ? l1 : l2;
        this.insts[split].y = n.greedy ? l2 : l1;
        return;
      }
      case "repeat":
        this.emitRepeat(n);
        return;
      case "group":
        if (n.index > 0) {
          this.pushInst({ op: "save", x: 2 * n.index, y: 0, ch: 0 });
          this.emit(n.sub);
          this.pushInst({ op: "save", x: 2 * n.index + 1, y: 0, ch: 0 });
        } else {
          this.emit(n.sub);
        }
        return;
      case "anchorStart":
        this.pushInst({ op: "assertStart", x: 0, y: 0, ch: 0 });
        return;
      case "anchorEnd":
        this.pushInst({ op: "assertEnd", x: 0, y: 0, ch: 0 });
        return;
      case "lineStart":
        this.pushInst({ op: "assertLineStart", x: 0, y: 0, ch: 0 });
        return;
      case "lineEnd":
        this.pushInst({ op: "assertLineEnd", x: 0, y: 0, ch: 0 });
        return;
    }
  }

  // emitRepeat unrolls `{min,max}` -> min mandatory copies, then a star ({min,}) or (max-min)
  // greedy/lazy quest copies. Each copy's emit checks the cap, so a giant interval aborts 54001.
  private emitRepeat(n: { sub: Node; min: number; max: number | null; greedy: boolean }): void {
    for (let i = 0; i < n.min; i++) this.emit(n.sub);
    if (n.max === null) {
      this.emit({ kind: "star", sub: n.sub, greedy: n.greedy });
      return;
    }
    for (let i = 0; i < n.max - n.min; i++) {
      this.emit({ kind: "quest", sub: n.sub, greedy: n.greedy });
    }
  }
}

// nullable reports whether a node can match the empty string (RE2's `nullable` flag): the star
// lowering branches on it (regex.md §3.3). Zero-width assertions are nullable; a repeat with min 0 is
// nullable, otherwise it is as nullable as its body.
function nullable(n: Node): boolean {
  switch (n.kind) {
    case "char":
    case "any":
    case "class":
      return false;
    case "concat":
      return n.subs.every(nullable);
    case "alt":
      return nullable(n.left) || nullable(n.right);
    case "plus":
    case "group":
      return nullable(n.sub);
    case "repeat":
      return n.min === 0 || nullable(n.sub);
    default: // empty, star, quest, the anchors
      return true;
  }
}

// foldRegexPattern is the case-insensitive pattern fold (regex.md §1): foldLowerSimple over the
// pattern's literal characters and class members, with the code point after each backslash kept as
// written. Every escape is a backslash plus one code point (§2), so this leaves escape letters intact —
// `\D \W \S` stay negated classes rather than becoming `\d \w \s`, and `\A` stays an anchor.
export function foldRegexPattern(pat: string, prop: PropertyTable | undefined): string {
  let out = "";
  let start = 0;
  for (let i = 0; i < pat.length; i++) {
    if (pat.charCodeAt(i) !== 0x5c) continue;
    out += foldLowerSimple(pat.slice(start, i), prop);
    let end = i + 1;
    if (end < pat.length) end += (pat.codePointAt(end) as number) > 0xffff ? 2 : 1;
    out += pat.slice(i, end);
    start = end;
    i = end - 1;
  }
  return out + foldLowerSimple(pat.slice(start), prop);
}

// compileRegex compiles a pattern to a program (regex.md §3). Throws 2201B on a malformed pattern
// and 54001 on a well-formed-but-too-large one. Does NOT meter — the caller charges
// regex_compile × regexNinst() (the precompilation contract, regex.md §5). For ~* the pattern must
// already be case-folded by the caller (foldRegexPattern).
export function compileRegex(pattern: string): RegexProgram {
  return compileRegexMode(pattern, NO_MODE);
}

// compileRegexMode is compileRegex under a jsonpath like_regex compile mode (regex.md §8d).
export function compileRegexMode(pattern: string, mode: RegexMode): RegexProgram {
  return compileRegexWrapped(pattern, mode, "search");
}

// The program wrapper around a pattern body: §3.2's unanchored search, or one of the gated wrappers
// of regex.md §8f, whose prefix is lazy (the leftmost gated start) or greedy (the rightmost).
type RegexWrapper = "search" | "gatedLeftmost" | "gatedRightmost";

function compileRegexWrapped(
  pattern: string,
  mode: RegexMode,
  wrapper: RegexWrapper,
): RegexProgram {
  const parser = new RegexParser(pattern, mode);
  const root = parser.parseAlt();
  if (!parser.atEnd()) throw regexInvalid("unbalanced parenthesis");
  const ngroups = parser.ngroups;
  const c = new RegexCompiler();
  // Wrapper (regex.md §3.2): lazy `.*?` prefix + group-0 save + match. The gated wrappers (§8f) add
  // a gate after the prefix, whose arms the rightmost form swaps.
  if (wrapper === "gatedRightmost") c.pushInst({ op: "split", x: 1, y: 3, ch: 0 });
  else c.pushInst({ op: "split", x: 3, y: 1, ch: 0 });
  c.pushInst({ op: "anyChar", x: 0, y: 0, ch: 0 });
  c.pushInst({ op: "jmp", x: 0, y: 0, ch: 0 });
  if (wrapper !== "search") c.pushInst({ op: "gate", x: 0, y: 0, ch: 0 });
  c.pushInst({ op: "save", x: 0, y: 0, ch: 0 });
  c.emit(root);
  c.pushInst({ op: "save", x: 1, y: 0, ch: 0 });
  c.pushInst({ op: "match", x: 0, y: 0, ch: 0 });
  const [capDepth, capLeafLen] = capShape(2 * (ngroups + 1));
  return { insts: c.insts, classes: c.classes, ngroups, capDepth, capLeafLen, seen: null };
}

// ---------------------------------------------------------------------------
// Capture slots — a persistent array (regex.md §4.1)
// ---------------------------------------------------------------------------

// A capture-slot tree's leaves hold CAP_FAN slots and its interior nodes CAP_FAN subtrees. Every
// group emits two saves, so a program's 2·(ngroups+1) slots never exceed MAX_REGEX_PROGRAM and fit in
// four levels.
const CAP_FAN_BITS = 4;
const CAP_FAN = 1 << CAP_FAN_BITS;

// The entries of a node no save has reached, copied to start one: every slot unset, every subtree
// missing. Built packed (Array.from), since slicing a packed array is the cheapest copy.
const UNSET_SLOTS: readonly number[] = Array.from({ length: CAP_FAN }, () => -1);
const NO_SUBTREES: readonly (CapNode | null)[] = Array.from({ length: CAP_FAN }, () => null);

// A node of a thread's capture slots, a persistent array (regex.md §4.1). A save makes a new version
// by copying only the nodes on the path to the slot it sets, at most CAP_FAN entries per level, so its
// work is bounded however many groups the pattern has; the older version stays intact for the threads
// still holding it. A leaf is its slot values (code-point offsets; -1 = unset), an interior node its
// subtrees. A null node means every slot under it is unset, so a search starts from a null root. A
// match has set slots 0 and 1, so its root is never null.
export type CapNode = number[] | (CapNode | null)[];

// capShape returns the depth of the capture-slot tree for nslots slots and the length of its leaves:
// a one-level tree is a single leaf of exactly nslots slots, a deeper one has full leaves.
function capShape(nslots: number): [number, number] {
  let depth = 1;
  for (let span = CAP_FAN; span < nslots; span *= CAP_FAN) depth++;
  return depth === 1 ? [1, nslots] : [depth, CAP_FAN];
}

// regexSlot returns capture slot i of a thread's or match's slots (-1 = unset).
export function regexSlot(p: RegexProgram, caps: CapNode | null, i: number): number {
  let n = caps;
  for (
    let shift = CAP_FAN_BITS * (p.capDepth - 1);
    shift > 0 && n !== null;
    shift -= CAP_FAN_BITS
  ) {
    n = (n as (CapNode | null)[])[(i >> shift) & (CAP_FAN - 1)];
  }
  return n === null ? -1 : (n as number[])[i & (CAP_FAN - 1)];
}

// withSlot returns the version of caps with slot i set to v, sharing every node off the path to it.
function withSlot(p: RegexProgram, caps: CapNode | null, i: number, v: number): CapNode {
  if (p.capDepth === 1) {
    // A one-leaf pattern's slots are one flat array, copied whole.
    const flat = caps === null ? UNSET_SLOTS.slice(0, p.capLeafLen) : (caps as number[]).slice();
    flat[i] = v;
    return flat;
  }
  return withSlotAt(p, caps, CAP_FAN_BITS * (p.capDepth - 1), i, v);
}

function withSlotAt(
  p: RegexProgram,
  n: CapNode | null,
  shift: number,
  i: number,
  v: number,
): CapNode {
  if (shift === 0) {
    const vals = n === null ? UNSET_SLOTS.slice(0, p.capLeafLen) : (n as number[]).slice();
    vals[i & (CAP_FAN - 1)] = v;
    return vals;
  }
  const kids = n === null ? NO_SUBTREES.slice() : (n as (CapNode | null)[]).slice();
  const k = (i >> shift) & (CAP_FAN - 1);
  kids[k] = withSlotAt(p, kids[k], shift - CAP_FAN_BITS, i, v);
  return kids;
}

// regexSlots returns every capture slot of caps as a flat array.
export function regexSlots(p: RegexProgram, caps: CapNode): number[] {
  const out: number[] = [];
  for (let i = 0; i < 2 * (p.ngroups + 1); i++) out.push(regexSlot(p, caps, i));
  return out;
}

// ---------------------------------------------------------------------------
// Pike VM (regex.md §4)
// ---------------------------------------------------------------------------

type Thread = { pc: number; caps: CapNode | null };

// The per-position dedup set (regex.md §4): marks[pc] === generation means pc was visited at the
// current position, so moving to the next position is an increment, not a clear. A program keeps its
// set and reuses it across searches and rows, so a search does no setup work in proportion to the
// program, only the states it explores, which regex_step meters (§4.2).
export type RegexSeen = { marks: Int32Array; generation: number };

// nextGeneration starts a new generation. When the counter wraps, the stale marks could alias the new
// generation, so they are cleared.
function nextGeneration(seen: RegexSeen): number {
  if (seen.generation === 0x7fffffff) {
    seen.marks.fill(0);
    seen.generation = 0;
  }
  return ++seen.generation;
}

// takeSeen returns a dedup set for one call's searches, the program's own when it holds one. The
// caller gives it back by assigning p.seen.
function takeSeen(p: RegexProgram): RegexSeen {
  const seen = p.seen ?? { marks: new Int32Array(p.insts.length), generation: 0 };
  p.seen = null;
  return seen;
}

// withSeen runs f with a dedup set for one call's searches (takeSeen) and gives it back afterwards.
function withSeen<T>(p: RegexProgram, f: (seen: RegexSeen) => T): T {
  const seen = takeSeen(p);
  try {
    return f(seen);
  } finally {
    p.seen = seen;
  }
}

// regexIsMatch reports whether the pattern matches somewhere in input (the `~` operator).
export function regexIsMatch(p: RegexProgram, input: number[], m: Meter): boolean {
  return regexRun(p, input, m) !== null;
}

// regexRun executes the Pike VM from the start of the input (regex.md §4). Returns the winning
// thread's capture slots on a match, or null.
export function regexRun(p: RegexProgram, input: number[], m: Meter): CapNode | null {
  const seen = takeSeen(p);
  try {
    return regexSearch(p, seen, input, 0, m);
  } finally {
    p.seen = seen;
  }
}

// regexGatedStart returns the start of the match of a gated program (regex.md §8f) whose start is in
// gate (one flag per position, input.length + 1 of them): the leftmost such start under the lazy
// prefix, the rightmost under the greedy one; null for no match.
export function regexGatedStart(
  p: RegexProgram,
  input: number[],
  gate: boolean[],
  m: Meter,
): number | null {
  const caps = withSeen(p, (seen) => regexSearch(p, seen, input, 0, m, gate, null));
  return caps === null ? null : regexSlot(p, caps, 0);
}

// regexMatchEnds is an ends run of a gated program (regex.md §8f): every position at which a match
// starting in gate can end, as one flag per position.
export function regexMatchEnds(
  p: RegexProgram,
  input: number[],
  gate: boolean[],
  m: Meter,
): boolean[] {
  const ends: boolean[] = new Array(input.length + 1).fill(false);
  withSeen(p, (seen) => regexSearch(p, seen, input, 0, m, gate, ends));
  return ends;
}

// regexSearch runs the Pike VM, considering only matches that START at code-point position `start` or
// later (the unanchored search seeds its lazy `.*?` prefix at `start`); ^/$ still anchor at the true
// input bounds. Used by regexp_replace's global loop. Charges regex_step per explored state and
// guards once per input position. `gate` is the gate set of a gated program (null outside the gated
// wrappers) and `ends` the end set an ends run fills (regex.md §8f).
function regexSearch(
  p: RegexProgram,
  seen: RegexSeen,
  input: number[],
  start: number,
  m: Meter,
  gate: boolean[] | null = null,
  ends: boolean[] | null = null,
): CapNode | null {
  const len = input.length;
  let clist: Thread[] = [];
  let nlist: Thread[] = [];
  let matched: CapNode | null = null;

  const marks = seen.marks;
  addThread(p, clist, marks, nextGeneration(seen), 0, null, start, input, m, gate);

  for (let sp = start; sp <= len; sp++) {
    const generation = nextGeneration(seen);
    nlist.length = 0;
    inner: for (let i = 0; i < clist.length; i++) {
      const th = clist[i];
      const inst = p.insts[th.pc];
      switch (inst.op) {
        case "char":
          if (sp < len && input[sp] === inst.ch) {
            addThread(p, nlist, marks, generation, th.pc + 1, th.caps, sp + 1, input, m, gate);
          }
          break;
        case "any":
          if (sp < len && input[sp] !== 0x0a) {
            addThread(p, nlist, marks, generation, th.pc + 1, th.caps, sp + 1, input, m, gate);
          }
          break;
        case "anyChar":
          if (sp < len) {
            addThread(p, nlist, marks, generation, th.pc + 1, th.caps, sp + 1, input, m, gate);
          }
          break;
        case "class":
          if (sp < len && classAdmits(p.classes[inst.x], input[sp])) {
            addThread(p, nlist, marks, generation, th.pc + 1, th.caps, sp + 1, input, m, gate);
          }
          break;
        case "match":
          // An ends run records the position and keeps every thread (regex.md §8f).
          if (ends !== null) {
            ends[sp] = true;
            break;
          }
          matched = th.caps;
          break inner; // cut lower-priority threads (leftmost-first, regex.md §4)
      }
    }
    const tmp = clist;
    clist = nlist;
    nlist = tmp;
    m.guard(); // §6 ceiling, once per input position
    if (clist.length === 0) break;
  }
  return matched;
}

// addThread performs the epsilon-closure: follow jmp/split/save/assert from pc, appending
// consuming/match threads to `list`, deduping by pc within this generation. Iterative (explicit
// stack) so a long jmp/split chain cannot overflow the native stack; the y arm of a split is pushed
// before x so x is processed first (higher priority). Charges regex_step per explored state.
function addThread(
  p: RegexProgram,
  list: Thread[],
  marks: Int32Array,
  generation: number,
  pc0: number,
  caps0: CapNode | null,
  sp: number,
  input: number[],
  m: Meter,
  gate: boolean[] | null,
): void {
  const len = input.length;
  const stack: Thread[] = [{ pc: pc0, caps: caps0 }];
  while (stack.length > 0) {
    const top = stack.pop() as Thread;
    if (marks[top.pc] === generation) continue;
    marks[top.pc] = generation;
    m.charge(COSTS.regexStep);
    const inst = p.insts[top.pc];
    switch (inst.op) {
      case "jmp":
        stack.push({ pc: inst.x, caps: top.caps });
        break;
      case "split":
        // Push y first, then x, so x pops first = higher priority.
        stack.push({ pc: inst.y, caps: top.caps });
        stack.push({ pc: inst.x, caps: top.caps });
        break;
      case "save":
        stack.push({ pc: top.pc + 1, caps: withSlot(p, top.caps, inst.x, sp) });
        break;
      case "assertStart":
        if (sp === 0) stack.push({ pc: top.pc + 1, caps: top.caps });
        break;
      case "assertEnd":
        if (sp === len) stack.push({ pc: top.pc + 1, caps: top.caps });
        break;
      case "assertLineStart":
        if (sp === 0 || input[sp - 1] === 0x0a) stack.push({ pc: top.pc + 1, caps: top.caps });
        break;
      case "assertLineEnd":
        if (sp === len || input[sp] === 0x0a) stack.push({ pc: top.pc + 1, caps: top.caps });
        break;
      case "gate":
        if (gate?.[sp]) stack.push({ pc: top.pc + 1, caps: top.caps });
        break;
      default:
        // char / any / class / match — parked for the consume loop.
        list.push(top);
        break;
    }
  }
}

// replaceMatches finds the matches regexp_replace(source, pattern, replacement, …) (regex.md §8)
// replaces: the first match, or every non-overlapping match when global. Each entry is a match's
// capture slots. The VM matches over matchInput (possibly case-folded); this is the kernel's whole
// search, so it charges every regex_step the replacement costs. replaceSize and replaceBuild then size
// and splice from these slots without searching again (cost.md §8.1).
// regexp_match (never global) and regexp_matches run the same search, then size and slice their
// capture arrays from the slots (captureArraySize, captureArray).
export function replaceMatches(
  p: RegexProgram,
  matchInput: number[],
  global: boolean,
  m: Meter,
): CapNode[] {
  return withSeen(p, (seen) => {
    const found: CapNode[] = [];
    let pos = 0;
    for (;;) {
      const caps = regexSearch(p, seen, matchInput, pos, m);
      if (caps === null) break;
      const s = regexSlot(p, caps, 0);
      const e = regexSlot(p, caps, 1);
      found.push(caps);
      if (!global) break;
      // Empty match: the next search starts past the char at `e`, so a pattern that can match empty
      // (`a*`) cannot loop forever — the PG global rule.
      pos = e > s ? e : e + 1;
      if (pos > matchInput.length) break;
    }
    return found;
  });
}

// reportedGroups returns the capture groups regexp_match / regexp_matches report for a match (regex.md
// §8): groups 1..n, or group 0 (the whole match) when the pattern has no group — the PG rule — as the
// inclusive range [lo, hi].
function reportedGroups(p: RegexProgram): [number, number] {
  return p.ngroups === 0 ? [0, 0] : [1, p.ngroups];
}

// captureArraySize is the exact payload (memory.md §3) of the one-dimensional text[] capture array for
// a match's slots, computed from the spans without building it (cost.md §8.1): ARRAY_DIM, plus VALUE
// and the span's UTF-8 bytes per reported group (an unset group is a bare VALUE). utf8At is
// utf8Offsets of the subject, so each span is measured in O(1).
export function captureArraySize(p: RegexProgram, caps: CapNode, utf8At: number[]): number {
  let size = MEMORY_ARRAY_DIM;
  const [lo, hi] = reportedGroups(p);
  for (let g = lo; g <= hi; g++) {
    const s = regexSlot(p, caps, 2 * g);
    const e = regexSlot(p, caps, 2 * g + 1);
    size += MEMORY_VALUE;
    if (s >= 0 && e >= 0) size += utf8At[e] - utf8At[s];
  }
  return size;
}

// captureArray returns the capture array for a match's slots, sliced from the ORIGINAL-case subject:
// the reported groups' substrings, an unset group null.
export function captureArray(p: RegexProgram, caps: CapNode, orig: number[]): (string | null)[] {
  const [lo, hi] = reportedGroups(p);
  const groups: (string | null)[] = [];
  for (let g = lo; g <= hi; g++) {
    groups.push(sliceGroup(orig, regexSlot(p, caps, 2 * g), regexSlot(p, caps, 2 * g + 1)));
  }
  return groups;
}

// utf8Offsets returns the UTF-8 byte offset of every code point of orig, plus the total at
// orig.length: the span [s, e) is offsets[e] - offsets[s] bytes.
export function utf8Offsets(orig: number[]): number[] {
  const offsets = new Array<number>(orig.length + 1);
  let at = 0;
  offsets[0] = 0;
  for (let i = 0; i < orig.length; i++) {
    at += cpUtf8Len(orig[i]);
    offsets[i + 1] = at;
  }
  return offsets;
}

// cpsUtf8Length returns the UTF-8 byte length of the code points cps.
export function cpsUtf8Length(cps: number[]): number {
  let n = 0;
  for (const cp of cps) n += cpUtf8Len(cp);
  return n;
}

// regexpSplitPieces returns the pieces of regexp_split_to_array / regexp_split_to_table (regex.md
// §8c): the text between consecutive matches, found with the global advance, then the text after the
// last. PostgreSQL's DEGENERATE matches split nothing: an empty match at the start or end of the
// subject, or one that ends no later than the previous match — so '' splits into characters and a
// split of the empty string is one empty piece. The pieces are sliced from the ORIGINAL-case subject
// (same length as the matched one).
export function regexpSplitPieces(
  p: RegexProgram,
  matchInput: number[],
  origInput: number[],
  m: Meter,
): string[] {
  return withSeen(p, (seen) => {
    const len = matchInput.length;
    const pieces: string[] = [];
    let pos = 0;
    let prevEnd = 0; // the end of the previous match, degenerate or not
    let pieceStart = 0; // the end of the previous SPLITTING match
    while (pos <= len) {
      const caps = regexSearch(p, seen, matchInput, pos, m);
      if (caps === null) break;
      const s = regexSlot(p, caps, 0);
      const e = regexSlot(p, caps, 1);
      if (s < len && e > prevEnd) {
        pieces.push(cpSlice(origInput, pieceStart, s));
        pieceStart = e;
      }
      prevEnd = e;
      pos = e > s ? e : e + 1;
    }
    pieces.push(cpSlice(origInput, pieceStart, len));
    return pieces;
  });
}

// regexpCount counts the non-overlapping matches at or after code-point position `start`
// (regexp_count, regex.md §8b). The advance is regexpReplace's global rule: after a match [s,e)
// continue at e, or at e+1 for an EMPTY match so a nullable pattern terminates. `start` may be up to
// len (an empty match at the very end still counts); start > len (clamped to len+1 by the caller)
// yields 0.
export function regexpCount(p: RegexProgram, input: number[], start: number, m: Meter): number {
  return withSeen(p, (seen) => {
    const len = input.length;
    let pos = start;
    let count = 0;
    while (pos <= len) {
      const caps = regexSearch(p, seen, input, pos, m);
      if (caps === null) break;
      count++;
      const s = regexSlot(p, caps, 0);
      const e = regexSlot(p, caps, 1);
      pos = e > s ? e : e + 1;
    }
    return count;
  });
}

// regexpNthMatch returns the capture slots of the N-th (1-based) non-overlapping match at or after
// `start` (regexp_substr / regexp_instr, regex.md §8b), or null when fewer than N matches exist.
// Same non-overlapping advance as regexpCount.
export function regexpNthMatch(
  p: RegexProgram,
  input: number[],
  start: number,
  n: number,
  m: Meter,
): CapNode | null {
  return withSeen(p, (seen) => {
    const len = input.length;
    let pos = start;
    let count = 0;
    while (pos <= len) {
      const caps = regexSearch(p, seen, input, pos, m);
      if (caps === null) break;
      count++;
      if (count === n) return caps;
      const s = regexSlot(p, caps, 0);
      const e = regexSlot(p, caps, 1);
      pos = e > s ? e : e + 1;
    }
    return null;
  });
}

// sliceGroup slices orig[start..end] to a string, or null for an unset (-1) group.
function sliceGroup(orig: number[], start: number, end: number): string | null {
  if (start < 0 || end < 0) return null;
  return cpSlice(orig, start, end);
}

// cpSlice converts the code points orig[start..end] to a string in bounded chunks: spreading a large
// array into String.fromCodePoint overflows the stack (the replaceBuild precedent).
function cpSlice(orig: number[], start: number, end: number): string {
  const parts: string[] = [];
  for (let i = start; i < end; i += 8192) {
    parts.push(String.fromCodePoint(...orig.slice(i, Math.min(i + 8192, end))));
  }
  return parts.join("");
}

// A piece of an expanded replacement: a literal code point (lo < 0), or the [lo, hi) span of the
// original input.
type PieceFn = (cp: number, lo: number, hi: number) => void;

// forEachPiece walks a replacement template, expanding \1..\9 (capture group), \& (whole match), and
// \\ (literal backslash) — the only special sequences (regex.md §8, PG appendStringInfoRegexpSubstr).
// Any other \<other> (including \0) keeps BOTH characters literally: the backslash is copied and
// <other> is read again as ordinary template text. A trailing lone \ is literal, and an unset or
// absent group expands to nothing. Sizing and building share this walk so they cannot disagree.
function forEachPiece(p: RegexProgram, repl: number[], caps: CapNode, f: PieceFn): void {
  const span = (start: number, end: number): void => {
    if (start >= 0 && end >= 0) f(0, start, end);
  };
  for (let i = 0; i < repl.length; i++) {
    const c = repl[i];
    if (c === 0x5c /* \ */ && i + 1 < repl.length) {
      const n = repl[i + 1];
      if (n >= 0x31 && n <= 0x39 /* 1-9 */) {
        const g = n - 0x30;
        if (g <= p.ngroups) span(regexSlot(p, caps, 2 * g), regexSlot(p, caps, 2 * g + 1));
      } else if (n === 0x26 /* & */) {
        span(regexSlot(p, caps, 0), regexSlot(p, caps, 1));
      } else if (n === 0x5c /* \ */) {
        f(n, -1, -1); // \\ -> \
      } else {
        // Not a special sequence: the backslash is literal and <other> is re-read as plain text
        // (so `\0` is `\0`, not the whole match).
        f(c, -1, -1);
        continue;
      }
      i++;
    } else {
      f(c, -1, -1);
    }
  }
}

// forEachOutput walks the pieces of a replacement in output order: unmatched source text, each match's
// expanded template, and, for an empty match, the code point after it.
function forEachOutput(
  p: RegexProgram,
  orig: number[],
  repl: number[],
  found: CapNode[],
  f: PieceFn,
): void {
  let pos = 0;
  for (const caps of found) {
    const s = regexSlot(p, caps, 0);
    const e = regexSlot(p, caps, 1);
    f(0, pos, s);
    forEachPiece(p, repl, caps, f);
    if (e > s) {
      pos = e;
    } else {
      if (e < orig.length) f(orig[e], -1, -1);
      pos = e + 1;
    }
  }
  if (pos <= orig.length) f(0, pos, orig.length);
}

function cpUtf8Len(cp: number): number {
  return cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
}

// replaceSize is the exact UTF-8 byte length of regexp_replace's result for the matches replaceMatches
// found, computed without building it (cost.md §8.1).
export function replaceSize(
  p: RegexProgram,
  orig: number[],
  repl: number[],
  found: CapNode[],
): bigint {
  let size = 0;
  forEachOutput(p, orig, repl, found, (cp, lo, hi) => {
    if (lo < 0) size += cpUtf8Len(cp);
    else for (let i = lo; i < hi; i++) size += cpUtf8Len(orig[i]);
  });
  return BigInt(size);
}

// replaceBuild builds regexp_replace's result from the matches replaceMatches found. Code points are
// converted in bounded chunks: spreading a large array into String.fromCodePoint overflows the stack.
export function replaceBuild(
  p: RegexProgram,
  orig: number[],
  repl: number[],
  found: CapNode[],
): string {
  const parts: string[] = [];
  let chunk: number[] = [];
  const push = (cp: number): void => {
    chunk.push(cp);
    if (chunk.length === 8192) {
      parts.push(String.fromCodePoint(...chunk));
      chunk = [];
    }
  };
  forEachOutput(p, orig, repl, found, (cp, lo, hi) => {
    if (lo < 0) push(cp);
    else for (let i = lo; i < hi; i++) push(orig[i]);
  });
  parts.push(String.fromCodePoint(...chunk));
  return parts.join("");
}

// ---------------------------------------------------------------------------
// SIMILAR TO (regex.md §8f)
// ---------------------------------------------------------------------------

// The compile mode of every SIMILAR program: dot-all, so `%` and `_` match a newline (regex.md §8f).
const SIMILAR_MODE: RegexMode = { nlStop: false, dotAll: true, nlAnchor: false };

// similarParts returns the part texts of a SIMILAR pattern: PostgreSQL's similar_escape_internal
// translation of each part between the escape-double-quote separators, one to three of them
// (regex.md §8f.1). An undefined escapeChar is the default `\`; an empty one is no escape character.
// An escape of two or more code points is 22025; a third separator is 2200C.
function similarParts(pattern: string, escapeChar: string | undefined): string[] {
  let esc: string | null = "\\";
  if (escapeChar !== undefined) {
    const e = Array.from(escapeChar);
    if (e.length > 1) throw engineError("invalid_escape_sequence", "invalid escape string");
    esc = e.length === 0 ? null : e[0];
  }
  // PostgreSQL's multibyte path: with a non-ASCII escape, a non-ASCII pattern character is copied
  // without touching the class state.
  const wideEscape = esc !== null && (esc.codePointAt(0) as number) >= 0x80;
  const parts: string[] = [""];
  let afterEscape = false;
  let inClass = false;
  let classStart = 0;
  for (const c of pattern) {
    const k = parts.length - 1;
    if (wideEscape && (c.codePointAt(0) as number) >= 0x80) {
      if (afterEscape) {
        parts[k] += "\\" + c;
        afterEscape = false;
      } else if (c === esc) {
        afterEscape = true;
      } else {
        parts[k] += c;
      }
      continue;
    }
    if (afterEscape) {
      if (c === '"' && !inClass) {
        if (parts.length === 3) {
          throw engineError(
            "invalid_use_of_escape_character",
            "SQL regular expression may not contain more than two escape-double-quote separators",
          );
        }
        parts.push("");
      } else {
        parts[k] += "\\" + c;
        classStart = 3;
      }
      afterEscape = false;
    } else if (c === esc) {
      afterEscape = true;
    } else if (inClass) {
      parts[k] += c === "\\" ? "\\\\" : c;
      // `]` is literal right after `[` or `[^` (classStart 1 or 2), else it closes the class.
      if (c === "]" && classStart > 2) inClass = false;
      else if (c === "^") classStart++;
      else classStart = 3;
    } else {
      switch (c) {
        case "[":
          parts[k] += "[";
          inClass = true;
          classStart = 1;
          break;
        case "%":
          parts[k] += ".*";
          break;
        case "_":
          parts[k] += ".";
          break;
        case "(":
          parts[k] += "(?:";
          break;
        case "\\":
        case ".":
        case "^":
        case "$":
          parts[k] += "\\" + c;
          break;
        default:
          parts[k] += c;
      }
    }
  }
  return parts;
}

// similarJoin builds the whole-pattern translation from its part texts: `^(?:t1)$`,
// `^(?:t1){1,1}?(t2)$`, or `^(?:t1){1,1}?(t2){1,1}(?:t3)$` (regex.md §8f.1).
function similarJoin(parts: string[]): string {
  let out = "^(?:" + parts[0];
  if (parts.length > 1) out += "){1,1}?(" + parts[1];
  if (parts.length > 2) out += "){1,1}(?:" + parts[2];
  return out + ")$";
}

// similarToEscape is similar_to_escape(pattern [, escape]) (regex.md §8f.1): PostgreSQL's
// translation of a SIMILAR pattern into a regular expression, byte for byte.
export function similarToEscape(pattern: string, escapeChar: string | undefined): string {
  return similarJoin(similarParts(pattern, escapeChar));
}

// compileSimilar compiles a translated SIMILAR pattern for SIMILAR TO (regex.md §8f.2): the §3.2
// program, dot-all.
export function compileSimilar(translated: string): RegexProgram {
  return compileRegexMode(translated, SIMILAR_MODE);
}

// The programs of substring(s, p, e) (regex.md §8f.3): `whole` alone with no separators (the §3.2
// program of `^(?:t1)$`), else A and B, and for three parts C and D.
export type SimilarMatcher =
  | { whole: RegexProgram }
  | { whole: null; a: RegexProgram; b: RegexProgram; cd: [RegexProgram, RegexProgram] | null };

// compileSimilarMatcher translates and compiles p with escape e. Does not meter; the caller charges
// regex_compile × similarNinst().
export function compileSimilarMatcher(pattern: string, escapeChar: string): SimilarMatcher {
  const parts = similarParts(pattern, escapeChar);
  if (parts.length === 1) return { whole: compileSimilar(similarJoin(parts)) };
  const gated = (body: string, wrapper: RegexWrapper) =>
    compileRegexWrapped(body, SIMILAR_MODE, wrapper);
  const a = gated(`(?:${parts[0]})`, "gatedLeftmost");
  if (parts.length === 2) {
    return { whole: null, a, b: gated(`(?:${parts[1]})$`, "gatedLeftmost"), cd: null };
  }
  const b = gated(`(?:${parts[1]})(?:${parts[2]})$`, "gatedLeftmost");
  const c = gated(`(?:${parts[1]})`, "gatedLeftmost");
  const d = gated(`(?:${parts[2]})$`, "gatedRightmost");
  return { whole: null, a, b, cd: [c, d] };
}

// similarNinst is the instruction count of every program = the regex_compile cost.
export function similarNinst(sm: SimilarMatcher): number {
  if (sm.whole !== null) return regexNinst(sm.whole);
  let n = regexNinst(sm.a) + regexNinst(sm.b);
  if (sm.cd !== null) n += regexNinst(sm.cd[0]) + regexNinst(sm.cd[1]);
  return n;
}

// similarSubstringSpan returns the code-point span [i, j) the SIMILAR substring returns, or null when
// the pattern does not match the whole input (regex.md §8f.3). Charges every run's regex_steps.
export function similarSubstringSpan(
  sm: SimilarMatcher,
  input: number[],
  m: Meter,
): [number, number] | null {
  const n = input.length;
  const at = (pos: number): boolean[] => {
    const gate: boolean[] = new Array(n + 1).fill(false);
    gate[pos] = true;
    return gate;
  };
  if (sm.whole !== null) return regexRun(sm.whole, input, m) === null ? null : [0, n];
  const e1 = regexMatchEnds(sm.a, input, at(0), m);
  const i = regexGatedStart(sm.b, input, e1, m);
  if (i === null) return null;
  if (sm.cd === null) return [i, n];
  const e2 = regexMatchEnds(sm.cd[0], input, at(i), m);
  // B's match proves a split exists, so D always finds one.
  const j = regexGatedStart(sm.cd[1], input, e2, m);
  return j === null ? null : [i, j];
}
