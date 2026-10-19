// Platform-neutral async embedding API. Only the adapters import platform worker APIs.
import type { Value } from "../value.ts";
import type { JsonNode } from "../json.ts";
import { toValue, valueToJs, type JsParam, type Row, type RunResult } from "../ergonomic.ts";
import type { SessionOptions, TxStatus } from "../snapshot.ts";
import type { ScriptSummary } from "../split.ts";
import { decodeError, TransportError } from "./errors.ts";
import { encodeValues, measureValues, WireBudget } from "./wire.ts";
import { decodePackedRows } from "./packed.ts";
import {
  RUNTIME_VERSION,
  type FetchReply,
  type QueryReply,
  type RuntimeLimits,
  type RuntimeReply,
  type RuntimeRequest,
  type RuntimeTransport,
  isControlOperation,
  runtimeLimits,
} from "./protocol.ts";

export type OperationOptions = { signal?: AbortSignal };
export type SessionSettings = {
  maxCost?: bigint;
  lifetimeMaxCost?: bigint;
  maxSqlLength?: number;
  maxScalarBytes?: bigint;
  maxQueryMemoryBytes?: bigint;
  workMem?: number;
  timeZone?: string;
  vars?: Record<string, string>;
};

let workerCount = 0;
export function acquireWorker(options: Partial<RuntimeLimits> = {}): () => void {
  const limits = runtimeLimits(options);
  if (workerCount >= limits.maxWorkers)
    throw new TransportError("WORKER_LIMIT", "worker limit reached");
  workerCount++;
  let released = false;
  return () => {
    if (!released) {
      released = true;
      workerCount--;
    }
  };
}

type Pending = {
  resolve(value: unknown): void;
  reject(error: unknown): void;
  cleanup(): void;
  bytes: number;
  control: boolean;
};

// Account before structured clone, including keys, containers, and binary/string payloads.
// Refuse closures, engine instances and cyclic input instead of letting clone silently alter them.
function requestBytes(value: unknown, budget: WireBudget, seen = new Set<object>()): void {
  budget.reserve(16);
  if (
    value === null ||
    value === undefined ||
    typeof value === "boolean" ||
    typeof value === "number"
  )
    return;
  if (typeof value === "string") {
    budget.reserve(value.length * 2);
    return;
  }
  if (typeof value === "bigint") {
    // bigint.toString() would allocate in proportion to an arbitrary host-provided integer.
    // SQL values, settings, counters and IEEE-bit payloads all fit this finite envelope.
    if (value < -(1n << 63n) || value > (1n << 64n) - 1n) {
      throw new TransportError("PROTOCOL", "worker integer fields must fit 64 bits");
    }
    budget.reserve(8);
    return;
  }
  if (typeof value !== "object")
    throw new TransportError("UNSUPPORTED", "worker options cannot contain functions");
  if (value instanceof Uint8Array) {
    budget.reserve(value.byteLength);
    return;
  }
  if (typeof SharedArrayBuffer !== "undefined" && value instanceof SharedArrayBuffer) {
    budget.reserve(value.byteLength);
    return;
  }
  if (seen.has(value)) throw new TransportError("PROTOCOL", "cyclic request value");
  if (seen.size >= 256) throw new TransportError("PROTOCOL", "request nesting exceeds 256");
  seen.add(value);
  if (Array.isArray(value)) {
    for (const item of value) requestBytes(item, budget, seen);
  } else {
    if (
      Object.getPrototypeOf(value) !== Object.prototype &&
      Object.getPrototypeOf(value) !== null
    ) {
      throw new TransportError(
        "UNSUPPORTED",
        "worker options must be plain data; register extensions inside the worker",
      );
    }
    for (const key in value) {
      if (!Object.hasOwn(value, key)) continue;
      budget.reserve(key.length * 2);
      requestBytes((value as Record<string, unknown>)[key], budget, seen);
    }
  }
  seen.delete(value);
}

