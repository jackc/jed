// The encrypted variant's cross-core byte contract (spec/fileformat/encrypted.md; CLAUDE.md §8): each
// encrypted_* golden is the from-scratch image of a logical database under the goldens' host key, with
// the key material and every page nonce drawn from the insecure seeded stream. This core must (a)
// WRITE the same logical database — built in an encrypted file, so its trees split at the encrypted
// capacity — to exactly the golden's bytes, and (b) READ the golden back with the key. The goldens are
// authored by spec/fileformat/verify.rb. Mirrors impl/rust/tests/encrypted_golden.rs and
// impl/go/encrypted_golden_test.go.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";

import type { Credential } from "../src/crypto.ts";
import {
  createDatabase,
  type Database,
  Environment,
  openDatabase,
  type Value,
} from "../src/lib.ts";
import { encryptedImageForTesting } from "../src/tooling.ts";
import { specPath } from "./tomlmini.ts";
import { bytesEqual, fillerBytesHex, fillerText } from "./util.ts";

const PAGE = 1024;
const ENC_KEY: Credential = { key: new Uint8Array(32).fill(0x4a) };
const ENC_SEED = 0x4a45_4442n;

const scratch = mkdtempSync(join(tmpdir(), "jed-encrypted-golden-"));
after(() => rmSync(scratch, { recursive: true, force: true }));

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

function query(db: Database, sql: string): Value[][] {
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

// check builds name's logical database with sql, compares its sealed image with the golden, and reads
// the golden back, checking probe against what the built database returns.
function check(name: string, sql: string[], probe: string): void {
  const db = createDatabase({
    path: join(scratch, `golden_${name}`),
    pageSize: PAGE,
    skipFsync: true,
    credential: ENC_KEY,
  });
  for (const stmt of sql) run(db, stmt);
  const want = query(db, probe);
  const image = encryptedImageForTesting(db, ENC_SEED, ENC_KEY);
  db.close();
  const golden = fixture(name);
  assert.ok(bytesEqual(image, golden), `${name}: the sealed image differs from the golden`);

  const copy = join(scratch, `read_${name}`);
  writeFileSync(copy, golden);
  const back = openDatabase(copy, { credential: ENC_KEY });
  try {
    assert.deepEqual(query(back, probe), want, `${name}: rows read back`);
  } finally {
    back.close();
  }
}

test("encrypted_empty_db", () => {
  check("encrypted_empty_db.jed", [], "SELECT 1");
});

test("encrypted_pk_table", () => {
  const sql = ["CREATE TABLE t (id i32 PRIMARY KEY, v i16)"];
  for (let i = 1; i <= 300; i++) {
    sql.push(`INSERT INTO t VALUES (${i}, ${i === 3 ? "NULL" : String(i * 10)})`);
  }
  check("encrypted_pk_table.jed", sql, "SELECT count(*), sum(id), sum(v), count(v) FROM t");
});

test("encrypted_overflow_table", () => {
  const sql = [
    "CREATE TABLE t (id i32 PRIMARY KEY, body text, blob bytea)",
    `INSERT INTO t VALUES (1, '${fillerText(2500)}', '\\x${fillerBytesHex(1200)}')`,
    "INSERT INTO t VALUES (2, 'small', '\\xcafe')",
    "INSERT INTO t VALUES (3, NULL, NULL)",
  ];
  check("encrypted_overflow_table.jed", sql, "SELECT id, md5(body), md5(blob) FROM t ORDER BY id");
});

test("encrypted_index_table", () => {
  const sql = [
    "CREATE TABLE t (a i32 PRIMARY KEY, v i64, r i32range)",
    "CREATE INDEX t_r ON t USING gist (r)",
    "CREATE INDEX t_v ON t (v)",
  ];
  for (let a = 1; a <= 150; a++) {
    sql.push(`INSERT INTO t VALUES (${a}, ${a * 3}, i32range(${a}, ${a + 5}))`);
  }
  check(
    "encrypted_index_table.jed",
    sql,
    "SELECT count(*) FILTER (WHERE r && i32range(40, 44)), count(*) FILTER (WHERE v = 300), sum(v) FROM t",
  );
});

test("encrypted_catalog", () => {
  const sql: string[] = [];
  for (let i = 0; i < 30; i++) {
    sql.push(`CREATE TABLE t${String(i).padStart(2, "0")} (id i32 PRIMARY KEY, a text, b i64)`);
  }
  check("encrypted_catalog.jed", sql, "SELECT count(*) FROM t29");
});

// A rekey's cross-core bytes (encrypted.md §12): a plaintext file holding encrypted_index_table's rows,
// encrypted in place, is byte-identical to the golden built at the rekey's txid — every tree rebuilt by
// ascending insertion at the encrypted capacity, the key block and every nonce the database's first
// key-entropy draws.
test("encrypted_rekey_index_table", () => {
  const path = join(scratch, "golden_rekey_index_table.jed");
  const db = createDatabase({
    path,
    pageSize: PAGE,
    skipFsync: true,
    environment: Environment.builder().insecureSeededKeyEntropy(ENC_SEED).build(),
    scratchEncryption: "on",
    locking: "exclusive",
  });
  run(db, "CREATE TABLE t (a i32 PRIMARY KEY, v i64, r i32range)");
  run(db, "CREATE INDEX t_r ON t USING gist (r)");
  run(db, "CREATE INDEX t_v ON t (v)");
  // Descending, so the plaintext trees differ from the ones the rekey rebuilds.
  for (let a = 150; a >= 1; a--) {
    run(db, `INSERT INTO t VALUES (${a}, ${a * 3}, i32range(${a}, ${a + 5}))`);
  }
  db.rekey("main", ENC_KEY);
  db.close();
  assert.ok(
    bytesEqual(new Uint8Array(readFileSync(path)), fixture("encrypted_rekey_index_table.jed")),
    "the rekeyed file differs from the golden",
  );
});
