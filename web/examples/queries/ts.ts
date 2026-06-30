import { openDatabase } from 'jed-ts';

const db = openDatabase('app.jed');

// run() binds native JS params to $1, $2, … and returns a command tag ({ changes, cost }). Values
// convert automatically: an integer-valued number → int, a bigint → int, a string → text, a
// Uint8Array → bytea, null → NULL. No hand-built Value.
const { changes } = db.run(
  'INSERT INTO account (id, name, balance) VALUES ($1, $2, $3)',
  1,
  'Ada',
  100
);
console.log(`inserted ${changes} row`);

// get() returns the first row as a plain object keyed by output column name (or undefined). Result
// values map int → bigint (i64 is exact — jed's identity), the other scalars to their JS type.
const row = db.get('SELECT balance FROM account WHERE id = $1', 1);
console.log(`balance = ${row?.balance}`); // 100n — a bigint

// prepare() returns a reusable Statement; all() materializes every row, *iterate() yields lazily.
const stmt = db.prepare('SELECT id, name FROM account ORDER BY id');
for (const acct of stmt.iterate()) {
  console.log(`${acct.id}: ${acct.name}`);
}

db.close();
