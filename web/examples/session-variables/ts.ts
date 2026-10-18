import { createDatabase } from 'jed-ts';

const db = await createDatabase({ path: 'app.jed' });
const session = await db.session();
try {
  // Namespaced custom variables belong to this worker-owned session and survive
  // transaction rollback. Setters also return promises.
  await session.setVar('myapp.tenant', 'acme');
  const row = await session.get("SELECT current_setting('myapp.tenant') AS tenant");
  console.log('tenant:', row?.tenant);

  // Missing variables raise 42704; missing_ok=true returns SQL NULL instead.
  console.log(await session.get("SELECT current_setting('myapp.unset', true) AS missing"));

  // Cast to text inside SQL to render in the session's time zone. Raw timestamp
  // values retain their exact, zone-free bigint instant across the worker wire.
  await session.configure({ timeZone: '+05:00' }); // POSIX sign: UTC-5
  const at = await session.get("SELECT timestamptz '2024-01-15 12:00:00+00'::text AS at");
  console.log(at?.at); // 2024-01-15 07:00:00-05
} finally {
  await session.close();
  await db.close();
}
