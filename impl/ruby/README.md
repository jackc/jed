# jed — Ruby gem

A Ruby binding for [jed](../../README.md), an embeddable single-file SQL database with
**PostgreSQL behavior** and a strict, static type system. The gem **wraps the safe Rust core**
(CLAUDE.md §2/§13): the engine runs at Rust speed and conforms by construction. Design record:
[spec/design/ruby.md](../../spec/design/ruby.md).

## Quickstart

```ruby
require "jed"

# In-memory (block form auto-closes):
Jed.memory do |db|
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, name text, score f64)")
  db.execute("INSERT INTO t VALUES (1, 'alice', 9.5), (2, 'bob', 7.0), (3, NULL, 8.25)")

  res = db.query("SELECT id, name, score FROM t ORDER BY id")
  res.columns        # => ["id", "name", "score"]
  res.column_types   # => ["i32", "text", "f64"]
  res.each do |row|
    row[:id]         # => Integer
    row[:name]       # => String, or nil for SQL NULL
    row[:score]      # => Float
  end
  res.cost           # => deterministic execution cost (CLAUDE.md §13)
end

# File-backed (autocommit — each statement is durable on its own):
Jed.create("data.jed") do |db|
  db.execute("CREATE TABLE kv (k i32 PRIMARY KEY, v text)")
  db.execute("INSERT INTO kv VALUES (1, 'one')")
end
Jed.open("data.jed", read_only: true) do |db|
  db.query("SELECT v FROM kv WHERE k = 1").first[:v]   # => "one"
end
```

### Bind parameters (`$N`)

Pass values positionally for `$1`, `$2`, …; the engine type-checks each against its use site
before touching any row:

```ruby
db.execute("INSERT INTO t VALUES ($1, $2, $3)", 1, "alice", 9.5)
db.query("SELECT * FROM t WHERE id = $1 AND name = $2", 1, "alice")
db.execute("UPDATE t SET score = $1 WHERE id = $2", 10.0, 1)

vals = [2, "bob"]
db.query("SELECT * FROM t WHERE id = $1 AND name = $2", *vals)   # splat an array
```

Params are `nil` / `Integer` / `Float` / `true` / `false` / `String` (richer typed binds are a
follow-on). The usual SQL errors raise `Jed::Error` (e.g. an integer overflowing an `i16` column →
`22003`); a value the gem can't encode raises `ArgumentError`.

### Errors

A structured engine error raises `Jed::Error`, carrying the 5-char SQLSTATE:

```ruby
begin
  db.execute("INSERT INTO kv VALUES (1, 'dup')")   # primary-key clash
rescue Jed::Error => e
  e.sqlstate   # => "23505"
  e.message    # => "23505: ..."
end
```

### Values

Cells come back coerced to native Ruby (mirroring ActiveRecord's PostgreSQL adapter), with SQL
`NULL` always `nil` and anything without a faithful native type left as its canonical String:

| jed type                     | Ruby value                                          |
| ---------------------------- | --------------------------------------------------- |
| `i16` `i32` `i64`            | `Integer`                                           |
| `f32` `f64`                  | `Float` (incl. `Infinity`/`-Infinity`/`NaN`)        |
| `boolean`                    | `true` / `false`                                    |
| `decimal`                    | `BigDecimal` (exact)                                |
| `date`                       | `Date`, or `±Float::INFINITY` for `±infinity`       |
| `timestamp` `timestamptz`    | `Time` (UTC), or `±Float::INFINITY` for `±infinity` |
| NULL                         | `nil`                                               |
| `interval` `uuid` `bytea` …  | `String` (the engine's canonical rendering)         |

Bind params accept the same set in reverse — `nil`, `Integer`, `Float`, `true`/`false`, `String`,
`BigDecimal`, `Date`, and `Time`/`DateTime` — and the engine type-checks each against its column.
Like ActiveRecord, an infinite `date`/`timestamp` reads back as `±Float::INFINITY` (so those
columns are `Date|Float` / `Time|Float`); a zoneless `timestamp` and a `timestamptz` both read as a
UTC `Time`.

### Collation & time zones (host-loaded bundles)

The bare engine ships only `C` collation and `UTC` + fixed offsets. Load a Unicode bundle and/or an
IANA time-zone bundle to enable linguistic collations and named zones — process-global, so one call
covers every database:

```ruby
Jed.load_unicode_data(File.binread("unicode.jucd"))    # → COLLATE "unicode", ILIKE, case folding
Jed.load_time_zone_data(File.binread("tzdata.jtz"))     # → AT TIME ZONE 'America/New_York', date_trunc(…, zone)

Jed.memory do |db|
  db.query(%(SELECT 'a' < 'B' COLLATE "unicode"))                      # => true (UCA), vs false under C
  db.query(%(SELECT now() AT TIME ZONE 'America/New_York'))            # local wall-clock
end
```

A malformed bundle raises `Jed::Error`.

### Host functions

Register your own scalar functions, callable from SQL by name. Pass the registry when you open a
database; the handle keeps the functions registered up to that point.

```ruby
reg = Jed::ExtensionRegistry.new

# Single-row: called once per row with that row's arguments.
reg.register("add_tax", [:decimal, :decimal], :decimal, volatility: :immutable) do |amount, rate|
  amount * (1 + rate)
end

# Batch: one Array per argument, plus an output Array to append one result per row to.
reg.register_batch("score", [:i32, :text], :i64, volatility: :immutable, cost: 2) do |ids, names, out|
  ids.zip(names) { |id, name| out << (id * name.length) }
end

Jed.memory(extensions: reg) do |db|
  db.query("SELECT add_tax(19.99, 0.08)").first[0]   # => 0.215892e2
end
```

- Functions are **strict**: a NULL argument returns NULL without calling the block. Return `nil` for
  a NULL result.
- Arguments arrive as the same Ruby values a query returns (table above). The types a host function
  may take or return are `i16` `i32` `i64` `f32` `f64` `boolean` `decimal` `text` `date` `timestamp`
  `timestamptz`.
- Raising fails the statement: a `Jed::Error.new("22012", "…")` keeps its SQLSTATE; any other
  exception is `38000` with the exception as the `Jed::Error`'s `cause`. A result of the wrong class
  is `22000`.
- A batch block that raises after appending some results fails at the row after them, exactly as the
  single-row form would.
- `volatility:` is `:immutable`, `:stable`, or `:volatile` (the default). Only a non-volatile batch
  function is called with chunks of up to 1,024 rows; that is what makes batching pay off.
- `cost:` (default `1`) is charged per call against the engine's cost meter.
- A host function may not use the database handle that is calling it (`55006`); other handles are
  fine.

## Build & test (in-repo)

The native extension is a Rust `cdylib`. From the **repo root**:

```sh
mise run ruby:build # compile the cdylib to impl/ruby/ext/target/release
mise run ruby:test # build + run the minitest seam tests (also part of `mise run test` / `mise run ci`)
```

The gem locates the compiled library automatically; override with `JED_RUBY_LIB=/path/to/libjed_ruby.so`.

> **Packaging note.** This slice loads the cdylib built by `mise run ruby:build`. A self-contained
> `gem install`-able native gem (via `rb-sys` + precompiled platform gems) is a follow-on —
> see [spec/design/ruby.md](../../spec/design/ruby.md) §6 and the TODO Phase 9 entry.
