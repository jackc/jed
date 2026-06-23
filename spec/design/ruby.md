# The jed Ruby gem — design

A Ruby binding for jed, shipped as a gem. It **wraps the safe Rust core** (`impl/rust`) rather
than reimplementing the engine — the language-reach decision [cores.md](cores.md) §6 records
("ship Ruby … as a wrapper, gem → Rust") and CLAUDE.md §2 blesses. The gem runs the engine at
Rust speed and **conforms by construction**: it *is* the Rust core behind a thin C ABI, so it
surfaces zero new semantic divergence (cores.md §1).

## 1. Scope & non-goals

The gem is a **host artifact**, not a core (CLAUDE.md §2). It links the Rust core through a
small C ABI and adds **no engine behavior**. It **conforms to nothing and votes on nothing** —
the conformance corpus binds the *engine*; a wrap can only echo Rust's answers, never disagree
(cores.md §1). It is therefore **not** an independent conformance voice and is absent from the
differential set (Rust + Go + TS); its value is *reach* — giving Ruby programs a first-class,
idiomatic jed — not spec-hardening.

This is distinct from the **Ruby file-format reference** ([spec/fileformat/verify.rb](../fileformat/verify.rb)):
that is an independent fourth encoder/decoder of the on-disk format, hand-written to cross-check
the golden fixtures (`rust == go == ts == ruby`, format.md §1). The gem here is a *shippable
binding over the whole engine*; the reference is a *test oracle for the byte format*. They share
a language and nothing else.

