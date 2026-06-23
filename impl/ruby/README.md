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

Cells come back coerced to native Ruby for the unambiguous scalars and as their canonical
String otherwise (lossless), with SQL `NULL` always `nil`:

| jed type            | Ruby value                                   |
| ------------------- | -------------------------------------------- |
| `i16` `i32` `i64`   | `Integer`                                    |
| `f32` `f64`         | `Float` (incl. `Infinity`/`-Infinity`/`NaN`) |
| `boolean`           | `true` / `false`                             |
| NULL                | `nil`                                        |
| everything else     | `String` (the engine's canonical rendering)  |

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
