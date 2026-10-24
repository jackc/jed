// The WebCrypto page-codec provider (crypto_web.ts; spec/design/encryption.md §9): the asynchronous
// backend a browser worker seals and opens pages with, run here through Node's own WebCrypto
// (globalThis.crypto.subtle). It must write the same bytes as the node:crypto provider: the shared
// vectors (spec/fileformat/encryption_vectors.toml, [rewrap] included), the encrypted_* goldens through
// the asynchronous image path, and a whole workload's file through the OPFS host — create, incremental
// commits (sealed through seal sessions), reopen — byte for byte against the Node file host.
// Host/byte-level coverage the corpus cannot express (CLAUDE.md §10).

import "../src/crypto_node.ts";

import assert from "node:assert/strict";
import { mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";
import { argon2idAsync } from "@noble/hashes/argon2.js";

import {
  createCodecAsync,
  type Credential,
  DEFAULT_ARGON2_PARAMS,
  hkdfAsync,
  newKeyBlockAsync,
  preferAsyncCryptoForTesting,
  unlockCodecAsync,
  XaesKeyAsync,
} from "../src/crypto.ts";
import { installWebCrypto } from "../src/crypto_web.ts";
import { Environment } from "../src/environment.ts";
import { ioSteps, runAsync, type Steps } from "../src/execution.ts";
import { toImageSteps } from "../src/format.ts";
import { createDatabase, type Database } from "../src/lib.ts";
import { closeOpfs, createOpfsWithHandleSteps, openOpfsWithHandleSteps } from "../src/opfs.ts";
import type { SyncAccessHandle } from "../src/opfsblockstore.ts";
import type { OpfsJournal } from "../src/opfsrewrite.ts";
import { seededRandomSource } from "../src/seam.ts";
import { Database as SharedDatabase, type Session } from "../src/shared.ts";
import type { Snapshot } from "../src/snapshot.ts";
import type { Engine } from "../src/executor.ts";
import { render } from "../src/value.ts";
import { readTomlSection, readTomlTables, specPath } from "./tomlmini.ts";
import { bytesEqual, bytesToHex, fillerBytesHex, fillerText } from "./util.ts";

installWebCrypto();

const VECTORS = specPath("fileformat/encryption_vectors.toml");
const PAGE = 1024;
const ENC_KEY: Credential = { key: new Uint8Array(32).fill(0x4a) };
const ENC_SEED = 0x4a45_4442n;

const scratch = mkdtempSync(join(tmpdir(), "jed-webcrypto-"));
after(() => rmSync(scratch, { recursive: true, force: true }));

function unhex(s: string): Uint8Array {
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = Number.parseInt(s.slice(i * 2, i * 2 + 2), 16);
  return out;
}

// withAsync runs fn with new codecs on the WebCrypto backend.
async function withAsync<T>(fn: () => Promise<T>): Promise<T> {
  preferAsyncCryptoForTesting(true);
  try {
    return await fn();
  } finally {
    preferAsyncCryptoForTesting(false);
  }
}

test("WebCrypto reproduces the shared vectors", async () => {
  for (const v of readTomlTables(VECTORS, "xaes")) {
    const key = await XaesKeyAsync.create(unhex(v.str("key_hex")));
    const nonce = unhex(v.str("nonce_hex"));
    const plaintext = unhex(v.str("plaintext_hex"));
    const aad = unhex(v.str("aad_hex"));
    const { ciphertext, tag } = await key.seal(nonce, plaintext, aad);
    assert.equal(bytesToHex(ciphertext) + bytesToHex(tag), v.str("sealed_hex"), v.str("name"));
    assert.equal(
      bytesToHex((await key.open(nonce, ciphertext, aad, tag))!),
      v.str("plaintext_hex"),
    );
    assert.equal(await key.open(nonce, ciphertext, Uint8Array.of(0x78), tag), null);
  }
  await withAsync(async () => {
    const sub = readTomlSection(VECTORS, "subkeys");
    const dek = unhex(sub.str("dek_hex"));
    const empty = new Uint8Array(0);
    assert.equal(bytesToHex(await hkdfAsync(dek, empty, "jed/v1/page")), sub.str("page_key_hex"));
    assert.equal(bytesToHex(await hkdfAsync(dek, empty, "jed/v1/meta")), sub.str("meta_key_hex"));
    const kek = readTomlSection(VECTORS, "host_kek");
    const hostKey = unhex(kek.str("key_hex"));
    assert.equal(
      bytesToHex(await hkdfAsync(hostKey, unhex(kek.str("kdf_salt_hex")), "jed/v1/kek")),
      kek.str("kek_hex"),
    );
    const kb = readTomlSection(VECTORS, "key_block");
    const { block, dek: drawn } = await newKeyBlockAsync(
      { key: hostKey },
      DEFAULT_ARGON2_PARAMS,
      seededRandomSource(kb.big("seed")),
    );
    assert.equal(bytesToHex(block), kb.str("block_hex"));
    assert.equal(bytesToHex(drawn), kb.str("dek_hex"));
    const rw = readTomlSection(VECTORS, "rewrap");
    const page = new Uint8Array(1024);
    page.set(block, 0);
    const codec = await unlockCodecAsync(
      page,
      page,
      { key: hostKey },
      seededRandomSource(rw.big("seed")),
    );
    assert.equal(codec.async, true);
    const rewrapped = await codec.rewrapAsync(
      { key: unhex(rw.str("key_hex")) },
      DEFAULT_ARGON2_PARAMS,
    );
    assert.equal(bytesToHex(rewrapped), rw.str("block_hex"));
    // A wrong credential does not unwrap (28P01).
    await assert.rejects(
      unlockCodecAsync(page, page, { key: new Uint8Array(32) }, seededRandomSource(1n)),
      /28P01/,
    );
  });
  const a = readTomlSection(VECTORS, "argon2id");
  const out = await argon2idAsync(new TextEncoder().encode(a.str("password")), a.str("salt"), {
    m: a.num("memory_kib"),
    t: a.num("iterations"),
    p: a.num("parallelism"),
    dkLen: 32,
    version: 0x13,
  });
  assert.equal(bytesToHex(out), a.str("key_hex"));
});

function fixture(name: string): Uint8Array {
  return new Uint8Array(readFileSync(specPath(`fileformat/fixtures/${name}`)));
}

function run(db: Database, sql: string): void {
  const s = db.session();
  try {
    s.execute(sql);
  } finally {
    s.close();
  }
}

// The encrypted_* goldens' logical databases (tests/encrypted_golden.test.ts builds the same).
function goldens(): { name: string; sql: string[] }[] {
  const pk = ["CREATE TABLE t (id i32 PRIMARY KEY, v i16)"];
  for (let i = 1; i <= 300; i++) {
    pk.push(`INSERT INTO t VALUES (${i}, ${i === 3 ? "NULL" : String(i * 10)})`);
  }
  const index = [
    "CREATE TABLE t (a i32 PRIMARY KEY, v i64, r i32range)",
    "CREATE INDEX t_r ON t USING gist (r)",
    "CREATE INDEX t_v ON t (v)",
  ];
  for (let a = 1; a <= 150; a++) {
    index.push(`INSERT INTO t VALUES (${a}, ${a * 3}, i32range(${a}, ${a + 5}))`);
  }
  const catalog: string[] = [];
  for (let i = 0; i < 30; i++) {
    catalog.push(`CREATE TABLE t${String(i).padStart(2, "0")} (id i32 PRIMARY KEY, a text, b i64)`);
  }
  return [
    { name: "encrypted_empty_db.jed", sql: [] },
    { name: "encrypted_pk_table.jed", sql: pk },
    {
      name: "encrypted_overflow_table.jed",
      sql: [
        "CREATE TABLE t (id i32 PRIMARY KEY, body text, blob bytea)",
        `INSERT INTO t VALUES (1, '${fillerText(2500)}', '\\x${fillerBytesHex(1200)}')`,
        "INSERT INTO t VALUES (2, 'small', '\\xcafe')",
        "INSERT INTO t VALUES (3, NULL, NULL)",
      ],
    },
    { name: "encrypted_index_table.jed", sql: index },
    { name: "encrypted_catalog.jed", sql: catalog },
  ];
}

test("the asynchronous image path writes every encrypted golden", async () => {
  for (const g of goldens()) {
    const db = createDatabase({
      path: join(scratch, `golden_${g.name}`),
      pageSize: PAGE,
      skipFsync: true,
      credential: ENC_KEY,
    });
    for (const sql of g.sql) run(db, sql);
    const committed = (db as unknown as { core: { committed: Snapshot } }).core.committed;
    const image = await withAsync(async () => {
      const codec = await createCodecAsync(
        ENC_KEY,
        DEFAULT_ARGON2_PARAMS,
        seededRandomSource(ENC_SEED),
      );
      return runAsync(toImageSteps(committed, PAGE, 1n, codec));
    });
    db.close();
    assert.ok(bytesEqual(image, fixture(g.name)), `${g.name}: the async image differs`);
  }
});

// FakeSyncAccessHandle is an in-memory FileSystemSyncAccessHandle (tests/opfs_parity.test.ts has the
// same): a growable byte buffer with positioned read and write.
class FakeSyncAccessHandle implements SyncAccessHandle {
  private buf: Uint8Array;
  private len: number;

  constructor(initial?: Uint8Array) {
    this.buf = initial ? initial.slice() : new Uint8Array(0);
    this.len = this.buf.length;
  }

  private ensure(n: number): void {
    if (n <= this.buf.length) return;
    let cap = Math.max(this.buf.length, 1);
    while (cap < n) cap *= 2;
    const next = new Uint8Array(cap);
    next.set(this.buf.subarray(0, this.len));
    this.buf = next;
  }

  read(buffer: Uint8Array, opts: { at: number }): number {
    const at = opts.at;
    if (at >= this.len) return 0;
    const n = Math.min(buffer.length, this.len - at);
    buffer.set(this.buf.subarray(at, at + n), 0);
    return n;
  }

  write(buffer: Uint8Array, opts: { at: number }): number {
    this.ensure(opts.at + buffer.length);
    this.buf.set(buffer, opts.at);
    if (opts.at + buffer.length > this.len) this.len = opts.at + buffer.length;
    return buffer.length;
  }

  truncate(newSize: number): void {
    this.ensure(newSize);
    if (newSize < this.len) this.buf.fill(0, newSize, this.len);
    this.len = newSize;
  }

  getSize(): number {
    return this.len;
  }

  flush(): void {}

  close(): void {}

  bytes(): Uint8Array {
    return this.buf.subarray(0, this.len).slice();
  }
}

function seeded(): Environment {
  return Environment.builder().insecureSeededKeyEntropy(ENC_SEED).build();
}

// WORKLOAD spans every page kind an encrypted commit seals: multi-level trees, a secondary and a GiST
// index, overflow chains, a persisted free list, and explicit and autocommit transactions.
const WORKLOAD: string[] = [
  "CREATE TABLE t (id i64 PRIMARY KEY, note text, blob bytea, span i32range)",
  "CREATE INDEX t_note ON t (note)",
  "CREATE INDEX t_span ON t USING gist (span)",
  "INSERT INTO t SELECT g, 'secret-' || g, NULL, i32range(g, g + 3) FROM generate_series(1, 300) g",
  "UPDATE t SET blob = decode(repeat(md5(id::text), 60), 'hex') WHERE id % 40 = 0",
  "DELETE FROM t WHERE id % 3 = 0",
  "BEGIN",
  "INSERT INTO t VALUES (1000, 'in a block', NULL, NULL)",
  "UPDATE t SET note = 'changed' WHERE id = 10",
  "COMMIT",
  "INSERT INTO t VALUES (2000, 'tail', NULL, NULL)",
];
const SUMMARY =
  "SELECT count(*), sum(id), max(note), sum(length(blob)), count(*) FILTER (WHERE span && i32range(10, 20)) FROM t";

function* querySteps(session: Session, sql: string): Steps<string[]> {
  const rows = yield* session.querySteps(sql);
  const out: string[] = [];
  try {
    for (;;) {
      const step = yield* rows.nextSteps();
      if (step.done) return out;
      out.push(step.value.map(render).join("|"));
    }
  } finally {
    rows.close();
  }
}

// opfsRun runs WORKLOAD through the OPFS host over a fake handle with the WebCrypto codec, every
// statement an awaited continuation, and returns the file's bytes and the summary read after a reopen.
async function opfsRun(): Promise<{ bytes: Uint8Array; summary: string[] }> {
  return withAsync(async () => {
    const handle = new FakeSyncAccessHandle();
    const engine = await runAsync(
      createOpfsWithHandleSteps(handle, {
        pageSize: PAGE,
        credential: ENC_KEY,
        environment: seeded(),
      }),
    );
    const db = SharedDatabase.fromEngine(engine);
    const session = db.session();
    for (const sql of WORKLOAD) await runAsync(session.executeSteps(sql));
    session.close();
    await runAsync(db.closeSteps());
    const bytes = handle.bytes();
    const reopened = SharedDatabase.fromEngine(
      await runAsync(
        openOpfsWithHandleSteps(new FakeSyncAccessHandle(bytes), { credential: ENC_KEY }),
      ),
    );
    const s = reopened.session();
    const summary = await runAsync(querySteps(s, SUMMARY));
    s.close();
    await runAsync(reopened.closeSteps());
    return { bytes, summary };
  });
}

test("an encrypted OPFS database through WebCrypto writes the Node file host's bytes", async () => {
  const path = join(scratch, "webcrypto_control.jed");
  const control = createDatabase({
    path,
    pageSize: PAGE,
    skipFsync: true,
    environment: seeded(),
    credential: ENC_KEY,
    locking: "none",
  });
  // One session, as the OPFS run: its BEGIN … COMMIT is one transaction.
  const s = control.session();
  for (const sql of WORKLOAD) s.execute(sql);
  const want = [...s.query(SUMMARY)].map((row) => row.map(render).join("|"));
  s.close();
  control.close();
  const fileBytes = new Uint8Array(readFileSync(path));

  const got = await opfsRun();
  assert.deepEqual(got.summary, want);
  assert.equal(got.bytes.length, fileBytes.length);
  assert.ok(bytesEqual(got.bytes, fileBytes), "WebCrypto and node:crypto write different bytes");
  assert.ok(!Buffer.from(got.bytes).includes(Buffer.from("secret-")));
  // The scratch directory holds nothing: OPFS has no scratch target.
  assert.equal(readdirSync(scratch).filter((f) => f.endsWith(".tmp")).length, 0);
});

test("an encrypted OPFS database fails closed without its credential", async () => {
  const { bytes } = await opfsRun();
  await withAsync(async () => {
    const open = (credential?: Credential): Promise<Engine> =>
      runAsync(openOpfsWithHandleSteps(new FakeSyncAccessHandle(bytes), { credential }));
    await assert.rejects(open(), /28P02/);
    await assert.rejects(open({ key: new Uint8Array(32).fill(1) }), /28P01/);
    await assert.rejects(open({ passphrase: "nope" }), /28P01/);
    const engine = await open(ENC_KEY);
    closeOpfs(engine);
  });
});

// A credential change on OPFS runs as a continuation (ts-runtime.md §11): the WebCrypto rewrap and the
// two-barrier key-block write are awaited, and the file equals the Node file host's after the same
// change under the same seeded key stream.
test("an OPFS credential change through WebCrypto writes the Node file host's bytes", async () => {
  const pass: Credential = { passphrase: "changed" };
  const kdf = { memoryKib: 64, iterations: 1, parallelism: 1 };
  const path = join(scratch, "webcrypto_credential.jed");
  const control = createDatabase({
    path,
    pageSize: PAGE,
    skipFsync: true,
    environment: seeded(),
    credential: ENC_KEY,
    locking: "none",
  });
  const s = control.session();
  for (const sql of WORKLOAD) s.execute(sql);
  s.close();
  control.changeCredential("main", pass, kdf);
  control.close();
  const fileBytes = new Uint8Array(readFileSync(path));

  await withAsync(async () => {
    const handle = new FakeSyncAccessHandle();
    const db = SharedDatabase.fromEngine(
      await runAsync(
        createOpfsWithHandleSteps(handle, {
          pageSize: PAGE,
          credential: ENC_KEY,
          environment: seeded(),
        }),
      ),
    );
    const session = db.session();
    for (const sql of WORKLOAD) await runAsync(session.executeSteps(sql));
    session.close();
    await runAsync(db.changeCredentialSteps("main", pass, kdf));
    // Without a journal the OPFS host cannot replace the database: 0A000, the rekey before its KDF.
    assert.throws(() => db.compact("main"), /0A000/);
    assert.throws(() => db.rekey("main", ENC_KEY), /0A000/);
    await runAsync(db.closeSteps());
    const bytes = handle.bytes();
    assert.ok(bytesEqual(bytes, fileBytes), "the OPFS credential change wrote different bytes");

    const open = (credential: Credential): Promise<Engine> =>
      runAsync(openOpfsWithHandleSteps(new FakeSyncAccessHandle(bytes), { credential }));
    await assert.rejects(open(ENC_KEY), /28P01/);
    const reopened = SharedDatabase.fromEngine(await open(pass));
    const r = reopened.session();
    assert.equal((await runAsync(querySteps(r, "SELECT count(*) FROM t")))[0], "202");
    r.close();
    await runAsync(reopened.closeSteps());
  });
});

// memoryJournal is an OPFS journal over a fake handle whose acquisition and removal are awaited, as the
// browser's are (opfs.ts opfsJournal).
function memoryJournal(): { journal: OpfsJournal; files: Map<string, FakeSyncAccessHandle> } {
  const files = new Map<string, FakeSyncAccessHandle>();
  const journal: OpfsJournal = {
    acquire: (create) =>
      ioSteps(
        () => {
          throw new Error("synchronous journal acquisition");
        },
        async () => {
          let h = files.get("j");
          if (h === undefined && create) {
            h = new FakeSyncAccessHandle();
            files.set("j", h);
          }
          return h ?? null;
        },
      ),
    discard: () =>
      ioSteps(
        () => {
          throw new Error("synchronous journal removal");
        },
        async () => {
          files.delete("j");
        },
      ),
  };
  return { journal, files };
}

// Compaction and rekey on OPFS through WebCrypto (hosts.md §5.1, ts-runtime.md §11.1): the database's
// pages are read ahead through the awaited open, the image is sealed through seal sessions into the
// journal, and the result equals the Node file host's compaction and rekey under the same seeded key
// stream, byte for byte. Then the rekeyed file decrypts in place.
test("OPFS compaction and rekey through WebCrypto write the Node file host's bytes", async () => {
  const pass: Credential = { passphrase: "rotated" };
  const kdf = { memoryKib: 64, iterations: 1, parallelism: 1 };
  const path = join(scratch, "webcrypto_compact.jed");
  const control = createDatabase({
    path,
    pageSize: PAGE,
    skipFsync: true,
    environment: seeded(),
    credential: ENC_KEY,
    locking: "none",
  });
  const s = control.session();
  for (const sql of WORKLOAD) s.execute(sql);
  s.close();
  control.compact("main");
  const compacted = new Uint8Array(readFileSync(path));
  control.rekey("main", pass, kdf);
  control.close();
  const rekeyed = new Uint8Array(readFileSync(path));

  await withAsync(async () => {
    const handle = new FakeSyncAccessHandle();
    const { journal, files } = memoryJournal();
    const db = SharedDatabase.fromEngine(
      await runAsync(
        createOpfsWithHandleSteps(
          handle,
          { pageSize: PAGE, credential: ENC_KEY, environment: seeded() },
          journal,
        ),
      ),
    );
    const session = db.session();
    for (const sql of WORKLOAD) await runAsync(session.executeSteps(sql));
    session.close();
    // The synchronous API cannot drive WebCrypto.
    assert.throws(() => db.compact("main"));
    await runAsync(db.compactSteps("main"));
    assert.ok(bytesEqual(handle.bytes(), compacted), "the OPFS compaction wrote different bytes");
    await runAsync(db.rekeySteps("main", pass, kdf));
    assert.ok(bytesEqual(handle.bytes(), rekeyed), "the OPFS rekey wrote different bytes");
    assert.equal(files.size, 0);
    const r = db.session();
    assert.equal((await runAsync(querySteps(r, "SELECT count(*) FROM t")))[0], "202");
    r.close();
    await runAsync(db.closeSteps());

    const reopened = SharedDatabase.fromEngine(
      await runAsync(openOpfsWithHandleSteps(handle, { credential: pass }, null, journal)),
    );
    await runAsync(reopened.rekeySteps("main", null));
    const summary = reopened.session();
    const want = await runAsync(querySteps(summary, SUMMARY));
    summary.close();
    await runAsync(reopened.closeSteps());
    assert.ok(!Buffer.from(rekeyed).includes(Buffer.from("secret-")));
    assert.ok(Buffer.from(handle.bytes()).includes(Buffer.from("secret-")), "decrypted in place");
    const plain = SharedDatabase.fromEngine(
      await runAsync(openOpfsWithHandleSteps(handle, {}, null, journal)),
    );
    const p = plain.session();
    assert.deepEqual(await runAsync(querySteps(p, SUMMARY)), want);
    p.close();
    await runAsync(plain.closeSteps());
  });
});
