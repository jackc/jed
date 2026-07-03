// memDb — the infallible in-memory test helper (not a `*.test.ts` file, so the runner does not
// execute it). The unified createDatabase returns the uniform (fallible) signature even for the
// in-memory backing, which cannot fail (spec/design/api.md §2.1.1); the tests are exactly the caller
// that unwraps it into an infallible in-memory handle. This is where an infallible in-memory Database
// lives — a test convenience, never public core API.

import { createDatabase, type Database } from "../src/tooling.ts";

// memDb builds a fresh, empty in-memory Database. An optional pageSize builds the tree at a
// non-default page size (byte-level fixtures / a test that round-trips through toImage(pageSize) must
// build the in-memory tree at that size — the page-backed B-tree's fan-out tracks the page size).
export function memDb(pageSize?: number): Database {
  return createDatabase(pageSize ? { pageSize } : {});
}
