// Experimental Node.js API over the safe Rust core. The native boundary uses compact buffers;
// callers see ordinary bigint/string/null cells and never handle the wire format directly.

import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

interface NativeDatabaseHandle {
  execute(sql: string, params: Buffer): void;
  query(sql: string, params: Buffer): Buffer;
  prepare(sql: string): number;
  executePrepared(statement: number, params: Buffer): void;
  queryPrepared(statement: number, params: Buffer): Buffer;
  freeStatement(statement: number): void;
  close(): void;
}

interface NativeBinding {
  abiVersion(): number;
  NativeDatabase: {
    open(path: string): NativeDatabaseHandle;
    create(path: string): NativeDatabaseHandle;
  };
  benchConcurrentRead(
    path: string,
    sql: string,
    warm: Buffer,
    measured: Buffer,
    expectedRows: number,
  ): Buffer;
}

const native = require("./jed_node.node") as NativeBinding;
if (native.abiVersion() !== 1)
  throw new Error(`unsupported @jed/node-rust ABI ${native.abiVersion()}`);

export type BindValue = bigint | string | null;
export type Cell = bigint | string | null;
export type Row = Cell[];

function encodeValueInto(parts: Buffer[], value: BindValue): void {
  if (value === null) {
    parts.push(Buffer.from([0]));
  } else if (typeof value === "bigint") {
    const part = Buffer.allocUnsafe(9);
    part[0] = 1;
    part.writeBigInt64LE(value, 1);
    parts.push(part);
  } else {
    const text = Buffer.from(value, "utf8");
    const head = Buffer.allocUnsafe(5);
    head[0] = 4;
    head.writeUInt32LE(text.length, 1);
    parts.push(head, text);
  }
}

export function encodeParams(values: readonly BindValue[]): Buffer {
  const head = Buffer.allocUnsafe(4);
  head.writeUInt32LE(values.length, 0);
  const parts = [head];
  for (const value of values) encodeValueInto(parts, value);
  return Buffer.concat(parts);
}

function decodeRows(buffer: Buffer): Row[] {
  let pos = 0;
  const columns = buffer.readUInt32LE(pos);
  pos += 4;
  const count = buffer.readUInt32LE(pos);
  pos += 4;
  const rows: Row[] = [];
  for (let r = 0; r < count; r++) {
    const row: Row = [];
    for (let c = 0; c < columns; c++) {
      const tag = buffer[pos++];
      if (tag === 0) {
        row.push(null);
      } else if (tag === 1) {
        row.push(buffer.readBigInt64LE(pos));
        pos += 8;
      } else if (tag === 4) {
        const length = buffer.readUInt32LE(pos);
        pos += 4;
        row.push(buffer.toString("utf8", pos, pos + length));
        pos += length;
      } else {
        throw new Error(`unknown @jed/node-rust result tag ${tag}`);
      }
    }
    rows.push(row);
  }
  if (pos !== buffer.length) throw new Error("trailing bytes in @jed/node-rust result");
  return rows;
}

export class PreparedStatement {
  readonly #database: Database;
  readonly #handle: number;
  #closed = false;

  constructor(database: Database, handle: number) {
    this.#database = database;
    this.#handle = handle;
  }

  query(params: readonly BindValue[] = []): Row[] {
    if (this.#closed) throw new Error("prepared statement is closed");
    return this.#database.queryPrepared(this.#handle, params);
  }

  execute(params: readonly BindValue[] = []): void {
    if (this.#closed) throw new Error("prepared statement is closed");
    this.#database.executePrepared(this.#handle, params);
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#database.freeStatement(this.#handle);
  }
}

export class Database {
  readonly #native: NativeDatabaseHandle;
  #closed = false;

  private constructor(handle: NativeDatabaseHandle) {
    this.#native = handle;
  }

  static open(path: string): Database {
    return new Database(native.NativeDatabase.open(path));
  }

  static create(path: string): Database {
    return new Database(native.NativeDatabase.create(path));
  }

  execute(sql: string, params: readonly BindValue[] = []): void {
    this.assertOpen();
    this.#native.execute(sql, encodeParams(params));
  }

  query(sql: string, params: readonly BindValue[] = []): Row[] {
    this.assertOpen();
    return decodeRows(this.#native.query(sql, encodeParams(params)));
  }

  prepare(sql: string): PreparedStatement {
    this.assertOpen();
    return new PreparedStatement(this, this.#native.prepare(sql));
  }

  queryPrepared(handle: number, params: readonly BindValue[]): Row[] {
    this.assertOpen();
    return decodeRows(this.#native.queryPrepared(handle, encodeParams(params)));
  }

  executePrepared(handle: number, params: readonly BindValue[]): void {
    this.assertOpen();
    this.#native.executePrepared(handle, encodeParams(params));
  }

  freeStatement(handle: number): void {
    if (!this.#closed) this.#native.freeStatement(handle);
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#native.close();
  }

  private assertOpen(): void {
    if (this.#closed) throw new Error("database is closed");
  }
}

function encodeBlocks(blocks: readonly (readonly (readonly BindValue[])[])[]): Buffer {
  const parts: Buffer[] = [];
  const blockHead = Buffer.allocUnsafe(4);
  blockHead.writeUInt32LE(blocks.length, 0);
  parts.push(blockHead);
  for (const block of blocks) {
    const queryHead = Buffer.allocUnsafe(4);
    queryHead.writeUInt32LE(block.length, 0);
    parts.push(queryHead);
    for (const params of block) parts.push(encodeParams(params));
  }
  return Buffer.concat(parts);
}

export interface ConcurrentResult {
  blockHexes: string[];
  elapsed: bigint[];
  rowsTotal: number;
  wallNs: bigint;
}

/** Internal benchmark bridge: runs the Rust core's real threaded reader path. */
export function benchConcurrentRead(
  path: string,
  sql: string,
  warm: readonly (readonly (readonly BindValue[])[])[],
  measured: readonly (readonly (readonly BindValue[])[])[],
  expectedRows: number,
): ConcurrentResult {
  const result = native.benchConcurrentRead(
    path,
    sql,
    encodeBlocks(warm),
    encodeBlocks(measured),
    expectedRows,
  );
  let pos = 0;
  const blockCount = result.readUInt32LE(pos);
  pos += 4;
  const blockHexes: string[] = [];
  for (let i = 0; i < blockCount; i++) {
    blockHexes.push(result.readBigUInt64LE(pos).toString(16).padStart(16, "0"));
    pos += 8;
  }
  const elapsedCount = result.readUInt32LE(pos);
  pos += 4;
  const elapsed: bigint[] = [];
  for (let i = 0; i < elapsedCount; i++) {
    elapsed.push(result.readBigInt64LE(pos));
    pos += 8;
  }
  const rowsTotalBig = result.readBigUInt64LE(pos);
  pos += 8;
  const wallNs = result.readBigUInt64LE(pos);
  pos += 8;
  if (pos !== result.length) throw new Error("trailing bytes in concurrent result");
  const rowsTotal = Number(rowsTotalBig);
  if (!Number.isSafeInteger(rowsTotal))
    throw new Error("concurrent row count exceeds JS precision");
  return { blockHexes, elapsed, rowsTotal, wallNs };
}
