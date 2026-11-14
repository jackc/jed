# Recorded experiment evidence

`acceptance.json` is the complete 42-gate report for the frozen source identified
in [VERIFICATION.md](../VERIFICATION.md). It retains commands, actor identities,
exit statuses and SHA-256 hashes. `acceptance-logs/` contains the exact gate logs,
compressed with gzip; `acceptance-log-index.json` records both compressed and
original hashes. Each variant's artifact manifest identifies its source, class,
JAR and dependency bytes. Process actor manifests identify the independent peers.

`repository-checks.json` and its compressed logs record the successful default
18-step CI run and Java generated-data check. The default gate does not run Java.

Absolute paths in receipts describe the original local run. Large actor binaries,
raw fault images and full captured source trees remain in the ignored development
directory. They are not published release artifacts. The source is committed here;
the supported peer/spec revision and toolchains are pinned. Reproduce the gate
with `mise run java-native:acceptance --evidence-dir NEW_EMPTY_DIRECTORY` after
the explicit Java setup task. See the experiment README for prerequisites.
