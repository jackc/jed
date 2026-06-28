package main

import (
	"log"

	jed "github.com/jackc/jed/impl/go"
)

func main() {
	db, err := jed.OpenDatabase("bank.jed")
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()

	// Update runs a read-write transaction on the handle: it commits on success and rolls back if the
	// callback returns an error — so the two writes are atomic. (Begin/Commit/Rollback is the explicit
	// form; View is the read-only sibling.)
	err = db.Update(func(tx *jed.Transaction) error {
		if _, err := tx.Execute("UPDATE account SET balance = balance - 100 WHERE id = 1", nil); err != nil {
			return err
		}
		_, err := tx.Execute("UPDATE account SET balance = balance + 100 WHERE id = 2", nil)
		return err
	})
	if err != nil {
		log.Fatal(err)
	}
}
