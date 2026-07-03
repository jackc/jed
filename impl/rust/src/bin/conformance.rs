//! Conformance harness for the Rust core (CLAUDE.md §7).
//!
//! Walks spec/conformance/suites, and for each `.test` file whose `# requires:`
//! capabilities are all in this core's `SUPPORTED_CAPABILITIES`, runs the
//! sqllogictest-style records against a fresh `Engine` and compares output.
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

use jed::{CreateOptions, Database, Outcome, SUPPORTED_CAPABILITIES, Session as JedSession, Value};

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
// The stepped-threaded mode (and its channel/thread machinery) is test-only — gate its imports so
// the ordinary `cargo run --bin conformance` build (sequential mode) carries no unused imports.
#[cfg(test)]
use std::sync::mpsc::{self, Receiver, Sender};
#[cfg(test)]
use std::thread::{self, JoinHandle};

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

    // The corpus runs in one of two storage MODES (spec/design/conformance.md §3): the default
    // "memory" mode drives a fresh in-memory Database, and the "disk" mode (arg `disk`) drives a
    // FILE-BACKED database REOPENED before every record, so each committed read faults its leaves from
    // disk through the demand-paged buffer pool. The two modes catch DIVERGENCES between resident and
    // on-disk-faulted reads (the window-operand touched-set bug the in-memory pass could not see).
    // Every record's SQL-in → rows/error/cost-out must be IDENTICAL in both modes.
    let disk = std::env::args().any(|a| a == "disk");
    let mode = if disk { "disk" } else { "memory" };

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

        let is_conc = is_concurrency_format(&text);
        // Disk mode cannot run a file whose semantics can't survive a per-record REOPEN: the
        // concurrency driver (multi-session schedule on one Database), or a file carrying reopen-fragile
        // session state — session-local temp tables, a spanning transaction, a sticky lifetime budget,
        // or a pre-built fixture image (spec/design/conformance.md §3). These are `# skip: disk` and
        // covered by the memory pass only; none exercises the on-disk faulted read path anyway.
        if disk && (is_conc || parse_skip_disk(&text)) {
            println!("SKIP {rel}  (disk-mode)");
            skipped += 1;
            continue;
        }

        // A `# format: concurrency` file is an explicit multi-session schedule run against a
        // Database (spec/design/concurrency-testing.md §4); everything else is the sequential
        // single-handle runner. Both share the result grammar; only the driver differs. The binary
        // always runs the canonical stepped-SEQUENTIAL mode; the stepped-threaded mode is exercised
        // by `cargo test` (the concurrency_threaded_tests below).
        let outcome = if is_conc {
            run_concurrency_file(&text)
        } else {
            run_file(&text, disk)
        };
        match outcome {
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

    println!("\n[{mode}] {passed} passed, {failed} failed, {skipped} skipped");
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn suites_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/conformance/suites")
}

/// Parse a `# load-collation: <name> = <fixture-path>[, <fixture-path>…]` directive body — the
/// corpus's deterministic, host-free way to make a collation available before the records that use
/// it (spec/design/collation.md §10). The named collation is provided by the engine's loaded `JUCD`
/// bundle (`load_collation` loads it); the fixture paths are now a documentary provenance note (the
/// collation's source definitions), not loaded. Returns the collation name and paths, or None.
fn parse_load_collation_directive(rest: &str) -> Option<(String, Vec<String>)> {
    let body = rest.trim_start().strip_prefix("load-collation:")?.trim();
    let (name, files) = body.split_once('=')?;
    let files: Vec<String> = files
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if files.is_empty() {
        return None;
    }
    Some((name.trim().to_string(), files))
}

/// Make a collation named `name` available to the records that follow (spec/design/collation.md
/// §2/§9/§10). The harness acts as the *host*: it loads jed's own pinned production `JUCD` bundle
/// (spec/collation/fixtures/unicode.jucd) into the engine-global set via `db.LoadUnicodeData`
/// (idempotent — the set is global), exactly as a production host would, then asserts the named
/// collation now resolves. A name no loaded bundle provides fails the test file, naming it.
fn load_collation(name: &str) -> std::result::Result<(), String> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/collation/fixtures/unicode.jucd");
    let bytes = std::fs::read(&path)
        .map_err(|e| format!("load-collation: read {}: {e}", path.display()))?;
    jed::load_unicode_data(&bytes)
        .map_err(|e| format!("load-collation: load unicode.jucd: {}", e.message))?;
    if jed::loaded_collation(name).is_some() {
        return Ok(());
    }
    Err(format!(
        "load-collation: collation \"{name}\" is not provided by the loaded bundle"
    ))
}

/// Parse a `# load-timezone: [<zone>]` directive body — the corpus's host-free way to make the IANA
/// time-zone data available before the records that use `AT TIME ZONE` (timezones.md §11). Loads
/// jed's pinned `JTZ` bundle; an optional zone name is asserted to resolve. Returns the (possibly
/// empty) zone name, or None if not this directive.
fn parse_load_timezone_directive(rest: &str) -> Option<String> {
    let body = rest.trim_start().strip_prefix("load-timezone:")?.trim();
    Some(body.to_string())
}

/// Make the IANA time zones available to the records that follow (timezones.md §3.3/§11). The harness
/// acts as the *host*: it loads jed's pinned production `JTZ` bundle (spec/tz/fixtures/tzdata.jtz)
/// into the engine-global set via `db.LoadTimeZoneData` (idempotent — the set is global), then, if a
/// zone name was given, asserts it now resolves. A named zone no loaded bundle provides fails the file.
fn load_timezone(name: &str) -> std::result::Result<(), String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/tz/fixtures/tzdata.jtz");
    let bytes =
        std::fs::read(&path).map_err(|e| format!("load-timezone: read {}: {e}", path.display()))?;
    jed::load_time_zone_data(&bytes)
        .map_err(|e| format!("load-timezone: load tzdata.jtz: {}", e.message))?;
    if name.is_empty() || jed::resolve_zone(name).is_some() {
        return Ok(());
    }
    Err(format!(
        "load-timezone: zone \"{name}\" is not provided by the loaded bundle"
    ))
}

/// Parse a `# timezone: <zone>` directive body (spec/design/session.md §6.2, timezones.md §9.4): the
/// SESSION time zone for the next record (the zone a `timestamptz` decomposes in). Per-record (reset
/// to `UTC` after), like `# set:`. A named zone must already be loaded (`# load-timezone:`). Distinct
/// from `# load-timezone:` (which loads the bundle). Returns the zone, or None if not this directive.
fn parse_timezone_directive(rest: &str) -> Option<String> {
    let body = rest.trim_start().strip_prefix("timezone:")?.trim();
    Some(body.to_string())
}

/// Parse a file-level `# fixture: <spec-relative-path>` directive — the corpus's way to run a file
/// against a PRE-BUILT database image instead of a fresh `Engine::new()`, so a test can exercise
/// on-disk state that SQL cannot construct (a version-skewed collation pin + a wrong-for-loaded
/// index — the skew read-safety regression, spec/design/collation.md §12/§14). The path is relative
/// to `spec/`. Gated by the `harness.fixture_open` capability. Returns the path, or None.
/// Parse a file-level `# attach: <name>` directive — the corpus's way to attach a fresh, empty
/// READ-WRITE in-memory database named `<name>` to the running handle before the records run
/// (spec/design/attached-databases.md §6), so SQL can `CREATE TABLE <name>.t`, populate it, and join
/// across attachments. Returns the name, or None if not this directive. Gated by `harness.attach`.
fn parse_attach_directive(rest: &str) -> Option<String> {
    let body = rest.trim_start().strip_prefix("attach:")?.trim();
    if body.is_empty() {
        return None;
    }
    Some(body.to_string())
}

fn parse_fixture_directive(rest: &str) -> Option<String> {
    let body = rest.trim_start().strip_prefix("fixture:")?.trim();
    (!body.is_empty()).then(|| body.to_string())
}

