// One independent TS database core lives in an execution worker. Requests are admitted before
// execution, ordered within a session, and dispatched exactly once. A writer waiter is retained
// as a continuation, never retried SQL, so the owning session can still reach COMMIT/ROLLBACK.
import { PreparedStatement, type Rows } from "../api.ts";
import { COOPERATE, runAsync, shouldCooperate, type Steps } from "../execution.ts";
import { EngineError, engineError } from "../errors.ts";
import { type Argon2Params, type Credential, DEFAULT_ARGON2_PARAMS } from "../crypto.ts";
import type { SessionOptions } from "../executor.ts";
import { PrivilegeSet } from "../privileges.ts";
import type { Database, Session } from "../shared.ts";
import type { Value } from "../value.ts";
import { encodeError, TransportError } from "./errors.ts";
import {
  isControlOperation,
  RUNTIME_VERSION,
  type FetchReply,
  type RuntimeLimits,
  type RuntimeReply,
  type RuntimeRequest,
  runtimeLimits,
} from "./protocol.ts";
import { decodeValues, WireBudget, type WireValue } from "./wire.ts";
import { PackedRowsWriter } from "./packed.ts";

type SessionHandle = { session: Session; transaction?: number };
type StatementHandle = { statement: PreparedStatement; session: number };
type CursorHandle = {
  rows: Rows;
  session: number;
  owner: SessionHandle;
  pending?: Value[];
  request: Job;
};
type Job = {
  request: RuntimeRequest;
  bytes: number;
  canceled: boolean;
  cancelFlag?: Int32Array;
  owner?: SessionHandle;
  statement?: PreparedStatement;
  params?: Value[];
  batchIndex: number;
  batchResults: unknown[];
  waitStarted?: number;
  waitTimer?: ReturnType<typeof setTimeout>;
};

const PARKED = Symbol("parked");
const MORE = Symbol("more");

// A conservative physical payload budget; unlike SQL's logical accounts this includes JS strings
// and object slots. The traversal itself is bounded and rejects cycles before allocating a copy.
function messageBytes(
  value: unknown,
  limit: number,
  parents = new Set<object>(),
  depth = 0,
): number {
  if (depth > 64) throw new TransportError("PROTOCOL", "request nesting exceeds 64 levels");
  if (typeof value === "string") return value.length * 2 + 16;
  if (value === null || value === undefined) return 8;
  if (typeof value !== "object") return 16;
  if (ArrayBuffer.isView(value)) return value.byteLength + 32;
  if (
    value instanceof ArrayBuffer ||
    (typeof SharedArrayBuffer !== "undefined" && value instanceof SharedArrayBuffer)
  ) {
    return value.byteLength + 32;
  }
  if (parents.has(value)) throw new TransportError("PROTOCOL", "cyclic request payload");
  parents.add(value);
  let size = 32;
  for (const [key, child] of Object.entries(value)) {
    size += key.length * 2 + 16 + messageBytes(child, limit - size, parents, depth + 1);
    if (size > limit) break;
  }
  parents.delete(value);
  return size;
}

function canceled(job: Job): boolean {
  if (job.canceled || job.request.aborted) return true;
  return job.cancelFlag !== undefined && Atomics.load(job.cancelFlag, 0) !== 0;
}

function canceledError(): EngineError {
  return engineError("query_canceled", "canceling statement due to user request");
}

// A message-only cancellation cannot change while a synchronous kernel is running. Leave the
// kernel's ordinary fast path intact when there is no shared flag to poll; queued cancellation is
// still checked at every dispatch/fetch boundary.
function cancellationCheck(job: Job): (() => boolean) | undefined {
  return job.request.cancel === undefined ? undefined : () => canceled(job);
}

export class RuntimeServer {
  private readonly openDatabase: (args: Record<string, unknown>) => Database | Promise<Database>;
  private readonly send: (reply: RuntimeReply, transfer?: ArrayBuffer[]) => void;
  private readonly scheduleTask: (callback: () => void) => void;
  private limits: RuntimeLimits;
  private database: Database | null = null;
  private sessions = new Map<number, SessionHandle>();
  private statements = new Map<number, StatementHandle>();
  private cursors = new Map<number, CursorHandle>();
  private jobs: Job[] = [];
  private queuedBytes = 0;
  private nextHandle = 1;
  private lastRequest = 0;
  private running = false;
  private activeJob: Job | null = null;
  private scheduled = false;
  private closing = false;
  private closed = false;

