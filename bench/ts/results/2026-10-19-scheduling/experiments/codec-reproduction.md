# Rejected fused codec experiments

These artifacts preserve the codec experiments against baseline
`2a8bdfcb264d1ef388296ee8554b485bee3e7d26`. The production codec and its tests were restored
to that baseline. The patches are experimental evidence, not changes to apply to the working tree.

| Artifact | Reconstructed source |
|---|---|
| `fused-decoder.patch` | `decoder-only.ts`: bounded validation and construction in one pass |
| `fused-writer.patch` | `writer-only.ts`: validation and packing in one pass, original decoder |
| `fused-codec.patch` | `combined.ts`: both fused passes |
| `fused-codec-tests.patch` | Tests for the combined prototype's revised allocation contract |
| `codec-sources.sha256` | Exact hashes of the four frozen codec sources, including baseline |
| `codec-probe.original.mjs` | Exact original isolated probe, including the original absolute paths |
| `codec-probe.mjs` | Same probe with configurable source, baseline, snapshot and output paths |
| `probe-scripts.sha256` | Hashes of both probe scripts |

The source and test patches use zero lines of context. Each is relative to the baseline,
not incremental on another prototype. Tests cover exact logical admission and golden bytes,
source validation, refusal/error rollback, bounded private prefixes, ownership and malformed
counts. The combined prototype passed 13 codec tests, 14 client tests, Node type checking and
changed-file lint. The writer also had a reviewed edge case: explicitly configured allowances
above 4 GiB could change buffer-limit versus malformed-suffix error precedence. It was not
revised because the experiment was rejected.

The probe constructs 200,000 rows containing integer, decimal, bytea, array and NULL values
before timing. It compares encoding baseline/combined and decoding baseline/decoder-only,
with three warmups and seven samples in alternating AB/BA order. Before warmup it checks
every packet byte, decoded value, per-row logical charge and final budget total. It also
verifies source hashes, identical shared dependencies, and identical readers in both fused
variants. Defaults are 512 rows and 256 KiB per batch. Natural garbage collection remains
inside samples; one explicit collection occurs before warmups when `--expose-gc` is used.

This measures isolated Node codec cost, excluding SQL, worker messaging, browser scheduling
and application result processing. It is not an end-to-end throughput or responsiveness test.
The original probe's hash is
`d90c6d58de2f4c0291e47919ffc7c560b1f941419e79655332e82cbd65586585`.

## Reproduce without changing the checkout

Run from the repository root, with no other performance workload active. This uses existing
Node/mise tooling, Git, GNU patch and tar; it adds no dependency. The commands reconstruct
sources only in a temporary directory.

```sh
repo=$(pwd)
artifacts="$repo/bench/ts/results/2026-10-19-scheduling/experiments"
scratch=$(mktemp -d /tmp/jed-fused-repro.XXXXXX)
mkdir -p "$scratch/baseline" "$scratch/snapshots"
git archive 2a8bdfcb264d1ef388296ee8554b485bee3e7d26 impl/ts/src |
  tar -x -C "$scratch/baseline"
cp "$scratch/baseline/impl/ts/src/async/packed.ts" "$scratch/snapshots/baseline.ts"
patch --silent --output "$scratch/snapshots/decoder-only.ts" \
  "$scratch/snapshots/baseline.ts" "$artifacts/fused-decoder.patch"
patch --silent --output "$scratch/snapshots/writer-only.ts" \
  "$scratch/snapshots/baseline.ts" "$artifacts/fused-writer.patch"
patch --silent --output "$scratch/snapshots/combined.ts" \
  "$scratch/snapshots/baseline.ts" "$artifacts/fused-codec.patch"
cp "$artifacts/codec-sources.sha256" "$scratch/snapshots/SHA256SUMS"
(cd "$scratch/snapshots" && sha256sum --check SHA256SUMS)

JED_CODEC_REPO="$repo" \
JED_CODEC_BASELINE="$scratch/baseline" \
JED_CODEC_SNAPSHOTS="$scratch/snapshots" \
JED_CODEC_OUTPUT="$scratch/output" \
  mise exec -- taskset -c 0,2 node --expose-gc "$artifacts/codec-probe.mjs" \
  > "$scratch/summary.json" 2> "$scratch/progress.log"
```

The measured host used CPU affinity `0,2`; use available CPUs and record the difference on
another host. Raw timings, execution order, source hashes and configuration are written to
`$scratch/output/results.json`. `JED_CODEC_ROWS`, `JED_CODEC_BATCH_ROWS` and
`JED_CODEC_BATCH_BYTES` optionally override the defaults. The dependency comparison deliberately
fails if the checkout's decimal, wire-budget or transport-error implementation differs from
the archived baseline; use a checkout with matching dependencies for a comparable run.
