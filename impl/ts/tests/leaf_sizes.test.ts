// A dirty leaf keeps its content-dependent size facts across inserts, deletes and row replacements
// (LeafSizer.addRow / removeRow). They must equal a fresh measurement at every use: the split and
// merge decisions, and so the file's bytes, depend on them. Mirrors impl/go/leaf_sizes_test.go.

import { test } from "node:test";
import { setLeafSizesCheck } from "../src/pmap.ts";
import { dbWith } from "./util.ts";

test("leaf size facts follow deletes and updates", () => {
  setLeafSizesCheck(true);
  try {
    const db = dbWith([
      "CREATE TABLE t (id bigint PRIMARY KEY, v bigint, w integer, s text, a bigint[])",
      "CREATE INDEX t_s ON t (s)",
      `INSERT INTO t SELECT x, x * 7, CASE WHEN x % 11 = 0 THEN NULL ELSE x % 100 END,
        'status-' || (x % 5), ARRAY[x, x + 1, x % 9] FROM generate_series(1, 6000) AS g(x)`,
    ]);
    const steps = [
      "DELETE FROM t WHERE id BETWEEN 100 AND 900",
      "UPDATE t SET v = 4000000000000 WHERE id % 97 = 0",
      "UPDATE t SET v = NULL, w = w + 100000 WHERE id % 13 = 0",
      "DELETE FROM t WHERE id % 3 = 0",
      "UPDATE t SET s = 'only-' || id WHERE id % 7 = 0",
      "UPDATE t SET a = ARRAY[id * 1000000000] WHERE id % 5 = 0",
      "UPDATE t SET a = NULL, s = NULL WHERE id % 17 = 0",
      "DELETE FROM t WHERE id > 5000",
      "INSERT INTO t SELECT x, 1, 1, 'status-1', '{}'::bigint[] FROM generate_series(100, 900) AS g(x)",
      "DELETE FROM t WHERE v = 4000000000000",
      "UPDATE t SET v = id, w = 0, s = 'x', a = '{1}'",
      "DELETE FROM t WHERE id % 2 = 0",
    ];
    for (const sql of steps) db.execute(sql);
    db.execute("DELETE FROM t");
  } finally {
    setLeafSizesCheck(false);
  }
});
