//! The `jed migrate` subcommand (spec/design/cli.md, design.md §9): apply / status /
//! set-version / new / renumber,
//! bundling the `jed-migrate` crate so migrations ship in the CLI with no separate binary.
//!
//! Dispatched from `main.rs` when the first CLI token is the literal `migrate`; everything
//! after it is parsed here. Exit codes follow cli.md: `0` success, `1` usage/open/load error,
//! `2` a migration failed (or a down through an irreversible migration).

use std::path::{Path, PathBuf};

use jed::{CreateOptions, Database};
use jed_migrate::{
    MigrateError, Migrator, Options, load_migrations, new_migration, resolve_targets,
};

pub const USAGE: &str = "\
usage: jed migrate [OPTIONS] DBFILE          apply migrations up/down to a target
       jed migrate status [OPTIONS] DBFILE   show current version, target, pending count
       jed migrate set-version VERSION [OPTIONS] DBFILE
                                             record VERSION without running migrations
       jed migrate new NAME [-m DIR]         scaffold the next migration file (no database)
       jed migrate renumber start|finish [-m DIR]
                                             renumber colliding migrations after a merge

  -d, --destination TARGET   integer | +N | -N | -+N | last   (default: last)
  -m, --migrations DIR       migrations directory              (default: ./migrations)
      --version-table NAME   override the default `schema_version` table
  -h, --help                 print this help, then exit

`jed migrate DBFILE` creates DBFILE if it does not exist, then applies every migration.
`set-version` adopts an existing DBFILE whose schema already matches migration VERSION.
`renumber start` (on the branch whose migrations keep their numbers) records the current set;
after merging, `renumber finish` moves every migration not in that set to the end, in order.
A DBFILE literally named `migrate` is reached by qualifying its path (`jed ./migrate`).";

/// Dispatch the migrate subcommand. `argv` is the arguments *after* the `migrate` token.
pub fn run(argv: &[String]) -> u8 {
    match argv.first().map(String::as_str) {
        Some("-h") | Some("--help") => {
            println!("{USAGE}");
            0
        }
        Some("status") => run_status(&argv[1..]),
        Some("set-version") => run_set_version(&argv[1..]),
        Some("new") => run_new(&argv[1..]),
        Some("renumber") => run_renumber(&argv[1..]),
        _ => run_apply(argv),
    }
}

// ───────────────────────────── shared flag parsing ─────────────────────────────

struct Common {
    db: Option<PathBuf>,
    dir: PathBuf,
    version_table: Option<String>,
    dest: String, // apply only; ignored by status
}

impl Default for Common {
    fn default() -> Self {
        Common {
            db: None,
            dir: PathBuf::from("migrations"),
            version_table: None,
            dest: "last".to_string(),
        }
    }
}

/// Parse the flags shared by `apply`, `status`, and `set-version` (`-d`/`-m`/`--version-table`) plus one
/// positional DBFILE. `allow_dest` gates whether `-d`/`--destination` is accepted.
fn parse_common(argv: &[String], allow_dest: bool) -> Result<Common, String> {
    let mut c = Common::default();
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        let mut value_for = |flag: &str| -> Result<String, String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match arg.as_str() {
            "-d" | "--destination" if allow_dest => c.dest = value_for("-d")?,
            "-m" | "--migrations" => c.dir = PathBuf::from(value_for("-m")?),
            "--version-table" => c.version_table = Some(value_for("--version-table")?),
            s if s.starts_with('-') && s != "-" => return Err(format!("unknown flag: {s}")),
            _ => {
                if c.db.is_some() {
                    return Err(format!("unexpected extra argument: {arg}"));
                }
                c.db = Some(PathBuf::from(arg));
            }
        }
    }
    Ok(c)
}

// ───────────────────────────── apply ─────────────────────────────

fn run_apply(argv: &[String]) -> u8 {
    let c = match parse_common(argv, true) {
        Ok(c) => c,
        Err(e) => return usage_error(&e),
    };
    let Some(db_path) = c.db.clone() else {
        return usage_error("a DBFILE is required");
    };
    let db = match open_or_create(&db_path) {
        Ok(db) => db,
        Err(code) => return code,
    };
    let mut m = match build_migrator(&db, &c) {
        Ok(m) => m,
        Err(code) => return code,
    };
    let current = match m.current_version() {
        Ok(v) => v,
        Err(e) => return migrate_error(&e),
    };
    let n = m.migrations().len() as u32;
    let targets = match resolve_targets(&c.dest, current, n) {
        Ok(t) => t,
        Err(e) => return usage_error(&e.to_string()),
    };
    match apply_targets(&mut m, &targets) {
        Ok(()) => 0,
        Err(e) => migrate_error(&e),
    }
}

