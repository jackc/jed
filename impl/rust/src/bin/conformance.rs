//! Conformance harness for the Rust core (CLAUDE.md §7).
//!
//! Walks spec/conformance/suites, and for each `.test` file whose `# requires:`
//! capabilities are all in this core's `SUPPORTED_CAPABILITIES`, runs the
//! sqllogictest-style records against a fresh `Database` and compares output.
//! Files needing a capability the core does not declare are SKIPPED (not failed),
//! so an incomplete engine reads as "fewer tests run" (spec/design/conformance.md §3).
//!
//! Needs no TOML: the per-impl gate is the file's `# requires:` header vs this
//! core's declared capability set. The manifest/profile data is validated
//! separately by `rake verify`. Exit code is nonzero if any run file fails.

use jed::{Database, Outcome, SUPPORTED_CAPABILITIES, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let suites = suites_dir();
    let mut files = Vec::new();
    collect_tests(&suites, &mut files);
    files.sort();

    let supported: BTreeSet<&str> = SUPPORTED_CAPABILITIES.iter().copied().collect();
    let (mut passed, mut failed, mut skipped) = (0u32, 0u32, 0u32);

    for file in &files {
        let text = std::fs::read_to_string(file).expect("read .test file");
        let requires = parse_requires(&text);
        let rel = file.strip_prefix(&suites).unwrap_or(file).display();

        let missing: Vec<&str> = requires
            .iter()
            .filter(|c| !supported.contains(c.as_str()))
            .map(|s| s.as_str())
            .collect();
        if !missing.is_empty() {
            println!("SKIP {rel}  (missing: {})", missing.join(", "));
            skipped += 1;
            continue;
        }

        match run_file(&text) {
            Ok(()) => {
                println!("PASS {rel}");
                passed += 1;
            }
            Err(e) => {
                println!("FAIL {rel}: {e}");
                failed += 1;
            }
        }
    }

    println!("\n{passed} passed, {failed} failed, {skipped} skipped");
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn suites_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/conformance/suites")
}

fn collect_tests(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_tests(&path, out);
        } else if path.extension().is_some_and(|e| e == "test") {
            out.push(path);
        }
    }
}

/// Extract the capabilities from the single `# requires:` header line.
fn parse_requires(text: &str) -> Vec<String> {
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("#") {
            let rest = rest.trim_start();
            if let Some(list) = rest.strip_prefix("requires:") {
                return list
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
        }
    }
    Vec::new()
}

/// Parse a `# cost: N` directive body (the comment text after the `#`). Returns the
/// asserted cost, or None if this comment is not a cost directive (CLAUDE.md §13).
fn parse_cost_directive(rest: &str) -> Option<i64> {
    rest.trim_start().strip_prefix("cost:")?.trim().parse().ok()
}

