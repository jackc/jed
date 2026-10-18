// Display facade for the shared typed browser runtime. Each database owns an
// execution worker; examples and the tool use identical session/cursor lifecycles.
import {
  createDatabase,
  openDatabase,
  type AsyncDatabase,
  type AsyncSession
} from '$jed/browser/async.ts';
import { EngineError } from '$jed/errors.ts';
import { render } from '$jed/value.ts';
import { splitSql } from './split.ts';
import type {
  DbMode,
  FileReply,
  FileRequest,
  OpfsFileInfo,
  RunResult,
  SchemaResult
} from './protocol.ts';

export const DEFAULT_MEMORY_MAX_COST = 5_000_000n;
// The types reference currently mounts 37 independent live examples. Keep the
// site's explicit bound above that page while the general library default is 32.
const WEBSITE_RUNTIME = { maxWorkers: 64 };

export class JedError extends Error {
  constructor(
    readonly code: string,
    message: string
  ) {
    super(message);
    this.name = 'JedError';
  }
}

async function displayed<T>(operation: () => Promise<T>): Promise<T> {
  try {
    return await operation();
  } catch (error) {
    if (error instanceof EngineError) throw new JedError(error.code(), error.message);
    throw error;
  }
}

function commandTag(sql: string, count: number | null): string {
  // This is only a display label. The engine parses and validates the statement in
  // its worker; no parser, planning, or database state is copied onto the UI thread.
  let start = 0;
  while (start < sql.length) {
    if (/\s/.test(sql[start]!)) {
      start++;
      continue;
    }
    if (sql.startsWith('--', start)) {
      const end = sql.indexOf('\n', start + 2);
      start = end < 0 ? sql.length : end + 1;
      continue;
    }
    if (sql.startsWith('/*', start)) {
      start += 2;
      let depth = 1;
      while (start < sql.length && depth > 0) {
        if (sql.startsWith('/*', start)) {
          depth++;
          start += 2;
        } else if (sql.startsWith('*/', start)) {
          depth--;
          start += 2;
        } else start++;
      }
      continue;
    }
    break;
  }
  const words =
    sql
      .slice(start)
      .toUpperCase()
      .match(/^[A-Z]+(?:\s+[A-Z]+){0,4}/)?.[0]
      .split(/\s+/) ?? [];
  switch (words[0]) {
    case 'INSERT':
      return `INSERT 0 ${count ?? 0}`;
    case 'UPDATE':
      return `UPDATE ${count ?? 0}`;
    case 'DELETE':
      return `DELETE ${count ?? 0}`;
    case 'BEGIN':
    case 'COMMIT':
    case 'ROLLBACK':
      return words[0];
    case 'CREATE':
    case 'DROP':
    case 'ALTER': {
      const object = words
        .slice(1)
        .find((word) => ['TABLE', 'INDEX', 'TYPE', 'VIEW'].includes(word));
      return object === undefined ? 'OK' : `${words[0]} ${object}`;
    }
    default:
      return 'OK';
  }
}

const openNames = new Set<string>();
let nextDbSeq = 1;
type Settings = { maxCost?: bigint; workMem?: number };

export class JedDb {
  readonly id = `db-${nextDbSeq++}`;
  private closed = false;

  constructor(
    private database: AsyncDatabase,
    private session: AsyncSession,
    readonly mode: DbMode,
    private settings: Settings,
    private readonly name?: string
  ) {}

  run(sql: string): Promise<RunResult[]> {
    return displayed(async () => {
      const results: RunResult[] = [];
      // A script keeps the editor's existing autocommit policy: each statement
      // commits separately unless SQL explicitly opens a transaction.
      for (const statement of splitSql(sql)) {
        const cursor = await this.session.query(statement.sql);
        try {
          const rows: string[][] = [];
          for await (const row of cursor) rows.push(row.map(render));
          results.push(
            cursor.columnNames.length > 0
              ? {
                  kind: 'query',
                  columnNames: cursor.columnNames,
                  columnTypes: cursor.columnTypes,
                  rows,
                  rowCount: rows.length,
                  cost: cursor.cost.toString()
                }
              : {
                  kind: 'statement',
                  rowsAffected: cursor.rowsAffected,
                  cost: cursor.cost.toString(),
                  tag: commandTag(statement.sql, cursor.rowsAffected)
                }
          );
        } finally {
          await cursor.close();
        }
      }
      return results;
    });
  }

