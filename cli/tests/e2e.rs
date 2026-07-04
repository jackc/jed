//! End-to-end golden tests for script mode (spec/design/cli.md §7). Deterministic by
//! construction: the engine is deterministic, cost footers are exact, wall-clock never
//! prints, there is no banner on piped stdin, and every golden query uses ORDER BY
//! (unordered row order is spec-unspecified). Cargo builds the binary for integration
//! tests and exposes it as CARGO_BIN_EXE_jed.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

struct Run {
    stdout: String,
    stderr: String,
    code: i32,
}

fn run(args: &[&str], stdin_text: &str) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_jed"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jed");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_text.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait jed");
    Run {
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
        code: out.status.code().unwrap(),
    }
}

fn testdata(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/testdata")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("jed_e2e_{}_{name}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn basic_aligned_session_matches_golden() {
    let r = run(&[], &testdata("basic.sql"));
    assert_eq!(r.stderr, "", "stderr should be empty");
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, testdata("basic.golden"));
}

#[test]
fn csv_format_matches_golden() {
    let r = run(&["--format", "csv"], &testdata("formats.sql"));
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert_eq!(r.stdout, testdata("formats_csv.golden"));
}

#[test]
fn json_format_quiet_matches_golden() {
    // -q drops the OK lines, leaving pure data (the json golden was made with -q).
    let r = run(&["--format", "json", "-q"], &testdata("formats.sql"));
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert_eq!(r.stdout, testdata("formats_json.golden"));
}

#[test]
fn script_stops_at_the_first_error_with_exit_2() {
    let r = run(&[], &testdata("errors.sql"));
    assert_eq!(r.code, 2);
    assert_eq!(r.stdout, "OK (cost 0)\nOK, 1 row (cost 0)\n");
    assert_eq!(
        r.stderr,
        "<stdin>:3: ERROR 23505: duplicate key value violates unique constraint: t_pkey\n"
    );
}

#[test]
fn continue_on_error_runs_the_rest_but_still_exits_2() {
    let r = run(&["--continue-on-error"], &testdata("errors.sql"));
    assert_eq!(r.code, 2);
    assert!(r.stdout.contains("(1 row, cost 3)"), "stdout: {}", r.stdout);
}

#[test]
fn framing_errors_reject_the_whole_input() {
    let r = run(&[], "SELECT 'unterminated");
    assert_eq!(r.code, 2);
    assert_eq!(
        r.stderr,
        "<stdin>:1: ERROR 42601: unterminated string literal\n"
    );
    assert_eq!(r.stdout, "");
}

#[test]
fn cost_ceiling_aborts_with_54p01_and_a_hint() {
    let r = run(
        &[
            "--max-cost",
            "2",
            "-c",
            "CREATE TABLE t (a i32 PRIMARY KEY)",
        ],
        "",
    );
    // DDL costs 0 — it survives; a scan does not.
    assert_eq!(r.code, 0);
    let db = tmp("ceiling.jed");
    let db_str = db.to_str().unwrap();
    let r = run(
        &[
            "--create",
            db_str,
            "-c",
            "CREATE TABLE t (a i32 PRIMARY KEY); INSERT INTO t VALUES (1)",
        ],
        "",
    );
    assert_eq!(r.code, 0);
    // A 1-row scan accrues page_read + row reads — past a ceiling of 2.
    let r = run(&[db_str, "--max-cost", "2", "-c", "SELECT a FROM t"], "");
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("ERROR 54P01:"), "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("hint: raise the ceiling with --max-cost"),
        "stderr: {}",
        r.stderr
    );
    let _ = std::fs::remove_file(&db);
}

