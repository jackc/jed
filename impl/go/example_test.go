package jed_test

import (
	"context"
	"errors"
	"fmt"
	"log"
	"time"

	jed "github.com/jackc/jed/impl/go"
)

func ExampleCreate() {
	// Set Path: "app.jed" to create a durable file instead.
	db, err := jed.Create(jed.CreateOptions{})
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
	db, err := jed.Create(jed.CreateOptions{})
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
	db, err := jed.Create(jed.CreateOptions{})
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

func ExampleDatabase_BeginFunc() {
	ctx := context.Background()
	db, err := jed.Create(jed.CreateOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()
	if _, err := db.Exec(ctx, "CREATE TABLE account (id i64 PRIMARY KEY, balance i64)"); err != nil {
		log.Fatal(err)
	}
	// The callback's statements commit together when it returns nil.
	err = db.BeginFunc(ctx, jed.TxOptions{}, func(tx *jed.Transaction) error {
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

func ExampleDatabase_Begin() {
	ctx := context.Background()
	db, err := jed.Create(jed.CreateOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()
	if _, err := db.Exec(ctx, "CREATE TABLE item (id i64 PRIMARY KEY)"); err != nil {
		log.Fatal(err)
	}
	tx, err := db.Begin(ctx, jed.TxOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer tx.Rollback(ctx) // ends the transaction on every early return; a no-op after Commit
	if _, err := tx.Exec(ctx, "INSERT INTO item VALUES (1), (2)"); err != nil {
		log.Fatal(err)
	}
	if err := tx.Commit(ctx); err != nil {
		log.Fatal(err)
	}
	var count int64
	if err := db.QueryRow(ctx, "SELECT count(*) FROM item").Scan(&count); err != nil {
		log.Fatal(err)
	}
	fmt.Println(count)
	// Output: 2
}

func ExampleDatabase_QueryRowPrepared() {
	db, err := jed.Create(jed.CreateOptions{})
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
	db, err := jed.Create(jed.CreateOptions{})
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
	db, err := jed.Create(jed.CreateOptions{})
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
	if err := db.BeginFunc(ctx, jed.TxOptions{}, func(tx *jed.Transaction) error { return exampleInsert(ctx, tx, 3) }); err != nil {
		log.Fatal(err)
	}
	var count int64
	if err := db.QueryRow(ctx, "SELECT count(*) FROM item").Scan(&count); err != nil {
		log.Fatal(err)
	}
	fmt.Println(count)
	// Output: 3
}

func ExampleEngineError() {
	ctx := context.Background()
	db, err := jed.Create(jed.CreateOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()
	if _, err := db.Exec(ctx, "CREATE TABLE person (email text PRIMARY KEY)"); err != nil {
		log.Fatal(err)
	}
	if _, err := db.Exec(ctx, "INSERT INTO person VALUES ($1)", "ada@example.com"); err != nil {
		log.Fatal(err)
	}
	_, err = db.Exec(ctx, "INSERT INTO person VALUES ($1)", "ada@example.com")
	var ee *jed.EngineError
	if errors.As(err, &ee) && ee.State == jed.UniqueViolation {
		fmt.Println(ee.Code(), ee.TableName, ee.Detail)
	}
	// Output: 23505 person Key (email)=(ada@example.com) already exists.
}

func ExampleNull() {
	ctx := context.Background()
	db, err := jed.Create(jed.CreateOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()
	if _, err := db.Exec(ctx, "CREATE TABLE person (id i64 PRIMARY KEY, nickname text)"); err != nil {
		log.Fatal(err)
	}
	// A Null[T] binds SQL NULL when Valid is false.
	if _, err := db.Exec(ctx, "INSERT INTO person VALUES ($1, $2)", 1, jed.Null[string]{}); err != nil {
		log.Fatal(err)
	}
	// Scanning into a Null[T] accepts NULL; a plain *string would fail.
	var nickname jed.Null[string]
	if err := db.QueryRow(ctx, "SELECT nickname FROM person WHERE id = 1").Scan(&nickname); err != nil {
		log.Fatal(err)
	}
	fmt.Println(nickname.Valid)
	// Output: false
}

func ExampleSession_Begin() {
	ctx := context.Background()
	db, err := jed.Create(jed.CreateOptions{})
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
	tx, err := session.Begin(ctx, jed.TxOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer tx.Rollback(ctx)
	if _, err := tx.Exec(ctx, "INSERT INTO item VALUES (1)"); err != nil {
		log.Fatal(err)
	}
	// The transaction sees its own write; other sessions do not until it commits.
	var inside, outside int64
	if err := tx.QueryRow(ctx, "SELECT count(*) FROM item").Scan(&inside); err != nil {
		log.Fatal(err)
	}
	if err := db.QueryRow(ctx, "SELECT count(*) FROM item").Scan(&outside); err != nil {
		log.Fatal(err)
	}
	if err := tx.Commit(ctx); err != nil {
		log.Fatal(err)
	}
	fmt.Println(inside, outside)
	// Output: 1 0
}

func ExampleEnvironmentBuilder_RegisterFunction() {
	ctx := context.Background()
	b := jed.NewEnvironmentBuilder()
	// discount(cents, pct) is strict: a NULL argument returns NULL without calling the kernel.
	discount := jed.NewHostFunction("discount", []string{"i64", "i64"}, "i64",
		func(args []jed.Value) (jed.Value, error) {
			var cents, pct int64
			if err := args[0].Scan(&cents); err != nil {
				return jed.Value{}, err
			}
			if err := args[1].Scan(&pct); err != nil {
				return jed.Value{}, err
			}
			return jed.IntValue(cents - cents*pct/100), nil
		}).WithVolatility(jed.VolatilityImmutable)
	if err := b.RegisterFunction(discount); err != nil {
		log.Fatal(err)
	}
	db, err := jed.Create(jed.CreateOptions{Environment: b.Build()})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()
	var price int64
	if err := db.QueryRow(ctx, "SELECT discount(1250, 20)").Scan(&price); err != nil {
		log.Fatal(err)
	}
	fmt.Println(price)
	// Output: 1000
}

func ExampleDatabase_Attach() {
	ctx := context.Background()
	db, err := jed.Create(jed.CreateOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()
	// Attach a second, empty in-memory database as "scratch"; jed.AttachFile attaches a file.
	if err := db.Attach(ctx, "scratch", jed.AttachMemory(), jed.AttachOptions{}); err != nil {
		log.Fatal(err)
	}
	if _, err := db.Exec(ctx, "CREATE TABLE scratch.note (id i64 PRIMARY KEY, body text)"); err != nil {
		log.Fatal(err)
	}
	if _, err := db.Exec(ctx, "INSERT INTO scratch.note VALUES (1, 'hello')"); err != nil {
		log.Fatal(err)
	}
	var body string
	if err := db.QueryRow(ctx, "SELECT body FROM scratch.note WHERE id = 1").Scan(&body); err != nil {
		log.Fatal(err)
	}
	fmt.Println(body)
	// Output: hello
}

func ExampleDatabase_Query_cancellation() {
	db, err := jed.Create(jed.CreateOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()
	ctx, cancel := context.WithTimeout(context.Background(), time.Minute)
	cancel() // canceled before the query starts, so the example is deterministic
	_, err = db.Query(ctx, "SELECT count(*) FROM generate_series(1, 100000000)")
	var ee *jed.EngineError
	fmt.Println(errors.Is(err, context.Canceled), errors.As(err, &ee) && ee.Code() == "57014")
	// Output: true true
}
