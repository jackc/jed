//! Host-only loading paths must not stage already committed GiST entries (§7).
use jed::{AttachSource, CreateOptions, Database, DeserializeOptions, Locking, SessionOptions};

#[test]
fn gist_deserialize_has_no_pending_writes() {
    check_gist_load("deserialize");
}

#[test]
fn gist_attachment_has_no_pending_writes() {
    check_gist_load("attach");
}

fn check_gist_load(mode: &str) {
    let path = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("gist_load_memory_{mode}.jed"));
    let _ = std::fs::remove_file(&path);
    let source = Database::create(CreateOptions {
        path: Some(path.clone()),
        skip_fsync: true,
        locking: Locking::None,
        ..Default::default()
    })
    .unwrap();
    let mut initial = source.session(SessionOptions::default());
    for sql in [
        "CREATE TABLE t (id i32 PRIMARY KEY, r i32range)",
        "CREATE INDEX t_r ON t USING gist (r)",
        "INSERT INTO t VALUES (1, '[1,5)')",
    ] {
        initial.execute(sql, &[]).unwrap();
    }
    initial.close();
    let image = source.serialize("main").unwrap();
    source.close().unwrap();
    let (db, name) = if mode == "deserialize" {
        (
            Database::deserialize(&image, DeserializeOptions::default()).unwrap(),
            "t",
        )
    } else {
        let db = Database::create(CreateOptions::default()).unwrap();
        db.attach(
            "aux",
            AttachSource::file(&path).locking(Locking::None),
            false,
        )
        .unwrap();
        (db, "aux.t")
    };
    let mut s = db.session(SessionOptions {
        max_query_memory_bytes: 1,
        ..Default::default()
    });
    // A point miss retains no input and stages nothing, including after load.
    s.execute(&format!("DELETE FROM {name} WHERE id = 99"), &[])
        .unwrap();
    let insert = format!("INSERT INTO {name} VALUES (2, '[10,20)')");
    assert_eq!(s.execute(&insert, &[]).unwrap_err().code(), "54P05");
    s.set_max_query_memory_bytes(4096);
    s.execute(&insert, &[]).unwrap();
    s.set_max_query_memory_bytes(1);
    s.execute(&format!("DELETE FROM {name} WHERE id = 99"), &[])
        .unwrap();
    s.set_max_query_memory_bytes(4096);
    let mut rows = s
        .query(&format!("SELECT count(*) FROM {name}"), &[])
        .unwrap();
    assert_eq!(rows.next(), Some(vec![jed::Value::Int(2)]));
    assert!(rows.next().is_none());
    rows.error().unwrap();
    drop(rows);
    s.close();
    db.close().unwrap();
    std::fs::remove_file(&path).unwrap();
}