function bindParams(params: JsParam[], maxBytes: number): Value[] {
  const budget = new WireBudget(maxBytes);
  const parents = new Set<readonly JsParam[]>();
  const measure = (param: JsParam, depth: number): void => {
    if (depth > 256) throw new TransportError("PROTOCOL", "parameter nesting exceeds 256");
    if (Array.isArray(param)) {
      // toValue recursively maps native arrays. Admit their complete shape before mapping
      // any of them, so an oversized/cyclic input cannot allocate a partial engine tree.
      budget.reserve(128 + param.length * 8);
      if (parents.has(param)) throw new TransportError("PROTOCOL", "cyclic bind parameter");
      parents.add(param);
      for (const element of param) measure(element, depth + 1);
      parents.delete(param);
    } else {
      // Scalar conversion builds only a small wrapper; bytes/Decimal/Value objects are
      // borrowed, then the typed preflight validates every recursive payload without copying.
      measureValues([toValue(param)], budget);
    }
  };
  budget.reserve(32 + params.length * 8);
  for (const param of params) measure(param, 0);
  return params.map(toValue);
}

export class RuntimeConnection {
  readonly limits: RuntimeLimits;
  private readonly transport: RuntimeTransport;
  private readonly sharedCancellation: boolean;
  private readonly onClosed: () => void;
  private pending = new Map<number, Pending>();
  private nextId = 1;
  private queuedBytes = 0;
  private queuedCount = 0;
  private controlCount = 0;
  private resultBytes = 0;
  private state: "open" | "closing" | "closed" = "open";
  private closing?: Promise<void>;
  private termination?: Promise<void>;
  private failure?: TransportError;

  constructor(
    transport: RuntimeTransport,
    limits: Partial<RuntimeLimits> = {},
    sharedCancellation = false,
    onClosed: () => void = () => {},
  ) {
    this.transport = transport;
    this.limits = runtimeLimits(limits);
    this.sharedCancellation = sharedCancellation;
    this.onClosed = onClosed;
    transport.onMessage((reply) => this.receive(reply));
    transport.onFailure((error) => this.fail(error));
  }

  async call(
    op: string,
    args: Record<string, unknown> = {},
    session?: number,
    transaction?: number,
    options: OperationOptions = {},
  ): Promise<unknown> {
    if (this.failure) throw this.failure;
    if (this.state !== "open" && !(this.state === "closing" && isControlOperation(op))) {
      throw new TransportError("CLOSED", "database is closed");
    }
    const control = isControlOperation(op);
    // Control calls have a separate finite reserve sized for every admitted handle.
    const controlLimit =
      this.limits.maxSessions + this.limits.maxStatements + this.limits.maxCursors + 4;
    if (
      control
        ? this.controlCount >= controlLimit
        : this.queuedCount >= this.limits.maxQueuedRequests
    ) {
      throw new TransportError("QUEUE_FULL", "worker request queue is full");
    }
    const budget = new WireBudget(control ? 65536 : this.limits.maxQueuedBytes - this.queuedBytes);
    try {
      requestBytes(args, budget);
    } catch (error) {
      if (error instanceof TransportError && error.code === "RESULT_TOO_LARGE") {
        throw new TransportError(
          "QUEUE_FULL",
          "worker request byte budget exceeded",
          "not-started",
        );
      }
      throw error;
    }
    const bytes = budget.used + 128;
    if (!control && this.queuedBytes + bytes > this.limits.maxQueuedBytes) {
      throw new TransportError("QUEUE_FULL", "worker request byte budget exceeded");
    }
    const id = this.nextId++;
    const signal = options.signal;
    const cancel = signal && this.sharedCancellation ? new SharedArrayBuffer(4) : undefined;
    const flag = cancel ? new Int32Array(cancel) : undefined;
    const request: RuntimeRequest = {
      version: RUNTIME_VERSION,
      id,
      op,
      session,
      transaction,
      args,
      cancel,
      aborted: signal?.aborted,
    };
    if (flag && signal?.aborted) Atomics.store(flag, 0, 1);
    return new Promise((resolve, reject) => {
      const abort = () => {
        if (flag) Atomics.store(flag, 0, 1);
        // Cancellation bypasses ordinary admission and creates no extra pending promise.
        try {
          this.transport.postMessage({
            version: RUNTIME_VERSION,
            id: this.nextId++,
            op: "cancel",
            args: { request: id },
          });
        } catch (error) {
          this.fail(error);
        }
      };
      const cleanup = () => signal?.removeEventListener("abort", abort);
      this.pending.set(id, { resolve, reject, cleanup, bytes, control });
      if (control) this.controlCount++;
      else {
        this.queuedCount++;
        this.queuedBytes += bytes;
      }
      signal?.addEventListener("abort", abort, { once: true });
      try {
        this.transport.postMessage(request);
      } catch (error) {
        this.remove(id);
        reject(
          new TransportError("PROTOCOL", error instanceof Error ? error.message : String(error)),
        );
      }
    });
  }