#[test]
fn create_then_reopen_round_trips() {
    let db = tmp("roundtrip.jed");
    let db_str = db.to_str().unwrap();
    let r = run(
        &[
            "--create",
            db_str,
            "-c",
            "CREATE TABLE t (a i32 PRIMARY KEY); INSERT INTO t VALUES (7)",
        ],
        "",
    );
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    let r = run(&[db_str, "-q", "-c", "SELECT a FROM t"], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert_eq!(r.stdout, " a\n---\n 7\n(1 row, cost 3)\n");
    // Creating over an existing file is 58P02, exit 1 (strict create — cli.md §3).
    let r = run(&["--create", db_str, "-c", "SELECT a FROM t"], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("ERROR 58P02:"), "stderr: {}", r.stderr);
    let _ = std::fs::remove_file(&db);
}

#[test]
fn missing_file_exits_1_with_a_create_hint() {
    let r = run(&["/nonexistent/nope.jed", "-c", "SELECT 1"], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("ERROR 58P01:"), "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("hint: pass --create"),
        "stderr: {}",
        r.stderr
    );
}

#[test]
fn usage_errors_exit_1() {
    let r = run(&["--nope"], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("unknown flag"), "stderr: {}", r.stderr);
}

#[test]
fn sources_run_in_command_line_order() {
    let r = run(
        &[
            "-c",
            "CREATE TABLE t (a i32 PRIMARY KEY)",
            "-c",
            "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)",
            "-q",
            "-c",
            "SELECT a FROM t ORDER BY a",
        ],
        "",
    );
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert_eq!(r.stdout, " a\n---\n 1\n 2\n(2 rows, cost 5)\n");
}

#[test]
fn readonly_serves_reads_and_rejects_writes() {
    let db = tmp("readonly.jed");
    let db_str = db.to_str().unwrap();
    let r = run(
        &[
            "--create",
            db_str,
            "-c",
            "CREATE TABLE t (a i32 PRIMARY KEY); INSERT INTO t VALUES (1)",
        ],
        "",
    );
    assert_eq!(r.code, 0);

    let r = run(&["--readonly", db_str, "-c", "SELECT a FROM t"], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert!(r.stdout.contains("(1 row, cost"), "stdout: {}", r.stdout);

    let r = run(
        &["--readonly", db_str, "-c", "INSERT INTO t VALUES (2)"],
        "",
    );
    assert_eq!(r.code, 2);
    assert!(
        r.stderr
            .contains("ERROR 25006: cannot execute INSERT in a read-only transaction"),
        "stderr: {}",
        r.stderr
    );

    // --readonly is strict about its shape.
    let r = run(&["--readonly"], "");
    assert_eq!(r.code, 1);
    let r = run(&["--readonly", "--create", db_str], "");
    assert_eq!(r.code, 1);
    let _ = std::fs::remove_file(&db);
}

#[test]
fn box_format_quiet_matches_golden() {
    let r = run(&["--format", "box", "-q"], &testdata("formats.sql"));
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert_eq!(r.stdout, testdata("formats_box.golden"));
}

#[test]
fn markdown_format_quiet_matches_golden() {
    let r = run(&["--format", "markdown", "-q"], &testdata("formats.sql"));
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert_eq!(r.stdout, testdata("formats_markdown.golden"));
}

#[test]
fn output_redirection_writes_results_to_the_file() {
    let out_path = tmp("out.txt");
    let out_str = out_path.to_str().unwrap();
    let r = run(
        &[
            "-o",
            out_str,
            "-c",
            "CREATE TABLE t (a i32 PRIMARY KEY); INSERT INTO t VALUES (7); SELECT a FROM t",
        ],
        "",
    );
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert_eq!(r.stdout, "", "stdout must be empty under -o");
    let written = std::fs::read_to_string(&out_path).unwrap();
    assert_eq!(
        written,
        "OK (cost 0)\nOK, 1 row (cost 0)\n a\n---\n 7\n(1 row, cost 3)\n"
    );
    let _ = std::fs::remove_file(&out_path);

    // `-o -` keeps stdout; errors stay on stderr either way.
    let r = run(&["-o", "-", "-q", "-c", "SELECT 1"], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert_eq!(
        r.stdout,
        " ?column?\n----------\n        1\n(1 row, cost 1)\n"
    );

    // An unwritable destination is a startup error, exit 1.
    let r = run(&["-o", "/nonexistent/dir/out.txt", "-c", "SELECT 1"], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("/nonexistent/dir/out.txt"));
}

#[test]
fn import_csv_inserts_atomically_in_command_line_order() {
    let csv_path = tmp("people.csv");
    std::fs::write(
        &csv_path,
        "name,id,note\nalice,1,\"says \"\"hi\"\"\"\nbob,2,\n",
    )
    .unwrap();
    let csv_str = csv_path.to_str().unwrap();

    // Create the table with -c, then import — sources run in command-line order. The
    // header maps by NAME (here deliberately not in declaration order); the column the
    // CSV omits (ok) takes its default; a bare empty field imports as NULL.
    let r = run(
        &[
            "-c",
            "CREATE TABLE p (id i32 PRIMARY KEY, name text, note text, ok boolean DEFAULT true)",
            "--import-csv",
            &format!("p={csv_str}"),
            "-c",
            "SELECT id, name, note, ok FROM p ORDER BY id",
        ],
        "",
    );
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert!(
        r.stdout.contains("OK, 2 rows (cost 0)"),
        "stdout: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains(" 1 | alice | says \"hi\" | true"),
        "stdout: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains(" 2 | bob   | NULL"),
        "stdout: {}",
        r.stdout
    );

    // A bad row aborts the WHOLE import (one atomic INSERT): nothing lands.
    let bad_path = tmp("bad.csv");
    std::fs::write(&bad_path, "id,name\nseven,x\n").unwrap();
    let bad_str = bad_path.to_str().unwrap();
    let r = run(
        &[
            "-c",
            "CREATE TABLE q (id i32 PRIMARY KEY, name text)",
            "--import-csv",
            &format!("q={bad_str}"),
        ],
        "",
    );
    assert_eq!(r.code, 2);
    assert!(
        r.stderr.contains("row 1, column id"),
        "stderr: {}",
        r.stderr
    );

    // An unknown table reports cleanly; a malformed spec is a usage error.
    let r = run(&["--import-csv", &format!("nope={csv_str}")], "");
    assert_eq!(r.code, 2);
    assert!(
        r.stderr.contains("table does not exist: nope"),
        "stderr: {}",
        r.stderr
    );
    let r = run(&["--import-csv", "no-equals"], "");
    assert_eq!(r.code, 1);

    let _ = std::fs::remove_file(&csv_path);
    let _ = std::fs::remove_file(&bad_path);
}

#[test]
fn csv_export_then_import_round_trips() {
    // --format csv -o is the export half; --import-csv reads the same dialect back,
    // including the quoted-empty ('') vs bare-empty (NULL) distinction.
    let db = tmp("roundtrip_csv.jed");
    let db_str = db.to_str().unwrap();
    let exported = tmp("export.csv");
    let exported_str = exported.to_str().unwrap();

    let r = run(
        &[
            "--create",
            db_str,
            "-c",
            "CREATE TABLE t (id i32 PRIMARY KEY, name text); \
             INSERT INTO t VALUES (1, 'a,b'), (2, NULL), (3, ''); \
             CREATE TABLE back (id i32 PRIMARY KEY, name text)",
        ],
        "",
    );
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    let r = run(
        &[
            db_str,
            "--format",
            "csv",
            "-q",
            "-o",
            exported_str,
            "-c",
            "SELECT id, name FROM t ORDER BY id",
        ],
        "",
    );
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    let r = run(
        &[
            db_str,
            "-q",
            "--import-csv",
            &format!("back={exported_str}"),
            "-c",
            "SELECT count(*) FROM back; SELECT id FROM back WHERE name IS NULL",
        ],
        "",
    );
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert!(r.stdout.contains(" 3\n"), "stdout: {}", r.stdout);
    // v1 caveat (cli.md §5): csv export writes NULL as an empty UNQUOTED field, and ''
    // as a quoted empty — both NULL and '' survive the round trip distinctly only when
    // the writer quotes ''. Today it does not, so '' comes back as NULL (accepted).
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(&exported);
}

#[test]
fn dump_replays_into_an_identical_database() {
    let db = tmp("dump_src.jed");
    let db_str = db.to_str().unwrap();
    let r = run(
        &[
            "--create",
            db_str,
            "-c",
            "CREATE TABLE t (id i32 PRIMARY KEY, name text, score numeric(5,2) DEFAULT 1.00); \
             INSERT INTO t (id, name) VALUES (2, 'it''s b'), (1, NULL); \
             CREATE TABLE u (a i64, b i32 UNIQUE); INSERT INTO u VALUES (5, 6)",
        ],
        "",
    );
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));

    // Dump composes with --readonly (the natural pairing) and -o.
    let dump_path = tmp("dump.sql");
    let dump_str = dump_path.to_str().unwrap();
    let r = run(&["--readonly", db_str, "--dump", "-o", dump_str], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    let first = std::fs::read_to_string(&dump_path).unwrap();
    assert!(first.starts_with("BEGIN;\n"), "dump: {first}");
    assert!(first.contains("PRIMARY KEY (id)"), "dump: {first}");
    assert!(
        first.contains("CREATE UNIQUE INDEX u_b_key ON u (b);"),
        "dump: {first}"
    );

    // Replay into a fresh database; its dump is byte-identical.
    let replayed = tmp("dump_replay.jed");
    let replayed_str = replayed.to_str().unwrap();
    let r = run(&["--create", replayed_str, "-q", "-f", dump_str], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    let r = run(&[replayed_str, "--dump"], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert_eq!(r.stdout, first);

    // --dump is exclusive of SQL sources and needs a DBFILE.
    let r = run(&[db_str, "--dump", "-c", "SELECT 1"], "");
    assert_eq!(r.code, 1);
    let r = run(&["--dump"], "");
    assert_eq!(r.code, 1);

    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(&dump_path);
    let _ = std::fs::remove_file(&replayed);
}

// ───────────────────────────── `jed migrate` subcommand ─────────────────────────────

/// The shared migrate corpus directory (../migrate/testdata/<sub>), used read-only.
fn migrate_testdata(sub: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../migrate/testdata")
        .join(sub)
        .to_str()
        .unwrap()
        .to_string()
}

fn tmp_dir(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("jed_e2e_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

#[test]
fn migrate_applies_up_then_status_then_down() {
    let db = tmp("migrate.jed");
    let db_str = db.to_str().unwrap();
    let migs = migrate_testdata("blog");

    // Apply to last on a fresh (nonexistent) file — it is created and every migration runs.
    let r = run(&["migrate", "-m", &migs, db_str], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""), "apply: {}", r.stderr);
    assert!(r.stdout.contains("up    001_create_users"), "{}", r.stdout);
    assert!(r.stdout.contains("done — now at version 3"), "{}", r.stdout);

    // status reports the high-water mark.
    let r = run(&["migrate", "status", "-m", &migs, db_str], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert!(r.stdout.contains("version:  3 of 3"), "{}", r.stdout);
    assert!(r.stdout.contains("[x] 003_add_email_index"), "{}", r.stdout);

    // Down to 1 (via an absolute destination), then a re-run is a no-op.
    let r = run(&["migrate", "-d", "1", "-m", &migs, db_str], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert!(
        r.stdout.contains("down  003_add_email_index"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("now at version 1"), "{}", r.stdout);

    let r = run(&["migrate", "-d", "1", "-m", &migs, db_str], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert!(r.stdout.contains("nothing to do"), "{}", r.stdout);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn migrate_irreversible_down_exits_2() {
    let db = tmp("migrate_irr.jed");
    let db_str = db.to_str().unwrap();
    let migs = migrate_testdata("irreversible");

    let r = run(&["migrate", "-m", &migs, db_str], "");
    assert_eq!(r.code, 0, "{}", r.stderr);

    // Migrating down through the irreversible 002 fails with exit code 2.
    let r = run(&["migrate", "-d", "0", "-m", &migs, db_str], "");
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("irreversible"), "{}", r.stderr);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn migrate_failing_statement_exits_2() {
    let db = tmp("migrate_txc.jed");
    let db_str = db.to_str().unwrap();
    let migs = migrate_testdata("tx_control");

    let r = run(&["migrate", "-m", &migs, db_str], "");
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("0A000"), "{}", r.stderr);
    assert!(r.stderr.contains("in statement:"), "{}", r.stderr);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn migrate_bad_target_exits_1() {
    let db = tmp("migrate_bad.jed");
    let db_str = db.to_str().unwrap();
    let migs = migrate_testdata("blog");
    let r = run(&["migrate", "-d", "9", "-m", &migs, db_str], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("out of range"), "{}", r.stderr);
    let _ = std::fs::remove_file(&db);
}

#[test]
fn migrate_status_on_missing_db_exits_1() {
    let db = tmp("migrate_missing.jed");
    let r = run(
        &[
            "migrate",
            "status",
            "-m",
            &migrate_testdata("blog"),
            db.to_str().unwrap(),
        ],
        "",
    );
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("does not exist"), "{}", r.stderr);
}

#[test]
fn migrate_new_scaffolds_next_sequence() {
    let dir = tmp_dir("migrate_new");
    let dir_str = dir.to_str().unwrap();

    let r = run(&["migrate", "new", "create_users", "-m", dir_str], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert!(r.stdout.contains("001_create_users.sql"), "{}", r.stdout);

    let r = run(&["migrate", "new", "add_posts", "-m", dir_str], "");
    assert_eq!(r.code, 0);
    assert!(r.stdout.contains("002_add_posts.sql"), "{}", r.stdout);

    let created = std::fs::read_to_string(dir.join("001_create_users.sql")).unwrap();
    assert!(created.contains("---- create above / drop below ----"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn migrate_new_without_name_exits_1() {
    let r = run(&["migrate", "new"], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("needs a NAME"), "{}", r.stderr);
}

#[test]
fn a_dbfile_is_not_shadowed_by_the_migrate_verb() {
    // The reserved word is only the *first* token; a normal invocation is untouched.
    let r = run(&["-c", "SELECT 1 AS x"], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert!(r.stdout.contains("x"), "{}", r.stdout);
}
