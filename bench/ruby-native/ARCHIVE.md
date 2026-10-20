# Ruby experiment evidence archive

The persisted Ruby core remains frozen, experimental and opt-in. Its library,
tests, reusable benchmark tools and human-readable reports are retained. The
cleaned history omits generated measurements, source tarballs, status logs and
run-specific drivers under `bench/ruby-native/results/` and
`impl/ruby-native/results/` from every experiment commit. New output in those
directories is ignored by Git.

The complete original history is preserved locally on
`codex/ruby-native-experiment`, at `48fd21cd34b1001b23aa2b8c04c4d53dd591fbee`. The archive includes failed
attempts, slower measurements and rejected candidates; filtering does not
change any historical measurement. This archive tip has not been pushed.
A clone containing only the cleaned branch will not contain that evidence.
Keep the original branch when retaining or transferring the research archive.

The cleaned branch is based on master `7d179be9674fb3ddcc1e64519b978488c65df471`.
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
git worktree add --detach /tmp/jed-ruby-evidence 48fd21cd34b1001b23aa2b8c04c4d53dd591fbee
```

New opt-in benchmarks use the current Ruby source by default. To reproduce
an archived `baseline` or `working-baseline` phase, restore its source archive
explicitly; the helper validates the pinned original library checksum:

```sh
mkdir -p /tmp/jed-ruby-baseline
git show 48fd21cd34b1001b23aa2b8c04c4d53dd591fbee:bench/ruby-native/results/baseline-source.tar.gz | tar -xz -C /tmp/jed-ruby-baseline
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
| `7a2fe03e1b0c` | `1dd96fd3ad28` | Add opt-in pure Ruby engine experiment and recorded prototype baseline |
| `8d586505c8a8` | `d089a1c0953c` | Expand Ruby experiment SQL coverage, resource kernels, indexes, and host domains |
| `29d84d42ab4a` | `0d28753e4e28` | Advance Ruby experiment with shared scan plans, joins, Unicode, and memory accounts |
| `704ed1b13baa` | `32e7df6ceeba` | Advance experimental Ruby joins, spill, and incremental storage |
| `ee545444af8b` | `34be5babd50e` | Expand experimental Ruby core parity, sessions and lazy file reads |
| `53253af1ffec` | `f5f61e3812da` | Expand experimental Ruby host, page service, and paged writes |
| `7807e37dd94e` | `02c0b6746a07` | Complete experimental Ruby parity and bound validation memory |
| `c3ba8e69f556` | `1c1bd4d677ff` | Record optimized Ruby benchmarks and freeze the experiment |
| `222b81aeddfd` | `876160349971` | Optimize experimental Ruby lookups and measure Ruby 4.1 JITs |
| `81dfcbb0d0de` | `51a458b501b3` | Optimize experimental Ruby point cursors and page decoding |
| `28b12a8acca8` | `47c224474ce0` | Optimize experimental Ruby scans, aggregates, and sorting |
| `e5dbbe514e41` | `e158cac3a72c` | Optimize experimental Ruby scalar queries and validate lazy execution |
| `627cbb631e3a` | `9d00d10f4e2f` | Compile experimental Ruby query expressions and optimize host APIs |
| `39f804e416fd` | `1297ace340da` | Compile experimental Ruby single-table scan pipelines |
| `e8ec94e5ef5a` | `b2ba360645f4` | Fix experimental Ruby cursor resolution, depth, and scan costs |
| `8243d0bc660c` | `1199e5872b89` | Compile experimental Ruby ORDER BY comparators |
| `fa4cbcc18dd1` | `68b3f6d9d906` | Compile experimental Ruby primitive aggregate loops |
| `8347075b8b23` | `df737f5379f7` | Compile experimental Ruby materialized relation loops |
| `2f947c81012f` | `1e5c944d7283` | Compile experimental Ruby C-text comparisons |
| `fc705ce719bf` | `33c8aba52e21` | Optimize experimental Ruby text comparison snapshots |
| `96b2f163f160` | `5057aa6f03a5` | Fix experimental Ruby cursor conformance and memory ownership |
| `fc12f4279842` | `fc737b48ba18` | Compile literal LIKE predicates in experimental Ruby core |
| `f8fa53cdee02` | `95e486173464` | Share generated Ruby methods across constant pools |
| `e914ae112753` | `0bb39fe319af` | Compile broader literal LIKE patterns in experimental Ruby core |
| `ef79bb744258` | `393981fe262a` | Record broad Ruby compilation performance checkpoint |
| `a612935772c9` | `e54f95b5517c` | Reuse prepared Ruby join resolution |
| `b5262268371f` | `21ba7fe273db` | Reuse Ruby resolution for multiway joins |
| `aa8783c99afd` | `203575e0cee4` | Reuse Ruby physical join plans for value-independent selection |
| `bd29129f6cb0` | `413233dc4738` | Reduce Ruby query setup allocations and guard deep predicates |
| `7a70c9f2543d` | `31240a0ddb82` | Reduce Ruby expression traversal allocations |
| `eb2c83db21bc` | `9977af98a824` | Reuse Ruby multiway LEFT join resolution |
| `d8084fb6e44b` | `1c5ea3650114` | Archive measured Ruby planner-work candidate and rejection evidence |
| `1657722bc9a8` | `f1a0302ea437` | Restore original estimator after inconclusive JIT measurements |
| `7b854f1f3755` | `2b82726df969` | Compile Ruby JOIN predicates with bounded source-pool reuse |
| `1056d146c294` | `12bbd2ed0d7f` | Reduce Ruby JOIN compilation setup for small inputs |
| `25a1a7cce500` | `6bb6bda959af` | Reuse Ruby JOIN resolution for scalar builtins |
| `7028f9bca642` | `15c444582e87` | Reuse Ruby indexed JOIN bound structure per execution |
| `c4094c6a3f20` | `00aaa7af54e0` | Archive Ruby N-way frontier optimization measurements |
| `e9c8cabc2843` | `b61c858554e1` | Restore Ruby planner after inconclusive frontier optimization |
| `48fd21cd34b1` | `651edb1f4e6a` | Share Ruby prepared JOIN column descriptors |