  reserveResult(): () => void {
    if (this.resultBytes + this.limits.maxBatchBytes > this.limits.maxResultBytes) {
      throw new TransportError(
        "QUEUE_FULL",
        "consume or close another result batch before fetching more rows",
      );
    }
    this.resultBytes += this.limits.maxBatchBytes;
    let released = false;
    return () => {
      if (!released) {
        released = true;
        this.resultBytes -= this.limits.maxBatchBytes;
      }
    };
  }

  private remove(id: number): Pending | undefined {
    const pending = this.pending.get(id);
    if (!pending) return;
    this.pending.delete(id);
    pending.cleanup();
    if (pending.control) this.controlCount--;
    else {
      this.queuedCount--;
      this.queuedBytes -= pending.bytes;
    }
    return pending;
  }

  private receive(reply: RuntimeReply): void {
    try {
      if (
        reply === null ||
        typeof reply !== "object" ||
        reply.version !== RUNTIME_VERSION ||
        !Number.isSafeInteger(reply.id) ||
        typeof reply.ok !== "boolean"
      ) {
        throw new Error("invalid runtime protocol response");
      }
      if (!this.pending.has(reply.id)) return;
      // Decode before removing admission state: a malformed response must reject its own
      // request together with the others, rather than leaving that promise orphaned.
      const error = reply.ok ? undefined : decodeError(reply.error);
      if (error instanceof TransportError && error.code === "WORKER_FAILED") {
        this.fail(error);
        return;
      }
      const pending = this.remove(reply.id)!;
      if (reply.ok) pending.resolve(reply.result);
      else pending.reject(error);
    } catch (error) {
      this.fail(error);
    }
  }

  private fail(error: unknown): void {
    if (this.state === "closed") return;
    // A lost response can follow a committed write. Never replay requests on a replacement worker.
    this.failure = new TransportError(
      "WORKER_FAILED",
      error instanceof Error ? error.message : String(error),
      "unknown",
    );
    for (const id of this.pending.keys()) this.remove(id)?.reject(this.failure);
    this.state = "closed";
    void this.terminate().catch(() => {});
  }

  private terminate(): Promise<void> {
    if (this.termination === undefined) {
      // Keep the worker slot occupied until the platform has finished termination, even
      // when failure already rejected every operation and invalidated the public handle.
      this.termination = Promise.resolve()
        .then(() => this.transport.terminate())
        .finally(() => this.onClosed());
    }
    return this.termination;
  }

  close(): Promise<void> {
    if (this.closing) return this.closing;
    if (this.state === "closed") return this.termination ?? Promise.resolve();
    this.state = "closing";
    this.closing = (async () => {
      try {
        await this.call("close");
      } finally {
        for (const id of this.pending.keys())
          this.remove(id)?.reject(new TransportError("CLOSED", "database closed"));
        if (this.state !== "closed") {
          this.state = "closed";
        }
        await this.terminate();
      }
    })();
    return this.closing;
  }
}

