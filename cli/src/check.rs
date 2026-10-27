//! The `jed check` subcommand (spec/design/cli.md §3.2, integrity-check.md §7): open a database
//! read-only, run the integrity check on `main`, print its findings and skips, and exit by verdict.
//! Dispatched from `main.rs` when the first CLI token is the literal `check`.

use std::io::Write;
use std::path::PathBuf;

use jed::{Database, IntegrityCheckOptions, IntegrityLevel, IntegrityReport, OpenOptions};

use crate::credential::{self, CredentialSource};
use crate::render::json_string;

const USAGE: &str = "\
usage: jed check [OPTIONS] DBFILE

Check DBFILE's pages, trees, free list and index agreement (spec/design/integrity-check.md).

  --quick              skip index agreement (SQLite's quick_check)
  --constraints        also re-check NOT NULL, typmods, CHECK, UNIQUE and foreign keys
  --max-findings N     stop after N findings (default 100; 0 = no limit)
  --format FORMAT      text (default) | json
  --key-file PATH | --passphrase-file PATH | --passphrase
                       the credential of an encrypted database

Exit codes: 0 nothing found · 1 usage or open error · 2 findings (or the file does not open
because it is damaged).";

struct Args {
    level: IntegrityLevel,
    constraints: bool,
    max_findings: u32,
    json: bool,
    credential: Option<CredentialSource>,
    db: Option<PathBuf>,
    help: bool,
}

fn parse(argv: &[String]) -> Result<Args, String> {
    let mut a = Args {
        level: IntegrityLevel::Full,
        constraints: false,
        max_findings: 100,
        json: false,
        credential: None,
        db: None,
        help: false,
    };
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        let mut value_for = |flag: &str| -> Result<String, String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        if let Some(source) = credential::parse_flag(arg, &a.credential, &mut value_for)? {
            a.credential = Some(source);
            continue;
        }
        match arg.as_str() {
            "--quick" => a.level = IntegrityLevel::Quick,
            "--constraints" => a.constraints = true,
            "--max-findings" => {
                let v = value_for("--max-findings")?;
                a.max_findings = v.parse().map_err(|_| format!("bad --max-findings: {v}"))?;
            }
            "--format" => match value_for("--format")?.as_str() {
                "text" => a.json = false,
                "json" => a.json = true,
                v => return Err(format!("bad --format: {v} (text | json)")),
            },
            "-h" | "--help" => a.help = true,
            s if s.starts_with('-') => return Err(format!("unknown flag: {s}")),
            _ if a.db.is_some() => return Err(format!("unexpected extra argument: {arg}")),
            _ => a.db = Some(PathBuf::from(arg)),
        }
    }
    Ok(a)
}

