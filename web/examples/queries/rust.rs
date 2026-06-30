use jed::Database;

fn main() -> jed::Result<()> {
    let mut db = Database::open("app.jed")?;

    // run() binds native params — a tuple here — to $1, $2, … and returns the affected-row count.
    // No hand-built Value::Int / Value::Text: the rusqlite-style ToValue/Params traits do the
    // conversion (and a raw &[Value] still works — it implements Params too).
    let affected = db.run(
        "INSERT INTO account (id, name, balance) VALUES ($1, $2, $3)",
        (1, "Ada", 100_i64),
    )?;
    println!("inserted {affected} row");

    // query_row maps the FIRST row through a closure, returning Option<T> (None when nothing
    // matched). row.get::<T>(i) converts column i to a native type (FromValue); Option<T> is the
    // only target that accepts SQL NULL — a bare scalar rejects it with 22004.
    let balance: Option<i64> =
        db.query_row("SELECT balance FROM account WHERE id = $1", (1,), |row| row.get(0))?;
    println!("balance = {balance:?}");

    // query_map maps every row; read columns by index or by name.
    let names: Vec<String> = db.query_map("SELECT name FROM account ORDER BY id", (), |row| {
        row.get_by_name("name")
    })?;
    println!("{} account(s): {names:?}", names.len());

    Ok(())
}
