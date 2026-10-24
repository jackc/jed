<script>
	import CodeTabs from '$lib/components/CodeTabs.svelte';
</script>

<svelte:head>
	<title>Host functions — jed</title>
	<meta name="description" content="Register your own scalar functions over jed's built-in types: registered on the immutable Environment supplied at open/create, resolved and evaluated beside the built-in catalog. Strict, exact-typed, cost-metered, with single-row or batch kernels." />
</svelte:head>

# Host functions

jed's built-in function catalog is **pure and side-effect-free** — that is what makes an untrusted
query safe to run. But an embedding host often has domain logic the engine will never ship: a pricing
rule, a scoring formula, a company-specific transform. You register those as **host scalar
functions** — your own code, callable from SQL by name, resolved and evaluated **beside** the
built-in catalog.

A host registers functions on an **environment builder**, builds the **`Environment`**, and passes it
in the create/open options. An environment is immutable — it also carries the loaded Unicode and
time-zone data and the random and clock sources — so the set of functions is fixed for the handle's
lifetime and shared into every session. One environment can serve many databases. Registering code never mutates
schema, and the function itself is never written to the file: a reopening host brings its own. Only
an index or `CHECK` that uses a host function records which implementation it was built against
(below).

For TypeScript, register functions in a module that exports `configure(builder)`, as shown below.
Registration runs inside the database's worker. Node loads the module from the trusted file URL
passed as `extensionModule` to `await createDatabase(...)` or `await openDatabase(...)`.
Browser applications import it into a custom worker, using the public helper shown below.
Application closures and `Environment` instances cannot cross the worker boundary.

<CodeTabs topic="host-functions" />

## Browser registration

Import your registration module into a worker and call the startup helper at the module's top
level:

```ts
// database-worker.ts
import { startDatabaseWorker } from 'jed-ts/browser/worker';
import { configure } from './discount.ts';

startDatabaseWorker({ configure });
```

Supply a factory when creating or opening the database. Keep `new Worker(new URL(...))` directly
in the factory so your bundler can discover the worker entry point:

```ts
import { createDatabase, openDatabase } from 'jed-ts/browser';

const workerFactory = () =>
  new Worker(new URL('./database-worker.ts', import.meta.url), { type: 'module' });

const db = await createDatabase({ path: 'shop.jed', workerFactory });
try {
  console.log(await db.get('SELECT discount(1250, 15) AS sale')); // { sale: 1063n }
} finally {
  await db.close();
}

// Bring the same registrations when reopening the file.
const reopened = await openDatabase('shop.jed', { workerFactory });
await reopened.close();
```

The factory must create a **fresh dedicated worker** for each database handle. Jed owns it,
terminates it after close or initialization failure, and rejects reuse with `PROTOCOL`.
Omitting `workerFactory` uses the ordinary package worker with no custom functions.

Call `startDatabaseWorker` once, synchronously. It installs the message listener immediately and
awaits `configure(builder)` before building the environment and opening storage. Configuration
can load data asynchronously; scalar and batch SQL kernels remain synchronous. A configuration
error rejects `createDatabase` or `openDatabase` and releases the worker.

The `jed-ts/browser/worker` entry point exports `EnvironmentBuilder`, `HostFunctionSpec`,
`HostKernel`, `HostBatchKernel`, `Value`, `intValue`, and `Decimal` for registration code. A module
using these imports can also serve Node's `extensionModule`, provided its other imports support
both hosts. Importing the entry point alone does not start a worker.

Functions can capture variables in their **worker module**. They cannot capture variables from
the application thread, and changing an application variable does not change worker state. Pass
changing inputs as SQL parameters. Calls run directly in the worker, without a callback to the
application thread.

## A function's shape

Each registered function carries a name, an **exact scalar argument signature**, a scalar result
type, a kernel, and three declarations:

- **`cost`** — a non-negative static weight, charged once per call. This is the load-bearing one: it
  is **guarded against a session's `max_cost`**, so a heavy host function aborts `54P01` *before its
  kernel runs*. This admits the declared work under the [cost ceiling](../resource-limits/); the host
  remains responsible for the kernel's actual work and memory use. (Defaults to `1`.)
