package migrate

import (
	"errors"
	"strings"
	"testing"
	"testing/fstest"
)

const (
	blogDir         = "../testdata/blog"
	irreversibleDir = "../testdata/irreversible"
	ignoredDir      = "../testdata/ignored"
)

func TestLoadBlogSet(t *testing.T) {
	migrations, err := LoadMigrations(blogDir)
	if err != nil {
		t.Fatalf("LoadMigrations: %v", err)
	}
	if len(migrations) != 3 {
		t.Fatalf("want 3 migrations, got %d", len(migrations))
	}
	wantNames := []string{"001_create_users", "002_add_posts", "003_add_email_index"}
	for i, m := range migrations {
		if m.Sequence != i+1 {
			t.Errorf("migration %d: sequence = %d, want %d", i, m.Sequence, i+1)
		}
		if m.Name != wantNames[i] {
			t.Errorf("migration %d: name = %q, want %q", i, m.Name, wantNames[i])
		}
		if m.Irreversible {
			t.Errorf("migration %d (%s): unexpectedly irreversible", i, m.Name)
		}
		if strings.TrimSpace(m.Up) == "" {
			t.Errorf("migration %d (%s): empty up half", i, m.Name)
		}
		if strings.TrimSpace(m.Down) == "" {
			t.Errorf("migration %d (%s): empty down half", i, m.Name)
		}
	}
	// The first migration is multi-statement (a CREATE plus two INSERTs); the up half must
	// carry the inserts and the down half must not.
	if !strings.Contains(migrations[0].Up, "insert into users") {
		t.Errorf("001 up half missing inserts:\n%s", migrations[0].Up)
	}
	if strings.Contains(migrations[0].Down, "insert into users") {
		t.Errorf("001 down half unexpectedly contains inserts:\n%s", migrations[0].Down)
	}
}

func TestLoadIrreversibleSet(t *testing.T) {
	migrations, err := LoadMigrations(irreversibleDir)
	if err != nil {
		t.Fatalf("LoadMigrations: %v", err)
	}
	if len(migrations) != 2 {
		t.Fatalf("want 2 migrations, got %d", len(migrations))
	}
	if migrations[0].Irreversible {
		t.Errorf("001 should be reversible")
	}
	if !migrations[1].Irreversible {
		t.Errorf("002 should be irreversible (no separator)")
	}
	if migrations[1].Down != "" {
		t.Errorf("002 (irreversible) should have an empty down half, got %q", migrations[1].Down)
	}
}

func TestLoadIgnoresNonMigrationFiles(t *testing.T) {
	migrations, err := LoadMigrations(ignoredDir)
	if err != nil {
		t.Fatalf("LoadMigrations: %v", err)
	}
	if len(migrations) != 1 {
		t.Fatalf("want exactly 1 migration (non-matching files ignored), got %d: %+v", len(migrations), migrations)
	}
	if migrations[0].Name != "001_only" {
		t.Errorf("name = %q, want 001_only", migrations[0].Name)
	}
}

func TestLoadMalformedIsRefused(t *testing.T) {
	cases := map[string]string{
		"gap":         "../testdata/malformed/gap",
		"duplicate":   "../testdata/malformed/duplicate",
		"missing_one": "../testdata/malformed/missing_one",
		"empty_up":    "../testdata/malformed/empty_up",
	}
	for name, dir := range cases {
		t.Run(name, func(t *testing.T) {
			_, err := LoadMigrations(dir)
			if err == nil {
				t.Fatalf("%s: expected a load error, got nil", name)
			}
			var le *LoadError
			if !errors.As(err, &le) {
				t.Fatalf("%s: expected *LoadError, got %T: %v", name, err, err)
			}
		})
	}
}

func TestLoadFromFS(t *testing.T) {
	// LoadMigrationsFS is the embedded-source path (an embed.FS behaves like this). A subdir
	// root plus non-migration files must produce the same result as the directory loader.
	fsys := fstest.MapFS{
		"migrations/001_a.sql":  {Data: []byte("create table a (id bigint primary key);\n---- create above / drop below ----\ndrop table a;\n")},
		"migrations/002_b.sql":  {Data: []byte("create table b (id bigint primary key);\n")}, // irreversible
		"migrations/README.md":  {Data: []byte("ignored")},
		"migrations/notes.sql2": {Data: []byte("ignored")},
	}
	migrations, err := LoadMigrationsFS(fsys, "migrations")
	if err != nil {
		t.Fatalf("LoadMigrationsFS: %v", err)
	}
	if len(migrations) != 2 {
		t.Fatalf("want 2 migrations, got %d", len(migrations))
	}
	if migrations[0].Name != "001_a" || migrations[0].Irreversible {
		t.Errorf("001: %+v", migrations[0])
	}
	if !migrations[1].Irreversible {
		t.Errorf("002 should be irreversible")
	}
}

func TestParseRejectsMoreThanOneSeparator(t *testing.T) {
	contents := "create table a (id bigint primary key);\n" +
		Separator + "\n" +
		"drop table a;\n" +
		Separator + "\n" +
		"select 1;\n"
	_, err := parseMigration(1, "001_two_seps", contents)
	if err == nil {
		t.Fatal("expected an error for two separators")
	}
	if !strings.Contains(err.Error(), "separator") {
		t.Errorf("error = %q, want it to mention the separator", err)
	}
}

func TestParseEmptyUpHalf(t *testing.T) {
	contents := "-- only a comment\n\n" + Separator + "\ndrop table x;\n"
	_, err := parseMigration(1, "001_blank", contents)
	if err == nil {
		t.Fatal("expected an error for an empty up half")
	}
	if !strings.Contains(err.Error(), "no SQL in forward migration step") {
		t.Errorf("error = %q, want the tern 'no SQL in forward migration step' message", err)
	}
}
