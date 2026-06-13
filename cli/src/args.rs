//! Hand-rolled flag parsing (spec/design/cli.md §2: ~10 flags do not justify a
//! dependency). Order of `-c`/`-f` is preserved — sources run in command-line order.

use std::path::PathBuf;

use crate::render::Format;

/// One script-mode SQL source, in command-line order.
#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    /// `-c SQL`
    Command(String),
    /// `-f FILE` (`-` = stdin)
    File(PathBuf),
    /// `--import-csv TABLE=FILE`
    ImportCsv { table: String, path: PathBuf },
}

#[derive(Debug)]
pub struct Args {
    pub db_path: Option<PathBuf>,
    pub create: bool,
    pub readonly: bool,
    pub dump: bool,
    pub page_size: Option<u32>,
    pub sources: Vec<Source>,
    pub format: Format,
    /// `-o FILE`: script-mode results go to FILE instead of stdout (`-` = stdout).
    pub output: Option<PathBuf>,
    pub max_cost: Option<i64>,
    pub continue_on_error: bool,
    pub quiet: bool,
    pub help: bool,
    pub version: bool,
}

pub const USAGE: &str = "\
usage: jed [OPTIONS] [DBFILE]

  (no DBFILE)             transient in-memory database
  --create                create DBFILE instead of opening it
  --readonly              open DBFILE read-only: writes fail with 25006, the file is never touched
  --page-size N           with --create: the page size locked into the file
  -c SQL                  execute the statements, then exit (repeatable)
  -f FILE                 execute a SQL file, then exit (repeatable; '-' = stdin)
  --import-csv TABLE=FILE import an RFC 4180 CSV (header row required) into TABLE as one
                          atomic INSERT (repeatable; runs in command-line order with -c/-f)
  --dump                  write the database as SQL (schema + rows + indexes), then exit;
                          composes with --readonly and -o
  --format FORMAT         script-mode output: aligned (default) | box | markdown | csv | json
  -o FILE                 script mode: write results to FILE instead of stdout ('-' = stdout);
                          errors still go to stderr
  --max-cost N            cost ceiling: statements abort with 54P01 at cost N
  --continue-on-error     script mode: keep going after a SQL error
  -q, --quiet             script mode: suppress OK lines
  --version               print the version, then exit
  -h, --help              print this help, then exit

With no -c/-f and stdin not a terminal, statements are read from stdin.
Otherwise jed opens the full-screen TUI (F1 inside for keys).";

