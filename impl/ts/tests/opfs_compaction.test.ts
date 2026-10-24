// OPFS compaction and rekey (spec/design/hosts.md §5.1): the OPFS host replaces a database by writing
// its from-scratch image to a sibling journal file, committing it with a durable trailer, and copying
// it over the database in place; an open that finds a complete journal finishes the copy. Host-API and
// byte-level coverage the corpus cannot reach (CLAUDE.md §10), run in Node over fake sync access
// handles with injected failures: the image equals the Node file host's compaction byte for byte, a
// failure before the trailer leaves the database untouched, and one during the copy is finished by
// the next open. The real-browser path is e2e/opfs.spec.ts.

import "../src/crypto_node.ts";

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";

import type { Credential } from "../src/crypto.ts";
import { Environment } from "../src/environment.ts";
import { ioSteps, runSync } from "../src/execution.ts";
import { createDatabase, Database, intValue, type Value } from "../src/lib.ts";
import { createOpfsWithHandle, openOpfsWithHandle } from "../src/opfs.ts";
import type { SyncAccessHandle } from "../src/opfsblockstore.ts";
import { discardJournalSteps, type OpfsJournal } from "../src/opfsrewrite.ts";
import { bytesEqual, errCode } from "./util.ts";

const PAGE = 1024;
const KEY: Credential = { key: new Uint8Array(32).fill(0x4a) };
const SEED = 0x4f50_4653n;

const scratch = mkdtempSync(join(tmpdir(), "jed-opfs-compact-"));
after(() => rmSync(scratch, { recursive: true, force: true }));

// FaultHandle is an in-memory FileSystemSyncAccessHandle whose writes and flushes can be made to fail
// after a count, standing in for a crash: every byte written before the failure stays (the fake has
// no volatile cache), which is the harder case for recovery.
class FaultHandle implements SyncAccessHandle {
  private buf = new Uint8Array(0);
  closed = false;
  failWriteAfter = Infinity;
  failFlushAfter = Infinity;
  writes = 0;
  flushes = 0;

  constructor(initial?: Uint8Array) {
    if (initial !== undefined) this.buf = initial.slice();
  }

  read(buffer: Uint8Array, opts: { at: number }): number {
    const n = Math.max(0, Math.min(buffer.length, this.buf.length - opts.at));
    buffer.set(this.buf.subarray(opts.at, opts.at + n));
    return n;
  }

  write(buffer: Uint8Array, opts: { at: number }): number {
    if (this.writes++ >= this.failWriteAfter) {
      // A torn write: its first half lands.
      this.failWriteAfter = Infinity;
      this.write(buffer.subarray(0, buffer.length >> 1), opts);
      this.failWriteAfter = 0;
      throw new Error("injected write failure");
    }
    const end = opts.at + buffer.length;
    if (end > this.buf.length) this.resize(end);
    this.buf.set(buffer, opts.at);
    return buffer.length;
  }

  truncate(newSize: number): void {
    this.resize(newSize);
  }

  private resize(newSize: number): void {
    const next = new Uint8Array(newSize);
    next.set(this.buf.subarray(0, Math.min(newSize, this.buf.length)));
    this.buf = next;
  }

  getSize(): number {
    return this.buf.length;
  }

  flush(): void {
    if (this.flushes++ >= this.failFlushAfter) throw new Error("injected flush failure");
  }

  close(): void {
    this.closed = true;
  }

  bytes(): Uint8Array {
    return this.buf.slice();
  }
}

// Dir is a fake OPFS directory holding journal files, reached synchronously (the browser's is async).
class Dir {
  files = new Map<string, FaultHandle>();
  // next, when set, is the handle the next created journal gets (to inject its failures).
  next: FaultHandle | null = null;

  journal(name: string): OpfsJournal {
    const jname = name + ".jedcompact";
    return {
      acquire: (create) =>
        ioSteps(() => {
          let h = this.files.get(jname);
          if (h === undefined) {
            if (!create) return null;
            h = this.next ?? new FaultHandle();
            this.next = null;
            this.files.set(jname, h);
          }
          h.closed = false;
          return h;
        }),
      discard: (handle) =>
        ioSteps(() => {
          handle.close();
          this.files.delete(jname);
        }),
    };
  }
}

function seeded(): Environment {
  return Environment.builder().insecureSeededKeyEntropy(SEED).build();
}

