use jed::{split_statements, Database, DatabaseOptions};

fn main() -> jed::Result<()> {
    let mut db = Database::create("app.jed", DatabaseOptions::default())?;

    // execute_script runs a whole migration as ONE implicit transaction: split it into statements,
    // run each in order, and commit all-or-nothing (any error rolls the lot back). It DISCARDS
    // result rows — you get back only an O(1) summary (statements run, rows affected, cost), so a
    // huge import never buffers results.
    let summary = db.execute_script(
        "CREATE TABLE account (id i32 PRIMARY KEY, balance i64);
         INSERT INTO account VALUES (1, 100), (2, 50);
         CREATE INDEX account_balance ON account (balance);",
    )?;
    println!("ran {} statements", summary.statements_run);

    // split_statements is the library-level primitive (no Database needed). When you DO want each
    // statement's rows, loop it yourself and run the spans through the normal path — the host owns
    // the policy (one transaction or autocommit, drain rows or drop them).
    for stmt in split_statements("SELECT id FROM account; SELECT balance FROM account") {
        let _rows = db.query(stmt.text(), &[])?;
    }

    Ok(())
}
