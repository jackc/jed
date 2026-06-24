// bench-wasm benchmarks the Rust jed core compiled to wasm32-wasip1 (impl/wasm), driven from Node
// through WebAssembly + the node:wasi host — engine=jed, lang=wasm, variant=wrap. It runs the SAME
// corpus the native cores run, so the per-bench delta tells two stories:
//
//   * jed/wasm/wrap − jed/ts/core   = wasm-sandboxed Rust vs the native TypeScript core (the
//     question the wasm/js comparison actually asks).
//   * jed/wasm/wrap − jed/rust/core = the wasm tax over native Rust (sandbox + the marshalling
//     round-trip across the linear-memory boundary).
//
// Unlike the Ruby gem wrap (which has no prepared-statement API and re-parses each call), this wrap
// exposes jed_prepare / jed_stmt_query, so it mirrors the core benches' "parse once, run many"
// exactly — the comparison isolates execution, not parse overhead. Requires Node's WASI:
// `node --experimental-wasi-unstable-preview1`.

import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { WASI } from "node:wasi";

import { type Arg, type Checksum, type Engine, mainWith, readSidecar } from "./lib.ts";

// The optimized release artifact (Rakefile `wasm:build`). Compiled once; instantiated per open.
const WASM_PATH = fileURLToPath(
  new URL("../../../impl/wasm/target/wasm32-wasip1/release/jed_wasm.wasm", import.meta.url),
);
const wasmModule = new WebAssembly.Module(readFileSync(WASM_PATH));

const te = new TextEncoder();
const td = new TextDecoder();

// Result-buffer tags (impl/wasm/src/lib.rs wire format).
const TAG_ERROR = 0;
const TAG_STATEMENT = 1;
const TAG_QUERY = 2;
const TAG_HANDLE = 3;

interface WasmExports {
  memory: WebAssembly.Memory;
  jed_abi_version(): number;
  jed_alloc(len: number): number;
  jed_dealloc(ptr: number, len: number): void;
  jed_open_memory(): number;
  jed_create(path: number): number;
  jed_open(path: number, readOnly: number): number;
  jed_close(db: number): void;
  jed_free(ptr: number): void;
  jed_execute(db: number, sql: number): number;
  jed_prepare(db: number, sql: number): number;
  jed_stmt_query(stmt: number, db: number, params: number, paramsLen: number): number;
  jed_stmt_execute(stmt: number, db: number, params: number, paramsLen: number): number;
  jed_stmt_free(stmt: number): void;
}

type Cell = string | null;

interface QueryResult {
  kind: "query";
  rows: Cell[][];
}
interface StatementResult {
  kind: "statement";
}
type Parsed = QueryResult | StatementResult;

// A loaded wasm instance + the linear-memory marshalling helpers. wasm linear memory may grow on
// any call, which detaches the JS ArrayBuffer — so every accessor takes a FRESH view. Offsets
// (pointers) stay valid across growth (linear memory only appends pages).
class WasmJed {
  private readonly ex: WasmExports;

  constructor(instance: WebAssembly.Instance) {
    this.ex = instance.exports as unknown as WasmExports;
  }

  private u8(): Uint8Array {
    return new Uint8Array(this.ex.memory.buffer);
  }
  private dv(): DataView {
    return new DataView(this.ex.memory.buffer);
  }

  // Write a NUL-terminated C string into wasm memory.
  private cstr(s: string): { ptr: number; len: number } {
    const bytes = te.encode(s);
    const len = bytes.length + 1;
    const ptr = this.ex.jed_alloc(len);
    const mem = this.u8();
    mem.set(bytes, ptr);
    mem[ptr + bytes.length] = 0;
    return { ptr, len };
  }

