//! The page service's stream transport (spec/pageservice/README.md §5.3): `Database::open_remote`
//! over a transport command and `Database::serve_page_stream` (`jed pages`) on the other end. These
//! are host-API tests (CLAUDE.md §10). The transport here is an in-process `serve_page_stream` over
//! OS pipes, which is what `ssh host jed pages /path` runs on the far side; the real-process corpus
//! (`service_stream.process.toml`) and the CLI's e2e tests run it as a separate process. No test
//! reaches a remote host.
#![cfg(unix)]

use std::path::{Path, PathBuf};

use jed::value::Value;
use jed::{CreateOptions, Database, OpenOptions};

fn tmp(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("remote_stream");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(path.with_extension("jed.lock"));
    path
}

fn create(path: &Path) {
    let mut db = Database::create(CreateOptions {
        path: Some(path.to_path_buf()),
        ..CreateOptions::default()
    })
    .unwrap();
    db.execute("CREATE TABLE t (id i64 PRIMARY KEY, v text)", &[])
        .unwrap();
    db.execute(
        "INSERT INTO t SELECT g, repeat('x', 200) FROM generate_series(1, 2000) g",
        &[],
    )
    .unwrap();
    db.close().unwrap();
}

fn open_remote(serve: &str, path: &Path) -> jed::Result<Database> {
    Database::open_remote_in_process(
        path,
        serve == "pages-read-only",
        &format!("remote:{}", path.display()),
        OpenOptions::default(),
    )
}

/// Open `path` locally once the in-process server of a closed remote handle has closed its own
/// handle (the far side of a real transport is another process, which exits instead).
fn open_after_server_exits(path: &Path) -> Database {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match Database::open(path) {
            Ok(db) => return db,
            Err(e) if e.code() == "55006" && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("open after the remote handle closed: {e:?}"),
        }
    }
}

fn scalar(db: &mut Database, sql: &str) -> i64 {
    let rows: Vec<_> = db.query(sql, &[]).unwrap().collect();
    match &rows[0][0] {
        Value::Int(n) => *n,
        other => panic!("expected an i64, got {other:?}"),
    }
}

/// With no owner, `jed pages` becomes the owner and serves the stream; the remote client reads,
/// writes, and its commits land in the file.
#[test]
fn remote_client_through_a_transport_that_owns() {
    let path = tmp("owns.jed");
    create(&path);
    let mut remote = open_remote("pages", &path).unwrap();
    assert_eq!(remote.page_service_role_for_testing(), "client");
    assert_eq!(scalar(&mut remote, "SELECT count(*) FROM t"), 2000);
    assert_eq!(
        scalar(&mut remote, "SELECT sum(id)::i64 FROM t WHERE id > 1990"),
        19955
    );
    remote
        .execute("INSERT INTO t VALUES (5000, 'remote')", &[])
        .unwrap();
    remote.close().unwrap();

    let mut local = open_after_server_exits(&path);
    assert_eq!(scalar(&mut local, "SELECT count(*) FROM t"), 2001);
    local.close().unwrap();
}

/// With a live owner (this process), `jed pages` relays the stream to its endpoint: both sides see
/// each other's commits.
#[test]
fn remote_client_relayed_to_a_live_owner() {
    let path = tmp("relay.jed");
    create(&path);
    let mut owner = Database::open(&path).unwrap();
    assert_eq!(owner.page_service_role_for_testing(), "owner");
    let mut remote = open_remote("pages", &path).unwrap();
    remote
        .execute("INSERT INTO t VALUES (6000, 'from remote')", &[])
        .unwrap();
    assert_eq!(scalar(&mut owner, "SELECT count(*) FROM t"), 2001);
    owner
        .execute("DELETE FROM t WHERE id <= 1000", &[])
        .unwrap();
    assert_eq!(scalar(&mut remote, "SELECT count(*) FROM t"), 1001);

    // The owner leaves: the remote client's next transaction runs the transport again, which now
    // finds no owner and owns the file itself.
    owner.close().unwrap();
    assert_eq!(scalar(&mut remote, "SELECT count(*) FROM t"), 1001);
    remote
        .execute("INSERT INTO t VALUES (6001, 'after handoff')", &[])
        .unwrap();
    remote.close().unwrap();

    let mut local = open_after_server_exits(&path);
    assert_eq!(scalar(&mut local, "SELECT count(*) FROM t"), 1002);
    local.close().unwrap();
}

