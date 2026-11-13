// Split-point shape (spec/fileformat/format.md "Split point") — pins the tree shapes
// the position-aware split rule produces, observed through the page_read block of a
// full scan (= structural node count, cost.md §3). Ascending inserts take the
// right-edge append split (byte-identical to the old always-largest-left rule, ~full
// leaves); random-order inserts even out with a sibling or take the balanced split —
// before the position-aware rule they splintered into [N-2 | 1] pairs and converged on a
// few-percent fill (the spec/design/benchmarks.md finding). CREATE INDEX inserts its
// entries sorted (indexes.md §1), so a built index packs like the ascending case.
// Mirrored in impl/go/split_shape_test.go and impl/rust/tests/split_shape.rs.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";
import { begin } from "../src/api.ts";
import { type Engine, create, execute, toImage } from "../src/tooling.ts";

function cost(db: Engine, sql: string): bigint {
  return execute(db, sql).cost;
}

// A 1210-row table at the fixture page size (256): id bigint pk, v integer = id % 7.
// Ascending inserts pks 0..1209 in order; shuffled inserts the permutation (i*37) mod
// 1210 — deterministic, identical in every core. (121 rows until format v52, whose leaves hold
// about ten times as many of these rows.) Each database is a fresh file in this run's scratch
// directory, removed whole after the last test.
const scratch = mkdtempSync(join(tmpdir(), "split-shape-"));
after(() => rmSync(scratch, { recursive: true, force: true }));
let dbSeq = 0;

function splitShapeDb(shuffled: boolean): Engine {
  return splitShapeOrderDb((i) => (shuffled ? (i * 37) % 1210 : i));
}

// The same table with its 1210 rows inserted in the order pk(0), pk(1), ….
function splitShapeOrderDb(pk: (i: number) => number): Engine {
  const db = create(join(scratch, `t${dbSeq++}.jed`), { pageSize: 256 });
  execute(db, "CREATE TABLE t (id bigint PRIMARY KEY, v integer)");
  for (let i = 0; i < 1210; i++) {
    const id = pk(i);
    execute(db, `INSERT INTO t VALUES (${id}, ${id % 7})`);
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
  assert.equal(cost(asc, "SELECT count(*) FROM t"), 2436n);
  const shuf = splitShapeDb(true);
  assert.equal(cost(shuf, "SELECT count(*) FROM t"), 2436n);

  // Evening out with a sibling (format.md): a leaf that overflows away from the tree's right edge
  // shares its records with a sibling that has room before it splits, which is why the shuffled
  // tree costs exactly what the ascending one does (2443 before the rule). Descending inserts
  // overflow at each leaf's left edge and even out with the right sibling; the zigzag order
  // (0, 1209, 1, 1208, …) grows two fronts that meet in the middle, so its overflows find room on
  // either side. Both land one node behind.
  const desc = splitShapeOrderDb((i) => 1209 - i);
  assert.equal(cost(desc, "SELECT count(*) FROM t"), 2437n);
  const zigzag = splitShapeOrderDb((i) => (i % 2 === 0 ? i / 2 : 1209 - (i - 1) / 2));
  assert.equal(cost(zigzag, "SELECT count(*) FROM t"), 2437n);

  // Sorted index build (indexes.md §1) packs the index tree like the ascending case;
  // the build charges only its table scan, and the bounded lookup's cost pins the
  // index path's shape (pk ≡ 3 mod 7 in [0,1209] ⇒ 173 admitted rows for v = 3). The
  // lookup rose from v23's 100: a B+tree point lookup always descends to a leaf
  // (interior records could terminate a v23 descent early), so each of the 17 table
  // fetches pays the full root→leaf path. Re-pinned for format v52 (the compact leaf), and again
  // when evening out left the shuffled table tree one level shorter (1045 before).
  assert.equal(cost(shuf, "CREATE INDEX t_v_idx ON t (v)"), 1225n);
  assert.equal(cost(shuf, "SELECT id FROM t WHERE v = 3"), 872n);

  // The complete INSERT mutation guard: the committed shape above already contains table-leaf and
  // table-interior splits plus a split secondary index. In one working root, overwrite an existing
  // table row/index entry, append enough rows to split both trees again, and pin the resulting exact
  // node counts through cost. Rollback must restore the byte-exact committed image and original costs.
  const before = toImage(shuf, 256, 1n);
  const tx = begin(shuf, true);
  execute(shuf, "UPDATE t SET v = 42 WHERE id = 1209");
  for (let id = 1210; id <= 2209; id++) execute(shuf, `INSERT INTO t VALUES (${id}, ${id % 7})`);
  assert.equal(cost(shuf, "SELECT count(*) FROM t"), 4450n);
  assert.equal(cost(shuf, "SELECT id FROM t WHERE v = 3"), 1907n);
  tx.rollback();
  assert.deepEqual(toImage(shuf, 256, 1n), before, "rollback changed the committed image");
  assert.equal(cost(shuf, "SELECT count(*) FROM t"), 2436n);
  assert.equal(cost(shuf, "SELECT id FROM t WHERE v = 3"), 872n);
});
