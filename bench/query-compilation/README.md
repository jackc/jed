# Query compilation probes

Research supporting [the three-core investigation](../../spec/design/query-compilation-investigation.md).
Nothing in this directory is imported by a production core. No dependencies are added.

Use the checkout's pinned Node/Ruby and existing `impl/ts/node_modules` dependencies.
The bundler is the Rolldown already installed with Vite; Playwright/Chromium are
also existing browser-test tools. This task never installs packages or browsers,
provisions reference sources, or starts PostgreSQL.

```sh
mise run experiment:query-compilation
# Optional Linux affinity, choosing a CPU allowed on your host:
JED_COMPILE_CPU=3 mise run experiment:query-compilation
```

`JED_COMPILE_RESULTS` changes the default output directory
`bench/results/query-compilation`. `JED_COMPILE_PLAYWRIGHT` can point at an already
installed Playwright `index.mjs`. If it is missing, browser checks are omitted and
`environment.json` records this. Browser launch failures fail the task; no
automatic download is attempted. Browser checks use a temporary loopback HTTP
server, closed when the checks complete. `JED_COMPILE_ONLY=browser,jit-evidence`
can rerun only those checks after an interrupted run; other result files are
retained, and the environment records the requested subset.

`kernels.mjs` tests three small hand-authored integer-typed resolved-expression
fixtures against the actual TS evaluator and Meter. The compiler is deliberately
not a general SQL compiler: it lacks production admission/type proof, plan-cache
integration, booleans/CASE, operator integration and resource ownership. All modes
retain boxed values and checked BigInt arithmetic helpers. The closure mode
removes node dispatch without generating source. The generated mode emits only
fixed templates and ordinals and keeps values/operators/types in a separate pool.

The task creates a plain-JS bundle once, then runs three normal processes and
one JIT-disabled process, sequentially. Each process uses 50,000 warmups followed
by five samples of 100,000 evaluations per mode, alternating order. Checksums and
semantic costs must match. Differential checks run before and after the timed
jobs. GC stays enabled. Timings exclude preparation and validation. Compilation
measurements are single observations in an already running process, not pristine
cold-start distributions. `postValidationSingleCallUs` includes one-row harness
overhead after validation and is not a cold first invocation.

A separate `--trace-opt --trace-deopt` process verifies completed TurboFan
optimization for each generated job. Its timings do not belong in performance
comparisons. JIT-disabled execution needs prebuilt JS: native TS stripping in
the tested Node needs WASM, unavailable under its `--jitless` configuration.

`platforms.mjs` tests Node flags/VM contexts and real Chromium page/worker CSP
headers. It runs probes inside page scripts, not DevTools evaluation, and tests
normal URL workers and blob workers separately. Trusted Types checks concern
ordinary strings, not a host-approved TrustedScript integration. The browser
probe tests capability, not performance. Other browsers and extension policies
were researched but not executed.

Recorded [2026-10-19 results](results/2026-10-19/) retain all timing samples,
capability results, source/bundle hashes and the separate V8 trace. The compiler
and benchmark are mechanism probes, not end-to-end query or conformance evidence.
Rust/Go findings come from source inspection, primary documentation and the
separately committed expression-VM experiment, as detailed in the investigation.
