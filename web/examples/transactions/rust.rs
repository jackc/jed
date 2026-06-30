use jed::Database;

fn main() -> jed::Result<()> {
    let mut db = Database::open("bank.jed")?;

    // update() runs a read-write transaction: it mints a session, runs the closure, commits on
    // success, and rolls back if the closure returns an error — so the two writes are atomic. view()
    // is the read-only sibling. (For an explicit block spanning calls, mint a Session and drive
    // begin/commit/rollback on it: `let mut s = db.session(SessionOptions::default());`.)
    db.update(|tx| {
        tx.execute("UPDATE account SET balance = balance - 100 WHERE id = 1", &[])?;
        tx.execute("UPDATE account SET balance = balance + 100 WHERE id = 2", &[])?;
        Ok(())
    })?;

    Ok(())
}
