// Cross-core collation contract: the TS compiler + executor + artifact codec must reproduce the
// byte-exact vectors in spec/collation/vectors/{compiler,sortkey}.toml (CLAUDE.md §8;
// spec/collation/README.md §2/§3/§4). Mirrors impl/rust/tests/collation.rs and
// impl/go/collation_test.go.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  buildBundle,
  compileCollation,
  foldCase,
  foldLowerSimple,
  loadBundle,
  loadedCollation,
  loadUnicodeData,
  openBundle,
  openCollation,
  type PropertyTable,
  saveBundle,
  saveCollation,
  serializeTable,
  sortKey,
} from "../src/collation.ts";
import { readTomlTables, specPath } from "./tomlmini.ts";
import { bytesToHex } from "./util.ts";

function definition(files: string[]): string {
  return files.map((f) => readFileSync(specPath(f), "utf8")).join("\n");
}

// Load jed's pinned production JUCD bundle (spec/collation/fixtures/unicode.jucd) into the
// engine-global loaded set — the production read path the cores now take (no embed). Idempotent.
function loadFixtureBundle(): void {
  loadUnicodeData(readFileSync(specPath("collation/fixtures/unicode.jucd")));
}

function cmpBytes(a: Uint8Array, b: Uint8Array): number {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) if (a[i] !== b[i]) return a[i] - b[i];
  return a.length - b.length;
}

test("collation compiler matches the pinned vectors", () => {
  const rows = readTomlTables(specPath("collation/vectors/compiler.toml"), "compiler");
  assert.ok(rows.length > 0, "no compiler vectors");
  for (const row of rows) {
    const name = row.str("name");
    const coll = compileCollation(row.str("coll_name"), definition(row.strs("def_files")));

    assert.equal(bytesToHex(serializeTable(coll)), row.str("table_hex"), `${name}: table`);

    const artifact = saveCollation(coll);
    const artifactHex = row.str("artifact_hex");
    assert.equal(bytesToHex(artifact), artifactHex, `${name}: artifact`);

    const reopened = openCollation(artifact);
    assert.deepStrictEqual(reopened, coll, `${name}: open == compiled`);
    assert.equal(bytesToHex(saveCollation(reopened)), artifactHex, `${name}: open→save round-trip`);
  }
});

test("collation sort keys match vectors and are strictly ascending", () => {
  // The real version-pinned collations (unicode, es) resolve from the loaded production bundle (the
  // host-load read path), not by recompiling their ~2.3 MB source; the small dev fixtures (not in the
  // bundle) fall back to compiling from their definition files.
  loadFixtureBundle();
  const rows = readTomlTables(specPath("collation/vectors/sortkey.toml"), "sortkey");
  assert.ok(rows.length > 0, "no sortkey vectors");

  let lastColl = "";
  let coll: ReturnType<typeof compileCollation> | null = null;
  let prev: Uint8Array | null = null;

  for (const row of rows) {
    const collName = row.str("coll_name");
    const s = row.str("string");
    const want = row.str("sortkey_hex");
    if (collName !== lastColl) {
      coll =
        loadedCollation(collName) ?? compileCollation(collName, definition(row.strs("def_files")));
      lastColl = collName;
      prev = null;
    }
    const key = sortKey(coll!, s);
    assert.equal(bytesToHex(key), want, `${collName} ${JSON.stringify(s)}: sort key`);
    if (prev !== null) {
      assert.ok(
        cmpBytes(prev, key) < 0,
        `${collName}: ${JSON.stringify(s)} must sort strictly after the previous entry`,
      );
    }
    prev = key;
  }
});

