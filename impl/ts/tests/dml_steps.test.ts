// Host suspension cannot be expressed by the SQL corpus: reject synchronous reads while
// exercising the same DML against a paged file, including implicit index/FK maintenance.
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { createDatabase, openDatabase } from "../src/file.ts";
import { FileBlockStore } from "../src/fileblockstore.ts";
import { EngineError } from "../src/errors.ts";
import { runAsync, runSync, type Steps } from "../src/execution.ts";
import type { Session } from "../src/shared.ts";
import { intValue, textValue, type Value } from "../src/value.ts";
import { fillerText } from "./util.ts";

type Collected = {
  names: string[];
  types: string[];
  values: Value[][];
  cost: bigint;
  changes: number | null;
};

function* collect(session: Session, sql: string, params: Value[] = []): Steps<Collected> {
  const rows = yield* session.queryPreparedSteps(session.prepareStatement(sql), params);
  try {
    const values: Value[][] = [];
    for (;;) {
      const next = yield* rows.nextSteps();
      if (next.done) break;
      values.push(next.value);
    }
    return {
      names: rows.columnNames,
      types: rows.columnTypes,
      values,
      cost: rows.cost,
      changes: rows.rowsAffected,
    };
  } finally {
    rows.close();
  }
}

async function asyncReadsOnly<T>(steps: Steps<T>): Promise<{ value: T; reads: number }> {
  const sync = FileBlockStore.prototype.readAt;
  const async = FileBlockStore.prototype.readAtAsync;
  let reads = 0;
  FileBlockStore.prototype.readAt = () => {
    throw new Error("DML used a synchronous page read");
  };
  FileBlockStore.prototype.readAtAsync = function (offset, length) {
    reads++;
    return async.call(this, offset, length);
  };
  try {
    return { value: await runAsync(steps, { quantum: 16 }), reads };
  } finally {
    FileBlockStore.prototype.readAt = sync;
    FileBlockStore.prototype.readAtAsync = async;
  }
}

const payload = fillerText(1400);
const statements: { label: string; sql: string; params?: Value[] }[] = [
  {
    label: "insert with FK and unique probes",
    sql: "INSERT INTO c VALUES (201, 4, 201, 'new') RETURNING id, p",
  },
  {
    label: "insert select and returning subquery",
    sql: "INSERT INTO c SELECT id + 200, id, id + 200, 'new' FROM p WHERE id <= 4 RETURNING id, (SELECT max(id) FROM p)",
  },
  {
    label: "bounded update",
    sql: "UPDATE c SET u = u + 1000 WHERE id >= 20 AND id < 31 RETURNING id, u",
  },
  {
    label: "update expression and partial indexes",
    sql: "UPDATE c SET txt = txt || 'changed' WHERE id <= 3 RETURNING id, length(txt)",
  },
  {
    label: "upsert existing overflow row",
    sql: "INSERT INTO c VALUES (1, 1, 1, 'new') ON CONFLICT (id) DO UPDATE SET txt = c.txt || excluded.txt RETURNING id, length(txt)",
  },
  {
    label: "upsert unique arbiter",
    sql: "INSERT INTO c VALUES (201, 2, 2, 'new') ON CONFLICT (u) DO UPDATE SET txt = excluded.txt RETURNING id, txt",
  },
  {
    label: "upsert do nothing",
    sql: "INSERT INTO c VALUES (201, 2, 2, 'new') ON CONFLICT DO NOTHING RETURNING id",
  },
  {
    label: "delete secondary index",
    sql: "DELETE FROM c WHERE u >= 20 AND u < 30 RETURNING id, length(txt)",
  },
  { label: "cascade delete", sql: "DELETE FROM p WHERE id <= 4 RETURNING id" },
  { label: "cascade update", sql: "UPDATE p SET id = id + 1000 WHERE id <= 4 RETURNING id" },
  { label: "GIN insert", sql: "INSERT INTO tags VALUES (201, ARRAY[0, 7]::i32[])" },
  {
    label: "GIN remove and insert",
    sql: "UPDATE tags SET tags = ARRAY[5, 12]::i32[] WHERE id < 12 RETURNING id",
  },
  {
    label: "GIN indexed delete",
    sql: "DELETE FROM tags WHERE tags @> ARRAY[7]::i32[] RETURNING id",
  },
  {
    label: "GiST implicit rebuild",
    sql: "INSERT INTO reservations VALUES (201, '[2010,2015)') RETURNING id",
  },
  { label: "GiST delete", sql: "DELETE FROM reservations WHERE id < 4 RETURNING id" },
  {
    label: "writable CTE",
    sql: "WITH d AS (DELETE FROM c WHERE id < 4 RETURNING id) INSERT INTO p SELECT id + 200 FROM d RETURNING id",
  },
  {
    label: "custom parameter plan",
    sql: "UPDATE c SET txt = $1 WHERE u > $2 RETURNING id",
    params: [textValue("bound"), intValue(97n)],
  },
  {
    label: "secondary index key set",
    sql: "UPDATE c SET txt = 'set' WHERE u IN (1, 7, 11) RETURNING id",
  },
  { label: "primary key set", sql: "DELETE FROM c WHERE id IN (1, 7, 11) RETURNING id" },
  {
    label: "three-way insert source",
    sql: "INSERT INTO p SELECT a.id + 200 FROM p a JOIN p b ON a.id = b.id JOIN p d ON b.id = d.id WHERE a.id < 4 RETURNING id",
  },
];

