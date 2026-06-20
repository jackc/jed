import { close, create, EngineError } from "jed-ts";

const db = create("app.jed");

// Serve untrusted queries through a session bounded TWO ways:
//   maxCost         — a per-STATEMENT ceiling: one runaway query aborts 54P01.
//   lifetimeMaxCost — a per-SESSION budget: the session's cumulative cost is capped, so a flood of
//                     cheap queries can't burn unbounded CPU. It aborts 54P02.
const untrusted = db.newSession({ maxCost: 10000n, lifetimeMaxCost: 3n });

// Each statement accrues into the session's running total; read it with lifetimeCost().
untrusted.execute(db, "SELECT 1"); // cost 1 — cumulative 1
untrusted.execute(db, "SELECT 1"); // cost 1 — cumulative 2

// The third drives the cumulative to the budget — the in-flight statement aborts 54P02, and the
// partial cost still counts, so the session is now spent.
try {
  untrusted.execute(db, "SELECT 1");
} catch (e) {
  if (e instanceof EngineError) console.log("denied:", e.code()); // 54P02
}
console.log("spent:", untrusted.lifetimeCost()); // 3n — the budget

// Once spent, every further statement is rejected at admission — the session is done.
try {
  untrusted.execute(db, "SELECT 1");
} catch (e) {
  if (e instanceof EngineError) console.log("admission:", e.code()); // 54P02
}

close(db);