  constructor(options: {
    open: (args: Record<string, unknown>) => Database | Promise<Database>;
    send: (reply: RuntimeReply, transfer?: ArrayBuffer[]) => void;
    limits?: Partial<RuntimeLimits>;
    schedule?: (callback: () => void) => void;
  }) {
    this.openDatabase = options.open;
    this.send = options.send;
    this.scheduleTask =
      options.schedule ??
      ((callback) => {
        setTimeout(callback, 0);
      });
    this.limits = runtimeLimits(options.limits);
  }

  receive(request: RuntimeRequest): void {
    try {
      if (
        request.version !== RUNTIME_VERSION ||
        !Number.isSafeInteger(request.id) ||
        request.id <= 0
      ) {
        throw new TransportError("PROTOCOL", "unsupported worker protocol or request id");
      }
      // Monotonic ids give exact-once admission without an unbounded history of completed requests.
      if (request.id <= this.lastRequest) {
        throw new TransportError("PROTOCOL", "request id has already been admitted");
      }
      this.lastRequest = request.id;
      if (
        request.cancel !== undefined &&
        (!(
          typeof SharedArrayBuffer !== "undefined" && request.cancel instanceof SharedArrayBuffer
        ) ||
          request.cancel.byteLength < 4)
      ) {
        throw new TransportError("PROTOCOL", "invalid cancellation buffer");
      }
      if (this.closed || (this.closing && request.op !== "close" && request.op !== "cancel")) {
        throw new TransportError("CLOSED", "database worker is closing");
      }
      if (request.op === "cancel") {
        const id = request.args.request;
        for (const job of this.jobs) if (job.request.id === id) job.canceled = true;
        for (const cursor of this.cursors.values()) {
          if (cursor.request.request.id === id) cursor.request.canceled = true;
        }
        this.reply(request.id, null);
        this.schedule();
        return;
      }
      const bytes = messageBytes(request, this.limits.maxQueuedBytes);
      const controls = this.jobs.filter((job) => isControlOperation(job.request.op)).length;
      if (isControlOperation(request.op)) {
        const capacity =
          this.limits.maxSessions + this.limits.maxStatements + this.limits.maxCursors + 4;
        if (controls >= capacity) throw new TransportError("QUEUE_FULL", "control queue is full");
      } else if (
        this.jobs.length - controls >= this.limits.maxQueuedRequests ||
        this.queuedBytes + bytes > this.limits.maxQueuedBytes
      ) {
        throw new TransportError("QUEUE_FULL", "worker request queue is full");
      }
      if (bytes > this.limits.maxQueuedBytes) {
        throw new TransportError("QUEUE_FULL", "request exceeds the worker byte budget");
      }
      if (request.op === "close") {
        this.closing = true;
        this.cancelQueued();
      } else if (request.op === "session.close") {
        // Session close has the same rollback-on-close policy and cannot sit behind its own waiter.
        this.cancelQueued(request.session ?? 0);
      }
      this.jobs.push({
        request,
        bytes,
        canceled: false,
        cancelFlag: request.cancel === undefined ? undefined : new Int32Array(request.cancel, 0, 1),
        batchIndex: 0,
        batchResults: [],
      });
      this.queuedBytes += bytes;
      this.schedule();
    } catch (error) {
      this.fail(request.id, error);
    }
  }

  private cancelQueued(session?: number): void {
    for (const job of [...this.jobs]) {
      if (session !== undefined && (job.request.session ?? 0) !== session) continue;
      // A suspended continuation owns its cursor, pins, and statement scopes until its finally
      // blocks have unwound. Close cancels it and waits; only queued work can be removed now.
      if (job === this.activeJob) {
        if (job.request.op !== "open") job.canceled = true;
        continue;
      }
      this.finish(job, new TransportError("CLOSED", "operation canceled by close"));
    }
  }

  private schedule(): void {
    if (this.scheduled || this.running) return;
    this.scheduled = true;
    // A task-queue yield, not a resolved promise: cancel/control messages get a turn between jobs.
    this.scheduleTask(() => {
      this.scheduled = false;
      void this.pump();
    });
  }

