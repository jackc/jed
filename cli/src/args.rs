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
}

#[derive(Debug)]
pub struct Args {
    pub db_path: Option<PathBuf>,
    pub create: bool,
    pub page_size: Option<u32>,
    pub sources: Vec<Source>,
    pub format: Format,
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
  --page-size N           with --create: the page size locked into the file
  -c SQL                  execute the statements, then exit (repeatable)
  -f FILE                 execute a SQL file, then exit (repeatable; '-' = stdin)
  --format FORMAT         script-mode output: aligned (default) | csv | json
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
        page_size: None,
        sources: Vec::new(),
        format: Format::Aligned,
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
            "--page-size" => {
                let v = value_for("--page-size")?;
                args.page_size = Some(v.parse().map_err(|_| format!("bad --page-size: {v}"))?);
            }
            "-c" | "--command" => args.sources.push(Source::Command(value_for("-c")?)),
            "-f" | "--file" => args
                .sources
                .push(Source::File(PathBuf::from(value_for("-f")?))),
            "--format" => {
                let v = value_for("--format")?;
                args.format = Format::parse(&v)
                    .ok_or_else(|| format!("bad --format: {v} (aligned | csv | json)"))?;
            }
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
    }

    #[test]
    fn stdin_file_dash_is_a_source_not_a_flag() {
        let a = p(&["-f", "-"]).unwrap();
        assert_eq!(a.sources, vec![Source::File(PathBuf::from("-"))]);
    }
}
