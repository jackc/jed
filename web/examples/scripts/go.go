package main

import (
	"fmt"
	"log"

	jed "github.com/jackc/jed/impl/go"
)

func main() {
	db, err := jed.CreateDatabase(jed.CreateOptions{Path: "app.jed"})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()

	// ExecuteScript runs a whole migration as ONE implicit transaction: split it into statements,
	// run each in order, and commit all-or-nothing (any error rolls the lot back). It DISCARDS
	// result rows — you get back only an O(1) summary (statements run, rows affected, cost), so a
	// huge import never buffers results.
	summary, err := db.ExecuteScript(
		`CREATE TABLE account (id i32 PRIMARY KEY, balance i64);
		 INSERT INTO account VALUES (1, 100), (2, 50);
		 CREATE INDEX account_balance ON account (balance);`)
	if err != nil {
		log.Fatal(err)
	}
	fmt.Printf("ran %d statements\n", summary.StatementsRun)

	// SplitStatements is the library-level primitive (no Database needed). When you DO want each
	// statement's rows, loop it yourself and run the spans through the normal path — the host owns
	// the policy (one transaction or autocommit, drain rows or drop them).
	for stmt := range jed.SplitStatements("SELECT id FROM account; SELECT balance FROM account") {
		if _, err := db.Query(stmt.Text, nil); err != nil {
			log.Fatal(err)
		}
	}
}
