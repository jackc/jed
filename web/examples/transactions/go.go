package main

import (
	"log"

	"jed"
)

func main() {
	db, err := jed.Open("bank.jed")
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()

	// Update runs a read-write transaction: it commits on success and rolls back if the callback
	// returns an error — so the two writes are atomic. (Begin/Commit/Rollback is the explicit form.)
	err = db.Update(func(tx *jed.Tx) error {
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
