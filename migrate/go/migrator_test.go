package migrate

import (
	"context"
	"errors"
	"reflect"
	"testing"

	jed "github.com/jackc/jed/impl/go"
)

// memDB returns a fresh in-memory jed database (design.md §10 — tests are fast and hermetic,
// no filesystem needed).
func memDB(t *testing.T) *jed.Database {
	t.Helper()
	db, err := jed.CreateDatabase(jed.CreateOptions{})
	if err != nil {
		t.Fatalf("CreateDatabase: %v", err)
	}
	t.Cleanup(func() { _ = db.Close() })
	return db
}

func newBlogMigrator(t *testing.T, db *jed.Database) *Migrator {
	t.Helper()
	migrations, err := LoadMigrations(blogDir)
	if err != nil {
		t.Fatalf("LoadMigrations: %v", err)
	}
	m, err := NewMigrator(db, migrations, Options{})
	if err != nil {
		t.Fatalf("NewMigrator: %v", err)
	}
	t.Cleanup(m.Close)
	return m
}

func TestMigrateUpThenDownRoundTrips(t *testing.T) {
	db := memDB(t)
	m := newBlogMigrator(t, db)

	// Before anything runs, the version is 0 and only the version table exists.
	if v, err := m.CurrentVersion(); err != nil || v != 0 {
		t.Fatalf("initial version = %d, %v; want 0, nil", v, err)
	}
	if got := db.TableNames(); !reflect.DeepEqual(got, []string{"schema_version"}) {
		t.Fatalf("after ensure, tables = %v; want [schema_version]", got)
	}

	// Migrate all the way up.
	if err := m.Migrate(); err != nil {
		t.Fatalf("Migrate up: %v", err)
	}
	if v, err := m.CurrentVersion(); err != nil || v != 3 {
		t.Fatalf("version after up = %d, %v; want 3, nil", v, err)
	}
	want := []string{"posts", "schema_version", "users"}
	if got := db.TableNames(); !reflect.DeepEqual(got, want) {
		t.Fatalf("tables after up = %v; want %v", got, want)
	}

	// Migrate all the way back down.
	if err := m.MigrateTo(0); err != nil {
		t.Fatalf("MigrateTo(0): %v", err)
	}
	if v, err := m.CurrentVersion(); err != nil || v != 0 {
		t.Fatalf("version after down = %d, %v; want 0, nil", v, err)
	}
	if got := db.TableNames(); !reflect.DeepEqual(got, []string{"schema_version"}) {
		t.Fatalf("tables after down = %v; want [schema_version]", got)
	}

	// And back up again — proves the down halves truly reversed the schema.
	if err := m.Migrate(); err != nil {
		t.Fatalf("Migrate up again: %v", err)
	}
	if got := db.TableNames(); !reflect.DeepEqual(got, want) {
		t.Fatalf("tables after second up = %v; want %v", got, want)
	}
}

func TestMigrateStepwise(t *testing.T) {
	db := memDB(t)
	m := newBlogMigrator(t, db)

	for target := 1; target <= 3; target++ {
		if err := m.MigrateTo(target); err != nil {
			t.Fatalf("MigrateTo(%d): %v", target, err)
		}
		if v, _ := m.CurrentVersion(); v != target {
			t.Fatalf("after MigrateTo(%d), version = %d", target, v)
		}
	}
	// Data seeded by 001 is present (the multi-statement up half ran).
	rows, err := db.Query(context.Background(), "select count(*) from users")
	if err != nil {
		t.Fatalf("query users: %v", err)
	}
	defer rows.Close()
	rows.Next()
	if n, _ := rows.Int(0); n != 2 {
		t.Fatalf("users count = %d, want 2", n)
	}
}

func TestMigrateFastPathNoop(t *testing.T) {
	db := memDB(t)
	m := newBlogMigrator(t, db)
	if err := m.Migrate(); err != nil {
		t.Fatalf("Migrate: %v", err)
	}
	// Migrating to the same version again is a no-op success.
	if err := m.MigrateTo(3); err != nil {
		t.Fatalf("MigrateTo(3) again: %v", err)
	}
	if err := m.Migrate(); err != nil {
		t.Fatalf("Migrate again: %v", err)
	}
}

func TestBadVersionTarget(t *testing.T) {
	db := memDB(t)
	m := newBlogMigrator(t, db)
	var bv *BadVersion
	if err := m.MigrateTo(4); !errors.As(err, &bv) {
		t.Fatalf("MigrateTo(4) error = %v; want *BadVersion", err)
	}
	if err := m.MigrateTo(-1); !errors.As(err, &bv) {
		t.Fatalf("MigrateTo(-1) error = %v; want *BadVersion", err)
	}
}

