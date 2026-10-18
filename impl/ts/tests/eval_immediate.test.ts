import assert from "node:assert/strict";
import { test } from "node:test";
import { Meter } from "../src/cost.ts";
import { COSTS } from "../src/costs.ts";
import { Environment } from "../src/environment.ts";
import {
  evalExpr,
  evalExprMaybeSteps,
  evalExprSteps,
  evalExpressionsMaybeSteps,
  evalFilterMaybeSteps,
  isPendingEval,
} from "../src/eval.ts";
import {
  Engine,
  EMPTY_CTE_CTX,
  type EvalEnv,
  type QueryPlan,
  type RExpr,
} from "../src/executor.ts";
import { ioSteps, runAsync, runSync } from "../src/execution.ts";
import { StmtRng } from "../src/seam.ts";
import { intValue, textValue, type Value } from "../src/value.ts";

const integer = (value: bigint): RExpr => ({ kind: "constInt", value });
const add = (lhs: RExpr, rhs: RExpr): RExpr => ({
  kind: "arith",
  op: "add",
  result: "i64",
  lhs,
  rhs,
});
const host: RExpr = { kind: "hostFunc", id: 0, name: "visit", args: [], result: "i64" };

function fixture() {
  const events: string[] = [];
  const builder = Environment.builder();
  builder.registerFunction({
    name: "visit",
    argTypes: [],
    result: "i64",
    volatility: "volatile",
    kernel() {
      events.push("host");
      return intValue(7n);
    },
  });
  const engine = new Engine(builder.build());
  const values = new Map<
    QueryPlan,
    { name: string; value: Value; wait?: Promise<void>; error?: Error }
  >();
  const env: EvalEnv = {
    params: [],
    outer: [],
    environment: engine.session.environment,
    rng: new StmtRng(),
    ctes: EMPTY_CTE_CTX,
    exec: engine,
    runSubquery() {
      throw new Error("unexpected synchronous subquery callback");
    },
    *runSubquerySteps(plan) {
      const item = values.get(plan)!;
      events.push(item.name);
      const value = yield* ioSteps(
        () => {
          if (item.error !== undefined) throw item.error;
          return item.value;
        },
        async () => {
          await item.wait;
          if (item.error !== undefined) throw item.error;
          return item.value;
        },
      );
      return {
        columnNames: ["v"],
        columnTypes: [{ kind: "int", ty: "i64" }],
        rows: [[value]],
        cost: 0n,
      };
    },
  };
  const deferred = (name: string, value: Value, wait?: Promise<void>, error?: Error): RExpr => {
    const plan = {} as QueryPlan;
    values.set(plan, { name, value, wait, error });
    return {
      kind: "subquery",
      plan,
      subKind: "scalar",
      lhs: null,
      negated: false,
      rowValued: false,
    };
  };
  return { env, events, deferred };
}

function state(code: string): (error: unknown) => boolean {
  return (error) =>
    error instanceof Error &&
    "code" in error &&
    typeof error.code === "function" &&
    error.code() === code;
}

test("resident expressions, filters and projections return immediate values with original cost", () => {
  const { env } = fixture();
  const expr: RExpr = {
    kind: "scalarFunc",
    func: "abs",
    args: [add({ kind: "column", index: 0 }, integer(1n))],
    result: "i64",
  };
  const meter = new Meter();
  const result = evalExprMaybeSteps(expr, [intValue(-9n)], env, meter);
  assert.equal(isPendingEval(result), false);
  assert.deepEqual(result, intValue(8n));
  assert.equal(meter.accrued, 2n * COSTS.operatorEval);
  const compare: RExpr = {
    kind: "compare",
    op: "eq",
    lhs: expr,
    rhs: integer(8n),
    collation: null,
  };
  assert.equal(evalFilterMaybeSteps(compare, [intValue(-9n)], env, new Meter()), true);
  const projection = evalExpressionsMaybeSteps(
    [expr, { kind: "constNull" }],
    [intValue(-9n)],
    env,
    new Meter(),
  );
  assert.equal(isPendingEval(projection), false);
  assert.deepEqual(projection, [intValue(8n), { kind: "null" }]);
  const cast: RExpr = {
    kind: "cast",
    operand: expr,
    target: "text",
    typmod: null,
    varcharLen: null,
    temporal: null,
  };
  assert.deepEqual(evalExprMaybeSteps(cast, [intValue(-9n)], env, new Meter()), textValue("8"));
});

test("pending binary left completes before the right host call and each operand runs once", async () => {
  const { env, events, deferred } = fixture();
  let release!: () => void;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const result = evalExprMaybeSteps(
    add(deferred("left", intValue(35n), gate), host),
    [],
    env,
    new Meter(),
  );
  assert.ok(isPendingEval(result));
  const running = runAsync(result);
  assert.deepEqual(events, ["left"]);
  release();
  assert.deepEqual(await running, intValue(42n));
  assert.deepEqual(events, ["left", "host"]);
});

