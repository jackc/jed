import { close, create, execute, PrivilegeSet } from "jed-ts";

const db = create("app.jed");
execute(db, "CREATE TABLE report (id i32 PRIMARY KEY, body text)");
execute(db, "INSERT INTO report VALUES (1, 'hello')");

// Serve untrusted queries through a session granted ONLY read access: defaultPrivileges = {select}
// (a read-only envelope) with DDL disabled. The engine enforces this at name resolution — any write
// or schema change resolves to 42501, with no in-database role catalog.
const untrusted = db.newSession({
  defaultPrivileges: PrivilegeSet.empty().with("select"),
  allowDdl: false,
});
untrusted.execute(db, "SELECT body FROM report"); // ok
try {
  untrusted.execute(db, "DELETE FROM report");
} catch (e) {
  console.log("denied"); // 42501 permission denied for table report
}

// grant/revoke adjust one object at a time, and revoke always wins. Revoke EXECUTE on a volatile
// function to pin a session's determinism — calls to it then fail 42501.
db.revoke(PrivilegeSet.empty().with("execute"), "uuidv4");

close(db);
