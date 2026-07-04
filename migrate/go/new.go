package migrate

import (
	"fmt"
	"os"
	"path/filepath"
	"strconv"
)

// stubTemplate is the scaffold written by NewMigration: an empty up half, the separator,
// and an empty down half (design.md §9). Delete the separator and the down half for an
// irreversible migration.
const stubTemplate = `-- Write your forward (up) migration here.


` + Separator + `

-- Write your reverse (down) migration here.
-- Delete this half (and the separator line above) for an irreversible migration.
`

// NewMigration scaffolds the next migration file in dir and returns its path (design.md §9).
// The sequence number is the highest existing sequence plus one (or 1 if the directory is
// empty), zero-padded to three digits, and name is appended as the label:
// dir/NNN_<name>.sql. It needs no database. The directory is created if it does not exist.
func NewMigration(dir, name string) (string, error) {
	if name == "" {
		return "", fmt.Errorf("migrate: new migration needs a name")
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return "", fmt.Errorf("migrate: creating %s: %w", dir, err)
	}
	next, err := nextSequence(dir)
	if err != nil {
		return "", err
	}
	fileName := fmt.Sprintf("%03d_%s.sql", next, name)
	path := filepath.Join(dir, fileName)
	if _, err := os.Stat(path); err == nil {
		return "", fmt.Errorf("migrate: %s already exists", path)
	}
	if err := os.WriteFile(path, []byte(stubTemplate), 0o644); err != nil {
		return "", fmt.Errorf("migrate: writing %s: %w", path, err)
	}
	return path, nil
}

// nextSequence scans dir for existing migration files and returns the next sequence number
// (highest present + 1, or 1 when none are present). Unlike LoadMigrations it does not
// require contiguity — it only needs the maximum so the new file sorts last.
func nextSequence(dir string) (int, error) {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return 0, fmt.Errorf("migrate: reading %s: %w", dir, err)
	}
	max := 0
	for _, entry := range entries {
		if entry.IsDir() {
			continue
		}
		match := fileNamePattern.FindStringSubmatch(entry.Name())
		if match == nil {
			continue
		}
		seq, err := strconv.Atoi(match[1])
		if err != nil {
			continue
		}
		if seq > max {
			max = seq
		}
	}
	return max + 1, nil
}
