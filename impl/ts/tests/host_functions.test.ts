// Host-defined scalar functions (spec/design/extensibility.md §4.2 / §5.1, delivery step 3). The
// registry/resolve/eval injection seam is a HOST-API surface the conformance corpus cannot express
// (it registers no host code), so it is tested per core (CLAUDE.md §10 — host-API is a sanctioned
// unit-test category). Mirrors impl/rust/tests/host_functions.rs one-for-one.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";
import {
  createDatabase,
  type Database,
  EngineError,
  ExtensionRegistry,
  openDatabase,
} from "../src/lib.ts";
import type { HostFunctionSpec } from "../src/lib.ts";
import { intValue, textValue, type Value } from "../src/value.ts";
import type { Session } from "../src/shared.ts";
import { queryOutcome } from "./util.ts";

function asInt(v: Value): bigint {
  if (v.kind !== "int") throw new Error("expected int arg");
  return v.int;
}
function asText(v: Value): string {
  if (v.kind !== "text") throw new Error("expected text arg");
  return v.text;
}

// host_add(i64, i64) -> i64 — integer sum (strict: never sees NULL).
const hostAdd: HostFunctionSpec = {
  name: "host_add",
  argTypes: ["i64", "i64"],
  result: "i64",
  kernel: (args) => intValue(asInt(args[0]!) + asInt(args[1]!)),
  volatility: "immutable",
  crossCore: true,
};

// host_add(text, text) -> text — a same-name overload on a different signature.
const hostAddText: HostFunctionSpec = {
  name: "host_add",
  argTypes: ["text", "text"],
  result: "text",
  kernel: (args) => textValue(asText(args[0]!) + asText(args[1]!)),
};

function regWith(...specs: HostFunctionSpec[]): ExtensionRegistry {
  const r = new ExtensionRegistry();
  for (const s of specs) r.registerFunction(s);
  return r;
}

function dbExt(extensions: ExtensionRegistry, stmts: string[] = []): Session {
  const s = createDatabase({ extensions }).session();
  for (const stmt of stmts) s.execute(stmt);
  return s;
}

function rowsOf(s: Session, sql: string): Value[][] {
  const o = queryOutcome(s, sql);
  if (o.kind !== "query") throw new Error(`expected a query result for ${sql}`);
  return o.rows;
}

function oneVal(s: Session, sql: string): Value {
  const rows = rowsOf(s, sql);
  if (rows.length !== 1 || rows[0]!.length !== 1)
    throw new Error(`expected one scalar row for ${sql}`);
  return rows[0]![0]!;
}

