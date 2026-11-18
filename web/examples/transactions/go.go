package main

import (
	"context"
	"log"

	jed "github.com/jackc/jed/impl/go"
)

func main() {
	db, err := jed.Open("bank.jed", jed.OpenOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()

	ctx := context.Background()

	// BeginFunc runs the callback in a transaction: it commits when the callback returns nil and
	// rolls back when it returns an error or panics — so the two writes are atomic. The zero
	// TxOptions is read-write; jed.TxOptions{AccessMode: jed.ReadOnly} reads one snapshot without
	// waiting for the writer. An empty or SELECT-only writable transaction writes nothing.
	err = db.BeginFunc(ctx, jed.TxOptions{}, func(tx *jed.Transaction) error {
		if _, err := tx.Exec(ctx, "UPDATE account SET balance = balance - 100 WHERE id = 1"); err != nil {
			return err
		}
		_, err := tx.Exec(ctx, "UPDATE account SET balance = balance + 100 WHERE id = 2")
		return err
	})
	if err != nil {
		log.Fatal(err)
	}

	// Begin returns the transaction for a block that spans calls; defer Rollback so every early
	// return ends it (it does nothing after Commit).
	tx, err := db.Begin(ctx, jed.TxOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer tx.Rollback(ctx)
	if _, err := tx.Exec(ctx, "UPDATE account SET balance = balance + 1 WHERE id = 1"); err != nil {
		log.Fatal(err)
	}
	if err := tx.Commit(ctx); err != nil {
		log.Fatal(err)
	}
}
