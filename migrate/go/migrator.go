package migrate

import (
	"context"
	"errors"
	"fmt"
	"regexp"
	"sort"

	jed "github.com/jackc/jed/impl/go"
)

// DefaultVersionTable is the version table name used when Options.VersionTable is empty
// (design.md §5). There is no schema qualifier — jed has no schema namespace.
const DefaultVersionTable = "schema_version"

// versionTablePattern bounds the configurable version table name to a safe identifier
// (optionally qualified by one attached-database name), since it is interpolated into the
// version-table SQL. The name is host-configured, not untrusted input, but validating it
// keeps the interpolation safe by construction.
var versionTablePattern = regexp.MustCompile(`^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)?$`)

// errEmptyVersionTable is the internal signal that the version table exists but holds no
// row (an externally-created, unseeded table); ensureVersionTable seeds it.
var errEmptyVersionTable = errors.New("version table has no row")

// Options configures a Migrator.
type Options struct {
	// VersionTable overrides the default DefaultVersionTable ("schema_version"). It may be a
	// bare name or a name qualified by an attached-database name ("reports.schema_version").
	VersionTable string
}

// Migrator applies a set of migrations to a jed database, tracking progress in a
// single-integer version table (design.md §5/§6). It owns an internal read-write session
// for its lifetime; call Close when done to release the session (Go has no destructor).
type Migrator struct {
	session      *jed.Session
	migrations   []Migration
	versionTable string
}

// NewMigrator builds a Migrator over db and the (already loaded, e.g. via LoadMigrations)
// migrations. It mints one internal read-write session that every step runs on — this is
// load-bearing: jed's bare Database convenience methods mint a fresh session per call
// (spec/design/session.md §2.4), so a step's schema change and its version bump must run on
// one persistent session to land in a single transaction. Call Close to release it.
func NewMigrator(db *jed.Database, migrations []Migration, opts Options) (*Migrator, error) {
	table := opts.VersionTable
	if table == "" {
		table = DefaultVersionTable
	}
	if !versionTablePattern.MatchString(table) {
		return nil, &LoadError{Msg: fmt.Sprintf("invalid version table name %q", table)}
	}
	sorted := append([]Migration(nil), migrations...)
	sort.Slice(sorted, func(i, j int) bool { return sorted[i].Sequence < sorted[j].Sequence })
	if err := validateSequence(sorted); err != nil {
		return nil, err
	}
	return &Migrator{
		session:      db.Session(jed.SessionOptions{}),
		migrations:   sorted,
		versionTable: table,
	}, nil
}

// Close releases the Migrator's internal session. Idempotent.
func (m *Migrator) Close() { m.session.Close() }

// Migrations returns the loaded migration set, ordered by sequence.
func (m *Migrator) Migrations() []Migration { return m.migrations }

// VersionTable returns the version table name in use.
func (m *Migrator) VersionTable() string { return m.versionTable }

// Migrate brings the database up to the latest version (design.md §6) — the dominant
// application-startup case, equivalent to MigrateTo(len(Migrations())).
func (m *Migrator) Migrate() error { return m.MigrateTo(len(m.migrations)) }

// MigrateTo brings the database to an absolute target version in 0 … N by stepping one
// migration at a time (design.md §6). Each step is its own committed transaction, so an
// interrupted run leaves the database at a clean intermediate version (resumable). A target
// outside 0 … N, or a version-table value outside it, is a *BadVersion error.
func (m *Migrator) MigrateTo(target int) error {
	if err := m.ensureVersionTable(); err != nil {
		return err
	}
	n := len(m.migrations)
	if target < 0 || target > n {
		return &BadVersion{Version: target, N: n, Whence: "target"}
	}
	current, err := m.readVersion()
	if err != nil {
		return err
	}
	if current < 0 || current > n {
		return &BadVersion{Version: current, N: n, Whence: "database"}
	}
	if current == target {
		return nil // fast path: already there, no write transaction opened
	}
	if target > current {
		for v := current + 1; v <= target; v++ {
			if err := m.up(v); err != nil {
				return err
			}
		}
	} else {
		for v := current; v > target; v-- {
			if err := m.down(v); err != nil {
				return err
			}
		}
	}
	return nil
}

