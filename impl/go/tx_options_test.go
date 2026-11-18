package jed

import (
	"context"
	"errors"
	"testing"
)

// Host-API surface for Begin / BeginFunc / TxOptions (spec/design/api.md §2.2): the corpus drives
// SQL BEGIN, not these handles.

func txTestDB(t *testing.T) *Database {
	t.Helper()
	db, err := Create(CreateOptions{})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = db.Close() })
	if _, err := db.Exec(context.Background(), "CREATE TABLE t (id i64 PRIMARY KEY)"); err != nil {
		t.Fatal(err)
	}
	return db
}

func count(t *testing.T, q interface {
	QueryRow(context.Context, string, ...any) *Row
},
) int64 {
	t.Helper()
	var n int64
	if err := q.QueryRow(context.Background(), "SELECT count(*) FROM t").Scan(&n); err != nil {
		t.Fatal(err)
	}
	return n
}

func wantState(t *testing.T, err error, want SQLState) {
	t.Helper()
	var ee *EngineError
	if !errors.As(err, &ee) || ee.State != want {
		t.Fatalf("got %v, want SQLSTATE %s", err, want.Code())
	}
}

func TestDatabaseBeginCommitAndRollback(t *testing.T) {
	ctx := context.Background()
	db := txTestDB(t)
	tx, err := db.Begin(ctx, TxOptions{})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := tx.Exec(ctx, "INSERT INTO t VALUES (1)"); err != nil {
		t.Fatal(err)
	}
	if got := count(t, tx); got != 1 {
		t.Fatalf("read-your-writes: %d", got)
	}
	if err := tx.Commit(ctx); err != nil {
		t.Fatal(err)
	}
	if err := tx.Rollback(ctx); err != nil {
		t.Fatalf("Rollback after Commit must be a no-op: %v", err)
	}
	if got := count(t, db); got != 1 {
		t.Fatalf("committed count %d", got)
	}

	tx, err = db.Begin(ctx, TxOptions{})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := tx.Exec(ctx, "INSERT INTO t VALUES (2)"); err != nil {
		t.Fatal(err)
	}
	if err := tx.Rollback(ctx); err != nil {
		t.Fatal(err)
	}
	if got := count(t, db); got != 1 {
		t.Fatalf("rolled-back write visible: %d", got)
	}
	// The ended transaction's session is gone: a statement through it is refused, not autocommitted.
	_, err = tx.Exec(ctx, "INSERT INTO t VALUES (3)")
	wantState(t, err, NoActiveSQLTransaction)
	if got := count(t, db); got != 1 {
		t.Fatalf("statement after end ran: %d", got)
	}
}

func TestReadOnlyTransactionRejectsWrites(t *testing.T) {
	ctx := context.Background()
	db := txTestDB(t)
	tx, err := db.Begin(ctx, TxOptions{AccessMode: ReadOnly})
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback(ctx)
	_, err = tx.Exec(ctx, "INSERT INTO t VALUES (1)")
	wantState(t, err, ReadOnlySQLTransaction)
}

func TestInvalidAccessMode(t *testing.T) {
	db := txTestDB(t)
	_, err := db.Begin(context.Background(), TxOptions{AccessMode: 7})
	wantState(t, err, InvalidParameterValue)
	err = db.BeginFunc(context.Background(), TxOptions{AccessMode: 7}, func(*Transaction) error { return nil })
	wantState(t, err, InvalidParameterValue)
}

func TestCommitOfFailedTransactionReports25P02(t *testing.T) {
	ctx := context.Background()
	db := txTestDB(t)
	tx, err := db.Begin(ctx, TxOptions{})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := tx.Exec(ctx, "INSERT INTO t VALUES (1)"); err != nil {
		t.Fatal(err)
	}
	if _, err := tx.Exec(ctx, "INSERT INTO t VALUES (1)"); err == nil {
		t.Fatal("duplicate key accepted")
	}
	wantState(t, tx.Commit(ctx), InFailedSQLTransaction)
	if got := count(t, db); got != 0 {
		t.Fatalf("failed transaction committed: %d", got)
	}
}

func TestSQLTransactionControlThroughTransactionIs2D000(t *testing.T) {
	ctx := context.Background()
	db := txTestDB(t)
	tx, err := db.Begin(ctx, TxOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback(ctx)
	_, err = tx.Exec(ctx, "COMMIT")
	wantState(t, err, InvalidTransactionTermination)
}

func TestCanceledCommitRollsBack(t *testing.T) {
	db := txTestDB(t)
	tx, err := db.Begin(context.Background(), TxOptions{})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := tx.Exec(context.Background(), "INSERT INTO t VALUES (1)"); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if err := tx.Commit(ctx); !errors.Is(err, context.Canceled) {
		t.Fatalf("got %v, want context.Canceled", err)
	}
	if got := count(t, db); got != 0 {
		t.Fatalf("canceled commit published: %d", got)
	}
}

func TestBeginFunc(t *testing.T) {
	ctx := context.Background()
	db := txTestDB(t)
	err := db.BeginFunc(ctx, TxOptions{}, func(tx *Transaction) error {
		_, err := tx.Exec(ctx, "INSERT INTO t VALUES (1)")
		return err
	})
	if err != nil {
		t.Fatal(err)
	}
	boom := errors.New("boom")
	err = db.BeginFunc(ctx, TxOptions{}, func(tx *Transaction) error {
		if _, err := tx.Exec(ctx, "INSERT INTO t VALUES (2)"); err != nil {
			return err
		}
		return boom
	})
	if !errors.Is(err, boom) {
		t.Fatalf("got %v", err)
	}
	err = db.BeginFunc(ctx, TxOptions{}, func(tx *Transaction) error {
		return tx.Commit(ctx)
	})
	wantState(t, err, InvalidTransactionTermination)
	if got := count(t, db); got != 1 {
		t.Fatalf("count %d, want 1", got)
	}
}

func TestSessionBeginHoldsTheBlock(t *testing.T) {
	ctx := context.Background()
	db := txTestDB(t)
	s, err := db.Session(SessionOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	tx, err := s.Begin(ctx, TxOptions{})
	if err != nil {
		t.Fatal(err)
	}
	if !s.InTransaction() {
		t.Fatal("session not in a transaction")
	}
	if _, err := s.Begin(ctx, TxOptions{}); err == nil {
		t.Fatal("nested Begin accepted")
	}
	// Statements the session runs directly join the open transaction.
	if _, err := s.Exec(ctx, "INSERT INTO t VALUES (1)"); err != nil {
		t.Fatal(err)
	}
	if err := tx.Commit(ctx); err != nil {
		t.Fatal(err)
	}
	if s.InTransaction() {
		t.Fatal("block still open after Commit")
	}
	if got := count(t, db); got != 1 {
		t.Fatalf("count %d", got)
	}
}
