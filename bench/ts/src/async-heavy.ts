// Shared Node/browser workloads. Setup, validation comparisons and timer probes are outside
// latency samples. Both boundaries include the same complete drain and typed fingerprint.
import type { Value } from "../../../impl/ts/src/value.ts";
import type { RuntimeLimits } from "../../../impl/ts/src/async/protocol.ts";

export type Boundary = "direct" | "worker";
export type HeavyConfig = {
  warmups: number;
  samples: number;
  responseSamples: number;
  decimalRows: number;
  tableRows: number;
  sortRows: number;
  typedRows: number;
  cases: string[];
};
const CASE_IDS = ["decimal-aggregate", "resident-join-group", "sort-top100", "typed-output"];

// Experimental transport settings belong to the benchmark, not the production defaults.
export function heavyRuntimeFrom(
  settings: Record<string, string | undefined>,
): Partial<Pick<RuntimeLimits, "maxBatchRows" | "maxBatchBytes">> {
  const overrides: Partial<Pick<RuntimeLimits, "maxBatchRows" | "maxBatchBytes">> = {};
  for (const [suffix, key] of [
    ["BATCH_ROWS", "maxBatchRows"],
    ["BATCH_BYTES", "maxBatchBytes"],
  ] as const) {
    const name = `JED_ASYNC_HEAVY_${suffix}`;
    if (settings[name] === undefined) continue;
    const value = Number(settings[name]);
    if (!Number.isSafeInteger(value) || value <= 0)
      throw new Error(`${name} must be a positive safe integer`);
    overrides[key] = value;
  }
  return overrides;
}

export function heavyConfigFrom(settings: Record<string, string | undefined>): HeavyConfig {
  const integer = (suffix: string, fallback: number, minimum = 1) => {
    const name = `JED_ASYNC_HEAVY_${suffix}`;
    const value = Number(settings[name] ?? fallback);
    if (!Number.isSafeInteger(value) || value < minimum)
      throw new Error(`${name} must be a safe integer >= ${minimum}`);
    return value;
  };
  const cases = settings.JED_ASYNC_HEAVY_CASES?.split(",").map((id) => id.trim()) ?? CASE_IDS;
  if (cases.length === 0 || cases.some((id) => !CASE_IDS.includes(id)))
    throw new Error(`JED_ASYNC_HEAVY_CASES must contain IDs from ${CASE_IDS.join(",")}`);
  return {
    warmups: integer("WARMUPS", 3, 0),
    samples: integer("SAMPLES", 7),
    responseSamples: integer("RESPONSE_SAMPLES", 3),
    decimalRows: integer("DECIMAL_ROWS", 1_000_000),
    tableRows: integer("TABLE_ROWS", 100_000),
    sortRows: integer("SORT_ROWS", 800_000),
    typedRows: integer("TYPED_ROWS", 200_000),
    cases: [...new Set(cases)],
  };
}

type RowsMetadata = {
  readonly columnNames: string[];
  readonly columnTypes: string[];
  readonly rowsAffected: number | null;
  readonly cost: bigint;
};
type DirectRows = RowsMetadata & Iterable<Value[]> & { close(): void };
type WorkerRows = RowsMetadata & AsyncIterable<Value[]> & { close(): Promise<void> };
export type SyncSession = { query(sql: string): DirectRows; execute(sql: string): unknown };
export type AsyncSession = {
  query(sql: string): Promise<WorkerRows>;
  execute(sql: string): Promise<unknown>;
};
export type DrainResult = {
  rows: number;
  fingerprint: string;
  columnNames: string[];
  columnTypes: string[];
  rowsAffected: number | null;
  cost: string;
};

// Two independent 32-bit accumulators, with tags, lengths and row boundaries. This is a
// reproducibility check, not a cryptographic proof. No rendering or transport encoding occurs.
// These workloads deliberately use this small typed surface; unexpected kinds fail loudly.
export class Fingerprint {
  private first = 0x811c9dc5;
  private second = 0x9e3779b9;
  rows = 0;

