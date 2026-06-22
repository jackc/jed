// The wire protocol between the main thread (session.ts) and the engine Web Worker (worker.ts).
// The engine and storage seam are synchronous; the ONLY async boundary is the postMessage RPC to
// the worker (spec/design/hosts.md §5). The worker renders Values to strings and bigints to decimal
// strings because Value carries class instances (Decimal, …) that structuredClone cannot preserve —
// this mirrors the canonical CLI/conformance display form (cli/src/render.rs).
//
// This site-owned worker generalizes impl/ts/src/browser/{worker,client}.ts in three ways the
// website needs: it holds MANY databases keyed by id (one worker, loaded once, for every live
// example + the tool), supports an in-memory mode as well as OPFS, and adds a schema-introspection
// op for the tool's sidebar. The core's tested OPFS host is left untouched.

export type DbMode = 'memory' | 'opfs';

// OpenSpec opens or creates a database under a caller-chosen id. For 'memory' a fresh Database; for
// 'opfs' an OPFS-backed file named `name` (create => 58P02 if it exists; else open => 58P01 if
// absent). maxCost/workMem are applied to the handle (untrusted-query ceiling, CLAUDE.md §13).
export type OpenSpec = {
  id: string;
  mode: DbMode;
  name?: string;
  create?: boolean;
  maxCost?: string; // i64 ceiling as a decimal string; 0/absent = unlimited
  workMem?: number;
  pageSize?: number;
  readOnly?: boolean;
};

// RunResult is one statement's serialized outcome: a query's rendered rows + column names/types, or
// a non-query statement's affected-row count and a command tag. cost is a decimal string (bigint).
export type RunResult =
  | {
      kind: 'query';
      columnNames: string[];
      columnTypes: string[];
      rows: string[][];
      rowCount: number;
      cost: string;
    }
  | { kind: 'statement'; rowsAffected: number | null; cost: string; tag: string };

// SchemaColumn / SchemaTable / SchemaResult describe the visible catalog for the tool's sidebar,
// flattened to display strings (the Type objects are not structured-clone-friendly).
export type SchemaColumn = { name: string; type: string; notNull: boolean; primaryKey: boolean };
export type SchemaIndex = { name: string; columns: string[]; unique: boolean };
export type SchemaTable = {
  name: string;
  columns: SchemaColumn[];
  primaryKey: string[]; // column names in key order
  indexes: SchemaIndex[];
};
export type SchemaType = { name: string; fields: { name: string; type: string }[] };
export type SchemaResult = { tables: SchemaTable[]; types: SchemaType[]; inTransaction: boolean };

// OpfsFileInfo describes one stored database file (the tool's OPFS file manager).
export type OpfsFileInfo = { name: string; size: number };

// WireError mirrors an engine EngineError across the worker boundary: the SQLSTATE plus its message,
// so api.md §5/§7's structured-error contract survives postMessage.
export type WireError = { code: string; message: string };

// Req is the client→worker request envelope (every message carries a unique rid the reply echoes).
export type Req =
  | { rid: number; op: 'open'; spec: OpenSpec }
  | { rid: number; op: 'run'; id: string; sql: string }
  | { rid: number; op: 'commit'; id: string }
  | { rid: number; op: 'rollback'; id: string }
  | { rid: number; op: 'reset'; id: string } // memory only: replace with a fresh Database
  | { rid: number; op: 'setMaxCost'; id: string; value: string } // i64 ceiling as decimal string
  | { rid: number; op: 'schema'; id: string }
  | { rid: number; op: 'close'; id: string }
  | { rid: number; op: 'exportBytes'; name: string } // OPFS: read a (closed) file out for download
  | { rid: number; op: 'importBytes'; name: string; bytes: Uint8Array; overwrite: boolean }
  | { rid: number; op: 'listFiles' } // OPFS: list .jed files at the origin root
  | { rid: number; op: 'deleteFile'; name: string }
  | { rid: number; op: 'opfsSupported' };

// Reply is the worker→client response envelope.
export type Reply =
  | { rid: number; ok: true; result: unknown }
  | { rid: number; ok: false; error: WireError };
