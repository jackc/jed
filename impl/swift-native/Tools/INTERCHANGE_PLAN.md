# Frozen file interchange acceptance (V10)

`mise run swift-native:interop` is opt-in. It builds four small transports using
only each native core's public host API, copies the three native executables and
Node transport into a retained evidence directory, then runs the complete matrix.
It does not add Swift to supported-core/default tasks or introduce dependencies.
`--no-build` requires all actors to exist; absent actors fail. `--check-adapter`
checks framing and coverage without building cores. `--inventory` writes the
applicability inventory and required phases without claiming an acceptance run.

The input is the exact 112 `.jed` images in the experiment's recorded baseline.
Every input hash is checked against `SpecSnapshot.json` and the independently
reviewed `Interop/fixtures.json` applicability inventory. The separate 24 integrity
finding images are outside this 112-image gate. The public quick/full integrity
API and its separate native adapter now pass all 50 expected reports from those 24
images, with exact golden open membership checked separately. See
[INTEGRITY_PLAN.md](INTEGRITY_PLAN.md). The simulation image oracle now calls the
same public API; its live resident-tree checker retains its separate contract.

For each Rust, Go and TypeScript peer, both Swift-first and peer-first branches
start from identical fixture bytes. Writable fixtures undergo original-row
self-updates plus sidecar insertion, foreign reopen/readback/update/delete, and a
final reopen in the original writer. Snapshots include every original base-table
value and all nine portable catalog relations, including statistics before
mutation invalidates them. Every snapshot is persisted as JSON. The two branches
must have equal SQL snapshots and byte-identical committed file extents after
both write phases. Bytes beyond the selected committed high-water mark are not
part of the committed image. Each written image also passes the separately
SHA-pinned Ruby format/CRC/overflow/tree/live-page checker.

Applicability is explicit rather than filtered on outcomes:

- 77 ordinary, 18 recovery and six encrypted fixtures are writable. Recovery
  probes assert the expected selected-root rows before mutation. Encrypted
  fixtures use their published test credential, raw `4A` repeated 32 times.
- Four images must reject opening with `XX001`, unchanged on disk.
- Five lazy-corruption images must first return their documented intact rows,
  then reject the exact corrupt read or commit with `XX001`, unchanged on disk.
- The collation-skew image must read with current Unicode ordering and reject
  its original table's write with `XX002`; the sanctioned collation upgrade then
  permits original-row mutation.
- Unparsable stored definitions retain their documented `XX003` errors. The
  global and table-filtered `jed_columns` errors are asserted explicitly. The
  six original base-table schemas are taken from the pinned fixture generator,
  and their complete table inventory and rows are checked alongside raw
  views/indexes/constraints. Only the documented
  writable original tables `t` and `d` are self-updated. Unknown definitions are
  never silently repaired to make the matrix pass.

Generated cases additionally cover fresh plaintext, raw-key and passphrase
files, both writer orders, large overflow values, secondary expression indexes,
views, sequences, exact decimals, arrays, ranges and foreign attachments. The
actors use frozen Unicode/JTZ data, the same immutable `geo_hash` host function,
public file create/open/close, explicit no-lock operation for sequential handoff,
synchronous durability, and test-only deterministic entropy. Scratch encryption
is explicitly on in every actor so a plaintext main can legally attach encrypted
files. Missing, wrong and inappropriate plaintext credentials are exercised.

Passphrase bytes are also checked: a private copy is rewrapped through the public
credential API to the published raw key, and the adapter first proves that only
the two key-block pages changed. The independent checker then authenticates and
validates the unchanged meta/body. The original passphrase file is used for all
cross-core opens, mutations and byte comparisons.

The full plan has 2,691 required phases. Duplicate, absent or unexpected phases,
missing files, malformed protocol output, actor crashes/timeouts, changed actors,
changed TypeScript engine source, image-checker failures and any SQL/byte mismatch
fail the task. The report retains successful phases and every missing phase after
a failed case. Filters, peer subsets and `--fixtures-only` are labelled diagnostic
subsets. A complete matrix is not evidence of success unless its failure list is
empty. The corrected full matrix passed all 2,691 phases with zero failures;
the retained report is `/tmp/jed-swift-interchange-20261106-19-17b0uj/report.json`
and `actors.json` records the exact executable hashes. The first run's opaque
metadata failures remain recorded separately in its original evidence directory.