/// Whether a file carries a `# skip: disk[ — free-text reason]` directive (spec/design/conformance.md
/// §3) — it opts out of the on-disk reopen pass because its session state (temp tables, a spanning
/// transaction, a sticky `lifetime_max_cost` budget) or its pre-built `# fixture:` image cannot
/// survive a per-record reopen. Honored only in disk mode; the memory pass ignores it. The first
/// whitespace token after `skip:` is the mode; any trailing text is a documentary reason.
fn parse_skip_disk(text: &str) -> bool {
    text.lines().any(|line| {
        line.trim()
            .strip_prefix('#')
            .and_then(|rest| rest.trim_start().strip_prefix("skip:"))
            .and_then(|v| v.split_whitespace().next())
            == Some("disk")
    })
}

/// A unique temp path for a disk-mode file's backing `.jed` image. Deterministic-free (this is the
/// harness, not the engine — std is fine here), unique per (process, call) so parallel `cargo test`
/// harness runs never collide.
fn disk_temp_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("jed-conformance-{}-{n}.jed", std::process::id()))
}

/// Recognize the `# upgrade-collations:` directive body (any/empty body after the prefix). A
/// file-level ACTION that runs the COLLATION UPGRADE migration (`db.upgrade_collations`) on the
/// running database — the privileged host op a test drives to clear a version-skew and assert the
/// table is read-write again (spec/design/collation.md §12; capability `harness.upgrade_collations`).
fn parse_upgrade_collations_directive(rest: &str) -> bool {
    rest.trim_start().starts_with("upgrade-collations:")
}

/// Open the pre-built database image named by a `# fixture:` directive (path relative to `spec/`).
/// The harness acts as the host: it first loads jed's pinned production bundle so any referenced
/// collation resolves on open (a skewed pin still resolves — to a *different* version, which is the
/// point), then reconstructs the database in memory via `from_image`. The handle is read-WRITE so a
/// write against a skewed table exercises the real XX002 guard (collation.md §12), not a
/// read-only-handle error.
fn open_fixture(rel: &str) -> std::result::Result<Database, String> {
    let bundle =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/collation/fixtures/unicode.jucd");
    if let Ok(bytes) = std::fs::read(&bundle) {
        let _ = jed::load_unicode_data(&bytes); // idempotent: the loaded set is engine-global
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec")
        .join(rel);
    let bytes =
        std::fs::read(&path).map_err(|e| format!("fixture: read {}: {e}", path.display()))?;
    Database::from_image(&bytes).map_err(|e| format!("fixture: open {rel}: {}", e.message))
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

/// Parse a `# lifetime_max_cost: N` directive body. Returns the per-SESSION cumulative cost budget,
/// or None if this comment is not a lifetime_max_cost directive. Unlike `# max_cost:` (per-record,
/// reset after each record), this is **sticky**: it sets the session budget for the rest of the file
/// (the cumulative cost builds across records on the one Engine the file runs against), so an
/// ordered statement sequence can drive the session to its budget and assert the `54P02` abort —
/// what the per-record `# cost:` directive cannot express (spec/design/session.md §5.4).
fn parse_lifetime_max_cost_directive(rest: &str) -> Option<i64> {
    rest.trim_start()
        .strip_prefix("lifetime_max_cost:")?
        .trim()
        .parse()
        .ok()
}

/// Parse a `# max_sql_length: N` directive body. Returns the per-handle input-size limit (bytes)
/// to run the next record under, or None if this comment is not a max_sql_length directive.
/// Mirrors `# max_cost:`: it lets a record set a *small* cap and assert that an over-long
/// statement aborts with `54000` (CLAUDE.md §13; spec/design/cost.md §7, api.md §8). `0` is
/// unlimited; absent ⇒ the engine default (1 MiB) for every other record.
fn parse_max_sql_length_directive(rest: &str) -> Option<usize> {
    rest.trim_start()
        .strip_prefix("max_sql_length:")?
        .trim()
        .parse()
        .ok()
}

/// Parse a comma/whitespace-separated privilege list (`SELECT, INSERT`, `EXECUTE`, the keyword
/// `ALL` = the four table privileges, `NONE` = the empty set) into a [`PrivilegeSet`]. Used by the
/// `# default_privileges:` / `# grant:` / `# revoke:` directives (spec/design/session.md §5.3).
fn parse_priv_set(list: &str) -> Option<jed::PrivilegeSet> {
    let body = list.trim();
    if body.eq_ignore_ascii_case("none") {
        return Some(jed::PrivilegeSet::EMPTY);
    }
    if body.eq_ignore_ascii_case("all") {
        return Some(jed::PrivilegeSet::ALL_TABLE);
    }
    let mut set = jed::PrivilegeSet::EMPTY;
    for tok in body.split(',') {
        let name = tok.trim();
        if name.is_empty() {
            continue;
        }
        set = set.with(jed::Privilege::from_name(name)?);
    }
    Some(set)
}

/// Parse a `# default_privileges: SELECT, INSERT` directive body (spec/design/session.md §5.3): the
/// table-privilege set granted to **every** table for the next record (`NONE` / `ALL` accepted).
fn parse_default_privileges_directive(rest: &str) -> Option<jed::PrivilegeSet> {
    parse_priv_set(rest.trim_start().strip_prefix("default_privileges:")?)
}

/// Parse a `# grant: PRIVS ON object` / `# revoke: PRIVS ON object` directive body (after the
/// `grant:` / `revoke:` prefix is stripped): the privilege set and the lowercased object name. The
/// object is the single word after the `ON` keyword (spec/design/session.md §5.3).
fn parse_priv_delta(body: &str) -> Option<(jed::PrivilegeSet, String)> {
    let lower = body.to_ascii_lowercase();
    let on = lower.find(" on ")?;
    let privs = parse_priv_set(&body[..on])?;
    let object = body[on + 4..].trim();
    if object.is_empty() || object.split_whitespace().count() != 1 {
        return None;
    }
    Some((privs, object.to_string()))
}

/// Parse a `# grant: PRIVS ON object` directive body (spec/design/session.md §5.3).
fn parse_grant_directive(rest: &str) -> Option<(jed::PrivilegeSet, String)> {
    parse_priv_delta(rest.trim_start().strip_prefix("grant:")?)
}

/// Parse a `# revoke: PRIVS ON object` directive body (spec/design/session.md §5.3).
fn parse_revoke_directive(rest: &str) -> Option<(jed::PrivilegeSet, String)> {
    parse_priv_delta(rest.trim_start().strip_prefix("revoke:")?)
}

/// Parse a `# allow_ddl: on|off` directive body (spec/design/session.md §5.3): whether DDL is
/// permitted on the session for the next record (`on`/`true` ⇒ allowed, `off`/`false` ⇒ denied).
fn parse_allow_ddl_directive(rest: &str) -> Option<bool> {
    let v = rest.trim_start().strip_prefix("allow_ddl:")?.trim();
    match v.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" => Some(true),
        "off" | "false" | "no" => Some(false),
        _ => None,
    }
}

/// Parse a `# allow_temp_ddl: on|off` directive body (spec/design/temp-tables.md §5): whether
/// session-local temporary-table DDL is permitted for the next record. The temp-scoped split of
/// `allow_ddl`; per-record, reset after.
fn parse_allow_temp_ddl_directive(rest: &str) -> Option<bool> {
    let v = rest.trim_start().strip_prefix("allow_temp_ddl:")?.trim();
    match v.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" => Some(true),
        "off" | "false" | "no" => Some(false),
        _ => None,
    }
}

/// Parse a `# temp_buffers: N` directive body (spec/design/temp-tables.md §7): the per-session
/// temp-table storage budget (bytes) to run the next record under (`0` ⇒ unlimited). Mirrors
/// `# max_cost:` — per-record, reset after — so a record can set a small budget and assert that an
/// over-budget temp write traps `54P03`.
fn parse_temp_buffers_directive(rest: &str) -> Option<usize> {
    rest.trim_start()
        .strip_prefix("temp_buffers:")?
        .trim()
        .parse()
        .ok()
}