/// Step one version at a time toward each resolved target, printing each migration once it has
/// applied (the line confirms the step committed; the library's `on_start` hook, design.md §6.2,
/// fires before a step runs, so it is not used here).
fn apply_targets(m: &mut Migrator, targets: &[u32]) -> Result<(), MigrateError> {
    let mut cur = m.current_version()?;
    let mut steps = 0u32;
    for &target in targets {
        while cur != target {
            let up = target > cur;
            let next = if up { cur + 1 } else { cur - 1 };
            let seq = if up { next } else { cur }; // the migration being applied/reversed
            let name = m.migrations()[(seq - 1) as usize].name.clone();
            m.migrate_to(next)?;
            println!("{}  {name}", if up { "up  " } else { "down" });
            cur = next;
            steps += 1;
        }
    }
    if steps == 0 {
        println!("already at version {cur}, nothing to do");
    } else {
        println!("done — now at version {cur}");
    }
    Ok(())
}

// ───────────────────────────── status ─────────────────────────────

fn run_status(argv: &[String]) -> u8 {
    let c = match parse_common(argv, false) {
        Ok(c) => c,
        Err(e) => return usage_error(&e),
    };
    let Some(db_path) = c.db.clone() else {
        return usage_error("a DBFILE is required");
    };
    let db = match open_existing(&db_path) {
        Ok(db) => db,
        Err(code) => return code,
    };
    let mut m = match build_migrator(&db, &c) {
        Ok(m) => m,
        Err(code) => return code,
    };
    let s = match m.status() {
        Ok(s) => s,
        Err(e) => return migrate_error(&e),
    };
    println!("database: {}", db_path.display());
    println!("version:  {} of {}", s.current, s.target);
    println!("pending:  {}", s.pending);
    if !m.migrations().is_empty() {
        println!();
        for mig in m.migrations() {
            let mark = if mig.sequence <= s.current { "x" } else { " " };
            let irr = if mig.is_irreversible() {
                "  (irreversible)"
            } else {
                ""
            };
            println!("  [{mark}] {}{irr}", mig.name);
        }
    }
    0
}

// ───────────────────────────── set-version ─────────────────────────────

/// `jed migrate set-version VERSION [OPTIONS] DBFILE` — the baseline (design.md §6.1): record
/// VERSION in the version table without running any migration. VERSION comes first; it is an
/// absolute integer in `0 … N` (the library range-checks it, a `BadVersion` exiting 1). The
/// database must already exist — adopting is for an existing database, so it is never created.
fn run_set_version(argv: &[String]) -> u8 {
    let Some(version_arg) = argv.first() else {
        return usage_error("`jed migrate set-version` needs a VERSION");
    };
    let Ok(version) = version_arg.parse::<u32>() else {
        return usage_error(&format!(
            "invalid VERSION {version_arg:?}: expected a non-negative integer"
        ));
    };
    let c = match parse_common(&argv[1..], false) {
        Ok(c) => c,
        Err(e) => return usage_error(&e),
    };
    let Some(db_path) = c.db.clone() else {
        return usage_error("a DBFILE is required");
    };
    let db = match open_existing(&db_path) {
        Ok(db) => db,
        Err(code) => return code,
    };
    let mut m = match build_migrator(&db, &c) {
        Ok(m) => m,
        Err(code) => return code,
    };
    match m.set_version(version) {
        Ok(()) => {
            println!("version set to {version} (no migrations run)");
            0
        }
        Err(e) => migrate_error(&e),
    }
}

// ───────────────────────────── new ─────────────────────────────

fn run_new(argv: &[String]) -> u8 {
    let mut dir = PathBuf::from("migrations");
    let mut name: Option<String> = None;
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-m" | "--migrations" => match it.next() {
                Some(v) => dir = PathBuf::from(v),
                None => return usage_error("-m needs a value"),
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            s if s.starts_with('-') && s != "-" => {
                return usage_error(&format!("unknown flag: {s}"));
            }
            _ => {
                if name.is_some() {
                    return usage_error(&format!("unexpected extra argument: {arg}"));
                }
                name = Some(arg.clone());
            }
        }
    }
    let Some(name) = name else {
        return usage_error("`jed migrate new` needs a NAME");
    };
    match new_migration(&dir, &name) {
        Ok(path) => {
            println!("created {}", path.display());
            0
        }
        Err(e) => {
            eprintln!("jed migrate: {e}");
            1
        }
    }
}

// ───────────────────────────── renumber ─────────────────────────────

