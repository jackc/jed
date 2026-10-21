// Encryption at rest (spec/design/encryption.md; bytes in spec/fileformat/encrypted.md). The
// credential is a host-API argument the SQL corpus cannot reach, so its contract lives here: an
// encrypted file round-trips every page kind (tree nodes, overflow chains, indexes, the free list),
// stores no plaintext, refuses a wrong or missing credential, detects tampering and replay as XX001,
// survives compaction, and writes the same bytes for the same history under a seeded key source. The
// cross-core byte contract is pinned by the encrypted_* goldens (encrypted_golden.test.ts). Mirrors
// impl/rust/tests/encryption.rs and impl/go/encryption_test.go.

import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";

import { backupForTesting } from "../src/backup.ts";
import type { Argon2Params, Credential } from "../src/crypto.ts";
import { crc32Ieee } from "../src/crc32.ts";
import { writeKeyBlockSteps } from "../src/persist.ts";
import { runSync } from "../src/execution.ts";
import type { Engine } from "../src/executor.ts";
import { setSpillDirForTest, tempSpills } from "../src/tooling.ts";
import { EngineError } from "../src/errors.ts";
import {
  attachFile,
  attachMemory,
  createDatabase,
  type Database,
  Environment,
  openDatabase,
  type Value,
} from "../src/lib.ts";
import { errCode } from "./util.ts";

const PAGE = 1024;
const KEY: Credential = { key: new Uint8Array(32).fill(0x4a) };

const scratch = mkdtempSync(join(tmpdir(), "jed-encryption-"));
after(() => rmSync(scratch, { recursive: true, force: true }));

function tmp(name: string): string {
  return join(scratch, name);
}

function seededEnv(): Environment {
  return Environment.builder().insecureSeededKeyEntropy(0x4a45_4442n).build();
}

function create(path: string, credential: Credential): Database {
  return createDatabase({
    path,
    pageSize: PAGE,
    skipFsync: true,
    environment: seededEnv(),
    credential,
  });
}

function open(path: string, credential?: Credential): Database {
  return openDatabase(path, { skipFsync: true, credential });
}

function exec(db: Database, sql: string): void {
  const s = db.session();
  try {
    s.execute(sql);
  } finally {
    s.close();
  }
}

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

