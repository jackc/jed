import { create, executeScript, splitStatements, query, close } from "jed-ts";

const db = create("app.jed");

// executeScript runs a whole migration as ONE implicit transaction: split it into statements, run
// each in order, and commit all-or-nothing (any error rolls the lot back). It DISCARDS result rows —
// you get back only an O(1) summary (statements run, rows affected, cost), so a huge import never
// buffers results.
const summary = executeScript(
  db,
  `CREATE TABLE account (id i32 PRIMARY KEY, balance i64);
   INSERT INTO account VALUES (1, 100), (2, 50);
   CREATE INDEX account_balance ON account (balance);`,
);
console.log(`ran ${summary.statementsRun} statements`);

// splitStatements is the library-level primitive (no Database needed). When you DO want each
// statement's rows, loop it yourself and run the spans through the normal path — the host owns the
// policy (one transaction or autocommit, drain rows or drop them).
for (const stmt of splitStatements("SELECT id FROM account; SELECT balance FROM account")) {
  query(db, stmt.text);
}

close(db);
