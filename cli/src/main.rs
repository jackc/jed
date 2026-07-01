//! The `jed` binary (spec/design/cli.md): a full-screen TUI client for interactive use,
//! plus a plain script mode (`-c` / `-f` / piped stdin) for automation. A HOST PROGRAM —
//! it links the Rust core through the public embedding API and adds no engine behavior.

mod args;
mod csv;
mod dump;
mod render;
mod script;
mod session;
mod splitter;
mod tui;

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;

use jed::{Database, DatabaseOptions, OpenOptions, SessionOptions};

use args::Source;
use session::Session;

fn main() -> ExitCode {
    ExitCode::from(run())
}

// Exit codes (cli.md §3): 0 success · 1 startup/usage error · 2 a SQL statement
// failed in script mode.
fn run() -> u8 {
    let parsed = match args::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("jed: {e}\n\n{}", args::USAGE);
            return 1;
        }
    };
    if parsed.help {
        println!("{}", args::USAGE);
        return 0;
    }
    if parsed.version {
        println!("jed {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }

    let (mut db, source) = match open_database(&parsed) {
        Ok(pair) => pair,
        Err(code) => return code,
    };
    if let Some(limit) = parsed.max_cost {
        db.set_max_cost(limit);
    }
    let mut session = Session::new(db, source);

    // --dump (cli.md §3): write the database as SQL to stdout / -o, then exit.
    if parsed.dump {
        let mut out: Box<dyn Write> = match &parsed.output {
            Some(path) if path.as_os_str() != "-" => match std::fs::File::create(path) {
                Ok(f) => Box::new(std::io::BufWriter::new(f)),
                Err(e) => {
                    eprintln!("jed: {}: {e}", path.display());
                    return 1;
                }
            },
            _ => Box::new(std::io::stdout()),
        };
        if let Err(e) = dump::dump(&mut session.db, &mut out).and_then(|()| out.flush()) {
            eprintln!("jed: writing dump: {e}");
            return 1;
        }
        return 0;
    }

    // Mode select (cli.md §3): -c/-f present, or stdin not a TTY → script mode.
    let interactive = parsed.sources.is_empty() && std::io::stdin().is_terminal();
    if interactive {
        if parsed.output.is_some() {
            eprintln!("jed: -o applies to script mode only (pass -c/-f or pipe stdin)");
            return 1;
        }
        match tui::run(session) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("jed: terminal error: {e}");
                1
            }
        }
    } else {
        let sources = match collect_sources(&parsed.sources) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("jed: {e}");
                return 1;
            }
        };
        let opts = script::Options {
            format: parsed.format,
            continue_on_error: parsed.continue_on_error,
            quiet: parsed.quiet,
        };
        // -o redirects results to a file (cli.md §3); errors stay on stderr. `-` keeps
        // stdout, so scripts can parameterize the destination uniformly.
        let mut out: Box<dyn Write> = match &parsed.output {
            Some(path) if path.as_os_str() != "-" => match std::fs::File::create(path) {
                Ok(f) => Box::new(std::io::BufWriter::new(f)),
                Err(e) => {
                    eprintln!("jed: {}: {e}", path.display());
                    return 1;
                }
            },
            _ => Box::new(std::io::stdout()),
        };
        let code = script::run(
            &mut session,
            &sources,
            &opts,
            &mut out,
            &mut std::io::stderr(),
        );
        if let Err(e) = out.flush() {
            eprintln!("jed: writing output: {e}");
            return 1;
        }
        code as u8
    }
}

fn open_database(a: &args::Args) -> Result<(jed::Session, String), u8> {
    let Some(path) = &a.db_path else {
        return Ok((
            Database::new_in_memory().session(SessionOptions::default()),
            "memory".to_string(),
        ));
    };
    let result = if a.create {
        let opts = DatabaseOptions {
            page_size: a.page_size.unwrap_or(jed::DEFAULT_PAGE_SIZE),
        };
        Database::create(path, opts)
    } else if a.readonly {
        Database::open_with_options(
            path,
            OpenOptions {
                read_only: true,
                ..OpenOptions::default()
            },
        )
    } else {
        Database::open(path)
    };
    match result {
        Ok(db) => {
            let source = if a.readonly {
                format!("{} (read-only)", path.display())
            } else {
                path.display().to_string()
            };
            Ok((db.session(SessionOptions::default()), source))
        }
        Err(e) => {
            eprintln!("ERROR {}: {}", e.code(), e.message);
            if e.code() == "58P01" {
                eprintln!("hint: pass --create to make a new database");
            }
            Err(1)
        }
    }
}

/// Resolve the ordered `-c`/`-f`/`--import-csv` sources to script inputs (their file
/// contents read up front, so a missing file is a startup error); with none given (and
/// stdin already known not to be a TTY), the single source is stdin.
fn collect_sources(sources: &[Source]) -> Result<Vec<script::Input>, String> {
    let stdin_text = || -> Result<String, String> {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| format!("reading stdin: {e}"))?;
        Ok(text)
    };
    if sources.is_empty() {
        return Ok(vec![script::Input::Sql {
            name: "<stdin>".to_string(),
            text: stdin_text()?,
        }]);
    }
    sources
        .iter()
        .map(|s| match s {
            Source::Command(sql) => Ok(script::Input::Sql {
                name: "<command>".to_string(),
                text: sql.clone(),
            }),
            Source::File(path) if path.as_os_str() == "-" => Ok(script::Input::Sql {
                name: "<stdin>".to_string(),
                text: stdin_text()?,
            }),
            Source::File(path) => Ok(script::Input::Sql {
                name: path.display().to_string(),
                text: std::fs::read_to_string(path)
                    .map_err(|e| format!("{}: {e}", path.display()))?,
            }),
            Source::ImportCsv { table, path } => Ok(script::Input::ImportCsv {
                name: path.display().to_string(),
                table: table.clone(),
                text: std::fs::read_to_string(path)
                    .map_err(|e| format!("{}: {e}", path.display()))?,
            }),
        })
        .collect()
}
