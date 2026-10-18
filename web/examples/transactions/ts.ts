import { openDatabase } from 'jed-ts';

const db = await openDatabase('bank.jed');
try {
  // The worker owns this transaction throughout the callback's awaits. It
  // commits on success and rolls back if the callback throws. view() is read-only.
  await db.update(async (tx) => {
    await tx.execute('UPDATE account SET balance = balance - 100 WHERE id = 1');
    await tx.execute('UPDATE account SET balance = balance + 100 WHERE id = 2');
  });

  // A batch preserves its surrounding transaction boundary. On the database
  // handle each statement autocommits; use tx.batch(...) inside update for an
  // atomic batch. Empty transactions do not rewrite storage or issue fsync.
} finally {
  await db.close();
}
