// A dirty leaf keeps its content-dependent size facts across inserts, deletes and row replacements
// (LeafSizer.addRow / removeRow). They must equal a fresh measurement at every use: the split and
// merge decisions, and so the file's bytes, depend on them. Mirrors impl/go/leaf_sizes_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import { setLeafSizesCheck } from "../src/pmap.ts";
import { dbWith, query } from "./util.ts";

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

// Merges and splits measure a leaf with one dictionary scan, find the split point with running
// facts, and keep a column of all-distinct strings without a map (LeafSizer.scan / scanAdd /
// scanFacts). With the check on, every split point is compared with the tabled rule, every kept fact
// with a fresh measurement, and every row classified without its disposition plan with the plan.
// Mirrors impl/go/leaf_sizes_test.go TestLeafMergeAndSplitFacts.
test("leaf size facts follow merges and splits", () => {
  setLeafSizesCheck(true);
  try {
    const db = dbWith([
      // note is unique, tag repeats, n is one value until an update widens it.
      "CREATE TABLE u (id bigint PRIMARY KEY, n integer, note text, tag text)",
      `INSERT INTO u SELECT x, 0, 'note-' || x, 'tag-' || (x % 4)
        FROM generate_series(1, 20000) AS g(x)`,
      // wide holds values that are never inline.
      "CREATE TABLE wide (id bigint PRIMARY KEY, n integer, body text)",
      `INSERT INTO wide SELECT g, 0, CASE WHEN g % 2 = 0
        THEN (SELECT string_agg(md5(CAST(g * 1000 + i AS text)), '' ORDER BY i) FROM generate_series(1, 300) i)
        ELSE repeat(md5(CAST(g AS text)), 300) END FROM generate_series(1, 300) g`,
      // Every text value of d is unique, so its split halves keep their facts. opt is mostly NULL:
      // a dictionary of unique values that still saves bytes. Some notes are long but stay inline.
      "CREATE TABLE d (id bigint PRIMARY KEY, n integer, note text, opt text)",
      `INSERT INTO d SELECT x, 0,
        CASE WHEN x % 10 = 0 THEN repeat('long-', 20) || x ELSE 'note-' || x END,
        CASE WHEN x % 4 = 0 THEN 'opt-' || x END FROM generate_series(1, 8000) AS g(x)`,
    ]);
    const steps = [
      "DELETE FROM u WHERE id BETWEEN 2000 AND 9000", // merges, then splits of the merged leaves
      "UPDATE u SET n = n + 1 WHERE id % 500 = 0", // widens n: splits brim-full leaves
      "UPDATE u SET note = 'changed-' || id WHERE id % 50 = 0", // replaces a value of a distinct column
      "UPDATE u SET note = 'same' WHERE id BETWEEN 9500 AND 9600", // repeats in a distinct column
      "INSERT INTO u SELECT x, 5, 'again-' || x, 'tag-9' FROM generate_series(3000, 3400) AS g(x)",
      "DELETE FROM u WHERE id BETWEEN 9400 AND 15000",
      "DELETE FROM u WHERE id % 3 = 0",
      "UPDATE wide SET n = n + 1 WHERE id = 150",
      "DELETE FROM wide WHERE id BETWEEN 40 AND 260",
      "UPDATE wide SET n = 7 WHERE id % 9 = 0",
      // One transaction: later statements edit the decoded leaves the earlier merges and splits
      // built, whose facts with a distinct column are dropped on the first insert or replacement.
      "BEGIN",
      "DELETE FROM u WHERE id BETWEEN 16000 AND 18500",
      "UPDATE u SET note = 'moved-' || id WHERE id BETWEEN 15990 AND 18520",
      "INSERT INTO u SELECT x, 1, 'new-' || x, 'tag-1' FROM generate_series(16000, 16400) AS g(x)",
      "UPDATE u SET n = 100000 WHERE id BETWEEN 18400 AND 19000",
      "DELETE FROM u WHERE id BETWEEN 16100 AND 19500",
      "COMMIT",
      "BEGIN",
      "DELETE FROM d WHERE id BETWEEN 1000 AND 5000", // merges; the merged leaves split again
      "UPDATE d SET n = id WHERE id % 300 = 0", // widens n: splits brim-full leaves
      "DELETE FROM d WHERE id % 5 = 0", // rows out of leaves kept without a map
      "UPDATE d SET note = 'moved-' || id WHERE id % 7 = 0", // a replacement gives a leaf its map
      "INSERT INTO d SELECT x, 3, 'new-' || x, NULL FROM generate_series(2000, 2600) AS g(x)",
      "UPDATE d SET opt = 'set-' || id WHERE id % 9 = 0",
      "UPDATE d SET note = 'same' WHERE id BETWEEN 6000 AND 6200",
      "DELETE FROM d WHERE id BETWEEN 5500 AND 7000",
      "COMMIT",
    ];
    for (const sql of steps) db.execute(sql);
    assert.deepEqual(
      query(db, "SELECT count(*), sum(n), count(DISTINCT note), count(DISTINCT opt) FROM d"),
      [["2601", "1803", "2601", "734"]],
    );
    assert.deepEqual(
      query(db, "SELECT count(*), sum(n), count(DISTINCT note), count(DISTINCT tag) FROM u"),
      [["2966", "1439", "2966", "5"]],
    );
    assert.deepEqual(query(db, "SELECT count(*), sum(n), sum(length(body)) FROM wide"), [
      ["79", "63", "758400"],
    ]);
    db.execute("DELETE FROM u");
    db.execute("DELETE FROM wide");
  } finally {
    setLeafSizesCheck(false);
  }
});

// A single changed row can widen a whole leaf's encodings so that no two halves fit (format.md
// "Multi-way split"): the running rule reports no split and the tabled one serves it. Mirrors the
// multi-way golden's workload (fileformat_golden.test.ts multiwaySplitDB).
test("a leaf with no two fitting halves splits multi-way under the check", () => {
  setLeafSizesCheck(true);
  try {
    const stmts = ["CREATE TABLE a (id integer PRIMARY KEY, v bigint)"];
    for (let i = 1; i <= 600; i++) {
      if (i !== 400) stmts.push(`INSERT INTO a VALUES (${i}, 7)`);
    }
    stmts.push("UPDATE a SET v = 4000000000000 WHERE id = 60");
    stmts.push("INSERT INTO a VALUES (400, -4000000000000)");
    const db = dbWith(stmts, 256);
    assert.deepEqual(query(db, "SELECT count(*), sum(v) FROM a"), [["600", "4186"]]);
  } finally {
    setLeafSizesCheck(false);
  }
});
