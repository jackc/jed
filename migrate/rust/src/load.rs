//! Loading migrations from a directory or an embedded set (design.md §7 — the source seam).

use std::path::Path;

use crate::error::MigrateError;
use crate::migration::{Migration, parse_migration};

/// Read and validate the migrations directory at `dir` from the filesystem (design.md §7 —
/// the default source). Returns the migrations ordered by sequence, or a load error if the
/// set is malformed (a gap, a duplicate, an empty forward half) before any statement runs.
pub fn load_migrations(dir: &Path) -> Result<Vec<Migration>, MigrateError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| MigrateError::Io(format!("reading {}: {e}", dir.display())))?;
    let mut files: Vec<(String, String)> = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| MigrateError::Io(format!("reading {}: {e}", dir.display())))?;
        let file_type = entry
            .file_type()
            .map_err(|e| MigrateError::Io(format!("stat {:?}: {e}", entry.path())))?;
        if file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if parse_file_name(&name).is_none() {
            continue; // not a migration file — ignore (README, .bak, draft_*.sql, …)
        }
        let contents = std::fs::read_to_string(entry.path())
            .map_err(|e| MigrateError::Io(format!("reading {}: {e}", entry.path().display())))?;
        files.push((name, contents));
    }
    build(files)
}

/// Build a validated migration set from an embedded set of `(file_name, contents)` pairs
/// (design.md §7 — the embedded source is first-class). A host that compiles migrations into
/// the binary produces this list however it likes — `include_str!`, an `include_dir!` table,
/// a build script — and gets the same ordered, validated result as [`load_migrations`], so
/// the algorithm and the file format are source-agnostic:
///
/// ```ignore
/// let migrations = load_migrations_from_entries(&[
///     ("001_create_users.sql", include_str!("../migrations/001_create_users.sql")),
///     ("002_add_posts.sql", include_str!("../migrations/002_add_posts.sql")),
/// ])?;
/// ```
pub fn load_migrations_from_entries(
    entries: &[(&str, &str)],
) -> Result<Vec<Migration>, MigrateError> {
    let mut files: Vec<(String, String)> = Vec::new();
    for (name, contents) in entries {
        if parse_file_name(name).is_none() {
            continue; // a non-migration name is ignored, exactly like the directory loader
        }
        files.push((name.to_string(), contents.to_string()));
    }
    build(files)
}

/// Turn `(file_name, contents)` pairs into a validated, ordered migration set: detect
/// duplicate sequences, parse each half, sort by sequence, and require the contiguous set
/// `1 … N`.
fn build(files: Vec<(String, String)>) -> Result<Vec<Migration>, MigrateError> {
    let mut migrations: Vec<Migration> = Vec::with_capacity(files.len());
    let mut seen: Vec<(u32, String)> = Vec::new();
    for (name, contents) in files {
        let seq = parse_file_name(&name).expect("filtered to migration files above");
        if let Some((_, first)) = seen.iter().find(|(s, _)| *s == seq) {
            return Err(MigrateError::Load(format!(
                "duplicate sequence {seq}: {first} and {name}"
            )));
        }
        seen.push((seq, name.clone()));
        migrations.push(parse_migration(seq, migration_label(&name), &contents)?);
    }
    migrations.sort_by_key(|m| m.sequence);
    validate_sequence(&migrations)?;
    Ok(migrations)
}

/// Check that the migrations form the contiguous set `1 … N` with no gaps (design.md §4/§7).
/// The slice must already be sorted by sequence.
pub(crate) fn validate_sequence(migrations: &[Migration]) -> Result<(), MigrateError> {
    for (i, m) in migrations.iter().enumerate() {
        let want = (i + 1) as u32;
        if m.sequence != want {
            return Err(MigrateError::Load(format!(
                "non-contiguous migration sequence: expected {want}, found {} ({}); \
                 sequences must be 1 … N with no gaps",
                m.sequence, m.name
            )));
        }
    }
    Ok(())
}

/// Match a migration file name `^(\d+)_.+\.sql$` and return its sequence (design.md §4), or
/// `None` if the name is not a migration file. Hand-rolled (no `regex` dependency, CLAUDE.md
/// §14).
fn parse_file_name(name: &str) -> Option<u32> {
    let stem = name.strip_suffix(".sql")?;
    let (seq, rest) = stem.split_once('_')?;
    if seq.is_empty() || rest.is_empty() {
        return None;
    }
    if !seq.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    seq.parse::<u32>().ok()
}

/// Strip the `.sql` extension from a file name to form the human-readable migration name
/// (e.g. `001_create_users`).
fn migration_label(file_name: &str) -> &str {
    file_name.strip_suffix(".sql").unwrap_or(file_name)
}
