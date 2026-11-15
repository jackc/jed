package jed

import (
	"context"
	"errors"
	"runtime"
	"testing"
	"time"
)

func requireContextError(t *testing.T, err, want error) {
	t.Helper()
	var ee *EngineError
	if !errors.As(err, &ee) || ee.State != QueryCanceled || !errors.Is(err, want) {
		t.Fatalf("want 57014 wrapping %v, got %v", want, err)
	}
}

func TestSessionCallbackPanicCleanup(t *testing.T) {
	for _, writable := range []bool{false, true} {
		t.Run(map[bool]string{false: "view", true: "update"}[writable], func(t *testing.T) {
			db := memDB()
			defer db.Close()
			s := mustSession(db, SessionOptions{})
			defer s.Close()
			mustExec(t, db, "CREATE TABLE t (id i64 PRIMARY KEY)")
			marker := &struct{}{}
			func() {
				defer func() {
					if got := recover(); got != marker {
						t.Fatalf("panic changed: %v", got)
					}
				}()
				body := func(tx *Transaction) error {
					if writable {
						if _, err := tx.Exec(t.Context(), "INSERT INTO t VALUES (1)"); err != nil {
							t.Fatal(err)
						}
					}
					panic(marker)
				}
				if writable {
					_ = s.Update(t.Context(), body)
				} else {
					_ = s.View(t.Context(), body)
				}
			}()
			if s.InTransaction() {
				t.Fatal("panic left the transaction open")
			}
			if !db.core.writeMu.TryLock() {
				t.Fatal("panic retained the writer gate")
			}
			db.core.writeMu.Unlock()
			if len(db.core.live) != 0 {
				t.Fatal("panic retained a read pin")
			}
			if err := s.Commit(); err != nil {
				t.Fatal(err)
			}
			if n := txCount(t, db, "t"); n != 0 {
				t.Fatalf("panic's writes survived: %d", n)
			}
			mustExec(t, s, "INSERT INTO t VALUES (2)")
			mustExec(t, db, "INSERT INTO t VALUES (3)")
		})
	}
}

// Observe registration under the gate's mutex, rather than sleeping and assuming
// the operation has reached the lock. The blocker remains held until after cancel.
func waitForLocalWriter(t *testing.T, db *Database) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		g := &db.core.writeMu
		g.mu.Lock()
		waiting := g.changed != nil
		g.mu.Unlock()
		if waiting {
			return
		}
		runtime.Gosched()
	}
	t.Fatal("operation did not reach the writer gate")
}

func TestContextCancelsLocalWriterWait(t *testing.T) {
	for _, op := range []string{"exec", "prepared", "begin", "update", "script"} {
		t.Run(op, func(t *testing.T) {
			db := memDB()
			defer db.Close()
			mustExec(t, db, "CREATE TABLE t (id i64 PRIMARY KEY)")
			blocker := mustSession(db, SessionOptions{})
			defer blocker.Close()
			if err := blocker.Begin(t.Context(), true); err != nil {
				t.Fatal(err)
			}
			s := mustSession(db, SessionOptions{})
			defer s.Close()
			stmt, err := db.Prepare("INSERT INTO t VALUES (1)")
			if err != nil {
				t.Fatal(err)
			}
			ctx, cancel := context.WithCancel(t.Context())
			defer cancel()
			finished := make(chan error, 1)
			go func() {
				var err error
				switch op {
				case "exec":
					_, err = db.Exec(ctx, "INSERT INTO t VALUES (1)")
				case "prepared":
					_, err = s.ExecPrepared(ctx, stmt)
				case "begin":
					err = s.Begin(ctx, true)
				case "update":
					err = db.Update(ctx, func(*Transaction) error { return errors.New("callback ran") })
				case "script":
					_, err = s.ExecuteScript(ctx, "INSERT INTO t VALUES (1)")
				}
				finished <- err
			}()
			waitForLocalWriter(t, db)
			cancel()
			select {
			case err := <-finished:
				requireContextError(t, err, context.Canceled)
			case <-time.After(5 * time.Second):
				t.Fatal("canceled waiter stayed blocked")
			}
			if s.InTransaction() {
				t.Fatal("canceled acquisition opened a transaction")
			}
			if err := blocker.Rollback(); err != nil {
				t.Fatal(err)
			}
			mustExec(t, db, "INSERT INTO t VALUES (2)")
			if n := txCount(t, db, "t"); n != 1 {
				t.Fatalf("canceled write ran: %d", n)
			}
		})
	}
}

