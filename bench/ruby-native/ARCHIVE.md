# Ruby experiment evidence archive

The persisted Ruby core remains frozen, experimental and opt-in. Its library,
tests, reusable benchmark tools and human-readable reports are retained. The
cleaned history omits generated measurements, source tarballs, status logs and
run-specific drivers under `bench/ruby-native/results/` and
`impl/ruby-native/results/` from every experiment commit. New output in those
directories is ignored by Git.

The complete original history is preserved locally on
`codex/ruby-native-experiment`, at `674dc97eb9d05fd0527df9c63fa1badf7c70b673`. The archive includes failed
attempts, slower measurements and rejected candidates; filtering does not
change any historical measurement. This archive tip has not been pushed.
A clone containing only the cleaned branch will not contain that evidence.
Keep the original branch when retaining or transferring the research archive.

The cleaned branch is based on master `23b2e40add6a19e04c59872dc5bdef5da329974b`.
All 40 experiment commits keep their order, authors, messages and author timestamps;
each filtered tree differs from its corresponding unfiltered rebased tree only by the two
omitted result directories. The final documentation/tooling cleanup is a
separate commit. No Ruby engine or host-test changes were needed for rebasing.
Master and the original experiment branch were not rewritten.

Historical reports under `impl/ruby-native` describe their recorded revisions,
not newly measured versions of master. Their archived evidence links lead here
and retain the original file path. Commands in historical reproduction sections
require the original checkout, its recorded runtimes and supporting fixtures.
The compact [final benchmark report](../../impl/ruby-native/FINAL_BENCHMARKS.md)
retains the final source's comparison results and API limitations.

## Retrieve evidence

When the original branch is present, inspect an individual report with:

```sh
git show codex/ruby-native-experiment:bench/ruby-native/results/join-key-followup/tables.md
```

Or inspect the complete original checkout in a separate directory:

```sh
git worktree add --detach /tmp/jed-ruby-evidence 674dc97eb9d05fd0527df9c63fa1badf7c70b673
```

New opt-in benchmarks use the current Ruby source by default. To reproduce
an archived `baseline` or `working-baseline` phase, restore its source archive
explicitly; the helper validates the pinned original library checksum:

```sh
mkdir -p /tmp/jed-ruby-baseline
git show 674dc97eb9d05fd0527df9c63fa1badf7c70b673:bench/ruby-native/results/baseline-source.tar.gz | tar -xz -C /tmp/jed-ruby-baseline
RUBY_NATIVE_SOURCE=/tmp/jed-ruby-baseline RUBY_NATIVE_PHASE=baseline mise run ruby-native:compare:source-check
```

For historical file-comparison lanes, restore `working-baseline-source.tar.gz`
instead and set `RUBY_NATIVE_BASELINE_SOURCE` to that extracted directory.
Default file comparisons select only current sources. No benchmark silently
substitutes current code for a missing historical baseline.

## Commit mapping

Original hashes in historical reports refer to the preserved research branch.
This mapping identifies their rebased, filtered counterparts for review:

