//! Script mode (spec/design/cli.md §3/§5): plain stdout, no raw terminal. Drives the
//! same `Session` as the TUI. Stop-on-error is the DEFAULT (psql's ON_ERROR_STOP-off
//! default is a half-applied-migration footgun); it is safe by construction — a failed
//! autocommit statement already rolled back, and close() rolls back an open block.

use std::io::Write;

use crate::csv;
use crate::render::{self, Format};
use crate::session::{ExecOutput, Session};
use crate::splitter;

pub struct Options {
    pub format: Format,
    pub continue_on_error: bool,
    pub quiet: bool,
}

/// One resolved script-mode input, in command-line order (cli.md §3).
pub enum Input {
    /// SQL statements from `-c`, `-f`, or stdin: (display name, SQL text).
    Sql { name: String, text: String },
    /// `--import-csv TABLE=FILE`, already read: (display name, table, CSV text).
    ImportCsv {
        name: String,
        table: String,
        text: String,
    },
}

/// Run the inputs in order. Returns the process exit code: 0 = success, 2 = at least one
/// statement (or the input's framing) failed.
pub fn run(
    session: &mut Session,
    inputs: &[Input],
    opts: &Options,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let mut failed = false;
    for input in inputs {
        let (name, stmts) = match input {
            Input::Sql { name, text } => match splitter::split(text) {
                Ok(stmts) => (name, stmts.into_iter().map(|s| (s.line, s.sql)).collect()),
                Err(e) => {
                    let _ = writeln!(err, "{name}:{}: ERROR 42601: {}", e.line, e.message);
                    return 2; // a malformed input never half-runs, even under --continue-on-error
                }
            },
            Input::ImportCsv { name, table, text } => {
                match import_statement(session, table, text) {
                    // A header-only file imports nothing — report it like a 0-row DML.
                    Ok(None) => {
                        if !opts.quiet {
                            let _ = writeln!(
                                out,
                                "{}",
                                crate::session::statement_line("OK", 0, Some(0))
                            );
                        }
                        continue;
                    }
                    Ok(Some(sql)) => (name, vec![(1usize, sql)]),
                    Err(message) => {
                        let _ = writeln!(err, "{name}: {message}");
                        if !opts.continue_on_error {
                            return 2;
                        }
                        failed = true;
                        continue;
                    }
                }
            }
        };
        for (line, sql) in stmts {
            match session.run(&sql) {
                Ok(ExecOutput::Statement { tag, cost, rows }) => {
                    if !opts.quiet {
                        // Transaction-control tags print bare; everything else carries
                        // the affected-row count (DML) and the deterministic cost
                        // (cli.md §5).
                        let _ =
                            writeln!(out, "{}", crate::session::statement_line(tag, cost, rows));
                    }
                }
                Ok(ExecOutput::Query {
                    columns,
                    rows,
                    cost,
                }) => {
                    let _ = render::write_query(out, opts.format, &columns, &rows, cost);
                }
                Err(e) => {
                    let _ = writeln!(err, "{name}:{line}: ERROR {}: {}", e.code(), e.message);
                    if let Some(hint) = hint_for(e.code()) {
                        let _ = writeln!(err, "hint: {hint}");
                    }
                    if !opts.continue_on_error {
                        return 2;
                    }
                    failed = true;
                }
            }
        }
    }
    if failed { 2 } else { 0 }
}

/// Resolve one `--import-csv` input to its synthesized INSERT (cli.md §3): parse the CSV,
/// look the table up in the session's visible catalog, and build the atomic statement.
fn import_statement(session: &Session, table: &str, text: &str) -> Result<Option<String>, String> {
    let records = csv::parse(text)?;
    let def = session
        .db
        .table(table)
        .ok_or_else(|| format!("table does not exist: {table}"))?;
    csv::import_statement(def, &records)
}

/// CLI-generated hints for the errors a CLI user can act on (cli.md §5).
pub fn hint_for(code: &str) -> Option<&'static str> {
    match code {
        // 54P01 cost_limit_exceeded — the ceiling is a CLI flag here.
        "54P01" => Some("raise the ceiling with --max-cost, or 0 to disable"),
        _ => None,
    }
}