test("DML steps preserve values and cost with asynchronous page reads only", async (t) => {
  const dir = mkdtempSync(join(tmpdir(), "jed-dml-steps-"));
  const path = join(dir, "data.jed");
  let db = createDatabase({ path, pageSize: 256, skipFsync: true });
  try {
    const seed = db.writeSession();
    try {
      seed.execute("CREATE TABLE p (id i32 PRIMARY KEY)");
      seed.execute("INSERT INTO p SELECT g FROM generate_series(1,100) AS g");
      seed.execute(
        "CREATE TABLE c (id i32 PRIMARY KEY, p i32 REFERENCES p ON UPDATE CASCADE ON DELETE CASCADE, u i32 UNIQUE, txt text, CHECK (length(txt) > 0))",
      );
      seed.execute("CREATE INDEX c_expr ON c ((length(txt))) WHERE id > 1");
      const insert = seed.prepareStatement("INSERT INTO c VALUES ($1, $1, $1, $2)");
      for (let i = 1; i <= 100; i++)
        seed.executePrepared(insert, [
          intValue(BigInt(i)),
          textValue(i <= 5 ? payload + i : "row-" + i),
        ]);
      seed.execute("CREATE TABLE tags (id i32 PRIMARY KEY, tags i32[])");
      seed.execute("CREATE INDEX tags_gin ON tags USING gin (tags)");
      seed.execute(
        "INSERT INTO tags SELECT g, ARRAY[0, g % 10]::i32[] FROM generate_series(1,100) AS g",
      );
      seed.execute(
        "CREATE TABLE reservations (id i32 PRIMARY KEY, during i32range, EXCLUDE USING gist (during WITH &&))",
      );
      for (let i = 1; i <= 20; i++)
        seed.execute("INSERT INTO reservations VALUES ($1, $2::i32range)", [
          intValue(BigInt(i)),
          textValue(`[${i * 10},${i * 10 + 5})`),
        ]);
      seed.commit();
    } finally {
      seed.close();
    }
    db.close();
    db = openDatabase(path, { cacheBytes: 256, skipFsync: true });
    let reads = 0;
    for (const probe of statements)
      await t.test(probe.label, async () => {
        const sync = db.writeSession();
        let expected: Collected;
        try {
          expected = runSync(collect(sync, probe.sql, probe.params));
        } finally {
          sync.rollback();
          sync.close();
        }
        const suspended = db.writeSession();
        try {
          const actual = await asyncReadsOnly(collect(suspended, probe.sql, probe.params));
          reads += actual.reads;
          assert.deepEqual(actual.value, expected);
        } finally {
          suspended.rollback();
          suspended.close();
        }
      });
    for (const probe of [
      { sql: "INSERT INTO c VALUES (201, 999, 201, 'new')", state: "23503" },
      { sql: "INSERT INTO c VALUES (201, 1, 1, 'new')", state: "23505" },
      {
        sql: "INSERT INTO c VALUES (1, 1, 1, 'new') ON CONFLICT (id) DO UPDATE SET u = 2",
        state: "23505",
      },
      { sql: "UPDATE c SET txt = '' WHERE id = 1", state: "23514" },
      { sql: "INSERT INTO reservations VALUES (201, '[10,12)')", state: "23P01" },
    ])
      await t.test(`constraint ${probe.state}`, async () => {
        const session = db.writeSession();
        try {
          await assert.rejects(
            asyncReadsOnly(collect(session, probe.sql)),
            (e: unknown) =>
              e instanceof EngineError && e.code() === probe.state && e.detail !== undefined,
          );
        } finally {
          session.rollback();
          session.close();
        }
      });
    assert.ok(reads > 0, "DML must fault pages through the async provider");
  } finally {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("cancellation after partial DML writes rolls back the implicit statement and releases the writer", async () => {
  const db = createDatabase();
  const session = db.session();
  try {
    session.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
    session.execute("INSERT INTO t SELECT g, g FROM generate_series(1,400) AS g");
    const before = runSync(collect(session, "SELECT id, v FROM t ORDER BY id"));
    // Watch the actual mutation boundary, then cancel on the next scheduled task. This tests
    // rollback after effects have happened rather than merely cancellation during the scan.
    const { TableStore } = await import("../src/storage.ts");
    const remove = TableStore.prototype.removeSteps;
    let changed = 0;
    let canceled = false;
    TableStore.prototype.removeSteps = function* (key) {
      const result = yield* remove.call(this, key);
      if (result) changed++;
      return result;
    };
    try {
      await assert.rejects(
        runAsync(collect(session, "DELETE FROM t"), {
          quantum: 4,
          timeSliceMs: 0,
          check: () => canceled,
          schedule(resume) {
            setImmediate(() => {
              if (changed > 0) canceled = true;
              resume();
            });
          },
        }),
        (e: unknown) => e instanceof EngineError && e.code() === "57014",
      );
    } finally {
      TableStore.prototype.removeSteps = remove;
    }
    assert.ok(changed > 0 && changed < 400);
    assert.deepEqual(runSync(collect(session, "SELECT id, v FROM t ORDER BY id")), before);
    assert.equal(session.execute("INSERT INTO t VALUES (401, 401)").changes, 1);
  } finally {
    session.close();
    db.close();
  }
});