export class AsyncRows implements AsyncIterableIterator<Value[]> {
  readonly columnNames: string[];
  readonly columnTypes: string[];
  readonly rowsAffected: number | null;
  cost: bigint;
  private readonly connection: RuntimeConnection;
  private readonly cursor: number;
  private readonly session?: number;
  private readonly transaction?: number;
  private readonly options: OperationOptions;
  private batch: Value[][] = [];
  private index = 0;
  private done = false;
  private closed = false;
  private release?: () => void;
  private tail: Promise<unknown> = Promise.resolve();
  private closing?: Promise<void>;
  private pendingPulls = 0;

  constructor(
    connection: RuntimeConnection,
    result: QueryReply,
    session?: number,
    transaction?: number,
    options: OperationOptions = {},
  ) {
    this.connection = connection;
    this.cursor = result.cursor;
    this.session = session;
    this.transaction = transaction;
    this.options = options;
    this.columnNames = result.columnNames;
    this.columnTypes = result.columnTypes;
    this.rowsAffected = result.rowsAffected;
    this.cost = result.cost;
  }

  [Symbol.asyncIterator](): AsyncIterableIterator<Value[]> {
    return this;
  }

  next(): Promise<IteratorResult<Value[]>> {
    if (this.pendingPulls >= this.connection.limits.maxQueuedRequests) {
      return Promise.reject(new TransportError("QUEUE_FULL", "too many pending cursor reads"));
    }
    // An already-decoded row needs no queued work. Consume it immediately in invocation order,
    // retaining the promise API without creating a chain of scheduling promises for every row.
    // Calls behind a fetch or close still join the queue, and abort must reach the worker while
    // its cursor is live even when a previous batch has resident rows left.
    if (this.pendingPulls === 0 && this.closing === undefined) {
      if (this.closed || (this.done && this.batch.length === 0)) {
        this.closed = true;
        return Promise.resolve({ done: true, value: undefined });
      }
      if (this.index < this.batch.length && (!this.options.signal?.aborted || this.done)) {
        return Promise.resolve(this.takeRow());
      }
    }
    this.pendingPulls++;
    const next = this.tail
      .then(() => this.pull())
      .finally(() => {
        this.pendingPulls--;
      });
    this.tail = next.catch(() => {});
    return next;
  }

  private clearBatch(): void {
    this.batch = [];
    this.index = 0;
    this.release?.();
    this.release = undefined;
  }

  private takeRow(): IteratorYieldResult<Value[]> {
    const row = this.batch[this.index++]!;
    if (this.index === this.batch.length) this.clearBatch();
    return { done: false, value: row };
  }

  private async pull(): Promise<IteratorResult<Value[]>> {
    if (this.closed) return { done: true, value: undefined };
    try {
      if (this.options.signal?.aborted && !this.done) this.clearBatch();
      if (this.index === this.batch.length) {
        this.clearBatch();
        if (this.done) {
          this.closed = true;
          return { done: true, value: undefined };
        }
        this.release = this.connection.reserveResult();
        const reply = (await this.connection.call(
          "fetch",
          { cursor: this.cursor },
          this.session,
          this.transaction,
          this.options,
        )) as FetchReply;
        if (
          reply === null ||
          typeof reply !== "object" ||
          typeof reply.done !== "boolean" ||
          typeof reply.cost !== "bigint" ||
          reply.rows === null ||
          typeof reply.rows !== "object" ||
          !Number.isSafeInteger(reply.rows.length) ||
          reply.rows.length < 0 ||
          reply.rows.length > this.connection.limits.maxBatchRows ||
          (reply.rows.length === 0 && !reply.done)
        ) {
          throw new TransportError("PROTOCOL", "invalid worker result batch");
        }
        const budget = new WireBudget(this.connection.limits.maxBatchBytes);
        budget.reserve(128);
        const decoded = decodePackedRows(reply.rows, budget);
        this.cost = reply.cost;
        this.done = reply.done;
        this.batch = decoded.rows;
        if (this.batch.length === 0) {
          this.clearBatch();
          this.closed = true;
          return { done: true, value: undefined };
        }
      }
      return this.takeRow();
    } catch (error) {
      this.clearBatch();
      this.closed = true;
      try {
        await this.connection.call(
          "cursor.close",
          { cursor: this.cursor },
          this.session,
          this.transaction,
        );
      } catch {
        /* Preserve the statement/transport failure. */
      }
      throw error;
    }
  }

