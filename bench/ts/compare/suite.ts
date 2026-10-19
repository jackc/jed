// The same workload and drain run against an untouched core snapshot or its public worker API.
// Each process loads one revision and boundary; comparisons across processes happen afterward.
import type { Value } from "../../../impl/ts/src/value.ts";
import {
  type AsyncSession,
  type DrainResult,
  type HeavyConfig,
  type HeavyObservation,
  type ObserveWork,
  type Summary,
  type SyncSession,
  Fingerprint,
  heavyCases,
  heavyConfigFrom,
  observeTimers,
  runDirectQuery,
  runWorkerQuery,
  summarize,
} from "../src/async-heavy.ts";

export type Mode = "direct" | "worker";
type Command = { changes: number; cost: bigint };
type DirectPrepared = {
  execute(): Command;
  query(): DrainResult;
  close(): void;
};
type WorkerPrepared = {
  execute(): Promise<Command>;
  query(): Promise<DrainResult>;
  close(): Promise<void>;
};
export type ComparisonDriver =
  | {
      mode: "direct";
      execute(sql: string): unknown;
      query(sql: string): DrainResult;
      prepare(sql: string, params: Value[]): DirectPrepared;
    }
  | {
      mode: "worker";
      execute(sql: string): Promise<unknown>;
      query(sql: string): Promise<DrainResult>;
      prepare(sql: string, params: Value[]): Promise<WorkerPrepared>;
    };

export function directDriver<Prepared>(
  session: SyncSession & {
    prepareStatement(sql: string): Prepared;
    executePrepared(statement: Prepared, params: Value[]): Command;
    queryPrepared(statement: Prepared, params: Value[]): ReturnType<SyncSession["query"]>;
  },
): ComparisonDriver {
  return {
    mode: "direct",
    execute: (sql) => session.execute(sql),
    query: (sql) => runDirectQuery(session, sql),
    prepare(sql, params) {
      const statement = session.prepareStatement(sql);
      return {
        execute: () => session.executePrepared(statement, params),
        query: () =>
          runDirectQuery(
            { query: () => session.queryPrepared(statement, params), execute: () => undefined },
            sql,
          ),
        close() {},
      };
    },
  };
}

export function workerDriver(
  session: AsyncSession & {
    prepare(sql: string): Promise<{
      execute(params: Value[]): Promise<Command>;
      query(params: Value[]): ReturnType<AsyncSession["query"]>;
      close(): Promise<void>;
    }>;
  },
): ComparisonDriver {
  return {
    mode: "worker",
    execute: (sql) => session.execute(sql),
    query: (sql) => runWorkerQuery(session, sql),
    async prepare(sql, params) {
      const statement = await session.prepare(sql);
      return {
        execute: () => statement.execute(params),
        query: () =>
          runWorkerQuery(
            {
              query: () => statement.query(params),
              execute: async () => undefined,
            },
            sql,
          ),
        close: () => statement.close(),
      };
    },
  };
}

export type ComparisonConfig = {
  heavy: HeavyConfig;
  cheap: { warmups: number; samples: number; operations: number };
};

export function comparisonConfigFrom(
  settings: Record<string, string | undefined>,
): ComparisonConfig {
  const integer = (name: string, fallback: number, minimum: number) => {
    const value = Number(settings[name] ?? fallback);
    if (!Number.isSafeInteger(value) || value < minimum)
      throw new Error(`${name} must be an integer >= ${minimum}`);
    return value;
  };
  return {
    heavy: heavyConfigFrom(settings),
    cheap: {
      warmups: integer("JED_COMPARE_CHEAP_WARMUPS", 3, 0),
      samples: integer("JED_COMPARE_CHEAP_SAMPLES", 21, 1),
      operations: integer("JED_COMPARE_CHEAP_OPERATIONS", 100, 1),
    },
  };
}

function serializable(value: unknown): string {
  return JSON.stringify(value, (_, item) => (typeof item === "bigint" ? item.toString() : item));
}

function same(actual: unknown, expected: unknown, context: string): void {
  if (serializable(actual) !== serializable(expected))
    throw new Error(`${context}: result mismatch: ${serializable({ actual, expected })}`);
}

type CheapResult = {
  id: string;
  sql: string;
  params: Value[];
  operations: number;
  validated: DrainResult | Command;
  preparedQueryValidation?: DrainResult;
  latencyUs: Summary;
  rawSamplesUs: number[];
};

