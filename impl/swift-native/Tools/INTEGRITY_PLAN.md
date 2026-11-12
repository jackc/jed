# Frozen public integrity acceptance

The native Swift API implements the frozen I1–I5 contract in
`spec/design/integrity-check.md`, with the 32 finding kinds and 5 skip reasons from
`spec/integrity/findings.toml`. The registry and all 24 damaged images/JSON reports
are mechanically copied from baseline
`0b76dd6e9c2675b552eb8af9e820065351ffa4b3`; no checker algorithm is generated.

`Database.integrityCheck(_:options:cancellation:)` checks one committed main or
named attachment. It retains the database host and an ordinary reader pin,
captures matching metadata, then releases both on every exit. It acquires no
writer lease. The default options are full checking, no constraint checks, and
100 findings; zero permits unlimited findings. Unknown domains, including TEMP,
return 42704. Cancellation, role loss and physical I/O failure propagate as errors.
XX001 from damaged persisted structures becomes a finding only at the specified
containment boundary. A final cancellation poll also runs after finding-limit
truncation.

The checker walks physical trees with bounded depth and a reachability bitmap,
then replays rows or index entries as needed. It does not collect the database's
decoded rows. Quick checking covers generation/catalog/tree/overflow/key/space
invariants; full checking adds real index agreement for ordered, expression,
partial, GIN and GiST indexes. Optional constraints check column nullability and
typmods, named CHECKs, unique constraints and foreign keys in canonical order.
Unavailable definitions or pinned environment data produce specified skips.
Each independent derived-row evaluation gets a fresh scratch expression context;
it does not consume a session's SQL cost or query-memory account.

A local file check rereads both meta slots and current manifest dependencies;
memory and page-client captures preserve their specified exceptions. GiST open
reconstructs its resident tree from persisted leaf keys, as all three frozen
cores do. It therefore accepts an incorrect persisted interior bound so the
ordinary integrity API can report `gist_bounds_violation`. The old Swift-only
expectations rejecting that bound and unused trailing bytes at open were
corrected; cycle, fanout, framing, checksum, pointer and depth defenses remain.
The native test also proves that either harmless-to-open edit reconstructs the
same canonical serialized tree. Unused trailing bytes are ignored by the frozen
read/check contracts; no new finding was invented for them.

`mise run swift-native:check -- [options] FILE` runs the opt-in `jed-check`
executable. It opens read-only, supports text/JSON and the existing raw-key/file/
no-echo passphrase channels, and uses the public integrity API. Exit 0 means no
findings; 2 means findings or an XX001 `open_failed` report; 1 means usage or another
open/check failure. It neither repairs the image nor introduces a default CLI
entry point. The simulator's image check now invokes this same public API on the
opened copy; its independent live-root reachability checker remains unchanged.

Independent reviews covered the generation/pin lifetime, tolerant tree framing,
GiST persisted-bound checks, index/constraint ordering, fatal-error propagation,
CLI, simulator hook and exact fixture membership. The first complete focused
rerun passed 21 tests: 6 integrity, 6 GiST, 4 simulation-integrity and 5 generated-spec
tests. Integrity evidence includes all 24 damaged images and exactly 50 expected
reports, all 112 goldens with 108 successful opens and 4 exact XX001 refusals,
attached/memory domain lookup, cancellation including the final CHECK callback,
checks beside a writer, live metadata/catalog corruption and physical-read
failure. A separate actual Unix-socket test passes with both plaintext and
encrypted clients, overflow values, committed visibility beside a writer,
owner-pin cleanup and role-loss propagation.

The initial logs are `/tmp/jed-swift-native-integrity-first-retry.log` (all new
integrity tests passed; two obsolete GiST assertions failed) and the successful focused/socket reruns in
`/tmp/jed-swift-native-integrity-second.log` and
`/tmp/jed-swift-native-integrity-sockets-retry.log`.

`mise run swift-native:check-smoke --no-build` passed all 50 frozen CLI JSON
reports plus 11 text/credential/usage/exit probes, with zero failures and all 51
copied fixture/report inputs byte-identical afterward. Its independently
reviewed driver captures the executable before launching any probe. Evidence:
`/tmp/jed-swift-integrity-cli-20261106-675919-qw5gej/report.json`, log
`/tmp/jed-swift-native-integrity-cli-first.log`, executable SHA-256
`d4f3288582f7e803b5cbbb4868db896695d9c96f8bc8a0dfe2ce933dcf3ef5d2`.
These earlier file-based probes did not drive the interactive prompt. Final
native/simulation acceptance is recorded separately in [VERIFICATION.md](../VERIFICATION.md).

The Linux interactive no-echo caveat is now closed by a separate PTY check of the
postoptimization release `jed-check` actor (SHA-256
`887b902725495e796643f4f19cf31406f3aeea8ce1fa1fc9c212c27c59d3588f`). The tested
implementation source is `3db8fc2d2653475abc5e9d43049254ccd1e92387`; the later
benchmark capture HEAD `7c4803e9` is a separate provenance fact, not the CLI's
actor identity. The temporary mise task uses Ruby's standard-library PTY and
io-console support, keeps the slave terminal open for observation, checks the
exact `stty -a` ECHO flag at each live prompt, and compares the complete `stty -g`
state before and after the child. Correct, wrong and empty passphrases all pass:
the correct phrase exits 0 with a JSON integrity report (six pages, five rows, no
findings; the unavailable `hosted_geo` function is explicitly skipped), while
wrong and empty phrases exit 1 with `28P01` and the empty-passphrase usage error.
No entered passphrase appears in captured terminal output, all three terminal
states are restored exactly, and every child is reaped under bounded deadlines.
Only copies of the completed generated interchange image were opened; its
SHA-256 `121ca8f66ea92357f9d99427e29a550b5e87e61d3c4b996b10b949d047828ab0`
and all copied database bytes remain unchanged. Script, temporary mise task,
transcripts, terminal-state evidence and final results are retained under
`results/cli-pty-postoptimization-20261106/verified/`, with
`results/cli-pty-postoptimization-20261106/verified.log`. Earlier harness attempts
are preserved beside them: Ruby's `echo?` also considers ancillary echo modes
and therefore reported true although the actual ECHO bit was cleared
(`8a7b` to `8a73`, with `-echo` in `stty -a`). This was a measurement correction,
not a CLI source change. These Linux PTY results make no Apple-platform claim.