  close(): Promise<void> {
    if (this.closing) return this.closing;
    this.closing = this.tail.then(async () => {
      if (this.closed) return;
      this.closed = true;
      this.clearBatch();
      if (!this.done)
        await this.connection.call(
          "cursor.close",
          { cursor: this.cursor },
          this.session,
          this.transaction,
        );
    });
    this.tail = this.closing.catch(() => {});
    return this.closing;
  }

  async return(): Promise<IteratorResult<Value[]>> {
    await this.close();
    return { done: true, value: undefined };
  }
  async [Symbol.asyncDispose](): Promise<void> {
    await this.close();
  }
}

type StatementInput = { sql: string; params?: Value[] };

class AsyncHandle {
  protected readonly connection: RuntimeConnection;
  protected readonly sessionId?: number;
  protected readonly transactionId?: number;
  protected closed = false;
  constructor(connection: RuntimeConnection, session?: number, transaction?: number) {
    this.connection = connection;
    this.sessionId = session;
    this.transactionId = transaction;
  }
  protected call(
    op: string,
    args: Record<string, unknown> = {},
    options: OperationOptions = {},
  ): Promise<unknown> {
    if (this.closed) return Promise.reject(new TransportError("CLOSED", "handle is closed"));
    return this.connection.call(op, args, this.sessionId, this.transactionId, options);
  }
  private encoded(params: Value[]): ReturnType<typeof encodeValues> {
    return encodeValues(params, new WireBudget(this.connection.limits.maxQueuedBytes));
  }
  async query(
    sql: string,
    params: Value[] = [],
    options: OperationOptions = {},
  ): Promise<AsyncRows> {
    const result = (await this.call(
      "query",
      { sql, params: this.encoded(params) },
      options,
    )) as QueryReply;
    return new AsyncRows(this.connection, result, this.sessionId, this.transactionId, options);
  }
  async execute(
    sql: string,
    params: Value[] = [],
    options: OperationOptions = {},
  ): Promise<RunResult> {
    return (await this.call(
      "execute",
      { sql, params: this.encoded(params) },
      options,
    )) as RunResult;
  }
  async prepare(sql: string): Promise<AsyncPreparedStatement> {
    const { statement } = (await this.call("prepare", { sql })) as { statement: number };
    return new AsyncPreparedStatement(
      this.connection,
      statement,
      this.sessionId,
      this.transactionId,
    );
  }
  async batch(statements: StatementInput[], options: OperationOptions = {}): Promise<RunResult[]> {
    const budget = new WireBudget(this.connection.limits.maxQueuedBytes);
    budget.reserve(32 + statements.length * 8);
    const encoded = statements.map(({ sql, params = [] }) => {
      budget.reserve(sql.length * 2 + 64);
      return { sql, params: encodeValues(params, budget) };
    });
    return (await this.call("batch", { statements: encoded }, options)) as RunResult[];
  }
  async executeScript(sql: string, options: OperationOptions = {}): Promise<ScriptSummary> {
    return (await this.call("script", { sql }, options)) as ScriptSummary;
  }
  async run(sql: string, ...params: JsParam[]): Promise<RunResult> {
    return this.execute(sql, bindParams(params, this.connection.limits.maxQueuedBytes));
  }
  async get(sql: string, ...params: JsParam[]): Promise<Row | undefined> {
    const rows = await this.query(sql, bindParams(params, this.connection.limits.maxQueuedBytes));
    try {
      const row = await rows.next();
      return row.done
        ? undefined
        : objectRow(
            rows.columnNames,
            row.value,
            new WireBudget(this.connection.limits.maxResultBytes),
          );
    } finally {
      await rows.close();
    }
  }
  async all(sql: string, ...params: JsParam[]): Promise<Row[]> {
    const rows = await this.query(sql, bindParams(params, this.connection.limits.maxQueuedBytes));
    const result: Row[] = [];
    const budget = new WireBudget(this.connection.limits.maxResultBytes);
    try {
      for await (const row of rows) {
        measureValues(row, budget);
        result.push(objectRow(rows.columnNames, row, budget));
      }
    } finally {
      await rows.close();
    }
    return result;
  }
}

