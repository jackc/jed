// depth_stack_child — the child half of depth_limit.test.ts's native-stack budget test (spec/design/
// cost.md §7.1). Not a `*.test.ts` file, so the runner does not execute it directly: the test spawns
// `node --stack-size=<budget> tests/depth_stack_child.ts [shape…]`, which runs every nesting vector
// (or the named ones) at the deepest depth the parser accepts, and just past it, on a fresh session,
// and prints one JSON line per shape: { name, depth, deepest, past }. `deepest` is "ok" when the
// accepted form runs end to end, `past` is "54001" when the over-limit form is rejected, and either
// is "overflow" when V8 throws a RangeError (the stack budget is exceeded).

import { EngineError, type Session } from "../src/tooling.ts";
import { MAX_EXPR_DEPTH, parseSQL } from "../src/parser.ts";
import { memDb } from "./mem_db.ts";

// One recursion vector: its nesting shape(n) and any statements it needs first.
interface Shape {
  name: string;
  shape: (n: number) => string;
  setup?: (n: number) => string[];
}

function nest(open: string, inner: string, close: string, n: number): string {
  return open.repeat(n) + inner + close.repeat(n);
}

function s(name: string, shape: (n: number) => string): Shape {
  return { name, shape };
}

// The same vectors as impl/rust/tests/depth_limit.rs's shapes().
export const SHAPES: Shape[] = [
  s("parens", (n) => `SELECT ${nest("(", "1", ")", n)} FROM t`),
  s("ARRAY", (n) => `SELECT ${nest("ARRAY[", "1", "]", n)} FROM t`),
  s("ROW", (n) => `SELECT ${nest("ROW(", "1", ")", n)} FROM t`),
  s("NOT", (n) => `SELECT ${"NOT ".repeat(n)}true FROM t`),
  s("unary minus", (n) => `SELECT ${"- ".repeat(n)}1 FROM t`),
  s(
    "operator chain",
    (n) =>
      `SELECT ${Array(n + 1)
        .fill("v")
        .join(" + ")} FROM t`,
  ),
  // The two-pass operand resolvers (cost.md §7): the `-` jsonb peek, `||`, the polymorphic array
  // functions, containment, and `= ANY(ARRAY[…])`.
  s(
    "minus chain",
    (n) =>
      `SELECT ${Array(n + 1)
        .fill("v")
        .join(" - ")} FROM t`,
  ),
  s(
    "array concat chain",
    (n) =>
      `SELECT ${Array(n + 1)
        .fill("ARRAY[v]")
        .join(" || ")} FROM t`,
  ),
  s(
    "jsonb concat chain",
    (n) =>
      `SELECT ${Array(n + 1)
        .fill("'{}'::jsonb")
        .join(" || ")} FROM t`,
  ),
  s("array function", (n) => `SELECT ${nest("array_append(", "ARRAY[v]", ", 1)", n)} FROM t`),
  s("containment", (n) => `SELECT ${nest("ARRAY[true] @> ARRAY[", "true", "]", n)} FROM t`),
  s("= ANY", (n) => `SELECT ${nest("true = ANY(ARRAY[", "true", "])", n)} FROM t`),
  s(
    "AND chain",
    (n) =>
      `SELECT ${Array(n + 1)
        .fill("true")
        .join(" AND ")} FROM t`,
  ),
  s("comparison operand", (n) => `SELECT ${nest("(true = ", "true", ")", n)} FROM t`),
  s("cast chain", (n) => `SELECT 1${"::int4".repeat(n)} FROM t`),
  s("CAST", (n) => `SELECT ${nest("CAST(", "1", " AS i64)", n)} FROM t`),
  s("function call", (n) => `SELECT ${nest("abs(", "1", ")", n)} FROM t`),
  s("COALESCE", (n) => `SELECT ${nest("coalesce(NULL, ", "1", ")", n)} FROM t`),
  s("CASE", (n) => `SELECT ${nest("CASE WHEN true THEN ", "1", " END", n)} FROM t`),
  s("subscript index", (n) => `SELECT ${nest("ARRAY[1, 2][", "1", "]", n)} FROM t`),
  s("scalar subquery", (n) => `SELECT ${nest("(SELECT ", "1", ")", n)} FROM t`),
  s("correlated subquery", (n) => `SELECT ${nest("(SELECT ", "t.v", ")", n)} FROM t`),
  s("EXISTS", (n) => `SELECT ${nest("EXISTS (SELECT ", "true", ")", n)} FROM t`),
  s("IN subquery", (n) => `SELECT ${nest("true IN (SELECT ", "true", ")", n)} FROM t`),
  s("derived table", (n) => `SELECT * FROM ${nest("(SELECT * FROM ", "t", ") AS x", n)}`),
  s("parenthesized query", (n) => nest("(", "SELECT 1", ")", n)),
  s(
    "parenthesized join",
    (n) => `SELECT * FROM ${nest("(", "t AS a JOIN t AS b ON true", ")", n)}`,
  ),
  s("UNION chain", (n) => `SELECT 1${" UNION ALL SELECT 1".repeat(n)}`),
  s("nested WITH", (n) => nest("WITH x AS (", "SELECT 1", ") SELECT * FROM x", n)),
  s("WHERE", (n) => `SELECT v FROM t WHERE ${nest("(", "v", " + 0)", n)} = 1`),
  s("GROUP BY expression", (n) => {
    const e = nest("(", "v", " + 1)", n);
    return `SELECT ${e} FROM t GROUP BY ${e}`;
  }),
  s("ORDER BY expression", (n) => `SELECT v FROM t ORDER BY ${nest("(", "v", " + 1)", n)}`),
  s("aggregate argument", (n) => `SELECT sum(${nest("(", "v", " + 1)", n)}) FROM t`),
  s("EXPLAIN VERBOSE", (n) => `EXPLAIN (VERBOSE) SELECT ${nest("(", "v", " + 1)", n)} FROM t`),
  s("UPDATE", (n) => `UPDATE t SET v = ${nest("(", "v", " + 0)", n)}`),
  s("INSERT SELECT", (n) => `INSERT INTO t SELECT id + 10, ${nest("(", "v", " + 0)", n)} FROM t`),
  s("DELETE", (n) => `DELETE FROM t WHERE ${nest("(", "v", " + 0)", n)} = 99`),
  {
    name: "CHECK",
    shape: () => "INSERT INTO c VALUES (1)",
    setup: (n) => [`CREATE TABLE c (a i32 CHECK (${nest("(", "a", " + 0)", n)} > 0))`],
  },
  {
    name: "DEFAULT",
    shape: () => "INSERT INTO d (k) VALUES (1)",
    setup: (n) => [
      `CREATE TABLE d (k i32 PRIMARY KEY, a i32 DEFAULT ${nest("(", "1", " + 0)", n)})`,
    ],
  },
  {
    name: "view",
    shape: () => "SELECT x FROM vp",
    setup: (n) => [`CREATE VIEW vp AS SELECT ${nest("(", "v", " + 0)", n)} AS x FROM t`],
  },
];

