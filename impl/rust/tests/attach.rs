//! Host-attached databases — the `Database::attach`/`detach` host API (spec/design/attached-databases.md
//! §4/§6, Slices 1b + 2). These are the behaviors the shared corpus CANNOT express (it is single-handle
//! SQL-in/rows-out and cannot call `db.attach` — CLAUDE.md §10): the attach/detach lifecycle, the
//! read-only write-rejection (25006), detach-in-use (55006), reserved/duplicate names (42710), unknown
//! detach (42704), and — for FILE attachments (Slice 2) — cross-file read/join, read-write durability
//! across a standalone reopen, the one-durable-writer rule (0A000), page-size independence, and
//! missing-file (58P01). The in-memory SQL routing lives in the corpus (suites/attach/in_memory.test);
//! file durability / reopen is inherently a per-core host test (out of corpus reach). Mirrors
//! impl/go/attach_test.go and impl/ts/tests/attach.test.ts.

use std::path::PathBuf;

use jed::value::Value;
use jed::{AttachSource, CreateOptions, Database, OpenOptions, Session, SessionOptions};

/// A path under Cargo's per-test temp dir (never the repo tree), matching tests/api.rs.
fn tmp(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_file(&path);
    path
}

/// Create a fresh single-file database at `path` with page size `page_size` (0 → default), run each
/// statement (autocommitting durably), and close it — the reusable fixture for the file-attach tests
/// (a self-describing jed file another handle can attach). Returns `path`.
fn make_file_db(path: PathBuf, page_size: u32, stmts: &[&str]) -> PathBuf {
    let db = Database::create(CreateOptions {
        path: Some(path.clone()),
        page_size,
    })
    .expect("create file db");
    let mut s = db.session(SessionOptions::default());
    for sql in stmts {
        exec(&mut s, sql);
    }
    s.close();
    db.close().expect("close file db");
    path
}

