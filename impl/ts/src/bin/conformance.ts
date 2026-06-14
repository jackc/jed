// The TypeScript core's conformance harness (CLAUDE.md §7). Mirrors
// cmd/conformance/main.go: walk spec/conformance/suites, and for each .test file whose
// `# requires:` capabilities are all in this core's SUPPORTED_CAPABILITIES, run the
// sqllogictest-style records against a fresh Database and compare output. Files needing
// a capability the core does not declare are SKIPPED (not failed). Needs no TOML.

import { existsSync, readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import process from "node:process";
import { Database, EngineError, execute, render, SUPPORTED_CAPABILITIES } from "../lib.ts";

function suitesDir(): string {
  let dir = import.meta.dirname; // .../impl/ts/src/bin
  for (;;) {
    const candidate = join(dir, "spec", "conformance", "suites");
    if (existsSync(candidate)) return candidate;
    const parent = dirname(dir);
    if (parent === dir) throw new Error("could not locate spec/conformance/suites");
    dir = parent;
  }
}

function parseRequires(text: string): string[] {
  for (const raw of text.split("\n")) {
    const t = raw.trim();
    if (!t.startsWith("#")) continue;
    const rest = t.slice(1).trim();
    if (rest.startsWith("requires:")) {
      return rest
        .slice("requires:".length)
        .split(",")
        .map((c) => c.trim())
        .filter((c) => c.length > 0);
    }
  }
  return [];
}

type Cursor = { i: number };

function takeSQL(lines: string[], c: Cursor): string {
  const out: string[] = [];
  while (c.i < lines.length && lines[c.i]!.trim() !== "") {
    out.push(lines[c.i]!);
    c.i++;
  }
  return out.join("\n");
}

function takeSQLUntilSeparator(lines: string[], c: Cursor): string {
  const out: string[] = [];
  while (c.i < lines.length) {
    if (lines[c.i]!.trim() === "----") {
      c.i++;
      break;
    }
    out.push(lines[c.i]!);
    c.i++;
  }
  return out.join("\n");
}

function codeOf(err: unknown): string {
  return err instanceof EngineError ? err.code() : "?";
}

function msgOf(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

function applySort(flat: string[], cols: number, sortmode: string): string[] {
  if (sortmode === "valuesort") {
    return [...flat].sort();
  }
  if (sortmode === "rowsort") {
    const c = cols < 1 ? 1 : cols;
    const rows: string[][] = [];
    for (let i = 0; i + c <= flat.length; i += c) {
      rows.push(flat.slice(i, i + c));
    }
    rows.sort((a, b) => {
      const ka = a.join("\x00");
      const kb = b.join("\x00");
      return ka < kb ? -1 : ka > kb ? 1 : 0;
    });
    return rows.flat();
  }
  return flat;
}

function renderOutcome(
  outcome: ReturnType<typeof execute>,
  cols: number,
  sortmode: string,
): string[] {
  if (outcome.kind !== "query") return [];
  const flat = outcome.rows.flatMap((row) => row.map((v) => render(v)));
  return applySort(flat, cols, sortmode);
}

function arrEq(a: string[], b: string[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}

// parseFloatCell parses an expected/actual float render cell to a JS number for the R-tag compare.
// Recognizes the PG/jed spellings the R column may carry: NaN, ±Infinity (and -0). Returns NaN for
// an unparseable cell (then both-NaN below would wrongly match — but a real float column never
// renders junk, so this is fine; the tolerant compare is only reached for an R-tagged column).
function parseFloatCell(s: string): number {
  if (s === "Infinity" || s === "+Infinity") return Infinity;
  if (s === "-Infinity") return -Infinity;
  return Number(s);
}

// floatCellsEqual is the R (real) render-tag's tolerant comparison (spec/design/float.md §9,
// conformance.md §1): parse both cells to f64 and consider them equal iff both NaN, OR bit-equal /
// Object.is (covers ±Inf and -0===+0 via the a===b arm), OR — for two finite values — within a
// small relative tolerance. Layout differences and a transcendental's last-ULP divergence never
// fail. This is the ONE tag compared by value, not by string.
function floatCellsEqual(expected: string, actual: string): boolean {
  const a = parseFloatCell(expected);
  const b = parseFloatCell(actual);
  const an = Number.isNaN(a);
  const bn = Number.isNaN(b);
  if (an || bn) return an && bn; // both NaN → equal; exactly one NaN → not
  if (a === b || Object.is(a, b)) return true; // ±Inf exact; -0 === +0 treated equal here
  if (Number.isFinite(a) && Number.isFinite(b)) {
    return Math.abs(a - b) <= 1e-9 * Math.max(Math.abs(a), Math.abs(b), 1);
  }
  return false;
}

// cellsEqual compares one result cell against its expected value, selecting the comparator by the
// column's coltype tag: an `R` (real/float) column compares by PARSED VALUE within a tolerance
// (floatCellsEqual); every other tag compares by exact string (the bit-exact, in-contract surface).
function cellsEqual(coltype: string, expected: string, actual: string): boolean {
  return coltype === "R" ? floatCellsEqual(expected, actual) : expected === actual;
}

// rowsEqual compares the actual vs expected flat cell arrays column-aware: a flat index's column is
// (index mod cols), whose coltype tag picks the comparator. Used after applySort has aligned both
// arrays (the R tag's tolerance only ever loosens equality, so sorting by string then comparing
// tolerantly is sound for the float renders the corpus produces).
function rowsEqual(coltypes: string, cols: number, actual: string[], expected: string[]): boolean {
  if (actual.length !== expected.length) return false;
  for (let i = 0; i < actual.length; i++) {
    const col = cols > 0 ? i % cols : 0;
    const tag = col < coltypes.length ? coltypes[col]! : "";
    if (!cellsEqual(tag, expected[i]!, actual[i]!)) return false;
  }
  return true;
}

// parseCostDirective parses a `# cost: N` directive line (CLAUDE.md §13). Returns the
// asserted cost, or null if the comment is not a cost directive.
function parseCostDirective(line: string): bigint | null {
  const m = line.match(/^#\s*cost:\s*(\S+)/);
  if (!m) return null;
  try {
    const n = BigInt(m[1]!);
    return n >= 0n ? n : null;
  } catch {
    return null;
  }
}

// parseMaxCostDirective parses a `# max_cost: N` directive line. Returns the caller-set cost
// ceiling to run the next record under, or null if not a max_cost directive. Mirrors `# cost:`,
// but instead of asserting the accrued cost it bounds it: the record is expected to abort with
// 54P01 once accrued cost reaches N (CLAUDE.md §13; spec/design/cost.md §6).
function parseMaxCostDirective(line: string): bigint | null {
  const m = line.match(/^#\s*max_cost:\s*(\S+)/);
  if (!m) return null;
  try {
    const n = BigInt(m[1]!);
    return n >= 0n ? n : null;
  } catch {
    return null;
  }
}

// assertCost checks the accrued execution cost matches a pending `# cost:` directive.
function assertCost(expected: bigint | null, actual: bigint, sql: string): void {
  if (expected !== null && expected !== actual) {
    throw new Error(`cost mismatch: expected ${expected}, got ${actual}\n  SQL: ${sql}`);
  }
}

// parseNamesDirective parses a `# names: a, b, ?column?` directive line. Returns the
// asserted output column names, or null if not a names directive (conformance.md §1).
function parseNamesDirective(line: string): string[] | null {
  const m = line.match(/^#\s*names:\s*(.+)$/);
  if (!m) return null;
  return m[1]!
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s !== "");
}

// assertNames checks the query's output column names match a pending `# names:` directive.
function assertNames(expected: string[] | null, actual: string[], sql: string): void {
  if (expected !== null && !arrEq(expected, actual)) {
    throw new Error(
      `column-name mismatch\n  SQL: ${sql}\n  expected: ${JSON.stringify(expected)}\n  actual:   ${JSON.stringify(actual)}`,
    );
  }
}

// parseTypesDirective parses a `# types: int16, text, decimal` directive line. Returns the
// asserted output column types — each the canonical name of a result column's resolved type (the
// integer WIDTH, the unconstrained `decimal`, `unknown` for an untyped NULL), beyond the
// `I`/`T`/`D` rendering tag (conformance.md §1/§7); null if not a types directive.
function parseTypesDirective(line: string): string[] | null {
  const m = line.match(/^#\s*types:\s*(.+)$/);
  if (!m) return null;
  return m[1]!
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s !== "");
}

// assertTypes checks the query's output column types match a pending `# types:` directive.
function assertTypes(expected: string[] | null, actual: string[], sql: string): void {
  if (expected !== null && !arrEq(expected, actual)) {
    throw new Error(
      `column-type mismatch\n  SQL: ${sql}\n  expected: ${JSON.stringify(expected)}\n  actual:   ${JSON.stringify(actual)}`,
    );
  }
}

// runFile runs all records in one .test file against a fresh database.
function runFile(text: string): void {
  const db = new Database();
  const lines = text.split("\n");
  const c: Cursor = { i: 0 };
  // A `# cost: N` / `# names: ...` / `# types: ...` / `# max_cost: N` directive sets these; the
  // next record consumes them.
  let pendingCost: bigint | null = null;
  let pendingNames: string[] | null = null;
  let pendingTypes: string[] | null = null;
  let pendingMaxCost: bigint | null = null;
  while (c.i < lines.length) {
    const line = lines[c.i]!.trim();
    if (line === "") {
      c.i++;
      continue;
    }
    if (line.startsWith("#")) {
      // `# cost:` / `# max_cost:` / `# names:` / `# types:` bind to the next record; every other
      // comment is ignored.
      const n = parseCostDirective(line);
      const mc = parseMaxCostDirective(line);
      if (n !== null) {
        pendingCost = n;
      } else if (mc !== null) {
        pendingMaxCost = mc;
      } else {
        const names = parseNamesDirective(line);
        if (names !== null) {
          pendingNames = names;
        } else {
          const types = parseTypesDirective(line);
          if (types !== null) pendingTypes = types;
        }
      }
      c.i++;
      continue;
    }
    // This record consumes any pending assertions (so they never leak forward).
    const expectedCost = pendingCost;
    const expectedNames = pendingNames;
    const expectedTypes = pendingTypes;
    pendingCost = null;
    pendingNames = null;
    pendingTypes = null;
    // Apply the per-record cost ceiling (0 = unlimited); set each record so it auto-resets.
    db.setMaxCost(pendingMaxCost ?? 0n);
    pendingMaxCost = null;
    const fields = line.split(/\s+/);
    if (fields[0] === "statement") {
      // `# names:` / `# types:` assert result columns, which a statement lacks.
      if (expectedNames !== null) {
        throw new Error("# names: directive precedes a non-query statement");
      }
      if (expectedTypes !== null) {
        throw new Error("# types: directive precedes a non-query statement");
      }
      const expect = fields[1] ?? "";
      c.i++;
      const sql = takeSQL(lines, c);
      let err: unknown = null;
      let outcome: ReturnType<typeof execute> | null = null;
      try {
        outcome = execute(db, sql);
      } catch (e) {
        err = e;
      }
      if (expect === "ok") {
        if (err !== null) {
          throw new Error(`statement expected ok, got error ${msgOf(err)}\n  SQL: ${sql}`);
        }
        assertCost(expectedCost, outcome!.cost, sql);
      } else if (expect === "error") {
        const want = fields[2] ?? "";
        if (err === null) {
          throw new Error(`statement expected error ${want}, but it succeeded\n  SQL: ${sql}`);
        }
        const got = codeOf(err);
        if (got !== want) {
          throw new Error(`statement expected error ${want}, got ${got}\n  SQL: ${sql}`);
        }
      } else {
        throw new Error(`unknown statement kind "${expect}"`);
      }
    } else if (fields[0] === "query") {
      const coltypes = fields[1] ?? "";
      const sortmode = fields[2] ?? "nosort";
      c.i++;
      const sql = takeSQLUntilSeparator(lines, c);
      const expected: string[] = [];
      while (c.i < lines.length && lines[c.i]!.trim() !== "") {
        expected.push(lines[c.i]!.trim());
        c.i++;
      }
      let outcome: ReturnType<typeof execute>;
      try {
        outcome = execute(db, sql);
      } catch (e) {
        throw new Error(`query failed with ${msgOf(e)}\n  SQL: ${sql}`);
      }
      const cols = coltypes.length === 0 ? 1 : coltypes.length;
      const actual = renderOutcome(outcome, cols, sortmode);
      const exp = applySort(expected, cols, sortmode);
      // Compare column-aware: an `R` (float) column compares by parsed value within a tolerance
      // (the R-tag exemption — float.md §9); every other tag is exact string. arrEq is the
      // exact-only fast path for the no-R-column case.
      const ok = coltypes.includes("R") ? rowsEqual(coltypes, cols, actual, exp) : arrEq(actual, exp);
      if (!ok) {
        throw new Error(
          `query result mismatch\n  SQL: ${sql}\n  expected: ${JSON.stringify(exp)}\n  actual:   ${JSON.stringify(actual)}`,
        );
      }
      assertCost(expectedCost, outcome.cost, sql);
      assertNames(expectedNames, outcome.kind === "query" ? outcome.columnNames : [], sql);
      assertTypes(expectedTypes, outcome.kind === "query" ? outcome.columnTypes : [], sql);
    } else {
      throw new Error(`unknown record kind "${fields[0]}"`);
    }
  }
}

function main(): number {
  const suites = suitesDir();
  const files = readdirSync(suites, { recursive: true })
    .filter((f): f is string => typeof f === "string" && f.endsWith(".test"))
    .sort();

  const supported = new Set(SUPPORTED_CAPABILITIES);

  let passed = 0;
  let failed = 0;
  let skipped = 0;
  for (const rel of files) {
    const text = readFileSync(join(suites, rel), "utf8");
    const missing = parseRequires(text).filter((c) => !supported.has(c));
    if (missing.length > 0) {
      console.log(`SKIP ${rel}  (missing: ${missing.join(", ")})`);
      skipped++;
      continue;
    }
    try {
      runFile(text);
      console.log(`PASS ${rel}`);
      passed++;
    } catch (e) {
      console.log(`FAIL ${rel}: ${msgOf(e)}`);
      failed++;
    }
  }

  console.log(`\n${passed} passed, ${failed} failed, ${skipped} skipped`);
  return failed !== 0 ? 1 : 0;
}

process.exit(main());