/// Parse argv (without the program name). Returns a usage-style error message on bad input.
pub fn parse(argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut args = Args {
        db_path: None,
        create: false,
        readonly: false,
        dump: false,
        page_size: None,
        sources: Vec::new(),
        format: Format::Aligned,
        output: None,
        max_cost: None,
        continue_on_error: false,
        quiet: false,
        help: false,
        version: false,
    };
    let mut argv = argv.peekable();
    while let Some(arg) = argv.next() {
        let mut value_for = |flag: &str| -> Result<String, String> {
            argv.next().ok_or_else(|| format!("{flag} needs a value"))
        };
        match arg.as_str() {
            "--create" => args.create = true,
            "--readonly" => args.readonly = true,
            "--dump" => args.dump = true,
            "--page-size" => {
                let v = value_for("--page-size")?;
                args.page_size = Some(v.parse().map_err(|_| format!("bad --page-size: {v}"))?);
            }
            "-c" | "--command" => args.sources.push(Source::Command(value_for("-c")?)),
            "-f" | "--file" => args
                .sources
                .push(Source::File(PathBuf::from(value_for("-f")?))),
            "--import-csv" => {
                let v = value_for("--import-csv")?;
                let Some((table, file)) = v.split_once('=') else {
                    return Err(format!("bad --import-csv: {v} (expected TABLE=FILE)"));
                };
                if table.is_empty() || file.is_empty() {
                    return Err(format!("bad --import-csv: {v} (expected TABLE=FILE)"));
                }
                args.sources.push(Source::ImportCsv {
                    table: table.to_string(),
                    path: PathBuf::from(file),
                });
            }
            "--format" => {
                let v = value_for("--format")?;
                args.format = Format::parse(&v).ok_or_else(|| {
                    format!("bad --format: {v} (aligned | box | markdown | csv | json)")
                })?;
            }
            "-o" | "--output" => args.output = Some(PathBuf::from(value_for("-o")?)),
            "--max-cost" => {
                let v = value_for("--max-cost")?;
                args.max_cost = Some(v.parse().map_err(|_| format!("bad --max-cost: {v}"))?);
            }
            "--continue-on-error" => args.continue_on_error = true,
            "-q" | "--quiet" => args.quiet = true,
            "-h" | "--help" => args.help = true,
            "--version" => args.version = true,
            s if s.starts_with('-') && s != "-" => return Err(format!("unknown flag: {s}")),
            _ => {
                if args.db_path.is_some() {
                    return Err(format!("unexpected extra argument: {arg}"));
                }
                args.db_path = Some(PathBuf::from(arg));
            }
        }
    }
    if args.page_size.is_some() && !args.create {
        return Err("--page-size requires --create".to_string());
    }
    if args.create && args.db_path.is_none() {
        return Err("--create requires a DBFILE".to_string());
    }
    if args.readonly && args.db_path.is_none() {
        return Err("--readonly requires a DBFILE".to_string());
    }
    if args.readonly && args.create {
        return Err("--readonly cannot be combined with --create".to_string());
    }
    if args.dump && args.db_path.is_none() {
        return Err("--dump requires a DBFILE".to_string());
    }
    if args.dump && !args.sources.is_empty() {
        return Err("--dump cannot be combined with -c/-f/--import-csv".to_string());
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Args, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_the_surface() {
        let a = p(&[
            "--create",
            "--page-size",
            "4096",
            "db.jed",
            "-c",
            "SELECT 1",
            "-f",
            "x.sql",
            "--format",
            "csv",
            "--max-cost",
            "100",
            "-q",
        ])
        .unwrap();
        assert_eq!(a.db_path, Some(PathBuf::from("db.jed")));
        assert!(a.create);
        assert_eq!(a.page_size, Some(4096));
        assert_eq!(
            a.sources,
            vec![
                Source::Command("SELECT 1".to_string()),
                Source::File(PathBuf::from("x.sql")),
            ]
        );
        assert_eq!(a.format, Format::Csv);
        assert_eq!(a.max_cost, Some(100));
        assert!(a.quiet);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(p(&["--nope"]).is_err());
        assert!(p(&["a.jed", "b.jed"]).is_err());
        assert!(p(&["--page-size", "4096", "a.jed"]).is_err()); // needs --create
        assert!(p(&["--create"]).is_err()); // needs a DBFILE
        assert!(p(&["--format", "xml"]).is_err());
        assert!(p(&["-c"]).is_err()); // missing value
        assert!(p(&["--readonly"]).is_err()); // needs a DBFILE
        assert!(p(&["--readonly", "--create", "a.jed"]).is_err()); // mutually exclusive
    }

    #[test]
    fn import_csv_takes_table_equals_file() {
        let a = p(&["--import-csv", "t=data.csv"]).unwrap();
        assert_eq!(
            a.sources,
            vec![Source::ImportCsv {
                table: "t".to_string(),
                path: PathBuf::from("data.csv"),
            }]
        );
        assert!(p(&["--import-csv", "no-equals"]).is_err());
        assert!(p(&["--import-csv", "=file"]).is_err());
        assert!(p(&["--import-csv", "t="]).is_err());
    }

    #[test]
    fn readonly_opens_a_dbfile() {
        let a = p(&["--readonly", "a.jed"]).unwrap();
        assert!(a.readonly);
        assert_eq!(a.db_path, Some(PathBuf::from("a.jed")));
    }

    #[test]
    fn stdin_file_dash_is_a_source_not_a_flag() {
        let a = p(&["-f", "-"]).unwrap();
        assert_eq!(a.sources, vec![Source::File(PathBuf::from("-"))]);
    }
}
