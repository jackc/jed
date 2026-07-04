//! Resolving tern-style destination specs to absolute target versions (design.md §6/§9).

use crate::error::MigrateError;

/// Resolve a tern-style destination `spec` into the ordered list of absolute target versions
/// to migrate to (design.md §6/§9). `current` is the version presently recorded; `n` is the
/// highest available sequence. The grammar:
///
/// | spec | meaning |
/// |---|---|
/// | `"last"` or `""` | migrate to `N` (the default) |
/// | `"<integer>"` | migrate to that absolute version |
/// | `"+N"` | migrate up `N` steps (`current + N`) |
/// | `"-N"` | migrate down `N` steps (`current - N`) |
/// | `"-+N"` | redo the last `N`: down `N`, then back up `N` (`[current-N, current]`) |
///
/// Every resolved target is range-checked against `0 … N`. Relative-grammar resolution is the
/// caller's concern (design.md §9 — typically a CLI); the library's
/// [`migrate_to`](crate::Migrator::migrate_to) takes only an absolute target.
pub fn resolve_targets(spec: &str, current: u32, n: u32) -> Result<Vec<u32>, MigrateError> {
    let spec = spec.trim();
    if spec.is_empty() || spec == "last" {
        return Ok(vec![n]);
    }

    // Redo: "-+N" (down N, then back up N). Checked before the "-N" case.
    if let Some(rest) = spec.strip_prefix("-+") {
        let steps: u32 = rest.parse().map_err(|_| bad_dest(spec))?;
        let down = (current as i64) - (steps as i64);
        check_range(down, n)?;
        return Ok(vec![down as u32, current]);
    }

    // Relative up/down: "+N" / "-N".
    if let Some(rest) = spec.strip_prefix('+') {
        let steps: u32 = rest.parse().map_err(|_| bad_dest(spec))?;
        let target = (current as i64) + (steps as i64);
        check_range(target, n)?;
        return Ok(vec![target as u32]);
    }
    if let Some(rest) = spec.strip_prefix('-') {
        let steps: u32 = rest.parse().map_err(|_| bad_dest(spec))?;
        let target = (current as i64) - (steps as i64);
        check_range(target, n)?;
        return Ok(vec![target as u32]);
    }

    // Absolute integer.
    let target: i64 = spec.parse().map_err(|_| bad_dest(spec))?;
    check_range(target, n)?;
    Ok(vec![target as u32])
}

fn check_range(target: i64, n: u32) -> Result<(), MigrateError> {
    if target < 0 || target > n as i64 {
        return Err(MigrateError::BadVersion {
            version: target,
            n,
            whence: "target",
        });
    }
    Ok(())
}

fn bad_dest(spec: &str) -> MigrateError {
    MigrateError::Load(format!(
        "bad destination {spec:?}: expected +N, -N, -+N, an integer, or last"
    ))
}
