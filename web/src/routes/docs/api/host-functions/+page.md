<script>
	import CodeTabs from '$lib/components/CodeTabs.svelte';
</script>

<svelte:head>
	<title>Host functions — jed</title>
	<meta name="description" content="Register your own scalar functions over jed's built-in types: a frozen ExtensionRegistry supplied at open/create, resolved and evaluated beside the built-in catalog. Strict, exact-typed, cost-metered, with single-row or batch kernels." />
</svelte:head>

# Host functions

jed's built-in function catalog is **pure and side-effect-free** — that is what makes an untrusted
query safe to run. But an embedding host often has domain logic the engine will never ship: a pricing
rule, a scoring formula, a company-specific transform. You register those as **host scalar
functions** — your own code, callable from SQL by name, resolved and evaluated **beside** the
built-in catalog.

A host builds an **`ExtensionRegistry`**, adds functions to it, and passes it in the create/open
options. The engine **freezes the registry for the handle's lifetime** and shares it into every
session, so the set of functions is fixed once the handle is open — registering code never mutates
schema, and nothing about a host function is written to the file (a reopening host brings its own).

<CodeTabs topic="host-functions" />

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
  type system.

## Batch kernels

A kernel can take one row at a time, as above, or a **whole column of rows per call**. A batch
kernel receives its arguments **column-major** — `args[j][i]` is argument `j` of row `i`, never
NULL — and appends one result per row, in order, to an output column. To fail, it stops at the
failing row and raises; the results it already appended mark which row that was, so the error is
reported for exactly the row a one-row kernel would have failed on.

Both forms behave identically from SQL — same rows, same cost, same errors in the same order. The
difference is how often your code is entered: where jed already holds a chunk of rows (a projection
over a filtered or joined result), it calls a non-`volatile` batch kernel once per chunk of up to
1,024 rows, then replays the row-by-row evaluation against those results. That matters most when a
call crosses a language boundary — a binding that wraps jed in another language pays the crossing
once per chunk instead of once per row. A `volatile` function is always called one row at a time,
so its calls happen exactly as a one-row kernel's would. So is a function with **no arguments**: its
batch kernel receives no columns to learn a row count from, so jed always hands it exactly one row,
and it appends exactly one result.

Because jed may compute a chunk ahead of the rows it emits, a batch kernel can run on rows a query
never returns — the tail of a chunk after a `LIMIT` is satisfied, or after an earlier error. Your
kernel must be a plain per-row map: result `i` depends only on row `i`'s arguments. On a session
with a cost ceiling, the chunk is capped at what the remaining budget could pay for at the declared
`cost`, so the speculation stays bounded.

## Resolution: built-ins win

A host name — or a host **overload** of a built-in name over a *new* argument signature — resolves
**after** the built-in catalog. So a built-in always wins an exact-signature collision (registering a
host `abs(i64)` is accepted but never reached), and overloading is by signature: `discount(i64, i64)`
and `discount(text, text)` are two different functions under one name. A call that matches no
signature is `42883`, exactly as an unknown built-in.

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

The file records that dependency. On reopen, if your registry supplies a **different `component_id`**,
a **bumped `semantic_version`**, or **no such function**, the index is treated as **unusable**:
reads simply skip it (you still get correct rows from a heap scan) and any write that would maintain
it is refused (`XX002` on a mismatch, `42883` when the function is missing) — never a silently stale
result. A non-`immutable` or unversioned host function used in an index is rejected up front with
`42P17`. (Rebuild the index to adopt the new version.)

Registration stays a **host-API act, never SQL** — there is no `CREATE FUNCTION` statement, so an
untrusted query surface can never register or redefine host code. The registry itself is a handle
setting a reopening host brings; only the *dependency* of an index on a function is written to the
file.

## The boundary

A host kernel is **opaque** to the engine — it may compute anything, and jed cannot know whether it
touches the filesystem, the network, or burns CPU. So host functions are deliberately **outside** the
built-in untrusted-query safety guarantee: a host that exposes them to an adversarial query surface
owns that decision. The engine's one mechanical defense is the **cost gate** above — and it binds
only a function that declared its cost. That is the whole trade: jed gives you a clean, first-class
extension seam and a legible line where your code takes over.

The surface is still deliberately narrow — **strict** and **exact scalar signatures** (no implicit
promotion). Host functions in `DEFAULT`/`CHECK` columns, host *types*, and non-strict functions come
later, as does batching at more sites (filters, streaming scans, writes, index builds — today they
call a batch kernel with one row).
