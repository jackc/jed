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
//!
//! `--rebaseline` rewrites every `# cost: N` directive in place to the cost this core
//! accrues (the tool for re-baselining the corpus after a cost-schedule change). This
//! core is the writer; the Go/TS harnesses stay pure verifiers, so re-running them is the
//! independent cross-core check that all cores agree on the new costs (CLAUDE.md §8).

use jed::{Database, Outcome, SUPPORTED_CAPABILITIES, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let suites = suites_dir();
    let mut files = Vec::new();
    collect_tests(&suites, &mut files);
    files.sort();

    // `--rebaseline` rewrites each `# cost: N` directive in place to the cost this core
    // actually accrues, then exits — the tool for re-baselining the corpus after a
    // cost-schedule change (e.g. P6.3's `page_read`). The Rust harness is the **writer**;
    // the Go and TS harnesses stay pure verifiers, so running them afterwards is the
    // independent cross-core check that every core agrees on the new costs (CLAUDE.md §8).
    if std::env::args().any(|a| a == "--rebaseline") {
        let mut rewritten = 0u32;
        for file in &files {
            let text = std::fs::read_to_string(file).expect("read .test file");
            if let Some(updated) = rebaseline_file(&text) {
                std::fs::write(file, updated).expect("write .test file");
                let rel = file.strip_prefix(&suites).unwrap_or(file).display();
                println!("rebaselined {rel}");
                rewritten += 1;
            }
        }
        println!("\n{rewritten} file(s) rebaselined");
        return ExitCode::SUCCESS;
    }

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

/// Parse a `# max_cost: N` directive body. Returns the caller-set cost ceiling to run the
/// next record under, or None if this comment is not a max_cost directive. Mirrors `# cost:`,
/// but instead of asserting the accrued cost it *bounds* it: the record is expected to abort
/// with `54P01` once accrued cost reaches N (CLAUDE.md §13; spec/design/cost.md §6).
fn parse_max_cost_directive(rest: &str) -> Option<i64> {
    rest.trim_start()
        .strip_prefix("max_cost:")?
        .trim()
        .parse()
        .ok()
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

/// Parse a `# types: int16, text, decimal` directive body. Returns the asserted output column
/// types — each the canonical name of a result column's resolved type (the integer WIDTH, the
/// unconstrained `decimal`, `unknown` for an untyped NULL), beyond the `I`/`T`/`D` rendering tag
/// (spec/design/conformance.md §1/§7). None if this comment is not a types directive.
fn parse_types_directive(rest: &str) -> Option<Vec<String>> {
    let list = rest.trim_start().strip_prefix("types:")?;
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

/// Assert the query's output column types match a pending `# types:` directive (if any).
fn assert_types(
    expected: Option<&[String]>,
    actual: &[String],
    sql: &str,
) -> std::result::Result<(), String> {
    match expected {
        Some(e) if e != actual => Err(format!(
            "column-type mismatch\n  SQL: {sql}\n  expected: {e:?}\n  actual:   {actual:?}"
        )),
        _ => Ok(()),
    }
}

/// Run all records in one .test file against a fresh database. Returns the first
/// mismatch as an error string.
fn run_file(text: &str) -> std::result::Result<(), String> {
    let mut db = Database::new();
    let mut lines = text.lines().peekable();
    // A `# cost: N` / `# names: ...` / `# types: ...` / `# max_cost: N` directive sets these; the
    // next record consumes them.
    let mut pending_cost: Option<i64> = None;
    let mut pending_names: Option<Vec<String>> = None;
    let mut pending_types: Option<Vec<String>> = None;
    let mut pending_max_cost: Option<i64> = None;

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('#') {
            // `# cost:` / `# names:` / `# types:` bind to the next record; every other comment
            // is ignored.
            if let Some(n) = parse_cost_directive(rest) {
                pending_cost = Some(n);
            } else if let Some(n) = parse_max_cost_directive(rest) {
                pending_max_cost = Some(n);
            } else if let Some(names) = parse_names_directive(rest) {
                pending_names = Some(names);
            } else if let Some(types) = parse_types_directive(rest) {
                pending_types = Some(types);
            }
            continue;
        }
        // This record consumes any pending assertions (so they never leak forward).
        let expected_cost = pending_cost.take();
        let expected_names = pending_names.take();
        let expected_types = pending_types.take();
        // Apply the per-record cost ceiling (0 = unlimited); set each record so it auto-resets.
        db.set_max_cost(pending_max_cost.take().unwrap_or(0));
        let mut parts = trimmed.split_whitespace();
        let kind = parts.next().unwrap();
        match kind {
            "statement" => {
                // `# names:` / `# types:` assert result columns, which a statement lacks.
                if expected_names.is_some() {
                    return Err("# names: directive precedes a non-query statement".to_string());
                }
                if expected_types.is_some() {
                    return Err("# types: directive precedes a non-query statement".to_string());
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
                if !results_match(&expected, &actual, coltypes, cols, sortmode) {
                    return Err(format!(
                        "query result mismatch\n  SQL: {sql}\n  expected: {expected:?}\n  actual:   {actual:?}"
                    ));
                }
                assert_cost(expected_cost, outcome.cost(), &sql)?;
                assert_names(expected_names.as_deref(), outcome.column_names(), &sql)?;
                assert_types(expected_types.as_deref(), outcome.column_types(), &sql)?;
            }
            other => return Err(format!("unknown record kind '{other}'")),
        }
    }
    Ok(())
}

/// Rewrite each `# cost: N` directive in `text` to the cost this core actually accrues,
/// returning the updated text if anything changed (else `None`) — the engine of
/// `--rebaseline`. Mirrors `run_file`'s record walk over an indexed line buffer, but instead
/// of asserting cost it patches the pending `# cost:` line. Result/error assertions are
/// skipped (only costs change); each statement still runs so later records see the same
/// database state. A `# cost:` that binds to a record producing no cost (a `statement error`)
/// is consumed but left untouched, exactly as `run_file` would ignore it.
fn rebaseline_file(text: &str) -> Option<String> {
    let mut out: Vec<String> = text.lines().map(str::to_string).collect();
    let mut db = Database::new();
    let mut pending_cost_line: Option<usize> = None;
    let mut pending_max_cost: Option<i64> = None;
    let mut changed = false;
    let mut i = 0;
    while i < out.len() {
        let trimmed = out[i].trim().to_string();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('#') {
            if parse_cost_directive(rest).is_some() {
                pending_cost_line = Some(i);
            } else if let Some(n) = parse_max_cost_directive(rest) {
                pending_max_cost = Some(n);
            }
            i += 1;
            continue;
        }
        // A record: collect its SQL (advancing `i` past the whole record) and run it to
        // accrue cost + advance DB state. Apply any per-record cost ceiling so an aborting
        // record evolves the DB state identically to `run_file` (it writes nothing).
        db.set_max_cost(pending_max_cost.take().unwrap_or(0));
        let mut parts = trimmed.split_whitespace();
        let kind = parts.next().unwrap();
        let actual_cost: Option<i64> = match kind {
            "statement" => {
                let expect = parts.next().unwrap_or("");
                i += 1;
                let mut sql = Vec::new();
                while i < out.len() && !out[i].trim().is_empty() {
                    sql.push(out[i].clone());
                    i += 1;
                }
                let result = jed::execute(&mut db, &sql.join("\n"));
                // Only a `statement ok` carries a cost; an error record never does.
                if expect == "ok" {
                    result.ok().map(|o| o.cost())
                } else {
                    None
                }
            }
            "query" => {
                i += 1;
                let mut sql = Vec::new();
                while i < out.len() && out[i].trim() != "----" {
                    sql.push(out[i].clone());
                    i += 1;
                }
                // Skip the `----` separator and the expected rows (until a blank line).
                if i < out.len() {
                    i += 1;
                }
                while i < out.len() && !out[i].trim().is_empty() {
                    i += 1;
                }
                jed::execute(&mut db, &sql.join("\n"))
                    .ok()
                    .map(|o| o.cost())
            }
            _ => {
                i += 1;
                continue;
            }
        };
        if let (Some(line), Some(cost)) = (pending_cost_line.take(), actual_cost) {
            let indent_len = out[line].len() - out[line].trim_start().len();
            let new_line = format!("{}# cost: {cost}", &out[line][..indent_len]);
            if out[line] != new_line {
                out[line] = new_line;
                changed = true;
            }
        }
    }
    if changed {
        let trailing = if text.ends_with('\n') { "\n" } else { "" };
        Some(out.join("\n") + trailing)
    } else {
        None
    }
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

/// Compare an expected vs actual flat row-major result list, applying the per-column conformance
/// render tag (spec/design/conformance.md §1). For most coltype letters the compare is EXACT
/// string equality, but the **`R` (real / float)** tag compares BY VALUE within a tolerance
/// (spec/design/float.md §9): each core's native shortest-round-trip layout may differ and a
/// transcendental may differ by an ULP, so a column tagged `R` parses both sides to f64 and
/// considers them equal iff both NaN, or `==` (covers ±Inf and -0==+0), or both finite within a
/// small relative tolerance. Column association comes from the row-major position (`i % cols`),
/// valid for `nosort`/`rowsort`; `valuesort` loses it, so there we fall back to exact compare
/// (float columns use nosort/rowsort).
fn results_match(
    expected: &[String],
    actual: &[String],
    coltypes: &str,
    cols: usize,
    sortmode: &str,
) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    let letters: Vec<char> = coltypes.chars().collect();
    let per_column_ok = sortmode != "valuesort" && !letters.is_empty();
    for (i, (e, a)) in expected.iter().zip(actual.iter()).enumerate() {
        let is_real = per_column_ok && letters.get(i % cols) == Some(&'R');
        let ok = if is_real {
            real_values_equal(e, a)
        } else {
            e == a
        };
        if !ok {
            return false;
        }
    }
    true
}

/// The `R`-tag tolerant float compare (spec/design/float.md §9): parse both strings to f64, then
/// equal iff (a) both NaN, (b) exactly one NaN → not equal, (c) `a == b` (covers ±Inf and -0==+0),
/// (d) both finite and `|a-b| <= 1e-9 * max(|a|,|b|,1.0)`, else not equal. A side that does not
/// parse as a float falls back to exact string compare (e.g. `NULL`).
fn real_values_equal(e: &str, a: &str) -> bool {
    let (pe, pa) = (parse_render_float(e), parse_render_float(a));
    match (pe, pa) {
        (Some(x), Some(y)) => {
            let (xn, yn) = (x.is_nan(), y.is_nan());
            if xn || yn {
                return xn && yn;
            }
            if x == y {
                return true; // ±Inf exact, -0 == +0
            }
            if x.is_finite() && y.is_finite() {
                let tol = 1e-9 * x.abs().max(y.abs()).max(1.0);
                (x - y).abs() <= tol
            } else {
                false
            }
        }
        // A non-float rendering (e.g. `NULL`) — exact string compare.
        _ => e == a,
    }
}

/// Parse a rendered float string (incl. the PG spellings `Infinity`/`-Infinity`/`NaN`) to f64,
/// or `None` if it is not a float rendering.
fn parse_render_float(s: &str) -> Option<f64> {
    match s {
        "Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        "NaN" => Some(f64::NAN),
        _ => s.parse::<f64>().ok(),
    }
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