test("collation JUCD bundle vectors round-trip and merge", () => {
  const rows = readTomlTables(specPath("collation/vectors/bundle.toml"), "bundle");
  assert.ok(rows.length > 0, "no bundle vectors");
  for (const row of rows) {
    const rootName = row.str("root_name");
    const root = compileCollation(rootName, definition(row.strs("root_def_files")));
    // Flat layout: tailoring_def_files[i] is the i-th tailoring's files joined by '|'.
    const names = row.strs("tailoring_names");
    const defs = row.strs("tailoring_def_files");
    assert.equal(names.length, defs.length, "tailoring_names/def_files length mismatch");
    const tailorings = names.map((n, i) => compileCollation(n, definition(defs[i].split("|"))));

    const bundle = buildBundle(root, tailorings, undefined, row.str("description"));
    const enc = saveBundle(bundle);
    const want = row.str("bundle_hex");
    assert.equal(bytesToHex(enc), want, "bundle bytes");

    const reopened = openBundle(enc);
    assert.equal(bytesToHex(saveBundle(reopened)), want, "bundle open→save round-trip");

    const { collations } = loadBundle(reopened);
    const find = (name: string) => {
      const c = collations.find((x) => x.name === name);
      assert.ok(c, `loaded bundle missing collation ${name}`);
      return c!;
    };
    assert.equal(
      bytesToHex(serializeTable(find(rootName))),
      bytesToHex(serializeTable(root)),
      "root table changed through the bundle",
    );
    for (const t of tailorings) {
      assert.equal(
        bytesToHex(serializeTable(find(t.name))),
        bytesToHex(serializeTable(t)),
        `merge identity for ${t.name}`,
      );
    }
  }
});

test("openCollation rejects a tampered artifact", () => {
  const coll = compileCollation("dev-root", definition(["collation/fixtures/dev-root.allkeys"]));
  const artifact = saveCollation(coll);
  artifact[artifact.length - 1] ^= 0xff;
  assert.throws(
    () => openCollation(artifact),
    (e: Error) => /XX001/.test(String(e)) || /corrupt/.test(String(e)),
    "tampered artifact must be data_corrupted",
  );
});

// The casing kernels' DIVERGENCES from the PG/glibc oracle (collation.md §16) — what the oracle corpus
// cannot express (CLAUDE.md §10): the ASCII baseline (non-ASCII passes through) and full SpecialCasing
// (ß→SS). The kernels take an EXPLICIT property table, so the un-loaded (ASCII) regime is deterministic
// regardless of the engine-global loaded set. Mirrors collation.rs casing_tests / collation_test.go.
test("casing kernels: ASCII baseline, full Unicode, and simple-only ILIKE folding", () => {
  const p: PropertyTable = {
    simple: [
      { cp: 0x41, upper: 0x41, lower: 0x61, title: 0x41 }, // A
      { cp: 0x61, upper: 0x41, lower: 0x61, title: 0x41 }, // a
      { cp: 0xc9, upper: 0xc9, lower: 0xe9, title: 0xc9 }, // É
      { cp: 0xe9, upper: 0xc9, lower: 0xe9, title: 0xc9 }, // é
    ],
    special: [{ cp: 0xdf, upper: [0x53, 0x53], lower: [0xdf], title: [0x53, 0x73] }],
  };
  // ASCII baseline (undefined property): fold a–z/A–Z, pass the rest through.
  assert.equal(foldCase("café", true, undefined), "CAFé");
  assert.equal(foldCase("CAFÉ", false, undefined), "cafÉ");
  assert.equal(foldCase("ß", true, undefined), "ß");
  // Full Unicode via the property table.
  assert.equal(foldCase("aé", true, p), "AÉ");
  assert.equal(foldCase("AÉ", false, p), "aé");
  assert.equal(foldCase("ß", true, p), "SS"); // SpecialCasing expansion — the glibc divergence
  assert.equal(foldCase("aßa", true, p), "ASSA");
  assert.equal(foldCase("z", true, p), "z"); // not in the table → identity
  // ILIKE folding is simple-only (never expands): ß stays one code point.
  assert.equal(foldLowerSimple("ß", p), "ß");
  assert.equal(foldLowerSimple("É", p), "é");
  assert.equal(foldLowerSimple("HELLO", undefined), "hello");
  assert.equal(foldLowerSimple("É", undefined), "É");
});
