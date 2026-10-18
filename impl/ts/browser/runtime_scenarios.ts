// Browser-only runtime contract probes, called by Playwright through the demo page.
import { createDatabase, openDatabase } from "../src/browser/async.ts";
import { Decimal } from "../src/decimal.ts";
import { EngineError } from "../src/errors.ts";
import { TransportError } from "../src/async/errors.ts";
import { byteaValue, decimalValue, intValue, render } from "../src/value.ts";

export async function typedScenario() {
  const db = await createDatabase({ runtime: { maxBatchRows: 1 } });
  try {
    const statement = await db.prepare(
      "SELECT $1::i64 AS huge, $2::decimal AS exact, $3::bytea AS bytes, NULL::text AS missing, ARRAY[1,2] AS items, ROW(1, 'x') AS pair, '[1,5)'::i32range AS span",
    );
    try {
      const rows = await statement.query([
        intValue(9007199254740993n),
        decimalValue(Decimal.fromDigitsScale(false, "12345678901234567890123456789", 9)),
        byteaValue(new Uint8Array([0, 127, 255])),
      ]);
      try {
        const item = await rows.next();
        if (item.done) throw new Error("missing typed row");
        return {
          kinds: item.value.map((value) => value.kind),
          values: item.value.map(render),
          decimalRestored:
            item.value[1]?.kind === "decimal" && item.value[1].dec instanceof Decimal,
          bytesRestored:
            item.value[2]?.kind === "bytea" && item.value[2].bytes instanceof Uint8Array,
          types: rows.columnTypes,
        };
      } finally {
        await rows.close();
      }
    } finally {
      await statement.close();
    }
  } finally {
    await db.close();
  }
}

export async function lifecycleScenario(name: string) {
  const db = await createDatabase({ path: name, runtime: { maxBatchRows: 1, maxCursors: 1 } });
  try {
    await db.execute("CREATE TABLE t (k i32 PRIMARY KEY)");
    const a = await db.session();
    const b = await db.session();
    const first = await a.begin();
    await first.execute("INSERT INTO t VALUES (1)");
    const waiting = b.begin();
    const visible = await db.get("SELECT count(*) AS n FROM t");
    await first.commit();
    const second = await waiting;
    await second.execute("INSERT INTO t VALUES (2), (3)");
    await second.commit();
    await a.close();
    await b.close();
    const cursor = await db.query("SELECT k FROM t ORDER BY k");
    let early = "";
    for await (const row of cursor) {
      early = render(row[0]!);
      break; // Must release the sole cursor slot and its reader pin.
    }
    const afterBreak = await db.get("SELECT count(*) AS n FROM t");
    const pending = await db.session();
    await pending.execute("BEGIN");
    await pending.execute("INSERT INTO t VALUES (4)");
    // Graceful database close must discard open transactions and release OPFS.
    await Promise.all([db.close(), db.close()]);
    const reopened = await openDatabase(name);
    try {
      const final = await reopened.get("SELECT count(*) AS n FROM t");
      return {
        beforeCommit: String(visible?.n),
        early,
        afterBreak: String(afterBreak?.n),
        reopened: String(final?.n),
      };
    } finally {
      await reopened.close();
    }
  } finally {
    await db.close();
  }
}

export async function errorScenario() {
  const db = await createDatabase({ runtime: { maxQueuedRequests: 1, maxBatchBytes: 1024 } });
  try {
    const first = db.execute("SELECT 1");
    let queue = "";
    try {
      await db.execute("SELECT 2");
    } catch (error) {
      queue = error instanceof TransportError ? error.code : String(error);
    }
    await first;
    await db.execute("CREATE TABLE t (k i32 PRIMARY KEY)");
    await db.execute("INSERT INTO t VALUES (1)");
    let sql = "";
    let table = "";
    try {
      await db.execute("INSERT INTO t VALUES (1)");
    } catch (error) {
      if (!(error instanceof EngineError)) throw error;
      sql = error.code();
      table = error.tableName ?? "";
    }
    let result = "";
    try {
      await db.all("SELECT repeat('x', 2000)");
    } catch (error) {
      result = error instanceof TransportError ? error.code : String(error);
    }
    const good = await db.get("SELECT 7 AS n");
    await db.close();
    let closed = "";
    try {
      await db.get("SELECT 1");
    } catch (error) {
      closed = error instanceof TransportError ? error.code : String(error);
    }
    return { queue, sql, table, result, recovery: String(good?.n), closed };
  } finally {
    await db.close();
  }
}

