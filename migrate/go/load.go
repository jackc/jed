package migrate

import (
	"io/fs"
	"os"
	"regexp"
	"sort"
	"strconv"
)

// fileNamePattern matches a migration file name: a decimal sequence prefix, an underscore,
// a free-form label, and the .sql extension (design.md §4). Files that do not match are not
// migrations and are silently ignored.
var fileNamePattern = regexp.MustCompile(`^(\d+)_.+\.sql$`)

// LoadMigrations reads and validates the migrations directory at dir from the filesystem
// (design.md §7 — the default source). It returns the migrations ordered by sequence, or a
// *LoadError if the set is malformed (a gap, a duplicate, an empty forward half) before any
// statement runs.
func LoadMigrations(dir string) ([]Migration, error) {
	return LoadMigrationsFS(os.DirFS(dir), ".")
}

// LoadMigrationsFS reads and validates migrations from any fs.FS rooted at root (root "."
// for the FS root), so an embed.FS of compiled-in migrations loads identically to a
// directory (design.md §7 — the embedded source is first-class):
//
//	//go:embed migrations/*.sql
//	var migrationsFS embed.FS
//	migrations, err := migrate.LoadMigrationsFS(migrationsFS, "migrations")
//
// It produces the same ordered (sequence, name, up, down) list as LoadMigrations, so the
// algorithm and the file format are source-agnostic.
func LoadMigrationsFS(fsys fs.FS, root string) ([]Migration, error) {
	if root == "" {
		root = "."
	}
	entries, err := fs.ReadDir(fsys, root)
	if err != nil {
		return nil, &LoadError{Msg: "reading migrations: " + err.Error()}
	}
	var migrations []Migration
	seen := map[int]string{} // sequence -> first file name, for duplicate detection
	for _, entry := range entries {
		if entry.IsDir() {
			continue
		}
		name := entry.Name()
		match := fileNamePattern.FindStringSubmatch(name)
		if match == nil {
			continue // not a migration file — ignore (README, .bak, draft_*.sql, …)
		}
		seq, err := strconv.Atoi(match[1])
		if err != nil {
			return nil, &LoadError{Name: name, Msg: "bad sequence number: " + err.Error()}
		}
		if prev, dup := seen[seq]; dup {
			return nil, &LoadError{Msg: "duplicate sequence " + strconv.Itoa(seq) +
				": " + prev + " and " + name}
		}
		seen[seq] = name

		path := name
		if root != "." {
			path = root + "/" + name
		}
		contents, err := fs.ReadFile(fsys, path)
		if err != nil {
			return nil, &LoadError{Name: name, Msg: "reading file: " + err.Error()}
		}
		m, err := parseMigration(seq, migrationLabel(name), string(contents))
		if err != nil {
			return nil, err
		}
		migrations = append(migrations, m)
	}

	sort.Slice(migrations, func(i, j int) bool {
		return migrations[i].Sequence < migrations[j].Sequence
	})
	if err := validateSequence(migrations); err != nil {
		return nil, err
	}
	return migrations, nil
}

// validateSequence checks that the migrations form the contiguous set 1 … N with no gaps
// and no duplicates (design.md §4/§7). The slice must already be sorted by sequence.
func validateSequence(migrations []Migration) error {
	for i, m := range migrations {
		want := i + 1
		if m.Sequence != want {
			return &LoadError{Msg: "non-contiguous migration sequence: expected " +
				strconv.Itoa(want) + ", found " + strconv.Itoa(m.Sequence) +
				" (" + m.Name + "); sequences must be 1 … N with no gaps"}
		}
	}
	return nil
}

// migrationLabel strips the .sql extension from a file name to form the human-readable
// migration name used in status output and errors (e.g. "001_create_users").
func migrationLabel(fileName string) string {
	return fileName[:len(fileName)-len(".sql")]
}
