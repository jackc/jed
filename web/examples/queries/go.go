package main

import (
	"context"
	"fmt"
	"log"

	jed "github.com/jackc/jed/impl/go"
)

// Account maps a result row by column name (the `db:"…"` tags), for RowToStructByName below.
type Account struct {
	ID   int64  `db:"id"`
	Name string `db:"name"`
}

func main() {
	db, err := jed.OpenDatabase("app.jed")
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()

	ctx := context.Background()

	// Exec binds native Go args to $1, $2, … and returns a command tag. No hand-built Value — the
	// pgx-style conversion handles int/string/[]byte/time.Time/…; the context.Context can cancel a
	// long-running statement with 57014.
	if _, err := db.Exec(ctx,
		"INSERT INTO account (id, name, balance) VALUES ($1, $2, $3)",
		1, "Ada", int64(100)); err != nil {
		log.Fatal(err)
	}

	// QueryRow + Scan reads one row into typed destinations (database/sql's shape). It returns
	// jed.ErrNoRows when empty; a *jed.Null[T] (or *any) destination accepts a NULL column.
	var balance int64
	if err := db.QueryRow(ctx, "SELECT balance FROM account WHERE id = $1", 1).Scan(&balance); err != nil {
		log.Fatal(err)
	}
	fmt.Printf("balance = %d\n", balance)

	// Query + the Collect iterator (Go 1.23+) maps each row to a struct by column name and ranges
	// them; the cursor closes on loop exit, and a stream error surfaces as the loop's err.
	rows, err := db.Query(ctx, "SELECT id, name FROM account ORDER BY id")
	if err != nil {
		log.Fatal(err)
	}
	for acct, err := range jed.Collect(rows, jed.RowToStructByName[Account]) {
		if err != nil {
			log.Fatal(err)
		}
		fmt.Printf("%d: %s\n", acct.ID, acct.Name)
	}
}
