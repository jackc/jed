// Split-point shape (spec/fileformat/format.md "Split point") — pins the tree shapes
// the position-aware split rule produces, observed through the page_read block of a
// full scan (= structural node count, cost.md §3). Ascending inserts take the
// right-edge append split (byte-identical to the old always-largest-left rule, ~full
// leaves); random-order inserts take the balanced split and settle near the classic
// ~2/3 fill — before this rule they splintered into [N-2 | 1] pairs and converged on a
// few-percent fill (the spec/design/benchmarks.md finding). CREATE INDEX inserts its
// entries sorted (indexes.md §1), so a built index packs like the ascending case.
// Mirrored in impl/go/split_shape_test.go and impl/rust/tests/split_shape.rs.

import assert from "node:assert/strict";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { type Engine, create, execute } from "../src/tooling.ts";

function cost(db: Engine, sql: string): bigint {
  return execute(db, sql).cost;
}

// A 121-row table at the fixture page size (256): id bigint pk, v integer = id % 7.
// Ascending inserts pks 0..120 in order; shuffled inserts the permutation (i*37) mod
// 121 — deterministic, identical in every core.
function splitShapeDb(shuffled: boolean): Engine {
  const dir = mkdtempSync(join(tmpdir(), "split-shape-"));
  const db = create(join(dir, "t.jed"), { pageSize: 256 });
  execute(db, "CREATE TABLE t (id bigint PRIMARY KEY, v integer)");
  for (let i = 0; i < 121; i++) {
    const pk = shuffled ? (i * 37) % 121 : i;
    execute(db, `INSERT INTO t VALUES (${pk}, ${pk % 7})`);
  }
  return db;
}

test("split shape costs are pinned", () => {
  // The same logical table costs nearly the same full scan whichever order built it:
  // ascending packs ~full (append splits), shuffled lands a few nodes behind (balanced
  // splits). Under the old always-largest-left rule the shuffled tree splintered into
  // hundreds of near-empty nodes and this cost exploded. The counts dropped from v23
  // (268/278/156) with the v24 B+tree (format.md): interior nodes are a record-free
  // separator skeleton with far higher fan-out, so a full scan touches fewer pages.
  const asc = splitShapeDb(false);
  assert.equal(cost(asc, "SELECT count(*) FROM t"), 258n);
  const shuf = splitShapeDb(true);
  assert.equal(cost(shuf, "SELECT count(*) FROM t"), 265n);

  // Sorted index build (indexes.md §1) packs the index tree like the ascending case;
  // the build charges only its table scan, and the bounded lookup's cost pins the
  // index path's shape (pk ≡ 3 mod 7 in [0,120] ⇒ 17 admitted rows for v = 3). The
  // lookup rose from v23's 100: a B+tree point lookup always descends to a leaf
  // (interior records could terminate a v23 descent early), so each of the 17 table
  // fetches pays the full root→leaf path.
  assert.equal(cost(shuf, "CREATE INDEX t_v_idx ON t (v)"), 143n);
  assert.equal(cost(shuf, "SELECT id FROM t WHERE v = 3"), 105n);
});
