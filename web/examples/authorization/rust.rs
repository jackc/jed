use jed::{Database, DatabaseOptions, Privilege, PrivilegeSet, SessionOptions};

fn main() -> jed::Result<()> {
    let mut db = Database::create("app.jed", DatabaseOptions::default())?;
    db.execute("CREATE TABLE report (id i32 PRIMARY KEY, body text)", &[])?;
    db.execute("INSERT INTO report VALUES (1, 'hello')", &[])?;

    // Serve untrusted queries through a SESSION granted ONLY read access: default_privileges =
    // {SELECT} (a read-only envelope) with DDL disabled. A session is a handle minted from the
    // database that shares its committed state. The engine enforces the envelope at name resolution —
    // any write or schema change resolves to 42501, with no in-database role catalog.
    let mut untrusted = db.session(SessionOptions {
        default_privileges: PrivilegeSet::EMPTY.with(Privilege::Select),
        allow_ddl: false,
        ..SessionOptions::default()
    });
    untrusted.execute("SELECT body FROM report", &[])?; // ok
    let denied = untrusted.execute("DELETE FROM report", &[]);
    assert_eq!(denied.unwrap_err().code(), "42501"); // permission denied for table report
    untrusted.close(); // release the session (and its reader pin)

    // grant/revoke adjust one object at a time on a session's envelope, and revoke always wins. Revoke
    // EXECUTE on a volatile function to pin a session's determinism — calls to it then fail 42501.
    let mut locked = db.session(SessionOptions::default());
    locked.revoke(PrivilegeSet::EMPTY.with(Privilege::Execute), "uuidv4");
    assert_eq!(locked.execute("SELECT uuidv4()", &[]).unwrap_err().code(), "42501");
    locked.close();

    Ok(())
}
