use jed::Database;

fn main() -> jed::Result<()> {
    let mut db = Database::open("bank.jed")?;

    // update() runs a read-write transaction on the handle: it commits on success and rolls back if
    // the closure returns an error — so the two writes are atomic. (begin/commit/rollback is the
    // explicit form; view() is the read-only sibling.)
    db.update(|tx| {
        tx.execute("UPDATE account SET balance = balance - 100 WHERE id = 1", &[])?;
        tx.execute("UPDATE account SET balance = balance + 100 WHERE id = 2", &[])?;
        Ok(())
    })?;

    Ok(())
}
