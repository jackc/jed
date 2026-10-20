# Pure Ruby core experiment

The pure Ruby engine in `impl/ruby-native` is an explicitly requested experiment,
separate from the Rust binding in `impl/ruby`. It is not a fourth member of the
Rust/Go/TypeScript differential set. It must be selected explicitly for testing,
conformance, benchmarking, and CI; default tasks must not acquire a dependency on it.

The experiment investigates Ruby integration and execution performance, including
the interpreter and available Ruby JITs. The engine itself must remain pure Ruby:
no FFI, native engine extension, embedded interpreter for another core, or SQL subprocess.
Ruby's built-in arbitrary-precision integers are permitted for exact arithmetic.
The maintainer explicitly approved Ruby's OpenSSL binding for vetted cryptographic
digest primitives, stdlib Zlib for byte-identical CRC32 checksums, and a well-supported
native checksum gem if profiling justifies it (prefer the standard library). These
utility exceptions do not authorize a native engine component; parser, planner,
remaining evaluation, and storage algorithms stay handwritten Ruby. Other new
third-party dependencies still require explicit confirmation.

Completion requires the current supported surface, shared corpus verification,
file-format and host-boundary verification, a recorded working baseline, followed
by a measured optimization pass and comparison with the supported cores. Missing
features and unavailable runtime configurations must be reported explicitly;
partial SQL coverage is not evidence of a complete port.

Once persisted, the experiment is frozen at its recorded source revision. Future
feature work does not update it, its frozen catalog data, or its benchmark results
unless the maintainer explicitly requests that work. Promotion to a supported core
requires a separate decision. An experiment need not pass a later expanded corpus;
its report must identify the revision it tested and retain failures and omissions.

The implementation and validation report are under `impl/ruby-native`. Results
must distinguish interpreter, YJIT, and ZJIT, record actual runtime availability,
warmup, dataset size, checksums, and command lines, and preserve baseline measurements
before optimizations. JIT settings belong to the benchmark launcher, never the library.

The separately requested runtime compilation experiment is described in
[`ruby-runtime-compilation.md`](ruby-runtime-compilation.md). It permits a bounded,
handwritten emitter for resolved expressions, eligible single-table row loops,
typed ORDER BY comparators, primitive aggregate input/reduction loops, and
materialized WHERE and grouping-key loops;
it does not permit evaluating SQL text as Ruby or introducing a native engine dependency.
The scalar emitter's C-text comparison extension preserves collation metadata
and string-work charges; linguistic collation and general string functions keep
their ordinary execution. The [literal LIKE extension](../../impl/ruby-native/LIKE_FOLLOWUP.md)
admits bounded exact, prefix, suffix and contains predicates under static C labels;
the [broader literal extension](../../impl/ruby-native/LITERAL_LIKE_FOLLOWUP.md)
adds fixed-position `_` with optional final `%`, and interior `%` without `_`.
Other complex patterns retain the handwritten matcher with unchanged values,
errors and costs.
The [method-sharing follow-up](../../impl/ruby-native/METHOD_SHARING_FOLLOWUP.md)
shares identical generated methods through a bounded process cache while keeping
each kernel's constants and execution ownership independent.
The [join planning follow-up](../../impl/ruby-native/JOIN_PLAN_FOLLOWUP.md)
retains bounded resolved roots and column metadata for eligible two-table joins.
Access selection remains specific to each execution and binding; mutable
execution state belongs to the cursor/SELECT invocation.

The historical [`PIPELINE_FOLLOWUP.md`](../../impl/ruby-native/PIPELINE_FOLLOWUP.md)
records a multi-page LIMIT cost defect and 71 lazy-audit failures already present
at source revision `627cbb631e3a922999c3a0cb3bcba32136fcb345`. The explicitly
requested [`CORRECTNESS_FOLLOWUP.md`](../../impl/ruby-native/CORRECTNESS_FOLLOWUP.md)
fixes the ordered scan page block, correlated output/lifetime accounting,
resolution/metadata, and stack-depth defects without changing shared contracts.
The later [cursor conformance correction](../../impl/ruby-native/CURSOR_CONFORMANCE_FIX.md)
corrects the earlier interpretation of the remaining memory failures: shared
conformance uses external cursor drains, not engine-owned result collectors.
The Ruby adapter now does the same for every query record, and mixed API routing
is rejected. Buffered WITH results and gathered column lanes preserve their
canonical ownership and admission checkpoints; `Rows#rows` retains its separate
engine-owned collector contract. Original failure evidence remains unchanged.