  schema(): Promise<SchemaResult> {
    return displayed(async () => {
      const tables = await this.session.all('SELECT name FROM jed_tables ORDER BY name');
      const columns = await this.session.all(
        'SELECT table_name, name, type, not_null, pk_ordinal FROM jed_columns ORDER BY table_name, ordinal'
      );
      const indexes: { table: string; name: string; columns: string[]; unique: boolean }[] = [];
      const indexRows = await this.session.query(
        'SELECT table_name, name, columns, is_unique FROM jed_indexes ORDER BY table_name, name'
      );
      try {
        for await (const row of indexRows) {
          indexes.push({
            table: render(row[0]!),
            name: render(row[1]!),
            columns: row[2]?.kind === 'array' ? row[2].elements.map(render) : [],
            unique: row[3]?.kind === 'bool' && row[3].value
          });
        }
      } finally {
        await indexRows.close();
      }
      const types = await this.session.all('SELECT name FROM jed_types ORDER BY name');
      const fields = await this.session.all(
        'SELECT type_name, name, type FROM jed_type_fields ORDER BY type_name, ordinal'
      );
      return {
        tables: tables.map((table) => {
          const cols = columns.filter((column) => column.table_name === table.name);
          return {
            name: String(table.name),
            columns: cols.map((column) => ({
              name: String(column.name),
              type: String(column.type),
              notNull: column.not_null === true,
              primaryKey: column.pk_ordinal !== null
            })),
            primaryKey: cols
              .filter((column) => column.pk_ordinal !== null)
              .sort((a, b) => Number(a.pk_ordinal) - Number(b.pk_ordinal))
              .map((column) => String(column.name)),
            indexes: indexes
              .filter((index) => index.table === table.name)
              .map((index) => ({
                name: index.name,
                columns: index.columns,
                unique: index.unique
              }))
          };
        }),
        types: types.map((type) => ({
          name: String(type.name),
          fields: fields
            .filter((field) => field.type_name === type.name)
            .map((field) => ({ name: String(field.name), type: String(field.type) }))
        })),
        inTransaction: (await this.session.status()).inTransaction
      };
    });
  }

  async reset(): Promise<void> {
    if (this.mode !== 'memory') throw new JedError('XX000', 'only in-memory examples can be reset');
    if (this.closed) throw new JedError('XX000', 'database is closed');
    await this.database.close();
    this.database = await createDatabase({ runtime: WEBSITE_RUNTIME });
    try {
      this.session = await this.database.session(this.settings);
    } catch (error) {
      await this.database.close();
      throw error;
    }
  }

  async setMaxCost(value: bigint): Promise<void> {
    await displayed(() => this.session.configure({ maxCost: value }));
    this.settings = { ...this.settings, maxCost: value };
  }

  async commit(): Promise<void> {
    await displayed(() => this.session.execute('COMMIT'));
  }

  async rollback(): Promise<void> {
    await displayed(() => this.session.execute('ROLLBACK'));
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    try {
      await this.database.close();
    } finally {
      if (this.name !== undefined) openNames.delete(this.name);
    }
  }
}

async function wrap(
  database: AsyncDatabase,
  mode: DbMode,
  settings: Settings,
  name?: string
): Promise<JedDb> {
  try {
    return new JedDb(database, await database.session(settings), mode, settings, name);
  } catch (error) {
    await database.close();
    throw error;
  }
}

export async function openMemory(options: Settings = {}): Promise<JedDb> {
  const settings = { ...options, maxCost: options.maxCost ?? DEFAULT_MEMORY_MAX_COST };
  return displayed(async () =>
    wrap(await createDatabase({ runtime: WEBSITE_RUNTIME }), 'memory', settings)
  );
}