/// The state file `renumber start` writes into the migrations directory and `renumber finish`
/// consumes (tern's `.tern-renumber.tmp`): one migration file name per line, in sequence order. It
/// does not match the migration file pattern, so a loader never mistakes it for a migration.
const RENUMBER_STATE_FILE: &str = ".jed-renumber.tmp";

/// `jed migrate renumber start|finish [-m DIR]` — tern's collision helper (design.md §9.1). Needs
/// no database: it only reads and renames files in the migrations directory.
fn run_renumber(argv: &[String]) -> u8 {
    let mut dir = PathBuf::from("migrations");
    let mut action: Option<&str> = None;
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-m" | "--migrations" => match it.next() {
                Some(v) => dir = PathBuf::from(v),
                None => return usage_error("-m needs a value"),
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            s if s.starts_with('-') && s != "-" => {
                return usage_error(&format!("unknown flag: {s}"));
            }
            s => {
                if action.is_some() {
                    return usage_error(&format!("unexpected extra argument: {s}"));
                }
                action = Some(s);
            }
        }
    }
    let result = match action {
        Some("start") => renumber_start(&dir),
        Some("finish") => renumber_finish(&dir),
        Some(other) => {
            return usage_error(&format!(
                "unknown renumber action {other:?}: expected start or finish"
            ));
        }
        None => return usage_error("`jed migrate renumber` needs start or finish"),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("jed migrate: {e}");
            1
        }
    }
}

/// `renumber start`: record the current migration set, which must be well-numbered (`1 … N`, no
/// gap or duplicate — a name-only check, file contents are not read, as in tern). Overwrites any
/// earlier state file, so a start can be redone.
fn renumber_start(dir: &Path) -> Result<(), String> {
    let files = scan_migration_files(dir)?;
    for (i, (seq, name)) in files.iter().enumerate() {
        if i > 0 && files[i - 1].0 == *seq {
            return Err(format!(
                "duplicate sequence {seq}: {} and {name}",
                files[i - 1].1
            ));
        }
        let want = (i + 1) as u32;
        if *seq != want {
            return Err(format!(
                "non-contiguous migration sequence: expected {want}, found {seq} ({name}); \
                 sequences must be 1 … N with no gaps"
            ));
        }
    }
    let state_path = dir.join(RENUMBER_STATE_FILE);
    let mut contents = String::new();
    for (_, name) in &files {
        contents.push_str(name);
        contents.push('\n');
    }
    std::fs::write(&state_path, contents)
        .map_err(|e| format!("writing {}: {e}", state_path.display()))?;
    println!(
        "recorded {} in {}; merge the other branch, then run `jed migrate renumber finish`",
        count_migrations(files.len()),
        state_path.display()
    );
    Ok(())
}

/// `renumber finish`: every migration file not named in the state file is renumbered after the
/// highest recorded sequence, ordered by its current sequence (then file name), keeping its label
/// and zero-padding to three digits; the recorded files keep their names. Then the state file is
/// removed. Duplicates and gaps are expected here, so the directory is scanned without validation.
fn renumber_finish(dir: &Path) -> Result<(), String> {
    let state_path = dir.join(RENUMBER_STATE_FILE);
    let state = std::fs::read_to_string(&state_path).map_err(|e| {
        format!(
            "reading {}: {e} (run `jed migrate renumber start` first)",
            state_path.display()
        )
    })?;
    let mut recorded: Vec<&str> = Vec::new();
    let mut last: u32 = 0;
    for (i, line) in state.lines().enumerate() {
        let Some(seq) = migration_file_sequence(line) else {
            return Err(format!(
                "{}: line {}: {line:?} is not a migration file name",
                state_path.display(),
                i + 1
            ));
        };
        last = last.max(seq);
        recorded.push(line);
    }

    // The new migrations, in their new order: (old name, new name), skipping a name that would
    // not change.
    let mut renames: Vec<(String, String)> = Vec::new();
    let mut next = last;
    for (_, name) in scan_migration_files(dir)? {
        if recorded.contains(&name.as_str()) {
            continue;
        }
        next = next
            .checked_add(1)
            .ok_or_else(|| "renumbering overflows the sequence range".to_string())?;
        let digits = name.bytes().take_while(u8::is_ascii_digit).count();
        let new_name = format!("{next:03}{}", &name[digits..]);
        if new_name != name {
            renames.push((name, new_name));
        }
    }

    // Never overwrite a file: a new name may be held only by a migration about to be renamed away.
    for (_, new_name) in &renames {
        if dir.join(new_name).exists() && !renames.iter().any(|(old, _)| old == new_name) {
            return Err(format!(
                "renumbering would overwrite {}",
                dir.join(new_name).display()
            ));
        }
    }
    // Rename each file once its new name is free. The sequence-ordered numbering admits no cycle
    // (design.md §9.1), so every pass makes progress.
    let renamed = renames.len();
    let mut pending = renames;
    while !pending.is_empty() {
        let ready = pending
            .iter()
            .position(|(_, new_name)| !pending.iter().any(|(old, _)| old == new_name))
            .ok_or_else(|| "renumbering found a rename cycle".to_string())?;
        let (old, new_name) = pending.remove(ready);
        std::fs::rename(dir.join(&old), dir.join(&new_name))
            .map_err(|e| format!("renaming {old} to {new_name}: {e}"))?;
        println!("renamed {old} -> {new_name}");
    }

    // A leftover state file is harmless (a repeated finish renames nothing), so failing to remove
    // it is only a warning.
    if let Err(e) = std::fs::remove_file(&state_path) {
        eprintln!(
            "jed migrate: warning: removing {}: {e}",
            state_path.display()
        );
    }
    if renamed == 0 {
        println!("no migrations to renumber");
    } else {
        println!("renumbered {}", count_migrations(renamed));
    }
    Ok(())
}

