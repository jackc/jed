// Cross-core collation contract: the TS compiler + executor + artifact codec must reproduce the
// byte-exact vectors in spec/collation/vectors/{compiler,sortkey}.toml (CLAUDE.md §8;
// spec/collation/README.md §2/§3/§4). Mirrors impl/rust/tests/collation.rs and
// impl/go/collation_test.go.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  compileCollation,
  openCollation,
  saveCollation,
  serializeTable,
  sortKey,
  vendoredCollation,
} from "../src/collation.ts";
import { readTomlTables, specPath } from "./tomlmini.ts";
import { bytesToHex } from "./util.ts";

function definition(files: string[]): string {
  return files.map((f) => readFileSync(specPath(f), "utf8")).join("\n");
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
      // The real version-pinned collations (unicode, es) resolve from the embedded .coll — the
      // production read path — rather than recompiling their ~2.3 MB source. The small dev fixtures
      // (not vendored) are compiled from their definition files.
      coll = vendoredCollation(collName) ?? compileCollation(collName, definition(row.strs("def_files")));
      lastColl = collName;
      prev = null;
    }
    const key = sortKey(coll!, s);
    assert.equal(bytesToHex(key), want, `${collName} ${JSON.stringify(s)}: sort key`);
    if (prev !== null) {
      assert.ok(cmpBytes(prev, key) < 0, `${collName}: ${JSON.stringify(s)} must sort strictly after the previous entry`);
    }
    prev = key;
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
