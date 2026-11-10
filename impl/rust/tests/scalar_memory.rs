use jed::{CreateOptions, Database, SessionOptions};

#[test]
fn scalar_budget_stays_with_cursor() {
    let db = Database::create(CreateOptions::default()).unwrap();
    let mut s = db.session(SessionOptions {
        max_scalar_bytes: 3,
        ..Default::default()
    });
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    s.execute("INSERT INTO t VALUES (1), (2)", &[]).unwrap();
    let mut old = s
        .query("SELECT repeat('x', 3) FROM t ORDER BY id", &[])
        .unwrap();
    assert!(matches!(old.next(), Some(Ok(_))));
    s.set_max_scalar_bytes(100);
    s.query_outcome("SELECT repeat('y', 9)", &[]).unwrap();
    let err = old.next().expect("the 54P04").unwrap_err();
    assert_eq!(err.code(), "54P04");
    assert!(old.next().is_none());
    s.set_max_scalar_bytes(0);
    assert_eq!(s.max_scalar_bytes(), 67108864);
}
