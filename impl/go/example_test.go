package jed_test

import (
	"context"
	"fmt"
	"log"
	"time"

	jed "github.com/jackc/jed/impl/go"
)

func ExampleCreateDatabase() {
	// Set Path: "app.jed" to create a durable file instead.
	db, err := jed.CreateDatabase(jed.CreateOptions{})
	if err != nil {
		log.Fatal(err)
	}
	var answer int64
	if err := db.QueryRow(context.Background(), "SELECT 42").Scan(&answer); err != nil {
		log.Fatal(err)
	}
	fmt.Println(answer)
	if err := db.Close(); err != nil {
		log.Fatal(err)
	}
	// Output: 42
}

func ExampleDatabase_Query() {
	db, err := jed.CreateDatabase(jed.CreateOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()
	rows, err := db.Query(context.Background(), "SELECT $1::text AS name", "Ada")
	if err != nil {
		log.Fatal(err)
	}
	defer rows.Close()
	for rows.Next() {
		var name string
		if err := rows.Scan(&name); err != nil {
			log.Fatal(err)
		}
		fmt.Println(name)
	}
	if err := rows.Err(); err != nil {
		log.Fatal(err)
	}
	// Output: Ada
}

func ExampleDatabase_Session() {
	db, err := jed.CreateDatabase(jed.CreateOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()
	session, err := db.Session(jed.SessionOptions{LockTimeout: 2 * time.Second, TimeZone: "UTC"})
	if err != nil {
		log.Fatal(err)
	}
	defer session.Close()
	if err := session.SetVar("app.tenant", "acme"); err != nil {
		log.Fatal(err)
	}
	var tenant string
	if err := session.QueryRow(context.Background(), "SELECT current_setting('app.tenant')").Scan(&tenant); err != nil {
		log.Fatal(err)
	}
	fmt.Println(tenant, session.LockTimeout())
	// Output: acme 2s
}

func ExampleDatabase_Update() {
	ctx := context.Background()
	db, err := jed.CreateDatabase(jed.CreateOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()
	if _, err := db.Exec(ctx, "CREATE TABLE account (id i64 PRIMARY KEY, balance i64)"); err != nil {
		log.Fatal(err)
	}
	err = db.Update(ctx, func(tx *jed.Transaction) error {
		if _, err := tx.Exec(ctx, "INSERT INTO account VALUES ($1, $2)", 1, 100); err != nil {
			return err
		}
		_, err := tx.Exec(ctx, "UPDATE account SET balance = balance + 25 WHERE id = $1", 1)
		return err
	})
	if err != nil {
		log.Fatal(err)
	}
	var balance int64
	if err := db.QueryRow(ctx, "SELECT balance FROM account WHERE id = $1", 1).Scan(&balance); err != nil {
		log.Fatal(err)
	}
	fmt.Println(balance)
	// Output: 125
}

func ExampleDatabase_QueryRowPrepared() {
	db, err := jed.CreateDatabase(jed.CreateOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()
	stmt, err := db.Prepare("SELECT $1::i64 + 1")
	if err != nil {
		log.Fatal(err)
	}
	for _, n := range []int64{10, 20} {
		var next int64
		if err := db.QueryRowPrepared(context.Background(), stmt, n).Scan(&next); err != nil {
			log.Fatal(err)
		}
		fmt.Println(next)
	}
	// Output:
	// 11
	// 21
}

func ExampleMapRows() {
	type person struct {
		ID   int64
		Name string
	}
	db, err := jed.CreateDatabase(jed.CreateOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()
	rows, err := db.Query(context.Background(), "SELECT 1 AS id, 'Ada' AS name")
	if err != nil {
		log.Fatal(err)
	}
	for p, err := range jed.MapRows(rows, jed.RowToStructByName[person]) {
		if err != nil {
			log.Fatal(err)
		}
		fmt.Println(p.ID, p.Name)
	}
	// Output: 1 Ada
}

// The application defines the one method this helper needs.
type exampleExecer interface {
	Exec(context.Context, string, ...any) (jed.Result, error)
}

func exampleInsert(ctx context.Context, q exampleExecer, id int64) error {
	_, err := q.Exec(ctx, "INSERT INTO item VALUES ($1)", id)
	return err
}

func ExampleQueryer() {
	ctx := context.Background()
	db, err := jed.CreateDatabase(jed.CreateOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()
	if _, err := db.Exec(ctx, "CREATE TABLE item (id i64 PRIMARY KEY)"); err != nil {
		log.Fatal(err)
	}
	session, err := db.Session(jed.SessionOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer session.Close()
	// All three handles satisfy the application's one-method interface.
	if err := exampleInsert(ctx, db, 1); err != nil {
		log.Fatal(err)
	}
	if err := exampleInsert(ctx, session, 2); err != nil {
		log.Fatal(err)
	}
	if err := db.Update(ctx, func(tx *jed.Transaction) error { return exampleInsert(ctx, tx, 3) }); err != nil {
		log.Fatal(err)
	}
	var count int64
	if err := db.QueryRow(ctx, "SELECT count(*) FROM item").Scan(&count); err != nil {
		log.Fatal(err)
	}
	fmt.Println(count)
	// Output: 3
}