/// Parse a `# names: a, b, ?column?` directive body. Returns the asserted output column
/// names, or None if this comment is not a names directive (spec/design/conformance.md §1).
fn parse_names_directive(rest: &str) -> Option<Vec<String>> {
    let list = rest.trim_start().strip_prefix("names:")?;
    Some(
        list.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

/// Assert the accrued execution cost matches a pending `# cost:` directive (if any).
fn assert_cost(expected: Option<i64>, actual: i64, sql: &str) -> std::result::Result<(), String> {
    match expected {
        Some(e) if e != actual => Err(format!(
            "cost mismatch: expected {e}, got {actual}\n  SQL: {sql}"
        )),
        _ => Ok(()),
    }
}

/// Assert the query's output column names match a pending `# names:` directive (if any).
fn assert_names(
    expected: Option<&[String]>,
    actual: &[String],
    sql: &str,
) -> std::result::Result<(), String> {
    match expected {
        Some(e) if e != actual => Err(format!(
            "column-name mismatch\n  SQL: {sql}\n  expected: {e:?}\n  actual:   {actual:?}"
        )),
        _ => Ok(()),
    }
}

/// Run all records in one .test file against a fresh database. Returns the first
/// mismatch as an error string.
fn run_file(text: &str) -> std::result::Result<(), String> {
    let mut db = Database::new();
    let mut lines = text.lines().peekable();
    // A `# cost: N` / `# names: ...` directive sets these; the next record consumes them.
    let mut pending_cost: Option<i64> = None;
    let mut pending_names: Option<Vec<String>> = None;

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('#') {
            // `# cost:` / `# names:` bind to the next record; every other comment is ignored.
            if let Some(n) = parse_cost_directive(rest) {
                pending_cost = Some(n);
            } else if let Some(names) = parse_names_directive(rest) {
                pending_names = Some(names);
            }
            continue;
        }
        // This record consumes any pending assertions (so they never leak forward).
        let expected_cost = pending_cost.take();
        let expected_names = pending_names.take();
        let mut parts = trimmed.split_whitespace();
        let kind = parts.next().unwrap();
        match kind {
            "statement" => {
                // A `# names:` directive asserts result columns, which a statement lacks.
                if expected_names.is_some() {
                    return Err("# names: directive precedes a non-query statement".to_string());
                }
                let expect = parts.next().unwrap_or("");
                let sql = take_sql(&mut lines);
                let result = jed::execute(&mut db, &sql);
                match expect {
                    "ok" => match result {
                        Ok(outcome) => assert_cost(expected_cost, outcome.cost(), &sql)?,
                        Err(e) => {
                            return Err(format!(
                                "statement expected ok, got error {}: {}\n  SQL: {sql}",
                                e.code(),
                                e.message
                            ));
                        }
                    },
                    "error" => {
                        let want = parts.next().unwrap_or("");
                        match result {
                            Ok(_) => {
                                return Err(format!(
                                    "statement expected error {want}, but it succeeded\n  SQL: {sql}"
                                ));
                            }
                            Err(e) if e.code() == want => {}
                            Err(e) => {
                                return Err(format!(
                                    "statement expected error {want}, got {}\n  SQL: {sql}",
                                    e.code()
                                ));
                            }
                        }
                    }
                    other => return Err(format!("unknown statement kind '{other}'")),
                }
            }
            "query" => {
                let coltypes = parts.next().unwrap_or("");
                let sortmode = parts.next().unwrap_or("nosort");
                let sql = take_sql_until_separator(&mut lines);
                let mut expected = Vec::new();
                for l in lines.by_ref() {
                    if l.trim().is_empty() {
                        break;
                    }
                    expected.push(l.trim().to_string());
                }
                let outcome = jed::execute(&mut db, &sql).map_err(|e| {
                    format!(
                        "query failed with {}: {}\n  SQL: {sql}",
                        e.code(),
                        e.message
                    )
                })?;
                let cols = coltypes.len().max(1);
                let actual = render_outcome(&outcome, cols, sortmode);
                let expected = apply_sort(expected, cols, sortmode);
                if actual != expected {
                    return Err(format!(
                        "query result mismatch\n  SQL: {sql}\n  expected: {expected:?}\n  actual:   {actual:?}"
                    ));
                }
                assert_cost(expected_cost, outcome.cost(), &sql)?;
                assert_names(expected_names.as_deref(), outcome.column_names(), &sql)?;
            }
            other => return Err(format!("unknown record kind '{other}'")),
        }
    }
    Ok(())
}

/// Collect SQL lines for a `statement` (until a blank line or EOF).
fn take_sql<'a, I: Iterator<Item = &'a str>>(lines: &mut std::iter::Peekable<I>) -> String {
    let mut sql = Vec::new();
    while let Some(l) = lines.peek() {
        if l.trim().is_empty() {
            break;
        }
        sql.push(lines.next().unwrap());
    }
    sql.join("\n")
}

/// Collect SQL lines for a `query` (until the `----` separator).
fn take_sql_until_separator<'a, I: Iterator<Item = &'a str>>(
    lines: &mut std::iter::Peekable<I>,
) -> String {
    let mut sql = Vec::new();
    while let Some(l) = lines.next() {
        if l.trim() == "----" {
            break;
        }
        sql.push(l);
    }
    sql.join("\n")
}

/// Render a query outcome to a flat, row-major vector of value strings, then apply
/// the sort mode. A non-query outcome renders empty (and will mismatch).
fn render_outcome(outcome: &Outcome, cols: usize, sortmode: &str) -> Vec<String> {
    let rows = match outcome {
        Outcome::Query { rows, .. } => rows,
        Outcome::Statement { .. } => return Vec::new(),
    };
    let mut flat = Vec::new();
    for row in rows {
        for v in row {
            flat.push(render_value(v));
        }
    }
    apply_sort(flat, cols, sortmode)
}

fn render_value(v: &Value) -> String {
    v.render()
}

/// Apply a sqllogictest sort mode to a flat row-major value list.
fn apply_sort(mut flat: Vec<String>, cols: usize, sortmode: &str) -> Vec<String> {
    match sortmode {
        "valuesort" => {
            flat.sort();
            flat
        }
        "rowsort" => {
            let mut rows: Vec<Vec<String>> = flat.chunks(cols.max(1)).map(|c| c.to_vec()).collect();
            rows.sort();
            rows.into_iter().flatten().collect()
        }
        _ => flat, // nosort (and unknown) keep order
    }
}
