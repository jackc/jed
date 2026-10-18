import assert from "node:assert/strict";
import { test } from "node:test";
import { Meter } from "../src/cost.ts";
import { evalExpr, evalExprSteps } from "../src/eval.ts";
import {
  Engine,
  EMPTY_CTE_CTX,
  type EvalEnv,
  type QueryPlan,
  type RExpr,
} from "../src/executor.ts";
import { ioSteps, runAsync, type Steps } from "../src/execution.ts";
import { makePage, resolveUnfetchedSelfSteps } from "../src/format.ts";
import { compileLaneFilter, laneKeepSteps, ScratchRow } from "../src/span_filter.ts";
import type { PNode } from "../src/pmap.ts";
import { StmtRng } from "../src/seam.ts";
import { intValue, textValue, type UnfetchedPager, type Value } from "../src/value.ts";

function environment(): EvalEnv {
  const engine = new Engine();
  return {
    params: [],
    outer: [],
    environment: engine.session.environment,
    rng: new StmtRng(),
    ctes: EMPTY_CTE_CTX,
    exec: engine,
    runSubquery() {
      throw new Error("unexpected synchronous subquery");
    },
  };
}

function externalText(text: string, pager: UnfetchedPager): Value {
  const bytes = new TextEncoder().encode(text);
  return {
    kind: "unfetched",
    ref: {
      form: 0x02,
      firstPage: 7,
      storedLen: bytes.length,
      rawLen: bytes.length,
      comp: undefined,
      ty: { cols: [{ kind: "scalar", scalar: "text" }], idx: 0 },
      paging: pager,
    },
  };
}

function textPages(text: string): Map<number, Uint8Array> {
  const bytes = new TextEncoder().encode(text);
  const pages = new Map<number, Uint8Array>();
  for (let start = 0, page = 7; start < bytes.length; start += 240, page++) {
    const slab = bytes.subarray(start, Math.min(start + 240, bytes.length));
    pages.set(
      page,
      makePage(256, 4, slab.length, start + slab.length < bytes.length ? page + 1 : 0, slab),
    );
  }
  return pages;
}

test("an expression resumes its overflow read without replaying an earlier subquery", async () => {
  const text = "overflow value ".repeat(60);
  const pages = textPages(text);
  const reads: number[] = [];
  let release!: () => void;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const pager: UnfetchedPager = {
    readBlock(index) {
      return pages.get(index)!;
    },
    *readBlockSteps(index): Steps<Uint8Array> {
      return yield* ioSteps(
        () => this.readBlock(index),
        async () => {
          reads.push(index);
          if (index === 7) await gate;
          return pages.get(index)!;
        },
      );
    },
  };
  let calls = 0;
  const env = environment();
  // The callback intentionally owns plan evaluation, just as a correlated executor does.
  const plan = {} as QueryPlan;
  // biome-ignore lint/correctness/useYield: this test implements the subquery step interface with an immediate result.
  env.runSubquerySteps = function* (actual, outer) {
    assert.equal(actual, plan);
    assert.equal(outer.length, 1);
    calls++;
    return {
      columnNames: ["v"],
      columnTypes: [{ kind: "int", ty: "i64" }],
      rows: [[intValue(9n)]],
      cost: 3n,
    };
  };
  const expr: RExpr = {
    kind: "row",
    fields: [
      { kind: "subquery", plan, subKind: "scalar", lhs: null, negated: false, rowValued: false },
      { kind: "column", index: 0 },
    ],
  };
  const row = [externalText(text, pager)];
  const meter = new Meter();
  const result = runAsync(evalExprSteps(expr, row, env, meter));
  assert.equal(calls, 1);
  assert.deepEqual(reads, [7]);
  release();
  assert.deepEqual(await result, { kind: "composite", fields: [intValue(9n), textValue(text)] });
  assert.equal(calls, 1);
  assert.deepEqual(reads, [...pages.keys()]);
  const syncMeter = new Meter();
  assert.deepEqual(evalExpr(expr, row, env, syncMeter), {
    kind: "composite",
    fields: [intValue(9n), textValue(text)],
  });
  assert.equal(meter.accrued, syncMeter.accrued);
});

test("short-circuit expressions do not issue reads for an untouched deferred value", async () => {
  const pager: UnfetchedPager = {
    readBlock() {
      throw new Error("untouched value read synchronously");
    },
    // biome-ignore lint/correctness/useYield: this step-interface implementation fails if the untouched branch is reached.
    *readBlockSteps(): Steps<Uint8Array> {
      throw new Error("untouched value read asynchronously");
    },
  };
  const expr: RExpr = {
    kind: "coalesce",
    args: [
      { kind: "constText", value: "first" },
      { kind: "column", index: 0 },
    ],
    coerceDecimal: false,
  };
  assert.deepEqual(
    await runAsync(evalExprSteps(expr, [externalText("later", pager)], environment(), new Meter())),
    textValue("first"),
  );
});

test("cancellation after an awaited overflow page prevents later page reads", async () => {
  const text = "large ".repeat(100);
  const pages = textPages(text);
  const reads: number[] = [];
  let canceled = false;
  const pager: UnfetchedPager = {
    readBlock() {
      throw new Error("async read unexpectedly used synchronous adapter");
    },
    *readBlockSteps(index): Steps<Uint8Array> {
      return yield* ioSteps(
        () => this.readBlock(index),
        async () => {
          reads.push(index);
          await Promise.resolve();
          canceled = true;
          return pages.get(index)!;
        },
      );
    },
  };
  const value = externalText(text, pager);
  assert.equal(value.kind, "unfetched");
  if (value.kind !== "unfetched") throw new Error("expected deferred fixture");
  await assert.rejects(
    runAsync(resolveUnfetchedSelfSteps(value.ref), { check: () => canceled }),
    (error: unknown) =>
      error instanceof Error &&
      "code" in error &&
      typeof error.code === "function" &&
      error.code() === "57014",
  );
  assert.deepEqual(reads, [7]);
});

test("columnar predicates suspend on deferred columns and reuse the resumed value", async () => {
  const text = "columnar ".repeat(50);
  const pages = textPages(text);
  const reads: number[] = [];
  const pager: UnfetchedPager = {
    readBlock() {
      throw new Error("columnar read used synchronous adapter");
    },
    *readBlockSteps(index): Steps<Uint8Array> {
      return yield* ioSteps(
        () => this.readBlock(index),
        async () => {
          reads.push(index);
          await Promise.resolve();
          return pages.get(index)!;
        },
      );
    },
  };
  const node: PNode = {
    keys: [new Uint8Array([1])],
    vals: [[externalText(text, pager)]],
    weights: [1],
    children: [],
    page: 0,
  };
  const env = environment();
  const expr: RExpr = {
    kind: "compare",
    op: "eq",
    lhs: { kind: "column", index: 0 },
    rhs: { kind: "constText", value: text },
    collation: null,
  };
  const filter = compileLaneFilter(expr, [{ kind: "scalar", scalar: "text" }], [true], env);
  const scratch = new ScratchRow(1);
  assert.equal(await runAsync(laneKeepSteps(filter, node, 0, scratch, env, new Meter())), true);
  assert.deepEqual(reads, [...pages.keys()]);
  assert.deepEqual(await runAsync(scratch.readSteps(node, 0, 0)), textValue(text));
  assert.deepEqual(reads, [...pages.keys()]);
});