  private word(value: number): void {
    this.first = Math.imul(this.first ^ value, 0x01000193) >>> 0;
    this.second = Math.imul(this.second ^ value, 0x85ebca6b) >>> 0;
  }

  private value(value: Value): void {
    switch (value.kind) {
      case "null":
        this.word(0);
        return;
      case "int": {
        this.word(1);
        const number = Number(value.int);
        if (!Number.isSafeInteger(number)) throw new Error("workload integer exceeds safe range");
        this.word(number >>> 0);
        this.word(Math.floor(number / 0x100000000));
        return;
      }
      case "decimal":
        this.word(2);
        this.word(value.dec.neg ? 1 : 0);
        this.word(value.dec.scale);
        this.word(value.dec.limbs.length);
        for (const limb of value.dec.limbs) this.word(limb);
        return;
      case "bytea":
        this.word(3);
        this.word(value.bytes.length);
        for (const byte of value.bytes) this.word(byte);
        return;
      case "array":
        this.word(4);
        this.word(value.dims.length);
        for (const dimension of value.dims) this.word(dimension);
        for (const bound of value.lbounds) this.word(bound);
        this.word(value.elements.length);
        for (const element of value.elements) this.value(element);
        return;
      default:
        throw new Error(`unexpected benchmark value kind: ${value.kind}`);
    }
  }

  row(row: Value[]): void {
    this.word(0x726f7700);
    this.word(row.length);
    for (const value of row) this.value(value);
    this.rows++;
  }

  result(rows: RowsMetadata): DrainResult {
    return {
      rows: this.rows,
      fingerprint: [this.first, this.second]
        .map((part) => part.toString(16).padStart(8, "0"))
        .join(""),
      columnNames: [...rows.columnNames],
      columnTypes: [...rows.columnTypes],
      rowsAffected: rows.rowsAffected,
      cost: rows.cost.toString(),
    };
  }
}

export function runDirectQuery(session: SyncSession, sql: string): DrainResult {
  const rows = session.query(sql);
  const fingerprint = new Fingerprint();
  try {
    for (const row of rows) fingerprint.row(row);
    return fingerprint.result(rows);
  } finally {
    rows.close();
  }
}

export async function runWorkerQuery(session: AsyncSession, sql: string): Promise<DrainResult> {
  const rows = await session.query(sql);
  const fingerprint = new Fingerprint();
  try {
    for await (const row of rows) fingerprint.row(row);
    return fingerprint.result(rows);
  } finally {
    await rows.close();
  }
}

export type Summary = { samples: number; min: number; p50: number; max: number };
export function summarize(samples: number[]): Summary {
  if (!samples.length) return { samples: 0, min: 0, p50: 0, max: 0 };
  const sorted = [...samples].sort((left, right) => left - right);
  const middle = Math.floor(sorted.length / 2);
  return {
    samples: sorted.length,
    min: sorted[0],
    p50: sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2,
    max: sorted.at(-1)!,
  };
}

const delay = (milliseconds: number) =>
  new Promise<void>((resolve) => setTimeout(resolve, milliseconds));
export type TimerObservation = {
  intervalMs: number;
  workStartedAtMs: number;
  workEndedAtMs: number;
  callbackTimesMs: number[];
  gapsMs: number[];
  ticksDuringWork: number;
  firstCallbackAfterWorkMs: number;
  overlappingGapsMs: number[];
  gaps: Summary;
};
export type HeavyObservation = { result: DrainResult; timer: TimerObservation; extra?: unknown };
export type ObserveWork = (
  work: () => DrainResult | Promise<DrainResult>,
  meta: { caseId: string; boundary: Boundary },
) => Promise<HeavyObservation>;

