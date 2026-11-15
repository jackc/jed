// A base-plus-offsets region with exceptions (region kind 6, src/leaf_pfor.ts; format.md "Leaf node").
// The size facts must equal the layout rule measured over the whole column, through adds and
// removals; and a workload whose leaves take the region must read back the rows it wrote, with every
// leaf's size from its page equal to the size measured from its rows, under each exception cap.
// Mirrors impl/go/leaf_forx_test.go.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { setLeafPatchCheck } from "../src/format.ts";
import { forWidth } from "../src/leaf_compact.ts";
import { PFOR_MAX_K, PforAcc, pforStats, setPforCap } from "../src/leaf_pfor.ts";
import { setLeafSizesCheck } from "../src/pmap.ts";
import type { Snapshot } from "../src/snapshot.ts";
import { createDatabase, openDatabase } from "../src/tooling.ts";
import { query } from "./util.ts";

// bruteSaving is kind 6's saving over a whole column (spec/fileformat/verify.rb offsets_plan): every
// width narrower than kind 1's and every base among the present values, n rows of width w.
function bruteSaving(vals: bigint[], n: number, w: number): number {
  if (vals.length === 0) return 0;
  const sorted = vals.slice().sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
  const k1bw = forWidth(w, sorted[sorted.length - 1]! - sorted[0]!);
  let best = -1;
  for (const bw of [0, 1, 2, 4]) {
    if (bw >= k1bw) break;
    const r = bw === 0 ? 0n : (1n << BigInt(8 * bw)) - 1n;
    for (let j = 0; j < sorted.length; j++) {
      if (j > 0 && sorted[j] === sorted[j - 1]) continue;
      const b = sorted[j]!;
      let above = 0;
      for (const v of sorted) if (v > b + r) above++;
      const e = j + above;
      if (e > PFOR_MAX_K) continue;
      const size = (e + 1) * (w + 2) + n * bw;
      if (best < 0 || size < best) best = size;
    }
  }
  if (best < 0) return 0;
  return Math.max(0, n * w - best);
}

// A deterministic xorshift32 stream.
function rng(seed: number): () => number {
  let x = seed >>> 0 || 1;
  return () => {
    x ^= x << 13;
    x >>>= 0;
    x ^= x >>> 17;
    x ^= x << 5;
    x >>>= 0;
    return x;
  };
}

test("kind 6 size facts equal the layout rule through adds and removals", () => {
  const next = rng(0x6a656462);
  for (let round = 0; round < 400; round++) {
    const w = [2, 4, 8][round % 3]!;
    const big = w > 4;
    const n = 1 + (next() % 120);
    // A cluster of close values and a few far ones, some repeated.
    const far = 1 + (next() % 40);
    const spread = [0, 200, 60000, 5_000_000][next() % 4]!;
    const span = w === 2 ? 30000 : w === 4 ? 2_000_000_000 : 4_000_000_000_000;
    const vals: bigint[] = [];
    for (let i = 0; i < n; i++) {
      let v: number;
      if (next() % 100 < far) v = (next() % 2 === 0 ? 1 : -1) * (next() % span);
      else v = spread === 0 ? 7 : 7 + (next() % spread);
      if (w === 2) v = Math.max(-32768, Math.min(32767, v));
      vals.push(BigInt(v));
    }
    const acc = new PforAcc(big);
    for (let i = 0; i < vals.length; i++) {
      acc.add(big ? vals[i]! : Number(vals[i]!));
      const got = acc.saving(i + 1, w);
      const want = bruteSaving(vals.slice(0, i + 1), i + 1, w);
      assert.equal(got, want, `round ${round} w ${w} after ${i + 1} adds`);
    }
    // Remove values one at a time while the facts stay exact; a stale removal ends the round.
    const left = vals.slice();
    while (left.length > 0) {
      const at = next() % left.length;
      const v = left[at]!;
      if (acc.remove(big ? v : Number(v))) break;
      left.splice(at, 1);
      assert.equal(
        acc.saving(n, w),
        bruteSaving(left, n, w),
        `round ${round} w ${w} after a removal`,
      );
    }
  }
});

