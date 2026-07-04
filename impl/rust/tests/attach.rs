//! Host-attached in-memory databases — the `Database::attach`/`detach` host API (spec/design/attached-
//! databases.md §4/§6, Slice 1b). These are the behaviors the shared corpus CANNOT express (it is
//! single-handle SQL-in/rows-out and cannot call `db.attach` — CLAUDE.md §10): the attach/detach
//! lifecycle, the read-only write-rejection (25006), detach-in-use (55006), reserved/duplicate names
//! (42710), unknown detach (42704), and the file-source deferral (0A000). The SQL routing itself lives
//! in the corpus (suites/attach/in_memory.test). Mirrors impl/go/attach_test.go and
//! impl/ts/test/attach.test.ts.

use jed::value::Value;
use jed::{AttachSource, CreateOptions, Database, Session, SessionOptions};

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

/// A FILE-backed attach source is Slice 2; `Database::attach` with a file source returns 0A000 now, so
/// the host-API signature never changes when file attach lands. This is also the one-durable-writer
/// guard's inert form in 1b (no writable file attachment can exist yet).
#[test]
fn attach_file_source_deferred() {
    let db = mem_db();
    let err = db
        .attach("f", AttachSource::file("/tmp/whatever.jed"), false)
        .expect_err("attach file source should fail");
    assert_eq!(err.code(), "0A000");
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