  private async pump(): Promise<void> {
    if (this.running) return;
    this.running = true;
    let progressed = false;
    const seenSessions = new Set<number>();
    try {
      for (const job of [...this.jobs]) {
        const sessionId = job.request.session ?? 0;
        if (
          sessionId !== 0 &&
          (seenSessions.has(sessionId) ||
            this.jobs.some(
              (prior) =>
                (prior.request.session ?? 0) === sessionId && prior.request.id < job.request.id,
            ))
        )
          continue;
        if (sessionId !== 0) seenSessions.add(sessionId);
        this.activeJob = job;
        try {
          if (canceled(job)) throw canceledError();
          if (job.request.op === "open") {
            if (this.database !== null)
              throw new TransportError("PROTOCOL", "database already open");
            const configured = runtimeLimits(
              job.request.args.limits as Partial<RuntimeLimits> | undefined,
            );
            this.database = await this.openDatabase(job.request.args);
            this.limits = configured;
            this.finish(job, undefined, null);
          } else {
            const cursorRequest =
              job.request.op === "fetch"
                ? this.cursors.get(job.request.args.cursor as number)?.request
                : undefined;
            const check = () =>
              canceled(job) || (cursorRequest !== undefined && canceled(cursorRequest));
            const poll =
              cancellationCheck(job) !== undefined ||
              (cursorRequest !== undefined && cancellationCheck(cursorRequest) !== undefined)
                ? check
                : undefined;
            const result = await runAsync(this.dispatchSteps(job), {
              check,
              poll,
              schedule: this.scheduleTask,
            });
            if (result === PARKED) continue;
            if (result !== MORE) this.finish(job, undefined, result);
            else {
              this.jobs.splice(this.jobs.indexOf(job), 1);
              this.jobs.push(job);
            }
          }
          progressed = true;
          break; // round-robin task turns; a batch is one statement per turn
        } catch (error) {
          if (error instanceof EngineError && error.code() === "57014") {
            this.cancelStatement(job);
          }
          // A failed public commit closes its transaction proxy. Release the matching worker
          // transaction even when cancellation rejected the request before dispatch began.
          if (job.request.op === "commit") {
            try {
              this.failedCommit(job);
            } catch {
              /* Preserve the commit failure after cleanup. */
            }
          }
          this.finish(job, error);
          progressed = true;
          break;
        } finally {
          this.activeJob = null;
        }
      }
    } finally {
      this.running = false;
      if (progressed && this.jobs.length > 0) this.schedule();
    }
  }

  private requireDatabase(): Database {
    if (this.database === null) throw new TransportError("CLOSED", "no database is open");
    return this.database;
  }

  private cancelStatement(job: Job): void {
    const { op, session: id = 0, transaction } = job.request;
    if (op !== "query" && op !== "execute" && op !== "batch" && op !== "fetch" && op !== "script")
      return;
    const owner = job.owner ?? this.sessions.get(id);
    if (owner !== undefined && owner.transaction === transaction)
      owner.session.failCurrentStatement();
    if (op === "fetch") {
      const cursorId = job.request.args.cursor as number;
      const cursor = this.cursors.get(cursorId);
      if (cursor?.session === id && cursor.owner.transaction === transaction)
        this.closeCursor(cursorId);
    }
  }

  private failedCommit(job: Job): void {
    const id = job.request.session ?? 0;
    const owner = job.owner ?? this.sessions.get(id);
    if (owner === undefined || owner.transaction !== job.request.transaction) return;
    try {
      try {
        this.closeSessionCursors(id);
      } finally {
        owner.session.rollback();
      }
    } finally {
      if (!owner.session.inTransaction()) owner.transaction = undefined;
    }
  }

  private owner(job: Job): SessionHandle {
    if (job.owner !== undefined) return job.owner;
    const id = job.request.session ?? 0;
    const owner = id === 0 ? { session: this.requireDatabase().session() } : this.sessions.get(id);
    if (owner === undefined) throw new TransportError("CLOSED", "session is closed");
    if (owner.transaction !== job.request.transaction) {
      throw new TransportError("BUSY", "operation does not own the active transaction");
    }
    job.owner = owner;
    return owner;
  }