function renderedChars(value: Value, limit: number): number {
  const checked = (chars: number): number => {
    if (!Number.isSafeInteger(chars) || chars > limit) {
      throw new TransportError("RESULT_TOO_LARGE", "rendered result exceeds its byte limit");
    }
    return chars;
  };
  switch (value.kind) {
    case "null":
      return checked(4);
    case "int":
      return checked(20);
    case "bool":
      return checked(5);
    case "f32":
    case "f64":
      return checked(32);
    case "text":
    case "json":
    case "jsonpath":
      return checked(value.text.length);
    case "bytea":
      return checked(2 + value.bytes.length * 2);
    case "uuid":
      return checked(36);
    case "date":
      return checked(64);
    case "timestamp":
    case "timestamptz":
    case "interval":
      return checked(128);
    case "decimal":
      return checked(Math.max(value.dec.limbs.length * 4, value.dec.scale + 1) + 2);
    case "jsonb":
      return jsonRenderedChars(value.node, limit);
    case "composite":
    case "array": {
      const children = value.kind === "composite" ? value.fields : value.elements;
      let chars = checked(value.kind === "array" ? 2 + value.dims.length * 64 : 2);
      for (const child of children) {
        // Delimiters, optional quotes, and doubled quote/backslash escapes at every nesting
        // level. Multiplication is checked before materializing the recursively escaped text.
        chars = checked(chars + 3 + (value.kind === "array" ? value.dims.length * 2 : 0));
        chars = checked(chars + 2 * renderedChars(child, Math.floor((limit - chars) / 2)));
      }
      return chars;
    }
    case "range": {
      let chars = checked(5);
      for (const bound of [value.lower, value.upper]) {
        if (bound !== null) {
          chars = checked(chars + 2);
          chars = checked(chars + 2 * renderedChars(bound, Math.floor((limit - chars) / 2)));
        }
      }
      return chars;
    }
    case "unfetched":
      throw new TransportError("PROTOCOL", "unresolved result value");
  }
}

function jsonRenderedChars(value: JsonNode, limit: number): number {
  const checked = (chars: number): number => {
    if (!Number.isSafeInteger(chars) || chars > limit) {
      throw new TransportError("RESULT_TOO_LARGE", "rendered JSON result exceeds its byte limit");
    }
    return chars;
  };
  switch (value.kind) {
    case "null":
      return checked(4);
    case "bool":
      return checked(5);
    case "number":
      return renderedChars({ kind: "decimal", dec: value.dec }, limit);
    case "string":
      return checked(2 + 6 * value.value.length);
    case "array": {
      let chars = checked(2);
      for (const child of value.elements) {
        chars = checked(chars + 2);
        chars = checked(chars + jsonRenderedChars(child, limit - chars));
      }
      return chars;
    }
    case "object": {
      let chars = checked(2);
      for (const member of value.members) {
        chars = checked(chars + 6 + 6 * member.key.length);
        chars = checked(chars + jsonRenderedChars(member.value, limit - chars));
      }
      return chars;
    }
  }
}

function objectRow(names: string[], values: Value[], budget: WireBudget): Row {
  // Ergonomic results render open types to text. Their output can dwarf wire bytes (a zero
  // Decimal at scale 16383, or nested quoting), so preflight the expansion before render().
  budget.reserve(32 + names.length * 16);
  for (let i = 0; i < names.length; i++) {
    budget.reserve(names[i].length * 2);
    const value = values[i];
    switch (value.kind) {
      case "null":
      case "bool":
      case "int":
      case "f32":
      case "f64":
        budget.reserve(8);
        break;
      case "text":
        budget.reserve(value.text.length * 2);
        break;
      case "bytea":
        budget.reserve(value.bytes.byteLength);
        break;
      default:
        budget.reserve(2 * renderedChars(value, Math.floor(budget.remaining / 2)));
    }
  }
  const row: Row = {};
  for (let i = 0; i < names.length; i++)
    Object.defineProperty(row, names[i], {
      value: valueToJs(values[i]),
      writable: true,
      enumerable: true,
      configurable: true,
    });
  return row;
}

