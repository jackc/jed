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

import { COSTS } from "./costs.ts";
import type { Meter } from "./cost.ts";
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
  | "class"
  | "split"
  | "jmp"
  | "save"
  | "assertStart"
  | "assertEnd"
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
// (excluding group 0, the whole match).
export type RegexProgram = {
  insts: Inst[];
  classes: RegexClass[];
  ngroups: number;
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
  | { kind: "anchorEnd" };

// ---------------------------------------------------------------------------
// Parser: pattern text -> AST (chars are code-point strings via Array.from)
// ---------------------------------------------------------------------------

class RegexParser {
  private chars: string[];
  private pos = 0;
  ngroups = 0;

  constructor(pattern: string) {
    this.chars = Array.from(pattern);
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
        return { kind: "any" };
      case "^":
        this.bump();
        return { kind: "anchorStart" };
      case "$":
        this.bump();
        return { kind: "anchorEnd" };
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
    const pc = predefClass(c);
    if (pc !== null) return { kind: "class", class: { negated: pc.negated, ranges: pc.ranges } };
    const ctrl = controlEscape(c);
    if (ctrl !== null) return { kind: "char", ch: ctrl };
    if (isRegexMeta(c)) return { kind: "char", ch: c.codePointAt(0) as number };
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
    if (isRegexMeta(e) || e === "-" || e === "]")
      return { ch: e.codePointAt(0) as number, set: null };
    throw regexInvalid(`invalid escape \\${e}`);
  }
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

// compileRegex compiles a pattern to a program (regex.md §3). Throws 2201B on a malformed pattern
// and 54001 on a well-formed-but-too-large one. Does NOT meter — the caller charges
// regex_compile × regexNinst() (the precompilation contract, regex.md §5). For ~* the pattern must
// already be case-folded by the caller.
export function compileRegex(pattern: string): RegexProgram {
  const parser = new RegexParser(pattern);
  const root = parser.parseAlt();
  if (!parser.atEnd()) throw regexInvalid("unbalanced parenthesis");
  const ngroups = parser.ngroups;
  const c = new RegexCompiler();
  // Wrapper (regex.md §3.2): lazy `.*?` prefix + group-0 save + match.
  c.pushInst({ op: "split", x: 3, y: 1, ch: 0 });
  c.pushInst({ op: "any", x: 0, y: 0, ch: 0 });
  c.pushInst({ op: "jmp", x: 0, y: 0, ch: 0 });
  c.pushInst({ op: "save", x: 0, y: 0, ch: 0 });
  c.emit(root);
  c.pushInst({ op: "save", x: 1, y: 0, ch: 0 });
  c.pushInst({ op: "match", x: 0, y: 0, ch: 0 });
  return { insts: c.insts, classes: c.classes, ngroups };
}

// ---------------------------------------------------------------------------
// Pike VM (regex.md §4)
// ---------------------------------------------------------------------------

type Thread = { pc: number; saves: number[] };

// regexIsMatch reports whether the pattern matches somewhere in input (the `~` operator).
export function regexIsMatch(p: RegexProgram, input: number[], m: Meter): boolean {
  return regexRun(p, input, m) !== null;
}

// regexRun executes the Pike VM from the start of the input (regex.md §4). Returns the winning
// thread's capture slots on a match (code-point offsets; -1 = unset), or null.
export function regexRun(p: RegexProgram, input: number[], m: Meter): number[] | null {
  return regexSearch(p, input, 0, m);
}

// regexSearch runs the Pike VM, considering only matches that START at code-point position `start` or
// later (the unanchored search seeds its lazy `.*?` prefix at `start`); ^/$ still anchor at the true
// input bounds. Used by regexp_replace's global loop. Charges regex_step per explored state and
// guards once per input position.
export function regexSearch(
  p: RegexProgram,
  input: number[],
  start: number,
  m: Meter,
): number[] | null {
  const nslots = 2 * (p.ngroups + 1);
  const len = input.length;
  const seen = new Int32Array(p.insts.length);
  let generation = 0;
  let clist: Thread[] = [];
  let nlist: Thread[] = [];
  let matched: number[] | null = null;

  generation++;
  const initSaves = new Array<number>(nslots).fill(-1);
  addThread(p, clist, seen, generation, 0, initSaves, start, len, m);

  for (let sp = start; sp <= len; sp++) {
    generation++;
    nlist.length = 0;
    inner: for (let i = 0; i < clist.length; i++) {
      const th = clist[i];
      const inst = p.insts[th.pc];
      switch (inst.op) {
        case "char":
          if (sp < len && input[sp] === inst.ch) {
            addThread(p, nlist, seen, generation, th.pc + 1, th.saves, sp + 1, len, m);
          }
          break;
        case "any":
          if (sp < len && input[sp] !== 0x0a) {
            addThread(p, nlist, seen, generation, th.pc + 1, th.saves, sp + 1, len, m);
          }
          break;
        case "class":
          if (sp < len && classAdmits(p.classes[inst.x], input[sp])) {
            addThread(p, nlist, seen, generation, th.pc + 1, th.saves, sp + 1, len, m);
          }
          break;
        case "match":
          matched = th.saves;
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
  seen: Int32Array,
  generation: number,
  pc0: number,
  saves0: number[],
  sp: number,
  len: number,
  m: Meter,
): void {
  const stack: Thread[] = [{ pc: pc0, saves: saves0 }];
  while (stack.length > 0) {
    const top = stack.pop() as Thread;
    if (seen[top.pc] === generation) continue;
    seen[top.pc] = generation;
    m.charge(COSTS.regexStep);
    const inst = p.insts[top.pc];
    switch (inst.op) {
      case "jmp":
        stack.push({ pc: inst.x, saves: top.saves });
        break;
      case "split":
        // Push y first, then x, so x pops first = higher priority.
        stack.push({ pc: inst.y, saves: top.saves });
        stack.push({ pc: inst.x, saves: top.saves });
        break;
      case "save": {
        const s = top.saves.slice();
        s[inst.x] = sp;
        stack.push({ pc: top.pc + 1, saves: s });
        break;
      }
      case "assertStart":
        if (sp === 0) stack.push({ pc: top.pc + 1, saves: top.saves });
        break;
      case "assertEnd":
        if (sp === len) stack.push({ pc: top.pc + 1, saves: top.saves });
        break;
      default:
        // char / any / class / match — parked for the consume loop.
        list.push({ pc: top.pc, saves: top.saves });
        break;
    }
  }
}

// regexpMatch is regexp_match(source, …) capture extraction (regex.md §8). Searches once; on a match
// returns the capture group strings (groups 1..n, or a 1-element whole-match list when the pattern
// has no group — the PG rule), an unset group being null. Returns null (the whole result) on no
// match. matchInput is the (possibly case-folded) subject the VM matches; origInput is the
// ORIGINAL-case subject the returned substrings are sliced from (same length).
export function regexpMatch(
  p: RegexProgram,
  matchInput: number[],
  origInput: number[],
  m: Meter,
): (string | null)[] | null {
  const saves = regexSearch(p, matchInput, 0, m);
  if (saves === null) return null;
  if (p.ngroups === 0) return [sliceGroup(origInput, saves[0], saves[1])];
  const groups: (string | null)[] = [];
  for (let g = 1; g <= p.ngroups; g++) {
    groups.push(sliceGroup(origInput, saves[2 * g], saves[2 * g + 1]));
  }
  return groups;
}

// regexpReplace is regexp_replace(source, pattern, replacement, …) (regex.md §8). Replaces the first
// match (or all when global) by the replacement TEMPLATE (\1..\9 = capture group, \& = whole match,
// \\ = literal backslash). Non-matched text and captured substrings come from origInput (original
// case); the VM matches over matchInput (possibly case-folded).
export function regexpReplace(
  p: RegexProgram,
  matchInput: number[],
  origInput: number[],
  replacement: number[],
  global: boolean,
  m: Meter,
): string {
  const out: number[] = [];
  let pos = 0;
  for (;;) {
    const saves = regexSearch(p, matchInput, pos, m);
    if (saves === null) break;
    const s = saves[0];
    const e = saves[1];
    for (let i = pos; i < s; i++) out.push(origInput[i]);
    spliceReplacement(out, replacement, saves, origInput);
    if (!global) {
      for (let i = e; i < origInput.length; i++) out.push(origInput[i]);
      return String.fromCodePoint(...out);
    }
    if (e > s) {
      pos = e;
    } else {
      // Empty match: emit the char at `e` (if any) and advance past it (the PG global rule).
      if (e < origInput.length) out.push(origInput[e]);
      pos = e + 1;
    }
    if (pos > origInput.length) return String.fromCodePoint(...out);
  }
  for (let i = pos; i < origInput.length; i++) out.push(origInput[i]);
  return String.fromCodePoint(...out);
}

// regexpCount counts the non-overlapping matches at or after code-point position `start`
// (regexp_count, regex.md §8b). The advance is regexpReplace's global rule: after a match [s,e)
// continue at e, or at e+1 for an EMPTY match so a nullable pattern terminates. `start` may be up to
// len (an empty match at the very end still counts); start > len (clamped to len+1 by the caller)
// yields 0.
export function regexpCount(p: RegexProgram, input: number[], start: number, m: Meter): number {
  const len = input.length;
  let pos = start;
  let count = 0;
  while (pos <= len) {
    const saves = regexSearch(p, input, pos, m);
    if (saves === null) break;
    count++;
    const s = saves[0];
    const e = saves[1];
    pos = e > s ? e : e + 1;
  }
  return count;
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
): number[] | null {
  const len = input.length;
  let pos = start;
  let count = 0;
  while (pos <= len) {
    const saves = regexSearch(p, input, pos, m);
    if (saves === null) break;
    count++;
    if (count === n) return saves;
    const s = saves[0];
    const e = saves[1];
    pos = e > s ? e : e + 1;
  }
  return null;
}

// sliceGroup slices orig[start..end] to a string, or null for an unset (-1) group.
function sliceGroup(orig: number[], start: number, end: number): string | null {
  if (start < 0 || end < 0) return null;
  return String.fromCodePoint(...orig.slice(start, end));
}

// spliceReplacement appends a replacement template to out, expanding \1..\9 (capture group), \&
// (whole match), \\ (literal backslash), and \<other> (the literal <other>). A trailing lone \ is
// literal.
function spliceReplacement(out: number[], repl: number[], saves: number[], orig: number[]): void {
  for (let i = 0; i < repl.length; i++) {
    const c = repl[i];
    if (c === 0x5c /* \ */ && i + 1 < repl.length) {
      const n = repl[i + 1];
      if (n >= 0x30 && n <= 0x39 /* 0-9 */) {
        const g = n - 0x30;
        if (2 * g + 1 < saves.length) {
          const grp = sliceGroup(orig, saves[2 * g], saves[2 * g + 1]);
          if (grp !== null) for (const cp of grp) out.push(cp.codePointAt(0) as number);
        }
      } else if (n === 0x26 /* & */) {
        const grp = sliceGroup(orig, saves[0], saves[1]);
        if (grp !== null) for (const cp of grp) out.push(cp.codePointAt(0) as number);
      } else {
        out.push(n); // \\ -> \, and \<other> -> <other>
      }
      i++;
    } else {
      out.push(c);
    }
  }
}
