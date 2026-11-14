# Recorded experiment evidence

`acceptance.json` is the complete 42-gate report for the frozen source identified
in [VERIFICATION.md](../VERIFICATION.md). It retains commands, actor identities,
exit statuses and SHA-256 hashes. `acceptance-logs/` contains the exact gate logs,
compressed with gzip; `acceptance-log-index.json` records both compressed and
original hashes. Each variant's artifact manifest identifies its source, class,
JAR and dependency bytes. Process actor manifests identify the independent peers.
`fault-receipts.json` records all 255 completed shared traces across the three
variants, their generator/input hashes, event counts and both transcript hashes;
228 exact transcript pairs were independently compared when producing it.

`repository-checks.json` and its compressed logs record the successful default
18-step CI run and Java generated-data check. The default gate does not run Java.

`benchmark-before-aggregate/` preserves the complete three-repeat, five-actor,
24-workload measurement before the resident aggregate optimization. It includes
raw samples, the readable summary, host activity observations and their hashes.
Unidentified overlapping Java processes prevent an idle-host claim for this run;
the report explicitly treats its timing ratios as shared-host observations.
`acceptance-before-aggregate/` preserves that source's complete correctness
acceptance separately, so later optimization evidence does not erase the baseline.

`benchmark-final/` records the accepted source's complete 24-workload, five-actor,
three-repeat comparison and 72 Java memory probes. All values and costs match.
The archive contains raw samples, conventional-median summaries for both source
snapshots, workload input, safe process observations, analysis scripts and hashes.
These remain shared-host diagnostics: background CPU and ongoing compilation
limit timing conclusions. See [BENCHMARKS.md](../BENCHMARKS.md) for measured gains,
remaining gaps and the distinction between raw upper medians and derived medians.

Absolute paths in receipts describe the original local run. Large actor binaries,
raw fault images and full captured source trees remain in the ignored development
directory. They are not published release artifacts. The source is committed here;
the supported peer/spec revision and toolchains are pinned. Reproduce the gate
with `mise run java-native:acceptance --evidence-dir NEW_EMPTY_DIRECTORY` after
the explicit Java setup task. See the experiment README for prerequisites.
