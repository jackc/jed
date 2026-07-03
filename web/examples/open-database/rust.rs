use jed::{CreateOptions, Database};

fn main() -> jed::Result<()> {
    // Open a database. `create`/`open` return a `Database` — the handle you run SQL through. A path
    // gives a single-file database on disk; `Database::create(CreateOptions::default())` (no path) is a transient in-memory one.
    // Each bare `execute` autocommits durably (it runs on a fresh session); for a multi-statement
    // transaction use `db.update(...)` or mint a `Session`.
    let mut db = Database::create(CreateOptions { path: Some("people.jed".into()), ..Default::default() })?;

    db.execute("CREATE TABLE person (id i32 PRIMARY KEY, name text NOT NULL)", &[])?;
    db.execute("INSERT INTO person VALUES (1, 'Ada'), (2, 'Grace')", &[])?;

    // query() returns a row cursor; execute() is for statements that produce no rows.
    for row in db.query("SELECT name FROM person ORDER BY id", &[])? {
        println!("{}", row[0].render());
    }

    Ok(())
}