| Original commit | Cleaned commit | Subject |
| --- | --- | --- |
| `dcbfbd8e415a` | `0805f0ffb538` | Add opt-in pure Ruby engine experiment and recorded prototype baseline |
| `9ab8ab16c706` | `436cad9042aa` | Expand Ruby experiment SQL coverage, resource kernels, indexes, and host domains |
| `449ad1e7a1e8` | `484ce7877255` | Advance Ruby experiment with shared scan plans, joins, Unicode, and memory accounts |
| `f0f86b485c78` | `255d5b569da3` | Advance experimental Ruby joins, spill, and incremental storage |
| `06b2708b0f93` | `a1ec6e846a98` | Expand experimental Ruby core parity, sessions and lazy file reads |
| `e704a4550c51` | `3f88db78efb6` | Expand experimental Ruby host, page service, and paged writes |
| `4caec0902e08` | `e5a83e439c82` | Complete experimental Ruby parity and bound validation memory |
| `8c5c6df9581f` | `00990fd890c7` | Record optimized Ruby benchmarks and freeze the experiment |
| `8fbc52590ea5` | `b44acbcc1c25` | Optimize experimental Ruby lookups and measure Ruby 4.1 JITs |
| `9e3071e3992b` | `ba1e6bda03da` | Optimize experimental Ruby point cursors and page decoding |
| `fa52671161d8` | `d9d772fa0521` | Optimize experimental Ruby scans, aggregates, and sorting |
| `c313e1038fa1` | `dd25ac42159d` | Optimize experimental Ruby scalar queries and validate lazy execution |
| `0912e9e2eff4` | `8ebca822abcf` | Compile experimental Ruby query expressions and optimize host APIs |
| `95401ae24343` | `c3d64a66d1c4` | Compile experimental Ruby single-table scan pipelines |
| `4114e8d7dd5f` | `c55f9441bd9a` | Fix experimental Ruby cursor resolution, depth, and scan costs |
| `83fb482b1e9d` | `810e4dd68f47` | Compile experimental Ruby ORDER BY comparators |
| `e888a0df4df3` | `b91954710e42` | Compile experimental Ruby primitive aggregate loops |
| `d385dc0f4ce2` | `5bceac4e247d` | Compile experimental Ruby materialized relation loops |
| `5ca7358167db` | `2518f7fc2f5e` | Compile experimental Ruby C-text comparisons |
| `651783e0f386` | `769b753df154` | Optimize experimental Ruby text comparison snapshots |
| `f78aa2b741f3` | `b219fdc806b6` | Fix experimental Ruby cursor conformance and memory ownership |
| `b70950c13f49` | `b5c2ac0face1` | Compile literal LIKE predicates in experimental Ruby core |
| `99ff18afb7a4` | `ba37cae3e7a9` | Share generated Ruby methods across constant pools |
| `5874935b65f3` | `c7737ffb3038` | Compile broader literal LIKE patterns in experimental Ruby core |
| `ab8dd2f4e233` | `c5ab4a20d2ab` | Record broad Ruby compilation performance checkpoint |
| `1a81035e3f69` | `83e31071fa06` | Reuse prepared Ruby join resolution |
| `f49ab392421b` | `98f1a0972462` | Reuse Ruby resolution for multiway joins |
| `eaf1886f6398` | `ee7d6c977724` | Reuse Ruby physical join plans for value-independent selection |
| `013e1cf55666` | `588cf64f9aa2` | Reduce Ruby query setup allocations and guard deep predicates |
| `ac50d8834e37` | `430467511950` | Reduce Ruby expression traversal allocations |
| `4261338c5a43` | `663ef7acd0f2` | Reuse Ruby multiway LEFT join resolution |
| `c0b704ab69f5` | `ea97a6a20d23` | Archive measured Ruby planner-work candidate and rejection evidence |
| `2972d126d16b` | `510a64c8561c` | Restore original estimator after inconclusive JIT measurements |
| `dadcdc2e3114` | `a941af3a915d` | Compile Ruby JOIN predicates with bounded source-pool reuse |
| `d8353e1bcaae` | `ce626bef25a7` | Reduce Ruby JOIN compilation setup for small inputs |
| `1b51383303b8` | `faa7e696524e` | Reuse Ruby JOIN resolution for scalar builtins |
| `85c30100e6cd` | `ee8ba0b9dd2b` | Reuse Ruby indexed JOIN bound structure per execution |
| `40d5fcaef6fa` | `c81c6ed1dc0a` | Archive Ruby N-way frontier optimization measurements |
| `2f23527f2ace` | `9cea8cb86626` | Restore Ruby planner after inconclusive frontier optimization |
| `674dc97eb9d0` | `f06d924bb101` | Share Ruby prepared JOIN column descriptors |
