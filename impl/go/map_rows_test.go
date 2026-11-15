package jed

import (
	"context"
	"errors"
	"testing"
)

func TestMapRowsLifetimeAndErrors(t *testing.T) {
	db := memDB()
	defer db.Close()
	ctx := context.Background()
	if _, err := db.Exec(ctx, "CREATE TABLE item (id i64 PRIMARY KEY)"); err != nil {
		t.Fatal(err)
	}
	if _, err := db.Exec(ctx, "INSERT INTO item VALUES (1), (2), (3)"); err != nil {
		t.Fatal(err)
	}
	for _, mode := range []string{"exhaustion", "break", "mapper error", "stream error", "panic"} {
		t.Run(mode, func(t *testing.T) {
			ctx, cancel := context.WithCancel(ctx)
			defer cancel()
			rows, err := db.Query(ctx, "SELECT id FROM item ORDER BY id")
			if err != nil {
				t.Fatal(err)
			}
			defer rows.Close()
			mapped := 0
			mappingErr := errors.New("mapping failed")
			seq := MapRows(rows, func(r *Rows) (int64, error) {
				mapped++
				if mode == "mapper error" {
					return 0, mappingErr
				}
				return RowTo[int64](r)
			})
			if mapped != 0 || rows.closed {
				t.Fatal("creating iterator consumed or closed rows")
			}
			if mode == "stream error" {
				cancel()
			}
			yielded := 0
			var gotErr error
			func() {
				if mode == "panic" {
					defer func() {
						if recover() != "stop" {
							t.Fatal("iterator did not propagate panic")
						}
					}()
				}
				for value, err := range seq {
					yielded++
					if err != nil {
						gotErr = err
						continue
					} // Iteration must stop even if a caller continues after an error.
					if value != int64(yielded) {
						t.Fatalf("got %d at row %d", value, yielded)
					}
					if mode == "break" {
						break
					}
					if mode == "panic" {
						panic("stop")
					}
				}
			}()
			if !rows.closed {
				t.Fatal("iteration left rows open")
			}
			switch mode {
			case "exhaustion":
				if yielded != 3 || mapped != 3 || gotErr != nil {
					t.Fatalf("yielded=%d mapped=%d error=%v", yielded, mapped, gotErr)
				}
			case "mapper error":
				if yielded != 1 || mapped != 1 || !errors.Is(gotErr, mappingErr) {
					t.Fatalf("yielded=%d mapped=%d error=%v", yielded, mapped, gotErr)
				}
			case "stream error":
				if yielded != 1 || mapped != 0 || !errors.Is(gotErr, context.Canceled) {
					t.Fatalf("yielded=%d mapped=%d error=%v", yielded, mapped, gotErr)
				}
			default:
				if yielded != 1 || mapped != 1 || gotErr != nil {
					t.Fatalf("yielded=%d mapped=%d error=%v", yielded, mapped, gotErr)
				}
			}
		})
	}
}
