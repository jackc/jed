import { createDatabase, EngineError, PrivilegeSet } from 'jed-ts';

const db = await createDatabase({ path: 'app.jed' });
try {
  await db.execute('CREATE TABLE report (id i32 PRIMARY KEY, body text)');
  await db.execute("INSERT INTO report VALUES (1, 'hello')");

  // Serve untrusted queries through a SESSION granted ONLY read access: defaultPrivileges = {select}
  // (a read-only envelope) with DDL disabled. A session is a handle minted from the database that
  // shares its committed state. The engine enforces the envelope at name resolution — any write or
  // schema change resolves to 42501, with no in-database role catalog.
  const untrusted = await db.session({
    defaultPrivileges: PrivilegeSet.empty().with('select'),
    allowDdl: false
  });
  try {
    await untrusted.execute('SELECT body FROM report'); // ok
    try {
      await untrusted.execute('DELETE FROM report');
    } catch (e) {
      if (!(e instanceof EngineError) || e.code() !== '42501') throw e;
      console.log('denied'); // 42501 permission denied for table report
    }
  } finally {
    await untrusted.close(); // release the session (and its reader pin)
  }
} finally {
  await db.close();
}
