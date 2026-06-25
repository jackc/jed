// bench-setup generates the benchmark databases from bench/corpus/datasets.toml
// (spec/design/benchmarks.md §4/§5): the .jed files via the Go core, the .sqlite files
// via modernc, and the jed_bench_* PostgreSQL databases via pgx CopyFrom — all from the
// same deterministic row streams, fingerprint-gated so an unchanged spec is a no-op.
//
//	bench-setup <corpus_dir> <data_dir> [--engine jed|sqlite|pg|all] [--force]
package main

import (
	"context"
	"database/sql"
	"fmt"
	"jed"
	"os"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/jackc/pgx/v5"
	_ "modernc.org/sqlite"

	"jed-bench/internal/bench"
)

func main() {
	if err := run(); err != nil {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}
}

func run() error {
	args := os.Args[1:]
	var corpusDir, dataDir string
	engine := "all"
	force := false
	var positional []string
	for i := 0; i < len(args); i++ {
		switch args[i] {
		case "--engine":
			i++
			if i >= len(args) {
				return fmt.Errorf("--engine needs a value")
			}
			engine = args[i]
		case "--force":
			force = true
		default:
			positional = append(positional, args[i])
		}
	}
	if len(positional) != 2 {
		return fmt.Errorf("usage: bench-setup <corpus_dir> <data_dir> [--engine jed|sqlite|pg|all] [--force]")
	}
	corpusDir, dataDir = positional[0], positional[1]

	datasets, err := bench.LoadDatasets(corpusDir)
	if err != nil {
		return err
	}
	fingerprint, err := bench.CorpusFingerprint(corpusDir)
	if err != nil {
		return err
	}
	if err := os.MkdirAll(dataDir, 0o755); err != nil {
		return err
	}

	for _, ds := range datasets.Dataset {
		if engine == "all" || engine == "jed" {
			if err := setupJed(dataDir, &ds, fingerprint, force); err != nil {
				return fmt.Errorf("jed/%s: %w", ds.Name, err)
			}
		}
		if engine == "all" || engine == "sqlite" {
			if err := setupSQLite(dataDir, &ds, fingerprint, force); err != nil {
				return fmt.Errorf("sqlite/%s: %w", ds.Name, err)
			}
		}
		if engine == "all" || engine == "pg" {
			if err := setupPG(&ds, fingerprint, force); err != nil {
				return fmt.Errorf("pg/%s: %w", ds.Name, err)
			}
		}
	}
	if engine == "all" || engine == "pg" {
		if err := ensureScratchDB(); err != nil {
			return fmt.Errorf("pg/scratch: %w", err)
		}
	}
	return nil
}

func skip(what string) { fmt.Fprintf(os.Stderr, "%s: fingerprint match, skipping\n", what) }
func load(what string) { fmt.Fprintf(os.Stderr, "%s: generating...\n", what) }

// --- jed ---

func setupJed(dataDir string, ds *bench.Dataset, fingerprint string, force bool) error {
	path := filepath.Join(dataDir, ds.Name+".jed")
	if !force && bench.ReadSidecar(dataDir, ds.Name, "jed") == fingerprint && jedFileReadable(path) {
		skip("jed/" + ds.Name)
		return nil
	}
	load("jed/" + ds.Name)
	os.Remove(path)
	os.Remove(bench.SidecarPath(dataDir, ds.Name, "jed"))

	db, err := jed.Create(path, jed.DefaultDatabaseOptions())
	if err != nil {
		return err
	}
	exec := func(sql string) error {
		_, err := db.ExecuteSQL(sql, nil)
		return err
	}
	for _, t := range ds.Table {
		if !t.AppliesTo("jed") {
			continue
		}
		ddl := t.DDL("jed")
		if err := exec(ddl[0]); err != nil { // CREATE TABLE
			return err
		}
		// All rows in one explicit block — one staging set, one durable commit.
		if err := exec("BEGIN"); err != nil {
			return err
		}
		stream := bench.NewRowStream(&t)
		const batch = 500
		var sb strings.Builder
		for {
			sb.Reset()
			n := 0
			for ; n < batch; n++ {
				row := stream.Next()
				if row == nil {
					break
				}
				if n == 0 {
					sb.WriteString("INSERT INTO " + t.Name + " VALUES ")
				} else {
					sb.WriteString(", ")
				}
				writeLiteralRow(&sb, row)
			}
			if n == 0 {
				break
			}
			if err := exec(sb.String()); err != nil {
				return err
			}
			if n < batch {
				break
			}
		}
		if err := exec("COMMIT"); err != nil {
			return err
		}
		for _, ddlStmt := range ddl[1:] { // CREATE INDEX (autocommit)
			if err := exec(ddlStmt); err != nil {
				return err
			}
		}
	}
	return bench.WriteSidecar(dataDir, ds.Name, "jed", fingerprint)
}