/// Query a single-column text result on a session (draining + dropping the cursor).
fn query_strs(s: &mut Session, sql: &str) -> Vec<String> {
    s.query(sql, &[])
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
        .collect::<Vec<Vec<Value>>>()
        .iter()
        .map(|r| match &r[0] {
            Value::Text(t) => t.clone(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect()
}

/// Run a statement on a session and fail the test on error.
fn exec(s: &mut Session, sql: &str) {
    s.query_outcome(sql, &[])
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message));
}

/// Run a statement expected to fail and return its SQLSTATE.
fn err_code(s: &mut Session, sql: &str) -> String {
    s.query_outcome(sql, &[])
        .expect_err(&format!("{sql:?}: expected an error"))
        .code()
        .to_string()
}

/// Query a single-column integer result on a session, draining + dropping the cursor (releasing its
/// reader pin — a live cursor would hold the roots and block a subsequent detach).
fn query_ints(s: &mut Session, sql: &str) -> Vec<i64> {
    let rows: Vec<Vec<Value>> = s
        .query(sql, &[])
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
        .collect();
    rows.iter()
        .map(|r| match &r[0] {
            Value::Int(n) => *n,
            other => panic!("expected an i64, got {other:?}"),
        })
        .collect()
}

fn mem_db() -> Database {
    Database::create(CreateOptions::default()).unwrap()
}

/// Drives the whole single-handle arc: attach an in-memory database, create + populate a table in it
/// by qualifier, read it back, then detach it (making it unreachable again).
#[test]
fn attach_lifecycle() {
    let db = mem_db();
    db.attach("mydb", AttachSource::memory(), false)
        .expect("attach");
    let mut s = db.session(SessionOptions::default());
    exec(&mut s, "CREATE TABLE mydb.t (id i32 PRIMARY KEY, v i32)");
    exec(&mut s, "INSERT INTO mydb.t VALUES (1, 10), (2, 20)");

    assert_eq!(
        query_ints(&mut s, "SELECT v FROM mydb.t ORDER BY id"),
        vec![10, 20]
    );

    // The committed attachment change is visible to a freshly-minted session over the same handle
    // (the roots.attached publish, §5) — proves the commit published a new attached root.
    let mut s2 = db.session(SessionOptions::default());
    assert_eq!(
        query_ints(&mut s2, "SELECT v FROM mydb.t WHERE id = 1"),
        vec![10]
    );

    db.detach("mydb").expect("detach");
    // After detach the qualifier is unknown again (42P01).
    let mut s3 = db.session(SessionOptions::default());
    assert_eq!(err_code(&mut s3, "SELECT v FROM mydb.t"), "42P01");
}

/// A read-only attachment rejects every write (DML + DDL) with 25006 before any I/O (§4), while a
/// bare/main write is unaffected.
#[test]
fn attach_read_only_rejects_writes() {
    let db = mem_db();
    db.attach("ro", AttachSource::memory(), true)
        .expect("attach read-only");
    let mut s = db.session(SessionOptions::default());
    for sql in [
        "CREATE TABLE ro.t (id i32 PRIMARY KEY)",
        "CREATE INDEX ix ON ro.t (id)",
        "INSERT INTO ro.t VALUES (1)",
        "UPDATE ro.t SET id = 2",
        "DELETE FROM ro.t",
    ] {
        assert_eq!(err_code(&mut s, sql), "25006", "{sql:?}");
    }
    // A write to main is unaffected by a read-only attachment elsewhere.
    exec(&mut s, "CREATE TABLE keep (id i32 PRIMARY KEY)");
}

/// Detaching while a live reader session pins the committed roots is 55006 (object_in_use); once the
/// reader closes, the detach succeeds (§4/§5, the reader-liveness watermark — a reader pins the whole
/// roots, so it pins every attachment).
#[test]
fn detach_in_use_is_55006() {
    let db = mem_db();
    db.attach("mydb", AttachSource::memory(), false)
        .expect("attach");
    let mut reader = db.read_session(); // pins the committed roots in the live registry
    let err = db
        .detach("mydb")
        .expect_err("detach while a reader is live should fail");
    assert_eq!(err.code(), "55006");
    reader.close(); // drains the pin
    db.detach("mydb").expect("detach after reader closed");
}

/// A reserved name (main/temp) or an already-attached name is 42710; detaching an unknown / reserved
/// database is 42704.
#[test]
fn attach_name_errors() {
    let db = mem_db();
    db.attach("mydb", AttachSource::memory(), false)
        .expect("attach");
    for name in ["main", "temp", "MAIN", "Temp", "mydb", "MyDB"] {
        let err = db
            .attach(name, AttachSource::memory(), false)
            .expect_err(&format!("attach {name:?} should fail"));
        assert_eq!(err.code(), "42710", "{name:?}");
    }
    for name in ["nope", "main", "temp"] {
        let err = db
            .detach(name)
            .expect_err(&format!("detach {name:?} should fail"));
        assert_eq!(err.code(), "42704", "{name:?}");
    }
}

/// Attach an existing file database read-only, join a local table against it, and confirm every write
/// to it is 25006 (the natural reference-database mode, attached-databases.md §4, Slice 2). Reads fault
/// the attached file's pages through its own pager.
#[test]
fn attach_file_read_only_cross_read() {
    let refdb = make_file_db(
        tmp("ref_ro.jed"),
        0,
        &[
            "CREATE TABLE city (id i32 PRIMARY KEY, name text)",
            "INSERT INTO city VALUES (1, 'Ada'), (2, 'Bos')",
        ],
    );
    let db = mem_db();
    db.attach("ref", AttachSource::file(&refdb), true)
        .expect("attach file read-only");
    let mut s = db.session(SessionOptions::default());
    exec(
        &mut s,
        "CREATE TABLE visit (city_id i32 PRIMARY KEY, n i32)",
    );
    exec(&mut s, "INSERT INTO visit VALUES (1, 7), (2, 9)");

    // A cross-FILE join: local `visit` against the read-only attached file's `city`.
    assert_eq!(
        query_strs(
            &mut s,
            "SELECT c.name FROM visit v JOIN ref.city c ON c.id = v.city_id ORDER BY c.id"
        ),
        vec!["Ada".to_string(), "Bos".to_string()]
    );

    // Every write to the read-only attachment is 25006, before any I/O.
    for sql in [
        "CREATE TABLE ref.t (id i32 PRIMARY KEY)",
        "INSERT INTO ref.city VALUES (3, 'Cai')",
        "UPDATE ref.city SET name = 'x'",
        "DELETE FROM ref.city",
    ] {
        assert_eq!(err_code(&mut s, sql), "25006", "{sql:?}");
    }
    s.close();
    db.detach("ref").expect("detach");
}

/// Attach a file read-write, create+populate a table in it by qualifier, detach, then open that file
/// STANDALONE and confirm the writes are durable (attached-databases.md §5 — a file attachment commits
/// durably through its own pager + alternating meta slot + fsync).
#[test]
fn attach_file_read_write_persists_across_reopen() {
    let work = make_file_db(tmp("work_rw.jed"), 0, &[]); // an empty writable file to attach
    let db = mem_db();
    db.attach("work", AttachSource::file(&work), false)
        .expect("attach file read-write");
    let mut s = db.session(SessionOptions::default());
    exec(
        &mut s,
        "CREATE TABLE work.acct (id i32 PRIMARY KEY, bal i32)",
    );
    exec(&mut s, "INSERT INTO work.acct VALUES (1, 100), (2, 200)");
    exec(&mut s, "CREATE INDEX acct_bal ON work.acct (bal)");
    s.close();
    db.detach("work").expect("detach");
    db.close().expect("close");

    // Reopen the attached file on its own — the rows must be there (durable + self-describing).
    let reopened = Database::open(&work).expect("reopen attached file standalone");
    let mut rs = reopened.session(SessionOptions::default());
    assert_eq!(
        query_ints(&mut rs, "SELECT bal FROM acct ORDER BY id"),
        vec![100, 200]
    );
    // The index persisted too (introspection — the catalog carries it).
    let tbl = reopened.table("acct").expect("acct table");
    assert_eq!(tbl.indexes.len(), 1);
    assert_eq!(tbl.indexes[0].name, "acct_bal");
}

/// A transaction may write at most one FILE-backed database (§5). With a FILE main and a read-write
/// FILE attachment, a block that writes BOTH is 0A000 at COMMIT and commits nothing; writing either one
/// alone succeeds. In-memory attachments never count against the slot.
#[test]
fn attach_file_one_durable_writer() {
    let main_path = make_file_db(
        tmp("main_odw.jed"),
        0,
        &["CREATE TABLE m (id i32 PRIMARY KEY)"],
    );
    let extra = make_file_db(
        tmp("extra_odw.jed"),
        0,
        &["CREATE TABLE e (id i32 PRIMARY KEY)"],
    );

    let db = Database::open(&main_path).expect("open file main");
    db.attach("extra", AttachSource::file(&extra), false)
        .expect("attach extra file");
    let mut s = db.session(SessionOptions::default());
    s.begin(true).expect("begin");
    exec(&mut s, "INSERT INTO m VALUES (1)"); // main (file) dirtied
    exec(&mut s, "INSERT INTO extra.e VALUES (1)"); // a SECOND durable (file) database dirtied
    assert_eq!(
        s.commit()
            .expect_err("commit writing two durable databases should fail")
            .code(),
        "0A000"
    );
    // Nothing was committed — both files are still empty of the attempted rows.
    assert_eq!(query_ints(&mut s, "SELECT count(*) FROM m"), vec![0]);
    assert_eq!(query_ints(&mut s, "SELECT count(*) FROM extra.e"), vec![0]);

    // Writing each durable database ALONE (its own autocommit statement) is fine.
    exec(&mut s, "INSERT INTO m VALUES (2)");
    exec(&mut s, "INSERT INTO extra.e VALUES (2)");
    assert_eq!(query_ints(&mut s, "SELECT id FROM m"), vec![2]);
    assert_eq!(query_ints(&mut s, "SELECT id FROM extra.e"), vec![2]);
    s.close();
    db.detach("extra").expect("detach");
}

/// The slot counts only FILE databases: an IN-MEMORY main plus a read-write FILE attachment is ONE
/// durable writer, so a block writing both commits cleanly (§5).
#[test]
fn attach_file_with_memory_main_multi_write() {
    let work = make_file_db(
        tmp("work_mm.jed"),
        0,
        &["CREATE TABLE w (id i32 PRIMARY KEY)"],
    );
    let db = mem_db(); // in-memory main — not durable
    db.attach("work", AttachSource::file(&work), false)
        .expect("attach");
    let mut s = db.session(SessionOptions::default());
    exec(&mut s, "CREATE TABLE local (id i32 PRIMARY KEY)");
    s.begin(true).expect("begin");
    exec(&mut s, "INSERT INTO local VALUES (1)"); // in-memory main (free)
    exec(&mut s, "INSERT INTO work.w VALUES (1)"); // the one durable writer
    s.commit()
        .expect("memory-main + one file attachment should commit");
    assert_eq!(query_ints(&mut s, "SELECT id FROM work.w"), vec![1]);
    s.close();
    db.detach("work").expect("detach");
}

/// An attached file keeps its OWN page space (§2): attaching a file created at a non-default page size
/// and writing into it serializes at THAT page size, verified by a standalone reopen. Guards the CREATE
/// TABLE / CREATE INDEX page-size routing (`attach_page_size`).
#[test]
fn attach_file_page_size_independent() {
    let small = make_file_db(tmp("small_ps.jed"), 256, &[]); // a 256-byte-page file, unlike default main
    let db = mem_db();
    db.attach("small", AttachSource::file(&small), false)
        .expect("attach");
    let mut s = db.session(SessionOptions::default());
    exec(
        &mut s,
        "CREATE TABLE small.grid (id i32 PRIMARY KEY, v i32)",
    );
    // Enough rows to force at least one leaf split at the small page size (its own page space).
    for i in 1..=40 {
        exec(
            &mut s,
            &format!("INSERT INTO small.grid VALUES ({i}, {})", i * i),
        );
    }
    s.close();
    db.detach("small").expect("detach");
    db.close().expect("close");

    let reopened = Database::open(&small).expect("reopen small-page file");
    assert_eq!(reopened.page_size(), 256);
    let mut rs = reopened.session(SessionOptions::default());
    // sum of i*i for i in 1..=40 = 22140.
    assert_eq!(query_ints(&mut rs, "SELECT count(*) FROM grid"), vec![40]);
    assert_eq!(query_ints(&mut rs, "SELECT sum(v) FROM grid"), vec![22140]);
}

/// Detaching a file releases it, so the same file can be attached again.
#[test]
fn attach_file_reattach() {
    let refdb = make_file_db(
        tmp("ref_reattach.jed"),
        0,
        &[
            "CREATE TABLE t (id i32 PRIMARY KEY)",
            "INSERT INTO t VALUES (1)",
        ],
    );
    let db = mem_db();
    for _ in 0..3 {
        db.attach("ref", AttachSource::file(&refdb), true)
            .expect("attach");
        let mut s = db.session(SessionOptions::default());
        assert_eq!(query_ints(&mut s, "SELECT id FROM ref.t"), vec![1]);
        s.close();
        db.detach("ref").expect("detach");
    }
}

/// Attaching a nonexistent file surfaces the same host/file code as opening main (§11 / hosts.md §4);
/// the failed attach leaves no registry entry.
#[test]
fn attach_file_missing_is_58p01() {
    let db = mem_db();
    let missing = tmp("nope_attach.jed");
    assert_eq!(
        db.attach("x", AttachSource::file(&missing), true)
            .expect_err("attach missing file should fail")
            .code(),
        "58P01"
    );
    // The name is free after the failed attach.
    db.attach("x", AttachSource::memory(), false)
        .expect("re-attach after failed file attach");
}

/// An attachment is reached case-insensitively by its qualifier (unquoted identifiers fold to lower
/// case), matching how main/temp resolve.
#[test]
fn attach_case_insensitive_qualifier() {
    let db = mem_db();
    db.attach("Reports", AttachSource::memory(), false)
        .expect("attach");
    let mut s = db.session(SessionOptions::default());
    exec(&mut s, "CREATE TABLE reports.sales (id i32 PRIMARY KEY)");
    exec(&mut s, "INSERT INTO REPORTS.sales VALUES (1)");
    assert_eq!(query_ints(&mut s, "SELECT id FROM Reports.sales"), vec![1]);
}
