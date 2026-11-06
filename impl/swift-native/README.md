# Native Swift experiment

**Work in progress. SQL and host API implementation is incomplete.**
The acceptance target is the full supported jed surface at
`362e0798ac3a8bd04bd2da556044b6ec73bf468a`. See [PROGRESS.md](PROGRESS.md) for
implemented and verified foundations, current failures/omissions and resume steps.
The [experiment design](../../spec/design/swift-native-experiment.md) and its linked
scope/host matrices define completion.

The independent engine is handwritten Swift. Approved crypto utilities are isolated
in `JedCrypto`, using exact-pinned SwiftPM dependencies and pinned reference Argon2
source; applications need no system crypto library. This experiment does not select
production Swift delivery or claim macOS/iOS support. Linux uses Swift 6.4.2.

Run from the repository through explicit opt-in tasks:

```
mise run swift-native:resolve
mise run swift-native:codegen --check
mise run swift-native:build
mise run swift-native:test
mise run swift-native:build --configuration release
mise run swift-native:unicode -- --help
```

Normal repository CI, tests, benchmarks and generated website references do not
include Swift. Generated metadata is pinned to the recorded baseline Git blobs;
updating supported cores does not refresh the experiment. Completion is followed
by a freeze, with promotion subject to a separate decision.