// Warm the timer before work and wait for the first callback after it. Otherwise a synchronous
// query could block every tick yet misleadingly report no gaps. All timestamps use performance.now.
export async function observeTimers<T>(
  work: () => T | Promise<T>,
): Promise<{ result: T; timer: TimerObservation }> {
  const intervalMs = 5;
  const callbackTimesMs: number[] = [];
  let wake: (() => void) | undefined;
  const interval = setInterval(() => {
    callbackTimesMs.push(performance.now());
    wake?.();
    wake = undefined;
  }, intervalMs);
  try {
    await delay(intervalMs * 4);
    const workStartedAtMs = performance.now();
    const result = await work();
    const workEndedAtMs = performance.now();
    await new Promise<void>((resolve) => {
      wake = resolve;
    });
    const gapsMs = callbackTimesMs.slice(1).map((time, index) => time - callbackTimesMs[index]);
    const overlappingGapsMs = gapsMs.filter(
      (_, index) =>
        callbackTimesMs[index] <= workEndedAtMs && callbackTimesMs[index + 1] >= workStartedAtMs,
    );
    return {
      result,
      timer: {
        intervalMs,
        workStartedAtMs,
        workEndedAtMs,
        callbackTimesMs,
        gapsMs,
        ticksDuringWork: callbackTimesMs.filter(
          (time) => time >= workStartedAtMs && time <= workEndedAtMs,
        ).length,
        firstCallbackAfterWorkMs: callbackTimesMs.find((time) => time >= workEndedAtMs)!,
        overlappingGapsMs,
        gaps: summarize(overlappingGapsMs),
      },
    };
  } finally {
    clearInterval(interval);
  }
}

export type HeavyCase = {
  id: string;
  description: string;
  setup: string[];
  sql: string;
  expectedRows: number;
};

export function heavyCases(config: HeavyConfig): HeavyCase[] {
  const cases: HeavyCase[] = [
    {
      id: "decimal-aggregate",
      description: "Generated exact-decimal sum; one result row",
      setup: [],
      sql: `SELECT sum(g::decimal) AS total FROM generate_series(1, ${config.decimalRows}) g`,
      expectedRows: 1,
    },
    {
      id: "resident-join-group",
      description: "Resident self-join and grouped integer sum; at most 128 result rows",
      setup: [
        "CREATE TABLE heavy_items (id i64 PRIMARY KEY, bucket i64, amount i64)",
        `INSERT INTO heavy_items SELECT g, g % 128, g % 1000 FROM generate_series(1, ${config.tableRows}) g`,
      ],
      sql: "SELECT l.bucket, sum(l.amount + r.amount) AS total FROM heavy_items l JOIN heavy_items r ON l.id = r.id GROUP BY l.bucket ORDER BY l.bucket",
      expectedRows: Math.min(config.tableRows, 128),
    },
    {
      id: "sort-top100",
      description: "Generated integer sort with top 100 output; synchronous sort kernel",
      setup: [],
      sql: `SELECT g::i64 AS id, (g::i64 * 7919) % 1000003 AS sort_key FROM generate_series(1, ${config.sortRows}) g ORDER BY sort_key, id LIMIT 100`,
      expectedRows: Math.min(config.sortRows, 100),
    },
    {
      id: "typed-output",
      description: "Complete drain of integer, decimal, bytes, array and NULL rows",
      setup: [],
      sql: `SELECT g::i64 AS id, g::decimal AS exact, decode('0001feff', 'hex') AS bytes, ARRAY[g::i64, g::i64 + 1] AS pair, NULL::text AS absent FROM generate_series(1, ${config.typedRows}) g ORDER BY id`,
      expectedRows: config.typedRows,
    },
  ];
  return cases.filter((workload) => config.cases.includes(workload.id));
}

export type HeavyCaseResult = HeavyCase & {
  validated: DrainResult;
  latencyMs: Record<Boundary, Summary>;
  rawSamplesMs: Record<Boundary, number[]>;
  responsiveness: Record<Boundary, HeavyObservation[]>;
};
export type HeavySuiteResult = {
  config: HeavyConfig;
  methodology: string[];
  cases: HeavyCaseResult[];
};