// tryRows is rows, or the SQLSTATE of the engine error it raised.
function tryRows(db: Database, sql: string): Value[][] | string {
  try {
    return rows(db, sql);
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
}

// tryOpenRows opens path with the key and reads sql: the rows, or the SQLSTATE of the engine error
// the open or the read raised.
function tryOpenRows(path: string, sql: string): Value[][] | string {
  let db: Database;
  try {
    db = open(path, KEY);
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  try {
    return tryRows(db, sql);
  } finally {
    db.close();
  }
}

const SUMMARY =
  "SELECT count(*), sum(id), max(note), sum(length(blob)), count(*) FILTER (WHERE span && i32range(10, 20)) FROM t";

// The last commit of populate: one row, so most live pages are shared with the generation before it
// and a tampered one is caught by its pointer, not by the newest commit's manifest.
const LAST = "INSERT INTO t VALUES (5000, 'tail', NULL, NULL)";

// populateButLast builds every page kind an encrypted file holds: a multi-level table tree, a
// secondary and a GiST index, incompressible values long enough to spill into overflow chains, and
// enough churn to persist a free list — up to populate's last commit.
function populateButLast(db: Database): void {
  exec(db, "CREATE TABLE t (id i64 PRIMARY KEY, note text, blob bytea, span i32range)");
  exec(db, "CREATE INDEX t_note ON t (note)");
  exec(db, "CREATE INDEX t_span ON t USING gist (span)");
  exec(
    db,
    "INSERT INTO t SELECT g, 'secret-' || g, NULL, i32range(g, g + 3) FROM generate_series(1, 400) g",
  );
  exec(db, "UPDATE t SET blob = decode(repeat(md5(id::text), 40), 'hex') WHERE id % 50 = 0");
  exec(db, "DELETE FROM t WHERE id % 3 = 0");
}

function populate(db: Database): void {
  populateButLast(db);
  exec(db, LAST);
}

function summary(db: Database): Value[][] {
  return rows(db, SUMMARY);
}

function contains(haystack: Uint8Array, needle: string): boolean {
  return Buffer.from(haystack).includes(Buffer.from(needle));
}

test("an encrypted file round-trips every page kind and stores no plaintext", () => {
  const path = tmp("enc_round_trip.jed");
  let db = create(path, KEY);
  populate(db);
  const before = summary(db);
  assert.deepEqual(before[0]![0], { kind: "int", int: 268n });
  db.close();

  const bytes = new Uint8Array(readFileSync(path));
  assert.ok(!contains(bytes, "secret-"), "a row value is stored in plaintext");
  assert.ok(!contains(bytes, "t_not"), "the catalog is stored in plaintext");

  db = open(path, KEY);
  assert.deepEqual(summary(db), before);
  exec(db, "INSERT INTO t VALUES (1000, 'later', NULL, NULL)");
  db.close();
  db = open(path, KEY);
  assert.deepEqual(rows(db, "SELECT note FROM t WHERE id = 1000")[0]![0], {
    kind: "text",
    text: "later",
  });
  db.close();
});

test("credentials fail closed", () => {
  const path = tmp("enc_credentials.jed");
  create(path, KEY).close();
  const code = (fn: () => Database): string =>
    errCode(() => {
      fn().close();
    });
  assert.equal(
    code(() => open(path, { key: new Uint8Array(32).fill(1) })),
    "28P01",
  );
  assert.equal(
    code(() => open(path, { passphrase: "x" })),
    "28P01",
  );
  assert.equal(
    code(() => open(path)),
    "28P02",
  );

  const plain = tmp("enc_plain.jed");
  createDatabase({ path: plain, pageSize: PAGE, skipFsync: true }).close();
  assert.equal(
    code(() => open(plain, KEY)),
    "28P03",
  );

  assert.equal(
    code(() => createDatabase({ credential: KEY })),
    "55000",
  );
  assert.equal(
    code(() =>
      createDatabase({
        path: tmp("enc_small.jed"),
        pageSize: 512,
        skipFsync: true,
        credential: KEY,
      }),
    ),
    "0A000",
  );
  assert.equal(
    code(() =>
      createDatabase({
        path: tmp("enc_short_key.jed"),
        pageSize: PAGE,
        skipFsync: true,
        credential: { key: new Uint8Array(16) },
      }),
    ),
    "22023",
  );
});

test("a passphrase unlocks and the credential reaches no error message", () => {
  const path = tmp("enc_passphrase.jed");
  let db = createDatabase({
    path,
    pageSize: PAGE,
    skipFsync: true,
    credential: { passphrase: "correct horse" },
    kdf: { memoryKib: 64, iterations: 1, parallelism: 1 },
  });
  exec(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  exec(db, "INSERT INTO t VALUES (7)");
  db.close();
  db = open(path, { passphrase: "correct horse" });
  assert.deepEqual(rows(db, "SELECT id FROM t")[0]![0], { kind: "int", int: 7n });
  db.close();
  try {
    open(path, { passphrase: "wrong" }).close();
    assert.fail("a wrong passphrase must not open the database");
  } catch (e) {
    assert.ok(e instanceof EngineError);
    assert.equal(e.code(), "28P01");
    assert.ok(!e.message.includes("wrong"), e.message);
  }
  assert.equal(
    errCode(() =>
      createDatabase({
        path: tmp("enc_bad_kdf.jed"),
        pageSize: PAGE,
        skipFsync: true,
        credential: { passphrase: "p" },
        kdf: { memoryKib: 4, iterations: 1, parallelism: 1 },
      }).close(),
    ),
    "22023",
  );
});

test("tampering and replay are detected", () => {
  const path = tmp("enc_tamper.jed");
  let db = create(path, KEY);
  populateButLast(db);
  const fallback = summary(db);
  exec(db, LAST);
  db.close();
  const original = new Uint8Array(readFileSync(path));
  const ps = PAGE;

  // Flip one ciphertext byte of body pages in turn: each open or full read fails XX001, never
  // touched the page, or — for a page the newest commit wrote, which recovery checks against its
  // manifest — falls back to the previous generation (encrypted.md §4). Never other rows.
  db = open(path, KEY);
  const expected = summary(db);
  db.close();
  const pages = original.length / ps;
  const flipped = [];
  for (let page = 4; page < pages; page += 5) flipped.push(page);
  flipped.push(pages - 1);
  for (const page of flipped) {
    const bytes = original.slice();
    bytes[page * ps + 40]! ^= 0x01;
    writeFileSync(path, bytes);
    const got = tryOpenRows(path, SUMMARY);
    if (typeof got === "string") assert.equal(got, "XX001", `page ${page}`);
    else
      assert.ok(
        JSON.stringify(got, bigintJson) === JSON.stringify(expected, bigintJson) ||
          JSON.stringify(got, bigintJson) === JSON.stringify(fallback, bigintJson),
        `page ${page}`,
      );
  }

  // Replay: copy a genuine body page over another index. A per-page AEAD alone would accept it; the
  // pointer binding names the overwritten page's tag, so a read that reaches it fails.
  let detected = 0;
  for (let b = 5; b < pages; b += 7) {
    const bytes = original.slice();
    bytes.set(original.subarray((b - 1) * ps, b * ps), b * ps);
    writeFileSync(path, bytes);
    const got = tryOpenRows(path, SUMMARY);
    if (typeof got === "string") {
      assert.equal(got, "XX001", `page ${b}`);
      detected++;
    } else {
      assert.ok(
        JSON.stringify(got, bigintJson) === JSON.stringify(expected, bigintJson) ||
          JSON.stringify(got, bigintJson) === JSON.stringify(fallback, bigintJson),
        `page ${b}`,
      );
    }
  }
  assert.ok(detected > 0, "no replay reached a live page");
  writeFileSync(path, original);
});

function bigintJson(_key: string, value: unknown): unknown {
  return typeof value === "bigint" ? value.toString() : value;
}

test("compaction keeps the key and reseals", () => {
  const path = tmp("enc_compact.jed");
  let db = create(path, KEY);
  populate(db);
  const before = summary(db);
  const keyBlock = new Uint8Array(readFileSync(path)).slice(2 * PAGE, 4 * PAGE);
  db.compact("main");
  assert.deepEqual(summary(db), before);
  db.close();
  const bytes = new Uint8Array(readFileSync(path));
  assert.deepEqual(bytes.slice(2 * PAGE, 4 * PAGE), keyBlock);
  db = open(path, KEY);
  assert.deepEqual(summary(db), before);
  db.close();
});

test("a seeded history writes identical bytes", () => {
  const image = (name: string): Uint8Array => {
    const path = tmp(name);
    const db = create(path, KEY);
    populate(db);
    db.close();
    return new Uint8Array(readFileSync(path));
  };
  assert.deepEqual(image("enc_seed_a.jed"), image("enc_seed_b.jed"));
});

test("attachments fail closed", () => {
  const path = tmp("enc_attach.jed");
  const enc = create(path, KEY);
  exec(enc, "CREATE TABLE a (id i32 PRIMARY KEY, v text)");
  exec(enc, "INSERT INTO a VALUES (1, 'one')");
  enc.close();

  // An in-memory main has no spill target: the attachment unlocks with its credential.
  const mem = createDatabase({});
  assert.equal(
    errCode(() => mem.attach("x", attachFile(path))),
    "28P02",
  );
  assert.equal(
    errCode(() => mem.attach("x", attachFile(path, { credential: { passphrase: "p" } }))),
    "28P01",
  );
  assert.equal(
    errCode(() => mem.attach("m", attachMemory({ credential: KEY }))),
    "55000",
  );
  mem.attach("x", attachFile(path, { credential: KEY }));
  assert.deepEqual(rows(mem, "SELECT v FROM x.a")[0]![0], { kind: "text", text: "one" });
  exec(mem, "INSERT INTO x.a VALUES (2, 'two')");
  mem.close();
  const reopened = open(path, KEY);
  assert.deepEqual(rows(reopened, "SELECT count(*) FROM a")[0]![0], { kind: "int", int: 2n });
  reopened.close();

  // A file main spills to plaintext scratch files under scratch encryption "auto": an encrypted
  // attachment is refused (scratch_encryption_gates_encrypted_attachments covers "on").
  const plain = createDatabase({
    path: tmp("enc_attach_main.jed"),
    pageSize: PAGE,
    skipFsync: true,
  });
  assert.equal(
    errCode(() => plain.attach("x", attachFile(path, { credential: KEY }))),
    "55000",
  );
  plain.close();
});

function keyPages(path: string): [Uint8Array, Uint8Array] {
  const bytes = new Uint8Array(readFileSync(path));
  return [bytes.slice(2 * PAGE, 3 * PAGE), bytes.slice(3 * PAGE, 4 * PAGE)];
}

function setKeyPages(path: string, page2: Uint8Array, page3: Uint8Array): void {
  const bytes = new Uint8Array(readFileSync(path));
  bytes.set(page2, 2 * PAGE);
  bytes.set(page3, 3 * PAGE);
  writeFileSync(path, bytes);
}

function generation(page: Uint8Array): bigint {
  return new DataView(page.buffer, page.byteOffset, page.byteLength).getBigUint64(8, false);
}

const SMALL_KDF: Argon2Params = { memoryKib: 64, iterations: 1, parallelism: 1 };

function key(byte: number): Credential {
  return { key: new Uint8Array(32).fill(byte) };
}

// openCode is the SQLSTATE an open of path with credential fails with.
function openCode(path: string, credential: Credential): string {
  return errCode(() => {
    open(path, credential).close();
  });
}

test("changing the credential rewraps the key block only", () => {
  const path = tmp("enc_change.jed");
  let db = create(path, KEY);
  populate(db);
  const before = summary(db);
  const body = new Uint8Array(readFileSync(path)).slice(4 * PAGE);
  const pass: Credential = { passphrase: "new phrase" };
  db.changeCredential("main", pass, SMALL_KDF);
  // The handle keeps working, and no body page was rewritten.
  assert.deepEqual(summary(db), before);
  db.close();
  assert.deepEqual(new Uint8Array(readFileSync(path)).slice(4 * PAGE), body);
  const [page2, page3] = keyPages(path);
  assert.deepEqual(page2, page3);
  assert.equal(generation(page2), 2n);

  assert.equal(openCode(path, KEY), "28P01");
  db = open(path, pass);
  assert.deepEqual(summary(db), before);
  // A second change, back to a host key, and a write after it.
  db.changeCredential("main", key(7));
  exec(db, "INSERT INTO t VALUES (6000, 'after', NULL, NULL)");
  db.close();
  db = open(path, key(7));
  assert.deepEqual(rows(db, "SELECT count(*) FROM t")[0]![0], { kind: "int", int: 269n });
  db.close();
  assert.equal(generation(keyPages(path)[0]), 3n);
});

test("credential change errors", () => {
  const path = tmp("enc_change_errors.jed");
  const db = create(path, KEY);
  const code = (fn: () => void): string => errCode(fn);
  assert.equal(
    code(() => db.changeCredential("nope", key(2))),
    "42704",
  );
  assert.equal(
    code(() => db.changeCredential("temp", key(2))),
    "42704",
  );
  assert.equal(
    code(() =>
      db.changeCredential(
        "main",
        { passphrase: "p" },
        { memoryKib: 1, iterations: 1, parallelism: 1 },
      ),
    ),
    "22023",
  );
  assert.equal(
    code(() => db.changeCredential("main", { key: new Uint8Array(16) })),
    "22023",
  );
  exec(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  const writer = db.session();
  writer.execute("BEGIN");
  writer.execute("INSERT INTO t VALUES (1)");
  assert.equal(
    code(() => db.changeCredential("main", key(2))),
    "55006",
  );
  writer.execute("ROLLBACK");
  writer.close();
  db.close();

  const ro = openDatabase(path, { skipFsync: true, readOnly: true, credential: KEY });
  assert.equal(
    code(() => ro.changeCredential("main", key(2))),
    "25006",
  );
  ro.close();

  const memory = createDatabase({});
  assert.equal(
    code(() => memory.changeCredential("main", key(2))),
    "55000",
  );
  memory.close();
  const plain = createDatabase({
    path: tmp("enc_change_plain.jed"),
    pageSize: PAGE,
    skipFsync: true,
  });
  assert.equal(
    code(() => plain.changeCredential("main", key(2))),
    "28P03",
  );
  plain.close();
});

test("an attachment changes its own credential", () => {
  const path = tmp("enc_change_attach.jed");
  let db = create(path, KEY);
  exec(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  exec(db, "INSERT INTO t VALUES (1)");
  db.close();
  // An encrypted attachment joins a handle that cannot spill: an encrypted main.
  const main = create(tmp("enc_change_main.jed"), key(3));
  main.attach("other", attachFile(path, { credential: KEY }));
  main.changeCredential("other", key(4));
  main.close();
  assert.equal(openCode(path, KEY), "28P01");
  db = open(path, key(4));
  assert.deepEqual(rows(db, "SELECT id FROM t")[0]![0], { kind: "int", int: 1n });
  db.close();
});

// A crash during a credential change (encryption.md §5): the copy that is not selected is written
// first, so a crash before the first barrier leaves the old credential working, and one after it the
// new — never neither. A writable open repairs a differing pair; a read-only one leaves it.
test("an interrupted credential change leaves exactly one credential", () => {
  const path = tmp("enc_change_crash.jed");
  let db = create(path, KEY);
  exec(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  exec(db, "INSERT INTO t VALUES (1)");
  db.close();
  const [old] = keyPages(path);
  db = open(path, KEY);
  db.changeCredential("main", key(8));
  db.close();
  const [next] = keyPages(path);

  // Torn first write (page 3, the unselected copy of an equal pair): the old credential.
  const torn = next.slice();
  torn.fill(0xee, 100);
  setKeyPages(path, old, torn);
  assert.equal(openCode(path, key(8)), "28P01");
  open(path, KEY).close();
  assert.deepEqual(keyPages(path), [old, old], "the open repairs the torn copy");

  // The first write landed, the second did not: the new credential, and a read-only open leaves the
  // pair as it is.
  setKeyPages(path, old, next);
  assert.equal(openCode(path, KEY), "28P01");
  const ro = openDatabase(path, { skipFsync: true, readOnly: true, credential: key(8) });
  assert.deepEqual(rows(ro, "SELECT id FROM t")[0]![0], { kind: "int", int: 1n });
  ro.close();
  assert.deepEqual(keyPages(path), [old, next]);
  open(path, key(8)).close();
  assert.deepEqual(keyPages(path), [next, next]);

  // Two valid copies of one generation that differ are tampering.
  const forged = next.slice();
  forged[40]! ^= 1;
  new DataView(forged.buffer).setUint32(124, crc32Ieee(forged.subarray(0, 124)), false);
  setKeyPages(path, next, forged);
  assert.equal(openCode(path, key(8)), "XX001");
});

// Two credential changes derived from one selected copy (encryption.md §5): the one that takes the gate
// second no longer follows the selected generation and fails 55006 before writing anything.
test("a credential change that lost the race fails 55006", () => {
  const path = tmp("enc_change_race.jed");
  const db = create(path, KEY);
  // The main domain's storage, reached as the handle's own changeCredential reaches it.
  const storage = (db as unknown as { core: { storage: Engine } }).core.storage;
  const codec = storage.paging!.codec()!;
  const first = codec.rewrap(key(5), SMALL_KDF);
  const second = codec.rewrap(key(6), SMALL_KDF);
  runSync(writeKeyBlockSteps(storage, first));
  const pages = keyPages(path);
  assert.equal(
    errCode(() => runSync(writeKeyBlockSteps(storage, second))),
    "55006",
  );
  assert.deepEqual(keyPages(path), pages);
  db.close();
  assert.equal(openCode(path, key(6)), "28P01");
  open(path, key(5)).close();
});

// scratchFiles are the bytes of every file in the scratch directory dir.
function scratchFiles(dir: string): Uint8Array[] {
  return readdirSync(dir).map((name) => new Uint8Array(readFileSync(join(dir, name))));
}

// An encrypted database spills (encrypted.md §11): sort runs, hash partitions and row spools are sealed,
// so no plaintext reaches the scratch directory, and the answers match the resident run.
test("an encrypted database spills to sealed scratch", () => {
  const dir = tmp("enc_scratch_dir");
  mkdirSync(dir, { recursive: true });
  const path = tmp("enc_scratch.jed");
  const db = create(path, KEY);
  setSpillDirForTest(db, dir);
  exec(db, "CREATE TABLE t (id i64 PRIMARY KEY, k i32, s text)");
  exec(
    db,
    "INSERT INTO t SELECT g, g % 37, 'secret-' || (g * 7919 % 1000) FROM generate_series(1, 3000) g",
  );
  const session = db.session();
  const queries = [
    "SELECT s, id FROM t ORDER BY s DESC, id",
    "SELECT k, count(DISTINCT s), max(s) FROM t GROUP BY k ORDER BY k",
    "SELECT a.id, b.id FROM t a JOIN t b ON a.s = b.s ORDER BY a.id, b.id",
  ];
  const all = (sql: string): Value[][] => {
    const r = session.query(sql);
    try {
      return [...r];
    } finally {
      r.close();
    }
  };
  for (const sql of queries) {
    session.setWorkMem(0);
    const resident = all(sql);
    session.setWorkMem(256);
    const cursor = session.query(sql);
    assert.ok(!cursor[Symbol.iterator]().next().done, sql);
    const files = scratchFiles(dir);
    assert.ok(files.length > 0, `${sql}: nothing spilled`);
    for (const bytes of files)
      assert.ok(!contains(bytes, "secret-"), `${sql}: plaintext in scratch`);
    cursor.close();
    assert.deepEqual(all(sql), resident, sql);
  }
  // A temp table past temp_buffers spills its pages to a sealed scratch store.
  const spills = tempSpills.count;
  session.setTempBuffers(8192);
  session.execute("CREATE TEMP TABLE tt AS SELECT id, s FROM t");
  assert.ok(tempSpills.count > spills);
  for (const bytes of scratchFiles(dir))
    assert.ok(!contains(bytes, "secret-"), "plaintext temp page");
  assert.deepEqual(all("SELECT count(*), max(s) FROM tt")[0]![0], { kind: "int", int: 3000n });
  session.close();
  db.close();
});

// An encrypted attachment may not join a handle whose scratch files are plaintext; a handle created with
// scratchEncryption "on" takes it (encrypted.md §11).
test("scratch encryption gates encrypted attachments", () => {
  const enc = tmp("enc_scratch_att.jed");
  create(enc, KEY).close();
  const plain = createDatabase({
    path: tmp("enc_scratch_p1.jed"),
    pageSize: PAGE,
    skipFsync: true,
  });
  assert.equal(
    errCode(() => plain.attach("e", attachFile(enc, { credential: KEY }))),
    "55000",
  );
  plain.close();
  const sealed = createDatabase({
    path: tmp("enc_scratch_p2.jed"),
    pageSize: PAGE,
    skipFsync: true,
    scratchEncryption: "on",
  });
  sealed.attach("e", attachFile(enc, { credential: KEY }));
  sealed.close();
  // The open options take it too.
  const reopened = openDatabase(tmp("enc_scratch_p1.jed"), {
    skipFsync: true,
    scratchEncryption: "on",
  });
  reopened.attach("e", attachFile(enc, { credential: KEY }));
  reopened.close();
});

function sealedHandle(path: string, credential?: Credential, pageSize = PAGE): Database {
  return createDatabase({
    path,
    pageSize,
    skipFsync: true,
    environment: seededEnv(),
    credential,
    scratchEncryption: "on",
    locking: "exclusive",
  });
}

function keyPage(path: string): Uint8Array {
  return new Uint8Array(readFileSync(path)).slice(2 * PAGE, 3 * PAGE);
}

// Rotating the data key, encrypting a plaintext file and decrypting an encrypted one (encrypted.md
// §12): every tree rebuilt under the target layout, the rows unchanged.
test("rekey rotates, encrypts and decrypts", () => {
  const path = tmp("enc_rekey.jed");
  let db = sealedHandle(path);
  populate(db);
  const before = summary(db);

  // Encrypt the plaintext file in place.
  db.rekey("main", KEY);
  assert.deepEqual(summary(db), before);
  exec(db, "INSERT INTO t VALUES (7000, 'after-encrypt', NULL, NULL)");
  db.close();
  assert.ok(!contains(new Uint8Array(readFileSync(path)), "secret-"));
  assert.equal(
    errCode(() => open(path).close()),
    "28P02",
  );
  db = open(path, KEY);
  assert.deepEqual(rows(db, "SELECT count(*) FROM t")[0]![0], { kind: "int", int: 269n });
  const keyBlock = keyPage(path);

  // Rotate: a new data key under a passphrase; the old key no longer opens the file.
  const pass: Credential = { passphrase: "rotated" };
  db.rekey("main", pass, SMALL_KDF);
  db.close();
  const rotated = keyPage(path);
  assert.notDeepEqual(rotated, keyBlock);
  assert.equal(generation(rotated), 1n);
  assert.equal(openCode(path, KEY), "28P01");
  db = open(path, pass);
  assert.deepEqual(rows(db, "SELECT count(*) FROM t")[0]![0], { kind: "int", int: 269n });

  // Decrypt.
  db.rekey("main", null);
  assert.deepEqual(rows(db, "SELECT note FROM t WHERE id = 7000")[0]![0], {
    kind: "text",
    text: "after-encrypt",
  });
  db.close();
  assert.equal(openCode(path, KEY), "28P03");
  db = open(path);
  assert.deepEqual(rows(db, "SELECT count(*) FROM t")[0]![0], { kind: "int", int: 269n });
  // The table survives every rebuild with its indexes usable.
  assert.deepEqual(rows(db, "SELECT count(*) FROM t WHERE note = 'secret-10'")[0]![0], {
    kind: "int",
    int: 1n,
  });
  db.close();
});

test("rekey errors", () => {
  // A handle whose scratch files are plaintext cannot encrypt.
  const plain = createDatabase({
    path: tmp("enc_rekey_plain.jed"),
    pageSize: PAGE,
    skipFsync: true,
    locking: "exclusive",
  });
  assert.equal(
    errCode(() => plain.rekey("main", KEY)),
    "55000",
  );
  plain.close();
  // The owner of a database the page service shares rekeys it (here it encrypts it).
  const served = createDatabase({
    path: tmp("enc_rekey_served.jed"),
    pageSize: PAGE,
    skipFsync: true,
    scratchEncryption: "on",
  });
  served.rekey("main", KEY);
  served.close();
  const small = sealedHandle(tmp("enc_rekey_small.jed"), undefined, 512);
  assert.equal(
    errCode(() => small.rekey("main", KEY)),
    "0A000",
  );
  small.close();
  const memory = createDatabase({});
  assert.equal(
    errCode(() => memory.rekey("main", KEY)),
    "55000",
  );
  memory.close();
  const db = sealedHandle(tmp("enc_rekey_err.jed"), KEY);
  assert.equal(
    errCode(() => db.rekey("nope", KEY)),
    "42704",
  );
  exec(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  exec(db, "INSERT INTO t VALUES (1), (2)");
  const reader = db.session();
  const cursor = reader.query("SELECT id FROM t");
  assert.ok(!cursor[Symbol.iterator]().next().done);
  assert.equal(
    errCode(() => db.rekey("main", null)),
    "55006",
  );
  cursor.close();
  reader.close();
  db.close();
});

// A row that fits a plaintext page but not an encrypted one (40 bytes less, wider pointers): the
// encryption fails before the file is replaced, which still holds every row (encrypted.md §12).
test("a rekey the target cannot store leaves the file untouched", () => {
  const path = tmp("enc_rekey_toolong.jed");
  const db = sealedHandle(path);
  exec(db, "CREATE TABLE t (k text PRIMARY KEY)");
  // The longest key the plaintext layout stores.
  let len = 1000;
  while (
    typeof tryRows(db, `INSERT INTO t VALUES (repeat('k', ${len})) RETURNING 1`) === "string"
  ) {
    len--;
  }
  const before = new Uint8Array(readFileSync(path));
  let code = "";
  try {
    db.rekey("main", KEY);
  } catch (e) {
    assert.ok(e instanceof EngineError);
    code = e.code();
  }
  assert.ok(code !== "" && code !== "XX001", `the key must not fit the encrypted layout: ${code}`);
  assert.deepEqual(new Uint8Array(readFileSync(path)), before);
  assert.deepEqual(rows(db, "SELECT length(k) FROM t")[0]![0], { kind: "int", int: BigInt(len) });
  db.close();
  assert.equal(readdirSync(scratch).includes("enc_rekey_toolong.jed.jedtmp"), false);
});

// The same history and rekey write the same bytes under the seeded key source.
test("a seeded rekey writes identical bytes", () => {
  const image = (name: string): Uint8Array => {
    const path = tmp(name);
    const db = sealedHandle(path, KEY);
    populate(db);
    db.rekey("main", key(6));
    db.close();
    return new Uint8Array(readFileSync(path));
  };
  assert.deepEqual(image("enc_rekey_seed_a.jed"), image("enc_rekey_seed_b.jed"));
});

// A rebuild larger than the page-cache budget moves its scratch domain to a sealed scratch file instead
// of holding every rebuilt page in memory (encrypted.md §11, §12).
test("a large rekey spills its scratch domain", () => {
  const path = tmp("enc_rekey_large.jed");
  const dir = tmp("enc_rekey_large_scratch");
  mkdirSync(dir, { recursive: true });
  sealedHandle(path, KEY).close();
  let db = openDatabase(path, {
    skipFsync: true,
    cacheBytes: 64 * 1024,
    credential: KEY,
    locking: "exclusive",
  });
  setSpillDirForTest(db, dir);
  exec(db, "CREATE TABLE big (id i64 PRIMARY KEY, s text)");
  exec(
    db,
    "INSERT INTO big SELECT g, 'secret-' || md5(g::text) || md5((g * 3)::text) FROM generate_series(1, 30000) g",
  );
  const sql = "SELECT count(*), max(s), sum(length(s)) FROM big";
  const want = rows(db, sql);
  const spills = tempSpills.count;
  db.rekey("main", key(2));
  assert.ok(tempSpills.count > spills);
  assert.deepEqual(rows(db, sql), want);
  db.close();
  assert.equal(readdirSync(dir).length, 0, "the scratch file is gone");
  db = open(path, key(2));
  assert.deepEqual(rows(db, sql), want);
  db.close();
});

test("an encrypted database backs up in both forms", () => {
  const path = tmp("enc_backup_src.jed");
  const db = create(path, KEY);
  populate(db);
  const before = summary(db);
  const source = new Uint8Array(readFileSync(path));
  const ps = PAGE;
  for (const [form, pageIdentical] of [
    ["pages", true],
    ["compact", false],
  ] as const) {
    const dest = tmp(`enc_backup_${form}.jed`);
    backupForTesting(db, dest, pageIdentical);
    const copy = new Uint8Array(readFileSync(dest));
    assert.ok(!contains(copy, "secret-"), form);
    assert.deepEqual(
      copy.slice(2 * ps, 4 * ps),
      source.slice(2 * ps, 4 * ps),
      `${form}: key block`,
    );
    if (pageIdentical) {
      // Every page the copy holds past the metas is the source's, byte for byte.
      for (let i = 4; i < copy.length / ps; i++) {
        const page = copy.subarray(i * ps, (i + 1) * ps);
        assert.ok(
          page.every((b) => b === 0) ||
            Buffer.from(page).equals(source.subarray(i * ps, (i + 1) * ps)),
          `${form}: page ${i}`,
        );
      }
    }
    assert.equal(
      errCode(() => open(dest).close()),
      "28P02",
      form,
    );
    const back = open(dest, KEY);
    assert.deepEqual(summary(back), before, form);
    exec(back, "INSERT INTO t VALUES (9000, 'in the copy', NULL, NULL)");
    back.close();
  }
  // After a credential change the copy carries the new key block.
  db.changeCredential("main", key(5));
  const dest = tmp("enc_backup_changed.jed");
  backupForTesting(db, dest, true);
  assert.equal(openCode(dest, KEY), "28P01");
  const changed = open(dest, key(5));
  assert.deepEqual(summary(changed), before);
  changed.close();
  db.close();
});
