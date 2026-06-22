/// <reference lib="webworker" />
// The jed engine running inside a dedicated Web Worker (spec/design/hosts.md §5). OPFS sync access
// handles are only usable off the main thread, and the engine above the storage seam is synchronous,
// so the whole TS core runs HERE; the main thread drives it over postMessage (session.ts).
//
// This is the site-owned generalization of impl/ts/src/browser/worker.ts: it holds MANY databases
// keyed by id (one worker for every live example + the tool, engine loaded once), supports an
// in-memory mode as well as OPFS, and adds schema introspection + OPFS file management. Imports come
// straight from the node-clean engine modules ($jed alias → impl/ts/src) — NEVER lib.ts/file.ts,
// which pull `node:fs` and cannot load in a browser bundle.

import { EngineError } from '$jed/errors.ts';
import { Database, type Outcome } from '$jed/executor.ts';
import { createOpfs, openOpfs, closeOpfs } from '$jed/opfs.ts';
import { parseSQL } from '$jed/parser.ts';
import type { Statement } from '$jed/ast.ts';
import { typeCanonicalName } from '$jed/types.ts';
import { render } from '$jed/value.ts';
import { splitSql } from './split.ts';
import type {
  OpenSpec,
  OpfsFileInfo,
  Req,
  RunResult,
  SchemaResult,
  SchemaTable
} from './protocol.ts';

// Every open database this worker owns, keyed by the caller's id (an in-memory or OPFS-backed
// Database). One worker, many databases — but at most one OPFS handle per distinct file (the
// exclusive single-writer guarantee, CLAUDE.md §3, enforced by refusing a second open of a name).
const dbs = new Map<string, Database>();
// The OPFS file names currently held open (exclusive sync handles), so we can reject a double-open.
const openOpfsNames = new Set<string>();
// The OPFS file name backing each open db id (so close() can release its name reservation).
const opfsNameOf = new Map<string, string>();

function requireDb(id: string): Database {
  const db = dbs.get(id);
  if (db === undefined) throw new Error(`no database is open for id ${id}`);
  return db;
}

// applyHandleSettings applies the per-handle untrusted-query guards (CLAUDE.md §13): a cost ceiling
// and the work-memory budget. A default maxCost keeps a runaway live example (e.g. a huge
// generate_series) returning a clean 54P01 instead of hanging the worker.
function applyHandleSettings(db: Database, spec: OpenSpec): void {
  if (spec.maxCost !== undefined) db.setMaxCost(BigInt(spec.maxCost));
  if (spec.workMem !== undefined) db.setWorkMem(spec.workMem);
}

// --- statement execution ---------------------------------------------------------------------

// commandTag is the short outcome label for a non-query statement, in the spirit of the CLI's
// command tags (cli/src/session.rs) and PostgreSQL's (INSERT/UPDATE/DELETE carry a count).
function commandTag(stmt: Statement, rowsAffected: number | null): string {
  switch (stmt.kind) {
    case 'begin':
      return 'BEGIN';
    case 'commit':
      return 'COMMIT';
    case 'rollback':
      return 'ROLLBACK';
    case 'createTable':
      return 'CREATE TABLE';
    case 'dropTable':
      return 'DROP TABLE';
    case 'createIndex':
      return 'CREATE INDEX';
    case 'dropIndex':
      return 'DROP INDEX';
    case 'createType':
      return 'CREATE TYPE';
    case 'dropType':
      return 'DROP TYPE';
    case 'insert':
      return `INSERT 0 ${rowsAffected ?? 0}`;
    case 'update':
      return `UPDATE ${rowsAffected ?? 0}`;
    case 'delete':
      return `DELETE ${rowsAffected ?? 0}`;
    default:
      return 'OK';
  }
}

function serializeOutcome(stmt: Statement, out: Outcome): RunResult {
  if (out.kind === 'query') {
    return {
      kind: 'query',
      columnNames: out.columnNames,
      columnTypes: out.columnTypes,
      rows: out.rows.map((r) => r.map(render)),
      rowCount: out.rows.length,
      cost: out.cost.toString()
    };
  }
  return {
    kind: 'statement',
    rowsAffected: out.rowsAffected,
    cost: out.cost.toString(),
    tag: commandTag(stmt, out.rowsAffected)
  };
}

