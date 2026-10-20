import { createDatabase, EngineError } from 'jed-ts';

const db = await createDatabase({ path: 'app.jed' });

try {
  // Serve untrusted queries through a session with independent work and memory limits:
  //   maxCost         — a per-STATEMENT ceiling: one runaway query aborts 54P01.
  //   lifetimeMaxCost — a per-SESSION budget: the session's cumulative cost is capped, so a flood of
  //                     cheap queries can't burn unbounded CPU. It aborts 54P02.
  const untrusted = await db.session({
    maxCost: 10000n,
    lifetimeMaxCost: 3n,
    maxScalarBytes: 8n * 1024n * 1024n, // covered scalar allocations; not total heap
    maxQueryMemoryBytes: 64n * 1024n * 1024n // rows and operator state (54P05); not total heap
  });
  try {
    // Each statement accrues into the session's running total; read it with lifetimeCost().
    await untrusted.execute('SELECT 1'); // cost 1 — cumulative 1
    await untrusted.execute('SELECT 1'); // cost 1 — cumulative 2

    // The third drives the cumulative to the budget — the in-flight statement aborts 54P02, and the
    // partial cost still counts, so the session is now spent.
    try {
      await untrusted.execute('SELECT 1');
    } catch (e) {
      if (!(e instanceof EngineError) || e.code() !== '54P02') throw e;
      console.log('denied:', e.code()); // 54P02
    }
    console.log('spent:', await untrusted.lifetimeCost()); // 3n — the budget

    // Once spent, every further statement is rejected at admission — the session is done.
    try {
      await untrusted.execute('SELECT 1');
    } catch (e) {
      if (!(e instanceof EngineError) || e.code() !== '54P02') throw e;
      console.log('admission:', e.code()); // 54P02
    }
  } finally {
    await untrusted.close();
  }
} finally {
  await db.close();
}