/// Parse a `# set: name=value, name2=value2` directive body (spec/design/session.md §6.1): the
/// session variables to set for the next record, returned as `(name, value)` pairs (or None if this
/// comment is not a `set:` directive). Per-record (reset after), like `# seed:` / `# grant:`. Each
/// pair splits on the first `=`; names are dotted (custom) variables (a non-dotted name would be
/// rejected `42704` when applied).
fn parse_set_directive(rest: &str) -> Option<Vec<(String, String)>> {
    let body = rest.trim_start().strip_prefix("set:")?;
    let mut pairs = Vec::new();
    for part in body.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (name, value) = part.split_once('=')?;
        pairs.push((name.trim().to_string(), value.trim().to_string()));
    }
    Some(pairs)
}

/// Parse a `# seed: N` directive body (entropy.md §6): the fixed PRNG seed (u64) to run the next
/// record under, making the volatile uuid generators deterministic + cross-core identical.
fn parse_seed_directive(rest: &str) -> Option<u64> {
    rest.trim_start().strip_prefix("seed:")?.trim().parse().ok()
}

/// Parse a `# clock: N` directive body (entropy.md §6): the fixed statement clock (i64 micros
/// since the Unix epoch) to run the next record under, fixing uuidv7's embedded timestamp.
fn parse_clock_directive(rest: &str) -> Option<i64> {
    rest.trim_start()
        .strip_prefix("clock:")?
        .trim()
        .parse()
        .ok()
}

