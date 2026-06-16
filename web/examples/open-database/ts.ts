import { create, execute, query, commit, close, render } from "jed-ts";

// Open a database. A path creates/opens a single-file database on disk; `new Database()` is a
// transient in-memory one. Writes accumulate until an explicit commit (close discards uncommitted
// changes).
const db = create("people.jed");

execute(db, "CREATE TABLE person (id int32 PRIMARY KEY, name text NOT NULL)");
execute(db, "INSERT INTO person VALUES (1, 'Ada'), (2, 'Grace')");
commit(db);

for (const row of query(db, "SELECT name FROM person ORDER BY id")) {
  console.log(render(row[0]));
}

close(db);
