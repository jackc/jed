import { close, create, query } from "jed-ts";

const db = create("app.jed");

// Session variables are PostgreSQL's GUC model, scoped to the session: a string→string map the host
// sets and SQL reads with current_setting(). A custom variable must be NAMESPACED — a dotted name
// like `myapp.tenant`; a non-dotted name is 42704.
db.setVar("myapp.tenant", "acme");

// Read it back through the host API — the name is case-insensitive; an unset name is undefined.
console.log("tenant:", db.var("myapp.tenant")); // acme

// ... or in SQL with current_setting(): `SELECT current_setting('myapp.tenant')` -> "acme".
query(db, "SELECT current_setting('myapp.tenant')");

// An unset name is 42704, unless the two-arg form passes missing_ok = true, which returns NULL:
//   SELECT current_setting('myapp.unset')        -- 42704
//   SELECT current_setting('myapp.unset', true)  -- NULL

// Variables are SESSION state, not data — they do NOT roll back with a transaction. resetVar clears
// one; resetVars() clears them all (PostgreSQL's RESET ALL).
db.resetVar("myapp.tenant");

close(db);
