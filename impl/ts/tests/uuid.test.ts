// UUID bit-level extractors (spec/design/functions.md §12) — version + embedded timestamp,
// oracle-verified against PostgreSQL 18. Mirrors the Rust/Go unit tests.

import assert from "node:assert/strict";
import { test } from "node:test";
import { parseUuid } from "../src/value.ts";
import { uuidExtractTimestampMicros, uuidExtractVersion } from "../src/uuid.ts";

function u(s: string): Uint8Array {
  const r = parseUuid(s);
  if ("error" in r) throw new Error(`parseUuid(${s}): ${r.error}`);
  return r.bytes;
}

test("uuid_extract_version gates on the RFC 4122 variant", () => {
  assert.equal(uuidExtractVersion(u("5b2cc7f0-9a3e-4e7b-8c1d-2f3a4b5c6d7e")), 4n);
  assert.equal(uuidExtractVersion(u("0190b6f7-8000-7000-8000-000000000000")), 7n);
  assert.equal(uuidExtractVersion(u("c232ab00-9414-11ec-b3c8-9e6bdeced846")), 1n);
  assert.equal(uuidExtractVersion(u("1ec9414c-232a-6b00-b3c8-9e6bdeced846")), 6n);
  // nil (variant 0), non-RFC (variant 0), Microsoft GUID (variant 11) → NULL.
  assert.equal(uuidExtractVersion(u("00000000-0000-0000-0000-000000000000")), null);
  assert.equal(uuidExtractVersion(u("5b2cc7f0-9a3e-4e7b-0c1d-2f3a4b5c6d7e")), null);
  assert.equal(uuidExtractVersion(u("5b2cc7f0-9a3e-4e7b-cc1d-2f3a4b5c6d7e")), null);
});

test("uuid_extract_timestamp returns micros for v1/v7 only", () => {
  assert.equal(uuidExtractTimestampMicros(u("0190b6f7-8000-7000-8000-000000000000")), 1_721_056_591_872_000n);
  assert.equal(uuidExtractTimestampMicros(u("c232ab00-9414-11ec-b3c8-9e6bdeced846")), 1_645_557_742_000_000n);
  // v1 sub-microsecond 100-ns ticks are truncated.
  assert.equal(uuidExtractTimestampMicros(u("c232ab07-9414-11ec-b3c8-9e6bdeced846")), 1_645_557_742_000_000n);
  // v6 (no PG-18 timestamp), v4 (no timestamp), nil → NULL.
  assert.equal(uuidExtractTimestampMicros(u("1ec9414c-232a-6b00-b3c8-9e6bdeced846")), null);
  assert.equal(uuidExtractTimestampMicros(u("5b2cc7f0-9a3e-4e7b-8c1d-2f3a4b5c6d7e")), null);
  assert.equal(uuidExtractTimestampMicros(u("00000000-0000-0000-0000-000000000000")), null);
});
