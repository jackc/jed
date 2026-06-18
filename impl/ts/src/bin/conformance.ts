// The TypeScript core's conformance harness (CLAUDE.md §7). Mirrors
// cmd/conformance/main.go: walk spec/conformance/suites, and for each .test file whose
// `# requires:` capabilities are all in this core's SUPPORTED_CAPABILITIES, run the
// sqllogictest-style records against a fresh Database and compare output. Files needing
// a capability the core does not declare are SKIPPED (not failed). Needs no TOML.

import { existsSync, readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import process from "node:process";
import {
  advancingClock,
  Database,
  EngineError,
  execute,
  fixedClock,
  type Outcome,
  ReadHandle,
  render,
  seededRandomSource,
  SharedDb,
  SUPPORTED_CAPABILITIES,
  WriteHandle,
} from "../lib.ts";

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

// parseSeedDirective parses a `# seed: N` directive line (spec/design/entropy.md §6): the fixed
// PRNG seed (u64) to run the next record under, making the uuid generators cross-core identical.
function parseSeedDirective(line: string): bigint | null {
  const m = line.match(/^#\s*seed:\s*(\S+)/);
  if (!m) return null;
  try {
    return BigInt.asUintN(64, BigInt(m[1]!));
  } catch {
    return null;
  }
}

// parseClockDirective parses a `# clock: N` directive line (entropy.md §6): the fixed statement
// clock (i64 micros since the Unix epoch) to run the next record under, fixing uuidv7's instant.
function parseClockDirective(line: string): bigint | null {
  const m = line.match(/^#\s*clock:\s*(-?\S+)/);
  if (!m) return null;
  try {
    return BigInt(m[1]!);
  } catch {
    return null;
  }
}

// parseClockAdvanceDirective parses a `# clock_advance: start,step` directive line (entropy.md §6):
// an advancing clock (start, start+step, … one increment per read) so clock_timestamp()'s per-call
// reads are deterministic and distinguishable from the statement-stable now(). Returns [start, step].
function parseClockAdvanceDirective(line: string): [bigint, bigint] | null {
  const m = line.match(/^#\s*clock_advance:\s*(-?\d+)\s*,\s*(-?\d+)/);
  if (!m) return null;
  try {
    return [BigInt(m[1]!), BigInt(m[2]!)];
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
  let pendingSeed: bigint | null = null;
  let pendingClock: bigint | null = null;
  let pendingClockAdvance: [bigint, bigint] | null = null;
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
      const sd = parseSeedDirective(line);
      const ck = parseClockDirective(line);
      const ca = parseClockAdvanceDirective(line);
      if (n !== null) {
        pendingCost = n;
      } else if (mc !== null) {
        pendingMaxCost = mc;
      } else if (sd !== null) {
        pendingSeed = sd;
      } else if (ck !== null) {
        pendingClock = ck;
      } else if (ca !== null) {
        pendingClockAdvance = ca;
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
    // Apply the per-record entropy seed + statement clock for the uuid generators (entropy.md §6);
    // absent ⇒ cleared (OS entropy / wall clock), so a directive never leaks forward.
    if (pendingSeed !== null) db.setRandomSource(seededRandomSource(pendingSeed));
    else db.clearRandomSource();
    pendingSeed = null;
    // `# clock_advance:` (an advancing clock) takes precedence over `# clock:` (a fixed one); a
    // record uses at most one. Absent ⇒ cleared, so a clock directive never leaks forward.
    if (pendingClockAdvance !== null) {
      db.setClockSource(advancingClock(pendingClockAdvance[0], pendingClockAdvance[1]));
    } else if (pendingClock !== null) {
      db.setClockSource(fixedClock(pendingClock));
    } else {
      db.clearClockSource();
    }
    pendingClock = null;
    pendingClockAdvance = null;
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

// --- the concurrency schedule runner (spec/design/concurrency-testing.md §4) -----------------
// A `.test` file carrying a `# format: concurrency` header is an explicit total order over named
// read/write SESSIONS opened on one SharedDb. Because jed read results depend only on the logical
// order of commits and pin-points — never on timing (§2) — executing the listed order on the single
// JS thread yields the canonical, deterministic result every core must produce. This core has no
// stepped-threaded mode (JS has no shared-memory threads for live objects, §4.3); it always runs the
// stepped-sequential mode, which is what defines the canonical output the threaded cores reproduce.
//
// The result grammar (statement / query, sortmodes, the R float tag) is reused verbatim from the
// sequential runner (runFile) — only the session control + state assertions are new.

// isConcurrencyFormat reports whether text opts into the schedule format via a `# format:
// concurrency` header line. Any other (or absent) format is the sequential runner.
function isConcurrencyFormat(text: string): boolean {
  for (const raw of text.split("\n")) {
    const t = raw.trim();
    if (!t.startsWith("#")) continue;
    const rest = t.slice(1).trim();
    if (rest.startsWith("format:")) return rest.slice("format:".length).trim() === "concurrency";
  }
  return false;
}

// CSession is one open handle in a schedule: exactly one of read/write is set.
type CSession = { read?: ReadHandle; write?: WriteHandle };

// sessionExecute runs sql against the session's handle, returning the outcome. A read session's
// writes are rejected with 25006 by the handle itself (without poisoning it).
function sessionExecute(s: CSession, sql: string): Outcome {
  return s.write ? s.write.execute(sql) : s.read!.execute(sql);
}

// concurrencyDirectives are the line-leading keywords that bound a record body. Unlike the
// sequential format, a schedule does not separate records with blank lines, so an `on` record's SQL
// (and a query's expected rows) runs until the next directive, a blank line, or a comment.
const concurrencyDirectives = new Set(["open", "on", "commit", "rollback", "close", "expect"]);

// isBoundary reports whether line ends the current record body: blank, a comment, or the start of
// the next schedule directive.
function isBoundary(line: string): boolean {
  const t = line.trim();
  if (t === "" || t.startsWith("#")) return true;
  const first = t.split(/\s+/)[0]!;
  return concurrencyDirectives.has(first);
}

// takeConcurrencySQL reads a statement's SQL body: lines from c.i up to the next record boundary.
function takeConcurrencySQL(lines: string[], c: Cursor): string {
  const sql: string[] = [];
  while (c.i < lines.length && !isBoundary(lines[c.i]!)) {
    sql.push(lines[c.i]!);
    c.i++;
  }
  return sql.join("\n");
}

// takeConcurrencyQuery reads a query body: SQL up to the `----` separator, then expected rows up to
// the next record boundary.
function takeConcurrencyQuery(lines: string[], c: Cursor): { sql: string; expected: string[] } {
  const body: string[] = [];
  while (c.i < lines.length) {
    if (lines[c.i]!.trim() === "----") {
      c.i++;
      break;
    }
    body.push(lines[c.i]!);
    c.i++;
  }
  const expected: string[] = [];
  while (c.i < lines.length && !isBoundary(lines[c.i]!)) {
    expected.push(lines[c.i]!.trim());
    c.i++;
  }
  return { sql: body.join("\n"), expected };
}

// runConcurrencyRecord runs one `on <sid> <record>` body (a sqllogictest statement/query) against
// session s, advancing c past the record's SQL and any expected rows.
function runConcurrencyRecord(s: CSession, sid: string, rec: string[], lines: string[], c: Cursor): void {
  if (rec[0] === "statement") {
    const expect = rec[1] ?? "";
    const sql = takeConcurrencySQL(lines, c);
    let err: unknown = null;
    try {
      sessionExecute(s, sql);
    } catch (e) {
      err = e;
    }
    if (expect === "ok") {
      if (err !== null) {
        throw new Error(`[${sid}] statement expected ok, got error ${msgOf(err)}\n  SQL: ${sql}`);
      }
    } else if (expect === "error") {
      const want = rec[2] ?? "";
      if (err === null) {
        throw new Error(`[${sid}] statement expected error ${want}, but it succeeded\n  SQL: ${sql}`);
      }
      const got = codeOf(err);
      if (got !== want) {
        throw new Error(`[${sid}] statement expected error ${want}, got ${got}\n  SQL: ${sql}`);
      }
    } else {
      throw new Error(`[${sid}] unknown statement kind "${expect}"`);
    }
  } else if (rec[0] === "query") {
    const coltypes = rec[1] ?? "";
    const sortmode = rec[2] ?? "nosort";
    const { sql, expected } = takeConcurrencyQuery(lines, c);
    let outcome: Outcome;
    try {
      outcome = sessionExecute(s, sql);
    } catch (e) {
      throw new Error(`[${sid}] query failed with ${msgOf(e)}\n  SQL: ${sql}`);
    }
    const cols = coltypes.length === 0 ? 1 : coltypes.length;
    const actual = renderOutcome(outcome, cols, sortmode);
    const exp = applySort(expected, cols, sortmode);
    const ok = coltypes.includes("R") ? rowsEqual(coltypes, cols, actual, exp) : arrEq(actual, exp);
    if (!ok) {
      throw new Error(
        `[${sid}] query result mismatch\n  SQL: ${sql}\n  expected: ${JSON.stringify(exp)}\n  actual:   ${JSON.stringify(actual)}`,
      );
    }
  } else {
    throw new Error(`[${sid}] unknown record kind "${rec[0]}"`);
  }
}

// endSession ends a session: commit/rollback a write session, close a read session.
function endSession(kind: string, s: CSession): void {
  if (kind === "close") {
    if (!s.read) throw new Error("close of a write session (use commit/rollback)");
    s.read.close();
  } else if (kind === "commit") {
    if (!s.write) throw new Error("commit of a read session (use close)");
    s.write.commit();
  } else if (kind === "rollback") {
    if (!s.write) throw new Error("rollback of a read session (use close)");
    s.write.rollback();
  }
}

// runConcurrencyFile runs one `# format: concurrency` file against a fresh SharedDb.
//
// The Layer 2 `blocks` annotation (concurrency-testing.md §5) is modeled here without ever truly
// blocking — which this single-threaded core could not do anyway (write() while a writer is open
// throws 25001, since one JS thread cannot block, shared.ts). A queued writer-open is NOT run when
// it is seen, but recorded — and run at the gate-releasing step, the instant the holder commits/rolls
// back: the equivalent serial order, identical to what a threaded run consistent with the schedule
// must produce. `gateHolder` is the live writer's sid (the single-writer gate); `blocked` is the
// at-most-one writer queued on it.
function runConcurrencyFile(text: string): void {
  const db = SharedDb.newInMemory();
  const sessions = new Map<string, CSession>();
  let gateHolder = ""; // the live writer holding the single-writer gate, "" if free
  let blocked = ""; // a writer queued on the gate (Layer 2 `blocks`), "" if none
  const lines = text.split("\n");
  const c: Cursor = { i: 0 };
  while (c.i < lines.length) {
    const line = lines[c.i]!.trim();
    if (line === "" || line.startsWith("#")) {
      c.i++;
      continue;
    }
    const fields = line.split(/\s+/);
    switch (fields[0]) {
      case "open": {
        if (fields.length < 3) throw new Error(`open needs \`<sid> read|write [blocks]\`: ${line}`);
        const sid = fields[1]!;
        const mode = fields[2]!;
        // An optional 4th token is the Layer 2 `blocks` annotation (writer-open on a held gate).
        let blocksAnn = false;
        if (fields.length > 3) {
          if (fields[3] !== "blocks") {
            throw new Error(`unknown open annotation "${fields[3]}" (want \`blocks\`): ${line}`);
          }
          blocksAnn = true;
        }
        if (sessions.has(sid) || sid === blocked) throw new Error(`session "${sid}" already open`);
        if (mode === "read") {
          if (blocksAnn) throw new Error(`open ${sid}: \`blocks\` is only valid for a write session`);
          sessions.set(sid, { read: db.read() }); // readers never take the gate
        } else if (mode === "write") {
          if (blocksAnn) {
            // Layer 2: assert the gate is held, then QUEUE the open — calling write() now would throw
            // 25001 (a writer is active). It opens at the releasing step below.
            if (gateHolder === "") {
              throw new Error(`open ${sid} write blocks: the writer gate is free (nothing to block on)`);
            }
            if (blocked !== "") {
              throw new Error(`open ${sid} write blocks: writer "${blocked}" is already blocked (one at a time)`);
            }
            blocked = sid;
          } else {
            if (gateHolder !== "") {
              throw new Error(`open ${sid} write: the gate is held by "${gateHolder}" — use \`blocks\``);
            }
            sessions.set(sid, { write: db.write() });
            gateHolder = sid;
          }
        } else {
          throw new Error(`unknown session mode "${mode}" (want read|write)`);
        }
        c.i++;
        break;
      }
      case "commit":
      case "rollback":
      case "close": {
        if (fields.length < 2) throw new Error(`${fields[0]} needs a session id: ${line}`);
        const sid = fields[1]!;
        if (sid === blocked) {
          throw new Error(`${fields[0]} of "${sid}" while it is blocked on the writer gate`);
        }
        const s = sessions.get(sid);
        if (!s) throw new Error(`${fields[0]} of unknown session "${sid}"`);
        endSession(fields[0]!, s);
        sessions.delete(sid);
        // If the ended session held the gate, release it — and let the queued writer (if any) acquire
        // it now: it opens (write() no longer throws 25001) capturing the version just published, the
        // equivalent serial order (§5).
        if (sid === gateHolder) {
          gateHolder = "";
          if (blocked !== "") {
            sessions.set(blocked, { write: db.write() });
            gateHolder = blocked;
            blocked = "";
          }
        }
        c.i++;
        break;
      }
      case "expect": {
        if (fields.length < 3) throw new Error(`expect needs \`version|oldest_live <n>\`: ${line}`);
        const want = BigInt(fields[2]!);
        let got: bigint;
        if (fields[1] === "version") got = db.version;
        else if (fields[1] === "oldest_live") got = db.oldestLiveTxid();
        else throw new Error(`unknown expect kind "${fields[1]}" (want version|oldest_live)`);
        if (got !== want) throw new Error(`expect ${fields[1]} ${want}, got ${got}`);
        c.i++;
        break;
      }
      case "on": {
        if (fields.length < 3) throw new Error(`on needs \`<sid> <record>\`: ${line}`);
        const sid = fields[1]!;
        if (sid === blocked) throw new Error(`on "${sid}" while it is blocked on the writer gate`);
        const s = sessions.get(sid);
        if (!s) throw new Error(`on unknown session "${sid}"`);
        c.i++;
        runConcurrencyRecord(s, sid, fields.slice(2), lines, c);
        break;
      }
      default:
        throw new Error(`unknown concurrency directive "${fields[0]}"`);
    }
  }
  if (sessions.size !== 0 || blocked !== "") {
    // Deterministic message; Map iteration order is insertion order but we sort to never leak it.
    const open = [...sessions.keys()];
    if (blocked !== "") open.push(blocked);
    throw new Error(`file ended with sessions still open: ${open.sort().join(", ")}`);
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
      // A `# format: concurrency` file is an explicit multi-session schedule run against a SharedDb
      // (spec/design/concurrency-testing.md §4); everything else is the sequential single-handle
      // runner. Both share the result grammar; only the driver differs.
      if (isConcurrencyFormat(text)) runConcurrencyFile(text);
      else runFile(text);
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
