// The Environment (spec/design/environment.md) — the host-API surface the conformance corpus cannot
// express: two environments isolated in one process, derivation through toBuilder, an environment's
// lifetime under the databases that hold it, and that every domain of one handle (sessions,
// attachments, temp tables, reopened files) uses the handle's environment. Mirrors
// impl/rust/tests/environment.rs and impl/go/environment_test.go.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import {
  attachFile,
  attachMemory,
  createDatabase,
  type Database,
  Environment,
  EnvironmentBuilder,
  ManualClock,
  manualClock,
  openDatabase,
  osRandomSource,
  SeededRandom,
  seededRandomSource,
  wallClock,
} from "../src/lib.ts";
import { intValue, type Value } from "../src/value.ts";
import { specPath } from "./tomlmini.ts";
import { errCode, query } from "./util.ts";

const unicodeBundle = (): Uint8Array =>
  new Uint8Array(readFileSync(specPath("collation/fixtures/unicode.jucd")));
const tzBundle = (): Uint8Array => new Uint8Array(readFileSync(specPath("tz/fixtures/tzdata.jtz")));

const JAN_2024 = 1_704_067_200_000_000n; // 2024-01-01 00:00:00 UTC
const JAN_2025 = 1_735_689_600_000_000n; // 2025-01-01 00:00:00 UTC

const doubled = {
  name: "doubled",
  argTypes: ["i64" as const],
  result: "i64" as const,
  kernel: (args: Value[]) => {
    const a = args[0]!;
    if (a.kind !== "int") throw new Error("strict i64 arg");
    return intValue(a.int * 2n);
  },
};

test("the default environment is the empty one", () => {
  const env = new Environment();
  assert.deepEqual(env.loadedCollations(), []);
  assert.deepEqual(env.loadedTimeZones(), []);
  assert.equal(env.hostFunctions(), null);
  assert.equal(env.randomSource(), osRandomSource());
  assert.equal(env.clockSource(), wallClock());
  assert.deepEqual(Environment.empty().loadedCollations(), []);

  // A database created or opened with no environment gets a FRESH empty one — never a shared default.
  const a = createDatabase();
  const b = createDatabase();
  assert.notEqual(a.environment(), b.environment());
  assert.deepEqual(a.loadedCollations(), []);
  assert.equal(
    errCode(() => query(a.session(), `SELECT 'a' < 'b' COLLATE "unicode"`)),
    "42704",
  );
  a.close();
  b.close();
});

test("two environments in one process are isolated", () => {
  // Different bundles and different clocks, side by side: each database sees only its own.
  const envA = Environment.builder()
    .loadUnicodeData(unicodeBundle())
    .clockSource(manualClock(JAN_2024))
    .build();
  const envB = Environment.builder()
    .loadTimeZoneData(tzBundle())
    .clockSource(manualClock(JAN_2025))
    .registerFunction(doubled)
    .build();
  const a = createDatabase({ environment: envA });
  const b = createDatabase({ environment: envB });
  const sa = a.session();
  const sb = b.session();

  // Collations: A has the bundle, B does not.
  assert.deepEqual(
    a.loadedCollations().map((c) => c.name),
    ["es", "unicode"],
  );
  assert.deepEqual(b.loadedCollations(), []);
  assert.deepEqual(query(sa, `SELECT 'ä' < 'z' COLLATE "unicode"`), [["true"]]);
  assert.equal(
    errCode(() => query(sb, `SELECT 'ä' < 'z' COLLATE "unicode"`)),
    "42704",
  );
  // Casing follows the loaded property table: full Unicode in A, ASCII-only in B.
  assert.deepEqual(query(sa, `SELECT upper('é')`), [["É"]]);
  assert.deepEqual(query(sb, `SELECT upper('é')`), [["é"]]);

  // Zones: B has the tz bundle, A does not.
  assert.ok(b.loadedTimeZones().some((z) => z.name === "America/New_York"));
  assert.deepEqual(a.loadedTimeZones(), []);
  assert.deepEqual(
    query(sb, `SELECT timestamptz '2024-07-01 12:00:00+00' AT TIME ZONE 'America/New_York'`),
    [["2024-07-01 08:00:00"]],
  );
  assert.equal(
    errCode(() =>
      query(sa, `SELECT timestamptz '2024-07-01 12:00:00+00' AT TIME ZONE 'America/New_York'`),
    ),
    "22023",
  );

  // Clocks: each database reads its own.
  assert.deepEqual(query(sa, `SELECT now() = timestamptz '2024-01-01 00:00:00+00'`), [["true"]]);
  assert.deepEqual(query(sb, `SELECT now() = timestamptz '2025-01-01 00:00:00+00'`), [["true"]]);

  // Host functions: only B's environment registers doubled.
  assert.deepEqual(query(sb, `SELECT doubled(21)`), [["42"]]);
  assert.equal(
    errCode(() => query(sa, `SELECT doubled(21)`)),
    "42883",
  );
  a.close();
  b.close();
});