  private prepared(job: Job, args: Record<string, unknown>): PreparedStatement {
    if (job.statement !== undefined) return job.statement;
    const owner = this.owner(job);
    if (typeof args.sql === "string") {
      job.statement = owner.session.prepareStatement(args.sql);
    } else {
      const handle = this.statements.get(args.statement as number);
      if (handle === undefined || handle.session !== (job.request.session ?? 0)) {
        throw new TransportError(
          "CLOSED",
          "prepared statement is closed or belongs to another session",
        );
      }
      job.statement = handle.statement;
    }
    job.params = decodeValues(
      (args.params ?? []) as WireValue[],
      new WireBudget(this.limits.maxQueuedBytes),
    );
    return job.statement;
  }

  private waits(job: Job, statement: PreparedStatement): boolean {
    const session = this.owner(job).session;
    if (!session.waitsForWriter(statement)) {
      job.waitStarted = undefined;
      if (job.waitTimer !== undefined) clearTimeout(job.waitTimer);
      job.waitTimer = undefined;
      return false;
    }
    job.waitStarted ??= Date.now();
    const timeout = session.lockTimeoutMs;
    if (timeout > 0) {
      const remaining = timeout - (Date.now() - job.waitStarted);
      if (remaining <= 0)
        throw engineError("lock_not_available", "timed out waiting for the writer");
      if (job.waitTimer === undefined) {
        job.waitTimer = setTimeout(() => {
          job.waitTimer = undefined;
          this.schedule();
        }, remaining);
      }
    }
    return true;
  }

  private *dispatchSteps(job: Job): Steps<unknown> {
    const { op, args } = job.request;
    const sessionId = job.request.session ?? 0;
    if (op === "close") {
      yield* this.closeAllSteps();
      return null;
    }
    const db = this.requireDatabase();
    if (op === "session.open") {
      if (this.sessions.size >= this.limits.maxSessions)
        throw new TransportError("QUEUE_FULL", "session limit reached");
      const options = { ...(args.options ?? {}) } as SessionOptions;
      if (options.defaultPrivileges !== undefined) {
        const bits = options.defaultPrivileges.bits;
        if (!Number.isInteger(bits) || bits < 0 || bits > 31) {
          throw new TransportError("PROTOCOL", "invalid session default privileges");
        }
        options.defaultPrivileges = new PrivilegeSet(bits);
      }
      const session = args.mode === "read" ? db.readSession(options) : db.session(options);
      const id = this.nextHandle++;
      this.sessions.set(id, { session });
      return { session: id };
    }
    if (op === "session.close") {
      this.closeSession(sessionId);
      return null;
    }
    if (op === "statement.close") {
      const statement = this.statements.get(args.statement as number);
      if (statement !== undefined && statement.session === sessionId)
        this.statements.delete(args.statement as number);
      return null;
    }
    if (op === "cursor.close") {
      const cursor = this.cursors.get(args.cursor as number);
      if (cursor !== undefined && cursor.session === sessionId)
        this.closeCursor(args.cursor as number);
      return null;
    }
    if (op === "fetch") return yield* this.fetchSteps(job);
    if (op === "compact" || op === "changeCredential" || op === "rekey") {
      if (sessionId !== 0)
        throw new TransportError("PROTOCOL", "database maintenance belongs to the database handle");
      yield* this.maintenanceSteps(db, op, args);
      return null;
    }
    const owner = this.owner(job);
    const session = owner.session;
    switch (op) {
      case "prepare": {
        if (this.statements.size >= this.limits.maxStatements)
          throw new TransportError("QUEUE_FULL", "prepared statement limit reached");
        const statement = session.prepareStatement(args.sql as string);
        const id = this.nextHandle++;
        this.statements.set(id, { statement, session: sessionId });
        return { statement: id };
      }
      case "begin": {
        if (sessionId === 0) throw new TransportError("PROTOCOL", "transactions require a session");
        job.statement ??= new PreparedStatement({
          kind: "begin",
          writable: args.writable !== false,
        });
        if (this.waits(job, job.statement)) return PARKED;
        session.begin(args.writable !== false);
        owner.transaction = this.nextHandle++;
        return { transaction: owner.transaction };
      }
      case "commit":
      case "rollback": {
        this.closeSessionCursors(sessionId);
        try {
          if (op === "commit") yield* session.commitSteps();
          else session.rollback();
        } finally {
          if (!session.inTransaction()) owner.transaction = undefined;
        }
        return null;
      }
      case "status":
        return {
          status: session.status(),
          inTransaction: session.inTransaction(),
          lifetimeCost: session.lifetimeCost(),
        };
      case "configure":
        this.configure(session, args);
        return null;
      case "script":
        job.statement ??= new PreparedStatement({ kind: "begin", writable: true });
        if (this.waits(job, job.statement)) return PARKED;
        return yield* session.executeScriptSteps(args.sql as string);
      case "query":
      case "execute":
      case "batch": {
        const statements = op === "batch" ? (args.statements as Record<string, unknown>[]) : [args];
        if (!Array.isArray(statements))
          throw new TransportError("PROTOCOL", "batch statements must be an array");
        if (op === "batch" && statements.length * 128 > this.limits.maxBatchBytes) {
          throw new TransportError(
            "RESULT_TOO_LARGE",
            "batch command results exceed the transport budget",
            "not-started",
          );
        }
        if (job.batchIndex >= statements.length) return job.batchResults;
        const statement = this.prepared(job, statements[job.batchIndex]);
        if (this.waits(job, statement)) return PARKED;
        if (op === "query") {
          if (this.cursors.size >= this.limits.maxCursors)
            throw new TransportError("QUEUE_FULL", "cursor limit reached");
          const rows = yield* session.queryPreparedSteps(statement, job.params);
          const id = this.nextHandle++;
          const reply = {
            cursor: id,
            columnNames: rows.columnNames,
            columnTypes: rows.columnTypes,
            rowsAffected: rows.rowsAffected,
            cost: rows.cost,
          };
          if (messageBytes(reply, this.limits.maxBatchBytes) > this.limits.maxBatchBytes) {
            rows.close();
            throw new TransportError(
              "RESULT_TOO_LARGE",
              "result metadata exceeds the transport budget",
              "unknown",
            );
          }
          this.cursors.set(id, {
            rows,
            owner,
            session: sessionId,
            request: job,
          });
          if (!session.inTransaction()) owner.transaction = undefined;
          return reply;
        }
        const result = yield* session.executePreparedSteps(statement, job.params);
        if (!session.inTransaction()) owner.transaction = undefined;
        if (op !== "batch") return result;
        job.batchResults.push(result);
        job.batchIndex++;
        job.statement = undefined;
        job.params = undefined;
        return job.batchIndex === statements.length ? job.batchResults : MORE;
      }
      default:
        throw new TransportError("PROTOCOL", `unknown operation ${op}`);
    }
  }

