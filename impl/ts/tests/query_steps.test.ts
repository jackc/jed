// Host-scheduling invariants beyond corpus reach: the synchronous and asynchronous drivers
// execute the same SQL algorithms, preserving rows, types, costs, pins, and exactly-once effects.
import assert from "node:assert/strict";
import { mkdtempSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { createDatabase, openDatabase } from "../src/file.ts";
import { FileBlockStore } from "../src/fileblockstore.ts";
import { Environment } from "../src/environment.ts";
import { EngineError } from "../src/errors.ts";
import { runAsync, type AsyncExecutionOptions, type Steps } from "../src/execution.ts";
import type { Engine } from "../src/executor.ts";
import { FileSpillSink } from "../src/spillfile.ts";
import type { Rows } from "../src/api.ts";
import type { Database, Session } from "../src/shared.ts";
import { intValue, textValue, type Value } from "../src/value.ts";
import { fillerText } from "./util.ts";

type Result = { names: string[]; types: string[]; values: Value[][]; cost: bigint };

function collectSync(session: Session, sql: string, params: Value[] = []): Result {
  const rows = session.queryPrepared(session.prepareStatement(sql), params);
  try {
    return { names: rows.columnNames, types: rows.columnTypes, values: [...rows], cost: rows.cost };
  } finally {
    rows.close();
  }
}

function* collectSteps(session: Session, sql: string, params: Value[] = []): Steps<Result> {
  let rows: Rows | undefined;
  try {
    rows = yield* session.queryPreparedSteps(session.prepareStatement(sql), params);
    const values: Value[][] = [];
    for (;;) {
      const next = yield* rows.nextSteps();
      if (next.done) break;
      values.push(next.value);
    }
    return { names: rows.columnNames, types: rows.columnTypes, values, cost: rows.cost };
  } finally {
    rows?.close();
  }
}

// Fail closed if an effectful query silently enters a synchronous file-read wrapper. Setup,
// synchronous comparison queries, and closing the database deliberately remain outside this scope.
async function asyncFileReadsOnly<T>(run: () => Promise<T>): Promise<{ value: T; reads: number }> {
  const read = FileBlockStore.prototype.readAt;
  const readAsync = FileBlockStore.prototype.readAtAsync;
  let reads = 0;
  FileBlockStore.prototype.readAt = () => {
    throw new Error("effectful query used a synchronous file read");
  };
  FileBlockStore.prototype.readAtAsync = function (offset, length) {
    reads++;
    return readAsync.call(this, offset, length);
  };
  try {
    return { value: await run(), reads };
  } finally {
    FileBlockStore.prototype.readAt = read;
    FileBlockStore.prototype.readAtAsync = readAsync;
  }
}

const overflowText = fillerText(1400);
function seed(db: Database): void {
  const session = db.writeSession();
  try {
    session.execute("CREATE TABLE t (id i32 PRIMARY KEY, k i32, v i32, txt text)");
    session.execute("CREATE INDEX t_k_idx ON t (k)");
    session.execute(
      "INSERT INTO t SELECT g, g % 7, g * 3, 'row-' || g::text FROM generate_series(1,80) AS g",
    );
    session.execute("CREATE TABLE keysets (id i32 PRIMARY KEY, v i32, txt text)");
    session.execute("CREATE INDEX keysets_v_idx ON keysets (v)");
    session.execute("INSERT INTO keysets SELECT id, v, txt FROM t");
    session.execute("CREATE TABLE b (id i32 PRIMARY KEY, k i32, v i32)");
    session.execute("INSERT INTO b SELECT g, g % 7, g * 5 FROM generate_series(1,20) AS g");
    session.execute("CREATE TABLE big (id i32 PRIMARY KEY, txt text)");
    for (let i = 1; i <= 8; i++)
      session.execute("INSERT INTO big VALUES ($1, $2)", [
        intValue(BigInt(i)),
        textValue(overflowText + i),
      ]);
    session.execute("CREATE TABLE gin_source (id i32 PRIMARY KEY, tags i32[])");
    session.execute("CREATE INDEX tags_gin ON gin_source USING gin (tags)");
    session.execute(
      "INSERT INTO gin_source SELECT g, ARRAY[0, g % 7, g % 20 + 100]::i32[] FROM generate_series(1,200) AS g",
    );
    session.commit();
  } finally {
    session.close();
  }
}

const queries: Array<{ label: string; sql: string; params?: Value[] }> = [
  { label: "filtered full scan", sql: "SELECT id, v + 1 FROM t WHERE v % 2 = 0 ORDER BY id" },
  {
    label: "bounded primary key",
    sql: "SELECT id, txt FROM t WHERE id >= 25 AND id < 41 ORDER BY id",
  },
  { label: "secondary index", sql: "SELECT id, v FROM t WHERE k = 3 ORDER BY id" },
  {
    label: "secondary index key set",
    sql: "SELECT id, txt FROM keysets WHERE v IN (3, 30, 90) ORDER BY id",
  },
  {
    label: "ordered three-way join",
    sql: "SELECT a.id, b.id, c.id FROM t a JOIN b ON a.id + 0 > 0 CROSS JOIN big c ORDER BY c.id LIMIT 2",
  },
  {
    label: "JSON_TABLE passing subquery",
    sql: "SELECT n FROM JSON_TABLE(jsonb '[1, 2, 3]', '$[*] ? (@ > $bound)' PASSING (SELECT min(id) FROM big) AS bound COLUMNS (n i32 PATH '$')) AS j ORDER BY n",
  },
  { label: "grouped aggregates", sql: "SELECT k, sum(v), count(*) FROM t GROUP BY k ORDER BY k" },
  { label: "distinct", sql: "SELECT DISTINCT k FROM t ORDER BY k" },
  { label: "sorted limit", sql: "SELECT id, txt FROM t ORDER BY v DESC LIMIT 11 OFFSET 3" },
  {
    label: "window arguments",
    sql: "SELECT id, sum(v + 1) OVER (PARTITION BY k ORDER BY id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
  },
  {
    label: "window filter",
    sql: "SELECT id, sum(v) FILTER (WHERE v > 50) OVER (ORDER BY id) FROM t ORDER BY id",
  },
  {
    label: "equijoin",
    sql: "SELECT a.id, b.id, b.v FROM t a JOIN b ON a.k = b.k WHERE a.id <= 8 ORDER BY a.id, b.id",
  },
  {
    label: "nested cross join",
    sql: "SELECT a.id, b.id FROM t a CROSS JOIN b WHERE a.id <= 3 AND b.id <= 4 ORDER BY a.id, b.id",
  },
  {
    label: "CTE",
    sql: "WITH x AS (SELECT k, v FROM t WHERE id < 20) SELECT k, sum(v) FROM x GROUP BY k ORDER BY k",
  },
  {
    label: "recursive CTE",
    sql: "WITH RECURSIVE x(n) AS (VALUES (1) UNION ALL SELECT n + 1 FROM x WHERE n < 80) SELECT sum(n) FROM x",
  },
  { label: "generated rows", sql: "SELECT sum(g::i64) FROM generate_series(1,100) AS g" },
  {
    label: "correlated aggregate",
    sql: "SELECT id, (SELECT sum(b.v) FROM b WHERE b.k = t.k) FROM t WHERE id < 9 ORDER BY id",
  },
  {
    label: "short-circuit subquery",
    sql: "SELECT id, CASE WHEN id < 4 THEN txt ELSE (SELECT txt FROM big WHERE big.id = 1) END FROM t WHERE id <= 5 ORDER BY id",
  },
  {
    label: "folded membership",
    sql: "SELECT id FROM big WHERE id IN (SELECT id FROM b WHERE id <= 3) ORDER BY id",
  },
  { label: "overflow projection", sql: "SELECT txt FROM big ORDER BY id" },
  { label: "overflow aggregate", sql: "SELECT count(txt), min(txt), max(txt) FROM big" },
  {
    label: "overflow columnar predicate",
    sql: "SELECT count(*) FROM big WHERE txt = $1",
    params: [textValue(overflowText + "3")],
  },
  {
    label: "subquery in window argument",
    sql: "SELECT id, lag((SELECT txt FROM big WHERE big.id = t.id)) OVER (ORDER BY id) FROM t WHERE id <= 8 ORDER BY id",
  },
  {
    label: "subquery in ordering",
    sql: "SELECT id FROM t WHERE id <= 8 ORDER BY (SELECT txt FROM big WHERE big.id = t.id) DESC",
  },
  {
    label: "aggregate FILTER and DISTINCT subqueries",
    sql: "SELECT count(DISTINCT (SELECT txt FROM big WHERE big.id = t.id)) FILTER (WHERE (SELECT v FROM b WHERE b.id = t.id) > 10) FROM t WHERE id <= 8",
  },
  {
    label: "joined aggregate ORDER BY subqueries",
    sql: "SELECT a.k, array_agg((SELECT txt FROM big WHERE big.id = a.id) ORDER BY (SELECT v FROM b WHERE b.id = a.id) DESC) FROM t a JOIN b ON a.id = b.id WHERE a.id <= 8 GROUP BY a.k ORDER BY a.k",
  },
  {
    label: "grouped HAVING subquery",
    sql: "SELECT k, sum(v) FROM t GROUP BY k HAVING k < (SELECT max(id) FROM big) ORDER BY k",
  },
  {
    label: "GIN intersection",
    sql: "SELECT id FROM gin_source WHERE tags @> ARRAY[0, 105] ORDER BY id",
  },
  {
    label: "GIN dense intersection",
    sql: "SELECT id FROM gin_source WHERE tags @> ARRAY[0, 3] ORDER BY id",
  },
  {
    label: "GIN disjunction",
    sql: "SELECT id FROM gin_source WHERE tags @> ARRAY[105] OR tags && ARRAY[104, 108] ORDER BY id",
  },
  {
    label: "EXPLAIN ANALYZE",
    sql: "EXPLAIN (ANALYZE, COSTS false) SELECT txt FROM big WHERE id <= 4 ORDER BY txt",
  },
];

for (const mode of ["memory", "file"] as const) {
  test(`query steps preserve values, types and cost across ${mode} execution paths`, async (t) => {
    const dir = mode === "file" ? mkdtempSync(join(tmpdir(), "jed-query-steps-")) : undefined;
    const path = dir === undefined ? undefined : join(dir, "data.jed");
    let db = createDatabase({ path, pageSize: 256, skipFsync: true });
    try {
      seed(db);
      if (path !== undefined) {
        db.close();
        db = openDatabase(path, { cacheBytes: 256, skipFsync: true });
      }
      const session = db.session();
      let yields = 0;
      let asyncReads = 0;
      const options: AsyncExecutionOptions = {
        quantum: 4,
        schedule: (resume) =>
          setImmediate(() => {
            yields++;
            resume();
          }),
      };
      try {
        for (const probe of queries) {
          await t.test(probe.label, async () => {
            if (probe.label.startsWith("GIN ")) {
              const plan = collectSync(session, "EXPLAIN (COSTS false) " + probe.sql);
              assert.ok(
                plan.values.flat().some((v) => v.kind === "text" && v.text.includes("GIN bound")),
                "fixture must select GIN traversal",
              );
            }
            const expected = collectSync(session, probe.sql, probe.params);
            const run = () => runAsync(collectSteps(session, probe.sql, probe.params), options);
            const captured =
              mode === "file" ? await asyncFileReadsOnly(run) : { value: await run(), reads: 0 };
            asyncReads += captured.reads;
            assert.deepEqual(captured.value, expected, probe.sql);
          });
        }
        assert.ok(yields > 0, "resident work must have real task-queue suspension points");
        if (mode === "file")
          assert.ok(asyncReads > 0, "file queries must exercise asynchronous page reads");
      } finally {
        session.close();
      }
    } finally {
      db.close();
      if (dir !== undefined) rmSync(dir, { recursive: true, force: true });
    }
  });
}

function hasState(state: string): (error: unknown) => boolean {
  return (error) => error instanceof EngineError && error.code() === state;
}

test("message-only cancellation interrupts generated work and releases its cursor pin", async () => {
  const db = createDatabase();
  const session = db.session();
  try {
    session.begin(true);
    let canceled = false;
    let yields = 0;
    await assert.rejects(
      runAsync(collectSteps(session, "SELECT sum(g::i64) FROM generate_series(1,1000000) AS g"), {
        quantum: 8,
        check: () => canceled,
        schedule(resume) {
          setImmediate(() => {
            if (++yields === 2) canceled = true;
            resume();
          });
        },
      }),
      hasState("57014"),
    );
    assert.equal(yields, 2);
    assert.throws(() => session.execute("SELECT 1"), hasState("25P02"));
    session.rollback();
    session.execute("CREATE TABLE after_cancel (v i32)");
    assert.equal(
      db.oldestLiveTxid(),
      db.version,
      "canceled cursor must release its old snapshot pin",
    );
    assert.deepEqual(collectSync(session, "SELECT 7").values, [[intValue(7n)]]);
  } finally {
    session.close();
    db.close();
  }
});

test("message cancellation reaches resident operators before their first output row", async (t) => {
  let visits = 0;
  const builder = Environment.builder();
  builder.registerFunction({
    name: "visit_progress",
    argTypes: ["i64"],
    result: "i64",
    volatility: "volatile",
    kernel(args) {
      visits++;
      return args[0]!;
    },
  });
  const db = createDatabase({ environment: builder.build() });
  const session = db.session();
  const cases = [
    {
      name: "aggregate over generated input",
      sql: "SELECT sum(visit_progress(g::i64)) FROM generate_series(1,1000000) AS g",
    },
    {
      name: "join rejecting all candidate pairs",
      sql: "SELECT count(*) FROM generate_series(1,100) AS a CROSS JOIN generate_series(1,100) AS b WHERE visit_progress(a::i64+b::i64)<0",
    },
    {
      name: "recursive input to a blocking aggregate",
      sql: "WITH RECURSIVE r(n) AS (SELECT visit_progress(1::i64) UNION ALL SELECT visit_progress(n+1) FROM r WHERE n<1000000) SELECT sum(n) FROM r",
    },
  ];
  try {
    for (const probe of cases) {
      await t.test(probe.name, async () => {
        visits = 0;
        let canceled = false;
        session.begin(true);
        await assert.rejects(
          runAsync(collectSteps(session, probe.sql), {
            quantum: 32,
            check: () => canceled,
            schedule(resume) {
              setImmediate(() => {
                // Arm only after expression execution starts. In particular, the join must
                // reach its candidate loop, rather than cancel while materializing its inputs.
                if (visits >= 32) canceled = true;
                resume();
              });
            },
          }),
          hasState("57014"),
        );
        assert.ok(visits >= 32 && visits < 10_000);
        assert.throws(() => session.execute("SELECT 1"), hasState("25P02"));
        session.rollback();
        assert.deepEqual(collectSync(session, "SELECT 7").values, [[intValue(7n)]]);
      });
    }
  } finally {
    session.close();
    db.close();
  }
});

test("async blocking operators spill with identical results and release scratch resources", async (t) => {
  const dir = mkdtempSync(join(tmpdir(), "jed-query-spill-steps-"));
  const path = join(dir, "data.jed");
  let db = createDatabase({ path, pageSize: 256, skipFsync: true });
  try {
    seed(db);
    db.close();
    db = openDatabase(path, { cacheBytes: 256, skipFsync: true });
    const sink = new FileSpillSink(dir);
    const createScratch = sink.createScratch.bind(sink);
    const writeRun = sink.writeRun.bind(sink);
    let created = 0;
    let live = 0;
    sink.createScratch = () => {
      const scratch = createScratch();
      const close = scratch.close.bind(scratch);
      let closed = false;
      created++;
      live++;
      scratch.close = () => {
        if (!closed) {
          closed = true;
          live--;
          close();
        }
      };
      return scratch;
    };
    sink.writeRun = (bytes) => {
      created++;
      return writeRun(bytes);
    };
    (db as unknown as { core: { storage: Engine } }).core.storage.spillSink = sink;
    const session = db.session();
    try {
      const probes = [
        "SELECT id, txt FROM t ORDER BY v DESC, id",
        "SELECT a.id, b.id FROM t a JOIN b ON a.v = b.v ORDER BY a.id, b.id",
        "SELECT DISTINCT k, txt FROM t ORDER BY k, txt",
        "SELECT k, count(DISTINCT txt), sum(v) FROM t GROUP BY k ORDER BY k",
        "SELECT array_agg((SELECT txt FROM big WHERE big.id = t.id) ORDER BY (SELECT v FROM b WHERE b.id = t.id) DESC) FILTER (WHERE (SELECT v FROM b WHERE b.id = t.id) > 0) FROM t WHERE id <= 8",
      ];
      for (const sql of probes)
        await t.test(sql, async () => {
          if (sql.includes(" JOIN ")) {
            const plan = collectSync(session, "EXPLAIN (COSTS false) " + sql);
            assert.ok(
              plan.values.flat().some((v) => v.kind === "text" && v.text === "Hash Join"),
              "fixture must select a hash join",
            );
          }
          session.setWorkMem(0);
          const expected = collectSync(session, sql);
          session.setWorkMem(128);
          const before = created;
          const captured = await asyncFileReadsOnly(() =>
            runAsync(collectSteps(session, sql), { quantum: 4, schedule: setImmediate }),
          );
          assert.deepEqual(captured.value, expected);
          assert.ok(created > before, "operator must exercise scratch storage");
          assert.equal(live, 0, "completed continuation must close every scratch handle");
          assert.deepEqual(
            readdirSync(dir).filter((name) => name.startsWith("jed-spill-")),
            [],
          );
        });
      await t.test("cancellation unwinds active spill resources", async () => {
        session.begin(true);
        const before = created;
        let canceled = false;
        await assert.rejects(
          asyncFileReadsOnly(() =>
            runAsync(collectSteps(session, "SELECT DISTINCT id, txt FROM t ORDER BY txt, id"), {
              quantum: 1,
              check: () => canceled,
              schedule(resume) {
                setImmediate(() => {
                  canceled = created > before;
                  resume();
                });
              },
            }),
          ),
          hasState("57014"),
        );
        assert.ok(created > before);
        assert.equal(live, 0, "throwing into a suspended spill must close its scratch handles");
        assert.deepEqual(
          readdirSync(dir).filter((name) => name.startsWith("jed-spill-")),
          [],
        );
        session.rollback();
      });
    } finally {
      session.close();
    }
  } finally {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("async page suspension invokes each projected host function exactly once", async () => {
  const calls: bigint[] = [];
  const builder = Environment.builder();
  builder.registerFunction({
    name: "visit_once",
    argTypes: ["i64"],
    result: "i64",
    volatility: "volatile",
    kernel(args) {
      const arg = args[0]!;
      assert.equal(arg.kind, "int");
      if (arg.kind !== "int") throw new Error("integer argument required");
      calls.push(arg.int);
      return arg;
    },
  });
  const environment = builder.build();
  const dir = mkdtempSync(join(tmpdir(), "jed-query-effect-"));
  const path = join(dir, "data.jed");
  let db = createDatabase({ path, pageSize: 256, skipFsync: true, environment });
  try {
    seed(db);
    db.close();
    db = openDatabase(path, { cacheBytes: 256, skipFsync: true, environment });
    const session = db.session();
    try {
      const result = await asyncFileReadsOnly(() =>
        runAsync(collectSteps(session, "SELECT visit_once(id::i64), txt FROM big ORDER BY id"), {
          quantum: 1,
          schedule: setImmediate,
        }),
      );
      assert.ok(result.reads > 0);
      assert.equal(result.value.values.length, 8);
      assert.deepEqual(calls, [1n, 2n, 3n, 4n, 5n, 6n, 7n, 8n]);
    } finally {
      session.close();
    }
  } finally {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  }
});