- **`volatility`** — `immutable` / `stable` / `volatile` (PostgreSQL's notion). An **`immutable`**
  function may back a persisted index (below); `stable`/`volatile` may not. Defaults to `volatile`
  (the safe assumption).
- **`cross_core`** — whether the function's results are byte-identical on every core. Recorded for the
  determinism ledger; not yet enforced. Defaults to `false`.

Two behaviors are guaranteed and free:

- **Strict.** A NULL argument short-circuits to a NULL result **before the kernel runs** — the kernel
  never sees a NULL, so it can read its arguments as concrete typed values.
- **Result-type checked.** A kernel that returns a value not matching its declared result type is
  caught (`22000`), so a misbehaving host function cannot leak a wrong-typed value into jed's strict
  type system. An integer result must also fit its declared width: a function declared to return
  `i32` that returns `2^40` raises `22003` (`value out of range for type i32`) on that row.

## Batch kernels

A kernel can take one row at a time, as above, or a **whole column of rows per call**. A batch
kernel receives its arguments **column-major** — `args[j][i]` is argument `j` of row `i`, never
NULL — and appends one result per row, in order, to an output column. To fail, it stops at the
failing row and raises; the results it already appended mark which row that was, so the error is
reported for exactly the row a one-row kernel would have failed on.

Both forms behave identically from SQL — same rows, same cost, same errors in the same order. The
difference is how often your code is entered: jed calls a non-`volatile` batch kernel once per chunk
of rows, then replays the row-by-row evaluation against those results. It does this

- in a **projection** over a filtered, joined, grouped, or sorted result, up to 1,024 rows at a
  time;
- in a **streaming primary-key scan** — `SELECT f(x) FROM t ORDER BY id`, a key range, or a key
  `IN` list — for both the projection and the `WHERE` clause (jed reads up to 64 rows ahead of the
  scan to form the chunk);
- in the output of a **streaming sort** (`ORDER BY` a non-key column), also up to 64 rows at a time.

A call is batched when it is a projection item or `WHERE` predicate itself, or sits beneath only
casts, arithmetic, comparisons, unary minus, `NOT`, or `IS [NOT] NULL` — `f(x) + 1` and
`WHERE f(x) > 10` batch — and its arguments are columns, constants, or parameters. A call under
`CASE`, `COALESCE`, `AND`/`OR`, or another function's arguments is called one row at a time. That
matters most when a call crosses a language boundary — a binding that wraps jed in another language
pays the crossing once per chunk instead of once per row. A single-row kernel is always called one row
at a time, exactly on the rows the query evaluates — batching it would save nothing — and so is a
`volatile` batch kernel, so its calls happen exactly as a one-row kernel's would. So is a function
with **no arguments**: its batch kernel receives no columns to learn a row count from, so jed always
hands it exactly one row, and it appends exactly one result.

Because jed may compute a chunk ahead of the rows it emits, a batch kernel can run on rows a query
never returns — the tail of a chunk after a `LIMIT` is satisfied, a row the `WHERE` clause rejects,
or a row after an earlier error. Your kernel must be a plain per-row map: result `i` depends only on
row `i`'s arguments; an error it raises for a row the query never evaluates is discarded. With a
`LIMIT`, the read-ahead never goes past the rows the limit can still return. On a session with a
cost ceiling, the chunk is capped at what the remaining budget could pay for at the declared `cost`,
so the speculation stays bounded.

## Resolution: built-ins win

A host name — or a host **overload** of a built-in name over a *new* argument signature — resolves
**after** the built-in catalog. So a built-in always wins an exact-signature collision (registering a
host `abs(i64)` is accepted but never reached), and overloading is by signature: `discount(i64, i64)`
and `discount(text, text)` are two different functions under one name.

A call that matches no signature exactly goes through the same implicit argument coercion built-ins
use. jed picks the host function (or built-in) whose arguments need the cheapest conversions:

- an integer widens to a wider integer (`i16 → i32 → i64`), the nearest width first;
- an integer becomes `numeric` (cost more than any integer widening);
- a number literal can become a float; an integer or `numeric` *column* never does;
- an `f32` widens to `f64`.

So a host `discount(i64, i64)` also takes `i32` columns, and a host `scale_by(numeric)` takes
`scale_by(3)`. Your kernel always receives values of the types it declared: jed converts the
arguments first. A built-in wins a tie, two host functions that tie are `42725`, and a call no
conversion can reach is `42883`, exactly as an unknown built-in. A bare `NULL` or `$1` never matches
a host signature.

The registered name is used exactly as given. SQL folds a bare name to lowercase, so register names
in lowercase: a function registered as `discount` is called as `discount(…)` or `DISCOUNT(…)`, but
one registered as `Discount` can only be called with quotes, as `"Discount"(…)`.

Registration itself rejects three mistakes up front: a **negative cost** is `22023`, an **unknown
type name** is `42704` (Go and TypeScript name argument types by string), and a **second function
with an identical `(name, arg-types)` signature** is `42723`.

## Backing an index

An `immutable` host function may be the key (or partial-`WHERE` predicate) of an index —
`CREATE INDEX ON t (geo_hash(location))` — so a derived value is indexed without any extra machinery.
Because the same SQL text re-binds to *your* code on reopen, an index like this must be able to tell
whether the code still matches. So an index-backing function declares two more things:

- **`component_id`** — a stable string naming the *implementation* (e.g. `"com.example/geo_hash"`),
  independent of the SQL name.
- **`semantic_version`** — a number you bump whenever a change to the results would invalidate keys
  built from the old code.

The file records that dependency. On reopen, if your environment supplies a **different `component_id`**,
a **bumped `semantic_version`**, or **no such function**, the index is treated as **unusable**:
reads simply skip it (you still get correct rows from a heap scan) and any write that would maintain
it is refused (`XX002` on a mismatch, `42883` when the function is missing) — never a silently stale
result. A non-`immutable` or unversioned host function used in an index is rejected up front with
`42P17`. To adopt the new version, rebuild the index with
[`REINDEX`](../../sql/indexes/#reindex) — `REINDEX INDEX geo_idx` (or
`REINDEX TABLE`, `REINDEX DATABASE`) re-resolves the expression against your current functions,
records their new identity, and rebuilds the keys, all or nothing.

## In `DEFAULT` and `CHECK`

A host function can supply a column default or enforce a check:

```sql
CREATE TABLE items (
  id  i64 PRIMARY KEY DEFAULT next_item_id(),
  sku text CHECK (valid_sku(sku))
);
```

The two are treated differently, because they relate differently to stored rows:

- **A `DEFAULT`** runs once, when a row is written, and the value it produces is ordinary data.
  Changing the function later affects only new rows. So any host function may be a default (any
  volatility, no `component_id` needed), and nothing about it is recorded in the file. A statement
  calls the function only if it actually uses the default: an `INSERT` that supplies the column, or
  an `UPDATE` that doesn't assign `DEFAULT`, works even when the host hasn't registered it. One
  that needs the default gets `42883`.
- **A `CHECK`** is a promise about every stored row, so a changed function could leave old rows
  silently violating it. A check's host function must declare a **`component_id`** (`42P17`
  otherwise), and the file records each one's identity and `semantic_version`, as for an index. Any
  volatility is allowed. On reopen with a **different `component_id`** or **bumped
  `semantic_version`**, every statement that would evaluate the check (`INSERT`, `UPDATE`, a
  foreign-key action that updates the table) is refused with `XX002` before writing anything. With
  the function missing, those statements get `42883`. Reads and `DELETE` never evaluate a check,
  so they keep working.

To adopt a new version, re-add the check in one statement. It re-checks every existing row with the
current code (failing with `23514`, and changing nothing, if one no longer passes) and records the
new version:

```sql
ALTER TABLE items DROP CONSTRAINT items_sku_check, ADD CONSTRAINT items_sku_check CHECK (valid_sku(sku));
```

Registration stays a **host-API act, never SQL** — there is no `CREATE FUNCTION` statement, so an
untrusted query surface can never register or redefine host code. The environment itself is a host
input a reopening host brings; only the *dependency* of an index or check on a function is
written to the file.

## The boundary

A host kernel is **opaque** to the engine — it may compute anything, and jed cannot know whether it
touches the filesystem, the network, or burns CPU. So host functions are deliberately **outside** the
built-in untrusted-query safety guarantee: a host that exposes them to an adversarial query surface
owns that decision. The engine's one mechanical defense is the **cost gate** above — and it binds
only a function that declared its cost. That is the whole trade: jed gives you a clean, first-class
extension seam and a legible line where your code takes over.

The surface is still deliberately narrow — **strict**, with **scalar signatures** only. Host *types* and non-strict functions come later, as does batching at more sites (filters inside joins and blocking scans, aggregate
arguments, writes, index builds — today they call a batch kernel with one row).
