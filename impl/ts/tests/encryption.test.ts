// Encryption at rest (spec/design/encryption.md; bytes in spec/fileformat/encrypted.md). The
// credential is a host-API argument the SQL corpus cannot reach, so its contract lives here: an
// encrypted file round-trips every page kind (tree nodes, overflow chains, indexes, the free list),
// stores no plaintext, refuses a wrong or missing credential, detects tampering and replay as XX001,
// survives compaction, and writes the same bytes for the same history under a seeded key source. The
// cross-core byte contract is pinned by the encrypted_* goldens (encrypted_golden.test.ts). Mirrors
// impl/rust/tests/encryption.rs and impl/go/encryption_test.go.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";

import { backupForTesting } from "../src/backup.ts";
import type { Credential } from "../src/crypto.ts";
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

test("attachments, backups and spill fail closed", () => {
  const path = tmp("enc_attach.jed");
  const enc = create(path, KEY);
  exec(enc, "CREATE TABLE a (id i32 PRIMARY KEY, v text)");
  exec(enc, "INSERT INTO a VALUES (1, 'one')");
  // A backup of an encrypted database is not built yet (encrypted.md §10).
  assert.equal(
    errCode(() => backupForTesting(enc, tmp("enc_backup.jed"), false)),
    "0A000",
  );
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

  // A file main may spill to a plaintext scratch file: an encrypted attachment is refused.
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
