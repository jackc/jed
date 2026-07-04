//! `jed-migrate` — a small, opt-in schema-migration library for
//! [jed](https://github.com/jackc/jed), modeled on
//! [tern](https://github.com/jackc/tern) and driven entirely through jed's public host
//! API. It links the engine in; it is never part of the engine core.
//!
//! A migrations directory is a flat set of files named `<sequence>_<name>.sql`, each
//! holding an up migration and an optional down migration separated by the magic line
//!
//! ```text
//! ---- create above / drop below ----
//! ```
//!
//! Sequence numbers are 1-based and contiguous (`1 … N`); the sequence *is* the version.
//! Schema state is a single-integer high-water mark in a version table (default
//! `schema_version`): version `0` means no migrations applied, version `N` means
//! migrations `1 … N` are applied.
//!
//! The shared, language-neutral contract (the file format, the version-table semantics,
//! and the migrate algorithm) lives in `../design.md`; the Go, Rust, and TS packages are
//! three independent implementations of it.
//!
//! ```no_run
//! use jed::{Database, OpenOptions};
//! use jed_migrate::{load_migrations, Migrator, Options};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let db = Database::open("app.jed")?;
//! let migrations = load_migrations("migrations".as_ref())?;
//! let mut m = Migrator::new(&db, migrations, Options::default())?;
//! m.migrate()?; // bring the database up to the latest version
//! # Ok(())
//! # }
//! ```

mod error;
mod load;
mod migration;
mod migrator;
mod scaffold;
mod target;

pub use error::MigrateError;
pub use load::{load_migrations, load_migrations_from_entries};
pub use migration::{Migration, SEPARATOR};
pub use migrator::{DEFAULT_VERSION_TABLE, Migrator, Options, Status};
pub use scaffold::new_migration;
pub use target::resolve_targets;