/// `1 migration` / `N migrations`.
fn count_migrations(n: usize) -> String {
    format!("{n} migration{}", if n == 1 { "" } else { "s" })
}

/// The migration files in `dir` (non-directories matching design.md §4's `^(\d+)_.+\.sql$`) as
/// `(sequence, file name)`, sorted by sequence then name. Nothing else is validated.
fn scan_migration_files(dir: &Path) -> Result<Vec<(u32, String)>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("reading {}: {e}", dir.display()))?;
        let file_type = entry
            .file_type()
            .map_err(|e| format!("stat {}: {e}", entry.path().display()))?;
        if file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(seq) = migration_file_sequence(&name) {
            files.push((seq, name));
        }
    }
    files.sort();
    Ok(files)
}

/// The sequence of a migration file name `^(\d+)_.+\.sql$` (design.md §4), or `None` if the name
/// is not a migration file — the jed-migrate loader's rule.
fn migration_file_sequence(name: &str) -> Option<u32> {
    let stem = name.strip_suffix(".sql")?;
    let (seq, rest) = stem.split_once('_')?;
    if seq.is_empty() || rest.is_empty() || !seq.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    seq.parse::<u32>().ok()
}

// ───────────────────────────── helpers ─────────────────────────────

/// Open DBFILE if it exists, else create it (the migration-bootstrap workflow — a fresh
/// project's first `jed migrate` creates the file and applies every migration).
fn open_or_create(path: &Path) -> Result<Database, u8> {
    let result = if path.exists() {
        Database::open(path)
    } else {
        Database::create(CreateOptions {
            path: Some(path.to_path_buf()),
            page_size: jed::DEFAULT_PAGE_SIZE,
            ..Default::default()
        })
    };
    result.map_err(|e| {
        eprintln!("jed migrate: {}: {e}", path.display());
        1
    })
}

/// Open an existing DBFILE (`status`, `set-version`); a missing file is an error pointing at
/// the apply action, which is the one that creates it.
fn open_existing(path: &Path) -> Result<Database, u8> {
    if !path.exists() {
        eprintln!(
            "jed migrate: {}: database does not exist (run `jed migrate {}` to create and migrate it)",
            path.display(),
            path.display()
        );
        return Err(1);
    }
    Database::open(path).map_err(|e| {
        eprintln!("jed migrate: {}: {e}", path.display());
        1
    })
}

/// Load migrations from the directory and build the migrator; map a load/build failure to a
/// stderr line + exit code 1.
fn build_migrator(db: &Database, c: &Common) -> Result<Migrator, u8> {
    let migrations = load_migrations(&c.dir).map_err(|e| {
        eprintln!("jed migrate: {e}");
        1u8
    })?;
    Migrator::new(
        db,
        migrations,
        Options {
            version_table: c.version_table.clone(),
            ..Options::default()
        },
    )
    .map_err(|e| {
        eprintln!("jed migrate: {e}");
        1u8
    })
}

fn usage_error(msg: &str) -> u8 {
    eprintln!("jed migrate: {msg}\n\n{USAGE}");
    1
}

/// Print a migrate error and return its exit code: `2` when a migration actually ran and
/// failed (a statement error, or a down through an irreversible migration), else `1`.
fn migrate_error(e: &MigrateError) -> u8 {
    eprintln!("jed migrate: {e}");
    match e {
        MigrateError::Migration { .. } | MigrateError::Irreversible { .. } => 2,
        _ => 1,
    }
}