test("toBuilder derives an environment that shares contents and changes only what it is told", () => {
  const random = seededRandomSource(1n);
  const clock = new ManualClock(JAN_2024);
  const base = Environment.builder()
    .loadUnicodeData(unicodeBundle())
    .registerFunction(doubled)
    .randomSource(random)
    .clockSource(clock)
    .build();

  const laterClock = manualClock(JAN_2025);
  const derived = base.toBuilder().clockSource(laterClock).loadTimeZoneData(tzBundle()).build();

  // Shared by reference, not copied.
  assert.equal(derived.collation("unicode"), base.collation("unicode"));
  assert.equal(derived.property(), base.property());
  assert.equal(derived.hostFunctions()!.functionAt(0), base.hostFunctions()!.functionAt(0));
  assert.equal(derived.randomSource(), random);
  // Changed only where told.
  assert.equal(derived.clockSource(), laterClock);
  assert.ok(derived.zone("America/New_York") !== undefined);
  // The original is unaffected.
  assert.equal(base.clockSource(), clock);
  assert.equal(base.zone("America/New_York"), undefined);
  assert.deepEqual(base.loadedTimeZones(), []);

  // A derived builder only adds: a re-loaded bundle is first-wins, and a duplicate function is 42723.
  const again = base.toBuilder().loadUnicodeData(unicodeBundle()).build();
  assert.equal(again.collation("unicode"), base.collation("unicode"));
  assert.equal(
    errCode(() => base.toBuilder().registerFunction(doubled)),
    "42723",
  );

  // A builder's later calls never reach an environment it already built.
  const builder = new EnvironmentBuilder();
  const first = builder.build();
  builder.loadUnicodeData(unicodeBundle()).registerFunction(doubled);
  const second = builder.build();
  assert.deepEqual(first.loadedCollations(), []);
  assert.equal(first.hostFunctions(), null);
  assert.equal(second.loadedCollations().length, 2);

  // The two databases see their own clock while sharing everything else.
  const a = createDatabase({ environment: base }).session();
  const b = createDatabase({ environment: derived }).session();
  assert.deepEqual(query(a, `SELECT now() = timestamptz '2024-01-01 00:00:00+00'`), [["true"]]);
  assert.deepEqual(query(b, `SELECT now() = timestamptz '2025-01-01 00:00:00+00'`), [["true"]]);
  assert.deepEqual(query(b, `SELECT doubled(4), 'ä' < 'z' COLLATE "unicode"`), [["8", "true"]]);
});

