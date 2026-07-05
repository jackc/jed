<script>
	import CodeTabs from '$lib/components/CodeTabs.svelte';
</script>

<svelte:head>
	<title>Queries & parameters — jed</title>
	<meta name="description" content="Bind parameters and read typed result rows from jed — in each language's native idiom: rusqlite for Rust, database/sql for Go, better-sqlite3 for TypeScript." />
</svelte:head>

# Queries & parameters

Running a query means two things: **binding parameters** into the SQL, and **reading the rows** back
out. jed gives each language an ergonomic layer for both — and deliberately **not** the same shape in
every language. Each core adopts its ecosystem's _de facto_ embedded-SQL idiom, so the code feels
native rather than translated:

- **Rust** — [rusqlite](https://docs.rs/rusqlite)'s traits: `run`, `query_row`, `query_map`, with
  `ToValue` / `FromValue` doing the conversions and `row.get::<T>(…)` reading a typed column.
- **Go** — [`database/sql`](https://pkg.go.dev/database/sql) / [pgx](https://github.com/jackc/pgx):
  `Exec`, `Query`, `QueryRow` taking `...any` args, with `Scan(&dest)` and struct mapping.
- **TypeScript** — [better-sqlite3](https://github.com/WiseLibs/better-sqlite3): `db.prepare(sql)`
  returns a `Statement` with `run` / `get` / `all` / `iterate`, and rows come back as plain objects.

Use the **language selector** in the top bar to switch this example between the three.

<CodeTabs topic="queries" />

## Binding parameters

Parameters are positional `$1`, `$2`, … placeholders, bound left to right from the values you pass.
You pass **native values**, not engine `Value`s — the ergonomic layer converts them: integers,
floats, booleans, strings, byte arrays, and `NULL` all map across. This keeps user data **out of the
SQL string**, so there is no string-interpolation injection surface.

A note on integers, because it is the one place the type systems differ. jed integers are 64-bit and
**exact**. In Rust and Go that is the natural integer type. In TypeScript a `number` is a float, so
jed uses **`bigint`** for integer values — an integer-valued `number` like `1` still binds as an
integer (you write `run(1)`, not `run(1n)`), but values _read back_ come as `bigint` so a large
`i64` never loses precision.

## Reading rows

How a row arrives depends on the idiom:

- **Rust** hands each row to a closure as a `Row`; call `row.get::<T>(index)` or
  `row.get_by_name::<T>(name)` to pull a typed column. `query_row` returns `Option<T>` (`None` when
  nothing matched); `query_map` maps every row.
- **Go** scans columns into pointers with `Scan(&a, &b)`, or maps a whole row into a struct by column
  name with `RowToStructByName`. `QueryRow(...).Scan(...)` returns `jed.ErrNoRows` on an empty result.
- **TypeScript** returns each row as an **object keyed by output column name** — `get` gives the first
  (or `undefined`), `all` gives an array, `iterate` yields them lazily.

### NULL

SQL `NULL` needs an explicit home, so it can't silently become a zero. Each layer has a nullable
target: Rust's **`Option<T>`** (a bare `T` rejects `NULL` with `22004`), Go's **`*jed.Null[T]`** (or
`*any`), and TypeScript's **`null`** in the result object. A column you expect to be nullable should
be read into one of these.

## The raw `Value` path is still there

These ergonomic methods are **additive** — a thin, idiomatic layer over the lower-level path that
speaks jed `Value`s directly. Rust and TypeScript expose that path as dedicated methods: `execute` /
`query` taking `&[Value]` in Rust and `Value[]` in TypeScript. Go needs no separate method — a raw
`jed.Value` passes straight through the same variadic `Query` / `Exec` / `QueryRow` args (mixed freely
with native Go values), and `Rows.Value(i)` reads a column back as its engine `Value`. Either way the
raw path stays available for full fidelity: a rich type with no clean native counterpart in your
language (a `range`, a `jsonb`, a composite) round-trips losslessly as a `Value`, where the ergonomic
layer renders it to its canonical text. Reach for the raw `Value` when you need the engine value
itself; reach for the native ergonomic form — the recommended default — for everything else.