// jedFileReadable reports whether path is a .jed file the current core can open. A matching
// fingerprint alone does not authorize a skip: the corpus fingerprint covers datasets.toml
// but NOT jed's on-disk format version (fingerprint.go), so a format bump leaves a stale .jed
// that the current core rejects (XX001) — regenerate whenever the existing file no longer opens.
func jedFileReadable(path string) bool {
	db, err := jed.Open(path)
	if err != nil {
		return false
	}
	_ = db.Close()
	return true
}

// writeLiteralRow renders one generated row as a SQL VALUES tuple. Generated text is
// pure a-z (benchmarks.md §4), so no quoting subtleties exist. An []int64 renders as the
// array text literal '{1,2,3}' (empty '{}'), which jed coerces to the column's array type.
func writeLiteralRow(sb *strings.Builder, row []any) {
	sb.WriteByte('(')
	for i, v := range row {
		if i > 0 {
			sb.WriteString(", ")
		}
		switch x := v.(type) {
		case int64:
			sb.WriteString(strconv.FormatInt(x, 10))
		case string:
			sb.WriteByte('\'')
			sb.WriteString(x)
			sb.WriteByte('\'')
		case []int64:
			sb.WriteString("'{")
			for j, e := range x {
				if j > 0 {
					sb.WriteByte(',')
				}
				sb.WriteString(strconv.FormatInt(e, 10))
			}
			sb.WriteString("}'")
		default:
			panic(fmt.Sprintf("unsupported literal type %T", v))
		}
	}
	sb.WriteByte(')')
}

// --- SQLite ---

func setupSQLite(dataDir string, ds *bench.Dataset, fingerprint string, force bool) error {
	anyApplies := false
	for i := range ds.Table {
		if ds.Table[i].AppliesTo("sqlite") {
			anyApplies = true
			break
		}
	}
	if !anyApplies {
		// e.g. the gin dataset (array table, jed+postgres only) — no SQLite file at all.
		fmt.Fprintf(os.Stderr, "sqlite/%s: no SQLite-applicable tables, skipping\n", ds.Name)
		return nil
	}
	path := filepath.Join(dataDir, ds.Name+".sqlite")
	if !force && bench.ReadSidecar(dataDir, ds.Name, "sqlite") == fingerprint {
		if _, err := os.Stat(path); err == nil {
			skip("sqlite/" + ds.Name)
			return nil
		}
	}
	load("sqlite/" + ds.Name)
	os.Remove(path)
	os.Remove(bench.SidecarPath(dataDir, ds.Name, "sqlite"))

	db, err := sql.Open("sqlite", path)
	if err != nil {
		return err
	}
	defer db.Close()
	db.SetMaxOpenConns(1)
	// Load-speed pragmas only — the harness re-opens with the durable configuration.
	for _, pragma := range []string{"PRAGMA journal_mode=MEMORY", "PRAGMA synchronous=OFF"} {
		if _, err := db.Exec(pragma); err != nil {
			return err
		}
	}
	for _, t := range ds.Table {
		if !t.AppliesTo("sqlite") {
			continue
		}
		ddl := t.DDL("sqlite")
		if _, err := db.Exec(ddl[0]); err != nil {
			return err
		}
		tx, err := db.Begin()
		if err != nil {
			return err
		}
		placeholders := make([]string, len(t.Column))
		for i := range placeholders {
			placeholders[i] = "?" + strconv.Itoa(i+1)
		}
		ins, err := tx.Prepare("INSERT INTO " + t.Name + " VALUES (" + strings.Join(placeholders, ", ") + ")")
		if err != nil {
			return err
		}
		stream := bench.NewRowStream(&t)
		for row := stream.Next(); row != nil; row = stream.Next() {
			if _, err := ins.Exec(row...); err != nil {
				return err
			}
		}
		if err := tx.Commit(); err != nil {
			return err
		}
		for _, ddlStmt := range ddl[1:] {
			if _, err := db.Exec(ddlStmt); err != nil {
				return err
			}
		}
	}
	if err := db.Close(); err != nil {
		return err
	}
	return bench.WriteSidecar(dataDir, ds.Name, "sqlite", fingerprint)
}

