package jed

import "testing"

// A dirty leaf keeps its content-dependent size facts across inserts, deletes and row replacements
// (leafSizes.addRow / removeRow). They must equal a fresh measurement at every use: the split and
// merge decisions, and so the file's bytes, depend on them.
func TestLeafSizeFactsFollowDeletesAndUpdates(t *testing.T) {
	leafSizesCheck.Store(true)
	defer leafSizesCheck.Store(false)
	db, err := CreateDatabase(CreateOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	mustExec(t, db, "CREATE TABLE t (id bigint PRIMARY KEY, v bigint, w integer, s text, a bigint[])")
	mustExec(t, db, "CREATE INDEX t_s ON t (s)")
	mustExec(t, db, `INSERT INTO t SELECT x, x * 7, CASE WHEN x % 11 = 0 THEN NULL ELSE x % 100 END,
		'status-' || (x % 5), ARRAY[x, x + 1, x % 9] FROM generate_series(1, 6000) AS g(x)`)
	steps := []string{
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
	}
	for _, sql := range steps {
		mustExec(t, db, sql)
	}
	mustExec(t, db, "DELETE FROM t")
}

// Merges and splits measure a leaf with one dictionary scan, find the split point with running
// facts, and keep a column of all-distinct strings without a map (scanDicts, splitPointRunning,
// leafSizesScanned). With the check on, every split point is compared with the tabled rule and every
// kept fact with a fresh measurement.
func TestLeafMergeAndSplitFacts(t *testing.T) {
	leafSizesCheck.Store(true)
	defer leafSizesCheck.Store(false)
	db, err := CreateDatabase(CreateOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	// note is unique, tag repeats, n is one value until an update widens it.
	mustExec(t, db, "CREATE TABLE u (id bigint PRIMARY KEY, n integer, note text, tag text)")
	mustExec(t, db, `INSERT INTO u SELECT x, 0, 'note-' || x, 'tag-' || (x % 4)
		FROM generate_series(1, 20000) AS g(x)`)
	// opt is unique and mostly NULL, so its dictionary saves bytes and a wrong entry count shows;
	// long is inline but well past a short value.
	mustExec(t, db, "CREATE TABLE d (id bigint PRIMARY KEY, opt text, long text)")
	mustExec(t, db, `INSERT INTO d SELECT x, CASE WHEN x % 6 = 0 THEN 'opt-' || x END,
		CASE WHEN x % 40 = 0 THEN repeat('n' || x, 20) END FROM generate_series(1, 20000) AS g(x)`)
	// wide holds values that are never inline.
	mustExec(t, db, "CREATE TABLE wide (id bigint PRIMARY KEY, n integer, body text)")
	mustExec(t, db, `INSERT INTO wide SELECT g, 0, CASE WHEN g % 2 = 0
		THEN (SELECT string_agg(md5(CAST(g * 1000 + i AS text)), '' ORDER BY i) FROM generate_series(1, 300) i)
		ELSE repeat(md5(CAST(g AS text)), 300) END FROM generate_series(1, 300) g`)
	steps := []string{
		"DELETE FROM u WHERE id BETWEEN 2000 AND 9000",              // merges, then splits of the merged leaves
		"UPDATE u SET n = n + 1 WHERE id % 500 = 0",                 // widens n: splits brim-full leaves
		"UPDATE u SET note = 'changed-' || id WHERE id % 50 = 0",    // replaces a value of a distinct column
		"UPDATE u SET note = 'same' WHERE id BETWEEN 9500 AND 9600", // repeats in a distinct column
		"INSERT INTO u SELECT x, 5, 'again-' || x, 'tag-9' FROM generate_series(3000, 3400) AS g(x)",
		"DELETE FROM u WHERE id BETWEEN 9400 AND 15000",
		"DELETE FROM u WHERE id % 3 = 0",
		"UPDATE wide SET n = n + 1 WHERE id = 150",
		"DELETE FROM d WHERE id BETWEEN 3000 AND 11000",
		"DELETE FROM d WHERE id % 4 = 1",
		"DELETE FROM wide WHERE id BETWEEN 40 AND 260",
		"UPDATE wide SET n = 7 WHERE id % 9 = 0",
	}
	for _, sql := range steps {
		mustExec(t, db, sql)
	}
	// One transaction: later statements edit the decoded leaves the earlier merges and splits built,
	// whose facts with a distinct column are dropped on the first insert or replacement.
	sess := db.Session(SessionOptions{})
	defer sess.Close()
	for _, sql := range []string{
		"BEGIN",
		"DELETE FROM u WHERE id BETWEEN 16000 AND 18500",
		"UPDATE u SET note = 'moved-' || id WHERE id BETWEEN 15990 AND 18520",
		"INSERT INTO u SELECT x, 1, 'new-' || x, 'tag-1' FROM generate_series(16000, 16400) AS g(x)",
		"UPDATE u SET n = 100000 WHERE id BETWEEN 18400 AND 19000",
		"DELETE FROM u WHERE id BETWEEN 16100 AND 19500",
		"DELETE FROM d WHERE id BETWEEN 12000 AND 15000",
		"DELETE FROM d WHERE id BETWEEN 15001 AND 15400 AND opt IS NOT NULL",
		"DELETE FROM d WHERE id BETWEEN 15001 AND 16000",
		"INSERT INTO d SELECT x, 'back-' || x, NULL FROM generate_series(12000, 12300) AS g(x)",
		"COMMIT",
	} {
		mustExec(t, sess, sql)
	}
	mustExec(t, db, "DELETE FROM u")
	mustExec(t, db, "DELETE FROM wide")
	mustExec(t, db, "DELETE FROM d")
}
