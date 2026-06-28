import { createDatabase, render } from 'jed-ts';

// Open a database. createDatabase/openDatabase return a Database — the handle you run SQL through. A
// path gives a single-file database on disk; `Database.newInMemory()` is a transient in-memory one.
// Writes accumulate until an explicit commit (close discards uncommitted changes).
const db = createDatabase('people.jed');

db.execute('CREATE TABLE person (id i32 PRIMARY KEY, name text NOT NULL)');
db.execute("INSERT INTO person VALUES (1, 'Ada'), (2, 'Grace')");
db.commit();

// query() returns a row cursor; execute() is for statements that produce no rows.
for (const row of db.query('SELECT name FROM person ORDER BY id')) {
  console.log(render(row[0]));
}

db.close();
