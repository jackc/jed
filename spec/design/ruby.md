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

The gem's public surface is `Jed` + `Jed::Database`, `Jed::Result` / `Jed::Row`,
`Jed::ExtensionRegistry` (host functions, §5b), and `Jed::Error`. It mirrors the Rust embedding
API ([api.md](api.md)) in Ruby idiom:

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
depends on the core by path and exposes this core set of C functions (plus the bundle loaders,
§5a, and the host-function registry, §5b):

```
jed_abi_version() -> u32
jed_open_memory(*Registry) -> *Database
jed_create(path, *Registry)  -> *buf          jed_open(path, read_only, *Registry) -> *buf
jed_execute(*Database, sql, params, params_len) -> *buf   jed_commit(*Database)  -> *buf
jed_close(*Database)                                       jed_free(*buf)
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
  5 TYPES:     u32 n ; n×lstr canonical type names (host-function registration, §5b)
```

`lstr` = u32 length + that many UTF-8 bytes. Ruby copies the buffer out, frees it immediately,
and parses it ([codec.rb](../../impl/ruby/lib/jed/codec.rb)), so no native allocation outlives a
call.

### 3a. Bind parameters

`jed_execute` also takes an optional **param buffer** (`*const u8` + length, null/0 for none)
encoding the `$N` values, little-endian:

```
u32 nparams ; nparams×( u8 tag ; payload )
  0 NULL  : (no payload)        2 FLOAT : f64           4 TEXT    : u32 len + utf8 bytes
  1 INT   : i64                 3 BOOL  : u8 (0/1)       5 DECIMAL : u8 neg ; u32 len + ascii digits ; u32 scale
                                                         6 DATE    : i32 days since 1970-01-01
                                                         7 TSTZ    : i64 µs since the 1970-01-01 UTC epoch
```

The gem encodes one Ruby value per param ([params.rb](../../impl/ruby/lib/jed/params.rb)) —
`nil`→NULL, `Integer`→INT, `Float`→FLOAT, `true`/`false`→BOOL, `String`→TEXT, `BigDecimal`→DECIMAL
(decomposed via `BigDecimal#split` into sign/digits/scale and rebuilt with `Decimal::from_digits_scale`),
`Date`→DATE (days via `Date` arithmetic, BC-correct), `Time`/`DateTime`→TSTZ (an instant in µs) — and
the native side decodes each to a `Value`. The engine then **context-types** every `$N` against its
use site and coerces/range-checks the bound value two-phase before any row is touched (api.md §5): an
`Integer` binds equally to an `i16`/`i32`/`i64`/`decimal` site, an out-of-range value traps `22003`
at bind, a NULL into a `NOT NULL` column `23502`, an undetermined type `42P18`, a `BigDecimal` into an
integer column a clean type error. Gem-side guards raise an `ArgumentError` *before* the call (a
programming error, not a SQL one): an unsupported Ruby type, an `Integer` outside the i64 range
(`Array#pack("q<")` would silently *wrap* it, so the range is checked explicitly), a non-finite
`BigDecimal` (jed decimal is finite-only), or a `Date`/`Time` outside jed's representable range.

**The value-rendering contract.** A query cell's text is exactly **`Value::render()`** — the
same canonical rendering the Rust conformance harness emits — so the gem reads byte-identical to
the corpus. A SQL **NULL is the `is_null` flag**, never the string `"NULL"`, so the gem can
distinguish a NULL from a `text` value that happens to render as `"NULL"`. The gem then **coerces**
to native Ruby ([coerce.rb](../../impl/ruby/lib/jed/coerce.rb)), **mirroring ActiveRecord's
PostgreSQL adapter** — coerce wherever Ruby has a faithful type, leave the rest as the canonical
String:

| jed type | Ruby |
|---|---|
| `i16`/`i32`/`i64` | `Integer` |
| `f32`/`f64` | `Float` (incl. `±Infinity`/`NaN`) |
| `boolean` | `true`/`false` |
| `decimal` | `BigDecimal` (finite-only → always clean) |
| `date` | `Date`, or `±Float::INFINITY` for `±infinity` |
| `timestamp`/`timestamptz` | `Time` (UTC), or `±Float::INFINITY` for `±infinity` |
| NULL | `nil` |
| `interval`/`uuid`/`bytea`/`range`/`array`/composite | canonical `String` |

