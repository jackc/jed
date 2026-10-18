package main

import (
	"context"
	"fmt"
	"strings"

	jed "github.com/jackc/jed/impl/go"
)

// Real file handoff complements the byte fixtures: each core adopts the other
// core's validated generation and commits before the next opener takes over.
func interop(path, phase string) {
	db, err := jed.OpenDatabaseWithOptions(path, jed.OpenOptions{Locking: jed.LockingExclusive})
	must(err)
	session := db.Session(jed.SessionOptions{})
	ctx := context.Background()
	rows, err := session.Query(ctx, "SELECT id, v, n FROM t ORDER BY id")
	must(err)
	var actual []string
	for rows.Next() {
		var values []string
		for _, value := range rows.Row() {
			values = append(values, value.Render())
		}
		actual = append(actual, strings.Join(values, "|"))
	}
	must(rows.Err())
	rows.Close()
	want := []string{"1|ruby|12.50", "2|" + strings.Repeat("overflow", 500) + "|NULL"}
	if phase == "final" {
		want = []string{"1|ruby-again|12.50", "2|from-go|NULL", "3|go|123.45"}
	}
	if strings.Join(actual, "\n") != strings.Join(want, "\n") {
		panic(fmt.Sprintf("interop %s rows differ: %v", phase, actual))
	}
	view, err := session.Query(ctx, "SELECT count(*) FROM visible")
	must(err)
	if !view.Next() || view.Row()[0].Int != int64(len(want)) {
		panic("persisted view differs")
	}
	view.Close()
	if phase == "update" {
		_, err = session.Exec(ctx, "UPDATE t SET v = 'from-go' WHERE id = 2")
		must(err)
		_, err = session.Exec(ctx, "INSERT INTO t VALUES (3, 'go', 123.45)")
		must(err)
	}
	session.Close()
	must(db.Close())
	fmt.Println("Go file handoff verified:", phase)
}
