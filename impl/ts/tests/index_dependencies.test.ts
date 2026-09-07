// Host state, dependency lifetime and deliberate argument-sensitive PG admission extensions.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { Engine, EngineError, execute, prepare, queryPrepared, intValue } from "../src/tooling.ts";
import { crc32Ieee, loadEngine, toImage } from "../src/format.ts";
import { loadTimeZoneData, openTzBundle, saveTzBundle } from "../src/timezone.ts";
import { specPath } from "./tomlmini.ts";

const bundle = () => new Uint8Array(readFileSync(specPath("tz/fixtures/tzdata.jtz")));
const run = (db: Engine, sql: string) => execute(db, sql);
const code = (f: () => unknown, expected: string) =>
  assert.throws(f, (e: unknown) => e instanceof EngineError && e.code() === expected);
const indexSQL =
  "CREATE UNIQUE INDEX zone_idx ON t (account) WHERE (ts AT TIME ZONE 'America/New_York')::date >= DATE '2024-01-01'";
function build(): Engine {
  loadTimeZoneData(bundle());
  const db = new Engine();
  run(db, "CREATE TABLE t(id int PRIMARY KEY, account int, ts timestamptz)");
  run(db, "INSERT INTO t VALUES (1,7,'2024-06-01 00:00:00+00'),(2,7,'2020-01-01 00:00:00+00')");
  run(db, indexSQL);
  return db;
}
function corruptPin(original: Uint8Array, mode: string): Uint8Array {
  const img = original.slice();
  const tag = new TextEncoder().encode(mode === "missing" ? "America/New_York" : "2026a");
  const start = Buffer.from(img).lastIndexOf(tag);
  assert.ok(start >= 0);
  const name = Buffer.from(img).lastIndexOf(new TextEncoder().encode("America/New_York"));
  if (mode === "mode") img[name - 5] = 2;
  else if (mode === "empty-static") {
    img[name - 4] = 0;
    img[name - 3] = 0;
  } else if (mode === "empty-name") {
    img[name - 2] = 0;
    img[name - 1] = 0;
  } else if (mode === "empty-version") {
    img[start - 2] = 0;
    img[start - 1] = 0;
  } else img[start + (mode === "checksum" ? tag.length : tag.length - 1)]! ^= 1;

  const page = Math.floor(start / 4096) * 4096;
  const covered = new Uint8Array(4096 - 4);
  covered.set(img.subarray(page, page + 12));
  covered.set(img.subarray(page + 16, page + 4096), 12);
  new DataView(img.buffer).setUint32(page + 12, crc32Ieee(covered), false);
  return img;
}

test("timezone pins persist; skew excludes reads and blocks writes; transactional rebuild", () => {
  const image = toImage(build(), 4096, 1n);
  for (const mode of ["mode", "empty-static", "empty-name", "empty-version"]) {
    code(() => loadEngine(corruptPin(image, mode)), "XX001");
  }
  const matching = loadEngine(image);
  code(() => run(matching, "INSERT INTO t VALUES (3,7,'2024-06-01 00:00:00+00')"), "23505");
  for (const mode of ["version", "checksum", "missing"] as const) {
    const db = loadEngine(corruptPin(image, mode));
    const explain = run(
      db,
      "EXPLAIN SELECT id FROM t WHERE (ts AT TIME ZONE 'America/New_York')::date >= DATE '2024-01-01' AND account = 7",
    );
    assert.ok(
      !JSON.stringify(explain, (_, v) => (typeof v === "bigint" ? String(v) : v)).includes(
        "zone_idx",
      ),
    );
    const rows = run(
      db,
      "SELECT id FROM t WHERE (ts AT TIME ZONE 'America/New_York')::date >= DATE '2024-01-01' AND account = 7",
    );
    assert.equal(rows.kind === "query" && rows.rows.length, 1);
    code(() => run(db, "INSERT INTO t VALUES (3,8,'2024-06-01 00:00:00+00')"), "XX002");
    code(() => run(db, "UPDATE t SET account = 8 WHERE id = 999"), "XX002");
    run(db, "BEGIN");
    run(db, "DROP INDEX zone_idx");
    code(() => run(db, "CREATE UNIQUE INDEX zone_idx ON t (account)"), "23505");
    run(db, "ROLLBACK");
    code(() => run(db, "DELETE FROM t WHERE id = 2"), "XX002");
    run(db, "BEGIN");
    run(db, "DROP INDEX zone_idx");
    run(db, indexSQL);
    run(db, "COMMIT");
    run(db, "INSERT INTO t VALUES (3,8,'2024-06-01 00:00:00+00')");
    code(() => run(db, "INSERT INTO t VALUES (4,7,'2024-06-01 00:00:00+00')"), "23505");
  }
});

