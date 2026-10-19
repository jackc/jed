# Controlled TS revision comparison

Run `JED_COMPARE_CPUS=0,2 mise run bench:ts_compare_matrix` on an otherwise idle Linux
host (omit the affinity setting elsewhere). Defaults compare `origin/master` with `HEAD`:
master's direct synchronous API, branch direct, and branch's production worker API.
`mise run bench:ts_compare_matrix --help` and `mise run bench:ts_compare --help` list overrides.

The matrix freezes committed engine sources using `git archive`, shares the existing installed
dependencies, and runs each variant in a fresh process/browser. Nothing is installed. Three
repeats rotate variant order; all runs are sequential. Ordinary Chromium is the browser default,
without cross-origin isolation. The worker uses production batching and yielding defaults.

The shared workloads cover cheap queries/prepared execution, three expensive queries returning
small results, and 200,000 typed result rows. Heavy latency includes complete draining and a typed
fingerprint on both paths. Setup, startup, comparison checks and responsiveness probes are outside
latency samples. Cheap direct operations have no per-call await. Headline numbers are medians of
process medians; raw samples and responsiveness observations remain available locally.

The summarizer rejects differences in workload configuration, values, types or query costs.
Source and browser bundle hashes are checked around measurement. This measures warm in-memory
query execution/delivery; it does not establish file I/O, cold startup, cancellation latency,
or performance on every browser/device. Fingerprints are consistency checks, not cryptographic
proofs. Keep compact reports when useful; generated sources, bundles and raw runs belong in the
ignored `bench/results/` directory.
