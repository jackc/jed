//! The Migrator: the version table (design.md §5) and the migrate algorithm (design.md §6).

use jed::{Database, Session, SessionOptions, SqlState, split_statements};

use crate::error::MigrateError;
use crate::load::validate_sequence;
use crate::migration::Migration;

/// The version table name used when [`Options::version_table`] is `None` (design.md §5).
/// There is no schema qualifier — jed has no schema namespace.
pub const DEFAULT_VERSION_TABLE: &str = "schema_version";

/// Configuration for a [`Migrator`].
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Overrides the default `schema_version` table name. May be a bare name or a name
    /// qualified by an attached-database name (`reports.schema_version`).
    pub version_table: Option<String>,
}

/// The result of [`Migrator::status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// The version recorded in the version table.
    pub current: u32,
    /// The latest available version (`N`).
    pub target: u32,
    /// How many migrations are not yet applied (`target - current`, clamped at 0).
    pub pending: u32,
}

/// Applies a set of migrations to a jed database, tracking progress in a single-integer
/// version table (design.md §5/§6). It owns an internal read-write [`Session`] for its
/// lifetime (released when the `Migrator` is dropped).
pub struct Migrator {
    session: Session,
    migrations: Vec<Migration>,
    version_table: String,
}

impl Migrator {
    /// Build a `Migrator` over `db` and the (already loaded, e.g. via
    /// [`load_migrations`](crate::load_migrations)) `migrations`. It mints one internal
    /// read-write session that every step runs on — load-bearing: jed's bare `Database`
    /// convenience methods mint a fresh session per call (session.md §2.4), so a step's
    /// schema change and its version bump must run on one persistent session to land in a
    /// single transaction.
    pub fn new(
        db: &Database,
        mut migrations: Vec<Migration>,
        opts: Options,
    ) -> Result<Migrator, MigrateError> {
        let table = opts
            .version_table
            .unwrap_or_else(|| DEFAULT_VERSION_TABLE.to_string());
        if !valid_version_table(&table) {
            return Err(MigrateError::Load(format!(
                "invalid version table name {table:?}"
            )));
        }
        migrations.sort_by_key(|m| m.sequence);
        validate_sequence(&migrations)?;
        Ok(Migrator {
            session: db.session(SessionOptions::default()),
            migrations,
            version_table: table,
        })
    }

    /// The loaded migration set, ordered by sequence.
    pub fn migrations(&self) -> &[Migration] {
        &self.migrations
    }

    /// The version table name in use.
    pub fn version_table(&self) -> &str {
        &self.version_table
    }

    /// Bring the database up to the latest version (design.md §6) — the dominant
    /// application-startup case, equivalent to `migrate_to(migrations().len())`.
    pub fn migrate(&mut self) -> Result<(), MigrateError> {
        self.migrate_to(self.migrations.len() as u32)
    }

    /// Bring the database to an absolute `target` version in `0 … N` by stepping one
    /// migration at a time (design.md §6). Each step is its own committed transaction, so an
    /// interrupted run leaves the database at a clean intermediate version (resumable). A
    /// target outside `0 … N`, or a version-table value outside it, is [`MigrateError::BadVersion`].
    pub fn migrate_to(&mut self, target: u32) -> Result<(), MigrateError> {
        self.ensure_version_table()?;
        let n = self.migrations.len() as u32;
        if target > n {
            return Err(MigrateError::BadVersion {
                version: target as i64,
                n,
                whence: "target",
            });
        }
        let current = self.read_version()?;
        if current < 0 || current > n as i64 {
            return Err(MigrateError::BadVersion {
                version: current,
                n,
                whence: "database",
            });
        }
        let current = current as u32;
        if current == target {
            return Ok(()); // fast path: already there, no write transaction opened
        }
        if target > current {
            for v in (current + 1)..=target {
                self.up(v)?;
            }
        } else {
            let mut v = current;
            while v > target {
                self.down(v)?;
                v -= 1;
            }
        }
        Ok(())
    }

    /// Report the current version, the target (`N`), and the number of pending migrations
    /// (design.md §9). Ensures the version table exists first.
    pub fn status(&mut self) -> Result<Status, MigrateError> {
        let current = self.current_version()?;
        let n = self.migrations.len() as u32;
        Ok(Status {
            current,
            target: n,
            pending: n.saturating_sub(current),
        })
    }