/// Parse a `# clock_advance: start,step` directive body (entropy.md §6): an advancing clock that
/// returns `start`, `start+step`, … one increment per read, so `clock_timestamp()`'s per-call reads
/// are deterministic and distinguishable from the statement-stable `now()`. Returns `(start, step)`.
fn parse_clock_advance_directive(rest: &str) -> Option<(i64, i64)> {
    let body = rest.trim_start().strip_prefix("clock_advance:")?;
    let (start, step) = body.split_once(',')?;
    Some((start.trim().parse().ok()?, step.trim().parse().ok()?))
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

/// Parse a `# types: i16, text, decimal` directive body. Returns the asserted output column
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
fn run_file(text: &str, disk: bool) -> std::result::Result<(), String> {
    // A Drop guard removes the disk-mode temp image on every exit path (memory mode: no temp file).
    struct TempGuard(Option<PathBuf>);
    impl Drop for TempGuard {
        fn drop(&mut self) {
            if let Some(p) = &self.0 {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    // Declared BEFORE `db`, so it drops AFTER `db` (reverse declaration order) — the handle closes,
    // then the file is removed.
    let tmp_path = if disk { Some(disk_temp_path()) } else { None };
    let _guard = TempGuard(tmp_path.clone());
    // In DISK mode the file is a temp .jed image reopened before every record (below), so each
    // committed read faults from disk; in MEMORY mode it is a fresh in-memory Database
    // (spec/design/conformance.md §3).
    let mut db = match &tmp_path {
        Some(p) => Database::create(CreateOptions {
            path: Some(std::path::PathBuf::from(p)),
            ..Default::default()
        })
        .map_err(|e| format!("disk mode: create {}: {}", p.display(), e.message))?,
        None => Database::create(CreateOptions::default()).unwrap(),
    };
    // `on_temp` tracks whether db/sess still point at the reopenable temp-file handle (a `# fixture:`
    // swap flips it off — but fixtures are `# skip: disk`, so that never coexists with disk mode).
    let mut on_temp = disk;
    // `Database` no longer owns a persistent default session (it mints a fresh session per
    // convenience call), so the harness drives one explicit session per file. Keeping a single
    // session for the whole file preserves the cross-record state the sequential corpus relies on
    // (sticky `lifetime_max_cost`, session-local temp tables, the per-record `# set:`/privilege
    // resets). Re-minted whenever a `# fixture:` swaps the underlying database, or (disk mode) whenever
    // the file is reopened before a record.
    let mut sess = db.session(jed::SessionOptions::default());
    let mut lines = text.lines().peekable();
    // A `# cost: N` / `# names: ...` / `# types: ...` / `# max_cost: N` directive sets these; the
    // next record consumes them.
    let mut pending_cost: Option<i64> = None;
    let mut pending_names: Option<Vec<String>> = None;
    let mut pending_types: Option<Vec<String>> = None;
    let mut pending_max_cost: Option<i64> = None;
    let mut pending_max_sql_length: Option<usize> = None;
    let mut pending_seed: Option<u64> = None;
    let mut pending_clock: Option<i64> = None;
    let mut pending_clock_advance: Option<(i64, i64)> = None;
    // The session privilege envelope for the next record (spec/design/session.md §5.3); reset after
    // each record so a directive never leaks forward. `grant`/`revoke` accumulate across lines.
    let mut pending_default_privileges: Option<jed::PrivilegeSet> = None;
    let mut pending_grants: Vec<(jed::PrivilegeSet, String)> = Vec::new();
    let mut pending_revokes: Vec<(jed::PrivilegeSet, String)> = Vec::new();
    let mut pending_allow_ddl: Option<bool> = None;
    let mut pending_allow_temp_ddl: Option<bool> = None;
    let mut pending_temp_buffers: Option<usize> = None;
    let mut pending_vars: Vec<(String, String)> = Vec::new();
    let mut pending_timezone: Option<String> = None;

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('#') {
            // `# load-collation:` is an ACTION (assert available now), not a pending assertion: the
            // named collation must be vendored in this build before the records run
            // (spec/design/collation.md §2/§9/§10).
            if let Some((name, _files)) = parse_load_collation_directive(rest) {
                load_collation(&name)?;
                continue;
            }
            // `# load-timezone: [<zone>]` is an ACTION: load jed's pinned `JTZ` bundle into the
            // engine-global set (and optionally assert a zone resolves) before the records that use
            // `AT TIME ZONE` (timezones.md §11).
            if let Some(name) = parse_load_timezone_directive(rest) {
                load_timezone(&name)?;
                continue;
            }
            // `# attach: <name>` (file-level) attaches a fresh empty read-write in-memory database to
            // the running handle (attached-databases.md §6): the records then CREATE / populate / join
            // it by the `<name>.table` qualifier. An ACTION applied to the Database, so every session
            // over it sees the attachment (refresh_committed re-pins the attached roots per statement).
            // In-memory attachments cannot survive the disk reopen, so # attach: files are # skip: disk.
            if let Some(name) = parse_attach_directive(rest) {
                db.attach(&name, jed::AttachSource::memory(), false)
                    .map_err(|e| format!("attach {name:?}: {}", e.message))?;
                continue;
            }
            // `# fixture:` (file-level) opens a PRE-BUILT image in place of the fresh `Engine::new()`
            // above — appears in the header before any record (spec/design/conformance.md).
            if let Some(rel) = parse_fixture_directive(rest) {
                db = open_fixture(&rel)?;
                sess = db.session(jed::SessionOptions::default());
                on_temp = false; // the handle is now the fixture image, not the reopenable temp file
                continue;
            }
            // `# upgrade-collations:` (file-level) runs the COLLATION UPGRADE migration on the running
            // DB — the privileged host op (`db.upgrade_collations`) that clears a version-skew
            // (collation.md §12); the records after it assert the table is read-write again.
            if parse_upgrade_collations_directive(rest) {
                sess.upgrade_collations()
                    .map_err(|e| format!("upgrade-collations: {}", e.message))?;
                continue;
            }
            // `# cost:` / `# names:` / `# types:` bind to the next record; every other comment
            // is ignored.
            if let Some(n) = parse_cost_directive(rest) {
                pending_cost = Some(n);
            } else if let Some(n) = parse_lifetime_max_cost_directive(rest) {
                // Sticky (spec/design/session.md §5.4): apply immediately and persistently — the
                // session cumulative builds across records, so a later record can assert the 54P02
                // abort. Not a pending per-record directive (it must NOT reset between records).
                sess.set_lifetime_max_cost(n);
            } else if let Some(n) = parse_max_cost_directive(rest) {
                pending_max_cost = Some(n);
            } else if let Some(n) = parse_max_sql_length_directive(rest) {
                pending_max_sql_length = Some(n);
            } else if let Some(p) = parse_default_privileges_directive(rest) {
                pending_default_privileges = Some(p);
            } else if let Some(g) = parse_grant_directive(rest) {
                pending_grants.push(g);
            } else if let Some(r) = parse_revoke_directive(rest) {
                pending_revokes.push(r);
            } else if let Some(a) = parse_allow_ddl_directive(rest) {
                pending_allow_ddl = Some(a);
            } else if let Some(a) = parse_allow_temp_ddl_directive(rest) {
                pending_allow_temp_ddl = Some(a);
            } else if let Some(n) = parse_temp_buffers_directive(rest) {
                pending_temp_buffers = Some(n);
            } else if let Some(vars) = parse_set_directive(rest) {
                pending_vars.extend(vars);
            } else if let Some(z) = parse_timezone_directive(rest) {
                pending_timezone = Some(z);
            } else if let Some(s) = parse_seed_directive(rest) {
                pending_seed = Some(s);
            } else if let Some(c) = parse_clock_directive(rest) {
                pending_clock = Some(c);
            } else if let Some(sa) = parse_clock_advance_directive(rest) {
                pending_clock_advance = Some(sa);
            } else if let Some(names) = parse_names_directive(rest) {
                pending_names = Some(names);
            } else if let Some(types) = parse_types_directive(rest) {
                pending_types = Some(types);
            }
            continue;
        }
        // DISK mode: reopen the temp image before this record so its reads fault leaves from disk (the
        // committed state carries on the file; the fresh session re-receives the per-record directives
        // applied just below). Writes reopen too — an UPDATE/DELETE over a faulted leaf exercises the
        // resolve-and-rewrite path. No-op in memory mode or after a fixture swap (which is `# skip: disk`).
        // Reassigning drops the old handle first (no file lock — src/paging.rs), so the reopen reads the
        // just-committed image.
        if disk && on_temp {
            let p = tmp_path.as_ref().expect("disk mode has a temp path");
            db = Database::open(p)
                .map_err(|e| format!("disk reopen: open {}: {}", p.display(), e.message))?;
            sess = db.session(jed::SessionOptions::default());
        }
        // This record consumes any pending assertions (so they never leak forward).
        let expected_cost = pending_cost.take();
        let expected_names = pending_names.take();
        let expected_types = pending_types.take();
        // Apply the per-record cost ceiling (0 = unlimited); set each record so it auto-resets.
        sess.set_max_cost(pending_max_cost.take().unwrap_or(0));
        // Apply the per-record input-size cap; absent ⇒ the engine default (1 MiB), so a
        // `# max_sql_length:` directive never leaks past its record (cost.md §7, api.md §8).
        sess.set_max_sql_length(
            pending_max_sql_length
                .take()
                .unwrap_or(jed::DEFAULT_MAX_SQL_LENGTH),
        );
        // Apply the per-record entropy seed + statement clock for the uuid generators (entropy.md
        // §6); absent ⇒ cleared (OS entropy / wall clock), so a directive never leaks forward.
        match pending_seed.take() {
            Some(s) => sess.set_random_source(jed::seeded_random_source(s)),
            None => sess.clear_random_source(),
        }
        // `# clock_advance:` (an advancing clock) takes precedence over `# clock:` (a fixed one);
        // a record uses at most one. Absent ⇒ cleared, so a clock directive never leaks forward.
        match (pending_clock_advance.take(), pending_clock.take()) {
            (Some((start, step)), _) => sess.set_clock_source(jed::advancing_clock(start, step)),
            (None, Some(c)) => sess.set_clock_source(jed::fixed_clock(c)),
            (None, None) => sess.clear_clock_source(),
        }
        // Apply the per-record session privilege envelope (spec/design/session.md §5.3): reset to
        // fully permissive (every table privilege, DDL allowed), then layer the pending directives,
        // so a `# default_privileges:` / `# grant:` / `# revoke:` / `# allow_ddl:` decorates only its
        // record and never leaks forward.
        sess.reset_privileges();
        if let Some(p) = pending_default_privileges.take() {
            sess.set_default_privileges(p);
        }
        for (privs, object) in pending_grants.drain(..) {
            sess.grant(privs, &object);
        }
        for (privs, object) in pending_revokes.drain(..) {
            sess.revoke(privs, &object);
        }
        if let Some(a) = pending_allow_ddl.take() {
            sess.set_allow_ddl(a);
        }
        // `# allow_temp_ddl:` overrides the temp-DDL gate (temp-tables.md §5); `reset_privileges` above
        // set it back to permissive, so it decorates only its record.
        if let Some(a) = pending_allow_temp_ddl.take() {
            sess.set_allow_temp_ddl(a);
        }
        // Apply the per-record temp-storage budget (temp-tables.md §7); absent ⇒ unlimited (`0`), so a
        // `# temp_buffers:` directive never leaks past its record. Mirrors `# max_cost:`.
        sess.set_temp_buffers(pending_temp_buffers.take().unwrap_or(0));
        // Apply the per-record session variables (spec/design/session.md §6.1): clear, then set each
        // pending `# set:` pair, so a directive decorates only its record and never leaks forward.
        sess.reset_vars();
        for (name, value) in pending_vars.drain(..) {
            sess.set_var(&name, &value)
                .expect("# set: directive uses a dotted (custom) variable name");
        }
        // Apply the per-record session time zone (spec/design/session.md §6.2, timezones.md §9.4):
        // reset to UTC, then set the pending `# timezone:` zone, so a directive decorates only its
        // record and never leaks forward. A named zone must already be loaded (`# load-timezone:`).
        sess.set_time_zone(pending_timezone.take().as_deref().unwrap_or("UTC"))
            .unwrap_or_else(|e| panic!("# timezone: {}", e.message));
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
                let result = sess.execute(&sql, &[]);
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
                let outcome = sess.execute(&sql, &[]).map_err(|e| {
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
    // A `# format: concurrency` schedule carries no `# cost:` directives and is not an ordinary
    // record stream — never rewrite it (its `open`/`on`/… directives would mis-parse as records).
    if is_concurrency_format(text) {
        return None;
    }
    let mut out: Vec<String> = text.lines().map(str::to_string).collect();
    let mut db = Database::create(CreateOptions::default()).unwrap();
    // One explicit session per file (see `run_file`): `Database` no longer keeps a persistent
    // default session, so the cost walk drives the same per-file session it would in `run_file`.
    let mut sess = db.session(jed::SessionOptions::default());
    let mut pending_cost_line: Option<usize> = None;
    let mut pending_max_cost: Option<i64> = None;
    let mut pending_max_sql_length: Option<usize> = None;
    let mut pending_seed: Option<u64> = None;
    let mut pending_clock: Option<i64> = None;
    let mut pending_clock_advance: Option<(i64, i64)> = None;
    let mut changed = false;
    let mut i = 0;
    while i < out.len() {
        let trimmed = out[i].trim().to_string();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('#') {
            if let Some(rel) = parse_fixture_directive(rest) {
                // Mirror `run_file`: a fixture file evolves DB state from the pre-built image, not a
                // fresh DB, so the cost walk sees the same starting state.
                db = open_fixture(&rel).ok()?;
                sess = db.session(jed::SessionOptions::default());
            } else if let Some(name) = parse_load_timezone_directive(rest) {
                // Mirror `run_file`: load the bundle so a named-zone record runs (and accrues cost)
                // during the cost walk (timezones.md §11).
                load_timezone(&name).ok()?;
            } else if let Some(z) = parse_timezone_directive(rest) {
                // The session zone does not change cost (zone-agnostic), but set it so the record
                // re-executes identically to `run_file`.
                sess.set_time_zone(&z).ok()?;
            } else if parse_upgrade_collations_directive(rest) {
                // Mirror `run_file`: clear a version-skew so the post-upgrade records run against the
                // migrated (read-write) state.
                sess.upgrade_collations().ok()?;
            } else if parse_cost_directive(rest).is_some() {
                pending_cost_line = Some(i);
            } else if let Some(n) = parse_lifetime_max_cost_directive(rest) {
                // Sticky session budget (spec/design/session.md §5.4): apply immediately so the DB
                // state evolves identically to `run_file` (an aborting record writes nothing).
                sess.set_lifetime_max_cost(n);
            } else if let Some(n) = parse_max_cost_directive(rest) {
                pending_max_cost = Some(n);
            } else if let Some(n) = parse_max_sql_length_directive(rest) {
                pending_max_sql_length = Some(n);
            } else if let Some(s) = parse_seed_directive(rest) {
                pending_seed = Some(s);
            } else if let Some(c) = parse_clock_directive(rest) {
                pending_clock = Some(c);
            } else if let Some(sa) = parse_clock_advance_directive(rest) {
                pending_clock_advance = Some(sa);
            }
            i += 1;
            continue;
        }
        // A record: collect its SQL (advancing `i` past the whole record) and run it to
        // accrue cost + advance DB state. Apply any per-record cost ceiling so an aborting
        // record evolves the DB state identically to `run_file` (it writes nothing).
        sess.set_max_cost(pending_max_cost.take().unwrap_or(0));
        sess.set_max_sql_length(
            pending_max_sql_length
                .take()
                .unwrap_or(jed::DEFAULT_MAX_SQL_LENGTH),
        );
        match pending_seed.take() {
            Some(s) => sess.set_random_source(jed::seeded_random_source(s)),
            None => sess.clear_random_source(),
        }
        match (pending_clock_advance.take(), pending_clock.take()) {
            (Some((start, step)), _) => sess.set_clock_source(jed::advancing_clock(start, step)),
            (None, Some(c)) => sess.set_clock_source(jed::fixed_clock(c)),
            (None, None) => sess.clear_clock_source(),
        }
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
                let result = sess.execute(&sql.join("\n"), &[]);
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
                sess.execute(&sql.join("\n"), &[]).ok().map(|o| o.cost())
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

// ============================================================================================
// The concurrency schedule runner (spec/design/concurrency-testing.md §4).
//
// A `.test` file carrying a `# format: concurrency` header is an explicit total order over named
// read/write SESSIONS opened on one Database. Because jed read results depend only on the logical
// order of commits and pin-points — never on timing (§2) — executing the listed order yields the
// canonical, deterministic result every core must produce. Two execution modes share one parse:
//   - stepped-SEQUENTIAL (the binary's default): walk the steps on one thread — defines canonical output.
//   - stepped-THREADED (`cargo test`, opt-in): one OS thread per session, the listed order enforced
//     with a turn token (signal turn → session executes → signal done → advance). Same schedule,
//     same result, but it drives the real concurrent code paths under the race detector / TSan and
//     proves Database is `Send + Sync` (it is moved into each worker thread). §4.3.
//
// The result grammar (statement / query, sortmodes, the R float tag) is reused verbatim from the
// sequential runner above — only the session control + state assertions are new.
// ============================================================================================

/// Report whether `text` opts into the schedule format via a `# format: concurrency` header line.
/// Any other (or absent) format is the ordinary sequential runner.
fn is_concurrency_format(text: &str) -> bool {
    for line in text.lines() {
        let t = line.trim();
        if !t.starts_with('#') {
            continue;
        }
        let rest = t.trim_start_matches('#').trim();
        if let Some(v) = rest.strip_prefix("format:") {
            return v.trim() == "concurrency";
        }
    }
    false
}

/// One sqllogictest record body run via `on <sid>`. `Clone` so it can be sent to a worker thread.
#[derive(Clone)]
enum Record {
    Statement {
        expect: String,
        code: String,
        sql: String,
    },
    Query {
        coltypes: String,
        sortmode: String,
        sql: String,
        expected: Vec<String>,
    },
}

/// One step of a schedule — the parsed form shared by both execution modes.
enum Step {
    Open {
        sid: String,
        mode: String,
        /// The Layer 2 `blocks` annotation (a writer-open on a currently-held gate, §5).
        blocks: bool,
    },
    On {
        sid: String,
        record: Record,
    },
    Commit(String),
    Rollback(String),
    Close(String),
    ExpectVersion(u64),
    ExpectOldestLive(u64),
}

/// One open handle in a schedule: a unified [`jed::Session`] tagged with its read/write mode (so the
/// end step dispatches commit vs. close — §2.4 folded `ReadHandle`/`WriteHandle` into one type). Used
/// only by the sequential runner (the threaded runner keeps each handle on its own worker thread).
enum Session {
    Read(JedSession),
    Write(JedSession),
}
impl Session {
    fn handle(&mut self) -> &mut JedSession {
        match self {
            Session::Read(h) => h,
            Session::Write(h) => h,
        }
    }
}

/// Run one `on <sid>` record against `sess`, returning the first mismatch as an error string. A
/// write through a read-only session is `25006` (rejected before dispatch, never poisoning it).
fn run_record(sess: &mut JedSession, sid: &str, record: &Record) -> Result<(), String> {
    match record {
        Record::Statement { expect, code, sql } => {
            let result = sess.execute(sql, &[]);
            match expect.as_str() {
                "ok" => {
                    if let Err(e) = result {
                        return Err(format!(
                            "[{sid}] statement expected ok, got error {}: {}\n  SQL: {sql}",
                            e.code(),
                            e.message
                        ));
                    }
                }
                "error" => match result {
                    Ok(_) => {
                        return Err(format!(
                            "[{sid}] statement expected error {code}, but it succeeded\n  SQL: {sql}"
                        ));
                    }
                    Err(e) if e.code() == code => {}
                    Err(e) => {
                        return Err(format!(
                            "[{sid}] statement expected error {code}, got {}\n  SQL: {sql}",
                            e.code()
                        ));
                    }
                },
                other => return Err(format!("[{sid}] unknown statement kind '{other}'")),
            }
        }
        Record::Query {
            coltypes,
            sortmode,
            sql,
            expected,
        } => {
            let outcome = sess.execute(sql, &[]).map_err(|e| {
                format!(
                    "[{sid}] query failed with {}: {}\n  SQL: {sql}",
                    e.code(),
                    e.message
                )
            })?;
            let cols = coltypes.len().max(1);
            let actual = render_outcome(&outcome, cols, sortmode);
            let exp = apply_sort(expected.clone(), cols, sortmode);
            if !results_match(&exp, &actual, coltypes, cols, sortmode) {
                return Err(format!(
                    "[{sid}] query result mismatch\n  SQL: {sql}\n  expected: {exp:?}\n  actual:   {actual:?}"
                ));
            }
        }
    }
    Ok(())
}

/// Whether `line` ends the current record body: blank, a comment, or the next schedule directive.
/// (A schedule does not separate records with blank lines, so a record runs until a boundary.)
fn is_boundary(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return true;
    }
    let first = t.split_whitespace().next().unwrap_or("");
    matches!(
        first,
        "open" | "on" | "commit" | "rollback" | "close" | "expect"
    )
}

/// Read a statement's SQL body: lines from `*i` up to the next record boundary.
fn take_concurrency_sql(lines: &[&str], i: &mut usize) -> String {
    let mut sql = Vec::new();
    while *i < lines.len() && !is_boundary(lines[*i]) {
        sql.push(lines[*i]);
        *i += 1;
    }
    sql.join("\n")
}

/// Read a query body: SQL up to the `----` separator, then expected rows up to the next boundary.
fn take_concurrency_query(lines: &[&str], i: &mut usize) -> (String, Vec<String>) {
    let mut body = Vec::new();
    while *i < lines.len() {
        if lines[*i].trim() == "----" {
            *i += 1;
            break;
        }
        body.push(lines[*i]);
        *i += 1;
    }
    let mut expected = Vec::new();
    while *i < lines.len() && !is_boundary(lines[*i]) {
        expected.push(lines[*i].trim().to_string());
        *i += 1;
    }
    (body.join("\n"), expected)
}

/// Parse a `# format: concurrency` file into its schedule (the steps both modes execute).
fn parse_schedule(text: &str) -> Result<Vec<Step>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut steps = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        if line.is_empty() || line.starts_with('#') {
            i += 1;
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields[0] {
            "open" => {
                if fields.len() < 3 {
                    return Err(format!("open needs `<sid> read|write [blocks]`: {line:?}"));
                }
                // An optional 4th token is the Layer 2 `blocks` annotation (writer-open, held gate).
                let blocks = match fields.get(3) {
                    None => false,
                    Some(&"blocks") => true,
                    Some(other) => {
                        return Err(format!(
                            "unknown open annotation '{other}' (want `blocks`): {line:?}"
                        ));
                    }
                };
                steps.push(Step::Open {
                    sid: fields[1].to_string(),
                    mode: fields[2].to_string(),
                    blocks,
                });
                i += 1;
            }
            "commit" | "rollback" | "close" => {
                if fields.len() < 2 {
                    return Err(format!("{} needs a session id: {line:?}", fields[0]));
                }
                let sid = fields[1].to_string();
                steps.push(match fields[0] {
                    "commit" => Step::Commit(sid),
                    "rollback" => Step::Rollback(sid),
                    _ => Step::Close(sid),
                });
                i += 1;
            }
            "expect" => {
                if fields.len() < 3 {
                    return Err(format!("expect needs `version|oldest_live <n>`: {line:?}"));
                }
                let n: u64 = fields[2]
                    .parse()
                    .map_err(|_| format!("expect value not a uint: {line:?}"))?;
                steps.push(match fields[1] {
                    "version" => Step::ExpectVersion(n),
                    "oldest_live" => Step::ExpectOldestLive(n),
                    other => {
                        return Err(format!(
                            "unknown expect kind '{other}' (want version|oldest_live)"
                        ));
                    }
                });
                i += 1;
            }
            "on" => {
                if fields.len() < 3 {
                    return Err(format!("on needs `<sid> <record>`: {line:?}"));
                }
                let sid = fields[1].to_string();
                i += 1;
                let record = parse_record(&fields[2..], &lines, &mut i)?;
                steps.push(Step::On { sid, record });
            }
            other => return Err(format!("unknown concurrency directive '{other}'")),
        }
    }
    Ok(steps)
}

/// Parse one `on <sid> <record>` body (the record kind + its SQL/expected rows), advancing `*i`.
fn parse_record(rec: &[&str], lines: &[&str], i: &mut usize) -> Result<Record, String> {
    match rec[0] {
        "statement" => Ok(Record::Statement {
            expect: rec.get(1).copied().unwrap_or("").to_string(),
            code: rec.get(2).copied().unwrap_or("").to_string(),
            sql: take_concurrency_sql(lines, i),
        }),
        "query" => {
            let coltypes = rec.get(1).copied().unwrap_or("").to_string();
            let sortmode = rec.get(2).copied().unwrap_or("nosort").to_string();
            let (sql, expected) = take_concurrency_query(lines, i);
            Ok(Record::Query {
                coltypes,
                sortmode,
                sql,
                expected,
            })
        }
        other => Err(format!("unknown record kind '{other}'")),
    }
}

/// End a session: commit/rollback a write session, close (drop) a read session.
fn end_session(kind: &str, sess: Session) -> Result<(), String> {
    let wrap = |e: jed::EngineError| format!("{}: {}", e.code(), e.message);
    match (kind, sess) {
        ("close", Session::Read(h)) => {
            drop(h); // Session::drop deregisters the read pin, advancing the watermark
            Ok(())
        }
        ("close", Session::Write(_)) => {
            Err("close of a write session (use commit/rollback)".into())
        }
        ("commit", Session::Write(mut h)) => h.commit().map_err(wrap),
        ("commit", Session::Read(_)) => Err("commit of a read session (use close)".into()),
        ("rollback", Session::Write(mut h)) => h.rollback().map_err(wrap),
        ("rollback", Session::Read(_)) => Err("rollback of a read session (use close)".into()),
        _ => Ok(()),
    }
}

/// Run one `# format: concurrency` file in the canonical stepped-SEQUENTIAL mode.
fn run_concurrency_file(text: &str) -> Result<(), String> {
    run_steps_sequential(&parse_schedule(text)?)
}

/// Run one `# format: concurrency` file in the stepped-THREADED mode (a turn token over per-session
/// OS threads). Used by `cargo test` for real concurrent-path coverage under the race detector.
#[cfg(test)]
fn run_concurrency_file_threaded(text: &str) -> Result<(), String> {
    run_steps_threaded(&parse_schedule(text)?)
}

/// Execute a schedule on a single thread: the canonical, deterministic transcript.
///
/// The Layer 2 `blocks` annotation (concurrency-testing.md §5) is modeled here without ever truly
/// blocking: a queued writer-open is NOT run when it is seen (calling `write()` now would block the
/// single thread forever on the held gate), but recorded — and run at the gate-releasing step, the
/// instant the holder commits/rolls back. That is the equivalent serial order, identical to what a
/// threaded run consistent with the schedule must produce. `gate_holder` is the live writer's sid
/// (the single-writer gate), and `blocked` is the at-most-one writer queued on it.
fn run_steps_sequential(steps: &[Step]) -> Result<(), String> {
    let db = Database::create(CreateOptions::default()).unwrap();
    let mut sessions: HashMap<String, Session> = HashMap::new();
    let mut gate_holder: Option<String> = None; // the live writer holding the gate
    let mut blocked: Option<String> = None; // a writer queued on the gate (Layer 2 `blocks`)
    for step in steps {
        match step {
            Step::Open { sid, mode, blocks } => {
                if sessions.contains_key(sid) || blocked.as_deref() == Some(sid.as_str()) {
                    return Err(format!("session '{sid}' already open"));
                }
                match mode.as_str() {
                    "read" => {
                        if *blocks {
                            return Err(format!(
                                "open {sid}: `blocks` is only valid for a write session"
                            ));
                        }
                        sessions.insert(sid.clone(), Session::Read(db.read_session())); // readers never gate
                    }
                    "write" if *blocks => {
                        // Layer 2: assert the gate is held, then QUEUE the open — running `write()`
                        // now would deadlock the single thread. It opens at the releasing step below.
                        if gate_holder.is_none() {
                            return Err(format!(
                                "open {sid} write blocks: the writer gate is free (nothing to block on)"
                            ));
                        }
                        if let Some(b) = &blocked {
                            return Err(format!(
                                "open {sid} write blocks: writer '{b}' is already blocked (one at a time)"
                            ));
                        }
                        blocked = Some(sid.clone());
                    }
                    "write" => {
                        if let Some(h) = &gate_holder {
                            return Err(format!(
                                "open {sid} write: the gate is held by '{h}' — use `blocks`"
                            ));
                        }
                        sessions.insert(sid.clone(), Session::Write(db.write_session()));
                        gate_holder = Some(sid.clone());
                    }
                    other => {
                        return Err(format!("unknown session mode '{other}' (want read|write)"));
                    }
                }
            }
            Step::On { sid, record } => {
                if blocked.as_deref() == Some(sid.as_str()) {
                    return Err(format!("on '{sid}' while it is blocked on the writer gate"));
                }
                let session = sessions
                    .get_mut(sid)
                    .ok_or_else(|| format!("on unknown session '{sid}'"))?;
                run_record(session.handle(), sid, record)?;
            }
            Step::Commit(sid) | Step::Rollback(sid) | Step::Close(sid) => {
                let kind = match step {
                    Step::Commit(_) => "commit",
                    Step::Rollback(_) => "rollback",
                    _ => "close",
                };
                if blocked.as_deref() == Some(sid.as_str()) {
                    return Err(format!(
                        "{kind} of '{sid}' while it is blocked on the writer gate"
                    ));
                }
                let sess = sessions
                    .remove(sid)
                    .ok_or_else(|| format!("{kind} of unknown session '{sid}'"))?;
                end_session(kind, sess).map_err(|e| format!("{kind} {sid}: {e}"))?;
                // If the ended session held the gate, release it — and let the queued writer (if any)
                // acquire it now: it opens (`write()` no longer blocks) capturing the version just
                // published, the equivalent serial order (§5).
                if gate_holder.as_deref() == Some(sid.as_str()) {
                    gate_holder = None;
                    if let Some(b) = blocked.take() {
                        sessions.insert(b.clone(), Session::Write(db.write_session()));
                        gate_holder = Some(b);
                    }
                }
            }
            Step::ExpectVersion(n) => {
                let got = db.version();
                if got != *n {
                    return Err(format!("expect version {n}, got {got}"));
                }
            }
            Step::ExpectOldestLive(n) => {
                let got = db.oldest_live_txid();
                if got != *n {
                    return Err(format!("expect oldest_live {n}, got {got}"));
                }
            }
        }
    }
    if !sessions.is_empty() || blocked.is_some() {
        let mut open: Vec<&str> = sessions.keys().map(String::as_str).collect();
        if let Some(b) = &blocked {
            open.push(b.as_str());
        }
        open.sort_unstable(); // deterministic message; map order must never leak (CLAUDE.md §8)
        return Err(format!(
            "file ended with sessions still open: {}",
            open.join(", ")
        ));
    }
    Ok(())
}

/// A command sent from the driver to a session's worker thread.
#[cfg(test)]
enum Cmd {
    Run(Record),
    End(String), // "commit" | "rollback" | "close"
}

/// A spawned per-session worker: its command channel, its reply channel, and the join handle.
#[cfg(test)]
struct Worker {
    cmd: Sender<Cmd>,
    reply: Receiver<Result<(), String>>,
    handle: JoinHandle<()>,
}

/// A read session's worker thread: pins a snapshot, runs records against it, and on `close` returns
/// (dropping the handle, which deregisters → advances the watermark).
#[cfg(test)]
fn read_worker(db: Database, sid: String, rx: Receiver<Cmd>, tx: Sender<Result<(), String>>) {
    let mut h = db.read_session();
    let _ = tx.send(Ok(())); // ack the open: the snapshot is pinned + registered
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Run(rec) => {
                let _ = tx.send(run_record(&mut h, &sid, &rec));
            }
            Cmd::End(kind) => {
                let r = match kind.as_str() {
                    "close" => Ok(()),
                    other => Err(format!("{other} of a read session (use close)")),
                };
                let _ = tx.send(r);
                return; // h dropped here → deregister
            }
        }
    }
}

/// A write session's worker thread: acquires the writer gate, runs records against the working set,
/// and on `commit`/`rollback` ends the transaction (publishing or discarding) then returns.
#[cfg(test)]
fn write_worker(db: Database, sid: String, rx: Receiver<Cmd>, tx: Sender<Result<(), String>>) {
    let mut h = db.write_session();
    let _ = tx.send(Ok(())); // ack the open: the writer gate is held, working set captured
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Run(rec) => {
                let _ = tx.send(run_record(&mut h, &sid, &rec));
            }
            Cmd::End(kind) => {
                let wrap = |e: jed::EngineError| format!("{}: {}", e.code(), e.message);
                let r = match kind.as_str() {
                    "commit" => h.commit().map_err(wrap),
                    "rollback" => h.rollback().map_err(wrap),
                    other => Err(format!("{other} of a write session (use commit/rollback)")),
                };
                let _ = tx.send(r);
                return; // on the close-error arm, h drops here → rollback + gate release
            }
        }
    }
}

/// The threaded-mode driver state: the live (open-acked) workers, the gate holder, and the
/// at-most-one writer queued on the gate (Layer 2 `blocks`). A blocked writer's thread is parked
/// inside `db.write_session()` on the held gate, so its open ack has NOT been received yet; it is drained at
/// the gate-releasing step, when its `write()` returns and it sends the deferred ack.
#[cfg(test)]
struct Driver {
    db: Database,
    workers: HashMap<String, Worker>,
    gate_holder: Option<String>,
    blocked: Option<(String, Worker)>,
}

#[cfg(test)]
impl Driver {
    fn new() -> Self {
        Driver {
            db: Database::create(CreateOptions::default()).unwrap(),
            workers: HashMap::new(),
            gate_holder: None,
            blocked: None,
        }
    }

    /// Spawn a per-session worker thread. `Database` is `Send + Sync` — proven by moving a clone into
    /// the thread, where the handle is created, used, and dropped (only the shared core crosses over).
    fn spawn(&self, mode: &str, sid: &str) -> Result<Worker, String> {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (rep_tx, rep_rx) = mpsc::channel();
        let dbc = self.db.clone();
        let sidc = sid.to_string();
        let handle = match mode {
            "read" => thread::spawn(move || read_worker(dbc, sidc, cmd_rx, rep_tx)),
            "write" => thread::spawn(move || write_worker(dbc, sidc, cmd_rx, rep_tx)),
            other => return Err(format!("unknown session mode '{other}' (want read|write)")),
        };
        Ok(Worker {
            cmd: cmd_tx,
            reply: rep_rx,
            handle,
        })
    }

    /// Run one schedule step (the turn token: dispatch, wait for the reply, advance).
    fn step(&mut self, step: &Step) -> Result<(), String> {
        match step {
            Step::Open { sid, mode, blocks } => {
                if self.workers.contains_key(sid)
                    || self.blocked.as_ref().is_some_and(|(s, _)| s == sid)
                {
                    return Err(format!("session '{sid}' already open"));
                }
                match mode.as_str() {
                    "read" => {
                        if *blocks {
                            return Err(format!(
                                "open {sid}: `blocks` is only valid for a write session"
                            ));
                        }
                        let w = self.spawn("read", sid)?;
                        self.ack_open(sid, w)
                    }
                    "write" if *blocks => {
                        // Layer 2: validate BEFORE spawning (an unspawned error path leaves nothing
                        // parked on the gate). The gate must be held; at most one writer may block.
                        if self.gate_holder.is_none() {
                            return Err(format!(
                                "open {sid} write blocks: the writer gate is free (nothing to block on)"
                            ));
                        }
                        if let Some((b, _)) = &self.blocked {
                            return Err(format!(
                                "open {sid} write blocks: writer '{b}' is already blocked (one at a time)"
                            ));
                        }
                        let w = self.spawn("write", sid)?;
                        // Do NOT receive the ack: the thread is parked inside `write()` on the held
                        // gate. The ack arrives only when the holder releases it (drained below).
                        self.blocked = Some((sid.clone(), w));
                        Ok(())
                    }
                    "write" => {
                        if let Some(h) = &self.gate_holder {
                            // A non-blocking write-open on a held gate would park the thread in
                            // `write()` and the ack recv would deadlock the driver — reject it.
                            return Err(format!(
                                "open {sid} write: the gate is held by '{h}' — use `blocks`"
                            ));
                        }
                        let w = self.spawn("write", sid)?;
                        self.ack_open(sid, w)?;
                        self.gate_holder = Some(sid.clone());
                        Ok(())
                    }
                    other => Err(format!("unknown session mode '{other}' (want read|write)")),
                }
            }
            Step::On { sid, record } => {
                if self.blocked.as_ref().is_some_and(|(s, _)| s == sid) {
                    return Err(format!("on '{sid}' while it is blocked on the writer gate"));
                }
                let Some(w) = self.workers.get(sid) else {
                    return Err(format!("on unknown session '{sid}'"));
                };
                if w.cmd.send(Cmd::Run(record.clone())).is_err() {
                    return Err(format!("[{sid}] worker died before record"));
                }
                recv_reply(&w.reply)?
            }
            Step::Commit(sid) | Step::Rollback(sid) | Step::Close(sid) => {
                let kind = match step {
                    Step::Commit(_) => "commit",
                    Step::Rollback(_) => "rollback",
                    _ => "close",
                };
                if self.blocked.as_ref().is_some_and(|(s, _)| s == sid) {
                    return Err(format!(
                        "{kind} of '{sid}' while it is blocked on the writer gate"
                    ));
                }
                let releasing = self.gate_holder.as_deref() == Some(sid.as_str());
                if releasing {
                    if let Some((bsid, bw)) = &self.blocked {
                        // Layer 2 real-blocking verification (§5): the queued writer must NOT have
                        // acquired the gate yet — its ack must still be pending while the holder holds.
                        match bw.reply.try_recv() {
                            Err(mpsc::TryRecvError::Empty) => {} // still blocked — good
                            Ok(_) => {
                                return Err(format!(
                                    "blocked writer '{bsid}' acquired the gate before '{sid}' released it"
                                ));
                            }
                            Err(mpsc::TryRecvError::Disconnected) => {
                                return Err(format!(
                                    "blocked writer '{bsid}' worker terminated unexpectedly"
                                ));
                            }
                        }
                    }
                }
                let Some(w) = self.workers.remove(sid) else {
                    return Err(format!("{kind} of unknown session '{sid}'"));
                };
                let _ = w.cmd.send(Cmd::End(kind.to_string()));
                let reply = recv_reply(&w.reply);
                // Join AFTER the reply so the handle's Drop (deregister / gate release) has run before
                // the next step reads the watermark — the join is the happens-before edge.
                let _ = w.handle.join();
                let end = match reply {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(format!("{kind} {sid}: {e}")),
                    Err(e) => Err(format!("{kind} {sid}: {e}")),
                };
                if releasing {
                    self.gate_holder = None;
                    if end.is_ok() {
                        if let Some((bsid, bw)) = self.blocked.take() {
                            // The gate is free now: the queued writer's parked `write()` returns and
                            // its thread sends the deferred ack. Receive it (the open logically
                            // completes), promoting it to the live gate holder — capturing the version
                            // the holder just published (§5).
                            match recv_reply(&bw.reply) {
                                Ok(Ok(())) => {
                                    self.workers.insert(bsid.clone(), bw);
                                    self.gate_holder = Some(bsid);
                                }
                                Ok(Err(e)) => {
                                    let _ = bw.handle.join();
                                    return Err(format!("open {bsid}: {e}"));
                                }
                                Err(e) => {
                                    let _ = bw.handle.join();
                                    return Err(format!("open {bsid}: {e}"));
                                }
                            }
                        }
                    }
                }
                end
            }
            Step::ExpectVersion(n) => {
                let got = self.db.version();
                if got != *n {
                    return Err(format!("expect version {n}, got {got}"));
                }
                Ok(())
            }
            Step::ExpectOldestLive(n) => {
                let got = self.db.oldest_live_txid();
                if got != *n {
                    return Err(format!("expect oldest_live {n}, got {got}"));
                }
                Ok(())
            }
        }
    }

    /// The turn token for a non-blocking open: wait for the open ack, registering the worker on
    /// success and joining the (failed) thread on error.
    fn ack_open(&mut self, sid: &str, w: Worker) -> Result<(), String> {
        match recv_reply(&w.reply) {
            Ok(Ok(())) => {
                self.workers.insert(sid.to_string(), w);
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = w.handle.join();
                Err(format!("open {sid}: {e}"))
            }
            Err(e) => {
                let _ = w.handle.join();
                Err(format!("open {sid}: {e}"))
            }
        }
    }

    /// End every still-open worker and return their ids (sorted, for a deterministic leftover
    /// message). Live workers go FIRST — dropping a command sender ends the loop, and a never-ended
    /// write handle's `Drop` releases the gate — so a still-parked blocked writer then unblocks: with
    /// the gate freed its `write()` returns, the thread reaches the (now-closed) command loop, and its
    /// handle drops → rollback. The deferred ack sits unread in the unbounded channel and is discarded
    /// on drop (no rendezvous needed — unlike Go's unbuffered channels, an mpsc send never blocks).
    fn teardown(&mut self) -> Vec<String> {
        let mut leftover: Vec<String> = self.workers.keys().cloned().collect();
        let blocked = self.blocked.take().map(|(sid, w)| {
            leftover.push(sid);
            w
        });
        for (_, w) in self.workers.drain() {
            drop(w.cmd);
            drop(w.reply);
            let _ = w.handle.join();
        }
        // The blocked writer last: live workers are down, so the gate is free and its parked write()
        // has returned. Dropping its command sender ends the loop → its handle rolls back and exits.
        if let Some(w) = blocked {
            drop(w.cmd);
            drop(w.reply);
            let _ = w.handle.join();
        }
        leftover.sort_unstable(); // deterministic message; map order must never leak (CLAUDE.md §8)
        leftover
    }
}

/// Execute a schedule with one OS thread per session, the listed order enforced by a turn token:
/// the driver sends a command and waits for the worker's reply (and, for an end step, joins the
/// thread) before advancing — so exactly one session runs at a time, in the listed order, yet every
/// operation runs on a real thread against the shared handle (race-detector / TSan coverage). The
/// canonical result is identical to the sequential mode (concurrency-testing.md §2). The Layer 2
/// `blocks` annotation additionally drives the REAL blocking acquire: the queued writer's thread
/// stays parked inside `write()` on the held gate (its open ack deferred) until the holder releases
/// it, the one concurrency path the sequential walk never exercises (§5).
#[cfg(test)]
fn run_steps_threaded(steps: &[Step]) -> Result<(), String> {
    let mut d = Driver::new();
    let mut result: Result<(), String> = Ok(());
    for step in steps {
        if let Err(e) = d.step(step) {
            result = Err(e);
            break;
        }
    }
    let still_open = d.teardown();
    if result.is_ok() && !still_open.is_empty() {
        return Err(format!(
            "file ended with sessions still open: {}",
            still_open.join(", ")
        ));
    }
    result
}

/// Receive one reply from a worker, mapping a closed channel (a panicked worker) to an error.
#[cfg(test)]
fn recv_reply(rx: &Receiver<Result<(), String>>) -> Result<Result<(), String>, String> {
    rx.recv()
        .map_err(|_| "worker thread terminated unexpectedly".to_string())
}

#[cfg(test)]
mod concurrency_threaded_tests {
    //! Run every `# format: concurrency` suite file in the stepped-THREADED mode (§4.3): one OS
    //! thread per session, the schedule order enforced by a turn token. The point is `cargo test`
    //! under the race detector / TSan — real concurrent-path coverage of Database that the
    //! single-threaded sequential walk cannot give. The asserted result is identical to sequential
    //! (the schedule is timing-free, §2), so a divergence here is a genuine concurrency bug.

    use super::*;

    #[test]
    fn schedules_run_threaded() {
        let suites = suites_dir();
        let mut files = Vec::new();
        collect_tests(&suites, &mut files);
        files.sort();
        let supported: BTreeSet<&str> = SUPPORTED_CAPABILITIES.iter().copied().collect();

        let mut ran = 0;
        for file in &files {
            let text = std::fs::read_to_string(file).expect("read .test file");
            if !is_concurrency_format(&text) {
                continue;
            }
            // Honor the same capability gate as the binary — skip a file needing a cap we lack.
            if parse_requires(&text)
                .iter()
                .any(|c| !supported.contains(c.as_str()))
            {
                continue;
            }
            run_concurrency_file_threaded(&text)
                .unwrap_or_else(|e| panic!("threaded {}: {e}", file.display()));
            ran += 1;
        }
        assert!(ran > 0, "no runnable concurrency files found");
    }

    /// A schedule left with a live holder AND a queued (blocked) writer must tear down without
    /// hanging, reporting BOTH as still open. The Layer 2 teardown path the suite `.test` files never
    /// reach (they always end every session): tearing down the holder releases the gate, so the
    /// parked writer's `write()` returns and its thread can be joined (§5).
    #[test]
    fn teardown_unblocks_a_leftover_blocked_writer() {
        let steps = vec![
            Step::Open {
                sid: "w1".into(),
                mode: "write".into(),
                blocks: false,
            },
            Step::Open {
                sid: "w2".into(),
                mode: "write".into(),
                blocks: true,
            },
        ];
        let err =
            run_steps_threaded(&steps).expect_err("a leftover holder + blocked writer must error");
        assert!(
            err.contains("w1") && err.contains("w2"),
            "want a leftover error naming w1 and w2, got: {err}"
        );
    }
}