async function measureCheap(
  driver: ComparisonDriver,
  config: ComparisonConfig["cheap"],
  onProgress?: (message: string) => void,
): Promise<CheapResult[]> {
  const results: CheapResult[] = [];
  const preparedSql = "SELECT $1::i64 + 1";
  const params: Value[] = [{ kind: "int", int: 42n }];
  for (const id of ["cheap-query", "prepared-execute"]) {
    const sql = id === "cheap-query" ? "SELECT 1" : preparedSql;
    const prepared = id === "prepared-execute" ? await driver.prepare(sql, params) : undefined;
    try {
      const preparedQueryValidation = prepared ? await prepared.query() : undefined;
      // Preserve a synchronous call path for direct measurements. Awaiting even an immediate
      // value once per operation would introduce overhead absent from the old application API.
      const runDirect = () =>
        prepared
          ? (prepared as DirectPrepared).execute()
          : (driver as Extract<ComparisonDriver, { mode: "direct" }>).query(sql);
      const runWorker = () =>
        prepared
          ? (prepared as WorkerPrepared).execute()
          : (driver as Extract<ComparisonDriver, { mode: "worker" }>).query(sql);
      const validated = driver.mode === "direct" ? runDirect() : await runWorker();
      if (preparedQueryValidation) {
        const expected = new Fingerprint();
        expected.row([{ kind: "int", int: 43n }]);
        const expectedRow = expected.result({
          ...preparedQueryValidation,
          cost: BigInt(preparedQueryValidation.cost),
        });
        if (
          preparedQueryValidation.rows !== 1 ||
          preparedQueryValidation.fingerprint !== expectedRow.fingerprint ||
          serializable(preparedQueryValidation.columnTypes) !== '["i64"]' ||
          preparedQueryValidation.rowsAffected !== null ||
          !("changes" in validated) ||
          validated.changes !== 0 ||
          validated.cost.toString() !== preparedQueryValidation.cost
        )
          throw new Error(
            "prepared parameter 42 must produce i64 value 43 and matching command cost",
          );
      }
      const runBatch = async (): Promise<{
        values: (DrainResult | Command)[];
        elapsedUs: number;
      }> => {
        const values: (DrainResult | Command)[] = [];
        const start = performance.now();
        if (driver.mode === "direct") {
          for (let index = 0; index < config.operations; index++) values.push(runDirect());
        } else {
          for (let index = 0; index < config.operations; index++) values.push(await runWorker());
        }
        const elapsedUs = ((performance.now() - start) * 1000) / config.operations;
        return { values, elapsedUs };
      };
      const validateBatch = (values: (DrainResult | Command)[]) => {
        for (const value of values) same(value, validated, id);
      };
      onProgress?.(`${id}: ${config.operations} sequential calls per sample`);
      for (let index = 0; index < config.warmups; index++) validateBatch((await runBatch()).values);
      const rawSamplesUs: number[] = [];
      for (let index = 0; index < config.samples; index++) {
        const sample = await runBatch();
        validateBatch(sample.values);
        rawSamplesUs.push(sample.elapsedUs);
      }
      results.push({
        id,
        sql,
        params: prepared ? params : [],
        operations: config.operations,
        validated,
        ...(preparedQueryValidation ? { preparedQueryValidation } : {}),
        latencyUs: summarize(rawSamplesUs),
        rawSamplesUs,
      });
    } finally {
      await prepared?.close();
    }
  }
  return results;
}

export async function runComparison(options: {
  driver: ComparisonDriver;
  config: ComparisonConfig;
  observe?: ObserveWork;
  onProgress?: (message: string) => void;
}) {
  const { driver, config, onProgress } = options;
  const observe: ObserveWork = options.observe ?? ((work) => observeTimers(work));
  const cheapCases = await measureCheap(driver, config.cheap, onProgress);
  const heavyResults = [];
  for (const workload of heavyCases(config.heavy)) {
    onProgress?.(`${workload.id}: setup and validation`);
    for (const sql of workload.setup) await driver.execute(sql);
    const run = () => driver.query(workload.sql);
    const validated = await run();
    if (validated.rows !== workload.expectedRows)
      throw new Error(
        `${workload.id}: expected ${workload.expectedRows} rows, got ${validated.rows}`,
      );
    for (let index = 0; index < config.heavy.warmups; index++)
      same(await run(), validated, `${workload.id}/warmup`);
    onProgress?.(`${workload.id}: ${config.heavy.samples} latency samples`);
    const rawSamplesMs: number[] = [];
    for (let index = 0; index < config.heavy.samples; index++) {
      const start = performance.now();
      const result = driver.mode === "direct" ? driver.query(workload.sql) : await run();
      rawSamplesMs.push(performance.now() - start);
      same(result, validated, `${workload.id}/sample${index}`);
    }
    const responsiveness: HeavyObservation[] = [];
    for (let index = 0; index < config.heavy.responseSamples; index++) {
      const observation = await observe(run, { caseId: workload.id, boundary: driver.mode });
      same(observation.result, validated, `${workload.id}/responsiveness${index}`);
      responsiveness.push(observation);
    }
    heavyResults.push({
      ...workload,
      validated,
      latencyMs: summarize(rawSamplesMs),
      rawSamplesMs,
      responsiveness,
    });
  }
  return {
    config,
    methodology: [
      "Each process loads one untouched engine revision and one boundary; cross-process results must be validated by the aggregator.",
      "Warm in-memory databases with 8192-byte pages. Database/session startup, setup and preparation are excluded from latency samples.",
      "Heavy latency includes query creation, complete typed drain, fingerprint and cursor close; result comparisons occur afterward.",
      "Cheap samples time sequential calls and divide elapsed time by their count; direct calls never await per operation.",
      "Every cheap operation is validated after timing. Prepared execution also has an untimed typed query validation.",
      "Heavy SQL, sizes, typed fingerprint and drains come unchanged from async-heavy.ts. No query receives an AbortSignal.",
      "Timer/frame observers run in separate responsiveness samples; they do not measure worker cancellation latency.",
    ],
    heavyCases: heavyResults,
    cheapCases,
  };
}