test("a controllable source is driven through the environment, not the handle", () => {
  // The environment fixes WHICH source is installed; the host keeps the object and controls it.
  const random = new SeededRandom(1n);
  const clock = new ManualClock(JAN_2024);
  const env = Environment.builder().randomSource(random).clockSource(clock).build();
  const s = createDatabase({ environment: env }).session();
  const first = query(s, "SELECT uuidv4()")[0]![0];
  assert.equal(first, "910a2dec-8902-4cc1-beeb-8da1658eec67");
  random.reseed(1n);
  assert.deepEqual(query(s, "SELECT uuidv4()"), [[first]]);
  clock.advance(366n * 86_400_000_000n); // 2024 is a leap year
  assert.deepEqual(query(s, `SELECT now() = timestamptz '2025-01-01 00:00:00+00'`), [["true"]]);
});

test("a database outlives the caller's reference to its environment", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-env-"));
  const path = join(dir, "lifetime.jed");
  try {
    // The environment is built inside open/create and the caller keeps no reference to it.
    const open = (create: boolean): Database => {
      const environment = Environment.builder()
        .loadUnicodeData(unicodeBundle())
        .registerFunction(doubled)
        .clockSource(manualClock(JAN_2024))
        .build();
      return create
        ? createDatabase({ path, skipFsync: true, environment })
        : openDatabase(path, { skipFsync: true, environment });
    };
    let db = open(true);
    db.execute(`CREATE TABLE t (id i64 PRIMARY KEY, name text COLLATE "unicode")`);
    db.execute(`INSERT INTO t VALUES (1, 'z'), (2, 'ä'), (3, 'a')`);
    db.close();

    db = open(false);
    const s = db.session();
    assert.deepEqual(query(s, `SELECT name FROM t ORDER BY name`), [["a"], ["ä"], ["z"]]);
    assert.deepEqual(query(s, `SELECT doubled(id) FROM t ORDER BY id`), [["2"], ["4"], ["6"]]);
    assert.deepEqual(query(s, `SELECT now() = timestamptz '2024-01-01 00:00:00+00'`), [["true"]]);
    assert.equal(s.environment(), db.environment());
    db.close();

    // Reopening the file with an environment that lacks the collation refuses it (XX002).
    assert.equal(
      errCode(() => openDatabase(path, { skipFsync: true })),
      "XX002",
    );

    // A file attachment opens under the attaching handle's environment (environment.md §3).
    const bare = createDatabase();
    assert.equal(
      errCode(() => bare.attach("f", attachFile(path), true)),
      "XX002",
    );
    bare.close();
    const host = createDatabase({
      environment: Environment.builder().loadUnicodeData(unicodeBundle()).build(),
    });
    host.attach("f", attachFile(path), true);
    assert.deepEqual(query(host.session(), `SELECT name FROM f.t ORDER BY name`), [
      ["a"],
      ["ä"],
      ["z"],
    ]);
    host.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("attachments and temp tables use the handle's environment", () => {
  const env = Environment.builder()
    .loadUnicodeData(unicodeBundle())
    .loadTimeZoneData(tzBundle())
    .build();
  const db = createDatabase({ environment: env });
  db.attach("aux", attachMemory(), false);
  const s = db.session();
  s.execute(`CREATE TABLE aux.t (id i32 PRIMARY KEY, name text)`);
  s.execute(`INSERT INTO aux.t VALUES (1, 'z'), (2, 'ä'), (3, 'a')`);
  assert.deepEqual(query(s, `SELECT name FROM aux.t ORDER BY name COLLATE "unicode"`), [
    ["a"],
    ["ä"],
    ["z"],
  ]);
  s.execute(`CREATE TEMP TABLE tt (name text PRIMARY KEY)`);
  s.execute(`INSERT INTO tt VALUES ('nz'), ('ña')`);
  assert.deepEqual(query(s, `SELECT name FROM tt ORDER BY name COLLATE "es"`), [["nz"], ["ña"]]);
  s.setTimeZone("America/New_York");
  assert.deepEqual(query(s, `SELECT timestamptz '2024-07-01 12:00:00+00'::text`), [
    ["2024-07-01 08:00:00-04"],
  ]);
  db.close();
});
