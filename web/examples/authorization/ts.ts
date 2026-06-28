import { createDatabase, PrivilegeSet } from 'jed-ts';

const db = createDatabase('app.jed');
db.execute('CREATE TABLE report (id i32 PRIMARY KEY, body text)');
db.execute("INSERT INTO report VALUES (1, 'hello')");

// Serve untrusted queries through a SESSION granted ONLY read access: defaultPrivileges = {select}
// (a read-only envelope) with DDL disabled. A session is a handle minted from the database that
// shares its committed state. The engine enforces the envelope at name resolution — any write or
// schema change resolves to 42501, with no in-database role catalog.
const untrusted = db.session({
  defaultPrivileges: PrivilegeSet.empty().with('select'),
  allowDdl: false
});
untrusted.execute('SELECT body FROM report'); // ok
try {
  untrusted.execute('DELETE FROM report');
} catch (e) {
  console.log('denied'); // 42501 permission denied for table report
}
untrusted.close(); // release the session (and its reader pin)

// grant/revoke adjust one object at a time, and revoke always wins. Revoke EXECUTE on a volatile
// function to pin a session's determinism — calls to it then fail 42501.
db.revoke(PrivilegeSet.empty().with('execute'), 'uuidv4');

db.close();