  // Encode bind args into the param buffer (u32 nparams; per arg: u8 tag; payload). INT=1, TEXT=4.
  private params(args: Arg[]): { ptr: number; len: number } {
    let len = 4;
    for (const a of args) len += typeof a === "bigint" ? 9 : 5 + te.encode(a).length;
    const ptr = this.ex.jed_alloc(len);
    const d = this.dv();
    let p = ptr;
    d.setUint32(p, args.length, true);
    p += 4;
    for (const a of args) {
      if (typeof a === "bigint") {
        d.setUint8(p, 1);
        p += 1;
        d.setBigInt64(p, a, true);
        p += 8;
      } else {
        const b = te.encode(a);
        d.setUint8(p, 4);
        p += 1;
        d.setUint32(p, b.length, true);
        p += 4;
        this.u8().set(b, p);
        p += b.length;
      }
    }
    return { ptr, len };
  }

  // Parse a result buffer; throws on ERROR. Frees the buffer.
  private parse(rptr: number): Parsed {
    try {
      const d = this.dv();
      const tag = d.getUint8(rptr + 8);
      let p = rptr + 9;
      if (tag === TAG_ERROR) {
        const state = td.decode(new Uint8Array(this.ex.memory.buffer, p, 5)).trim();
        p += 5;
        const mlen = d.getUint32(p, true);
        p += 4;
        const msg = td.decode(new Uint8Array(this.ex.memory.buffer, p, mlen));
        throw new Error(`${state}: ${msg}`);
      }
      if (tag === TAG_STATEMENT || tag === TAG_HANDLE) {
        return { kind: "statement" };
      }
      if (tag === TAG_QUERY) {
        const ncols = d.getUint32(p, true);
        p += 4;
        const nrows = d.getUint32(p, true);
        p += 4;
        const rows: Cell[][] = [];
        for (let r = 0; r < nrows; r++) {
          const row: Cell[] = [];
          for (let c = 0; c < ncols; c++) {
            const isNull = d.getUint8(p);
            p += 1;
            if (isNull) {
              row.push(null);
            } else {
              const l = d.getUint32(p, true);
              p += 4;
              row.push(td.decode(new Uint8Array(this.ex.memory.buffer, p, l)));
              p += l;
            }
          }
          rows.push(row);
        }
        return { kind: "query", rows };
      }
      throw new Error(`unexpected result tag ${tag}`);
    } finally {
      this.ex.jed_free(rptr);
    }
  }

  // A HANDLE buffer carries a u64 pointer; read it before freeing.
  private handle(rptr: number): number {
    const d = this.dv();
    const tag = d.getUint8(rptr + 8);
    if (tag === TAG_ERROR) {
      try {
        return this.parse(rptr) as never;
      } finally {
        // parse() throws on error and frees; unreachable return keeps types happy.
      }
    }
    const ptr = Number(d.getBigUint64(rptr + 9, true));
    this.ex.jed_free(rptr);
    return ptr;
  }

  abiVersion(): number {
    return this.ex.jed_abi_version();
  }

  openMemory(): number {
    return this.handle(this.ex.jed_open_memory());
  }
  createFile(path: string): number {
    const { ptr, len } = this.cstr(path);
    try {
      return this.handle(this.ex.jed_create(ptr));
    } finally {
      this.ex.jed_dealloc(ptr, len);
    }
  }
  openFile(path: string, readOnly: boolean): number {
    const { ptr, len } = this.cstr(path);
    try {
      return this.handle(this.ex.jed_open(ptr, readOnly ? 1 : 0));
    } finally {
      this.ex.jed_dealloc(ptr, len);
    }
  }
  closeDb(db: number): void {
    this.ex.jed_close(db);
  }

  execute(db: number, sql: string): Parsed {
    const { ptr, len } = this.cstr(sql);
    try {
      return this.parse(this.ex.jed_execute(db, ptr));
    } finally {
      this.ex.jed_dealloc(ptr, len);
    }
  }
  prepare(db: number, sql: string): number {
    const { ptr, len } = this.cstr(sql);
    try {
      return this.handle(this.ex.jed_prepare(db, ptr));
    } finally {
      this.ex.jed_dealloc(ptr, len);
    }
  }
  stmtQuery(stmt: number, db: number, args: Arg[]): Cell[][] {
    const { ptr, len } = this.params(args);
    try {
      const res = this.parse(this.ex.jed_stmt_query(stmt, db, ptr, len));
      if (res.kind !== "query") throw new Error("expected a query result");
      return res.rows;
    } finally {
      this.ex.jed_dealloc(ptr, len);
    }
  }
  stmtExecute(stmt: number, db: number, args: Arg[]): void {
    const { ptr, len } = this.params(args);
    try {
      this.parse(this.ex.jed_stmt_execute(stmt, db, ptr, len));
    } finally {
      this.ex.jed_dealloc(ptr, len);
    }
  }
  stmtFree(stmt: number): void {
    this.ex.jed_stmt_free(stmt);
  }
}

