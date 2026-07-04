package main

import (
	"context"
	"fmt"
	"log"

	jed "github.com/jackc/jed/impl/go"
)

func main() {
	// Open a database. CreateDatabase/OpenDatabase return a *Database — the handle you run SQL
	// through. A path gives a single-file database on disk; jed.CreateDatabase(jed.CreateOptions{}) (no path) is a transient
	// in-memory one. Each bare Exec autocommits durably (it runs on a fresh session); for a
	// multi-statement transaction use db.Update(...) or mint a Session.
	db, err := jed.CreateDatabase(jed.CreateOptions{Path: "people.jed"})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()

	ctx := context.Background()
	mustExec(db, "CREATE TABLE person (id i32 PRIMARY KEY, name text NOT NULL)")
	mustExec(db, "INSERT INTO person VALUES (1, 'Ada'), (2, 'Grace')")

	// Query returns a row cursor; Exec is for statements that produce no rows.
	rows, err := db.Query(ctx, "SELECT name FROM person ORDER BY id")
	if err != nil {
		log.Fatal(err)
	}
	for rows.Next() {
		fmt.Println(rows.Row()[0].Render())
	}
}

func mustExec(db *jed.Database, sql string) {
	if _, err := db.Exec(context.Background(), sql); err != nil {
		log.Fatal(err)
	}
}
