import { createDatabase } from 'jed-ts';

const db = createDatabase({ path: 'app.jed' });

// Session variables are PostgreSQL's GUC model — they live on a SESSION, so mint one from the
// database rather than using the bare handle. A custom variable must be NAMESPACED — a dotted name
// like `myapp.tenant`; a non-dotted name is 42704.
const s = db.session({});
s.setVar('myapp.tenant', 'acme');

// Read it back through the host API — the name is case-insensitive; an unset name is undefined.
console.log('tenant:', s.var('myapp.tenant')); // acme

// ... or in SQL with current_setting(): `SELECT current_setting('myapp.tenant')` -> "acme".
s.query("SELECT current_setting('myapp.tenant')");

// An unset name is 42704, unless the two-arg form passes missing_ok = true, which returns NULL:
//   SELECT current_setting('myapp.unset')        -- 42704
//   SELECT current_setting('myapp.unset', true)  -- NULL

// Variables are SESSION state, not data — they do NOT roll back with a transaction. resetVar clears
// one by name.
s.resetVar('myapp.tenant');
s.close();

db.close();
