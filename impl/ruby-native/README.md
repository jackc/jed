# Experimental pure Ruby engine — incomplete

`JedNative` is an independent, handwritten Ruby implementation. It does not load
the Rust binding, WASM, FFI, another SQL engine, or a native extension. The existing
`impl/ruby` package remains the Rust binding.

**This port has not reached conformance parity and is not ready for promotion.**
The [experiment report](REPORT.md) records executed checks, failures, omissions,
and preliminary performance measurements. Passing parser coverage or selected
benchmarks does not imply complete SQL, storage, or resource behavior.

```ruby
require_relative 'lib/jed_native'

JedNative.memory do |db|
  db.execute('CREATE TABLE items (id i32 PRIMARY KEY, label text)')
  db.execute('INSERT INTO items VALUES ($1, $2)', 1, 'one')
  statement = db.prepare('SELECT label FROM items WHERE id = $1')
  db.query_prepared(statement, 1).each { |row| puts row[:label] }
end
```

`JedNative.create(path)` and `JedNative.open(path)` use the shared v49 file format
and a pure Ruby `flock` host. Exclusive protocol-v2 ownership is the current default;
`locking: :auto` enables the experimental page-service owner/client implementation.
Commits persist changed tree paths with
validated content manifests, reuse eligible pages, and publish changed attachment
domains independently. File reads use lazy rows and a bounded page cache; ordinary
DML stages changed rows over immutable pages. `locking: :none` is only for hosts that
already exclude every other opener.

`Database#query` and `#execute` accept positional `$N` parameters; `#prepare` creates
a reusable parsed statement. Each database convenience call uses a fresh session.
Use `db.session` for session settings, temporary objects, or SQL transactions;
`db.read_session` pins a committed snapshot. Sessions expose `#transaction` and
`#execute_script` for atomic block/script execution. Errors carry a five-character
`sqlstate`.

`#query` and `#query_prepared` return a lazy `Rows` enumerable for reads. Rows have
numeric and name access. Exhausting enumeration or calling `#close` releases the
snapshot; `#rows` drains and collects the result. `columns` and `column_types` are
available before iteration, while `cost` advances as the cursor runs. `#execute`
and `#execute_prepared` eagerly collect the result and expose its final cost and
`rows_affected`. Cost parity and the supported cores' full host API are still
being verified.

An immutable `Environment` can register Ruby scalar or batch functions with
`register_function(name:, args:, returns:, ...)`. Pass it as `environment:` when
opening a database. Host functions are an explicit escape from built-in purity;
persisted expressions pin eligible functions by component identity and version.

Only explicitly selected tasks run this experiment:

```sh
mise run ruby-native:test
mise run ruby-native:page-service-test
mise run ruby-native:process
mise run ruby-native:conformance expr/arithmetic
JED_NATIVE_REPORT=/tmp/ruby-audit.json mise run ruby-native:conformance
mise run ruby-native:compare:build
mise run ruby-native:compare:verify
mise run ruby-native:compare
RUBY_NATIVE_PHASE=optimized mise run ruby-native:compare
# Full benchmark adapter, using the separately prepared standard databases:
mise run ruby-native:bench bench/corpus bench/data /tmp/ruby-native.jsonl
```

The full conformance audit fails on mismatches and reports unsupported harness
files separately. It does not declare capability parity or hide missing features.
The standard benchmark adapter records failures and exits nonzero if any selected
workload fails. The separate preliminary comparison uses a shared 1,000-row plan,
records raw timings and checksums, and distinguishes interpreter/YJIT/ZJIT; an
unavailable JIT is reported, never silently measured as interpreter Ruby.

The engine adds no gem dependencies. `tools/generate_catalog.rb` snapshots only
mechanical function metadata from shared spec data using the existing development
TOML parser; normal codegen never runs it. Parser, resolver, executor, codecs, and
recursive type behavior are handwritten. The file codec adapts the independent
Ruby fixture verifier's handwritten algorithms; the verifier remains unchanged.

After this experiment is persisted, future feature work must leave it frozen unless
explicitly requested. It is absent from default tests, benchmarks, and CI. See
[the experiment policy](../../spec/design/ruby-native-experiment.md).
