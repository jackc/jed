# Experimental Ruby engine

`JedNative` is an independent, handwritten Ruby implementation. It does not load
the Rust binding, WASM, FFI, or another SQL engine. SQL digest functions use Ruby's
OpenSSL extension, and page CRC32 uses stdlib Zlib, both explicitly approved utility
dependencies; all database algorithms remain Ruby. The existing `impl/ruby` package
remains the Rust binding.

**This frozen experiment is not a supported core.** It implements the SQL, storage
and host surface of its recorded source revision. The [experiment report](REPORT.md)
records conformance, interoperability, resource limits and benchmark measurements.
The [lookup follow-up](LOOKUP_FOLLOWUP.md) records further profiling and measurements
with Ruby 4.1.0-preview1 YJIT and ZJIT. The [second lookup pass](LOOKUP_SECOND_PASS.md)
records the specialized lazy point cursor and repeated comparisons within 10× Go.
[Runtime instructions](../../bench/ruby-native/RUNTIMES.md)
describe the separate opt-in build; the repository's normal Ruby version is unchanged.
Future features do not update this directory unless explicitly requested.

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
and a pure Ruby `flock` host. The default `locking: :auto` uses protocol-v2
page-service ownership: one process owns the file and other processes run their
own Ruby engine as page clients. Use `locking: :exclusive` to require sole access.
Commits persist changed tree paths with
validated content manifests, reuse eligible pages, and publish changed attachment
domains independently. File reads use lazy rows and a bounded page cache; ordinary
DML stages changed rows over immutable pages. `locking: :none` is only for hosts that
already exclude every other opener.

`synchronous: :off` batches durable barriers with cumulative validated manifests;
closing the database flushes the pending batch. `db.compact('main')` rewrites a file
without changing its tree shape and refuses active readers or writers. File backups
use `db.backup(path, form: :page_identical)` or `form: :compacted`; memory databases
support the compacted form.

`Database#query` and `#execute` accept positional `$N` parameters; `#prepare` creates
a reusable parsed statement. Each database convenience call uses a fresh session.
Use `db.session` for session settings, temporary objects, or SQL transactions;
`db.read_session` pins a committed snapshot. Sessions expose `#transaction` and
`#execute_script` for atomic block/script execution. Errors carry a five-character
`sqlstate`.

Database settings and grants become defaults for future sessions, including the
fresh sessions used by convenience calls. Existing sessions retain their settings;
explicit options to `db.session` override the defaults. Sessions expose
`max_cost=`, `lifetime_max_cost=`, `lifetime_cost`, `max_scalar_bytes=`,
`max_query_memory_bytes=`, and `work_mem=`. Closing an exhausted writer rolls back
its transaction and releases its lock without refunding the accrued cost.

`path`, `page_size`, `read_only?`, and `resident_leaves` describe the storage host.
`storage_bytes(name = 'main')` measures committed main or attachment storage.
`to_image(page_size: ..., txid: 0)` exports committed main to a Ruby byte string;
a session inside a transaction or pinned read exports its committed snapshot.
Pending changes, temporary objects, and attachments are excluded from that image.
Use `JedNative.from_image(bytes)` to open the image in memory.

`#query` and `#query_prepared` return a lazy `Rows` enumerable for reads. Rows have
numeric and name access. Exhausting enumeration or calling `#close` releases the
snapshot; `#rows` drains and collects the result. `columns` and `column_types` are
available before iteration, while `cost` advances as the cursor runs. `#execute`
and `#execute_prepared` eagerly collect the result and expose its final cost and
`rows_affected`. The shared corpus checks values, types, names, errors and costs;
separate host tests cover settings, callbacks, cursor lifetimes and file operations.

An immutable `Environment` can register Ruby scalar or batch functions with
`register_function(name:, args:, returns:, ...)`. Pass it as `environment:` when
opening a database. Host functions are an explicit escape from built-in purity;
persisted expressions pin eligible functions by component identity and version.

```ruby
environment = JedNative::Environment.new.register_function(
  name: 'twice', args: ['i32'], returns: 'i32', cost: 1,
  volatility: :immutable, component_id: 'example.twice', semantic_version: 1
) { |value| value * 2 }

JedNative.memory(environment: environment) do |db|
  puts db.execute('SELECT twice(21::i32)').rows.first.first # 42
end
```