function pforWorkload(dir: string, cap: number): string {
  setPforCap(cap);
  setLeafSizesCheck(true);
  setLeafPatchCheck(true);
  const path = join(dir, `pfor-${cap}.jed`);
  let db = createDatabase({ path, pageSize: 4096, skipFsync: true });
  let sess = db.session();
  const out: string[] = [];
  try {
    const reopen = (): void => {
      sess.close();
      db.close();
      db = openDatabase(path, { skipFsync: true });
      sess = db.session();
    };
    const show = (sql: string): void => {
      for (const row of query(sess, sql)) out.push(row.join("|"));
    };
    sess.execute(
      "CREATE TABLE f (id bigint PRIMARY KEY, a integer, b bigint, c smallint, d date, e bigint)",
    );
    sess.execute(`INSERT INTO f SELECT x, x % 7, CASE WHEN x % 97 = 0 THEN 9000000000 + x ELSE x % 50 END,
      CASE WHEN x % 13 = 0 THEN NULL ELSE 3 END, DATE '2024-01-01' + (x % 3),
      CASE WHEN x % 29 = 0 THEN NULL ELSE x * 1000 + x % 3 END FROM generate_series(1, 3000) AS g(x)`);
    reopen();
    sess.execute("UPDATE f SET a = 2000000000 WHERE id = 10");
    sess.execute("UPDATE f SET c = -32000 WHERE id = 500");
    sess.execute("UPDATE f SET a = -2000000000 WHERE id IN (1200, 1201)");
    reopen();
    show("SELECT count(*), sum(a), sum(b), sum(c), min(d), max(d), sum(e) FROM f");
    sess.execute("UPDATE f SET a = 3 WHERE id = 10");
    sess.execute("DELETE FROM f WHERE id % 97 = 0 AND id < 1500");
    sess.execute(
      "INSERT INTO f VALUES (5000, 1, 7000000000000, NULL, DATE '1999-12-31', 123456789012)",
    );
    reopen();
    show("SELECT count(*), sum(a), sum(b), sum(c), min(d), max(d), sum(e) FROM f");
    sess.execute("UPDATE f SET e = -5 WHERE id % 300 = 7");
    sess.execute("DELETE FROM f WHERE id BETWEEN 2000 AND 2100");
    reopen();
    show("SELECT count(*), sum(e), min(e), max(e) FROM f");
    show(
      "SELECT id, a, b, c, d, e FROM f WHERE id IN (10, 97, 500, 1200, 1201, 1552, 1807, 5000) ORDER BY id",
    );
    show("SELECT * FROM f ORDER BY id");
    sess.close();
    db.close();
    // Every leaf's size from its page equals the size measured from its rows.
    const check = openDatabase(path, { skipFsync: true });
    try {
      const committed = (check as unknown as { core: { committed: Snapshot } }).core.committed;
      assert.ok(committed.store("f").checkPackedSizes() > 0, "no packed leaf was checked");
    } finally {
      check.close();
    }
    assert.ok(readFileSync(path).length > 0);
  } finally {
    setPforCap(PFOR_MAX_K);
    setLeafSizesCheck(false);
    setLeafPatchCheck(false);
  }
  return out.join("\n");
}

test("kind 6 regions read back their rows under each exception cap", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-leaf-pfor-"));
  try {
    const plain = pforWorkload(dir, 0);
    for (const cap of [1, 4, PFOR_MAX_K]) {
      const before = pforStats.regions;
      assert.equal(pforWorkload(dir, cap), plain, `cap ${cap}: rows differ from kind 1 alone`);
      assert.ok(pforStats.regions > before, `cap ${cap}: the workload wrote no kind 6 region`);
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
