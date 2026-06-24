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

## Build & test (in-repo)

The native extension is a Rust `cdylib`. From the **repo root**:

```sh
rake ruby:build    # compile the cdylib to impl/ruby/ext/target/release
rake ruby:test     # build + run the minitest seam tests (also part of `rake test` / `rake ci`)
```

The gem locates the compiled library automatically; override with `JED_RUBY_LIB=/path/to/libjed_ruby.so`.

> **Packaging note.** This slice loads the cdylib built by `rake ruby:build`. A self-contained
> `gem install`-able native gem (via `rb-sys` + precompiled platform gems) is a follow-on —
> see [spec/design/ruby.md](../../spec/design/ruby.md) §6 and the TODO Phase 9 entry.