Functions are strict: a NULL argument returns NULL without calling Ruby. Overloads
match their declared argument types. For a batch kernel, pass
`batch_kernel: ->(columns, output) { columns.first.each { |v| output << v * 2 } }`
instead of a block. Each input column contains only non-NULL values. The kernel
appends one result per row; on failure, it leaves the successful prefix and raises
at the first unanswered row. Stable and immutable kernels can receive bounded
batches; volatile kernels run one row at a time. Cost, errors, and result validation
remain attached to the row's normal evaluation point.

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
# The TypeScript file lane needs its existing OS-lock artifact:
mise run ts:lock_build
mise run ruby-native:file-compare -- --run
# Investigate allocation/call counts separately from canonical timing:
JED_NATIVE_TIMEOUT=180 JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:bounded -- ruby --yjit bench/ruby-native/lookup_profile.rb --report /tmp/lookup.json
# Disk SQL audit; default locking is auto (page service):
JED_NATIVE_STORAGE=disk JED_NATIVE_LOCKING=exclusive mise run ruby-native:conformance
# Full benchmark adapter, using the separately prepared standard databases:
mise run ruby-native:bench bench/corpus bench/data /tmp/ruby-native.jsonl
```

The full conformance audit fails on mismatches and reports unsupported harness
files separately. It does not declare capability parity or hide missing features.
Each corpus file runs in a separate child, sequentially, with a 512 MiB sampled
process-group RSS ceiling, a hard 1 GiB address-space ceiling, and a 10-second
deadline. The larger address-space ceiling accommodates Ruby's unused virtual
reservations. `JED_NATIVE_MEMORY_BYTES`, `JED_NATIVE_ADDRESS_SPACE_BYTES`, and
`JED_NATIVE_FILE_TIMEOUT` explicitly override those positive limits;
`RUBY_NATIVE_ADDRESS_SPACE_BYTES` is also accepted for benchmark tooling. Timeouts,
allocation failures, and interruptions terminate the worker process group; reports
record the active SQL and peak sampled resident memory. A watchdog also stops a
worker if its supervisor dies. These harness limits are separate from SQL resource
settings and prevent a failed audit from continuing in a damaged or growing heap.
Worker output files are limited to 8 MiB. Reports retain at most 64 KiB per SQL or
error diagnostic and 4 MiB of failure details; original sizes and omitted-detail
counts make truncation explicit, while pass/failure totals remain exact.
The standard benchmark adapter isolates each workload in a capped child too. It
records failures and exits nonzero if any selected workload fails. Its explicit
limits are `RUBY_NATIVE_MEMORY_BYTES`, `RUBY_NATIVE_ADDRESS_SPACE_BYTES`, and
`RUBY_NATIVE_BENCH_TIMEOUT` (60 seconds for smoke checks, 1,800 for measurements).
`RUBY_NATIVE_WORK_MEM` explicitly changes the logical operator budget; it does not
change the worker's heap limit. `RUBY_NATIVE_BENCH_NAMES` selects exact workload names
separated by commas. Both settings are recorded in benchmark metadata. Smoke checks
execute the original setup and one measured operation; they validate answers and
are not throughput measurements. The file comparison runs canonical warmups and
iterations for primary-key reads, cold handle opens, and durable commits, sequentially
on private copies of the standard datasets.
The separate comparison uses a shared 1,000-row plan,
records raw timings and checksums, and distinguishes interpreter/YJIT/ZJIT; an
unavailable JIT is reported, never silently measured as interpreter Ruby.

The approved OpenSSL and Zlib extensions are required for digests and CRC32.
`tools/generate_catalog.rb` snapshots only
mechanical function metadata from shared spec data using the existing development
TOML parser; normal codegen never runs it. Parser, resolver, executor, codecs, and
recursive type behavior are handwritten. The file codec adapts the independent
Ruby fixture verifier's handwritten algorithms; the verifier remains unchanged.

After this experiment is persisted, future feature work must leave it frozen unless
explicitly requested. It is absent from default tests, benchmarks, and CI. See
[the experiment policy](../../spec/design/ruby-native-experiment.md).
