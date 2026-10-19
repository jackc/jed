# Reproduce the delivery and hex experiments

These artifacts reconstruct the measured variants without editing the checkout. All patches
are relative to baseline `74ed390506e5443fa6793328194bd85ae441f204`, not incremental on one
another. The selected production variant and measured comparisons are described in the parent
report; retaining a prototype patch does not endorse it for production.

| Variant | Source change |
|---|---|
| `baseline` | Unmodified engine at the baseline commit |
| `hex` | SQL hex decoding reads valid ASCII directly, retaining the existing error decoder |
| `codec` | Packed decoding reuses one packet byte view for independent bytea/UUID copies |
| `dispatch` | An idle worker begins a received request in its existing delivery task |
| `combined` | All three changes |

Each variant has a `.patch` and `.json` manifest. The manifest records exact engine/workload
source hashes and the patch hash. `codec-tests.patch` and `dispatch-tests.patch` retain focused
prototype tests separately; the benchmark snapshots do not need those test files.

## Reconstruct frozen sources

Run from the repository root with the existing mise/Node/Ruby toolchain, the already installed
`impl/ts/node_modules`, and the already installed Chromium used by Playwright. These commands
install or download nothing. The benchmark tasks execute from this checkout, while
`JED_PROBE_ROOT` selects each frozen source tree. Do not run other performance workloads at the
same time.

```sh
repo=$(pwd)
artifacts="$repo/bench/ts/results/2026-10-19-delivery-optimization/experiments"
scratch=$(mktemp -d /tmp/jed-delivery-repro.XXXXXX)
mkdir -p "$scratch/sources/baseline"
git archive 74ed390506e5443fa6793328194bd85ae441f204 \
  impl/ts/src impl/ts/package.json bench/ts/src bench/ts/browser \
  mise-tasks/bench/ts_async_browser |
  tar -x -C "$scratch/sources/baseline"

for variant in baseline hex codec dispatch combined; do
  source="$scratch/sources/$variant"
  if [ "$variant" != baseline ]; then
    cp -a "$scratch/sources/baseline" "$source"
  fi
  if [ -s "$artifacts/$variant.patch" ]; then
    patch --batch --fuzz=0 -p1 -d "$source" < "$artifacts/$variant.patch"
  fi
  cp "$artifacts/$variant.json" "$source/variant.json"
  cp "$artifacts/$variant.patch" "$source/variant.patch"
done

for variant in baseline hex codec dispatch combined; do
  source="$scratch/sources/$variant"
  ln -s "$repo/impl/ts/node_modules" "$source/impl/ts/node_modules"
  if [ -f "$repo/impl/ts/jed_lock.node" ]; then
    ln -s "$repo/impl/ts/jed_lock.node" "$source/impl/ts/jed_lock.node"
  fi
  # The browser driver hashes this common launching task for metadata.
  cp "$repo/mise-tasks/bench/ts_async_browser" "$source/mise-tasks/bench/ts_async_browser"
done

ruby -rjson -rdigest -e '
  ARGV.each do |source|
    manifest = JSON.parse(File.read(File.join(source, "variant.json")))
    manifest.fetch("sourceSha256").each do |file, expected|
      actual = Digest::SHA256.file(File.join(source, file)).hexdigest
      abort "Source mismatch: #{source}/#{file}" unless actual == expected
    end
    actual = Digest::SHA256.file(File.join(source, "variant.patch")).hexdigest
    abort "Patch mismatch: #{source}" unless actual == manifest.fetch("patchSha256")
  end
' "$scratch"/sources/*
```

## Run the serial matrix

The runner fixes affinity to Linux CPUs `0,2`, as in the recorded experiment. Use available
CPUs on another host and record that difference. Remove inherited workload-size, batch-limit,
browser-isolation and cancellation-setting overrides before reproducing the default matrix:

```sh
unset JED_ASYNC_HEAVY_DECIMAL_ROWS JED_ASYNC_HEAVY_TABLE_ROWS
unset JED_ASYNC_HEAVY_SORT_ROWS JED_ASYNC_HEAVY_TYPED_ROWS
unset JED_ASYNC_HEAVY_BATCH_ROWS JED_ASYNC_HEAVY_BATCH_BYTES
unset JED_ASYNC_BROWSER_ISOLATED
unset JED_CANCEL_FULL_WARMUPS JED_CANCEL_WARMUPS JED_CANCEL_SAMPLES JED_CANCEL_DELAY_MS

export JED_EXPERIMENT_REPO="$repo"
export JED_EXPERIMENT_RESULTS="$scratch"
ruby "$artifacts/run-matrix.rb" pilot
ruby "$artifacts/run-matrix.rb" repeated baseline hex combined
ruby "$artifacts/run-matrix.rb" cancel baseline hex
ruby "$artifacts/summarize.rb" "$scratch" "$scratch/summary.json"
```

