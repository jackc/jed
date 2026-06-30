import { openDatabase } from 'jed-ts';

const db = openDatabase('bank.jed');

// update() runs a read-write transaction: it mints a session, runs the callback, commits on success,
// and rolls back if the callback throws — so the two writes are atomic. view() is the read-only
// sibling. (For an explicit block spanning calls, mint a session with db.session({}) and drive
// begin/commit/rollback on it.)
db.update((tx) => {
  tx.execute('UPDATE account SET balance = balance - 100 WHERE id = 1');
  tx.execute('UPDATE account SET balance = balance + 100 WHERE id = 2');
});

db.close();
