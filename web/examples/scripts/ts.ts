import { createDatabase, splitStatements, render } from 'jed-ts';

const db = await createDatabase({ path: 'app.jed' });
try {
  // One implicit transaction: any statement error rolls the whole script back.
  // Result rows are discarded inside the worker, leaving an O(1) summary.
  const summary = await db.executeScript(
    `CREATE TABLE account (id i32 PRIMARY KEY, balance i64);
     INSERT INTO account VALUES (1, 100), (2, 50);
     CREATE INDEX account_balance ON account (balance);`
  );
  console.log(`ran ${summary.statementsRun} statements`);

  // To receive each statement's rows, split explicitly and choose the boundary.
  // These calls each run under autocommit; use a session for explicit SQL BEGIN.
  for (const stmt of splitStatements('SELECT id FROM account; SELECT balance FROM account')) {
    for await (const row of await db.query(stmt.text)) console.log(row.map(render));
  }
} finally {
  await db.close();
}