test("safe timestamptz expressions remain indexed across session changes", () => {
  const db = build();
  for (const [i, pred] of [
    "ts IS NULL",
    "EXTRACT(epoch FROM ts)>0",
    "date_part('epoch',ts)>0",
    "make_timestamptz(2024,1,1,0,0,0,'UTC') < ts",
    "ts + INTERVAL '1 hour' > ts",
    "uuid_extract_timestamp(uuid '01941f29-7c00-7000-8000-000000000000') IS NOT NULL",
  ].entries())
    run(db, `CREATE INDEX safe_${i} ON t (account) WHERE ${pred}`);
  for (const zone of ["UTC", "+01", "America/New_York"]) {
    db.setTimeZone(zone);
    code(() => run(db, "INSERT INTO t VALUES (3,7,'2024-06-01 00:00:00+00')"), "23505");
  }
  code(() => run(db, "CREATE INDEX unknown_zone ON t ((ts AT TIME ZONE 'Missing/Zone'))"), "22023");
});

test("dynamic zone sets disable read and insert caching, and additions require rebuilding", () => {
  loadTimeZoneData(bundle());
  const db = new Engine();
  run(db, "CREATE TABLE d (id int PRIMARY KEY, account int, ts timestamptz, zone text)");
  run(db, "INSERT INTO d VALUES (1,7,'2024-06-01 00:00:00+00','UTC')");
  const ddl =
    "CREATE INDEX dynamic_idx ON d (account) WHERE (ts AT TIME ZONE zone)::date >= DATE '2024-01-01'";
  run(db, ddl);
  const selectSQL =
    "SELECT id FROM d WHERE (ts AT TIME ZONE zone)::date >= DATE '2024-01-01' AND account = 7";
  const stmt = prepare(db, selectSQL);
  const drain = () => {
    const q = queryPrepared(db, stmt, []);
    for (const row of q) assert.ok(row.length > 0);
    q.close();
  };
  drain();
  drain();
  assert.equal((stmt as unknown as { scHolder: { cache: unknown } }).scHolder.cache, null);
  const ins = prepare(
    db,
    "INSERT INTO d VALUES ($1,7,'2024-06-01 00:00:00+00','UTC') RETURNING id",
  );
  const insert = (id: bigint) => {
    const q = queryPrepared(db, ins, [intValue(id)]);
    for (const row of q) assert.ok(row.length > 0);
    q.close();
  };
  insert(90n);
  insert(91n);
  assert.equal((ins as unknown as { icHolder: { cache: unknown } }).icHolder.cache, null);
  const b = openTzBundle(bundle());
  b.zones = [{ name: "Test/IndexAdded", raw: b.zones[0]!.raw }];
  b.links = [];
  loadTimeZoneData(saveTzBundle(b));
  drain();
  code(() => insert(92n), "XX002");
  code(() => run(db, "INSERT INTO d VALUES (2,7,'2024-06-01 00:00:00+00','UTC')"), "XX002");
  run(db, "BEGIN");
  run(db, "DROP INDEX dynamic_idx");
  run(db, ddl);
  run(db, "COMMIT");
  run(db, "INSERT INTO d VALUES (2,7,'2024-06-01 00:00:00+00','UTC')");
});