function errCodeOf(fn: () => void): string {
  try {
    fn();
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error("expected an EngineError");
}

test("host scalar function over literals", () => {
  const s = dbExt(regWith(hostAdd));
  assert.equal(asInt(oneVal(s, "SELECT host_add(2, 3)")), 5n);
  assert.equal(asInt(oneVal(s, "SELECT host_add(host_add(1, 1), 40)")), 42n);
});

test("host scalar function over columns", () => {
  const s = dbExt(regWith(hostAdd), [
    "CREATE TABLE t (id i32 PRIMARY KEY, a i64, b i64)",
    "INSERT INTO t VALUES (1, 10, 20), (2, 100, 1)",
  ]);
  const rows = rowsOf(s, "SELECT host_add(a, b) FROM t ORDER BY id");
  assert.deepEqual(
    rows.map((r) => asInt(r[0]!)),
    [30n, 101n],
  );
});

test("host function is strict on a typed NULL", () => {
  // A NULL-valued argument of a KNOWN type short-circuits to NULL before the kernel runs (§4.2); the
  // kernel (which throws on a non-int arg) is never called.
  const s = dbExt(regWith(hostAdd), [
    "CREATE TABLE t (id i32 PRIMARY KEY, a i64, b i64)",
    "INSERT INTO t VALUES (1, NULL, 20)",
  ]);
  assert.equal(oneVal(s, "SELECT host_add(a, b) FROM t").kind, "null");
});

test("bare NULL literal finds no overload", () => {
  // A bare untyped NULL matches no concrete scalar signature — 42883, exactly as a built-in
  // (abs(NULL)) behaves. Strictness is an eval-time property of a TYPED null, not a resolution one.
  const s = dbExt(regWith(hostAdd));
  assert.equal(
    errCodeOf(() => queryOutcome(s, "SELECT host_add(NULL, 3)")),
    "42883",
  );
});

test("overload by signature", () => {
  const s = dbExt(regWith(hostAdd, hostAddText));
  assert.equal(asInt(oneVal(s, "SELECT host_add(2, 3)")), 5n);
  assert.equal(asText(oneVal(s, "SELECT host_add('foo', 'bar')")), "foobar");
});

test("built-in wins over host on the same signature", () => {
  // Registering a host abs(i64) is accepted but never reached — the built-in abs shadows it (§4.2).
  // If the host kernel (returning a sentinel 999) ran, abs(-5) would be 999.
  const s = dbExt(
    regWith({
      name: "abs",
      argTypes: ["i64"],
      result: "i64",
      kernel: () => intValue(999n),
    }),
  );
  assert.equal(asInt(oneVal(s, "SELECT abs(-5)")), 5n);
});

test("duplicate signature is rejected", () => {
  const r = new ExtensionRegistry();
  r.registerFunction(hostAdd);
  // Same (name, arg types) — rejected 42723 (signature-level, §4.2).
  assert.equal(
    errCodeOf(() => r.registerFunction(hostAdd)),
    "42723",
  );
  // A different signature on the same name is fine (overloading).
  r.registerFunction(hostAddText);
});

test("negative cost is rejected", () => {
  const r = new ExtensionRegistry();
  assert.equal(
    errCodeOf(() =>
      r.registerFunction({
        name: "host_neg",
        argTypes: [],
        result: "i64",
        kernel: () => intValue(0n),
        cost: -1n,
      }),
    ),
    "22023",
  );
});

test("unknown type name is rejected", () => {
  const r = new ExtensionRegistry();
  assert.equal(
    errCodeOf(() =>
      r.registerFunction({
        name: "host_bad",
        // deliberately invalid type name (validated at registration)
        argTypes: ["not_a_type" as never],
        result: "i64",
        kernel: () => intValue(0n),
      }),
    ),
    "42704",
  );
});

test("declared cost is charged per call", () => {
  // Two 0-arg functions identical but for their declared static weight; the query-cost difference is
  // exactly the weight difference (cost.md §6 design (a), charged once per call).
  const const0 = (name: string, cost: bigint): HostFunctionSpec => ({
    name,
    argTypes: [],
    result: "i64",
    kernel: () => intValue(0n),
    cost,
  });
  const s = dbExt(regWith(const0("host_c0", 0n), const0("host_c1000", 1000n)));
  const c0 = queryOutcome(s, "SELECT host_c0()").cost;
  const c1000 = queryOutcome(s, "SELECT host_c1000()").cost;
  assert.equal(c1000 - c0, 1000n);
});

test("declared cost gates the max_cost ceiling", () => {
  // A declared weight above the ceiling aborts 54P01 before the kernel runs (guard after charge).
  const s = createDatabase({
    extensions: regWith({
      name: "host_heavy",
      argTypes: [],
      result: "i64",
      kernel: () => intValue(0n),
      cost: 1_000_000n,
    }),
  }).session();
  s.setMaxCost(1000n);
  assert.equal(
    errCodeOf(() => queryOutcome(s, "SELECT host_heavy()")),
    "54P01",
  );
});

test("wrong result type is rejected", () => {
  // A kernel that violates its declared RETURNS i64 (returns text) is caught (22000) rather than
  // leaking a wrong-typed value into jed's strict type system (CLAUDE.md §13).
  const s = dbExt(
    regWith({
      name: "host_liar",
      argTypes: [],
      result: "i64",
      kernel: () => textValue("oops"),
    }),
  );
  assert.equal(
    errCodeOf(() => queryOutcome(s, "SELECT host_liar()")),
    "22000",
  );
});

test("unknown host function is still undefined", () => {
  const s = dbExt(regWith(hostAdd));
  assert.equal(
    errCodeOf(() => queryOutcome(s, "SELECT host_missing(1)")),
    "42883",
  );
});

test("EXPLAIN renders the host function name", () => {
  const s = dbExt(regWith(hostAdd), ["CREATE TABLE t (id i32 PRIMARY KEY, a i64, b i64)"]);
  const text = rowsOf(s, "EXPLAIN (VERBOSE) SELECT host_add(a, b) FROM t")
    .flat()
    .filter((v): v is { kind: "text"; text: string } => v.kind === "text")
    .map((v) => v.text)
    .join("\n");
  assert.ok(
    text.includes("host_add("),
    `EXPLAIN should render the host function name; got:\n${text}`,
  );
});

test("no extensions is unaffected", () => {
  // The built-in-only path is untouched: an empty registry resolves nothing new, and a call to a
  // would-be host name is 42883.
  const s = dbExt(new ExtensionRegistry());
  assert.equal(asInt(oneVal(s, "SELECT abs(-7)")), 7n);
  assert.equal(
    errCodeOf(() => queryOutcome(s, "SELECT host_add(1, 2)")),
    "42883",
  );
  // A handle opened with no extensions option (undefined registry) behaves the same.
  const s2 = createDatabase({}).session();
  assert.equal(
    errCodeOf(() => queryOutcome(s2, "SELECT host_add(1, 2)")),
    "42883",
  );
});

// ---------------------------------------------------------------------------------------------
// Delivery step 4 (extensibility.md §8.1 / §14): host scalar functions in PERSISTED INDEXES.
// An `immutable` host function carrying a componentId + semanticVersion may back an expression /
// partial index; the file records the resolved dependency (format_version 31) and re-checks it on
// reopen. A missing / different-component / bumped-version function makes the index unusable —
// skipped for reads (correct heap scan), refused for writes (read-only) — never a silent stale-key
// read. These cover what the corpus cannot express (host-API registration + on-disk reopen with a
// *different* registry); they mirror impl/rust/tests/host_functions.rs one-for-one.
// ---------------------------------------------------------------------------------------------

// geo_hash(i64) -> i64 — the canonical index-backing host function. component/version pin its
// identity; "immutable" + a componentId are the two admission requirements (§8.1).
function geoHash(component: string, version: number): HostFunctionSpec {
  return {
    name: "geo_hash",
    argTypes: ["i64"],
    result: "i64",
    kernel: (args) => intValue(asInt(args[0]!) * 10n),
    volatility: "immutable",
    crossCore: true,
    componentId: component,
    semanticVersion: version,
  };
}

// One scratch directory per run, removed whole after the last test — so each database's `<path>.lock/`
// coordination bundle (spec/design/locking.md) goes with it, even when a test fails partway.
const scratch = mkdtempSync(join(tmpdir(), "jed-hostfunc-"));
after(() => rmSync(scratch, { recursive: true, force: true }));

function tmpFile(name: string): string {
  return join(scratch, name);
}

function createFile(path: string, reg: ExtensionRegistry, stmts: string[]): void {
  const db = createDatabase({ path, skipFsync: true, extensions: reg });
  for (const s of stmts) db.execute(s);
  db.close();
}

function openFile(path: string, reg: ExtensionRegistry): Database {
  return openDatabase(path, { skipFsync: true, extensions: reg });
}

function idsFile(db: Database, sql: string): bigint[] {
  const o = queryOutcome(db, sql);
  if (o.kind !== "query") throw new Error(`expected a query result for ${sql}`);
  return o.rows.map((r) => asInt(r[0]!));
}

test("volatile host function in an index is rejected", () => {
  // The latent-bug fix: a volatile host function used to leak silently into an index expression (the
  // immutability gate was purely syntactic and did not see host functions). Now 42P17.
  const volatileGeo: HostFunctionSpec = {
    name: "geo_hash",
    argTypes: ["i64"],
    result: "i64",
    kernel: (args) => intValue(asInt(args[0]!) * 10n),
    componentId: "com.example/geo_hash", // volatile by default
  };
  const s = dbExt(regWith(volatileGeo), ["CREATE TABLE t (a i64)"]);
  assert.equal(
    errCodeOf(() => queryOutcome(s, "CREATE INDEX ix ON t (geo_hash(a))")),
    "42P17",
  );
});

test("unversioned host function in an index is rejected", () => {
  // Immutable but no component identity → cannot persist a sound dependency (42P17).
  const unversioned: HostFunctionSpec = {
    name: "geo_hash",
    argTypes: ["i64"],
    result: "i64",
    kernel: (args) => intValue(asInt(args[0]!) * 10n),
    volatility: "immutable",
  };
  const s = dbExt(regWith(unversioned), ["CREATE TABLE t (a i64)"]);
  assert.equal(
    errCodeOf(() => queryOutcome(s, "CREATE INDEX ix ON t (geo_hash(a))")),
    "42P17",
  );
});

test("immutable versioned host function in an index is ok", () => {
  const s = dbExt(regWith(geoHash("com.example/geo_hash", 1)), [
    "CREATE TABLE t (id i64 PRIMARY KEY, a i64)",
    "INSERT INTO t VALUES (1, 3), (2, 7)",
    "CREATE INDEX ix ON t (geo_hash(a))",
  ]);
  // geo_hash(3) = 30 → row id 1.
  assert.deepEqual(
    rowsOf(s, "SELECT id FROM t WHERE geo_hash(a) = 30").map((r) => asInt(r[0]!)),
    [1n],
  );
});

test("host-dep index reopen with a matching registry is usable", () => {
  const path = tmpFile("hostfunc_index_match.jed");
  createFile(path, regWith(geoHash("com.example/geo_hash", 1)), [
    "CREATE TABLE t (id i64 PRIMARY KEY, a i64)",
    "INSERT INTO t VALUES (1, 3), (2, 7)",
    "CREATE INDEX ix ON t (geo_hash(a))",
  ]);
  // Reopen (v31 deserialize) with the SAME component + version: the dependency matches, so reads use
  // the index and writes maintain it.
  const db = openFile(path, regWith(geoHash("com.example/geo_hash", 1)));
  assert.deepEqual(idsFile(db, "SELECT id FROM t WHERE geo_hash(a) = 30"), [1n]);
  queryOutcome(db, "INSERT INTO t VALUES (3, 3)");
  assert.deepEqual(idsFile(db, "SELECT id FROM t WHERE geo_hash(a) = 30 ORDER BY id"), [1n, 3n]);
  db.close();
});

test("host-dep index reopen with a bumped version is unusable", () => {
  const path = tmpFile("hostfunc_index_bump.jed");
  createFile(path, regWith(geoHash("com.example/geo_hash", 1)), [
    "CREATE TABLE t (id i64 PRIMARY KEY, a i64)",
    "INSERT INTO t VALUES (1, 3), (2, 7)",
    "CREATE INDEX ix ON t (geo_hash(a))",
  ]);
  // Reopen with a BUMPED semanticVersion → the index's stored keys are stale.
  const db = openFile(path, regWith(geoHash("com.example/geo_hash", 2)));
  // Reads still correct: a plain read (no index) and one that COULD use the index (skipped → heap
  // scan) both return the right rows — never a silent stale-key read.
  assert.deepEqual(idsFile(db, "SELECT id FROM t ORDER BY id"), [1n, 2n]);
  assert.deepEqual(idsFile(db, "SELECT id FROM t WHERE geo_hash(a) = 30"), [1n]);
  // A write that would maintain the stale index is refused (XX002) — the table is read-only.
  assert.equal(
    errCodeOf(() => queryOutcome(db, "INSERT INTO t VALUES (3, 3)")),
    "XX002",
  );
  db.close();
});

test("host-dep index reopen with a different component is unusable", () => {
  const path = tmpFile("hostfunc_index_component.jed");
  createFile(path, regWith(geoHash("com.example/geo_hash", 1)), [
    "CREATE TABLE t (id i64 PRIMARY KEY, a i64)",
    "INSERT INTO t VALUES (1, 3)",
    "CREATE INDEX ix ON t (geo_hash(a))",
  ]);
  // Reopen with a DIFFERENT component id for the same name/signature → a different implementation.
  const db = openFile(path, regWith(geoHash("org.other/geo_hash", 1)));
  assert.deepEqual(idsFile(db, "SELECT id FROM t WHERE geo_hash(a) = 30"), [1n]);
  assert.equal(
    errCodeOf(() => queryOutcome(db, "INSERT INTO t VALUES (2, 3)")),
    "XX002",
  );
  db.close();
});

test("host-dep index reopen with the function missing", () => {
  const path = tmpFile("hostfunc_index_missing.jed");
  createFile(path, regWith(geoHash("com.example/geo_hash", 1)), [
    "CREATE TABLE t (id i64 PRIMARY KEY, a i64)",
    "INSERT INTO t VALUES (1, 3), (2, 7)",
    "CREATE INDEX ix ON t (geo_hash(a))",
  ]);
  // Reopen with NO extensions: the index expression can no longer resolve.
  const db = openFile(path, new ExtensionRegistry());
  // A read that does not reference the missing function still works (the index is simply unused).
  assert.deepEqual(idsFile(db, "SELECT id FROM t ORDER BY id"), [1n, 2n]);
  // A write that would maintain the index needs the missing function → 42883 (resolution fails).
  assert.equal(
    errCodeOf(() => queryOutcome(db, "INSERT INTO t VALUES (3, 3)")),
    "42883",
  );
  db.close();
});

// ---------------------------------------------------------------------------------------------
// The batched kernel ABI (extensibility.md §4.2.1). Batching is host-observable only (how often, and
// over how many rows, a kernel runs); rows, cost, and errors must be identical to batch-of-one. These
// mirror the Rust/Go batch tests one-for-one.
// ---------------------------------------------------------------------------------------------

// host_twice(i64) -> i64 as a BATCH kernel: doubles each row; throws at the row whose value is
// `failAt` (the prefix rule — the rows before it are pushed first). Records each call's row count.
function twiceBatched(
  calls: number[],
  volatility: "immutable" | "volatile",
  failAt: bigint | null,
): HostFunctionSpec {
  return {
    name: "host_twice",
    argTypes: ["i64"],
    result: "i64",
    batchKernel: (args, out) => {
      calls.push(args[0]!.length);
      for (const v of args[0]!) {
        const x = asInt(v);
        if (x === failAt)
          throw new EngineError("invalid_parameter_value", `host_twice refuses ${x}`);
        out.push(intValue(x * 2n));
      }
    },
    volatility,
    cost: 5n,
  };
}

// The same function as a SINGLE-ROW kernel — the batch-of-one reference.
function twiceRow(calls: number[], failAt: bigint | null): HostFunctionSpec {
  return {
    name: "host_twice",
    argTypes: ["i64"],
    result: "i64",
    kernel: (args) => {
      calls.push(1);
      const x = asInt(args[0]!);
      if (x === failAt) throw new EngineError("invalid_parameter_value", `host_twice refuses ${x}`);
      return intValue(x * 2n);
    },
    volatility: "immutable",
    cost: 5n,
  };
}

// A table t(id i64 PRIMARY KEY, a i64) of `n` rows (g, g), with `a` NULL where g % 10 = 0.
function batchDb(f: HostFunctionSpec, n: number): Session {
  return dbExt(regWith(f), [
    "CREATE TABLE t (id i64 PRIMARY KEY, a i64)",
    `INSERT INTO t SELECT g, CASE WHEN g % 10 = 0 THEN NULL ELSE g END FROM generate_series(1, ${n}) AS g`,
  ]);
}

// { rows, cost } of a query, or its "code message" error.
function runQ(s: Session, sql: string): { rows: Value[][]; cost: bigint } | string {
  try {
    const o = queryOutcome(s, sql);
    if (o.kind !== "query") throw new Error(`expected a query result for ${sql}`);
    return { rows: o.rows, cost: o.cost };
  } catch (e) {
    if (e instanceof EngineError) return `${e.code()} ${e.message}`;
    throw e;
  }
}

test("batch kernel is called once per chunk", () => {
  // 2500 rows (250 of them NULL) through the buffered projection: three chunks of ≤1024 rows, each
  // one kernel call over its non-NULL rows only (strict).
  const calls: number[] = [];
  const s = batchDb(twiceBatched(calls, "immutable", null), 2500);
  const got = runQ(s, "SELECT id, host_twice(a) FROM t");
  assert.ok(typeof got !== "string", String(got));
  assert.equal(got.rows.length, 2500);
  for (const r of got.rows) {
    const id = asInt(r[0]!);
    if (id % 10n === 0n) assert.equal(r[1]!.kind, "null");
    else assert.equal(asInt(r[1]!), id * 2n);
  }
  assert.deepEqual(calls, [922, 922, 406]);
});

test("batch matches the row kernel in rows and cost", () => {
  // The same query against a batch kernel and a single-row kernel: identical rows and cost.
  const sqls = [
    "SELECT id, host_twice(a) FROM t",
    "SELECT id, host_twice(a), host_twice(id) FROM t WHERE id > 100",
    "SELECT a, host_twice(a) FROM t ORDER BY a DESC LIMIT 7 OFFSET 3",
    "SELECT x.id, host_twice(y.a) FROM t AS x JOIN t AS y ON x.id = y.id + 1",
    "SELECT host_twice(count(*)) FROM t",
  ];
  for (const sql of sqls) {
    const batched = batchDb(twiceBatched([], "immutable", null), 1500);
    const single = batchDb(twiceRow([], null), 1500);
    assert.deepEqual(runQ(batched, sql), runQ(single, sql), sql);
  }
});

test("volatile batch kernel is called per row", () => {
  // A volatile function's call set stays exactly the scalar one: one-row batches, one per non-NULL
  // row.
  const calls: number[] = [];
  const s = batchDb(twiceBatched(calls, "volatile", null), 30);
  runQ(s, "SELECT host_twice(a) FROM t");
  assert.deepEqual(calls, new Array(27).fill(1));
});

test("batch error raises at the scalar row", () => {
  // The kernel fails at a = 57. Alone, that is the error; with an earlier per-row error in the same
  // projection (division by zero at id = 40) the earlier row still wins, exactly as the single-row
  // kernel orders them.
  for (const sql of [
    "SELECT host_twice(a) FROM t",
    "SELECT 1 / (id - 40), host_twice(a) FROM t",
    "SELECT host_twice(a), 1 / (id - 40) FROM t",
    "SELECT host_twice(a), 1 / (id - 60) FROM t",
  ]) {
    const batched = batchDb(twiceBatched([], "immutable", 57n), 200);
    const single = batchDb(twiceRow([], 57n), 200);
    const got = runQ(batched, sql);
    assert.equal(typeof got, "string", sql);
    assert.deepEqual(got, runQ(single, sql), sql);
  }
});

test("batch cost abort matches the row kernel and bounds speculation", () => {
  // Under a ceiling the abort is the single-row kernel's (same code, same accrued cost), and the
  // prefetch ran the kernel over no more rows than the budget could pay for (§4.2.1).
  const calls: number[] = [];
  const batched = batchDb(twiceBatched(calls, "immutable", null), 2000);
  const single = batchDb(twiceRow([], null), 2000);
  const budget = 200n;
  batched.setMaxCost(budget);
  single.setMaxCost(budget);
  const sql = "SELECT id, host_twice(a) FROM t";
  const b = runQ(batched, sql);
  const s = runQ(single, sql);
  assert.equal(typeof b, "string");
  assert.ok(String(b).startsWith("54P01 "), String(b));
  assert.equal(b, s);
  const speculated = calls.reduce((a, c) => a + c, 0);
  assert.ok(speculated <= Number(budget / 5n) + 1, `kernel ran over ${speculated} rows`);
});

test("batch through the streaming cursor matches", () => {
  // The lazy query() cursor drives the same buffered projection: same rows, chunked calls.
  const calls: number[] = [];
  const s = batchDb(twiceBatched(calls, "immutable", null), 1100);
  const rows: Value[][] = [];
  const cur = s.query("SELECT host_twice(a) FROM t");
  try {
    for (const r of cur) rows.push(r);
  } finally {
    cur.close();
  }
  assert.equal(rows.length, 1100);
  assert.equal(asInt(rows[0]![0]!), 2n);
  assert.equal(rows[9]![0]!.kind, "null");
  assert.deepEqual(calls, [922, 68]);
});

test("batch through the eager materialized drive matches", () => {
  // TS-only: a top-level SELECT drains through the lazy cursor above, so the eager drive
  // (drainEmitterEager — a WITH body, a set-operation arm, a derived table, INSERT … SELECT) is
  // reached through a WITH wrapper here. Same chunked calls, and the same rows/cost as the row kernel.
  const calls: number[] = [];
  const s = batchDb(twiceBatched(calls, "immutable", null), 2500);
  const sql = "WITH w AS (SELECT 1) SELECT id, host_twice(a) FROM t";
  const got = runQ(s, sql);
  assert.deepEqual(calls, [922, 922, 406]);
  assert.deepEqual(got, runQ(batchDb(twiceRow([], null), 2500), sql));
});

test("batch shape violations are 22000", () => {
  // Too few results on a normal return, and too many, are caught rather than misattributed.
  const shaped = (extra: boolean): HostFunctionSpec => ({
    name: "host_shape",
    argTypes: ["i64"],
    result: "i64",
    batchKernel: (args, out) => {
      const n = args[0]!.length;
      const want = extra ? n + 1 : n - 1;
      for (let i = 0; i < want; i++) out.push(intValue(0n));
    },
    volatility: "immutable",
  });
  for (const extra of [false, true]) {
    const s = batchDb(shaped(extra), 20);
    assert.equal(
      errCodeOf(() => queryOutcome(s, "SELECT host_shape(a) FROM t")),
      "22000",
      `extra=${extra}`,
    );
  }
  // A wrong-typed result in a batch is caught at its own row too.
  const s = batchDb(
    {
      name: "host_liar",
      argTypes: ["i64"],
      result: "i64",
      batchKernel: (args, out) => {
        for (const _ of args[0]!) out.push(textValue("oops"));
      },
      volatility: "immutable",
    },
    20,
  );
  assert.equal(
    errCodeOf(() => queryOutcome(s, "SELECT host_liar(a) FROM t")),
    "22000",
  );
});

test("batch kernel serves a lone call", () => {
  // Outside a prefetching site (a FROM-less SELECT, a WHERE filter) a batch kernel is handed a
  // one-row batch.
  const calls: number[] = [];
  const s = batchDb(twiceBatched(calls, "immutable", null), 5);
  assert.equal(asInt(oneVal(s, "SELECT host_twice(21)")), 42n);
  const rows = rowsOf(s, "SELECT id FROM t WHERE host_twice(a) = 6");
  assert.deepEqual(
    rows.map((r) => asInt(r[0]!)),
    [3n],
  );
  assert.deepEqual(calls, new Array(6).fill(1));
});

test("exactly one of kernel and batchKernel is required", () => {
  // Both, or neither, is rejected 22023 at registration (the code a negative cost uses).
  const r = new ExtensionRegistry();
  const base = { name: "host_k", argTypes: [] as [], result: "i64" as const };
  assert.equal(
    errCodeOf(() => r.registerFunction(base)),
    "22023",
  );
  assert.equal(
    errCodeOf(() =>
      r.registerFunction({
        ...base,
        kernel: () => intValue(0n),
        batchKernel: (_args, out) => {
          out.push(intValue(0n));
        },
      }),
    ),
    "22023",
  );
});
