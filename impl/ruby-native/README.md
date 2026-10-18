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
and a pure Ruby `flock` host. The current host takes exclusive protocol-v2 ownership;
page-service clients are not implemented. Commits append complete snapshots and
validated content manifests. Incremental domain persistence, bounded residency,
and page reuse remain unfinished. `locking: :none` is only for hosts that already
exclude every other opener.

`Database#query` and `#execute` accept positional `$N` parameters; `#prepare` creates
a reusable parsed statement. Results expose `columns`, `column_types`, `rows`,
`rows_affected`, and `cost`; enumeration yields rows with numeric or name access.
`#transaction` and `#execute_script` provide atomic block/script execution. Errors
carry a five-character `sqlstate`. Cost parity and the supported cores' full host
API are still incomplete. Results currently collect rows eagerly.

Only explicitly selected tasks run this experiment:

```sh
mise run ruby-native:test
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
