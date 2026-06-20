// Collation slice 1c — the host db.importCollation API (spec/design/collation.md §4). These are the
// host-API behaviors the conformance corpus cannot express (CLAUDE.md §10): the import call itself,
// its idempotency, the same-name conflict, and the C rejection. The SQL behavior a loaded collation
// drives (COLLATE / ORDER BY / errors) lives in suites/collation/collate.test, which runs on every
// core. Mirrors impl/rust/tests/collation_host.rs and impl/go/collation_host_test.go.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { Database } from "../src/lib.ts";
import { type Collation, compileCollation } from "../src/collation.ts";
import { specPath } from "./tomlmini.ts";
import { errCode, query } from "./util.ts";

function devRoot(): Collation {
  return compileCollation(
    "dev-root",
    readFileSync(specPath("collation/fixtures/dev-root.allkeys"), "utf8"),
  );
}

// A collation under the name "dev-root" but with the dev-nordic table (a different content hash) —
// the conflicting import.
function devRootNamedButNordicTable(): Collation {
  const def =
    readFileSync(specPath("collation/fixtures/dev-root.allkeys"), "utf8") +
    "\n" +
    readFileSync(specPath("collation/fixtures/dev-nordic.ldml"), "utf8");
  return compileCollation("dev-root", def);
}

test("importCollation then use in a query", () => {
  const db = new Database();
  assert.equal(db.importCollation(devRoot()), "dev-root");
  // The imported collation is usable by name: 'ä' < 'z' is true under dev-root (ä near a), the
  // opposite of the C byte order where it is false.
  assert.deepEqual(query(db, `SELECT 'ä' < 'z' COLLATE "dev-root"`), [["true"]]);
});

test("importCollation is idempotent by name and hash", () => {
  const db = new Database();
  db.importCollation(devRoot());
  // Re-importing the identical (name, content) collation is a no-op success.
  assert.equal(db.importCollation(devRoot()), "dev-root");
});

test("importCollation conflict (same name, different table) is 42710", () => {
  const db = new Database();
  db.importCollation(devRoot());
  // A DIFFERENT table under a name already in use is a conflict (collation.md §4).
  assert.equal(
    errCode(() => db.importCollation(devRootNamedButNordicTable())),
    "42710",
  );
});

test("importing C is rejected", () => {
  const db = new Database();
  // C is table-free and built in; it is never imported (collation.md §4).
  const c = compileCollation(
    "C",
    readFileSync(specPath("collation/fixtures/dev-root.allkeys"), "utf8"),
  );
  assert.equal(
    errCode(() => db.importCollation(c)),
    "42710",
  );
});
