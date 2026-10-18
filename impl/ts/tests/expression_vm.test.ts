// The shared experiment corpus pins evaluator behavior that SQL planning can otherwise fold away:
// exact guard/charge boundaries, filter-context NULL branches, and runtime overflow visitation.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { LifetimeBudget, Meter } from "../src/cost.ts";
import { EngineError } from "../src/errors.ts";
import { evalExpr, evalFilter } from "../src/eval.ts";
import { EMPTY_CTE_CTX, type EvalEnv, type RExpr, Scope, ParamTypes } from "../src/executor.ts";
import {
  compileExpressionVm,
  expressionVmStats,
  resetExpressionVmStats,
  withExpressionVm,
} from "../src/expression_vm.ts";
import { parseExpression } from "../src/parser.ts";
import { resolve } from "../src/resolve.ts";
import { StmtRng } from "../src/seam.ts";
import { Engine, execute, createDatabase, drainOutcome } from "../src/tooling.ts";
import { boolValue, intValue, nullValue, render, type Value } from "../src/value.ts";

function fixture(name: string): string[][] {
  return readFileSync(
    new URL(`../../../spec/experiments/expression-vm/${name}`, import.meta.url),
    "utf8",
  )
    .split("\n")
    .filter((line) => line.length > 0 && !line.startsWith("#"))
    .map((line) => line.split("\t"));
}

function inputValue(cell: string): Value {
  if (cell === "NULL") return nullValue();
  if (cell === "true" || cell === "false") return boolValue(cell === "true");
  return intValue(BigInt(cell));
}

function evaluate(
  expr: RExpr,
  row: Value[],
  env: EvalEnv,
  filter: boolean,
  vm: boolean,
  limit: bigint,
  lifetime = false,
) {
  const budget = lifetime ? new LifetimeBudget(limit) : undefined;
  const meter = new Meter(lifetime ? 0n : limit, budget);
  try {
    const value = withExpressionVm(vm, () =>
      filter ? boolValue(evalFilter(expr, row, env, meter)) : evalExpr(expr, row, env, meter),
    );
    return { value: render(value), kind: value.kind, cost: meter.accrued, error: null };
  } catch (error) {
    if (!(error instanceof EngineError)) throw error;
    return {
      value: null,
      kind: null,
      cost: meter.accrued,
      error: {
        code: error.code(),
        message: error.message,
        position: error.position,
        detail: error.detail,
        hint: error.hint,
        constraintName: error.constraintName,
        tableName: error.tableName,
        columnName: error.columnName,
        dataTypeName: error.dataTypeName,
      },
    };
  }
}

test("expression VM matches shared cases at every statement and lifetime budget boundary", () => {
  const engine = new Engine();
  execute(engine, "CREATE TABLE t(id i32 PRIMARY KEY, a i32, b i32, p boolean, q boolean)");
  const scope = new Scope(
    [{ label: "t", table: engine.table("t")!, offset: 0 }],
    engine,
    null,
    false,
  );
  const env: EvalEnv = {
    params: [intValue(7n), intValue(3n)],
    outer: [],
    runSubquery: () => {
      throw new Error("unexpected subquery in experiment");
    },
    environment: engine.session.environment,
    rng: new StmtRng(),
    ctes: EMPTY_CTE_CTX,
    exec: engine,
  };
  const rows = fixture("rows.tsv").map((row) => row.map(inputValue));
  resetExpressionVmStats(true);
  for (const [name, context, sql] of fixture("cases.tsv")) {
    const params = new ParamTypes();
    params.note(0, "i32");
    params.note(1, "i32");
    const { node } = resolve(
      scope,
      parseExpression(sql),
      null,
      { collecting: false, groupKeys: [], specs: [] },
      params,
    );
    const filter = context === "filter";
    if (name.startsWith("fallback_")) assert.equal(compileExpressionVm(node, filter), null, name);
    else assert.notEqual(compileExpressionVm(node, filter), null, name);
    if (process.env.JED_EXPR_VM_COMPILE === "1" && !name.startsWith("fallback_")) {
      const iterations = 10_000;
      let program = compileExpressionVm(node, filter)!;
      const start = process.hrtime.bigint();
      for (let i = 0; i < iterations; i++) program = compileExpressionVm(node, filter)!;
      const elapsed = process.hrtime.bigint() - start;
      console.log(
        JSON.stringify({
          core: "ts",
          mode: "compile",
          case: name,
          iterations,
          elapsed_ns: elapsed.toString(),
          instructions: program.instructions.length,
          slots: program.slots,
        }),
      );
    }
    for (const row of rows) {
      const full = evaluate(node, row, env, filter, false, 0n);
      for (let limit = 0n; limit <= full.cost + 2n; limit++) {
        for (const lifetime of [false, true]) {
          assert.deepEqual(
            evaluate(node, row, env, filter, true, limit, lifetime),
            evaluate(node, row, env, filter, false, limit, lifetime),
            `${name}, row ${render(row[0])}, limit ${limit}, lifetime ${lifetime}`,
          );
        }
      }
    }
  }
  assert.ok(expressionVmStats.compiled > 20);
  assert.ok(expressionVmStats.fallbacks >= 2);
});

test("VM programs follow prepared plan invalidation and never retain execution parameters", () => {
  const db = createDatabase();
  const session = db.session();
  session.execute("CREATE TABLE t(id i32 PRIMARY KEY, a i32, b i32, p boolean, q boolean)");
  session.execute("INSERT INTO t VALUES (1, 5, 2, true, false)");
  const statement = session.prepareStatement("SELECT (a + $1) * b FROM t");
  withExpressionVm(true, () => {
    resetExpressionVmStats(true);
    const first = drainOutcome(session.queryPrepared(statement, [intValue(3n)]));
    assert.equal(first.kind, "query");
    if (first.kind === "query") assert.equal(render(first.rows[0][0]), "16");
    const compiled = expressionVmStats.compiled;
    const second = drainOutcome(session.queryPrepared(statement, [intValue(7n)]));
    if (second.kind === "query") assert.equal(render(second.rows[0][0]), "24");
    assert.equal(expressionVmStats.compiled, compiled);
    session.execute("DROP TABLE t");
    session.execute("CREATE TABLE t(id i32 PRIMARY KEY, b i32, a i32, p boolean, q boolean)");
    session.execute("INSERT INTO t VALUES (1, 3, 9, true, false)");
    const changed = drainOutcome(session.queryPrepared(statement, [intValue(1n)]));
    if (changed.kind === "query") assert.equal(render(changed.rows[0][0]), "30");
    assert.ok(expressionVmStats.compiled > compiled);
  });
  session.close();
  db.close();
});