Non-goals: no server / wire protocol (jed is embedded); no SQL dialect of its own (statements
pass through verbatim — the engine's grammar is the only dialect); no second implementation of
any engine logic.

## 2. Surface

The gem's public surface is `Jed` + `Jed::Database`, `Jed::Result` / `Jed::Row`, and
`Jed::Error`. It mirrors the Rust embedding API ([api.md](api.md)) in Ruby idiom:

```ruby
require "jed"

Jed.memory do |db|                                  # also: Jed.create(path), Jed.open(path, read_only:)
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, name text)")
  db.execute("INSERT INTO t VALUES (1, 'alice'), (2, NULL)")
  res = db.query("SELECT id, name FROM t ORDER BY id")
  res.each { |row| puts "#{row[:id]}: #{row[:name].inspect}" } # 1: "alice" / 2: nil
  res.cost                                                     # deterministic execution cost (§13)
end
```

- **`Database#execute(sql, *params)`** → a `Jed::Result` for a query, or `{ rows_affected:, cost: }`
  for DDL/DML. **`#query(sql, *params)`** always returns a `Jed::Result` (raises if the statement
  produces no rows). `$N` placeholders bind to the positional `params` (`$1` ⇒ the first); pass an
  array with the splat (`db.execute(sql, *vals)`). **`#commit`** publishes an explicit transaction
  (a no-op success under autocommit).
  **`#close`** releases the handle (rolls back an open block; never commits implicitly — api.md
  §2.3). The block forms close automatically.
- **Autocommit** is the default (CLAUDE.md §3): each `execute` is durable on its own. An explicit
  `BEGIN … COMMIT/ROLLBACK` works because the handle keeps transaction state across `execute`
  calls.
- **`Jed::Result`** is `Enumerable` over `Jed::Row`; a row offers positional (`row[0]`) and
  by-name (`row[:id]` / `row["id"]`) access, `#to_h`, `#to_a`.
- **`Jed::Error`** carries the 5-char **`sqlstate`** (spec/errors/registry.toml) and the engine's
  deterministic message — the same error any host sees. `Jed::LoadError` is a distinct wiring
  failure (missing/mismatched native library).

## 3. The FFI seam & wire format

The Ruby side loads the native cdylib through Ruby's stdlib **`fiddle`** — **no third-party
gem** (CLAUDE.md §14). The native side is a standalone `cdylib` crate (`impl/ruby/ext`) that
depends on the core by path and exposes eight C functions:

```
jed_abi_version() -> u32
jed_open_memory() -> *Database
jed_create(path)  -> *buf          jed_open(path, read_only) -> *buf
jed_execute(*Database, sql) -> *buf   jed_commit(*Database)  -> *buf
jed_close(*Database)                  jed_free(*buf)
```

Every fallible call returns one heap **result buffer** the caller frees with `jed_free`. The
buffer is self-describing, little-endian, single-allocation:

```
[0..8)  u64  total length (whole buffer)
[8]     u8   tag
  0 ERROR:     [5] sqlstate ascii ; lstr message
  1 STATEMENT: u8 has_rows_affected ; i64 rows_affected ; i64 cost
  2 QUERY:     i64 cost ; u32 ncols ; ncols×(lstr name, lstr type)
               ; u32 nrows ; nrows×ncols×(u8 is_null ; if !null: lstr value)
  3 HANDLE:    u64 database pointer (create/open success)
  4 UNIT:      (no payload; ok with no value, e.g. commit)
```

`lstr` = u32 length + that many UTF-8 bytes. Ruby copies the buffer out, frees it immediately,
and parses it ([codec.rb](../../impl/ruby/lib/jed/codec.rb)), so no native allocation outlives a
call.

### 3a. Bind parameters

`jed_execute` also takes an optional **param buffer** (`*const u8` + length, null/0 for none)
encoding the `$N` values, little-endian:

```
u32 nparams ; nparams×( u8 tag ; payload )
  0 NULL  : (no payload)        2 FLOAT : f64           4 TEXT : u32 len + utf8 bytes
  1 INT   : i64                 3 BOOL  : u8 (0/1)
```

The gem encodes one Ruby scalar per param ([params.rb](../../impl/ruby/lib/jed/params.rb)) —
`nil`→NULL, `Integer`→INT, `Float`→FLOAT, `true`/`false`→BOOL, `String`→TEXT — and the native side
decodes each to a `Value`. The engine then **context-types** every `$N` against its use site and
coerces/range-checks the bound value two-phase before any row is touched (api.md §5): an `Integer`
binds equally to an `i16`/`i32`/`i64`/`decimal` site, an out-of-range value traps `22003` at bind,
a NULL into a `NOT NULL` column `23502`, an undetermined type `42P18`. Two gem-side guards raise an
`ArgumentError` *before* the call (a programming error, not a SQL one): an unsupported Ruby type, or
an `Integer` outside the i64 range (`Array#pack("q<")` would silently *wrap* it, so the range is
checked explicitly). Richer typed binds (`BigDecimal`/`Time`/array/…) are a follow-on (§6).

**The value-rendering contract.** A query cell's text is exactly **`Value::render()`** — the
same canonical rendering the Rust conformance harness emits — so the gem prints byte-identical
to the corpus. A SQL **NULL is the `is_null` flag**, never the string `"NULL"`, so the gem can
distinguish a NULL from a `text` value that happens to render as `"NULL"`. The gem then
**coerces** the unambiguous scalars to native Ruby — `i16`/`i32`/`i64` → `Integer`, `f32`/`f64`
→ `Float` (with `Infinity`/`-Infinity`/`NaN`), `boolean` → `true`/`false`, NULL → `nil` — and
leaves everything else (`decimal`, `timestamp`/`tz`, `date`, `interval`, `uuid`, `bytea`,
`range`, `array`, composite) as its **canonical String**: lossless and surprise-free
([coerce.rb](../../impl/ruby/lib/jed/coerce.rb)).

## 4. Memory safety & untrusted queries

The gem wraps the **safe** Rust core, so the engine's guarantees carry through unchanged
(CLAUDE.md §2/§13): memory safety, the pure side-effect-free built-in surface, and the
deterministic cost meter all hold — a wrap cannot weaken them because it *is* the same engine.

The C ABI crate is the **single place in the project's product path that uses `unsafe`**,
confined to pointer marshalling at the boundary: every `extern "C"` body is wrapped in
`catch_unwind` (a panic across the ABI is undefined behavior, so a bug aborts cleanly into an
`XX000` error instead of corrupting the host), C-string borrows are validated for null / UTF-8,
and the handle is closed exactly once (the gem guards double-close; a finalizer is the safety
net for a forgotten close, undefined on explicit close so it never double-frees). The boxed
result buffer is reclaimed by reconstructing its exact `Vec` from the pointer + the length
stored in its header.

## 5. Loading & versioning

The loader ([ffi.rb](../../impl/ruby/lib/jed/ffi.rb)) resolves the platform cdylib
(`libjed_ruby.{so,dylib}` / `jed_ruby.dll`) from, in order: `JED_RUBY_LIB` (explicit override),
the in-repo cargo outputs (`ext/target/{release,debug}`), then the gem's own `lib/`. A missing
library raises a `Jed::LoadError` pointing at `rake ruby:build`. On load the gem checks
`jed_abi_version()` against its own `Jed::ABI_VERSION` and refuses a mismatch — a stale cdylib
beside a newer gem fails loudly, never as a silent wire misparse.

## 6. Build, test, and follow-ons

- **Build / test.** `rake ruby:build` compiles the cdylib; `rake ruby:test` builds it and runs
  the gem's minitest **seam** tests (`impl/ruby/test`), folded into `rake test`/`rake ci` like
  the CLI. Per CLAUDE.md §10 those tests cover only what the corpus cannot — the binding seam
  itself (marshalling, value coercion, NULL handling, handle lifecycle, error mapping,
  persistence). SQL semantics stay in the shared corpus, inherited by construction.
- **Landed:** create/open/execute/commit/close over literal SQL (slice 1); **`$N` bind
  parameters** (slice 2 — §3a, ABI v2).
- **Follow-ons:**
  - **Richer typed values** — optional `BigDecimal` / `Time` / `Date` coercion for the
    String-today types, both on read (the coerce table) and as bind params (beyond the slice-2
    `nil`/`Integer`/`Float`/bool/`String` set), behind an explicit opt-in.
  - **Host-loaded bundles** — expose `load_unicode_data` / `load_time_zone_data` so the gem can
    run collation / time-zone features (collation.md, timezones.md).
  - **Distributable packaging** — a `gem install`-able native gem via **`rb-sys` + precompiled
    platform gems** (or `magnus` for richer Rust ergonomics), replacing the in-repo
    `rake ruby:build` step. The TODO Phase 9 entry names this as the packaging approach.
  - **A Ruby conformance runner** — optional, to demonstrate (not establish) the inherited
    corpus pass directly through the gem.

## 7. Crate / gem layout

```
impl/ruby/
  jed.gemspec            # the gem (lib + ext sources)
  README.md              # user-facing quickstart
  lib/jed.rb             # entry point
  lib/jed/{version,error,ffi,codec,coerce,params,result,database}.rb
  ext/Cargo.toml         # standalone cdylib crate, jed = { path = "../../rust" }
  ext/src/lib.rs         # the C ABI (the only unsafe in the product path)
  test/database_test.rb  # minitest seam tests
```
