use jed::{Database, DatabaseOptions, Privilege, PrivilegeSet, SessionOptions};

fn main() -> jed::Result<()> {
    let mut db = Database::create("app.jed", DatabaseOptions::default())?;
    db.execute("CREATE TABLE report (id i32 PRIMARY KEY, body text)", &[])?;
    db.execute("INSERT INTO report VALUES (1, 'hello')", &[])?;

    // Serve untrusted queries through a session granted ONLY read access: default_privileges =
    // {SELECT} (a read-only envelope) with DDL disabled. The engine enforces this at name
    // resolution — any write or schema change resolves to 42501, with no in-database role catalog.
    let mut untrusted = db.session(SessionOptions {
        default_privileges: PrivilegeSet::EMPTY.with(Privilege::Select),
        allow_ddl: false,
        ..SessionOptions::default()
    });
    untrusted.execute(&mut db, "SELECT body FROM report", &[])?; // ok
    let denied = untrusted.execute(&mut db, "DELETE FROM report", &[]);
    assert_eq!(denied.unwrap_err().code(), "42501"); // permission denied for table report

    // grant/revoke adjust one object at a time, and revoke always wins. Revoke EXECUTE on a volatile
    // function to pin a session's determinism — calls to it then fail 42501.
    db.revoke(PrivilegeSet::EMPTY.with(Privilege::Execute), "uuidv4");

    db.close();
    Ok(())
}
