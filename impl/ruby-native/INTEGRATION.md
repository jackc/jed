# Ruby experiment integration review

Prepared branch: `codex/ruby-native-cleanup`, based on master
`7d179be9674fb3ddcc1e64519b978488c65df471`.
The original `codex/ruby-native-experiment` remains at
`48fd21cd34b1001b23aa2b8c04c4d53dd591fbee`, including all historical evidence.
This preparation does not merge to master or publish either branch.

## History and retained scope

All 40 experiment commits were rebased, then filtered individually to omit
`bench/ruby-native/results/` and `impl/ruby-native/results/`. The original
authors, messages and author timestamps are preserved. Each filtered tree was checked
against its rebased source; only the two result directories differ. The
[commit mapping and evidence retrieval instructions](../../bench/ruby-native/ARCHIVE.md)
identify the corresponding commits and archived files.

Those directories accounted for about 95% of the original 1,660,094 added lines.
The remaining change is approximately 84,000 added lines, mostly the independent
Ruby implementation and its tests. The final cleanup commit adds archive notices,
a [compact final benchmark report](FINAL_BENCHMARKS.md), ignored result paths,
and explicit historical-source selection for benchmark tools.

Review the remaining areas separately:

- `impl/ruby-native/lib`: handwritten engine and frozen generated catalog/schedule.
- `impl/ruby-native/test`: host APIs, storage bytes, resource boundaries and compiler
  invariants; unchanged from the original experiment tip.
- `impl/ruby-native/bin` and `tools`: explicit conformance/process adapters,
  mechanical generators and bounded worker supervision.
- `bench/ruby-native`: reusable opt-in harnesses, plans and diagnostics. Current
  source is the default; historical phases require explicitly restored, verified
  sources. Run-specific drivers remain in the original archive.
- `AGENTS.md`, `CLAUDE.md`, implementation/design indexes and the experiment spec:
  frozen, opt-in policy and the previously authorized compilation/utility exceptions.
- `mise.toml` and `scripts/process_conformance.rb`: explicit Ruby-native tasks and
  an optional process actor; normal task/core lists remain unchanged.
- Four shared corpus files and `oracle_overrides.toml`: regression coverage for
  repeated integer parameters, deep flat IN lists, cast pushdown and analyzed JOIN
  estimates. These are deliberately retained correctness work from the experiment.
- `spec/design/api.md` and the website query documentation: the matching clarification
  of existing repeated-integer-parameter behavior. `Gemfile` changes only a comment.

No supported Rust, Go or TypeScript engine implementation is changed by this
branch relative to master. Master's newer work is retained. The Ruby library,
host tests, runtime adapters and internal tools are byte-identical to the original
experiment tip. Its library SHA-256 remains
`4d67d30ffbd9163214d1164c1c9c11dcdf5ca63e260a2b502402cc741314bf91`.

## Experimental status and validation

Ruby native remains outside normal test, CI, benchmark and code-generation tasks,
and outside the Rust/Go/TypeScript differential set. Future features do not update
it automatically. Rebase does not expand its recorded feature coverage or promote
it to supported status.

On the cleaned, rebased checkout:

- All 71 Ruby host-test files passed with YJIT under the explicit `ruby-native:test`
  task, with a 512 MiB RSS guard; peak sampled RSS was 231.1 MiB.
- Benchmark source-selection regressions and file-comparison preflight tests passed.
  The first preflight invocation used an outer virtual-address limit too small for
  V8; it passed with the harness's existing 16 GiB virtual allowance and the same
  RSS limit. No engine or preflight assertion was relaxed.
- Both restored historical source archives matched their pinned library checksums.
- All 289 Ruby implementation/tool/test files passed syntax compilation.
- Local historical-report links were checked after archive-path conversion.
- An independent audit verified the 40 filtered trees, preserved master changes,
  unchanged normal task/core lists, and absence of runtime dependencies on the
  removed result files.

The earlier full corpus results remain historical evidence for the recorded
revision. This cleanup does not claim a fresh full CI or full expanded-corpus run.
Review the shared changes and run the applicable merge gate before integration.
Master can then advance with a fast-forward only, provided it has not moved.

```sh
git diff --stat master...codex/ruby-native-cleanup
git log --reverse --oneline master..codex/ruby-native-cleanup
git diff master...codex/ruby-native-cleanup -- . ':!impl/ruby-native' ':!bench/ruby-native'
```
