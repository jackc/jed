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
handwritten emitter for resolved expressions; it does not permit evaluating SQL
text as Ruby or introducing a native engine dependency.
