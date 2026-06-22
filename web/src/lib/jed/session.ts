// The main-thread client for the jed engine Web Worker (worker.ts) — the ONLY engine API the Svelte
// UI sees (spec/design/hosts.md §5). It owns ONE lazily-spawned worker shared across every live
// example and the tool: the ~engine bundle loads once, off the main thread, and each database is a
// handle (JedDb) keyed by an id inside that one worker. query/run are always async whether a db is
// in-memory or OPFS-backed — one uniform path, so a docs widget and the tool share this abstraction.
//
// Browser-only: it constructs a Worker, so every method must be called client-side (inside onMount /
// an event handler), never during SSR/prerender.

import type {
  DbMode,
  OpenSpec,
  OpfsFileInfo,
  Reply,
  Req,
  RunResult,
  SchemaResult
} from './protocol.ts';

// A request body is a Req without its rid (the transport assigns it). Distributes over the union.
type ReqBody = Req extends infer R ? (R extends { rid: number } ? Omit<R, 'rid'> : never) : never;

// DEFAULT_MEMORY_MAX_COST is the cost ceiling applied to in-memory demo/example databases by default
// (CLAUDE.md §13): generous enough for real demos (thousands of rows), low enough that a runaway
// query — e.g. SELECT * FROM generate_series(1, 1e12) — aborts with a clean 54P01 instead of hanging
// the worker. The tool overrides this (default unlimited, user-settable).
export const DEFAULT_MEMORY_MAX_COST = 5_000_000n;

// JedError mirrors an engine error on the main thread: a SQLSTATE code plus message (the structured
// error survives the worker boundary). The result grid renders code + message prominently.
export class JedError extends Error {
  readonly code: string;
  constructor(code: string, message: string) {
    super(message);
    this.name = 'JedError';
    this.code = code;
  }
}

let worker: Worker | null = null;
let nextRid = 1;
let nextDbSeq = 1;
const pending = new Map<number, { resolve: (v: unknown) => void; reject: (e: unknown) => void }>();

function ensureWorker(): Worker {
  if (worker !== null) return worker;
  // The new URL(..., import.meta.url) form lets Vite emit the worker as its own ES-module chunk
  // (the proven impl/ts pattern). Spawned lazily on first use so a reader who never runs an example
  // never pays for the engine.
  const w = new Worker(new URL('./worker.ts', import.meta.url), { type: 'module' });
  w.addEventListener('message', (ev: MessageEvent<Reply>) => {
    const reply = ev.data;
    const p = pending.get(reply.rid);
    if (p === undefined) return;
    pending.delete(reply.rid);
    if (reply.ok) p.resolve(reply.result);
    else p.reject(new JedError(reply.error.code, reply.error.message));
  });
  worker = w;
  return w;
}

function call(body: ReqBody): Promise<unknown> {
  const w = ensureWorker();
  const rid = nextRid++;
  const msg = { rid, ...body } as Req;
  return new Promise<unknown>((resolve, reject) => {
    pending.set(rid, { resolve, reject });
    // Transfer the bytes of an import so a large file image isn't structured-clone-copied.
    if (body.op === 'importBytes') {
      w.postMessage(msg, [body.bytes.buffer]);
    } else {
      w.postMessage(msg);
    }
  });
}

// JedDb is one open database (in-memory or OPFS-backed), driven through the shared worker.
export class JedDb {
  readonly id: string;
  readonly mode: DbMode;
  private closed = false;

  constructor(id: string, mode: DbMode) {
    this.id = id;
    this.mode = mode;
  }

  // run executes an editor buffer (one or more `;`-separated statements) and returns every
  // statement's outcome — the last query is what a grid shows; the per-statement tags are a log.
  async run(sql: string): Promise<RunResult[]> {
    return (await call({ op: 'run', id: this.id, sql })) as RunResult[];
  }

  // schema returns the visible catalog for a sidebar (tables/columns/PK/indexes + composite types).
  async schema(): Promise<SchemaResult> {
    return (await call({ op: 'schema', id: this.id })) as SchemaResult;
  }

  // reset replaces an in-memory database with a fresh one under the same id (a live example's
  // "reset to seed"). A no-op-ish convenience that is cheaper than close + reopen.
  async reset(): Promise<void> {
    await call({ op: 'reset', id: this.id });
  }

  // setMaxCost changes the cost ceiling on this handle live (0 = unlimited) — the tool's max_cost
  // control. A query aborts with 54P01 the instant accrued cost reaches it.
  async setMaxCost(value: bigint): Promise<void> {
    await call({ op: 'setMaxCost', id: this.id, value: value.toString() });
  }

  async commit(): Promise<void> {
    await call({ op: 'commit', id: this.id });
  }

  async rollback(): Promise<void> {
    await call({ op: 'rollback', id: this.id });
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    await call({ op: 'close', id: this.id });
  }
}

// --- factories ---------------------------------------------------------------------------------

// openMemory creates a fresh transient in-memory database (the home hero + every live docs example).
export async function openMemory(
  opts: { maxCost?: bigint; workMem?: number } = {}
): Promise<JedDb> {
  const id = `mem-${nextDbSeq++}`;
  const spec: OpenSpec = {
    id,
    mode: 'memory',
    maxCost: (opts.maxCost ?? DEFAULT_MEMORY_MAX_COST).toString(),
    workMem: opts.workMem
  };
  await call({ op: 'open', spec });
  return new JedDb(id, 'memory');
}

// createOpfsDb creates a new OPFS-backed database file (58P02 if it already exists).
export async function createOpfsDb(
  name: string,
  opts: { maxCost?: bigint; workMem?: number; pageSize?: number } = {}
): Promise<JedDb> {
  const id = `opfs-${nextDbSeq++}`;
  const spec: OpenSpec = {
    id,
    mode: 'opfs',
    name,
    create: true,
    pageSize: opts.pageSize,
    maxCost: opts.maxCost?.toString(),
    workMem: opts.workMem
  };
  await call({ op: 'open', spec });
  return new JedDb(id, 'opfs');
}

// openOpfsDb opens an existing OPFS-backed database file (58P01 if absent; XX001 if malformed).
export async function openOpfsDb(
  name: string,
  opts: { maxCost?: bigint; workMem?: number; readOnly?: boolean } = {}
): Promise<JedDb> {
  const id = `opfs-${nextDbSeq++}`;
  const spec: OpenSpec = {
    id,
    mode: 'opfs',
    name,
    create: false,
    readOnly: opts.readOnly,
    maxCost: opts.maxCost?.toString(),
    workMem: opts.workMem
  };
  await call({ op: 'open', spec });
  return new JedDb(id, 'opfs');
}

// --- OPFS file management (the tool's file manager) -------------------------------------------

export function opfsSupported(): Promise<boolean> {
  return call({ op: 'opfsSupported' }) as Promise<boolean>;
}

export function listOpfsFiles(): Promise<OpfsFileInfo[]> {
  return call({ op: 'listFiles' }) as Promise<OpfsFileInfo[]>;
}

export function exportOpfsFile(name: string): Promise<Uint8Array> {
  return call({ op: 'exportBytes', name }) as Promise<Uint8Array>;
}

export function importOpfsFile(name: string, bytes: Uint8Array, overwrite: boolean): Promise<void> {
  return call({ op: 'importBytes', name, bytes, overwrite }) as Promise<void>;
}

export function deleteOpfsFile(name: string): Promise<void> {
  return call({ op: 'deleteFile', name }) as Promise<void>;
}
