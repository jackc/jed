package jed_test

// Deserialization and file attachment are host-only loading paths. Neither may
// turn already committed GiST keys into a transaction's pending writes (§7).
import (
	"context"
	"errors"
	"path/filepath"
	"testing"

	jed "github.com/jackc/jed/impl/go"
)

func TestGistCommittedLoadHasNoPendingWrites(t *testing.T) {
	for _, mode := range []string{"deserialize", "attach"} {
		t.Run(mode, func(t *testing.T) {
			ctx := context.Background()
			path := filepath.Join(t.TempDir(), "source.jed")
			source, err := jed.CreateDatabase(jed.CreateOptions{Path: path, SkipFsync: true, Locking: jed.LockingNone})
			if err != nil {
				t.Fatal(err)
			}
			defer source.Close()
			for _, sql := range []string{
				"CREATE TABLE t (id i32 PRIMARY KEY, r i32range)",
				"CREATE INDEX t_r ON t USING gist (r)",
				"INSERT INTO t VALUES (1, '[1,5)')",
			} {
				if _, err := source.Exec(ctx, sql); err != nil {
					t.Fatal(err)
				}
			}
			image, err := source.Serialize("main")
			if err != nil {
				t.Fatal(err)
			}
			if err := source.Close(); err != nil {
				t.Fatal(err)
			}
			var db *jed.Database
			name := "t"
			if mode == "deserialize" {
				db, err = jed.DeserializeDatabase(image, jed.DeserializeOptions{})
			} else {
				db, err = jed.CreateDatabase(jed.CreateOptions{})
				if err == nil {
					err = db.Attach("aux", jed.AttachFileWithOptions(path, jed.AttachFileOptions{Locking: jed.LockingNone}), false)
				}
				name = "aux.t"
			}
			if err != nil {
				t.Fatal(err)
			}
			defer db.Close()
			s := db.Session(jed.SessionOptions{MaxQueryMemoryBytes: 1})
			defer s.Close()
			// The point miss retains no input and stages nothing. It must not inherit
			// a pending balance from opening the committed GiST leaf store.
			if _, err := s.Exec(ctx, "DELETE FROM "+name+" WHERE id = 99"); err != nil {
				t.Fatal(err)
			}
			// Real new writes still obey the budget, and failure releases the frame.
			_, err = s.Exec(ctx, "INSERT INTO "+name+" VALUES (2, '[10,20)')")
			var engine *jed.EngineError
			if !errors.As(err, &engine) || engine.Code() != "54P05" {
				t.Fatalf("fresh write: %v", err)
			}
			s.SetMaxQueryMemoryBytes(4096)
			if _, err := s.Exec(ctx, "INSERT INTO "+name+" VALUES (2, '[10,20)')"); err != nil {
				t.Fatal(err)
			}
			s.SetMaxQueryMemoryBytes(1)
			if _, err := s.Exec(ctx, "DELETE FROM "+name+" WHERE id = 99"); err != nil {
				t.Fatal(err)
			}
			s.SetMaxQueryMemoryBytes(4096)
			var n int64
			if err := s.QueryRow(ctx, "SELECT count(*) FROM "+name).Scan(&n); err != nil || n != 2 {
				t.Fatalf("readback %d: %v", n, err)
			}
		})
	}
}
