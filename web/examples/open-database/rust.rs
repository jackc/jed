use jed::{execute, Database, DatabaseOptions, Outcome};

fn main() -> jed::Result<()> {
    // Open a database. A path creates a single-file database on disk; `Database::new()` is a
    // transient in-memory one. Writes accumulate until an explicit commit (close discards
    // uncommitted changes).
    let mut db = Database::create("people.jed", DatabaseOptions::default())?;

    execute(&mut db, "CREATE TABLE person (id int32 PRIMARY KEY, name text NOT NULL)")?;
    execute(&mut db, "INSERT INTO person VALUES (1, 'Ada'), (2, 'Grace')")?;
    db.commit()?;

    if let Outcome::Query { rows, .. } = execute(&mut db, "SELECT name FROM person ORDER BY id")? {
        for row in &rows {
            println!("{}", row[0].render());
        }
    }

    db.close();
    Ok(())
}
