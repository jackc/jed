//! Scaffolding a new migration file (design.md §9). Needs no database.

use std::path::{Path, PathBuf};

use crate::error::MigrateError;

/// The scaffold written by [`new_migration`]: an empty up half, the separator, and an empty
/// down half (design.md §9). Delete the separator and the down half for an irreversible
/// migration.
const STUB_TEMPLATE: &str = "\
-- Write your forward (up) migration here.


---- create above / drop below ----

-- Write your reverse (down) migration here.
-- Delete this half (and the separator line above) for an irreversible migration.
";

/// Scaffold the next migration file in `dir` and return its path (design.md §9). The sequence
/// number is the highest existing sequence plus one (or 1 if the directory is empty),
/// zero-padded to three digits, with `name` as the label: `dir/NNN_<name>.sql`. It needs no
/// database. The directory is created if it does not exist.
pub fn new_migration(dir: &Path, name: &str) -> Result<PathBuf, MigrateError> {
    if name.is_empty() {
        return Err(MigrateError::Io("new migration needs a name".to_string()));
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| MigrateError::Io(format!("creating {}: {e}", dir.display())))?;
    let next = next_sequence(dir)?;
    let file_name = format!("{next:03}_{name}.sql");
    let path = dir.join(&file_name);
    if path.exists() {
        return Err(MigrateError::Io(format!(
            "{} already exists",
            path.display()
        )));
    }
    std::fs::write(&path, STUB_TEMPLATE)
        .map_err(|e| MigrateError::Io(format!("writing {}: {e}", path.display())))?;
    Ok(path)
}

/// The next sequence number for `dir`: the highest present migration sequence plus one (or 1
/// when none are present). Unlike loading, this needs only the maximum, not contiguity.
fn next_sequence(dir: &Path) -> Result<u32, MigrateError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| MigrateError::Io(format!("reading {}: {e}", dir.display())))?;
    let mut max = 0u32;
    for entry in entries {
        let entry =
            entry.map_err(|e| MigrateError::Io(format!("reading {}: {e}", dir.display())))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(seq) = sequence_of(&name)
            && seq > max
        {
            max = seq;
        }
    }
    Ok(max + 1)
}

/// The sequence number of a migration file name, or `None` if it is not a migration file.
fn sequence_of(name: &str) -> Option<u32> {
    let stem = name.strip_suffix(".sql")?;
    let (seq, rest) = stem.split_once('_')?;
    if seq.is_empty() || rest.is_empty() || !seq.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    seq.parse::<u32>().ok()
}