function session(): Session {
  const db = memDb().session();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  db.execute("INSERT INTO t VALUES (1, 1)");
  return db;
}

function statements(sh: Shape, n: number): string[] {
  return [...(sh.setup?.(n) ?? []), sh.shape(n)];
}

// runAll is the SQLSTATE of running stmts in order on a fresh session, "ok", or "overflow" for a
// native-stack RangeError; it stops at the first error.
function runAll(stmts: string[]): string {
  const db = session();
  for (const sql of stmts) {
    try {
      db.execute(sql);
    } catch (e) {
      if (e instanceof RangeError) return "overflow";
      if (e instanceof EngineError) return e.code();
      return `non-engine: ${e}`;
    }
  }
  return "ok";
}

// deepestAccepted is the deepest n at which the shape's statements parse without 54001, by binary
// search (the rejection is monotone in n). Any other parse error, a RangeError included, propagates.
function deepestAccepted(sh: Shape): number {
  const rejected = (n: number) =>
    statements(sh, n).some((sql) => {
      try {
        parseSQL(sql);
        return false;
      } catch (e) {
        if (e instanceof EngineError && e.code() === "54001") return true;
        throw e;
      }
    });
  let lo = 1;
  let hi = MAX_EXPR_DEPTH * 2;
  if (rejected(lo) || !rejected(hi)) throw new Error(`${sh.name}: no 54001 boundary`);
  while (hi - lo > 1) {
    const mid = (lo + hi) >> 1;
    if (rejected(mid)) hi = mid;
    else lo = mid;
  }
  return lo;
}

// viewChain creates `v0` over t and then each `v<i+1>` over `v<i>` until the next view would be
// rejected 54001 — each view reference adds its body's depth (spec/design/views.md §4.1), so the chain
// is as deep as the nested derived tables it stands for — and then reads through all of it.
function viewChain(): { name: string; depth: number; deepest: string; past: string } {
  const db = session();
  const name = "view chain";
  try {
    db.execute("CREATE VIEW v0 AS SELECT id, v FROM t");
    let last = 0;
    let past = "";
    for (;;) {
      try {
        db.execute(`CREATE VIEW v${last + 1} AS SELECT id, v FROM v${last}`);
        last++;
      } catch (e) {
        if (!(e instanceof EngineError)) throw e;
        past = e.code();
        break;
      }
    }
    db.execute(`SELECT * FROM v${last}`);
    return { name, depth: last, deepest: "ok", past };
  } catch (e) {
    if (e instanceof RangeError) return { name, depth: 0, deepest: "overflow", past: "" };
    throw e;
  }
}

const wanted = new Set(process.argv.slice(2));
for (const sh of SHAPES) {
  if (wanted.size > 0 && !wanted.has(sh.name)) continue;
  try {
    const depth = deepestAccepted(sh);
    const deepest = runAll(statements(sh, depth));
    const past = runAll(statements(sh, MAX_EXPR_DEPTH * 2));
    console.log(JSON.stringify({ name: sh.name, depth, deepest, past }));
  } catch (e) {
    if (!(e instanceof RangeError)) throw e;
    console.log(JSON.stringify({ name: sh.name, depth: 0, deepest: "overflow", past: "" }));
  }
}
if (wanted.size === 0 || wanted.has("view chain")) console.log(JSON.stringify(viewChain()));
