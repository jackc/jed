//! The structured error surface (design.md §8). Every variant preserves the engine's
//! [`jed::EngineError`] underneath where relevant, so a caller can still branch on the
//! SQLSTATE while getting migration context.

use std::fmt;

use jed::EngineError;

/// An error raised by the migration library.
#[derive(Debug)]
pub enum MigrateError {
    /// A load-time failure: a malformed file, a gap or duplicate in the sequence numbers,
    /// an empty forward half, an invalid version-table name, or an unreadable source
    /// (design.md §7/§8). Raised before any statement runs.
    Load(String),

    /// A statement in a migration failed (design.md §8 — the tern `MigrationPgError`
    /// analogue). Carries the migration `name`, the `direction` (`"up"`/`"down"`), the
    /// failing `statement` text, and the underlying engine error.
    Migration {
        name: String,
        direction: &'static str,
        statement: String,
        source: EngineError,
    },

    /// A down-migration was requested through a migration that has no down half.
    Irreversible { sequence: u32, name: String },

    /// A target version, or the version read from the table, is outside `0 … N`. `whence`
    /// is `"target"` or `"database"`.
    BadVersion {
        version: i64,
        n: u32,
        whence: &'static str,
    },

    /// An engine error that is not tied to a specific migration statement (e.g. the version
    /// table read/write, or ensuring the table exists).
    Engine(EngineError),

    /// A filesystem error while loading a directory or scaffolding a new migration.
    Io(String),
}

impl MigrateError {
    /// The underlying engine SQLSTATE, when this error wraps one — a convenience so callers
    /// need not match on the variant to branch on the SQL state.
    pub fn sql_state(&self) -> Option<&'static str> {
        match self {
            MigrateError::Migration { source, .. } | MigrateError::Engine(source) => {
                Some(source.code())
            }
            _ => None,
        }
    }
}

impl fmt::Display for MigrateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MigrateError::Load(msg) => write!(f, "{msg}"),
            MigrateError::Migration {
                name,
                direction,
                statement,
                source,
            } => {
                write!(
                    f,
                    "migration {name:?} ({direction}) failed: {source}\n  in statement: {statement}"
                )
            }
            MigrateError::Irreversible { sequence, name } => write!(
                f,
                "migration {sequence} ({name:?}) is irreversible: it has no down migration"
            ),
            MigrateError::BadVersion { version, n, whence } => {
                write!(f, "{whence} version {version} is out of range 0 … {n}")
            }
            MigrateError::Engine(e) => write!(f, "{e}"),
            MigrateError::Io(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for MigrateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MigrateError::Migration { source, .. } | MigrateError::Engine(source) => Some(source),
            _ => None,
        }
    }
}

impl From<EngineError> for MigrateError {
    fn from(e: EngineError) -> Self {
        MigrateError::Engine(e)
    }
}
