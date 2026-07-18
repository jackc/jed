<script>
	import CodeTabs from '$lib/components/CodeTabs.svelte';
</script>

<svelte:head>
	<title>Host functions — jed</title>
	<meta name="description" content="Register your own scalar functions over jed's built-in types: a frozen ExtensionRegistry supplied at open/create, resolved and evaluated beside the built-in catalog. Strict, exact-typed, cost-metered." />
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
  kernel runs*. It is how a host function stays inside the [resource-limit](../resource-limits/)
  bound. (Defaults to `1`.)
- **`volatility`** — `immutable` / `stable` / `volatile` (PostgreSQL's notion). Recorded
  forward-compat; only `immutable` will later admit constant-folding or backing an index expression.
  Defaults to `volatile` (the safe assumption: the planner folds nothing).
- **`cross_core`** — whether the function's results are byte-identical on every core. Recorded for the
  determinism ledger; not yet enforced. Defaults to `false`.

Two behaviors are guaranteed and free:

- **Strict.** A NULL argument short-circuits to a NULL result **before the kernel runs** — the kernel
  never sees a NULL, so it can read its arguments as concrete typed values.
- **Result-type checked.** A kernel that returns a value not matching its declared result type is
  caught (`22000`), so a misbehaving host function cannot leak a wrong-typed value into jed's strict
  type system.

## Resolution: built-ins win

A host name — or a host **overload** of a built-in name over a *new* argument signature — resolves
**after** the built-in catalog. So a built-in always wins an exact-signature collision (registering a
host `abs(i64)` is accepted but never reached), and overloading is by signature: `discount(i64, i64)`
and `discount(text, text)` are two different functions under one name. A call that matches no
signature is `42883`, exactly as an unknown built-in.

Registration itself rejects three mistakes up front: a **negative cost** is `22023`, an **unknown
type name** is `42704` (Go and TypeScript name argument types by string), and a **second function
with an identical `(name, arg-types)` signature** is `42723`.

## The boundary

A host kernel is **opaque** to the engine — it may compute anything, and jed cannot know whether it
touches the filesystem, the network, or burns CPU. So host functions are deliberately **outside** the
built-in untrusted-query safety guarantee: a host that exposes them to an adversarial query surface
owns that decision. The engine's one mechanical defense is the **cost gate** above — and it binds
only a function that declared its cost. That is the whole trade: jed gives you a clean, first-class
extension seam and a legible line where your code takes over.

This slice is deliberately narrow — **ephemeral** (no persisted use, no `CREATE FUNCTION` DDL yet),
**strict**, **exact scalar signatures** (no implicit promotion), and **single-row** kernels (the
vectorized/batched ABI is a follow-on). Host types, non-strict functions, and catalog-bound versioned
functions come later.
