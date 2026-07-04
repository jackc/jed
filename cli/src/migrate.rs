//! The `jed migrate` subcommand (spec/design/cli.md, design.md §9): apply / status / new,
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
       jed migrate new NAME [-m DIR]         scaffold the next migration file (no database)

  -d, --destination TARGET   integer | +N | -N | -+N | last   (default: last)
  -m, --migrations DIR       migrations directory              (default: ./migrations)
      --version-table NAME   override the default `schema_version` table
  -h, --help                 print this help, then exit

`jed migrate DBFILE` creates DBFILE if it does not exist, then applies every migration.
A DBFILE literally named `migrate` is reached by qualifying its path (`jed ./migrate`).";

/// Dispatch the migrate subcommand. `argv` is the arguments *after* the `migrate` token.
pub fn run(argv: &[String]) -> u8 {
    match argv.first().map(String::as_str) {
        Some("-h") | Some("--help") => {
            println!("{USAGE}");
            0
        }
        Some("status") => run_status(&argv[1..]),
        Some("new") => run_new(&argv[1..]),
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

/// Parse the flags shared by `apply` and `status` (`-d`/`-m`/`--version-table`) plus one
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

/// Step one version at a time toward each resolved target, printing each migration applied
/// (per-step progress, since the library's OnStart hook is a deferred feature, design.md §11).
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
    if !db_path.exists() {
        eprintln!(
            "jed migrate: {}: database does not exist (run `jed migrate {}` to create and migrate it)",
            db_path.display(),
            db_path.display()
        );
        return 1;
    }
    let db = match Database::open(&db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("jed migrate: {}: {e}", db_path.display());
            return 1;
        }
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