export async function failureScenario() {
  const NativeWorker = globalThis.Worker;
  let created: Worker | undefined;
  globalThis.Worker = class extends NativeWorker {
    constructor(url: string | URL, options?: WorkerOptions) {
      super(url, options);
      created = this;
    }
  };
  let db: Awaited<ReturnType<typeof createDatabase>>;
  try {
    db = await createDatabase();
  } finally {
    globalThis.Worker = NativeWorker;
  }
  try {
    const pending = db.execute("SELECT 1");
    created!.dispatchEvent(
      new ErrorEvent("error", { message: "injected worker failure", cancelable: true }),
    );
    try {
      await pending;
    } catch (error) {
      if (!(error instanceof TransportError)) throw error;
      return { code: error.code, outcome: error.outcome };
    }
    throw new Error("worker failure did not reject pending operation");
  } finally {
    await db.close();
  }
}

export async function cancellationScenario() {
  const db = await createDatabase({ runtime: { maxBatchRows: 1 } });
  try {
    await db.execute("CREATE TABLE t (k i32 PRIMARY KEY)");
    await db.execute("INSERT INTO t SELECT generate_series FROM generate_series(1, 20)");
    const signal = new AbortController();
    const rows = await db.query("SELECT k FROM t ORDER BY k", [], { signal: signal.signal });
    const first = await rows.next();
    signal.abort();
    let code = "";
    try {
      await rows.next();
    } catch (error) {
      code = error instanceof EngineError ? error.code() : String(error);
    }
    await rows.close();

    const already = new AbortController();
    already.abort();
    let admission = "";
    try {
      await db.execute("INSERT INTO t VALUES (21)", [], { signal: already.signal });
    } catch (error) {
      admission = error instanceof EngineError ? error.code() : String(error);
    }
    const count = await db.get("SELECT count(*) AS n FROM t");
    let ticks = 0;
    const timer = setInterval(() => {
      ticks++;
    }, 0);
    try {
      await db.execute("SELECT sum(generate_series) FROM generate_series(1, 200000)");
    } finally {
      clearInterval(timer);
    }
    return {
      isolated: crossOriginIsolated,
      first: !first.done,
      code,
      admission,
      count: String(count?.n),
      responsive: ticks > 0,
    };
  } finally {
    await db.close();
  }
}

export async function cooperativeCancellationScenario() {
  const db = await createDatabase();
  try {
    await db.execute("CREATE TABLE t (k i32)");
    await db.execute("INSERT INTO t SELECT g FROM generate_series(1, 200000) g");
    const session = await db.session();
    await session.execute("BEGIN READ ONLY");
    const tx = session;
    const controller = new AbortController();
    const rows = await tx.query("SELECT k FROM t WHERE k % 2 = -1", [], {
      signal: controller.signal,
    });
    const before = (await tx.status()).lifetimeCost;
    const next = rows.next();
    // This origin has no isolation headers, so the only cancellation path is the worker message.
    // The scan produces no result batches; its row checkpoints must admit that message.
    const timer = setTimeout(() => controller.abort(), 10);
    let code = "";
    try {
      await next;
    } catch (error) {
      code = error instanceof EngineError ? error.code() : String(error);
    } finally {
      clearTimeout(timer);
      await rows.close();
    }
    const after = await tx.status();
    await tx.rollback();
    const recovered = await session.get("SELECT 7 AS n");
    await session.close();
    return {
      isolated: crossOriginIsolated,
      code,
      progressed: after.lifetimeCost > before,
      status: after.status,
      recovered: String(recovered?.n),
    };
  } finally {
    await db.close();
  }
}
