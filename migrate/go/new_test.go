package migrate

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestNewMigrationScaffold(t *testing.T) {
	dir := t.TempDir()

	// First migration in an empty directory is 001.
	path, err := NewMigration(dir, "create_users")
	if err != nil {
		t.Fatalf("NewMigration: %v", err)
	}
	if base := filepath.Base(path); base != "001_create_users.sql" {
		t.Fatalf("first file = %q, want 001_create_users.sql", base)
	}
	body, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read stub: %v", err)
	}
	if !strings.Contains(string(body), Separator) {
		t.Errorf("stub missing the separator line:\n%s", body)
	}

	// Next migration takes the next sequence number.
	path2, err := NewMigration(dir, "add_posts")
	if err != nil {
		t.Fatalf("NewMigration 2: %v", err)
	}
	if base := filepath.Base(path2); base != "002_add_posts.sql" {
		t.Fatalf("second file = %q, want 002_add_posts.sql", base)
	}

	// The scaffolded files load as a valid (if empty-bodied) set only once real SQL is added;
	// as written the up halves are comment-only, so loading refuses them — proving the stub is
	// a starting point, not a runnable migration.
	if _, err := LoadMigrations(dir); err == nil {
		t.Errorf("expected loading comment-only stubs to fail (empty up half)")
	}
}

func TestNewMigrationRequiresName(t *testing.T) {
	if _, err := NewMigration(t.TempDir(), ""); err == nil {
		t.Fatal("expected an error for an empty name")
	}
}
