# Frozen Swift Unicode tooling contract

Baseline: `362e0798ac3a8bd04bd2da556044b6ec73bf468a`.

The source and byte contracts are `spec/collation/README.md` §§1–5,
`spec/design/collation.md` §§4, 6, 9, 14, 16, the two compiler vectors,
the bundle vector, and the committed Unicode 17.0.0/CLDR 48 artifacts.
The compiler is handwritten and belongs to `JedUnicodeCompiler`, a separate
tooling target. `Jed` contains only a package-level artifact codec bridge;
production callers cannot parse collation definitions or consult host Unicode.

The baseline compiler frontend is Rust's `gen_collation_vectors`; its bundle
frontend is `build_collation_bundle --preset casing-only|non-cjk|everything
--out PATH`. Swift provides `jed-unicode compile`, `casing`, and `bundle`, all
with an explicit output path. `everything` currently selects the same root and
Spanish tailoring as `non-cjk`, because CJK/implicit weights remain deferred.
No command modifies canonical fixtures by default or downloads source data.

`UnicodeCompiler.compile(name:definitions:)` returns a JCOLL artifact;
`compileCasing` returns the version and property payload; `bundle` consumes
compiled artifacts and optional casing and returns JUCD. Existing runtime
LZ4, CRC32, and byte validation are reused through `UnicodeArtifactCodec`.
The CLI owns file I/O; the compiler accepts strings/bytes only.

DUCET parsing accepts explicit scalar mappings, expansions, contractions, the
optional fourth ignored weight, and `@version`. Unimplemented directive records
do not synthesize implicit mappings. LDML accepts one scalar reset followed by
`<`, `<<`, `<<<`, or `=` relations and one scalar per target. An anchor must have
one collation element. The frozen allocator picks the midpoint before the next
greater weight at that level (matching higher levels), or appends gaps 0x0200,
0x0020, 0x0006 for primary/secondary/tertiary. New lower levels reset to secondary
0x0020 and tertiary 0x0002. Dense gaps and overflow remain 0A000. These constants
are observable in the frozen compiler/artifact vectors; they do not implement
the future general UCA allocator.

Table serialization sorts scalar mappings numerically and contractions
lexicographically. JUCD uses property, root, then caller-ordered tailorings;
the delta payload uses the frozen counts-first table header, as confirmed by
the shared bundle fixture. Casing compiles simple and unconditional special
mappings only. Tests rebuild both small compiler vectors and the real Unicode,
Spanish, property, and JUCD bytes, then exercise malformed tooling inputs.

The four `UnicodeCompilerTests` passed in the integrated `mise run
swift-native:test` run on 2026-11-05. They cover all shared compiler/bundle
vectors, byte-for-byte rebuilding of the production artifacts, source-order and
line-ending invariance, and malformed/version/delta boundaries. The CLI target
also compiled in that run; its command-line smoke test remains outstanding.