export class AsyncPreparedStatement {
  readonly id: number;
  private readonly connection: RuntimeConnection;
  private readonly sessionId?: number;
  private readonly transactionId?: number;
  private closed = false;
  private closing?: Promise<void>;
  constructor(connection: RuntimeConnection, id: number, session?: number, transaction?: number) {
    this.connection = connection;
    this.sessionId = session;
    this.transactionId = transaction;
    this.id = id;
  }
  private call(
    op: string,
    args: Record<string, unknown>,
    options: OperationOptions = {},
  ): Promise<unknown> {
    if (this.closed)
      return Promise.reject(new TransportError("CLOSED", "prepared statement is closed"));
    return this.connection.call(op, args, this.sessionId, this.transactionId, options);
  }
  // Prepared methods deliberately bind only values: the parsed SQL remains worker-owned.
  async query(params: Value[] = [], options: OperationOptions = {}): Promise<AsyncRows> {
    const result = (await this.call(
      "query",
      {
        statement: this.id,
        params: encodeValues(params, new WireBudget(this.connection.limits.maxQueuedBytes)),
      },
      options,
    )) as QueryReply;
    return new AsyncRows(this.connection, result, this.sessionId, this.transactionId, options);
  }
  async execute(params: Value[] = [], options: OperationOptions = {}): Promise<RunResult> {
    return (await this.call(
      "execute",
      {
        statement: this.id,
        params: encodeValues(params, new WireBudget(this.connection.limits.maxQueuedBytes)),
      },
      options,
    )) as RunResult;
  }
  async batch(bindings: Value[][], options: OperationOptions = {}): Promise<RunResult[]> {
    const budget = new WireBudget(this.connection.limits.maxQueuedBytes);
    budget.reserve(32 + bindings.length * 8);
    const statements = bindings.map((params) => ({
      statement: this.id,
      params: encodeValues(params, budget),
    }));
    return (await this.call("batch", { statements }, options)) as RunResult[];
  }
  async run(...params: JsParam[]): Promise<RunResult> {
    return this.execute(bindParams(params, this.connection.limits.maxQueuedBytes));
  }
  async get(...params: JsParam[]): Promise<Row | undefined> {
    const rows = await this.query(bindParams(params, this.connection.limits.maxQueuedBytes));
    try {
      const row = await rows.next();
      return row.done
        ? undefined
        : objectRow(
            rows.columnNames,
            row.value,
            new WireBudget(this.connection.limits.maxResultBytes),
          );
    } finally {
      await rows.close();
    }
  }
  async all(...params: JsParam[]): Promise<Row[]> {
    const rows = await this.query(bindParams(params, this.connection.limits.maxQueuedBytes));
    const result: Row[] = [];
    const budget = new WireBudget(this.connection.limits.maxResultBytes);
    try {
      for await (const row of rows) {
        measureValues(row, budget);
        result.push(objectRow(rows.columnNames, row, budget));
      }
    } finally {
      await rows.close();
    }
    return result;
  }
  close(): Promise<void> {
    if (this.closing) return this.closing;
    const closing = this.call("statement.close", { statement: this.id })
      .then(() => {})
      .catch((error) => {
        // A rejected admission did not close the worker handle; permit cleanup to retry.
        this.closed = false;
        this.closing = undefined;
        throw error;
      });
    this.closed = true;
    this.closing = closing;
    return closing;
  }
  async [Symbol.asyncDispose](): Promise<void> {
    await this.close();
  }
}

