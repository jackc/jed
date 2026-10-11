<script>
	import CodeTabs from '$lib/components/CodeTabs.svelte';
</script>

<svelte:head>
	<title>Session variables — jed</title>
	<meta name="description" content="Carry per-session string settings on a jed session — PostgreSQL's GUC model: set them on the host API and read them in SQL with current_setting()." />
</svelte:head>

# Session variables

A jed **session** can carry named string settings — PostgreSQL's GUC model, scoped to the session.
The host sets them through the API; SQL reads them with **`current_setting()`**. They are a clean way
to thread per-request context (a tenant id, a request id, a feature flag) into the queries a session
runs, without weaving it through every statement's parameters.

## Setting and reading

A variable is a **string → string** pair (PostgreSQL settings are all text). A custom variable must be
**namespaced** — a dotted name like `myapp.tenant` — to stay distinct from built-in settings; a
non-dotted name is rejected with `42704`. Names are case-insensitive; values are kept verbatim.

<CodeTabs topic="session-variables" />

## Reading a variable in SQL

`current_setting('name')` returns the variable's value as text:

- An **unset** name raises **`42704`** (unrecognized configuration parameter).
- The two-argument form `current_setting('name', true)` passes **`missing_ok`** — an unset name
  returns **NULL** instead of raising.
- `current_setting` is **stable** and null-propagating, so it composes in ordinary expressions.

## They are session state, not data

Session variables live on the session, not in the database:

- They are **not** stored in the file, and they do **not** roll back when a transaction rolls back
  (PostgreSQL's `SET SESSION` behavior).
- Each session has its own independent set — a variable on one session is invisible to another.
- `reset_var(name)` clears one variable by name.

## The session time zone

The one built-in setting is the session **time zone**, `UTC` by default. Set it with
`set_time_zone` (Rust), `SetTimeZone` (Go), or `setTimeZone` (TypeScript) to `UTC`, a fixed offset
such as `+05:30` (POSIX sign: `+05:30` is UTC−5:30), or a named zone from a loaded time-zone
database; an unknown zone raises `22023`. The zone decides how a `timestamptz` is read without an
offset, how `date_trunc`, `EXTRACT`, and the datetime casts decompose it, and how it renders as text.

A query returns a `timestamptz` as a zone-free value. To show it the way PostgreSQL would, render it
through the session: `session.render(&value)` (Rust), `session.Render(value)` (Go), or
`session.render(value)` (TypeScript). This renders every `timestamptz` in the value — including one
inside an array, composite, or range — in the session's current zone, for example
`2024-01-15 07:00:00-05`. A `Value`'s own render is always UTC (`2024-01-15 12:00:00+00`). Both
spellings carry the offset, so either one reads back as the same instant.

> **Scope.** This is the v1 surface — the host API plus the `current_setting()` read. The SQL
> `SET` / `RESET` / `SHOW` grammar (including `SET TIME ZONE`) and transaction-scoped `SET LOCAL`
> variables are planned follow-ons.