async function openFile(
  name: string,
  create: boolean,
  options: Settings & { pageSize?: number; readOnly?: boolean }
): Promise<JedDb> {
  if (openNames.has(name))
    throw new JedError('XX000', `database ${name} is already open in this tab`);
  openNames.add(name); // Reserve before async acquisition, including simultaneous opens.
  try {
    return await displayed(async () => {
      const database = create
        ? await createDatabase({ path: name, pageSize: options.pageSize, runtime: WEBSITE_RUNTIME })
        : await openDatabase(name, {
            readOnly: options.readOnly,
            workMem: options.workMem,
            runtime: WEBSITE_RUNTIME
          });
      return wrap(database, 'opfs', options, name);
    });
  } catch (error) {
    openNames.delete(name);
    throw error;
  }
}

export function createOpfsDb(
  name: string,
  options: Settings & { pageSize?: number } = {}
): Promise<JedDb> {
  return openFile(name, true, options);
}

export function openOpfsDb(
  name: string,
  options: Settings & { readOnly?: boolean } = {}
): Promise<JedDb> {
  return openFile(name, false, options);
}

// The remaining utility worker only lists/imports/exports closed OPFS files. It
// contains no SQL engine and cannot diverge from the shared runtime scheduler.
type FileBody = FileRequest extends infer R
  ? R extends { rid: number }
    ? Omit<R, 'rid'>
    : never
  : never;
let fileWorker: Worker | null = null;
let nextRid = 1;
const pending = new Map<number, { resolve(value: unknown): void; reject(error: unknown): void }>();

function ensureFileWorker(): Worker {
  if (fileWorker !== null) return fileWorker;
  const worker = new Worker(new URL('./worker.ts', import.meta.url), { type: 'module' });
  worker.addEventListener('message', (event: MessageEvent<FileReply>) => {
    const reply = event.data;
    const request = pending.get(reply.rid);
    if (request === undefined) return;
    pending.delete(reply.rid);
    if (reply.ok) request.resolve(reply.result);
    else request.reject(new JedError(reply.error.code, reply.error.message));
  });
  const fail = () => {
    worker.terminate();
    fileWorker = null;
    for (const request of pending.values()) request.reject(new Error('OPFS file worker failed'));
    pending.clear();
  };
  worker.addEventListener('error', fail);
  worker.addEventListener('messageerror', fail);
  fileWorker = worker;
  return worker;
}

function fileCall(body: FileBody): Promise<unknown> {
  if (pending.size >= 32) return Promise.reject(new Error('OPFS file-manager queue is full'));
  const worker = ensureFileWorker();
  const rid = nextRid++;
  return new Promise((resolve, reject) => {
    pending.set(rid, { resolve, reject });
    try {
      worker.postMessage(
        { rid, ...body },
        body.op === 'importBytes' ? [body.bytes.buffer as ArrayBuffer] : []
      );
    } catch (error) {
      pending.delete(rid);
      reject(error);
    }
  });
}

export function opfsSupported(): Promise<boolean> {
  return fileCall({ op: 'opfsSupported' }) as Promise<boolean>;
}
export function listOpfsFiles(): Promise<OpfsFileInfo[]> {
  return fileCall({ op: 'listFiles' }) as Promise<OpfsFileInfo[]>;
}
export function exportOpfsFile(name: string): Promise<Uint8Array> {
  return fileCall({ op: 'exportBytes', name }) as Promise<Uint8Array>;
}
export function importOpfsFile(name: string, bytes: Uint8Array, overwrite: boolean): Promise<void> {
  if (openNames.has(name))
    return Promise.reject(
      new JedError('XX000', `database ${name} is open; close it before importing`)
    );
  return fileCall({ op: 'importBytes', name, bytes, overwrite }) as Promise<void>;
}
export function deleteOpfsFile(name: string): Promise<void> {
  if (openNames.has(name))
    return Promise.reject(
      new JedError('XX000', `database ${name} is open; close it before deleting`)
    );
  return fileCall({ op: 'deleteFile', name }) as Promise<void>;
}
