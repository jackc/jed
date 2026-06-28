import { openDatabase } from 'jed-ts';

const db = openDatabase('bank.jed');

// A transaction's writes are atomic. Open an explicit read-write block with begin(true), run the
// statements, and commit — or rollback to discard them all. (The TypeScript handle drives the block
// directly: there is no update()/view() closure helper as in Rust and Go.) If commit() is never
// reached, close() discards the open block.
db.begin(true);
try {
  db.execute('UPDATE account SET balance = balance - 100 WHERE id = 1');
  db.execute('UPDATE account SET balance = balance + 100 WHERE id = 2');
  db.commit();
} catch (e) {
  db.rollback();
  throw e;
}

db.close();
