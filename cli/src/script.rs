//! Script mode (spec/design/cli.md §3/§5): plain stdout, no raw terminal. Drives the
//! same `Session` as the TUI. Stop-on-error is the DEFAULT (psql's ON_ERROR_STOP-off
//! default is a half-applied-migration footgun); it is safe by construction — a failed
//! autocommit statement already rolled back, and close() rolls back an open block.

use std::io::Write;

use crate::render::{self, Format};
use crate::session::{ExecOutput, Session};
use crate::splitter;

pub struct Options {
    pub format: Format,
    pub continue_on_error: bool,
    pub quiet: bool,
}

/// Run named SQL sources in order. Returns the process exit code: 0 = success,
/// 2 = at least one statement (or the input's framing) failed.
pub fn run(
    session: &mut Session,
    sources: &[(String, String)], // (display name, SQL text)
    opts: &Options,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let mut failed = false;
    for (name, text) in sources {
        let stmts = match splitter::split(text) {
            Ok(stmts) => stmts,
            Err(e) => {
                let _ = writeln!(err, "{name}:{}: ERROR 42601: {}", e.line, e.message);
                return 2; // a malformed input never half-runs, even under --continue-on-error
            }
        };
        for stmt in stmts {
            match session.run(&stmt.sql) {
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
                    let _ = writeln!(
                        err,
                        "{name}:{}: ERROR {}: {}",
                        stmt.line,
                        e.code(),
                        e.message
                    );
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

/// CLI-generated hints for the errors a CLI user can act on (cli.md §5).
pub fn hint_for(code: &str) -> Option<&'static str> {
    match code {
        // 54P01 cost_limit_exceeded — the ceiling is a CLI flag here.
        "54P01" => Some("raise the ceiling with --max-cost, or 0 to disable"),
        _ => None,
    }
}
