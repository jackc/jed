package bench

import (
	"database/sql"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
)

// SQLiteEngine adapts a database/sql SQLite driver to the Engine contract. It is shared
// by bench-sqlite (modernc, pure Go) and bench-sqlite-cgo (mattn) — this package imports
// no driver, so cgo stays confined to the mattn binary (spec/design/benchmarks.md §7).
type SQLiteEngine struct {
	db      *sql.DB
	dataDir string
	dataset string
	scratch string // temp dir holding the scratch file, removed on Close ("" otherwise)
}

var placeholderRe = regexp.MustCompile(`\$(\d+)`)

// rewritePlaceholders turns the corpus's $N into SQLite's explicit-numbered ?N (§3).
func rewritePlaceholders(sqlText string) string {
	return placeholderRe.ReplaceAllString(sqlText, "?$1")
}

// OpenSQLite opens (or, for "scratch", creates) the dataset's SQLite database via the
// named database/sql driver. The connection pool is pinned to one connection so the
// PRAGMAs and the prepared statements live on the same connection. The durability
// PRAGMAs (journal_mode=DELETE, synchronous=FULL — SQLite's classic durable
// configuration) are set unconditionally; they only matter for write benches.
func OpenSQLite(driver, dataDir, dataset string) (*SQLiteEngine, error) {
	e := &SQLiteEngine{dataDir: dataDir, dataset: dataset}
	path := filepath.Join(dataDir, dataset+".sqlite")
	if dataset == "scratch" {
		dir, err := os.MkdirTemp(dataDir, "scratch-")
		if err != nil {
			return nil, err
		}
		e.scratch = dir
		path = filepath.Join(dir, "scratch.sqlite")
	} else if _, err := os.Stat(path); err != nil {
		return nil, StaleErr(dataset, "sqlite")
	}
	db, err := sql.Open(driver, path)
	if err != nil {
		return nil, err
	}
	db.SetMaxOpenConns(1)
	db.SetMaxIdleConns(1)
	for _, pragma := range []string{"PRAGMA journal_mode=DELETE", "PRAGMA synchronous=FULL"} {
		if _, err := db.Exec(pragma); err != nil {
			db.Close()
			return nil, fmt.Errorf("%s: %w", pragma, err)
		}
	}
	e.db = db
	return e, nil
}

func (e *SQLiteEngine) Exec(sqlText string) error {
	_, err := e.db.Exec(sqlText)
	return err
}

func (e *SQLiteEngine) QueryInt(sqlText string) (int64, error) {
	var n int64
	err := e.db.QueryRow(sqlText).Scan(&n)
	return n, err
}

func (e *SQLiteEngine) StoredFingerprint() (string, error) {
	return ReadSidecar(e.dataDir, e.dataset, "sqlite"), nil
}

func (e *SQLiteEngine) Close() error {
	err := e.db.Close()
	if e.scratch != "" {
		os.RemoveAll(e.scratch)
	}
	return err
}

func (e *SQLiteEngine) Prepare(sqlText string) (Stmt, error) {
	stmt, err := e.db.Prepare(rewritePlaceholders(sqlText))
	if err != nil {
		return nil, err
	}
	return &sqliteStmt{stmt: stmt}, nil
}

type sqliteStmt struct {
	stmt *sql.Stmt
}

func (s *sqliteStmt) Exec(args []any) error {
	_, err := s.stmt.Exec(args...)
	return err
}

func (s *sqliteStmt) Query(args []any, sum *Checksum) (int, error) {
	rows, err := s.stmt.Query(args...)
	if err != nil {
		return 0, err
	}
	defer rows.Close()
	cols, err := rows.Columns()
	if err != nil {
		return 0, err
	}
	vals := make([]any, len(cols))
	ptrs := make([]any, len(cols))
	for i := range vals {
		ptrs[i] = &vals[i]
	}
	n := 0
	for rows.Next() {
		if err := rows.Scan(ptrs...); err != nil {
			return n, err
		}
		n++
		if sum == nil {
			continue
		}
		for _, v := range vals {
			switch x := v.(type) {
			case nil:
				sum.Null()
			case int64:
				sum.Int(x)
			case string:
				sum.Text(x)
			case []byte:
				sum.Text(string(x))
			case float64:
				// SQLite sums are INTEGER for integer inputs; a float here means the
				// corpus broke the common-subset type rule (§3).
				return n, fmt.Errorf("unexpected float result %v", x)
			default:
				return n, fmt.Errorf("unexpected result type %T (%v)", v, strconv.Quote(fmt.Sprint(v)))
			}
		}
		sum.EndRow()
	}
	return n, rows.Err()
}

func (s *sqliteStmt) Close() error { return s.stmt.Close() }
