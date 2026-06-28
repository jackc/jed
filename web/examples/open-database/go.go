package main

import (
	"fmt"
	"log"

	jed "github.com/jackc/jed/impl/go"
)

func main() {
	// Open a database. CreateDatabase/OpenDatabase return a *Database — the handle you run SQL
	// through. A path gives a single-file database on disk; jed.NewDatabase() is a transient
	// in-memory one. Writes accumulate until an explicit commit (Close discards uncommitted changes).
	db, err := jed.CreateDatabase("people.jed", jed.DatabaseOptions{PageSize: jed.DefaultPageSize})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()

	mustExec(db, "CREATE TABLE person (id i32 PRIMARY KEY, name text NOT NULL)")
	mustExec(db, "INSERT INTO person VALUES (1, 'Ada'), (2, 'Grace')")
	if err := db.Commit(); err != nil {
		log.Fatal(err)
	}

	// Query returns a row cursor; Execute is for statements that produce no rows.
	rows, err := db.Query("SELECT name FROM person ORDER BY id", nil)
	if err != nil {
		log.Fatal(err)
	}
	for rows.Next() {
		fmt.Println(rows.Row()[0].Render())
	}
}

func mustExec(db *jed.Database, sql string) {
	if _, err := db.Execute(sql, nil); err != nil {
		log.Fatal(err)
	}
}
