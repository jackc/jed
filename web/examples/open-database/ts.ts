import { createDatabase, render } from 'jed-ts';

// Node uses a long-lived execution worker. For the same API with browser OPFS,
// import from 'jed-ts/browser'; path names an origin-private file there.
// Omit path to create a transient in-memory database on either platform.
const db = await createDatabase({ path: 'people.jed', backend: 'typescript' });
try {
  await db.execute('CREATE TABLE person (id i32 PRIMARY KEY, name text NOT NULL)');
  await db.execute("INSERT INTO person VALUES (1, 'Ada'), (2, 'Grace')");

  // Each bare operation autocommits. Delivery fetches bounded typed batches;
  // leaving the loop early closes its cursor and releases the read snapshot.
  for await (const row of await db.query('SELECT name FROM person ORDER BY id')) {
    console.log(render(row[0]));
  }
} finally {
  await db.close(); // releases sessions, transactions, storage, and the worker
}