export class AsyncSession extends AsyncHandle {
  private closing?: Promise<void>;
  async begin(writable = true): Promise<AsyncTransaction> {
    const { transaction } = (await this.call("begin", { writable })) as { transaction: number };
    return new AsyncTransaction(this.connection, this.sessionId!, transaction);
  }
  async commit(): Promise<void> {
    await this.call("commit");
  }
  async rollback(): Promise<void> {
    await this.call("rollback");
  }
  async status(): Promise<{ status: TxStatus; inTransaction: boolean; lifetimeCost: bigint }> {
    return (await this.call("status")) as {
      status: TxStatus;
      inTransaction: boolean;
      lifetimeCost: bigint;
    };
  }
  async lifetimeCost(): Promise<bigint> {
    return (await this.status()).lifetimeCost;
  }
  async setVar(name: string, value: string): Promise<void> {
    await this.configure({ vars: { [name]: value } });
  }
  async configure(settings: SessionSettings): Promise<void> {
    await this.call("configure", settings);
  }
  async update<R>(fn: (tx: AsyncTransaction) => R | Promise<R>): Promise<R> {
    return this.withTransaction(true, fn);
  }
  async view<R>(fn: (tx: AsyncTransaction) => R | Promise<R>): Promise<R> {
    return this.withTransaction(false, fn);
  }
  private async withTransaction<R>(
    writable: boolean,
    fn: (tx: AsyncTransaction) => R | Promise<R>,
  ): Promise<R> {
    const tx = await this.begin(writable);
    try {
      const result = await fn(tx);
      await tx.commit();
      return result;
    } catch (error) {
      try {
        await tx.rollback();
      } catch {
        /* Preserve the callback/commit failure. */
      }
      throw error;
    }
  }
  close(): Promise<void> {
    if (this.closing) return this.closing;
    const closing = this.call("session.close")
      .then(() => {})
      .catch((error) => {
        this.closed = false;
        this.closing = undefined;
        throw error;
      });
    this.closed = true;
    this.closing = closing;
    return closing;
  }
  async [Symbol.asyncDispose](): Promise<void> {
    await this.close();
  }
}

export class AsyncTransaction extends AsyncHandle {
  async commit(): Promise<void> {
    if (this.closed) return;
    // Retain the proxy on failure so a callback can still roll back when admission was
    // rejected. If COMMIT actually ended the block, the worker rejects its stale token.
    await this.call("commit");
    this.closed = true;
  }
  async rollback(): Promise<void> {
    if (this.closed) return;
    await this.call("rollback");
    this.closed = true;
  }
  async close(): Promise<void> {
    await this.rollback();
  }
  async [Symbol.asyncDispose](): Promise<void> {
    await this.rollback();
  }
}

export class AsyncDatabase extends AsyncHandle {
  async session(options: SessionOptions = {}): Promise<AsyncSession> {
    const { session } = (await this.call("session.open", { options: sessionOptions(options) })) as {
      session: number;
    };
    return new AsyncSession(this.connection, session);
  }
  async readSession(options: SessionOptions = {}): Promise<AsyncSession> {
    const { session } = (await this.call("session.open", {
      options: sessionOptions(options),
      mode: "read",
    })) as { session: number };
    return new AsyncSession(this.connection, session);
  }
  async update<R>(fn: (tx: AsyncTransaction) => R | Promise<R>): Promise<R> {
    const session = await this.session();
    try {
      return await session.update(fn);
    } finally {
      await session.close();
    }
  }
  async view<R>(fn: (tx: AsyncTransaction) => R | Promise<R>): Promise<R> {
    const session = await this.session();
    try {
      return await session.view(fn);
    } finally {
      await session.close();
    }
  }
  close(): Promise<void> {
    this.closed = true;
    return this.connection.close();
  }
  async [Symbol.asyncDispose](): Promise<void> {
    await this.close();
  }
}

function sessionOptions(options: SessionOptions): Record<string, unknown> {
  return {
    ...options,
    defaultPrivileges:
      options.defaultPrivileges === undefined
        ? undefined
        : { bits: options.defaultPrivileges.bits },
  };
}
