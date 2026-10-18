import { openDatabase, render } from 'jed-ts';

const db = await openDatabase('app.jed');
try {
  // Native JS parameters bind to $1, $2, …; integer results remain exact bigint.
  const { changes } = await db.run(
    'INSERT INTO account (id, name, balance) VALUES ($1, $2, $3)',
    1,
    'Ada',
    100
  );
  console.log(`inserted ${changes} row`);

  const row = await db.get('SELECT balance FROM account WHERE id = $1', 1);
  console.log(`balance = ${row?.balance}`);

  // Preparation lives in the worker. get/all/run accept native JS parameters;
  // query accepts typed Value[] and yields typed rows through async iteration.
  const stmt = await db.prepare('SELECT id, name FROM account ORDER BY id');
  try {
    for await (const acct of await stmt.query()) {
      console.log(`${render(acct[0])}: ${render(acct[1])}`);
    }
  } finally {
    await stmt.close();
  }
} finally {
  await db.close();
}