// WORKLOAD leaves most pages dead: a multi-level tree with a secondary and a GiST index, overflow
// chains, and a deletion of most rows.
const WORKLOAD: string[] = [
  "CREATE TABLE t (id i64 PRIMARY KEY, pad text, n i64, blob bytea, span i32range)",
  "CREATE INDEX t_n ON t (n)",
  "CREATE INDEX t_span ON t USING gist (span)",
  "INSERT INTO t SELECT g, repeat('x', 200), g % 7, NULL, i32range(g, g + 3) FROM generate_series(1, 1500) g",
  "UPDATE t SET blob = decode(repeat(md5(id::text), 80), 'hex') WHERE id % 50 = 0",
  "DELETE FROM t WHERE id > 120",
];
const PROBE =
  "SELECT count(*), sum(id), sum(length(pad)), sum(length(blob)), count(*) FILTER (WHERE span && i32range(10, 20)) FROM t";

function rows(db: Database, sql: string): Value[][] {
  const s = db.session();
  try {
    const r = s.query(sql);
    try {
      return [...r];
    } finally {
      r.close();
    }
  } finally {
    s.close();
  }
}

function run(db: Database, stmts: string[]): void {
  const s = db.session();
  try {
    for (const sql of stmts) s.execute(sql);
  } finally {
    s.close();
  }
}

// fileControl runs WORKLOAD on a Node file database, applies after, and returns its bytes and rows.
function fileControl(
  name: string,
  credential: Credential | undefined,
  after: (db: Database) => void,
): { bytes: Uint8Array; rows: Value[][] } {
  const path = join(scratch, name);
  const db = createDatabase({
    path,
    pageSize: PAGE,
    skipFsync: true,
    environment: seeded(),
    credential,
    locking: "none",
  });
  run(db, WORKLOAD);
  after(db);
  const got = rows(db, PROBE);
  db.close();
  return { bytes: new Uint8Array(readFileSync(path)), rows: got };
}

// opfsDb creates an OPFS database over main with dir's journal and runs WORKLOAD.
function opfsDb(main: FaultHandle, dir: Dir, credential?: Credential): Database {
  const db = Database.fromEngine(
    createOpfsWithHandle(
      main,
      { pageSize: PAGE, environment: seeded(), credential },
      dir.journal("db"),
    ),
  );
  run(db, WORKLOAD);
  return db;
}

function reopen(main: FaultHandle, dir: Dir, credential?: Credential): Database {
  return Database.fromEngine(openOpfsWithHandle(main, { credential }, dir.journal("db")));
}

test("OPFS compaction writes the Node file host's compacted bytes", () => {
  const want = fileControl("plain.jed", undefined, (db) => db.compact("main"));
  const main = new FaultHandle();
  const dir = new Dir();
  const db = opfsDb(main, dir);
  const before = main.getSize();
  const version = db.version;
  db.compact("main");
  assert.equal(db.version, version + 1n);
  assert.ok(main.getSize() * 3 < before, `compaction shrinks: ${before} -> ${main.getSize()}`);
  assert.equal(main.getSize(), db.pageCount * PAGE);
  assert.equal(dir.files.size, 0, "the journal is removed");
  assert.deepEqual(rows(db, PROBE), want.rows);
  // The handle keeps working on the compacted file.
  run(db, ["INSERT INTO t VALUES (5000, 'after', 1, NULL, NULL)"]);
  run(db, ["DELETE FROM t WHERE id = 5000"]);
  db.close();
  const compacted = fileControl("plain2.jed", undefined, (d) => {
    d.compact("main");
    run(d, ["INSERT INTO t VALUES (5000, 'after', 1, NULL, NULL)"]);
    run(d, ["DELETE FROM t WHERE id = 5000"]);
  });
  assert.ok(bytesEqual(main.bytes(), compacted.bytes), "OPFS and the file host differ");
  const again = reopen(main, dir);
  assert.deepEqual(rows(again, PROBE), want.rows);
  again.close();
});

test("an encrypted OPFS database compacts and rekeys like the file host", () => {
  const pass: Credential = { passphrase: "rotated" };
  const kdf = { memoryKib: 64, iterations: 1, parallelism: 1 };
  const want = fileControl("enc.jed", KEY, (db) => {
    db.compact("main");
    db.rekey("main", pass, kdf);
  });
  const main = new FaultHandle();
  const dir = new Dir();
  const db = opfsDb(main, dir, KEY);
  db.compact("main");
  db.rekey("main", pass, kdf);
  assert.deepEqual(rows(db, PROBE), want.rows);
  db.close();
  assert.ok(bytesEqual(main.bytes(), want.bytes), "the encrypted OPFS rewrite differs");
  assert.equal(
    errCode(() => reopen(main, dir, KEY)),
    "28P01",
  );
  const opened = reopen(main, dir, pass);
  // Decrypt it, then the file opens with no credential.
  opened.rekey("main", null);
  assert.deepEqual(rows(opened, PROBE), want.rows);
  opened.close();
  const plain = reopen(main, dir);
  assert.deepEqual(rows(plain, PROBE), want.rows);
  plain.close();
});