    /// Ensure the version table exists, then read and return the current version.
    pub fn current_version(&mut self) -> Result<u32, MigrateError> {
        self.ensure_version_table()?;
        let v = self.read_version()?;
        if v < 0 {
            return Err(MigrateError::BadVersion {
                version: v,
                n: self.migrations.len() as u32,
                whence: "database",
            });
        }
        Ok(v as u32)
    }

    /// Apply migration `v`'s up half, then bump the version to `v` — one atomic step.
    fn up(&mut self, v: u32) -> Result<(), MigrateError> {
        let mg = &self.migrations[(v - 1) as usize];
        let (name, sql) = (mg.name.clone(), mg.up.clone());
        self.run_step(&name, "up", &sql, v)
    }

    /// Apply migration `v`'s down half, then bump the version to `v - 1` — one atomic step. A
    /// migration with no down half is irreversible.
    fn down(&mut self, v: u32) -> Result<(), MigrateError> {
        let mg = &self.migrations[(v - 1) as usize];
        match &mg.down {
            None => Err(MigrateError::Irreversible {
                sequence: mg.sequence,
                name: mg.name.clone(),
            }),
            Some(down) => {
                let (name, sql) = (mg.name.clone(), down.clone());
                self.run_step(&name, "down", &sql, v - 1)
            }
        }
    }

    /// Run one migration half plus the version bump in a single write transaction
    /// (design.md §6). Each statement runs via `execute_script` joining the open
    /// transaction, which rejects in-script `BEGIN`/`COMMIT`/`ROLLBACK` (`0A000`) so the
    /// schema change and the version bump are one atomic unit. On any error the transaction
    /// is rolled back and a [`MigrateError::Migration`] naming the migration and failing
    /// statement is returned.
    fn run_step(
        &mut self,
        name: &str,
        direction: &'static str,
        sql: &str,
        new_version: u32,
    ) -> Result<(), MigrateError> {
        self.session.begin(true)?;
        for span in split_statements(sql) {
            if let Err(e) = self.session.execute_script(span.text()) {
                let _ = self.session.rollback();
                return Err(MigrateError::Migration {
                    name: name.to_string(),
                    direction,
                    statement: span.text().to_string(),
                    source: e,
                });
            }
        }
        let bump = format!(
            "update {} set version = {}",
            self.version_table, new_version
        );
        if let Err(e) = self.session.execute_script(&bump) {
            let _ = self.session.rollback();
            return Err(MigrateError::Engine(e));
        }
        self.session.commit()?;
        Ok(())
    }

    /// Create the version table (seeded with `0`) if it does not already exist, idempotently,
    /// in its own committed transaction (design.md §5). Safe to call repeatedly.
    fn ensure_version_table(&mut self) -> Result<(), MigrateError> {
        let create = format!(
            "create table {} (version integer not null)",
            self.version_table
        );
        // A create against an existing table is DuplicateTable (42P07) — tolerated so ensure is
        // idempotent; any other error is real.
        if let Err(e) = self.session.execute_script(&create)
            && e.state != SqlState::DuplicateTable
        {
            return Err(MigrateError::Engine(e));
        }
        let seed = format!(
            "insert into {t} (version) select 0 where not exists (select 1 from {t})",
            t = self.version_table
        );
        self.session.execute_script(&seed)?;
        Ok(())
    }

    /// Read the single high-water-mark row from the version table.
    fn read_version(&mut self) -> Result<i64, MigrateError> {
        let sql = format!("select version from {}", self.version_table);
        match self.session.query_row(&sql, (), |row| row.get::<i64>(0))? {
            Some(v) => Ok(v),
            None => Err(MigrateError::Load(format!(
                "version table {} has no row",
                self.version_table
            ))),
        }
    }
}

/// Whether `name` is a safe (optionally attached-db-qualified) identifier — the version table
/// is interpolated into SQL, so validating it keeps the interpolation safe by construction.
fn valid_version_table(name: &str) -> bool {
    fn valid_ident(s: &str) -> bool {
        let mut chars = s.chars();
        match chars.next() {
            Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
            _ => return false,
        }
        chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
    }
    match name.split_once('.') {
        Some((a, b)) => valid_ident(a) && valid_ident(b),
        None => valid_ident(name),
    }
}
