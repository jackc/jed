// The integrity check (spec/design/integrity-check.md). The damaged fixtures pin the cross-core report
// (spec/fileformat/integrity/*.json, written by the reference spec/fileformat/integrity.rb); every clean
// golden checks clean at both levels; the rest is host-API surface the shared fixtures cannot express:
// 42704, cancellation, truncation, a check beside an open writer, the async API, in-memory and attached
// databases, and the live-only kinds that need damage done after the handle opened (CLAUDE.md §10).
import assert from "node:assert/strict";
import { mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { after, test } from "node:test";
import { openDatabase as openAsync } from "../src/async/node.ts";
import { withCancellation } from "../src/cancel.ts";
import { pageCrc } from "../src/format.ts";
import {
  attachMemory,
  createDatabase,
  type Database,
  EngineError,
  Environment,
  type IntegrityReport,
  IntegrityKind,
  IntegritySkipReason,
  intValue,
  openDatabase,
} from "../src/lib.ts";
import { specPath, readTomlTables } from "./tomlmini.ts";

const scratch = mkdtempSync(join(tmpdir(), "jed-integrity-"));
after(() => rmSync(scratch, { recursive: true, force: true }));

const ENC_KEY = { key: new Uint8Array(32).fill(0x4a) };

// The goldens' environment (fileformat_golden.test.ts): the pinned Unicode and time-zone bundles and
// the geo_hash host function the host-function goldens call.
const GOLDEN_ENV = Environment.builder()
  .loadUnicodeData(new Uint8Array(readFileSync(specPath("collation/fixtures/unicode.jucd"))))
  .loadTimeZoneData(new Uint8Array(readFileSync(specPath("tz/fixtures/tzdata.jtz"))))
  .registerFunction({
    name: "geo_hash",
    argTypes: ["i64"],
    result: "i64",
    kernel: (args) => {
      const a = args[0]!;
      if (a.kind !== "int") throw new Error("strict + resolved i64 arg");
      return intValue(a.int * 10n);
    },
    volatility: "immutable",
    componentId: "com.example/geo_hash",
    semanticVersion: 1,
  })
  .build();

type Contract = Omit<IntegrityReport, "findings" | "ok"> & {
  findings: {
    kind: string;
    database: string;
    object: string | null;
    page: number | null;
    key: string | null;
  }[];
};

// contract is the cross-core part of a report (§4): everything but each finding's detail, keys as hex.
function contract(r: IntegrityReport): Contract {
  return {
    findings: r.findings.map((f) => ({
      kind: f.kind,
      database: f.database,
      object: f.object,
      page: f.page,
      key: f.key === null ? null : Buffer.from(f.key).toString("hex"),
    })),
    truncated: r.truncated,
    skipped: r.skipped,
    pages: r.pages,
    rows: r.rows,
  };
}

let copies = 0;
function copyOf(source: string): string {
  const path = join(scratch, `copy${copies++}.jed`);
  writeFileSync(path, readFileSync(source));
  return path;
}

function sqlstate(code: string): (error: unknown) => boolean {
  return (error) => {
    assert.ok(error instanceof EngineError, String(error));
    assert.equal(error.code(), code);
    return true;
  };
}

type Run = { level: "quick" | "full"; max_findings: number; report: Contract };
const FIXTURE_DIR = dirname(specPath("fileformat/integrity/clean.json"));
const GOLDEN_DIR = dirname(specPath("fileformat/fixtures/empty_db.jed"));
const FIXTURES = readdirSync(FIXTURE_DIR)
  .filter((f) => f.endsWith(".json"))
  .sort()
  .map(
    (f) =>
      JSON.parse(readFileSync(join(FIXTURE_DIR, f), "utf8")) as { fixture: string; runs: Run[] },
  );

test("every damaged fixture reproduces its expected report", () => {
  assert.ok(FIXTURES.length > 0);
  for (const fx of FIXTURES) {
    const db = openDatabase(copyOf(join(FIXTURE_DIR, fx.fixture)), { readOnly: true });
    try {
      for (const run of fx.runs) {
        const r = db.integrityCheck("main", { level: run.level, maxFindings: run.max_findings });
        assert.deepEqual(contract(r), run.report, `${fx.fixture} ${run.level} ${run.max_findings}`);
        assert.equal(r.ok, run.report.findings.length === 0 && !run.report.truncated);
        for (const f of r.findings) assert.equal(typeof f.detail, "string");
      }
    } finally {
      db.close();
    }
  }
});

// The goldens the reference rejects or that fail open by design (verify.rb's reject / malformed /
// expected_open_error), and the one whose index is deliberately misordered.
const NOT_CLEAN = (f: string): boolean =>
  f.startsWith("malformed_") ||
  f === "cow_invalid_free_list.jed" ||
  f === "collation_skew_corrupt.jed";

test("every clean golden checks with no findings at both levels", () => {
  const goldens = readdirSync(GOLDEN_DIR).filter((f) => f.endsWith(".jed"));
  let checked = 0;
  for (const name of goldens.sort()) {
    if (NOT_CLEAN(name)) continue;
    const db = openDatabase(copyOf(join(GOLDEN_DIR, name)), {
      readOnly: true,
      environment: GOLDEN_ENV,
      credential: name.startsWith("encrypted_") ? ENC_KEY : undefined,
    });
    try {
      for (const level of ["quick", "full"] as const) {
        const r = db.integrityCheck("main", { level, maxFindings: 0 });
        assert.deepEqual(r.findings, [], `${name} ${level}`);
        assert.deepEqual(r.skipped, [], `${name} ${level}`);
        assert.ok(r.ok);
      }
    } finally {
      db.close();
    }
    checked++;
  }
  assert.ok(checked > 80, `only ${checked} goldens checked`);

  // collation_skew_corrupt's index is ordered for another collation version: full finds every entry
  // missing, quick (no agreement) finds nothing.
  const db = openDatabase(copyOf(specPath("fileformat/fixtures/collation_skew_corrupt.jed")), {
    readOnly: true,
    environment: GOLDEN_ENV,
  });
  try {
    assert.ok(db.integrityCheck("main", { level: "quick" }).ok);
    const full = db.integrityCheck("main");
    assert.ok(full.findings.length > 0);
    assert.ok(full.findings.every((f) => f.kind.startsWith("index_entry_")));
  } finally {
    db.close();
  }
});

test("every kind and skip reason the check emits is registered", () => {
  const registry = specPath("integrity/findings.toml");
  const kinds = readTomlTables(registry, "finding").map((r) => r.str("kind"));
  const reasons = readTomlTables(registry, "skip").map((r) => r.str("reason"));
  assert.deepEqual(new Set(Object.values(IntegrityKind)), new Set(kinds));
  assert.deepEqual(new Set(Object.values(IntegritySkipReason)), new Set(reasons));
  // The check's own emission sites: every kind passed to add, and every skip reason.
  const source = readFileSync(new URL("../src/integrity.ts", import.meta.url), "utf8");
  const emitted = [...source.matchAll(/this\.add\(\s*(?:\n\s*)?"([a-z_]+)"/g)].map((m) => m[1]!);
  emitted.push(...[...source.matchAll(/e\.kind/g)].map(() => "page_out_of_range"));
  emitted.push("page_reached_twice", "overflow_chain_corrupt", "value_undecodable");
  assert.ok(emitted.length > 20);
  for (const k of emitted) assert.ok(kinds.includes(k), `unregistered kind ${k}`);
  const skips = [...source.matchAll(/reason: "([a-z_]+)"/g)].map((m) => m[1]!);
  assert.ok(skips.length >= 2);
  for (const r of skips) assert.ok(reasons.includes(r), `unregistered skip reason ${r}`);
});

function populated(path?: string): Database {
  const db = createDatabase({ path, pageSize: 1024, skipFsync: true });
  db.execute("CREATE TABLE t (id i64 PRIMARY KEY, v i32, note text, r i32range)");
  db.execute("CREATE INDEX t_v ON t (v)");
  db.execute("CREATE INDEX t_r ON t USING gist (r)");
  db.execute("CREATE INDEX t_expr ON t ((v + 1))");
  db.execute("CREATE TABLE u (k text PRIMARY KEY, big text)");
  db.execute("CREATE SEQUENCE s");
  db.execute(
    "INSERT INTO t SELECT g, g % 17, 'note ' || g, i32range(g, g + 3) FROM generate_series(1, 400) g",
  );
  db.execute("INSERT INTO u VALUES ('a', repeat('x', 5000)), ('b', repeat('yz', 3000))");
  db.execute("DELETE FROM t WHERE id % 5 = 0");
  db.execute("UPDATE t SET note = note || '!' WHERE id % 7 = 0");
  db.execute("SELECT nextval('s')");
  return db;
}

test("a file database checks clean across commits, beside an open writer", () => {
  const db = populated(join(scratch, "live.jed"));
  try {
    const before = db.integrityCheck("main");
    assert.deepEqual(contract(before).findings, []);
    assert.equal(before.rows, 322);
    // An open write transaction is neither waited for nor seen.
    const w = db.writeSession();
    try {
      w.execute("INSERT INTO t VALUES (1000, 1, 'pending', NULL)");
      const during = db.integrityCheck("main", { level: "full" });
      assert.ok(during.ok);
      assert.equal(during.rows, before.rows);
      w.commit();
    } finally {
      w.close();
    }
    const committed = db.integrityCheck("main");
    assert.ok(committed.ok, JSON.stringify(committed.findings));
    assert.equal(committed.rows, before.rows + 1);
  } finally {
    db.close();
  }
  const back = openDatabase(join(scratch, "live.jed"));
  try {
    assert.ok(back.integrityCheck("main").ok);
  } finally {
    back.close();
  }
});

test("in-memory and attached databases check clean; unknown names are 42704", () => {
  const db = populated();
  try {
    const r = db.integrityCheck("main");
    assert.ok(r.ok, JSON.stringify(r.findings));
    // Compaction replaces the byte store; a commit after it writes the next generation.
    db.compact("main");
    assert.ok(db.integrityCheck("main").ok);
    db.execute("INSERT INTO t VALUES (5000, 1, 'after', i32range(1, 2))");
    const after = db.integrityCheck("main");
    assert.ok(after.ok, JSON.stringify(after.findings));
    assert.equal(after.rows, r.rows + 1);
    db.attach("aux", attachMemory());
    db.execute("CREATE TABLE aux.w (id i32 PRIMARY KEY, n text)");
    db.execute("INSERT INTO aux.w SELECT g, 'n' || g FROM generate_series(1, 300) g");
    const aux = db.integrityCheck("aux");
    assert.ok(aux.ok, JSON.stringify(aux.findings));
    assert.equal(aux.rows, 300);
    assert.equal(aux.findings.length, 0);
    assert.throws(() => db.integrityCheck("nope"), sqlstate("42704"));
    assert.throws(() => db.integrityCheck("temp"), sqlstate("42704"));
    assert.throws(() => db.integrityCheck("Main"), sqlstate("42704"));
  } finally {
    db.close();
  }
});

test("cancellation is 57014 and maxFindings truncates to a prefix", () => {
  const swapped = FIXTURES.find((f) => f.fixture === "leaf_keys_swapped.jed")!;
  const db = openDatabase(copyOf(join(FIXTURE_DIR, swapped.fixture)), { readOnly: true });
  try {
    const aborted = new AbortController();
    aborted.abort();
    assert.throws(() => db.integrityCheck("main", { signal: aborted.signal }), sqlstate("57014"));
    // A cancellation requested mid-walk is seen at the next page or row.
    let polls = 0;
    assert.throws(
      () =>
        withCancellation(
          () => ++polls > 5,
          () => db.integrityCheck("main"),
        ),
      sqlstate("57014"),
    );
    const full = db.integrityCheck("main", { maxFindings: 0 });
    assert.ok(full.findings.length > 2);
    assert.equal(full.truncated, false);
    const one = db.integrityCheck("main", { maxFindings: 1 });
    assert.equal(one.truncated, true);
    assert.equal(one.ok, false);
    assert.deepEqual(contract(one).findings, contract(full).findings.slice(0, 1));
    assert.throws(() => db.integrityCheck("main", { maxFindings: -1 }), sqlstate("22023"));
    // The pin is released: the handle still answers queries after a canceled check.
    assert.equal(
      db.integrityCheck("main", { maxFindings: 0 }).findings.length,
      full.findings.length,
    );
  } finally {
    db.close();
  }
});

test("the async API returns the same report across the worker boundary", async () => {
  const fx = FIXTURES.find((f) => f.fixture === "row_rekeyed.jed")!;
  const db = await openAsync(copyOf(join(FIXTURE_DIR, fx.fixture)), { readOnly: true });
  try {
    for (const run of fx.runs) {
      const r = await db.integrityCheck("main", {
        level: run.level,
        maxFindings: run.max_findings,
      });
      assert.ok(r.findings.every((f) => f.key === null || f.key instanceof Uint8Array));
      assert.deepEqual(contract(r), run.report);
      assert.equal(r.ok, false);
    }
    await assert.rejects(db.integrityCheck("nope"), sqlstate("42704"));
    const aborted = new AbortController();
    aborted.abort();
    await assert.rejects(db.integrityCheck("main", { signal: aborted.signal }), sqlstate("57014"));
  } finally {
    await db.close();
  }
});

// damage rewrites page `page` of the file at path through f, keeping its page CRC valid when reseal.
function damage(
  path: string,
  ps: number,
  page: number,
  f: (p: Uint8Array) => void,
  reseal: boolean,
): void {
  const bytes = new Uint8Array(readFileSync(path));
  const p = bytes.subarray(page * ps, (page + 1) * ps);
  f(p);
  if (reseal) new DataView(p.buffer, p.byteOffset).setUint32(12, pageCrc(p), false);
  writeFileSync(path, bytes);
}

function metaOf(path: string, ps: number): { slot: number; root: number; freeHead: number } {
  const bytes = new Uint8Array(readFileSync(path));
  const dv = new DataView(bytes.buffer);
  const t0 = dv.getBigUint64(12);
  const t1 = dv.getBigUint64(ps + 12);
  const slot = t1 > t0 ? 1 : 0;
  return { slot, root: dv.getUint32(slot * ps + 20), freeHead: dv.getUint32(slot * ps + 28) };
}

test("damage after open is reported by the live-only kinds", () => {
  const ps = 1024;
  const kinds = (db: Database): string[] =>
    db.integrityCheck("main", { maxFindings: 0 }).findings.map((f) => f.kind);

  // A padding byte of the catalog page, its CRC resealed: the chain is not the snapshot's encoding,
  // and the commit manifest that listed the page no longer matches it.
  const a = join(scratch, "live_catalog.jed");
  let db = populated(a);
  try {
    const { root } = metaOf(a, ps);
    damage(a, ps, root, (p) => (p[ps - 1] = 0x5a), true);
    const got = kinds(db);
    assert.ok(got.includes("catalog_mismatch"), got.join());
    assert.ok(got.includes("manifest_page_mismatch"), got.join());
  } finally {
    db.close();
  }

  // The same page with its checksum broken: catalog_page_corrupt.
  const b = join(scratch, "live_catalog_crc.jed");
  db = populated(b);
  try {
    const { root } = metaOf(b, ps);
    damage(b, ps, root, (p) => (p[ps - 1] ^= 0xff), false);
    const got = kinds(db);
    assert.ok(got.includes("catalog_page_corrupt"), got.join());
  } finally {
    db.close();
  }

  // The pinned generation's meta slot: meta_invalid.
  const c = join(scratch, "live_meta.jed");
  db = populated(c);
  try {
    const { slot } = metaOf(c, ps);
    damage(c, ps, slot, (p) => (p[100] ^= 0xff), false);
    const got = kinds(db);
    assert.deepEqual(got.slice(0, 1), ["meta_invalid"], got.join());
  } finally {
    db.close();
  }

  // A free-list page that no longer verifies: free_list_corrupt.
  const d = join(scratch, "live_free.jed");
  db = populated(d);
  try {
    const { freeHead } = metaOf(d, ps);
    assert.notEqual(freeHead, 0, "the workload leaves a persisted free list");
    damage(d, ps, freeHead, (p) => (p[0] = 2), true);
    const got = kinds(db);
    assert.ok(got.includes("free_list_corrupt"), got.join());
  } finally {
    db.close();
  }
});
