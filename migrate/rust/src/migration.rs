//! The migration file format (design.md §4): the up/down split and the non-empty-up rule.

use crate::error::MigrateError;

/// The magic line that splits a migration file's up half from its down half (design.md §4).
/// Kept verbatim from tern; it is itself a valid jed `--` line comment, so a file is inert if
/// ever fed straight to the engine.
pub const SEPARATOR: &str = "---- create above / drop below ----";

/// One loaded migration (design.md §4/§6). `sequence` is 1-based; `name` is the free-form
/// label from the filename. `up` is the forward SQL (never empty). `down` is `None` exactly
/// when the migration is irreversible (the file had no separator).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    pub sequence: u32,
    pub name: String,
    pub up: String,
    pub down: Option<String>,
}

impl Migration {
    /// Whether this migration has no down half (migrating down through it is an error).
    pub fn is_irreversible(&self) -> bool {
        self.down.is_none()
    }
}

/// Split a file's raw `contents` into a [`Migration`] (design.md §4). The file is split on
/// the separator line; text before it is the up half, text after it is the down half. A file
/// with no separator is up-only (irreversible). The up half must be non-empty (only
/// whitespace/comments is a load-time error).
pub(crate) fn parse_migration(
    sequence: u32,
    name: &str,
    contents: &str,
) -> Result<Migration, MigrateError> {
    let (up, down) = split_halves(name, contents)?;
    if !has_sql(&up) {
        return Err(MigrateError::Load(format!(
            "migration {name:?}: no SQL in forward migration step"
        )));
    }
    Ok(Migration {
        sequence,
        name: name.to_string(),
        up,
        down,
    })
}

/// Split `contents` on the [`SEPARATOR`] line. A line is a separator iff its trimmed content
/// equals `SEPARATOR`, so trailing whitespace / a Windows `\r` is tolerated. At most one
/// separator is allowed (design.md §7): a second separator line is a load-time error rather
/// than silently folding into the down half. The down half is `None` when there is no
/// separator (irreversible).
fn split_halves(name: &str, contents: &str) -> Result<(String, Option<String>), MigrateError> {
    let lines: Vec<&str> = contents.split('\n').collect();
    let mut sep: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == SEPARATOR {
            if sep.is_some() {
                return Err(MigrateError::Load(format!(
                    "migration {name:?}: more than one separator line ({SEPARATOR})"
                )));
            }
            sep = Some(i);
        }
    }
    match sep {
        None => Ok((contents.to_string(), None)),
        Some(i) => {
            let up = lines[..i].join("\n");
            let down = lines[i + 1..].join("\n");
            Ok((up, Some(down)))
        }
    }
}

/// Whether `text` contains any SQL beyond whitespace and comments — the check that a
/// migration half is non-empty. Reuses the engine's lexer-aware splitter, which skips
/// comment-only / blank spans, so a half of only comments yields zero statements.
fn has_sql(text: &str) -> bool {
    jed::split_statements(text).next().is_some()
}
