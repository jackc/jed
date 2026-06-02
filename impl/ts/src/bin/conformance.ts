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

// assertCost checks the accrued execution cost matches a pending `# cost:` directive.
function assertCost(expected: bigint | null, actual: bigint, sql: string): void {
  if (expected !== null && expected !== actual) {
    throw new Error(`cost mismatch: expected ${expected}, got ${actual}\n  SQL: ${sql}`);
  }
}

// runFile runs all records in one .test file against a fresh database.
function runFile(text: string): void {
  const db = new Database();
  const lines = text.split("\n");
  const c: Cursor = { i: 0 };
  // A `# cost: N` directive sets this; the next record consumes it (CLAUDE.md §13).
  let pendingCost: bigint | null = null;
  while (c.i < lines.length) {
    const line = lines[c.i]!.trim();
    if (line === "") {
      c.i++;
      continue;
    }
    if (line.startsWith("#")) {
      // `# cost: N` binds to the next record; every other comment is ignored.
      const n = parseCostDirective(line);
      if (n !== null) pendingCost = n;
      c.i++;
      continue;
    }
    // This record consumes any pending cost assertion (so it never leaks forward).
    const expectedCost = pendingCost;
    pendingCost = null;
    const fields = line.split(/\s+/);
    if (fields[0] === "statement") {
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
      if (!arrEq(actual, exp)) {
        throw new Error(
          `query result mismatch\n  SQL: ${sql}\n  expected: ${JSON.stringify(exp)}\n  actual:   ${JSON.stringify(actual)}`,
        );
      }
      assertCost(expectedCost, outcome.cost, sql);
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