test("pending binary right retains an earlier host result across success and failure", async () => {
  for (const fails of [false, true]) {
    const { env, events, deferred } = fixture();
    const failure = new Error("right failed");
    const expr = add(
      host,
      deferred("right", intValue(35n), Promise.resolve(), fails ? failure : undefined),
    );
    const result = evalExprMaybeSteps(expr, [], env, new Meter());
    assert.ok(isPendingEval(result));
    if (fails) await assert.rejects(runAsync(result), (error) => error === failure);
    else assert.deepEqual(await runAsync(result), intValue(42n));
    assert.deepEqual(events, ["host", "right"]);
  }
});

test("strict scalar arguments stop after a pending NULL before evaluating later effects", async () => {
  const { env, events, deferred } = fixture();
  const expr: RExpr = {
    kind: "scalarFunc",
    func: "substr",
    result: "text",
    args: [{ kind: "constText", value: "abc" }, deferred("null", { kind: "null" }), host],
  };
  assert.deepEqual(await runAsync(evalExprSteps(expr, [], env, new Meter())), { kind: "null" });
  assert.deepEqual(events, ["null"]);
});

test("pending boolean and filter left sides preserve distinct NULL short-circuit rules", async () => {
  for (const kind of ["and", "or"] as const) {
    const { env, events, deferred } = fixture();
    const expr: RExpr = {
      kind,
      lhs: deferred("left", { kind: "bool", value: kind === "or" }),
      rhs: deferred("unreached", { kind: "bool", value: true }),
    };
    assert.deepEqual(await runAsync(evalExprSteps(expr, [], env, new Meter())), {
      kind: "bool",
      value: kind === "or",
    });
    assert.deepEqual(events, ["left"]);
  }
  const { env, events, deferred } = fixture();
  const expr: RExpr = {
    kind: "and",
    lhs: deferred("null", { kind: "null" }),
    rhs: deferred("right", { kind: "bool", value: false }),
  };
  assert.deepEqual(await runAsync(evalExprSteps(expr, [], env, new Meter())), {
    kind: "bool",
    value: false,
  });
  assert.deepEqual(events, ["null", "right"]);
  events.length = 0;
  const filter = evalFilterMaybeSteps(expr, [], env, new Meter());
  assert.ok(isPendingEval(filter));
  assert.equal(await runAsync(filter), false);
  assert.deepEqual(events, ["null"]);
});

test("CASE and COALESCE still skip later branches after a suspended condition or argument", async () => {
  const { env, events, deferred } = fixture();
  const caseExpr: RExpr = {
    kind: "case",
    arms: [{ cond: deferred("condition", { kind: "bool", value: true }), result: host }],
    els: deferred("unreached", intValue(0n)),
    coerceDecimal: false,
  };
  assert.deepEqual(await runAsync(evalExprSteps(caseExpr, [], env, new Meter())), intValue(7n));
  assert.deepEqual(events, ["condition", "host"]);
  events.length = 0;
  const coalesce: RExpr = {
    kind: "coalesce",
    args: [deferred("first", intValue(9n)), host],
    coerceDecimal: false,
  };
  assert.deepEqual(await runAsync(evalExprSteps(coalesce, [], env, new Meter())), intValue(9n));
  assert.deepEqual(events, ["first"]);
});

test("wide pending argument lists retain one resume loop and preserve each prefix", () => {
  const { env, events, deferred } = fixture();
  const fields = Array.from({ length: 4096 }, (_, index) =>
    deferred(String(index), intValue(BigInt(index))),
  );
  const meter = new Meter();
  const result = evalExprMaybeSteps({ kind: "row", fields }, [], env, meter);
  assert.ok(isPendingEval(result));
  assert.deepEqual(runSync(result), {
    kind: "composite",
    fields: fields.map((_, index) => intValue(BigInt(index))),
  });
  assert.deepEqual(
    events,
    fields.map((_, index) => String(index)),
  );
  assert.equal(meter.accrued, BigInt(fields.length + 1) * COSTS.operatorEval);
});

test("cost errors precede later callbacks both before and after a suspension", async () => {
  const { env, events, deferred } = fixture();
  assert.throws(
    () => evalExprMaybeSteps(add(host, integer(1n)), [], env, new Meter(COSTS.operatorEval)),
    state("54P01"),
  );
  assert.deepEqual(events, []);
  const pending = evalExprMaybeSteps(
    add(deferred("left", intValue(1n)), host),
    [],
    env,
    new Meter(2n * COSTS.operatorEval),
  );
  assert.ok(isPendingEval(pending));
  await assert.rejects(runAsync(pending), state("54P01"));
  assert.deepEqual(events, ["left"]);
  const divide: RExpr = {
    kind: "arith",
    op: "div",
    result: "i64",
    lhs: integer(1n),
    rhs: integer(0n),
  };
  assert.throws(() => evalExpr(add(divide, host), [], env, new Meter()), state("22012"));
  assert.deepEqual(events, ["left"]);
});
