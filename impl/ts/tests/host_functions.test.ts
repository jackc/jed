// Host-defined scalar functions (spec/design/extensibility.md §4.2 / §5.1, delivery step 3). The
// registry/resolve/eval injection seam is a HOST-API surface the conformance corpus cannot express
// (it registers no host code), so it is tested per core (CLAUDE.md §10 — host-API is a sanctioned
// unit-test category). Mirrors impl/rust/tests/host_functions.rs one-for-one.

import assert from "node:assert/strict";
import { test } from "node:test";
import { createDatabase, EngineError, ExtensionRegistry } from "../src/lib.ts";
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