// runBatch splits an editor buffer into statements and runs each, returning every statement's
// outcome (the UI shows the last query in the grid + a per-statement log). A statement error throws
// — earlier statements already committed under autocommit, matching the CLI's stop-on-error default.
function runBatch(id: string, sql: string): RunResult[] {
  const db = requireDb(id);
  const out: RunResult[] = [];
  for (const stmt of splitSql(sql)) {
    const parsed = parseSQL(stmt.sql);
    out.push(serializeOutcome(parsed, db.executeStmtParams(parsed, [])));
  }
  return out;
}

// --- schema introspection (the tool's sidebar) -----------------------------------------------

function displayColumnType(
  type: string,
  decimal: { precision: number; scale: number } | null
): string {
  return decimal ? `numeric(${decimal.precision},${decimal.scale})` : type;
}

function schemaOf(id: string): SchemaResult {
  const db = requireDb(id);
  const tables: SchemaTable[] = db.tableNames().map((name) => {
    const t = db.table(name)!;
    const columns = t.columns.map((c) => ({
      name: c.name,
      type: displayColumnType(typeCanonicalName(c.type), c.decimal),
      notNull: c.notNull,
      primaryKey: c.primaryKey
    }));
    return {
      name: t.name,
      columns,
      primaryKey: t.pk.map((ord) => t.columns[ord]!.name),
      indexes: t.indexes.map((ix) => ({
        name: ix.name,
        columns: ix.columns.map((ord) => t.columns[ord]!.name),
        unique: ix.unique
      }))
    };
  });
  return { tables, types: schemaTypes(db), inTransaction: db.inTransaction() };
}

// schemaTypes lists the user-defined composite types in the visible snapshot. The committed snapshot
// is the read-side source; we read it via the public catalog by probing known names is not possible,
// so we reach the snapshot's sorted composite list directly through the read snapshot.
function schemaTypes(db: Database): SchemaResult['types'] {
  // committed is the public snapshot; under an open tx the working snapshot is what reads see, but
  // for the sidebar the committed view is sufficient and avoids reaching private state.
  const snap = db.committed;
  return snap.compositeTypesSorted().map((ct) => ({
    name: ct.name,
    fields: ct.fields.map((f) => ({ name: f.name, type: typeCanonicalName(f.type) }))
  }));
}

// --- OPFS file management (the tool's file manager) -------------------------------------------

function opfsRoot(): Promise<FileSystemDirectoryHandle> {
  if (typeof navigator === 'undefined' || !navigator.storage || !navigator.storage.getDirectory) {
    throw new EngineError('feature_not_supported', 'OPFS is not available in this browser');
  }
  return navigator.storage.getDirectory();
}

async function listFiles(): Promise<OpfsFileInfo[]> {
  const root = await opfsRoot();
  const files: OpfsFileInfo[] = [];
  for await (const [name, handle] of root.entries()) {
    if (handle.kind === 'file' && name.endsWith('.jed')) {
      const file = await (handle as FileSystemFileHandle).getFile();
      files.push({ name, size: file.size });
    }
  }
  files.sort((a, b) => a.name.localeCompare(b.name));
  return files;
}

async function exportBytes(name: string): Promise<Uint8Array> {
  const root = await opfsRoot();
  const handle = await root.getFileHandle(name);
  const file = await handle.getFile();
  return new Uint8Array(await file.arrayBuffer());
}

async function importBytes(name: string, bytes: Uint8Array, overwrite: boolean): Promise<void> {
  const root = await opfsRoot();
  if (!overwrite) {
    // Reject an existing name (mirrors create's 58P02), unless overwrite is requested.
    let exists = true;
    try {
      await root.getFileHandle(name);
    } catch {
      exists = false;
    }
    if (exists) throw new EngineError('duplicate_file', `database ${name} already exists`);
  }
  const handle = await root.getFileHandle(name, { create: true });
  // Write through a sync access handle (worker-only) — the portable path (Safari historically lacks
  // createWritable in OPFS). The same primitive the OPFS host uses (opfs.ts).
  const access = await handle.createSyncAccessHandle();
  try {
    access.truncate(0);
    access.write(bytes, { at: 0 });
    access.flush();
  } finally {
    access.close();
  }
}