function sameResult(actual: DrainResult, expected: DrainResult, context: string): void {
  if (JSON.stringify(actual) !== JSON.stringify(expected))
    throw new Error(`${context} result mismatch: ${JSON.stringify({ actual, expected })}`);
}

export async function runHeavySuite(options: {
  direct: SyncSession;
  worker: AsyncSession;
  config: HeavyConfig;
  onProgress?: (message: string) => void;
  observe?: ObserveWork;
}): Promise<HeavySuiteResult> {
  const { direct, worker, config } = options;
  const observe: ObserveWork = options.observe ?? ((work) => observeTimers(work));
  const cases: HeavyCaseResult[] = [];
  for (const workload of heavyCases(config)) {
    options.onProgress?.(`${workload.id}: setup and validation`);
    for (const sql of workload.setup) {
      direct.execute(sql);
      await worker.execute(sql);
    }
    const run = {
      direct: () => runDirectQuery(direct, workload.sql),
      worker: () => runWorkerQuery(worker, workload.sql),
    };
    const validated = run.direct();
    if (validated.rows !== workload.expectedRows)
      throw new Error(
        `${workload.id}: expected ${workload.expectedRows} rows, got ${validated.rows}`,
      );
    sameResult(await run.worker(), validated, `${workload.id}/validation`);
    for (let index = 0; index < config.warmups; index++) {
      const order: Boundary[] = index % 2 ? ["worker", "direct"] : ["direct", "worker"];
      for (const boundary of order)
        sameResult(await run[boundary](), validated, `${workload.id}/warmup`);
    }
    options.onProgress?.(`${workload.id}: ${config.samples} latency samples per boundary`);
    const rawSamplesMs: Record<Boundary, number[]> = { direct: [], worker: [] };
    for (let index = 0; index < config.samples; index++) {
      const order: Boundary[] = index % 2 ? ["worker", "direct"] : ["direct", "worker"];
      for (const boundary of order) {
        const start = performance.now();
        const result = await run[boundary]();
        rawSamplesMs[boundary].push(performance.now() - start);
        sameResult(result, validated, `${workload.id}/${boundary}/sample${index}`);
      }
    }
    options.onProgress?.(
      `${workload.id}: ${config.responseSamples} separate responsiveness samples`,
    );
    const responsiveness: Record<Boundary, HeavyObservation[]> = { direct: [], worker: [] };
    for (let index = 0; index < config.responseSamples; index++) {
      const order: Boundary[] = index % 2 ? ["worker", "direct"] : ["direct", "worker"];
      for (const boundary of order) {
        const observation = await observe(run[boundary], { caseId: workload.id, boundary });
        sameResult(
          observation.result,
          validated,
          `${workload.id}/${boundary}/responsiveness${index}`,
        );
        responsiveness[boundary].push(observation);
      }
    }
    cases.push({
      ...workload,
      validated,
      latencyMs: { direct: summarize(rawSamplesMs.direct), worker: summarize(rawSamplesMs.worker) },
      rawSamplesMs,
      responsiveness,
    });
  }
  return {
    config,
    methodology: [
      "Warm in-memory databases; setup and worker startup excluded. SQL and data are identical across boundaries and hosts.",
      "Latency includes query creation, complete typed row drain, fingerprint and cursor close. Comparisons occur after timing.",
      "Every run checks row count, ordered typed fingerprint, column names/types, rowsAffected and final deterministic SQL cost.",
      "The fingerprint is a pair of 32-bit accumulators, not a cryptographic proof; values are not retained after each row.",
      "Direct/worker order alternates. Timers and browser observers run in separate responsiveness samples, never latency samples.",
      "Timer gaps include the first delayed callback after work. Main-thread responsiveness does not measure worker cancellation latency.",
      `${config.samples} latency samples per boundary are summarized by median and range; this report does not estimate tail percentiles.`,
    ],
    cases,
  };
}