/// `jed pages --read-only` makes the connection read-only whatever the client asks.
#[test]
fn read_only_transport_refuses_writes() {
    let path = tmp("read_only.jed");
    create(&path);
    let mut remote = open_remote("pages-read-only", &path).unwrap();
    assert!(remote.read_only());
    assert_eq!(scalar(&mut remote, "SELECT count(*) FROM t"), 2000);
    let error = remote
        .execute("INSERT INTO t VALUES (7000, 'nope')", &[])
        .unwrap_err();
    assert_eq!(error.code(), "25006");
    remote.close().unwrap();
}

/// An error the serving side reports at open is the open's error.
#[test]
fn missing_remote_file_is_the_open_error() {
    let path = tmp("missing.jed");
    let Err(error) = open_remote("pages", &path) else {
        panic!("opening a missing remote file succeeded");
    };
    assert_eq!(error.code(), "58P01");
}

/// A transport that cannot connect fails the open `08006` with its standard error.
#[test]
fn failing_transport_reports_its_stderr() {
    let Err(error) = Database::open_remote(
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo 'ssh: connect to host nowhere: Connection refused' >&2; exit 255".to_string(),
        ],
        "remote:nowhere",
        OpenOptions {
            file_lock_timeout_ms: 300,
            ..OpenOptions::default()
        },
    ) else {
        panic!("a failing transport opened");
    };
    assert_eq!(error.code(), "08006");
    assert!(
        error.message.contains("Connection refused"),
        "message: {}",
        error.message
    );
}

/// `read_pages` requests this thread has sent.
fn read_requests() -> u64 {
    jed::pageservice::client::READ_PAGES_SENT.with(|sent| sent.get())
}

/// Scans over the stream transport read ahead: a full scan fetches its leaves in batches, a scan
/// stopped by LIMIT fetches no leaf beyond the first, and the cost is the local run's.
#[test]
fn scans_read_ahead_without_changing_cost() {
    let path = tmp("read_ahead.jed");
    create(&path);
    let full = "SELECT sum(length(v))::i64 FROM t";
    let first = "SELECT id FROM t ORDER BY id LIMIT 1";
    let (full_cost, first_cost) = {
        let mut local = Database::open(&path).unwrap();
        let mut rows = local.query(full, &[]).unwrap();
        rows.by_ref().for_each(drop);
        let full_cost = rows.cost();
        let mut rows = local.query(first, &[]).unwrap();
        rows.by_ref().for_each(drop);
        let first_cost = rows.cost();
        local.close().unwrap();
        (full_cost, first_cost)
    };

    let mut remote = open_remote("pages", &path).unwrap();
    let before = read_requests();
    let mut rows = remote.query(first, &[]).unwrap();
    rows.by_ref().for_each(drop);
    assert_eq!(rows.cost(), first_cost);
    drop(rows);
    assert_eq!(read_requests() - before, 1, "LIMIT 1 fetches one leaf");

    let before = read_requests();
    let mut rows = remote.query(full, &[]).unwrap();
    let total: Vec<_> = rows.by_ref().collect();
    assert_eq!(rows.cost(), full_cost);
    drop(rows);
    assert_eq!(total[0][0], Value::Int(2000 * 200));
    let leaves = 2000 * 200 / 4096;
    let requests = read_requests() - before;
    assert!(
        requests * 4 < leaves,
        "{requests} read_pages requests for about {leaves} leaves"
    );
    remote.close().unwrap();
}