// Instantiate the module under a WASI host that preopens the data dir as /data (so a .jed file is
// reachable as a WASI path) — one fresh instance per open, for clean isolation.
function instantiate(dataDir: string): WasmJed {
  const wasi = new WASI({ version: "preview1", args: [], env: {}, preopens: { "/data": dataDir } });
  const instance = new WebAssembly.Instance(wasmModule, wasi.getImportObject());
  wasi.initialize(instance);
  return new WasmJed(instance);
}

class WasmEngine implements Engine {
  private readonly w: WasmJed;
  private readonly db: number;
  private readonly dataDir: string;
  private readonly dataset: string;
  private readonly scratch: string | null;
  private stmt: number | null = null;

  constructor(w: WasmJed, db: number, dataDir: string, dataset: string, scratch: string | null) {
    this.w = w;
    this.db = db;
    this.dataDir = dataDir;
    this.dataset = dataset;
    this.scratch = scratch;
  }

  async exec(sql: string): Promise<void> {
    this.w.execute(this.db, sql);
  }

  async prepare(sql: string): Promise<void> {
    if (this.stmt !== null) this.w.stmtFree(this.stmt);
    this.stmt = this.w.prepare(this.db, sql);
  }

  async queryPrepared(args: Arg[], sum: Checksum | null): Promise<number> {
    const rows = this.w.stmtQuery(this.stmt!, this.db, args);
    if (sum !== null) {
      for (const row of rows) {
        for (const v of row) {
          // A rendered cell hashes byte-identically whether it came from an int or text value
          // (the FNV int/text paths share the 0x1f separator and `render()` == `toString()` for
          // integers) — so feeding the render reproduces the cross-engine answer checksum.
          if (v === null) sum.null();
          else sum.text(v);
        }
        sum.endRow();
      }
    }
    return rows.length;
  }

  async execPrepared(args: Arg[]): Promise<void> {
    this.w.stmtExecute(this.stmt!, this.db, args);
  }

  async queryInt(sql: string): Promise<bigint> {
    const res = this.w.execute(this.db, sql);
    if (res.kind === "query" && res.rows.length > 0 && res.rows[0][0] !== null) {
      return BigInt(res.rows[0][0] as string);
    }
    throw new Error(`expected one integer from ${sql}`);
  }

  async storedFingerprint(): Promise<string> {
    return readSidecar(this.dataDir, this.dataset, "jed");
  }

  async close(): Promise<void> {
    if (this.stmt !== null) this.w.stmtFree(this.stmt);
    this.w.closeDb(this.db);
    if (this.scratch !== null) rmSync(this.scratch, { recursive: true, force: true });
  }
}

// Map an absolute path under dataDir to its WASI path beneath the /data preopen.
function wasiPath(dataDir: string, abs: string): string {
  return `/data/${relative(dataDir, abs).split(/[\\/]/).join("/")}`;
}

await mainWith({
  engine: "jed",
  lang: "wasm",
  variant: "wrap",
  async open(dataDir: string, dataset: string): Promise<Engine> {
    const w = instantiate(dataDir);
    if (dataset === "scratch") {
      const dir = mkdtempSync(join(dataDir, "scratch-wasm-"));
      const db = w.createFile(wasiPath(dataDir, join(dir, "scratch.jed")));
      return new WasmEngine(w, db, dataDir, dataset, dir);
    }
    const db = w.openFile(wasiPath(dataDir, join(dataDir, `${dataset}.jed`)), false);
    return new WasmEngine(w, db, dataDir, dataset, null);
  },
});
