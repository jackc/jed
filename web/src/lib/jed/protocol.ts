// Website display models. Database requests use the shared typed runtime;
// only OPFS file-manager operations have a site-owned worker protocol.
export type DbMode = 'memory' | 'opfs';

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

export type FileRequest =
  | { rid: number; op: 'exportBytes'; name: string }
  | { rid: number; op: 'importBytes'; name: string; bytes: Uint8Array; overwrite: boolean }
  | { rid: number; op: 'listFiles' }
  | { rid: number; op: 'deleteFile'; name: string }
  | { rid: number; op: 'opfsSupported' };

export type FileReply =
  | { rid: number; ok: true; result: unknown }
  | { rid: number; ok: false; error: WireError };
