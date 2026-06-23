// Cross-core time-zone contract: the TS RFC 8536 reader + the JTZ bundle codec must reproduce the
// byte-exact vectors in spec/tz/vectors/{tzif,bundle}.toml (CLAUDE.md §8; spec/tz/README.md §3/§4).
// Mirrors impl/rust/tests/timezone.rs and impl/go/timezone_test.go.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  loadTimeZoneData,
  offsetAtRef,
  openTzBundle,
  resolveZone,
  saveTzBundle,
} from "../src/timezone.ts";
import { readTomlTables, specPath } from "./tomlmini.ts";

function bundleBytes(): Uint8Array {
  // Normalize the Node Buffer to a plain Uint8Array so deepStrictEqual against saveTzBundle's
  // Uint8Array compares bytes, not the Buffer-vs-Uint8Array type.
  return Uint8Array.from(readFileSync(specPath("tz/fixtures/tzdata.jtz")));
}

const SECS = 1_000_000n;

test("timezone reader matches the pinned vectors", () => {
  loadTimeZoneData(bundleBytes());
  const rows = readTomlTables(specPath("tz/vectors/tzif.toml"), "case");
  assert.ok(rows.length > 0, "no tzif vectors");
  for (const row of rows) {
    const zone = row.str("zone");
    const inst = row.big("instant_micros");
    const zr = resolveZone(zone);
    assert.ok(zr !== undefined, `resolve ${zone}`);
    const off = offsetAtRef(zr, inst / SECS - (inst < 0n && inst % SECS !== 0n ? 1n : 0n));
    assert.equal(BigInt(off.utoff), row.big("utoff_secs"), `${zone} @ ${inst}: utoff`);
    assert.equal(off.abbrev, row.str("abbrev"), `${zone} @ ${inst}: abbrev`);
    assert.equal(off.isDst, row.bool("is_dst"), `${zone} @ ${inst}: is_dst`);
  }
});

test("timezone bundle matches the pinned vectors", () => {
  const data = bundleBytes();
  const parsed = openTzBundle(data);
  const rows = readTomlTables(specPath("tz/vectors/bundle.toml"), "bundle");
  assert.equal(rows.length, 1, "want 1 bundle row");
  const b = rows[0];

  assert.equal(parsed.tzdataVersion, b.str("tzdata_version"));
  assert.deepEqual(
    parsed.zones.map((z) => z.name),
    b.strs("zones"),
    "zone manifest",
  );
  assert.deepEqual(
    parsed.links.map((l) => `${l.alias}=${l.target}`),
    b.strs("links"),
    "link table",
  );
  assert.deepEqual(saveTzBundle(parsed), data, "bundle round-trip");
});
