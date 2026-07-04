// Package migrate is a small, opt-in schema-migration library for jed
// (github.com/jackc/jed/impl/go). It is modeled on tern
// (https://github.com/jackc/tern) and driven entirely through jed's public host
// API — it links the engine in, it is never part of the engine core.
//
// A migrations directory is a flat set of files named <sequence>_<name>.sql, each
// holding an up migration and an optional down migration separated by the magic
// line
//
//	---- create above / drop below ----
//
// Sequence numbers are 1-based and contiguous (1 … N); the sequence is the version.
// Schema state is tracked as a single-integer high-water mark in a version table
// (default "schema_version"): version 0 means no migrations applied, version N means
// migrations 1 … N are applied.
//
// The shared, language-neutral contract (the file format, the version-table
// semantics, and the migrate algorithm) is documented in ../design.md; the Go, Rust,
// and TS packages are three independent implementations of it.
//
// Typical use:
//
//	db, _ := jed.OpenDatabase("app.jed")
//	migrations, _ := migrate.LoadMigrations("migrations")
//	m, _ := migrate.NewMigrator(db, migrations, migrate.Options{})
//	defer m.Close()
//	if err := m.Migrate(); err != nil { // bring the database up to the latest version
//		log.Fatal(err)
//	}
package migrate