/// Run the check subcommand. `argv` is the arguments *after* the `check` token.
pub fn run(argv: &[String]) -> u8 {
    let a = match parse(argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("jed check: {e}\n\n{USAGE}");
            return 1;
        }
    };
    if a.help {
        println!("{USAGE}");
        return 0;
    }
    let Some(path) = &a.db else {
        eprintln!("jed check: missing DBFILE\n\n{USAGE}");
        return 1;
    };
    let credential = match &a.credential {
        Some(source) => match credential::load(source, false) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("jed check: {e}");
                return 1;
            }
        },
        None => None,
    };
    let opened = Database::open_with_options(
        path,
        OpenOptions {
            read_only: true,
            credential,
            ..OpenOptions::default()
        },
    );
    let mut out = std::io::stdout().lock();
    let db = match opened {
        Ok(db) => db,
        // A file too damaged to open is the answer to "is it sound?", not a usage error.
        Err(e) if e.code() == "XX001" => {
            let _ = if a.json {
                writeln!(
                    out,
                    "{{\"open_failed\":{{\"code\":\"XX001\",\"message\":{}}}}}",
                    json_string(&e.message)
                )
            } else {
                writeln!(out, "open_failed: XX001 {}", e.message)
            };
            return 2;
        }
        Err(e) => {
            eprintln!("jed check: ERROR {}: {}", e.code(), e.message);
            if e.code() == "28P02" {
                eprintln!("hint: pass --key-file, --passphrase-file or --passphrase");
            }
            return 1;
        }
    };
    let opts = IntegrityCheckOptions {
        level: a.level,
        max_findings: a.max_findings,
        constraints: a.constraints,
        ..IntegrityCheckOptions::default()
    };
    let report = match db.integrity_check("main", &opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("jed check: ERROR {}: {}", e.code(), e.message);
            return 1;
        }
    };
    let written = if a.json {
        writeln!(out, "{}", report_json(&report))
    } else {
        write_text(&mut out, &report)
    };
    if let Err(e) = written.and_then(|()| out.flush()) {
        eprintln!("jed check: writing output: {e}");
        return 1;
    }
    if report.findings.is_empty() && !report.truncated {
        0
    } else {
        2
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One line per finding, then one per skip, then a summary line.
fn write_text(out: &mut dyn Write, r: &IntegrityReport) -> std::io::Result<()> {
    for f in &r.findings {
        let mut line = f.kind.to_string();
        if let Some(object) = &f.object {
            line.push(' ');
            line.push_str(object);
        }
        if let Some(page) = f.page {
            line.push_str(&format!(" page {page}"));
        }
        if let Some(key) = &f.key {
            line.push_str(&format!(" key {}", hex(key)));
        }
        if !f.detail.is_empty() {
            line.push_str(": ");
            line.push_str(&f.detail);
        }
        writeln!(out, "{line}")?;
    }
    for s in &r.skipped {
        writeln!(out, "skipped {}: {}", s.object, s.reason)?;
    }
    let verdict = match (r.findings.len(), r.truncated) {
        (0, false) => "ok".to_string(),
        (1, false) => "1 finding".to_string(),
        (n, false) => format!("{n} findings"),
        (n, true) => format!("{n} findings (stopped at --max-findings; more may exist)"),
    };
    writeln!(out, "{verdict}: {} pages, {} rows checked", r.pages, r.rows)
}

/// The report of integrity-check.md §2 as one JSON object (`key` as lowercase hex).
fn report_json(r: &IntegrityReport) -> String {
    let opt = |s: &Option<String>| s.as_deref().map_or("null".to_string(), json_string);
    let findings: Vec<String> = r
        .findings
        .iter()
        .map(|f| {
            format!(
                "{{\"kind\":{},\"database\":{},\"object\":{},\"page\":{},\"key\":{},\"detail\":{}}}",
                json_string(f.kind),
                json_string(&f.database),
                opt(&f.object),
                f.page.map_or("null".to_string(), |p| p.to_string()),
                f.key.as_ref().map_or("null".to_string(), |k| json_string(&hex(k))),
                json_string(&f.detail)
            )
        })
        .collect();
    let skipped: Vec<String> = r
        .skipped
        .iter()
        .map(|s| {
            format!(
                "{{\"object\":{},\"reason\":{}}}",
                json_string(&s.object),
                json_string(s.reason)
            )
        })
        .collect();
    format!(
        "{{\"findings\":[{}],\"truncated\":{},\"skipped\":[{}],\"pages\":{},\"rows\":{}}}",
        findings.join(","),
        r.truncated,
        skipped.join(","),
        r.pages,
        r.rows
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Result<Args, String> {
        parse(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_flags_and_dbfile() {
        let a = args(&[
            "--quick",
            "--constraints",
            "--max-findings",
            "5",
            "--format",
            "json",
            "x.jed",
        ])
        .unwrap();
        assert_eq!(a.level, IntegrityLevel::Quick);
        assert!(a.constraints && a.json);
        assert_eq!(a.max_findings, 5);
        assert_eq!(a.db, Some(PathBuf::from("x.jed")));
    }

    #[test]
    fn rejects_bad_usage() {
        assert!(args(&["--format", "xml", "x.jed"]).is_err());
        assert!(args(&["--max-findings", "-1", "x.jed"]).is_err());
        assert!(args(&["a.jed", "b.jed"]).is_err());
        assert!(args(&["--bogus"]).is_err());
    }
}