  private *fetchSteps(job: Job): Steps<unknown> {
    const id = job.request.args.cursor as number;
    const cursor = this.cursors.get(id);
    if (cursor === undefined || cursor.session !== (job.request.session ?? 0)) {
      throw new TransportError("CLOSED", "cursor is closed or belongs to another session");
    }
    if (cursor.owner.transaction !== job.request.transaction) {
      throw new TransportError("BUSY", "cursor operation does not own the transaction");
    }
    let done = false;
    try {
      const batch = new PackedRowsWriter(this.limits.maxBatchBytes);
      let count = 0;
      while (count < this.limits.maxBatchRows) {
        if (shouldCooperate()) yield COOPERATE;
        if (canceled(job) || canceled(cursor.request)) throw canceledError();
        let row = cursor.pending;
        if (row === undefined) {
          const next = yield* cursor.rows.nextSteps();
          if (next.done) {
            done = true;
            break;
          }
          row = next.value;
        }
        if (!batch.append(row)) {
          cursor.pending = row;
          break;
        }
        cursor.pending = undefined;
        count++;
      }
      const cost = cursor.rows.cost;
      if (done) this.closeCursor(id);
      return { rows: batch.finish(), done, cost } satisfies FetchReply;
    } catch (error) {
      this.closeCursor(id);
      throw error;
    }
  }