func TestContextDeadlineErrorIdentity(t *testing.T) {
	db := memDB()
	defer db.Close()
	s := mustSession(db, SessionOptions{})
	defer s.Close()
	ctx, cancel := context.WithDeadline(t.Context(), time.Unix(0, 0))
	defer cancel()
	var n int64
	requireContextError(t, db.QueryRow(ctx, "SELECT 1").Scan(&n), context.DeadlineExceeded)
	_, err := s.Exec(ctx, "SELECT 1")
	requireContextError(t, err, context.DeadlineExceeded)
	requireContextError(t, s.Begin(ctx, true), context.DeadlineExceeded)
	requireContextError(t, db.View(ctx, func(*Transaction) error { t.Fatal("callback ran"); return nil }), context.DeadlineExceeded)
	_, err = db.ExecuteScript(ctx, "")
	requireContextError(t, err, context.DeadlineExceeded)
	if s.InTransaction() {
		t.Fatal("canceled Begin opened a transaction")
	}
}

func TestCallbackContextCanceledBeforeCommit(t *testing.T) {
	for _, session := range []bool{false, true} {
		db := memDB()
		mustExec(t, db, "CREATE TABLE t (id i64 PRIMARY KEY)")
		s := mustSession(db, SessionOptions{})
		ctx, cancel := context.WithCancel(t.Context())
		body := func(tx *Transaction) error {
			if _, err := tx.Exec(t.Context(), "INSERT INTO t VALUES (1)"); err != nil {
				t.Fatal(err)
			}
			cancel()
			return nil
		}
		var err error
		if session {
			err = s.Update(ctx, body)
		} else {
			err = db.Update(ctx, body)
		}
		requireContextError(t, err, context.Canceled)
		if s.InTransaction() {
			t.Fatal("canceled callback retained its block")
		}
		if n := txCount(t, db, "t"); n != 0 {
			t.Fatalf("canceled callback committed %d rows", n)
		}
		s.Close()
		if err := db.Close(); err != nil {
			t.Fatal(err)
		}
	}
}

func TestScriptContextCancellationRollback(t *testing.T) {
	for _, existing := range []bool{false, true} {
		ctx, cancel := context.WithCancel(t.Context())
		gate := NewHostFunction("cancel_script", []string{"i64"}, "i64", func(a []Value) (Value, error) {
			cancel()
			return a[0], nil
		})
		db := memDBIn(envWith(t, gate))
		mustExec(t, db, "CREATE TABLE t (id i64 PRIMARY KEY)")
		s := mustSession(db, SessionOptions{})
		if existing {
			if err := s.Begin(t.Context(), true); err != nil {
				t.Fatal(err)
			}
			mustExec(t, s, "INSERT INTO t VALUES (99)")
		}
		_, err := s.ExecuteScript(ctx, "INSERT INTO t VALUES (1); SELECT cancel_script(2); INSERT INTO t VALUES (3)")
		requireContextError(t, err, context.Canceled)
		if existing {
			if s.engine.session.tx == nil || !s.engine.session.tx.failed {
				t.Fatal("script did not abort caller's block")
			}
		} else if s.InTransaction() {
			t.Fatal("script did not end its wrapper")
		}
		if err := s.Rollback(); err != nil {
			t.Fatal(err)
		}
		if n := txCount(t, db, "t"); n != 0 {
			t.Fatalf("script committed %d rows", n)
		}
		s.Close()
		if err := db.Close(); err != nil {
			t.Fatal(err)
		}
	}
}

func TestCancellationAfterPublicationPreservesSuccess(t *testing.T) {
	// afterPersistHook is global; keep this test non-parallel like its other users.
	for _, op := range []string{"exec", "returning", "prepared", "update", "script"} {
		t.Run(op, func(t *testing.T) {
			db := memDB()
			defer db.Close()
			mustExec(t, db, "CREATE TABLE t (id i64 PRIMARY KEY)")
			ctx, cancel := context.WithCancel(t.Context())
			defer cancel()
			afterPersistHook = cancel
			defer func() { afterPersistHook = nil }()
			var err error
			switch op {
			case "exec":
				_, err = db.Exec(ctx, "INSERT INTO t VALUES (1)")
			case "returning":
				var id int64
				err = db.QueryRow(ctx, "INSERT INTO t VALUES (1) RETURNING id").Scan(&id)
				if err == nil && id != 1 {
					t.Fatalf("id = %d", id)
				}
			case "prepared":
				stmt, e := db.Prepare("INSERT INTO t VALUES (1)")
				if e != nil {
					t.Fatal(e)
				}
				_, err = db.ExecPrepared(ctx, stmt)
			case "update":
				err = db.Update(ctx, func(tx *Transaction) error { _, e := tx.Exec(ctx, "INSERT INTO t VALUES (1)"); return e })
			case "script":
				_, err = db.ExecuteScript(ctx, "INSERT INTO t VALUES (1)")
			}
			if err != nil {
				t.Fatalf("committed operation reported failure: %v", err)
			}
			if ctx.Err() == nil {
				t.Fatal("publication hook did not run")
			}
			if n := txCount(t, db, "t"); n != 1 {
				t.Fatalf("committed rows = %d", n)
			}
		})
	}
}