// Status reports the current version, the target (the latest version N), and the number of
// pending migrations (design.md §9). It ensures the version table exists first.
func (m *Migrator) Status() (Status, error) {
	if err := m.ensureVersionTable(); err != nil {
		return Status{}, err
	}
	current, err := m.readVersion()
	if err != nil {
		return Status{}, err
	}
	n := len(m.migrations)
	pending := n - current
	if pending < 0 {
		pending = 0
	}
	return Status{Current: current, Target: n, Pending: pending}, nil
}

// Status is the result of Migrator.Status.
type Status struct {
	Current int // the version recorded in the version table
	Target  int // the latest available version (N)
	Pending int // how many migrations are not yet applied (Target - Current, clamped at 0)
}

// CurrentVersion ensures the version table exists, then reads and returns the current
// version.
func (m *Migrator) CurrentVersion() (int, error) {
	if err := m.ensureVersionTable(); err != nil {
		return 0, err
	}
	return m.readVersion()
}

// up applies migration v's up half, then bumps the version to v — one atomic step.
func (m *Migrator) up(v int) error {
	mg := m.migrations[v-1]
	return m.runStep(mg, "up", mg.Up, v)
}

// down applies migration v's down half, then bumps the version to v-1 — one atomic step. A
// migration with no down half is irreversible.
func (m *Migrator) down(v int) error {
	mg := m.migrations[v-1]
	if mg.Irreversible {
		return &IrreversibleMigration{Sequence: mg.Sequence, Name: mg.Name}
	}
	return m.runStep(mg, "down", mg.Down, v-1)
}

// runStep runs one migration half plus the version bump in a single write transaction
// (design.md §6). Each statement in the half runs via ExecuteScript joining the open
// transaction, which rejects in-script BEGIN/COMMIT/ROLLBACK (0A000) so the schema change
// and the version bump are one atomic unit. On any error the transaction is rolled back
// (the step made no change) and a *MigrationError naming the migration and failing
// statement is returned.
func (m *Migrator) runStep(mg Migration, direction, sql string, newVersion int) error {
	if err := m.session.Begin(true); err != nil {
		return err
	}
	for span := range jed.SplitStatements(sql) {
		if _, err := m.session.ExecuteScript(span.Text); err != nil {
			_ = m.session.Rollback()
			return &MigrationError{Name: mg.Name, Direction: direction, Statement: span.Text, Err: err}
		}
	}
	bump := fmt.Sprintf("update %s set version = %d", m.versionTable, newVersion)
	if _, err := m.session.ExecuteScript(bump); err != nil {
		_ = m.session.Rollback()
		return fmt.Errorf("migrate: updating %s to version %d: %w", m.versionTable, newVersion, err)
	}
	return m.session.Commit()
}

// ensureVersionTable creates the version table (seeded with 0) if it does not already exist,
// idempotently, in its own committed transaction (design.md §5). Safe to call repeatedly.
func (m *Migrator) ensureVersionTable() error {
	create := fmt.Sprintf("create table %s (version integer not null)", m.versionTable)
	if _, err := m.session.ExecuteScript(create); err != nil {
		// A create against an existing table is DuplicateTable (42P07) — tolerated so ensure is
		// idempotent; any other error is real.
		if !errIsState(err, jed.DuplicateTable) {
			return err
		}
	}
	seed := fmt.Sprintf(
		"insert into %s (version) select 0 where not exists (select 1 from %s)",
		m.versionTable, m.versionTable,
	)
	_, err := m.session.ExecuteScript(seed)
	return err
}

// readVersion reads the single high-water-mark row from the version table.
func (m *Migrator) readVersion() (int, error) {
	rows, err := m.session.Query(context.Background(), fmt.Sprintf("select version from %s", m.versionTable))
	if err != nil {
		return 0, err
	}
	defer rows.Close()
	if !rows.Next() {
		if e := rows.Err(); e != nil {
			return 0, e
		}
		return 0, errEmptyVersionTable
	}
	v, err := rows.Int(0)
	if err != nil {
		return 0, err
	}
	return int(v), nil
}

// errIsState reports whether err wraps a *jed.EngineError with the given SQLSTATE, compared
// by the engine's typed SqlState constant (jed.DuplicateTable, …) rather than a raw string.
func errIsState(err error, state jed.SqlState) bool {
	var ee *jed.EngineError
	return errors.As(err, &ee) && ee.State == state
}