  // Host maintenance on the database itself (spec/design/api.md §2.6, Encryption at rest). It never
  // waits for the writer: a session of this handle holding a write transaction, or a reader, cursor or
  // read session pinning a snapshot, is 55006 exactly as on the synchronous API.
  private *maintenanceSteps(db: Database, op: string, args: Record<string, unknown>): Steps<void> {
    const name = args.name;
    if (typeof name !== "string") throw new TransportError("PROTOCOL", "a database name is text");
    if (op === "compact") {
      db.compact(name);
      return;
    }
    const kdf = (args.kdf ?? DEFAULT_ARGON2_PARAMS) as Argon2Params;
    if (typeof kdf !== "object") throw new TransportError("PROTOCOL", "kdf is an object");
    const credential = args.credential ?? null;
    if (credential !== null && typeof credential !== "object") {
      throw engineError("invalid_parameter_value", "a credential is a passphrase or a 32-byte key");
    }
    if (op === "rekey") {
      db.rekey(name, credential as Credential | null, kdf);
    } else {
      if (credential === null) {
        throw engineError(
          "invalid_parameter_value",
          "a credential is a passphrase or a 32-byte key",
        );
      }
      yield* db.changeCredentialSteps(name, credential as Credential, kdf);
    }
  }

  private configure(session: Session, args: Record<string, unknown>): void {
    if (args.maxCost !== undefined) session.setMaxCost(args.maxCost as bigint);
    if (args.lifetimeMaxCost !== undefined)
      session.setLifetimeMaxCost(args.lifetimeMaxCost as bigint);
    if (args.maxSqlLength !== undefined) session.setMaxSqlLength(args.maxSqlLength as number);
    if (args.maxScalarBytes !== undefined) session.setMaxScalarBytes(args.maxScalarBytes as bigint);
    if (args.maxQueryMemoryBytes !== undefined)
      session.setMaxQueryMemoryBytes(args.maxQueryMemoryBytes as bigint);
    if (args.workMem !== undefined) session.setWorkMem(args.workMem as number);
    if (args.timeZone !== undefined) session.setTimeZone(args.timeZone as string);
    if (args.lockTimeoutMs !== undefined) session.lockTimeoutMs = args.lockTimeoutMs as number;
    if (args.vars !== undefined) {
      for (const [name, value] of Object.entries(args.vars as Record<string, string>))
        session.setVar(name, value);
    }
  }

  private closeCursor(id: number): void {
    const cursor = this.cursors.get(id);
    if (cursor === undefined) return;
    this.cursors.delete(id);
    try {
      cursor.rows.close();
    } finally {
      if (cursor.session === 0) cursor.owner.session.close();
    }
  }

  private closeSessionCursors(session: number): void {
    for (const [id, cursor] of this.cursors) if (cursor.session === session) this.closeCursor(id);
  }

  private closeSession(id: number): void {
    this.closeSessionCursors(id);
    for (const [key, statement] of this.statements)
      if (statement.session === id) this.statements.delete(key);
    const handle = this.sessions.get(id);
    this.sessions.delete(id);
    handle?.session.close();
  }

  private *closeAllSteps(): Steps<void> {
    let failure: unknown;
    const attempt = (fn: () => void) => {
      try {
        fn();
      } catch (error) {
        failure ??= error;
      }
    };
    for (const id of [...this.cursors.keys()]) attempt(() => this.closeCursor(id));
    for (const id of [...this.sessions.keys()]) attempt(() => this.closeSession(id));
    this.statements.clear();
    // An asynchronous storage stack flushes a relaxed batch through an awaited barrier.
    try {
      if (this.database !== null) yield* this.database.closeSteps();
    } catch (error) {
      failure ??= error;
    }
    this.database = null;
    this.closed = true;
    if (failure !== undefined) throw failure;
  }

  private finish(job: Job, error?: unknown, result?: unknown): void {
    const index = this.jobs.indexOf(job);
    if (index < 0) return;
    this.jobs.splice(index, 1);
    this.queuedBytes -= job.bytes;
    if (job.waitTimer !== undefined) clearTimeout(job.waitTimer);
    const keepsSession = error === undefined && job.request.op === "query";
    if ((job.request.session ?? 0) === 0 && !keepsSession) job.owner?.session.close();
    if (error !== undefined) this.fail(job.request.id, error);
    else
      this.reply(
        job.request.id,
        result,
        job.request.op === "fetch" ? [(result as FetchReply).rows.data] : undefined,
      );
  }

  private reply(id: number, result: unknown, transfer?: ArrayBuffer[]): void {
    this.send({ version: RUNTIME_VERSION, id, ok: true, result }, transfer);
  }

  private fail(id: number, error: unknown): void {
    this.send({ version: RUNTIME_VERSION, id, ok: false, error: encodeError(error) });
  }
}
