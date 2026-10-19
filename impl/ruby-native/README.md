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
The [query-shape follow-up](QUERY_FOLLOWUP.md) records further scan, aggregate,
grouping, sort, and general prepared-execution improvements.
The [scalar follow-up](SCALAR_FOLLOWUP.md) records lightweight scalar cursors,
conditional/text execution, and comparisons across both JITs.
The [binding comparison](BINDING_COMPARISON.md) compares the native and Rust-backed
Ruby APIs, host callbacks, and remaining promotion requirements.
The [API follow-up](API_FOLLOWUP.md) measures ordinary query setup, host-call
planning, and batch execution improvements, including fresh SQL and both JITs.
The [runtime compilation follow-up](COMPILED_FOLLOWUP.md) measures generated Ruby
expression methods inside reusable plans, including direct YJIT/ZJIT evidence.
The [pipeline follow-up](PIPELINE_FOLLOWUP.md) extends that compiler into eligible
single-table scan/filter/projection loops, preserving lazy LIMIT/OFFSET behavior.
It also records lazy-API failures and a multi-page LIMIT cost defect found during
that audit. The [correctness follow-up](CORRECTNESS_FOLLOWUP.md) fixes the metadata,
stack-depth, and cost defects and distinguishes materialized-memory corpus
expectations from lazy collector thresholds.
The [ordering follow-up](ORDERING_FOLLOWUP.md) specializes typed ORDER BY
comparators and measures whole-query gains with interpreter/YJIT/ZJIT controls.
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

Ordinary SQL calls reuse a bounded database-owned cache of parsed statements
(64 entries, 64 KiB of SQL, at most 16 KiB per statement). Bindings, privileges,
session settings, and catalog state are checked for each execution; results are
never cached. `EXPLAIN` and larger statements bypass this cache. Public `prepare`
still returns an independently owned statement. Reusing an explicit session also
retains its resolved plans; database convenience methods still use fresh sessions.

Reused plans can compile supported integer and boolean expressions into bounded
Ruby methods after their first execution. This also applies to ordinary repeated
SQL in a session. Unsupported expressions keep the existing evaluator, with the
same SQL coverage and costs. No SQL text or literal is evaluated as Ruby source.
Enable YJIT or ZJIT on the Ruby command line to let the runtime optimize these
methods; the library does not change JIT settings. Short-lived queries may not
amortize compilation. See the [design](../../spec/design/ruby-runtime-compilation.md)
for admission, cache limits, and semantic boundaries.

Eligible single-table queries fuse the row loop, predicate, and projections into
one generated method. Ordered scans can retain streaming OFFSET/LIMIT and cursor
pull behavior. Exact-point/scalar paths, host callbacks, DISTINCT, blocking sorts,
profiling, and finite query-memory accounting retain their existing executors.
Pipeline methods share the plan's 64 KiB source allowance with expression kernels.

Blocking single-table sorts can also compile comparisons of integer and boolean
keys, including direction, NULL placement, and stable ties. Key evaluation and
the existing heap, full-sort, and spill algorithms keep their original execution
and resource checkpoints. Other key types use the existing comparator. Ordering
methods share the same plan source limit and compile only after reuse.

On memory hosts, reused integer and boolean aggregates can compile their input
and reduction loops too. Input evaluation still finishes before reduction, so
errors and cost-limit failures retain their original order. DISTINCT, ordered
aggregates, unsupported types, and file-host aggregation use their existing
execution. See the [aggregate follow-up](AGGREGATE_FOLLOWUP.md) for measurements.

The general memory executor also compiles reused materialized WHERE filters and
complete integer/boolean grouping-key vectors. It preserves the separate filter
and key-evaluation phases, rejected-row memory release, and ordinary grouping
and HAVING behavior. See the [relation follow-up](RELATION_FOLLOWUP.md).

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
# General query shapes through lazy and eager prepared APIs:
JED_NATIVE_TIMEOUT=180 JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:bounded -- ruby --yjit bench/ruby-native/query_profile.rb --report /tmp/queries.json
# Expanded scalar comparison (the default benchmark plan remains unchanged):
RUBY_NATIVE_COMPARE_PLAN=bench/ruby-native/compare/scalar-plan.json \
  RUBY_NATIVE_PHASE=scalar-local mise run ruby-native:compare
# Exercise the lazy API against expression-corpus expectations:
JED_NATIVE_QUERY_API=query mise run ruby-native:conformance expr/
# Mixed audit: finite query-memory fixtures retain their materialized drive:
JED_NATIVE_QUERY_API=query JED_NATIVE_QUERY_MEMORY_API=execute mise run ruby-native:conformance
# Force generated expression admission in one-shot conformance statements:
JED_NATIVE_COMPILATION=immediate mise run ruby-native:conformance
# Expanded runtime-compilation comparison (explicit opt-in):
RUBY_NATIVE_COMPARE_PLAN=bench/ruby-native/compare/compiled-plan.json \
  RUBY_NATIVE_PHASE=compiled-local mise run ruby-native:compare
# Fused scan comparisons, including LIMIT/OFFSET (explicit opt-in):
RUBY_NATIVE_COMPARE_PLAN=bench/ruby-native/compare/pipeline-plan.json \
  RUBY_NATIVE_PHASE=pipeline-local mise run ruby-native:compare
# Disk SQL audit; default locking is auto (page service):
JED_NATIVE_STORAGE=disk JED_NATIVE_LOCKING=exclusive mise run ruby-native:conformance
# Full benchmark adapter, using the separately prepared standard databases:
mise run ruby-native:bench bench/corpus bench/data /tmp/ruby-native.jsonl
```

The full conformance audit fails on mismatches and reports unsupported harness
files separately. It does not declare capability parity or hide missing features.
It uses eager `execute` by default. `JED_NATIVE_QUERY_API=query` explicitly selects
lazy cursors, drains within the error boundary, and records how many statements
use the scalar cursor. Finite query-memory fixtures describe eager collection
thresholds. `JED_NATIVE_QUERY_MEMORY_API=execute` explicitly routes those fixtures
to the materialized API and reports the routing counts; this is a mixed audit,
not a full pure-query pass. Lazy collectors have separate host tests described in
the [correctness follow-up](CORRECTNESS_FOLLOWUP.md).
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
