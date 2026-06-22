import { open, update, close } from 'jed-ts';

const db = open('bank.jed');

// update() runs a read-write transaction: every statement commits atomically, or — if the callback
// throws — none of them do. (begin()/tx.commit()/tx.rollback() is the explicit form.)
update(db, (tx) => {
  tx.execute('UPDATE account SET balance = balance - 100 WHERE id = 1');
  tx.execute('UPDATE account SET balance = balance + 100 WHERE id = 2');
});

close(db);
