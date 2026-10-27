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
    remove_db(&p);
    p
}

/// Remove a temp database file and the `<path>.lock/` coordination bundle opening it created beside
/// it (spec/design/locking.md) — removing only the file leaks one directory per run.
fn remove_db(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let mut bundle = path.as_os_str().to_owned();
    bundle.push(".lock");
    let _ = std::fs::remove_dir_all(bundle);
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
        "<stdin>:3: ERROR 23505: duplicate key value violates unique constraint: t_pkey\n\
         DETAIL:  Key (id)=(1) already exists.\n"
    );
}

#[test]
fn a_non_main_database_follows_the_detail() {
    // DatabaseName (spec/design/cli.md §5, error-fields.md §12): printed for `temp` / an
    // attachment, not for `main`.
    let r = run(
        &[],
        "CREATE TEMP TABLE t (id i32 PRIMARY KEY);\nINSERT INTO t VALUES (1);\nINSERT INTO t VALUES (1);\n",
    );
    assert_eq!(r.code, 2);
    assert_eq!(
        r.stderr,
        "<stdin>:3: ERROR 23505: duplicate key value violates unique constraint: t_pkey\n\
         DETAIL:  Key (id)=(1) already exists.\n\
         DATABASE:  temp\n"
    );
}

#[test]
fn error_cursor_and_hint_follow_the_error_line() {
    // psql's `LINE n:` cursor (line within the statement) and the structured Hint
    // (spec/design/cli.md §5, error-fields.md §10/§11).
    let r = run(
        &[],
        "CREATE TABLE t (name i32);\nSELECT 1,\n  nme FROM t;\n",
    );
    assert_eq!(r.code, 2);
    assert_eq!(
        r.stderr,
        "<stdin>:2: ERROR 42703: column does not exist: nme\n\
         LINE 2:   nme FROM t\n\
         \x20         ^\n\
         HINT:  Perhaps you meant to reference the column \"t.name\".\n"
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
    remove_db(&db);
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
    remove_db(&db);
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
    remove_db(&db);
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
    remove_db(&db);
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

    remove_db(&db);
    let _ = std::fs::remove_file(&dump_path);
    remove_db(&replayed);
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

    remove_db(&db);
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

    remove_db(&db);
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

    remove_db(&db);
}

#[test]
fn migrate_bad_target_exits_1() {
    let db = tmp("migrate_bad.jed");
    let db_str = db.to_str().unwrap();
    let migs = migrate_testdata("blog");
    let r = run(&["migrate", "-d", "9", "-m", &migs, db_str], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("out of range"), "{}", r.stderr);
    remove_db(&db);
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
fn migrate_set_version_records_without_running() {
    let db = tmp("migrate_setv.jed");
    let db_str = db.to_str().unwrap();
    let migs = migrate_testdata("blog");

    // set-version never creates the database: adopting is for an existing one.
    let r = run(&["migrate", "set-version", "1", "-m", &migs, db_str], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("does not exist"), "{}", r.stderr);

    let r = run(&["migrate", "-d", "1", "-m", &migs, db_str], "");
    assert_eq!(r.code, 0, "{}", r.stderr);

    // Baseline at 3: the version moves, no migration runs.
    let r = run(&["migrate", "set-version", "3", "-m", &migs, db_str], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert!(r.stdout.contains("version set to 3"), "{}", r.stdout);
    assert!(!r.stdout.contains("up "), "{}", r.stdout);
    let r = run(&["migrate", "status", "-m", &migs, db_str], "");
    assert!(r.stdout.contains("version:  3 of 3"), "{}", r.stdout);
    let r = run(
        &["-c", "SELECT name FROM jed_tables ORDER BY name", db_str],
        "",
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("users"), "{}", r.stdout);
    assert!(
        !r.stdout.contains("posts"),
        "002 must not have run: {}",
        r.stdout
    );

    // Out of range is a BadVersion (exit 1); a non-integer VERSION is a usage error (exit 1).
    let r = run(&["migrate", "set-version", "9", "-m", &migs, db_str], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("out of range"), "{}", r.stderr);
    let r = run(&["migrate", "set-version", "last", "-m", &migs, db_str], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("invalid VERSION"), "{}", r.stderr);

    remove_db(&db);
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

/// Write each `(file name, contents)` into `dir`.
fn write_files(dir: &std::path::Path, files: &[(&str, &str)]) {
    for (name, contents) in files {
        std::fs::write(dir.join(name), contents).unwrap();
    }
}

/// The migration-looking file names in `dir`, sorted.
fn sql_files(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".sql"))
        .collect();
    names.sort();
    names
}

#[test]
fn migrate_renumber_moves_merged_migrations_after_the_recorded_set() {
    // tern's TestRenumber: start on 001–003, merge in 002–004, finish renumbers them 004–006.
    let dir = tmp_dir("migrate_renumber");
    std::fs::create_dir_all(&dir).unwrap();
    let dir_str = dir.to_str().unwrap();
    write_files(
        &dir,
        &[("001_a.sql", "a"), ("002_b.sql", "b"), ("003_c.sql", "c")],
    );

    let r = run(&["migrate", "renumber", "start", "-m", dir_str], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert!(r.stdout.contains("recorded 3 migrations"), "{}", r.stdout);
    let state = std::fs::read_to_string(dir.join(".jed-renumber.tmp")).unwrap();
    assert_eq!(state, "001_a.sql\n002_b.sql\n003_c.sql\n");

    write_files(
        &dir,
        &[("002_d.sql", "d"), ("003_e.sql", "e"), ("004_f.sql", "f")],
    );
    let r = run(&["migrate", "renumber", "finish", "-m", dir_str], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert!(
        r.stdout.contains("renamed 002_d.sql -> 004_d.sql"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("renumbered 3 migrations"), "{}", r.stdout);
    assert_eq!(
        sql_files(&dir),
        [
            "001_a.sql",
            "002_b.sql",
            "003_c.sql",
            "004_d.sql",
            "005_e.sql",
            "006_f.sql"
        ]
    );
    assert_eq!(std::fs::read_to_string(dir.join("004_d.sql")).unwrap(), "d");
    assert!(!dir.join(".jed-renumber.tmp").exists());

    // The state file is gone, so a second finish has nothing to work from.
    let r = run(&["migrate", "renumber", "finish", "-m", dir_str], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("renumber start"), "{}", r.stderr);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn migrate_renumber_never_overwrites_a_pending_migration() {
    // 001_t.sql → 002_t.sql would land on the not-yet-moved 002_t.sql: it moves first.
    let dir = tmp_dir("migrate_renumber_chain");
    std::fs::create_dir_all(&dir).unwrap();
    let dir_str = dir.to_str().unwrap();
    write_files(&dir, &[("001_base.sql", "base")]);
    let r = run(&["migrate", "renumber", "start", "-m", dir_str], "");
    assert_eq!(r.code, 0, "{}", r.stderr);

    write_files(&dir, &[("001_t.sql", "first"), ("002_t.sql", "second")]);
    let r = run(&["migrate", "renumber", "finish", "-m", dir_str], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert_eq!(sql_files(&dir), ["001_base.sql", "002_t.sql", "003_t.sql"]);
    assert_eq!(
        std::fs::read_to_string(dir.join("002_t.sql")).unwrap(),
        "first"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("003_t.sql")).unwrap(),
        "second"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn migrate_renumber_errors_exit_1() {
    let dir = tmp_dir("migrate_renumber_err");
    std::fs::create_dir_all(&dir).unwrap();
    let dir_str = dir.to_str().unwrap();

    // finish before start: no state file.
    let r = run(&["migrate", "renumber", "finish", "-m", dir_str], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains(".jed-renumber.tmp"), "{}", r.stderr);

    // start refuses a set that is not 1 … N (a gap, then a duplicate), writing no state.
    write_files(&dir, &[("001_a.sql", "a"), ("003_c.sql", "c")]);
    let r = run(&["migrate", "renumber", "start", "-m", dir_str], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("non-contiguous"), "{}", r.stderr);
    write_files(&dir, &[("001_b.sql", "b")]);
    let r = run(&["migrate", "renumber", "start", "-m", dir_str], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("duplicate sequence 1"), "{}", r.stderr);
    assert!(!dir.join(".jed-renumber.tmp").exists());

    // A missing or unknown action is a usage error.
    let r = run(&["migrate", "renumber", "-m", dir_str], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("needs start or finish"), "{}", r.stderr);
    let r = run(&["migrate", "renumber", "begin", "-m", dir_str], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("unknown renumber action"), "{}", r.stderr);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_dbfile_is_not_shadowed_by_the_migrate_verb() {
    // The reserved word is only the *first* token; a normal invocation is untouched.
    let r = run(&["-c", "SELECT 1 AS x"], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert!(r.stdout.contains("x"), "{}", r.stdout);
}

/// A directory holding a fake `ssh` that runs its remote command locally (with this build's `jed`
/// first on `PATH`) and logs the destination and command, so `jed ssh://…` is tested end to end
/// without reaching any host.
#[cfg(unix)]
fn fake_ssh_dir(name: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("jed_e2e_{}_{name}_bin", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_jed"), dir.join("jed")).unwrap();
    let ssh = dir.join("ssh");
    std::fs::write(
        &ssh,
        "#!/bin/sh\n\
         # Fake ssh: skip options up to --, log the destination and command, run it locally.\n\
         while [ \"$1\" != \"--\" ]; do shift; done\n\
         shift\n\
         printf '%s|%s\\n' \"$1\" \"$2\" >> \"$(dirname \"$0\")/log\"\n\
         shift\n\
         exec sh -c \"$*\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

#[cfg(unix)]
fn run_with_path(bin: &std::path::Path, args: &[&str]) -> Run {
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_jed"))
        .args(args)
        .env("PATH", path)
        .stdin(Stdio::null())
        .output()
        .expect("spawn jed");
    Run {
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
        code: out.status.code().unwrap(),
    }
}

#[cfg(unix)]
#[test]
fn ssh_url_opens_a_remote_database_through_jed_pages() {
    let bin = fake_ssh_dir("ssh");
    // A space and a quote in the path: the remote command must quote it for the remote shell.
    let db = bin.join("my db's.jed");
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
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    let url = format!("ssh://app@dbhost:2222{db_str}");

    let r = run_with_path(
        &bin,
        &[
            &url,
            "-q",
            "-c",
            "INSERT INTO t VALUES (2)",
            "-c",
            "SELECT a FROM t ORDER BY a",
        ],
    );
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert_eq!(r.stdout, " a\n---\n 1\n 2\n(2 rows, cost 5)\n");
    let log = std::fs::read_to_string(bin.join("log")).unwrap();
    let first = log.lines().next().unwrap();
    assert_eq!(
        first,
        format!("app@dbhost|jed pages '{}'", db_str.replace('\'', r"'\''"))
    );

    // --readonly over ssh: reads work, writes fail 25006.
    let r = run_with_path(
        &bin,
        &["--readonly", &url, "-c", "INSERT INTO t VALUES (3)"],
    );
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("ERROR 25006"), "stderr: {}", r.stderr);

    // The commit landed in the file.
    let r = run(&[db_str, "-q", "-c", "SELECT count(*) AS n FROM t"], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert!(r.stdout.contains(" 2\n"), "stdout: {}", r.stdout);

    // A missing remote file is the remote's 58P01.
    let missing = format!("ssh://dbhost{}", bin.join("missing.jed").display());
    let r = run_with_path(&bin, &[&missing, "-c", "SELECT 1"]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("ERROR 58P01"), "stderr: {}", r.stderr);

    // --create and a malformed URL are usage errors.
    let r = run_with_path(&bin, &["--create", &url]);
    assert_eq!(r.code, 1);
    let r = run_with_path(&bin, &["ssh://dbhost", "-c", "SELECT 1"]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("bad ssh:// URL"), "stderr: {}", r.stderr);

    // An encrypted database needs no key on the server: the client's --key-file credential travels
    // in its hello, and the jed pages that owns the file opens it with that (README.md §5.3).
    let enc = bin.join("enc.jed");
    let enc_str = enc.to_str().unwrap();
    let key = bin.join("enc.key");
    std::fs::write(&key, format!("{}\n", "4a".repeat(32))).unwrap();
    let key_str = key.to_str().unwrap();
    let r = run(
        &[
            "--create",
            "--key-file",
            key_str,
            enc_str,
            "-c",
            "CREATE TABLE t (a i32 PRIMARY KEY); INSERT INTO t VALUES (1)",
        ],
        "",
    );
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    let enc_url = format!("ssh://dbhost{enc_str}");
    let r = run_with_path(
        &bin,
        &[
            "--key-file",
            key_str,
            &enc_url,
            "-q",
            "-c",
            "INSERT INTO t VALUES (2)",
            "-c",
            "SELECT count(*) AS n FROM t",
        ],
    );
    assert_eq!((r.code, r.stderr.as_str()), (0, ""), "{}", r.stderr);
    assert!(r.stdout.contains(" 2\n"), "stdout: {}", r.stdout);
    let r = run_with_path(&bin, &[&enc_url, "-c", "SELECT 1"]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("ERROR 28P02"), "stderr: {}", r.stderr);
    let _ = std::fs::remove_dir_all(&bin);
}

#[test]
fn pages_subcommand_rejects_bad_usage() {
    let r = run(&["pages"], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("missing DBFILE"), "stderr: {}", r.stderr);
    let r = run(&["pages", "--nope", "x.jed"], "");
    assert_eq!(r.code, 1);
    // The credential arrives in the client's hello, so jed pages takes no key flags.
    let r = run(&["pages", "--key-file", "k", "x.jed"], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("unknown flag"), "stderr: {}", r.stderr);
    // The first frame must be hello: anything else gets a fatal error frame, not SQL output.
    let db = tmp("pages_garbage.jed");
    let r = run(&["--create", db.to_str().unwrap()], "");
    assert_eq!(r.code, 0);
    let r = run(
        &["pages", db.to_str().unwrap()],
        "garbage that is not a frame",
    );
    assert_eq!(r.code, 1);
    assert!(
        r.stderr.starts_with("jed pages: ERROR"),
        "stderr: {}",
        r.stderr
    );
    remove_db(&db);
}

// ───────────────────────────── encrypted databases ─────────────────────────────

/// An encrypted database through the CLI (cli.md §3): created with a key file, refused without a
/// credential (`28P02`, with a hint) or with the wrong one (`28P01`), opened with a passphrase file
/// after `jed migrate` made one, and never given a credential on the command line.
#[test]
fn encrypted_databases_take_a_key_or_passphrase_file() {
    let db = tmp("enc.jed");
    let db_str = db.to_str().unwrap();
    let key = tmp("enc.key");
    std::fs::write(&key, format!("{}\n", "4a".repeat(32))).unwrap();
    let key_str = key.to_str().unwrap();
    let r = run(
        &[
            "--create",
            "--key-file",
            key_str,
            db_str,
            "-q",
            "-c",
            "CREATE TABLE t (id i32 PRIMARY KEY, s text); INSERT INTO t VALUES (1, 'secret-cli')",
        ],
        "",
    );
    assert_eq!((r.code, r.stderr.as_str()), (0, ""), "{}", r.stderr);
    let bytes = std::fs::read(&db).unwrap();
    assert!(!bytes.windows(10).any(|w| w == b"secret-cli"));

    let r = run(&[db_str, "-c", "SELECT s FROM t"], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("ERROR 28P02"), "{}", r.stderr);
    assert!(r.stderr.contains("--key-file"), "{}", r.stderr);
    let wrong = tmp("enc_wrong.key");
    std::fs::write(&wrong, [7u8; 32]).unwrap();
    let r = run(
        &[
            "--key-file",
            wrong.to_str().unwrap(),
            db_str,
            "-c",
            "SELECT 1",
        ],
        "",
    );
    assert!(r.stderr.contains("ERROR 28P01"), "{}", r.stderr);
    let r = run(
        &[
            "--key-file",
            key_str,
            "--readonly",
            db_str,
            "-c",
            "SELECT s FROM t",
        ],
        "",
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("secret-cli"), "{}", r.stdout);

    // A malformed key file and a repeated credential flag are usage errors.
    let bad = tmp("enc_bad.key");
    std::fs::write(&bad, "not a key").unwrap();
    let r = run(
        &[
            "--key-file",
            bad.to_str().unwrap(),
            db_str,
            "-c",
            "SELECT 1",
        ],
        "",
    );
    assert_eq!(r.code, 1);
    assert!(
        r.stderr.contains("32 raw bytes or 64 hex digits"),
        "{}",
        r.stderr
    );
    let r = run(&["--key-file", key_str, "--passphrase", db_str], "");
    assert_ne!(r.code, 0);

    // jed migrate creates an encrypted database under a passphrase file and reopens it.
    let mdb = tmp("enc_migrate.jed");
    let mdb_str = mdb.to_str().unwrap();
    let pass = tmp("enc.pass");
    std::fs::write(&pass, "correct horse\n").unwrap();
    let pass_str = pass.to_str().unwrap();
    let migs = migrate_testdata("blog");
    let r = run(
        &[
            "migrate",
            "--passphrase-file",
            pass_str,
            "-m",
            &migs,
            mdb_str,
        ],
        "",
    );
    assert_eq!((r.code, r.stderr.as_str()), (0, ""), "{}", r.stderr);
    let r = run(&["migrate", "status", "-m", &migs, mdb_str], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("28P02"), "{}", r.stderr);
    let r = run(
        &[
            "migrate",
            "status",
            "--passphrase-file",
            pass_str,
            "-m",
            &migs,
            mdb_str,
        ],
        "",
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("version:  3 of 3"), "{}", r.stdout);

    for p in [&key, &wrong, &bad, &pass] {
        let _ = std::fs::remove_file(p);
    }
    remove_db(&db);
    remove_db(&mdb);
}

/// A copy of a shared integrity fixture (spec/fileformat/integrity/), so opening it leaves no lock
/// bundle in the spec tree.
fn integrity_fixture(name: &str) -> PathBuf {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../spec/fileformat/integrity")
        .join(format!("{name}.jed"));
    let dst = tmp(&format!("check_{name}.jed"));
    std::fs::copy(&src, &dst).unwrap_or_else(|e| panic!("{}: {e}", src.display()));
    dst
}

#[test]
fn check_reports_findings_and_exits_by_verdict() {
    // A clean database: exit 0 and the summary line.
    let db = tmp("check_clean.jed");
    let db_str = db.to_str().unwrap();
    let r = run(
        &[
            "--create",
            db_str,
            "-q",
            "-c",
            "CREATE TABLE t (id i32 PRIMARY KEY, v i32 CHECK (v > 0)); CREATE INDEX ON t (v); \
             INSERT INTO t VALUES (1, 10), (2, 20)",
        ],
        "",
    );
    assert_eq!((r.code, r.stderr.as_str()), (0, ""), "{}", r.stderr);
    let r = run(&["check", "--constraints", db_str], "");
    assert_eq!((r.code, r.stderr.as_str()), (0, ""), "{}", r.stderr);
    assert!(r.stdout.starts_with("ok: "), "{}", r.stdout);
    remove_db(&db);

    // A damaged one: one line per finding, then the summary; exit 2.
    let bad = integrity_fixture("expr_overflow");
    let r = run(&["check", bad.to_str().unwrap()], "");
    assert_eq!(r.code, 2, "{}", r.stderr);
    let lines: Vec<&str> = r.stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{}", r.stdout);
    assert!(
        lines[0].starts_with("index_entry_error u_v1_idx key 8000000a: "),
        "{}",
        r.stdout
    );
    assert!(
        lines[1].starts_with("index_entry_count_mismatch u_v1_idx: "),
        "{}",
        r.stdout
    );
    assert_eq!(lines[2], "2 findings: 53 pages, 40 rows checked");
    // --quick skips index agreement, so the same file checks clean.
    let r = run(&["check", "--quick", bad.to_str().unwrap()], "");
    assert_eq!(r.code, 0, "{}", r.stdout);
    remove_db(&bad);

    // JSON is the report object; --max-findings truncates it.
    let broken = integrity_fixture("constraints_broken");
    let broken_str = broken.to_str().unwrap();
    let r = run(
        &[
            "check",
            "--constraints",
            "--format",
            "json",
            "--max-findings",
            "2",
            broken_str,
        ],
        "",
    );
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(
        r.stdout.starts_with(
            "{\"findings\":[{\"kind\":\"foreign_key_violation\",\"database\":\"main\",\"object\":\"c_ab_fkey\",\"page\":null,\"key\":\"80000004\","
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .ends_with(",\"truncated\":true,\"skipped\":[],\"pages\":27,\"rows\":60}\n"),
        "{}",
        r.stdout
    );
    // Without --constraints the same rows are structurally sound.
    let r = run(&["check", broken_str], "");
    assert_eq!(r.code, 0, "{}", r.stdout);
    remove_db(&broken);
}

#[test]
fn check_usage_and_open_errors_exit_1() {
    let r = run(&["check"], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("missing DBFILE"), "{}", r.stderr);
    let r = run(&["check", "--format", "xml", "x.jed"], "");
    assert_eq!(r.code, 1);
    let missing = tmp("check_missing.jed");
    let r = run(&["check", missing.to_str().unwrap()], "");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("58P01"), "{}", r.stderr);
    let r = run(&["check", "--help"], "");
    assert_eq!(r.code, 0);
    assert!(r.stdout.starts_with("usage: jed check"), "{}", r.stdout);
}

#[test]
fn check_takes_the_credential_of_an_encrypted_database() {
    let db = tmp("check_enc.jed");
    let db_str = db.to_str().unwrap();
    let key = tmp("check_enc.key");
    std::fs::write(&key, format!("{}\n", "4b".repeat(32))).unwrap();
    let key_str = key.to_str().unwrap();
    let r = run(
        &[
            "--create",
            "--key-file",
            key_str,
            db_str,
            "-q",
            "-c",
            "CREATE TABLE t (id i32 PRIMARY KEY); INSERT INTO t VALUES (1)",
        ],
        "",
    );
    assert_eq!((r.code, r.stderr.as_str()), (0, ""), "{}", r.stderr);
    let r = run(&["check", db_str], "");
    assert_eq!(r.code, 1);
    assert!(
        r.stderr.contains("28P02") && r.stderr.contains("--key-file"),
        "{}",
        r.stderr
    );
    let r = run(&["check", "--key-file", key_str, db_str], "");
    assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
    let _ = std::fs::remove_file(&key);
    remove_db(&db);
}

#[test]
fn check_reports_a_file_too_damaged_to_open() {
    // Both meta slots fail their checksum: open fails XX001, which is the check's answer, not a
    // usage error.
    let db = integrity_fixture("u_clean");
    let mut bytes = std::fs::read(&db).unwrap();
    for off in [40, 256 + 40] {
        bytes[off] ^= 0xFF;
    }
    std::fs::write(&db, &bytes).unwrap();
    let r = run(&["check", db.to_str().unwrap()], "");
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stdout.starts_with("open_failed: XX001 "), "{}", r.stdout);
    let r = run(&["check", "--format", "json", db.to_str().unwrap()], "");
    assert_eq!(r.code, 2);
    assert!(
        r.stdout
            .starts_with("{\"open_failed\":{\"code\":\"XX001\",\"message\":"),
        "{}",
        r.stdout
    );
    remove_db(&db);
}
