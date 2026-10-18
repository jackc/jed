<script>
	import CodeTabs from '$lib/components/CodeTabs.svelte';
</script>

<svelte:head>
	<title>Queries & parameters — jed</title>
	<meta name="description" content="Bind parameters and read typed result rows from jed — in each language's native idiom: rusqlite for Rust, database/sql for Go, an asynchronous worker API for TypeScript." />
</svelte:head>

# Queries & parameters

Running a query means two things: **binding parameters** into the SQL, and **reading the rows** back
out. jed gives each language an ergonomic layer for both — and deliberately **not** the same shape in
every language. Rust and Go use familiar embedded-SQL interfaces; TypeScript keeps database work off the application thread:

- **Rust** — [rusqlite](https://docs.rs/rusqlite)'s traits: `run`, `query_row`, `query_map`, with
  `ToValue` / `FromValue` doing the conversions and `row.get::<T>(…)` reading a typed column.
- **Go** — [`database/sql`](https://pkg.go.dev/database/sql) / [pgx](https://github.com/jackc/pgx):
  `Exec`, `Query`, `QueryRow` taking `...any` args, with `Scan(&dest)` and struct mapping.
- **TypeScript** — `await db.prepare(sql)` returns a worker-owned statement. Its `run`, `get`,
  and `all` methods return promises, with object rows for `get` and `all`. Use
  `for await (const row of await statement.query())` for bounded batches of typed values.

Use the **language selector** in the top bar to switch this example between the three.

<CodeTabs topic="queries" />

## Binding parameters

Parameters are positional `$1`, `$2`, … placeholders, bound left to right from the values you pass.
You pass **native values**, not engine `Value`s — the ergonomic layer converts them: integers,
floats, booleans, strings, byte arrays, and `NULL` all map across. This keeps user data **out of the
SQL string**, so there is no string-interpolation injection surface.

Each placeholder's static type comes from its SQL context: a compared column, an arithmetic sibling,
an `INSERT`/`UPDATE` target, a cast, the common result type of `CASE`, `COALESCE`, `GREATEST`, and
`LEAST`, or the argument type a function declares. For example, `COALESCE($1, 0)` infers an integer
parameter, `translate($1, 'a', 'b')` a text one, and `s LIKE $1` or `'x%' LIKE $1` a text pattern. A
bare `SELECT $1` has no type context and reports `42P18`, and a function whose overloads leave the type
open (`abs($1)`, `round($1, 2)`) reports `42725`; write an explicit cast when the surrounding expression
cannot supply one.

A note on integers, because it is the one place the type systems differ. jed integers are 64-bit and
**exact**. In Rust and Go that is the natural integer type. In TypeScript a `number` is a float, so
jed uses **`bigint`** for integer values — an integer-valued `number` like `1` still binds as an
integer (you write `run(1)`, not `run(1n)`), but values _read back_ come as `bigint` so a large
`i64` never loses precision.

An exact `decimal` binds from the engine's own `Decimal` type, which keeps its written scale; in Go,
`jed.ParseDecimal("12.50")` builds one from text. An integer or a `Decimal` also binds to a `f32`/`f64`
parameter, rounded to the nearest float.

### Arrays, ranges, and composite values

A parameter can also be an array, a range, or a composite type. It takes the container type it is
compared or assigned to: `tags @> $1` and `UPDATE t SET tags = $1` bind a `text[]`, `during && $1`
binds the range, `home = $1` binds the composite, and `$1::i32[]` binds an `i32[]`. `id = ANY($1)`
binds the array of `id`'s type, which is how you pass a list:

- **Rust** — wrap the list: `session.query_map("… WHERE id = ANY($1)", (jed::Array(vec![1, 3]),), …)`.
  A bare `Vec` is a list of separate parameters, and `Vec<u8>` binds as `bytea`.
- **Go** — pass a slice: `session.Query(ctx, "… WHERE id = ANY($1)", []int64{1, 3})`. A nil slice
  binds `NULL`; `[]byte` stays `bytea`.
- **TypeScript** — pass a JS array: `(await db.prepare("… WHERE id = ANY($1)")).all([1, 3])`.

On a primary key or an indexed column, `id = ANY($1)` looks up each element of the list, like
`id IN (1, 3)` ([EXPLAIN](/docs/sql/explain) shows it as an interval set). On a composite primary key,
`tenant = $1 AND id = ANY($2)` looks up each `(tenant, id)` pair, and so does an index on `(tenant, id)`.

Any container also binds from its text form, read like a string literal of that type: `'{1,3}'`,
`'[1,5)'`, or `'(1,x)'`. To bind a range or composite value directly, pass the engine `Value`.

## Reading rows

How a row arrives depends on the idiom:

- **Rust** hands each row to a closure as a `Row`; call `row.get::<T>(index)` or
  `row.get_by_name::<T>(name)` to pull a typed column. `query_row` returns `Option<T>` (`None` when
  nothing matched); `query_map` maps every row.
- **Go** scans columns into pointers with `Scan(&a, &b)`, or maps a whole row into a struct by column
  name with `RowToStructByName`. `QueryRow(...).Scan(...)` returns `jed.ErrNoRows` on an empty result.
- **TypeScript** returns each row as an **object keyed by output column name** — `get` gives the first
  (or `undefined`) and `all` gives an array, both through promises. `query` yields positional
  typed values through async iteration with bounded batch fetching.

### NULL

SQL `NULL` needs an explicit home, so it can't silently become a zero. Each layer has a nullable
target: Rust's **`Option<T>`** (a bare `T` rejects `NULL` with `22004`), Go's **`*jed.Null[T]`** (or
`*any`), and TypeScript's **`null`** in the result object. A column you expect to be nullable should
be read into one of these.

## Handling constraint errors

A failed statement raises an **`EngineError`** carrying a stable 5-char SQLSTATE (`.code()`) — never
match on the message text, which is informational. When the failure is a constraint or type
violation, the error also carries **structured diagnostic fields** (modeled on pgx's `PgError`) so you
can tell _which_ constraint fired without parsing the message: `constraint_name`, `table_name`,
`database_name`, `column_name`, and `data_type_name`. A field is absent (`None` / `""` / `undefined`)
when it does not apply. They are set on the integrity violations — `23505` unique, `23514` check,
`23503` foreign key, `23P01` exclusion (→ constraint + table); `23502` not-null (→ table + column) — and
the type errors `22003` / `22001` (→ data type). **`database_name`** (`DatabaseName` in Go,
`databaseName` in TypeScript) is jed's counterpart of PostgreSQL's schema name: whenever `table_name`
is set it names the database holding that table — `main`, `temp` for a temporary table, or the name a
host attached a database under.

```rust
// Rust — fields are `Option<String>` on the returned EngineError
if let Err(e) = session.run("INSERT INTO users (email) VALUES ($1)", ("a@b.com",)) {
    if e.code() == "23505" {
        // e.g. route the failure back to the offending form field
        eprintln!("duplicate on {:?}", e.constraint_name.as_deref()); // Some("users_email_key")
        eprintln!("{:?}", e.detail.as_deref()); // Some("Key (email)=(a@b.com) already exists.")
    }
}
```

```go
// Go — fields are strings ("" when unset); assert the *jed.EngineError
_, err := session.Exec(ctx, "INSERT INTO users (email) VALUES ($1)", "a@b.com")
var ee *jed.EngineError
if errors.As(err, &ee) && ee.Code() == "23505" {
    fmt.Println("duplicate on", ee.ConstraintName) // users_email_key
    fmt.Println(ee.Detail)                         // Key (email)=(a@b.com) already exists.
}
```

```ts
// TypeScript — fields are optional string properties on EngineError
try {
  await session.run("INSERT INTO users (email) VALUES ($1)", "a@b.com");
} catch (e) {
  if (e instanceof EngineError && e.code() === "23505") {
    console.log("duplicate on", e.constraintName); // users_email_key
    console.log(e.detail); // Key (email)=(a@b.com) already exists.
  }
}
```

A fifth field, **`detail`** (`Detail` in Go), carries PostgreSQL's DETAIL sentence naming the
offending values, so you can tell the user _which_ value was the problem without re-querying:

| Error | `detail` |
|---|---|
| `23505` unique | `Key (email)=(a@b.com) already exists.` |
| `23503` foreign key, insert/update side | `Key (customer_id)=(99) is not present in table "customers".` |
| `23503` foreign key, delete/update side | `Key (id)=(1) is still referenced from table "orders".` |
| `23P01` exclusion | `Key (span)=([3,7)) conflicts with existing key (span)=([1,5)).` |
| `23514` check, `23502` not null | `Failing row contains (4, null, -1).` |

Each value is rendered as a `SELECT` would show it, cut to 64 bytes (`...` marks a cut). A session
that lacks `SELECT` on the table gets no values: the field is absent, or for a foreign key the
value-free `Key is not present in table "customers".`. Like the other fields, `detail` is never
part of the rendered message.

## Pointing at the mistake: `position` and `hint`

For a query tool that shows errors to the person who typed the SQL, two more fields locate and
explain the mistake. **`position`** (`Position` in Go) is the 1-based **character** offset of the
offending token, as in PostgreSQL. It is set on syntax errors (`42601` and every other error the
parser raises, except the `54xxx` limits), on an unknown or ambiguous column reference (`42703`,
`42702`, `42P01`), on an unknown `INSERT` column or `SET` target, on an unknown FROM table or
`INSERT` / `UPDATE` / `DELETE` target (`42P01`), on an unknown function (`42883`, at its name), on an
operator with no implementation for its operand types (`42883` / `42804`, at the operator), on a
non-boolean `WHERE` / `HAVING` / `ON` / `AND` / `OR` / `NOT` / `CASE WHEN` argument and a value of the
wrong type for its column or for its `CASE` / `COALESCE` / `ARRAY` / `VALUES` / `UNION` siblings
(`42804`, at that expression), on an unknown type name (`42704`), on a cast with no conversion (at
the `::` or `CAST`), on a simple `CASE`'s comparison (at its `WHEN`), on a sort, grouping, or
`DISTINCT` key with no ordering or equality operator, on a recursive query's column type, and on a
malformed string literal converted at resolve time (`'abc'::int`, or an `INSERT … VALUES`
literal, `22P02` and the other input errors, at the literal). An error raised while running
the query — a cast of a column's value, say — has none. It
counts characters, not bytes, and a syntax error at end of input points one past the last
character. For `execute_script` it counts from the start of the whole script. It is absent
(`None` / `0` / `undefined`) otherwise. **`hint`** (`Hint`) carries PostgreSQL's suggestion, in its
exact words: for a misspelled column, a table name used in place of its alias, a column or table of
a FROM item to the left of a subquery not marked `LATERAL` (with a `detail` saying the item cannot be
referenced from there), a function or operator with no implementation for its argument types, a
value of the wrong type for its column, a collation conflict, a write to a `GENERATED ALWAYS`
identity column, and a `DROP` that other objects depend on:

| Statement | `code` | `position` | `hint` |
|---|---|---|---|
| `SELECT 1 +` | `42601` | `11` | |
| `SELECT nofunc(1)` | `42883` | `8` | `No function matches the given name and argument types. You might need to add explicit type casts.` |
| `SELECT 'abc'::int` | `22P02` | `8` | |
| `SELECT nme FROM users` | `42703` | `8` | `Perhaps you meant to reference the column "users.name".` |
| `SELECT users.id FROM users u` | `42P01` | `8` | `Perhaps you meant to reference the table alias "u".` |
| `SELECT * FROM users, (SELECT id) s` | `42703` | `30` | `To reference that column, you must mark this subquery with LATERAL.` |

```ts
// TypeScript: underline the offending character. Positions count code points, so index
// Array.from(sql), not the UTF-16 string.
try {
  await session.execute(sql);
} catch (e) {
  if (e instanceof EngineError && e.position !== undefined) {
    const chars = Array.from(sql);
    const at = e.position - 1;
    const lineStart = at === 0 ? 0 : chars.lastIndexOf("\n", at - 1) + 1;
    const lineEnd = chars.indexOf("\n", at);
    console.log(chars.slice(lineStart, lineEnd < 0 ? chars.length : lineEnd).join(""));
    console.log(" ".repeat(at - lineStart) + "^");
    if (e.hint) console.log("HINT: " + e.hint);
  }
}
```

The `jed` CLI prints both the way psql does: a `LINE n:` excerpt with a `^` under the position, then
`HINT:  …`. When the error's table is in an attached database or `temp`, a `DATABASE:  …` line follows.

The rendered error message is unchanged — these fields are additive metadata alongside it.

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

## Prepared statements

For a hot statement — a point lookup run thousands of times — jed offers an explicit **prepared
statement**: the SQL is parsed once, and its **resolved query plan is cached** and reused across
executes (planning dominates the latency of a trivial lookup, so this is the single biggest win for
high-frequency queries). Rust and Go prepared statements can be passed to the handle that should execute them.
TypeScript prepared statements stay in their execution worker and retain the database, session,
or transaction that prepared them; explicitly close them when finished.

- **Rust** — `db.prepare(sql)` → `session.query_prepared(&stmt, &params)` /
  `session.execute_prepared(&stmt, &params)` (also on `Database` and `Transaction`).
- **Go** — `db.Prepare(sql)` → `session.QueryPrepared(ctx, stmt, args...)` /
  `ExecPrepared` / `QueryRowPrepared` (also on `Database` and `Transaction`; the statement is safe
  to share across goroutines, each running it on its own session).
- **TypeScript** — `await session.prepare(sql)` → `await stmt.query(params)` or
  `await stmt.execute(params)` for typed parameters. `stmt.get`, `stmt.all`, and `stmt.run` accept
  native JavaScript parameters. `await stmt.batch(bindings)` amortizes messaging across repeated
  executions without changing the surrounding transaction boundary. Finish with `await stmt.close()`.

The cache is transparent and always correct: a schema change (`CREATE`/`DROP`/`ALTER`) invalidates
it and the next execute re-plans; reusing a cached plan is by construction result- and
cost-identical to planning fresh, so a prepared statement never changes what a query returns — only
how fast it gets there.

### Plans see your values

Every execution is planned **as if its bound values were literals** — its *custom plan*. After
[`ANALYZE`](/docs/sql/statistics), `WHERE status = $1` uses the most-common-value list and histogram
for the value you bind, exactly as `WHERE status = 'shipped'` would: a value in nearly every row can
choose a full scan while a rare one takes the index, from one prepared statement. The first planning
works out whether any estimate actually depends on a value:

- **Value-blind** statements — a primary-key or unique lookup like `WHERE id = $1`, or parameters
  that reach no statistics — keep one cached plan, so a hot point lookup still skips planning.
- **Parameter-sensitive** statements plan once per execution with their values, and remember the
  plans of up to 8 recent bindings, so a value you bind repeatedly is not planned again.

Unlike PostgreSQL, a statement never switches to a "generic" plan after some number of executions,
and there is no `plan_cache_mode`: the plan of an execution depends only on the statement, its
values, and the database — never on which values earlier runs happened to bind. `EXPLAIN` accepts
bound values and shows the plan they get.

## Cancellation

Go's `Query`, `QueryPrepared`, `Exec`, and `QueryRow` accept a `context.Context` on database,
session, and transaction handles. Keep that context alive while consuming the rows: cancellation
also interrupts engine work inside a long `Rows.Next()` after `Query` has returned. Check
`Rows.Err()` for SQLSTATE `57014`, and `defer rows.Close()` when you might stop early. Exhaustion
and terminal errors release the cursor automatically.

Rust's `query_cancelable` and `execute_cancelable` accept a clonable `CancellationToken`; another
thread can call `cancel()` while the query runs. After manually draining Rust rows, call `error()`
to check for an execution error.

Cancellation during execution fails an open transaction, which must then be rolled back. It is
best effort, checked periodically during scans, joins, and other execution loops rather than at\nevery expression. There is no fixed wall-clock response guarantee: sorting, individual scalar kernels,
host callbacks, and commit work can delay observation. Cancellation does not change completed-query
cost totals or deterministic cost limits.