func TestIrreversibleMigrationDownFails(t *testing.T) {
	db := memDB(t)
	migrations, err := LoadMigrations(irreversibleDir)
	if err != nil {
		t.Fatalf("LoadMigrations: %v", err)
	}
	m, err := NewMigrator(db, migrations, Options{})
	if err != nil {
		t.Fatalf("NewMigrator: %v", err)
	}
	defer m.Close()

	if err := m.Migrate(); err != nil { // up to 2 is fine
		t.Fatalf("Migrate up: %v", err)
	}
	if v, _ := m.CurrentVersion(); v != 2 {
		t.Fatalf("version = %d, want 2", v)
	}
	// Migrating down through the irreversible 002 fails, and the version is unmoved.
	var im *IrreversibleMigration
	if err := m.MigrateTo(0); !errors.As(err, &im) {
		t.Fatalf("MigrateTo(0) error = %v; want *IrreversibleMigration", err)
	}
	if v, _ := m.CurrentVersion(); v != 2 {
		t.Fatalf("version after failed down = %d, want 2 (unmoved)", v)
	}
}

func TestMigrationErrorCarriesContext(t *testing.T) {
	db := memDB(t)
	// A hand-built migration whose up half references a nonexistent table — fails at run time.
	migrations := []Migration{{
		Sequence: 1,
		Name:     "001_bad",
		Up:       "create table ok (id bigint primary key);\ninsert into nope (id) values (1);",
	}}
	m, err := NewMigrator(db, migrations, Options{})
	if err != nil {
		t.Fatalf("NewMigrator: %v", err)
	}
	defer m.Close()

	err = m.Migrate()
	var me *MigrationError
	if !errors.As(err, &me) {
		t.Fatalf("Migrate error = %v (%T); want *MigrationError", err, err)
	}
	if me.Name != "001_bad" || me.Direction != "up" {
		t.Errorf("MigrationError name/direction = %q/%q", me.Name, me.Direction)
	}
	if me.SqlState() != "42P01" { // undefined table
		t.Errorf("SqlState = %q, want 42P01", me.SqlState())
	}
	if me.Statement == "" {
		t.Errorf("MigrationError.Statement is empty; want the failing statement text")
	}
	// The failed step rolled back: no version table advance, and the "ok" table is gone.
	if v, _ := m.CurrentVersion(); v != 0 {
		t.Errorf("version after failed migration = %d, want 0 (rolled back)", v)
	}
	for _, name := range db.TableNames() {
		if name == "ok" {
			t.Errorf("table 'ok' persisted; the failed step should have rolled back")
		}
	}
}

func TestInScriptTransactionControlIsRejected(t *testing.T) {
	db := memDB(t)
	migrations, err := LoadMigrations("../testdata/tx_control")
	if err != nil {
		t.Fatalf("LoadMigrations: %v", err)
	}
	m, err := NewMigrator(db, migrations, Options{})
	if err != nil {
		t.Fatalf("NewMigrator: %v", err)
	}
	defer m.Close()

	err = m.Migrate()
	var me *MigrationError
	if !errors.As(err, &me) {
		t.Fatalf("Migrate error = %v (%T); want *MigrationError", err, err)
	}
	if me.SqlState() != "0A000" { // feature_not_supported — transaction control inside a script
		t.Errorf("SqlState = %q, want 0A000", me.SqlState())
	}
	if v, _ := m.CurrentVersion(); v != 0 {
		t.Errorf("version = %d, want 0 (rolled back)", v)
	}
}

func TestStatus(t *testing.T) {
	db := memDB(t)
	m := newBlogMigrator(t, db)
	s, err := m.Status()
	if err != nil {
		t.Fatalf("Status: %v", err)
	}
	if s != (Status{Current: 0, Target: 3, Pending: 3}) {
		t.Fatalf("initial status = %+v, want {0 3 3}", s)
	}
	if err := m.MigrateTo(2); err != nil {
		t.Fatalf("MigrateTo(2): %v", err)
	}
	s, _ = m.Status()
	if s != (Status{Current: 2, Target: 3, Pending: 1}) {
		t.Fatalf("status = %+v, want {2 3 1}", s)
	}
}

func TestCustomVersionTable(t *testing.T) {
	db := memDB(t)
	migrations, _ := LoadMigrations(blogDir)
	m, err := NewMigrator(db, migrations, Options{VersionTable: "migration_state"})
	if err != nil {
		t.Fatalf("NewMigrator: %v", err)
	}
	defer m.Close()
	if err := m.MigrateTo(1); err != nil {
		t.Fatalf("MigrateTo(1): %v", err)
	}
	// The custom table exists and holds the version; the default one does not.
	names := db.TableNames()
	var hasCustom, hasDefault bool
	for _, n := range names {
		if n == "migration_state" {
			hasCustom = true
		}
		if n == "schema_version" {
			hasDefault = true
		}
	}
	if !hasCustom || hasDefault {
		t.Fatalf("tables = %v; want migration_state present and schema_version absent", names)
	}
}

func TestInvalidVersionTableName(t *testing.T) {
	db := memDB(t)
	migrations, _ := LoadMigrations(blogDir)
	if _, err := NewMigrator(db, migrations, Options{VersionTable: "bad name; drop table x"}); err == nil {
		t.Fatal("expected an error for an invalid version table name")
	}
}