// --- PostgreSQL ---

func setupPG(ds *bench.Dataset, fingerprint string, force bool) error {
	ctx := context.Background()
	dbName := "jed_bench_" + ds.Name

	if !force && pgFingerprint(ctx, dbName) == fingerprint {
		skip("pg/" + ds.Name)
		return nil
	}
	load("pg/" + ds.Name)

	admin, err := pgx.Connect(ctx, "dbname=postgres")
	if err != nil {
		return err
	}
	if _, err := admin.Exec(ctx, "DROP DATABASE IF EXISTS "+dbName+" WITH (FORCE)"); err != nil {
		admin.Close(ctx)
		return err
	}
	if _, err := admin.Exec(ctx, "CREATE DATABASE "+dbName); err != nil {
		admin.Close(ctx)
		return err
	}
	admin.Close(ctx)

	conn, err := pgx.Connect(ctx, "dbname="+dbName)
	if err != nil {
		return err
	}
	defer conn.Close(ctx)
	for _, t := range ds.Table {
		if !t.AppliesTo("postgres") {
			continue
		}
		ddl := t.DDL("postgres")
		if _, err := conn.Exec(ctx, ddl[0]); err != nil {
			return err
		}
		cols := make([]string, len(t.Column))
		for i, c := range t.Column {
			cols[i] = c.Name
		}
		if _, err := conn.CopyFrom(ctx, pgx.Identifier{t.Name}, cols, &copySource{stream: bench.NewRowStream(&t)}); err != nil {
			return err
		}
		for _, ddlStmt := range ddl[1:] {
			if _, err := conn.Exec(ctx, ddlStmt); err != nil {
				return err
			}
		}
	}
	if _, err := conn.Exec(ctx, "CREATE TABLE _bench_meta (key text PRIMARY KEY, value text)"); err != nil {
		return err
	}
	_, err = conn.Exec(ctx, "INSERT INTO _bench_meta VALUES ('fingerprint', $1)", fingerprint)
	return err
}

// pgFingerprint reads the stored fingerprint, or "" if the database/table/row is absent.
func pgFingerprint(ctx context.Context, dbName string) string {
	conn, err := pgx.Connect(ctx, "dbname="+dbName)
	if err != nil {
		return ""
	}
	defer conn.Close(ctx)
	var fp string
	if err := conn.QueryRow(ctx, "SELECT value FROM _bench_meta WHERE key = 'fingerprint'").Scan(&fp); err != nil {
		return ""
	}
	return fp
}

// ensureScratchDB creates the empty jed_bench_scratch database used by write_durable
// benches (the harness drops/recreates the scratch table per run; the database persists).
func ensureScratchDB() error {
	ctx := context.Background()
	admin, err := pgx.Connect(ctx, "dbname=postgres")
	if err != nil {
		return err
	}
	defer admin.Close(ctx)
	var exists bool
	if err := admin.QueryRow(ctx, "SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = 'jed_bench_scratch')").Scan(&exists); err != nil {
		return err
	}
	if exists {
		return nil
	}
	fmt.Fprintln(os.Stderr, "pg/scratch: creating empty database")
	_, err = admin.Exec(ctx, "CREATE DATABASE jed_bench_scratch")
	return err
}

// copySource adapts a deterministic RowStream to pgx.CopyFromSource.
type copySource struct {
	stream *bench.RowStream
	row    []any
}

func (c *copySource) Next() bool {
	c.row = c.stream.Next()
	return c.row != nil
}

func (c *copySource) Values() ([]any, error) { return c.row, nil }
func (c *copySource) Err() error             { return nil }