test("a failure before the journal's trailer is durable leaves the database untouched", () => {
  const main = new FaultHandle();
  const dir = new Dir();
  const db = opfsDb(main, dir);
  const before = main.bytes();
  const want = rows(db, PROBE);
  // The journal's second flush is the trailer's.
  const journal = new FaultHandle();
  journal.failFlushAfter = 1;
  dir.next = journal;
  assert.throws(() => db.compact("main"), /injected flush failure/);
  assert.ok(bytesEqual(main.bytes(), before), "the database was not touched");
  assert.equal(dir.files.size, 0, "the interrupted journal is removed");
  // The handle is not poisoned: it reads and writes the old file.
  assert.deepEqual(rows(db, PROBE), want);
  run(db, ["INSERT INTO t VALUES (9000, 'kept', 2, NULL, NULL)"]);
  db.close();
});

test("an interrupted journal found at open is discarded", () => {
  const main = new FaultHandle();
  const dir = new Dir();
  const db = opfsDb(main, dir);
  const want = rows(db, PROBE);
  db.close();
  const bytes = main.bytes();
  // A journal whose trailer never landed: image bytes with no valid trailer.
  for (const garbage of [new Uint8Array(0), bytes.slice(0, 3000), bytes]) {
    dir.files.set("db.jedcompact", new FaultHandle(garbage));
    const opened = reopen(new FaultHandle(bytes), dir);
    assert.equal(dir.files.size, 0);
    assert.deepEqual(rows(opened, PROBE), want);
    opened.close();
  }
});

test("a failure during the copy is finished by the next open", () => {
  const control = fileControl("copy.jed", undefined, (d) => d.compact("main"));
  for (const readOnly of [false, true]) {
    const main = new FaultHandle();
    const dir = new Dir();
    const db = opfsDb(main, dir);
    // The copy's first write tears: half of it lands, then the database's writes fail.
    main.failWriteAfter = main.writes;
    assert.throws(() => db.compact("main"), /injected write failure/);
    assert.equal(dir.files.size, 1, "the committed journal is kept");
    // The handle's storage was replaced: it refuses commits until it is reopened.
    main.failWriteAfter = Infinity;
    assert.throws(() => run(db, ["INSERT INTO t VALUES (9000, 'lost', 2, NULL, NULL)"]));
    assert.ok(!bytesEqual(main.bytes(), control.bytes), "the copy is torn");
    // Even a read-only open finishes the committed replace.
    const opened = Database.fromEngine(openOpfsWithHandle(main, { readOnly }, dir.journal("db")));
    assert.equal(dir.files.size, 0, "recovery removes the journal");
    assert.ok(bytesEqual(main.bytes(), control.bytes), "recovery finishes the compaction");
    assert.deepEqual(rows(opened, PROBE), control.rows);
    opened.close();
  }
});

test("create discards a journal a removed database left behind", () => {
  const dir = new Dir();
  const stale = new FaultHandle();
  {
    const db = opfsDb(new FaultHandle(), dir);
    db.close();
  }
  dir.files.set("db.jedcompact", stale);
  runSync(discardJournalSteps(dir.journal("other"))); // another name's journal is not touched
  assert.equal(dir.files.size, 1);
  const db = Database.fromEngine(createOpfsWithHandle(new FaultHandle(), {}, dir.journal("db")));
  assert.equal(dir.files.size, 0);
  assert.deepEqual(rows(db, "SELECT 1"), [[intValue(1n)]]);
  db.close();
});

test("OPFS compaction keeps its preconditions", () => {
  const main = new FaultHandle();
  const dir = new Dir();
  const db = opfsDb(main, dir);
  const reader = db.readSession();
  assert.equal(
    errCode(() => db.compact("main")),
    "55006",
  );
  reader.close();
  db.close();
  const ro = Database.fromEngine(
    openOpfsWithHandle(new FaultHandle(main.bytes()), { readOnly: true }, dir.journal("db")),
  );
  assert.equal(
    errCode(() => ro.compact("main")),
    "25006",
  );
  ro.close();
  assert.equal(dir.files.size, 0);
});