async function deleteFile(name: string): Promise<void> {
  if (openOpfsNames.has(name)) {
    // Not an engine condition — a tool-level guard (surfaces as a clear message, XX000).
    throw new Error(`database ${name} is open; close it before deleting`);
  }
  const root = await opfsRoot();
  await root.removeEntry(name);
}

function opfsSupported(): boolean {
  return (
    typeof navigator !== 'undefined' &&
    !!navigator.storage &&
    typeof navigator.storage.getDirectory === 'function'
  );
}

// --- open / close ----------------------------------------------------------------------------

async function openDb(spec: OpenSpec): Promise<null> {
  if (dbs.has(spec.id)) throw new Error(`a database is already open for id ${spec.id}`);
  if (spec.mode === 'memory') {
    const db = new Database();
    applyHandleSettings(db, spec);
    dbs.set(spec.id, db);
    return null;
  }
  // OPFS mode.
  const name = spec.name;
  if (name === undefined) throw new Error('OPFS open requires a name');
  if (openOpfsNames.has(name)) {
    // Single-writer honesty (CLAUDE.md §3): one exclusive OPFS handle per file. Surfaced as a
    // clear message (XX000) rather than a stack trace.
    throw new Error(`database ${name} is already open in this tab`);
  }
  const db = spec.create
    ? await createOpfs(name, { pageSize: spec.pageSize })
    : await openOpfs(name, { readOnly: spec.readOnly, workMem: spec.workMem });
  applyHandleSettings(db, spec);
  dbs.set(spec.id, db);
  openOpfsNames.add(name);
  opfsNameOf.set(spec.id, name);
  return null;
}

function closeDb(id: string): null {
  const db = dbs.get(id);
  if (db === undefined) return null;
  const name = opfsNameOf.get(id);
  if (name !== undefined) {
    closeOpfs(db);
    openOpfsNames.delete(name);
    opfsNameOf.delete(id);
  }
  dbs.delete(id);
  return null;
}

// --- message loop ----------------------------------------------------------------------------

async function handle(req: Req): Promise<unknown> {
  switch (req.op) {
    case 'open':
      return openDb(req.spec);
    case 'run':
      return runBatch(req.id, req.sql);
    case 'commit':
      requireDb(req.id).commitTx();
      return null;
    case 'rollback':
      requireDb(req.id).rollbackTx();
      return null;
    case 'reset': {
      // In-memory re-seed: drop the db and create a fresh one under the same id (cheaper UX than
      // close+open for a live example's "reset"). OPFS dbs are not reset this way.
      closeDb(req.id);
      const db = new Database();
      dbs.set(req.id, db);
      return null;
    }
    case 'setMaxCost':
      requireDb(req.id).setMaxCost(BigInt(req.value));
      return null;
    case 'schema':
      return schemaOf(req.id);
    case 'close':
      return closeDb(req.id);
    case 'opfsSupported':
      return opfsSupported();
    case 'listFiles':
      return listFiles();
    case 'exportBytes':
      return exportBytes(req.name);
    case 'importBytes':
      return importBytes(req.name, req.bytes, req.overwrite);
    case 'deleteFile':
      return deleteFile(req.name);
  }
}

addEventListener('message', (ev: MessageEvent<Req>) => {
  const req = ev.data;
  handle(req).then(
    (result) => {
      // Transfer the bytes of an export so the (possibly large) file image doesn't get copied.
      const transfer = result instanceof Uint8Array ? [result.buffer as ArrayBuffer] : [];
      postMessage({ rid: req.rid, ok: true, result }, transfer);
    },
    (e: unknown) =>
      postMessage({
        rid: req.rid,
        ok: false,
        error: {
          code: e instanceof EngineError ? e.code() : 'XX000',
          message: e instanceof Error ? e.message : String(e)
        }
      })
  );
});