The principle is **totality**: coerce iff Ruby's type is a faithful, total target for jed's value
space. `Integer`/`Float`/`BigDecimal` are total (no jed value escapes them — jed `decimal` is
finite-only). `date`/`timestamp` carry a first-class **`±infinity`** that `Date`/`Time` cannot hold,
so — exactly as AR does — those values become **`±Float::INFINITY`** (the column's Ruby type is then
`Date|Float` / `Time|Float`). A zoneless `timestamp` and a `timestamptz` both decode to a **UTC**
`Time` (AR's `default_timezone = :utc` convention); BC dates use astronomical year numbering, which
`Date`/`Time` share, and a `Date` is built in the proleptic Gregorian calendar (`Date::GREGORIAN`) that
jed and `Time` use — Ruby's default `Date` calendar is Julian before 1582, which would name a different
day and shift it when bound back. A render shape the parser doesn't recognize degrades to the String rather than
raising.

## 4. Memory safety & untrusted queries

The gem wraps the **safe** Rust core, so the engine's guarantees carry through unchanged
(CLAUDE.md §2/§13): memory safety, the pure side-effect-free built-in surface, and the
deterministic cost meter all hold — a wrap cannot weaken them because it *is* the same engine.

The C ABI crate is the **single place in the project's product path that uses `unsafe`**,
confined to pointer marshalling at the boundary (the host-function upcall's additions are justified
in §5b): every `extern "C"` body is wrapped in
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
library raises a `Jed::LoadError` pointing at `mise run ruby:build`. On load the gem checks
`jed_abi_version()` against its own `Jed::ABI_VERSION` (**before** binding the rest of the surface,
so a stale cdylib fails with a clear version message rather than a missing-symbol error) and refuses
a mismatch — never a silent wire misparse.

## 5a. Host-loaded bundles

The bare engine ships only `C` collation and `UTC` + fixed offsets (collation.md, timezones.md). A
host adds linguistic collations and IANA zones by loading byte **bundles** through two engine seams,
exposed as module functions over `jed_load_unicode_data` / `jed_load_time_zone_data` (each a
`(bytes, len) → UNIT|ERROR` call):

```ruby
Jed.load_unicode_data(File.binread("unicode.jucd"))   # JUCD → COLLATE "unicode", ILIKE, case folding
Jed.load_time_zone_data(File.binread("tzdata.jtz"))    # JTZ  → AT TIME ZONE 'America/New_York', date_trunc(…, zone)
```

Both are **engine-global** (the SQLite model) — they load into process-wide state, so one call
affects every open and future `Database`, and they take the raw bundle bytes (the host reads the
file). A malformed bundle raises `Jed::Error` (`XX001`). The repo's fixtures
(`spec/collation/fixtures/unicode.jucd`, `spec/tz/fixtures/tzdata.jtz`) are what the gem's tests
load; producing/shipping bundles is the host's concern, identical to the other cores'
`db.LoadUnicodeData` / `db.LoadTimeZoneData`.

## 5b. Host functions

A Ruby program registers its own scalar functions — the gem's face of the engine's host-function
seam ([extensibility.md](extensibility.md) §4.2) and its batched kernel ABI (§4.2.1). This is the
first **upcall** in the binding: the engine, running in Rust, calls back into Ruby. It is what the
batched ABI was designed to make affordable, so the gem is also the project's measurement of what
a wrapped core pays per host call ([benchmarks.md](benchmarks.md) §8.2; the evidence behind the
native-vs-wrap call in [cores.md](cores.md) §2.1).

### Surface

```ruby
reg = Jed::ExtensionRegistry.new
# Single-row kernel: the block receives one row's arguments and returns the result.
reg.register("add_tax", [:decimal, :decimal], :decimal, volatility: :immutable) { |amt, rate| amt * (1 + rate) }
# Batch kernel: one Array per argument (column-major), plus an output Array to append to.
reg.register_batch("mix", [:i32], :i64, volatility: :immutable, cost: 2) do |xs, out|
  xs.each { |x| out << x * 2654435761 % 1000003 }
end

Jed.memory(extensions: reg) { |db| db.query("SELECT mix(amount) FROM orders") }
# also: Jed.create(path, extensions: reg), Jed.open(path, read_only:, extensions: reg)
```

- **`register(name, arg_types, result, volatility: :volatile, cost: 1) { |*args| … }`** — a
  single-row kernel, called once per row. **`register_batch(…) { |*columns, out| … }`** — a batch
  kernel, called with one `Array` per argument (`columns[j][i]` is argument `j` of row `i`) and an
  empty `out` `Array`; it appends one result per row, in row order. The defaults are Rust's
  (`HostFunction::new`): `:volatile` (the safe assumption) and unit cost. `volatility:` is
  `:immutable`/`:stable`/`:volatile`; only a non-volatile batch kernel is prefetched in chunks
  (§4.2.1), so a volatile one is still called once per row. Types are jed type names as `Symbol`s or
  `String`s, aliases included (`:int`, `"bigint"`, `"double precision"`).
- **Freezing.** As in every core, a handle's function set is fixed when it opens: each `open`/
  `create`/`memory` builds the engine registry from the functions registered *so far*. Registering
  more later affects only handles opened later. One `ExtensionRegistry` serves any number of handles.
- **Validation is eager.** `register` raises `Jed::Error` immediately, with the engine's codes: a
  duplicate `(name, arg_types)` is `42723`, a negative cost `22023`, an unknown type name `42704`,
  and a type outside the supported set below `0A000`. Non-`Symbol`/`String` arguments, a missing
  block, or an unknown `volatility:` raise `ArgumentError`.
- **Strict.** A NULL argument yields NULL without calling the block (§4.2), so a kernel never sees
  `nil`. A kernel may *return* `nil` (SQL NULL).

**Supported types** are the scalars the gem coerces to a faithful Ruby class (§3): `i16`/`i32`/
`i64`, `f32`/`f64`, `boolean`, `decimal`, `text`, `date`, `timestamp`, `timestamptz`. Arguments
arrive as exactly the Ruby values a query cell of that type decodes to — `Integer`, `Float`,
`true`/`false`, `BigDecimal`, `String`, `Date`, UTC `Time`, or `±Float::INFINITY` for an infinite
date/timestamp. Results are accepted as the inverse of that table: `Integer` for the integers (and
for `decimal`), `Float` for the floats, `true`/`false`, `BigDecimal`, `String`, `Date`, `Time`/
`DateTime` (a `timestamp` takes the instant's UTC wall clock — the inverse of decoding a
`timestamp` as a UTC `Time`), and `±Float::INFINITY` for date/timestamp infinity. Anything else is
a wrong-typed result (below). `bytea`/`uuid`/`interval`/`json`/`jsonb`/`jsonpath` need a native
text→value parse at the boundary and are a follow-on with their typed coercion (§6).

### The C ABI

Five additions (ABI v5). `jed_open_memory`/`jed_create`/`jed_open` gain a trailing registry
pointer (null = no extensions):

```
jed_registry_new() -> *Registry                      jed_registry_free(*Registry)
jed_registry_register(*Registry, name, arg_types, result, volatility u8, cost i64,
                      batched u8, callback, user_data uintptr) -> *buf     (TYPES | ERROR)
jed_host_result(sink uintptr, bytes, len u64)        (the large-result sink, below)
jed_open_memory(*Registry) / jed_create(path, *Registry) / jed_open(path, read_only, *Registry)
```

`arg_types` is a comma-separated list of type names (empty for zero arguments) and `result` a type
name; the native side resolves them with `ScalarType::from_name` and returns the canonical names
(a TYPES buffer, §3), which key the gem's argument decoders. The native `Registry` keeps each
function as plain **specs** (name, types, volatility, cost, kernel form, callback, user data) beside
a shadow `ExtensionRegistry` that runs the engine's own `register_function` validation, so
registration errors are exactly the core's. Each open *borrows* the registry and builds a fresh
`Arc<ExtensionRegistry>` from the specs; the handle never refers to the `Registry` again. A
single-row spec becomes `HostFunction::new` (the engine loops it — one upcall per row); a batch
spec becomes `HostFunction::batched` (one upcall per prefetched chunk).

Both kernel forms upcall through one **callback** — a C function pointer, with the opaque
`user_data` the host passed at registration handed back (the gem passes 0; one
`Fiddle::Closure` per function already identifies it):

```
i64 callback(uintptr user_data, uintptr args, u64 args_len, uintptr out, u64 out_cap, uintptr sink)
```

Every pointer crosses as an integer (`uintptr`), so Fiddle builds no `Fiddle::Pointer` per call.

**Arguments (engine → Ruby)** are one buffer, marshalled once per call, column-major to match the
kernel ABI: `u32 nrows`, then each argument column in turn as `nrows` values in that column's
declared-type encoding — `i64` for the integers, `f64` for `f64`, `u8` for `boolean`, and the
`lstr` canonical rendering (`Value::render()`, the §3 query-cell contract) for every other type.
A column carries no NULL flags, because a strict kernel is never sent one. Ruby reads a numeric
column with one `unpack`, and passes rendered columns through the same `Jed::Coerce` a query cell
uses, so an argument is exactly the Ruby value `SELECT` would have produced.

**Results (Ruby → engine)** are one buffer: `u8 form ; u32 nresults ; nresults × value ; u8
has_error ; if has_error: [5] sqlstate ; lstr message`. In the **column form** (1) the values are the
declared result type's argument-column encoding, packed by one `Array#pack`; the gem uses it when the
result type is an integer, `f64`, or `boolean` and every value is exactly that class (an `Integer`
within `i64`, a `Float`, `true`/`false`). Otherwise it writes the **tagged form** (0): each value in
the §3a bind-parameter encoding, written by the same `Jed::Params` encoder, so a batch with one
odd value still reports it at its own row. The engine side decodes each value and conforms it to
the declared result type by the table above (range-checking a narrower integer, `22003`), or leaves
the decoded value as-is when the class does not fit, so the engine's own result check raises `22000`
at that row. Ruby writes the buffer into the caller-provided scratch `out` (capacity `out_cap`,
sized for small scalar results) and returns its length. A larger result is handed to
`jed_host_result(sink, bytes, len)`, which copies it into Rust memory while Ruby still holds the
`String`; the callback then returns that length. A negative return means "no result" (below).

**Errors and the failing row.** A block's exception becomes an engine error for the row it was
raised on:

- A `Jed::Error` carries its own `sqlstate` and message through, so a kernel can raise
  `Jed::Error.new("22012", "division by zero")` like a built-in. An unregistered code becomes
  `38000` with the code kept in the message.
- Any other exception is **`38000 external_routine_exception`** — PostgreSQL's code for an uncaught
  error in an externally defined routine (PL/Perl, PL/Python), with message
  `"host function NAME raised ExceptionClass: message"`. The `Jed::Error` that `execute` raises has
  the original Ruby exception as its `cause`.
- A result the gem cannot encode (an unsupported class) is `22000` at that row; an `Integer` beyond
  `i64` for an integer result is `22003`.
- **The batch failing-row rule** is §4.2.1's prefix rule: when a batch block raises, the results
  already in `out` are the rows that succeeded, and `out.length` is the failing row. The gem sends
  those results plus the error, and the engine side appends them before returning the error, so a
  batch raises at the same row its single-row form would.
- **Non-standard exits.** An exception, `throw`, `break`, `return`, or `Thread#kill` must not unwind
  through the Rust frames between the engine and the callback (a `longjmp` across them is undefined
  behavior). The callback's Ruby body therefore rescues `Exception` and returns from an `ensure`,
  which also stops `throw`/`break`/`kill`. A non-local exit sends no payload and becomes `38000`
  (`"…exited non-locally"`); the swallowed jump is not re-delivered. A non-`StandardError` exception
  (`Interrupt`, `SignalException`, `SystemExit`, `NoMemoryError`, a `Timeout` interrupt) is recorded
  and re-raised as itself by `execute` once the native call returns, so the host still sees the
  signal rather than a `Jed::Error`.

**The GVL.** `jed_execute` runs without the GVL (Fiddle's default, so other Ruby threads keep
running during a query), and Fiddle's closure trampoline re-acquires it for each upcall
(`rb_thread_call_with_gvl`). That re-acquire is part of the measured boundary cost.

### Lifetime

A `Fiddle::Closure`'s trampoline is freed when the closure is collected, so the gem keeps every
closure a handle can call alive for the handle's whole life: `Database` holds the frozen list of
function entries it was opened with. A finalizer that closes a forgotten handle calls no kernel (a
close drops the session and core), so it is safe even if the closures are collected in the same GC
cycle. The native `Registry` is freed by the `Jed::ExtensionRegistry` finalizer. A gem `execute`
drains the whole result inside one native call (§3), so no host call can happen after `execute`
returns.

### Memory safety (§4)

The callback path adds three kinds of `unsafe`, all at the C ABI seam, and adds no `unsafe` to the
engine:

1. **Calling a foreign function pointer.** The callback type is `unsafe extern "C" fn`. Its safety
   contract (the pointer stays valid and callable for every handle opened with it) is the host's,
   and the gem meets it with the lifetime rule above. Calling it passes only integers. The callback
   cannot unwind into Rust: Fiddle's trampoline calls Ruby without `rb_protect`, so the gem's
   rescue-and-ensure body is what keeps a Ruby exception or jump from crossing the native frames.
2. **Reading the result.** `out` is a **zero-initialized** Rust `Vec` of `out_cap` bytes. A returned
   length ≤ `out_cap` is read as a bounds-checked slice of it, never past `out_cap`, and a length
   above it must match what the sink received. A lying callback can produce a malformed buffer (a
   `38000`), never an out-of-bounds or uninitialized read. Every field is decoded by the
   bounds-checked cursor used for bind parameters.
3. **The sink.** `jed_host_result` dereferences the `sink` integer as the `&mut` result slot of the
   callback currently on the stack (the only place such a value comes from), and copies `len` bytes
   from Ruby's live `String`.

**Re-entrancy.** A host block that calls back into the *same* `Database` (an `execute`, `commit`,
or `close` from inside a kernel) would alias the handle that the outer `jed_execute` holds `&mut`.
`Database` therefore serializes every native call on a per-handle `Mutex`, which also closes the
cross-thread case (two Ruby threads using one handle while the GVL is released). A same-thread
re-entrant call raises `Jed::Error` `55006` (object in use) instead of deadlocking. Another handle,
or a new one, may be used from a kernel. The engine evaluates on the calling thread and spawns no
thread that could invoke a callback, so Fiddle's trampoline always has a Ruby thread to re-acquire
the GVL on.

**Untrusted queries.** A host function is host code, outside the untrusted-query guarantee
(CLAUDE.md §13): it may do I/O, loop forever, or allocate without bound. The engine still charges its
declared `cost` per call and bounds how far a batch is speculated by the cost headroom (§4.2.1). A
wrong-typed result cannot reach the engine's codecs.

## 6. Build, test, and follow-ons

- **Build / test.** `mise run ruby:build` compiles the cdylib; `mise run ruby:test` builds it and runs
  the gem's minitest **seam** tests (`impl/ruby/test`), folded into `mise run test`/`mise run ci` like
  the CLI. Per CLAUDE.md §10 those tests cover only what the corpus cannot — the binding seam
  itself (marshalling, value coercion, NULL handling, handle lifecycle, error mapping,
  persistence). SQL semantics stay in the shared corpus, inherited by construction.
- **Landed:** create/open/execute/commit/close over literal SQL (slice 1); **`$N` bind
  parameters** (slice 2 — §3a, ABI v2); **richer typed values** — `BigDecimal`/`Date`/`Time`
  coercion both directions, AR-style, always-on (slice 3 — §3, ABI v3; adds the `bigdecimal`
  gemspec dependency, a bundled stdlib gem); **host-loaded bundles** —
  `load_unicode_data`/`load_time_zone_data` (slice 4 — §5a, ABI v4); **host functions** —
  `Jed::ExtensionRegistry` with single-row and batch kernels over Fiddle closures, and the per-handle
  `Mutex` that refuses re-entrant use (slice 5 — §5b, ABI v5; measured in benchmarks.md §8.2).
- **Follow-ons:**
  - **Host functions over `bytea`/`uuid`/`interval`/`json`/`jsonb`/`jsonpath`** — a native
    text→value parse at the boundary, alongside the typed coercion below.
  - **`interval` / `uuid` / `bytea`** typed coercion — the remaining String-today scalars
    (`ActiveSupport::Duration` / a `uuid` wrapper / an ASCII-8BIT `String`), if the demand appears.
    Left as String for now (no single obvious native target, unlike decimal/date/time).
  - **Distributable packaging** — a `gem install`-able native gem via **`rb-sys` + precompiled
    platform gems** (or `magnus` for richer Rust ergonomics), replacing the in-repo
    `mise run ruby:build` step. The TODO Phase 9 entry names this as the packaging approach.
  - **A Ruby conformance runner** — optional, to demonstrate (not establish) the inherited
    corpus pass directly through the gem.

## 7. Crate / gem layout

```
impl/ruby/
  jed.gemspec            # the gem (lib + ext sources)
  README.md              # user-facing quickstart
  lib/jed.rb             # entry point
  lib/jed/{version,error,ffi,codec,coerce,params,result,database,extension}.rb
  ext/Cargo.toml         # standalone cdylib crate, jed = { path = "../../rust" }
  ext/src/lib.rs         # the C ABI (the only unsafe in the product path)
  test/*_test.rb         # minitest seam tests (database, params, rich types, bundles, host functions)
```