Each command completes before the next starts. `pilot` measures all five variants once per
host with two warmups, three latency samples and one responsiveness observation per boundary
and case. `repeated` measures the listed variants in three fresh processes per host, with
three warmups, seven latency samples and three responsiveness observations. Both modes use
`decimal-aggregate` and `typed-output`. `coverage` runs the other two existing cases,
`resident-join-group` and `sort-top100`, once per listed host/variant with the repeated-mode
sample counts. A later coverage pass is separate evidence, not an extra primary repeat.
No coverage pass was included in this report; `ruby "$artifacts/run-matrix.rb" coverage baseline hex`
is an optional extension.

The outer order is repeat, then Node/browser, then the listed variants; variant order reverses
on repeat 2 and returns to the original order on repeat 3. Each process's shared harness
alternates direct/worker sample order. The runner refuses existing output directories and
checks frozen sources and patch hashes before and after each process. Progress is printed to
stdout, while each process writes `run.log`, `results.json` and a copy of `variant.json`.

Cancellation uses `mise run bench:ts_async_cancel`, with three full-query warmups, two
cancellation warmups, seven samples and a 20 ms delay after the active-execution marker.
Node uses its atomic cancellation flag; ordinary Chromium has no shared memory. Limits remain
512 rows / 256 KiB per batch / 2 MiB held results. The primary issue-to-settle measurement
uses only the caller's clock. Worker telemetry uses only the worker clock; the report does
not subtract timestamps across those clocks. Every measured cancellation must reject with
`57014`, avoid its watchdog, and leave the same session usable for a follow-up query.

For a single manual run, the corresponding public tasks accept the same frozen root:

```sh
JED_PROBE_ROOT="$scratch/sources/hex" \
  JED_ASYNC_HEAVY_CASES=decimal-aggregate,typed-output \
  JED_ASYNC_RESULTS="$scratch/manual-node" \
  mise run bench:ts_async_heavy
JED_PROBE_ROOT="$scratch/sources/hex" \
  JED_ASYNC_HEAVY_CASES=decimal-aggregate,typed-output \
  JED_ASYNC_BROWSER_RESULTS="$scratch/manual-browser" \
  mise run bench:ts_async_browser
```

These manual commands omit the matrix runner's affinity wrapper and are not additional
controlled samples unless that setup is reproduced as well.

## Summaries and provenance

`summarize.rb` keeps pilot, repeated and coverage timing results separate. Its latency headline
is the median of process medians; it also reports pooled means and sample extrema. Responsiveness
uses the median of each observation's maximum gap, the largest observed gap, and total long
tasks. These statistics are descriptive, not confidence intervals or latency guarantees.

Cancellation is parsed from each suite's `results` array, separately from latency `cases`.
The summary verifies sample counts, rejection/cleanup/watchdog status, issue-to-settle clock
arithmetic, fixed limits, full-query validation equality and each stored process summary. It
reports the median of process medians, pooled median, pooled maximum and sample count. The
raw records retain all other telemetry. A non-canceled sample causes this comparison summary
to fail rather than silently excluding it.

The heavy tasks record the launching checkout's Git revision and dirty status. That revision
does **not** identify the frozen engine selected by `JED_PROBE_ROOT`; use the variant manifest,
per-run source hashes and browser bundle hashes for that identity. Cancellation can label an
archive-only root `source-snapshot` instead. The browser build also records installed tooling
and generated artifacts; changing toolchain or dependency versions can change bundle hashes
even when source hashes agree. No dependency contents are installed by these scripts.

`prepare-variant.rb` documents the original development workflow: after extracting a baseline,
`baseline` freezes it, and `VARIANT REPO_RELATIVE_FILE...` copies that baseline and replaces only
the named files from the current checkout. It generates the patch and manifest and links
existing dependencies. It is for creating a new experiment; use the retained patches above
to reproduce all original variants after production has selected only one of them.
