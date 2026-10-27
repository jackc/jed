# Integrity check — design

> A host-API operation that checks one database's storage on a live handle and returns structured
> findings: SQLite's `PRAGMA integrity_check` / `quick_check`, and the job PostgreSQL's `amcheck`
> (`bt_index_check … heapallindexed`) does for its B-trees. **Status: designed, not built.** The
> invariants already exist as test-only checkers in each core's simulation runner and in
> `spec/fileformat/verify.rb --check-image` ([simulation-testing.md](simulation-testing.md) §8.3); this
> doc moves them into the engine, extends them where they stop short, and fixes the result as a
> cross-core contract.

## 1. Why, and what it is not

jed already detects corruption on every read: a page whose checksum (or, in an encrypted file, whose
AEAD tag) does not match is `XX001` ([storage.md](storage.md) §6). Detection on read has two gaps:

1. **Nothing reads everything.** A damaged leaf, overflow page, or index page is found only when a
   query happens to touch it, possibly months later. A host wants to ask "is this file sound?" after
   a crash, a copy, a restore, a disk warning, or before shipping a prebuilt database.
2. **A valid checksum is not a valid database.** A page can carry the right checksum and still be
   wrong: an engine bug, a host that wrote a well-formed page to the wrong place, or a restore that
   mixed generations. Rows stored under a key that is not their primary key, an index missing an
   entry, a free page that is also live, or a live page count that disagrees with the reachable
   pages all pass every checksum. Only a walk that checks the invariants between pages finds them.

**Not in scope:**

- **Repair.** The check reports; it never writes. Recovery from a damaged file is a backup restore, a
  dump, or `compact` (which rewrites from the trees it can read). A `REINDEX` already rebuilds an
  index whose entries disagree with its table.
- **SQL access.** Like `compact` and `attach`, the check is a host-API act (CLAUDE.md §13). It is
  unmetered by design (§5.4), so exposing it to SQL would put unbounded work on the untrusted-query
  surface. A metered SQL form is a possible follow-on, not part of this design.
- **The engine's in-memory bookkeeping.** The simulation's *live checker* (resident trees against the
  in-memory free list) tests the engine against itself, depends on what happens to be resident, and
  has no action a host could take. It stays a simulation oracle.
- **Opening a file that will not open.** The check runs on an open handle. A file whose meta pages or
  catalog do not load fails `open` with `XX001` as today; the CLI reports that as a finding (§7), and
  `verify.rb --check-image` remains the tool that dissects such a file.

## 2. The call

| core | spelling |
|---|---|
| Rust | `db.integrity_check(name: &str, opts: &IntegrityCheckOptions) -> Result<IntegrityReport>` |
| Go | `db.IntegrityCheck(ctx context.Context, name string, opts IntegrityCheckOptions) (IntegrityReport, error)` |
| TS | `db.integrityCheck(name, opts?)` (internal core); `await db.integrityCheck(name, opts?)` on the public async API ([ts-runtime.md](ts-runtime.md) §10) |

`name` is `main` or an attached database, matched exactly, as for `compact` ([api.md](api.md) §2.6).
Both backings are supported: a file, and an in-memory database or attachment (its
`MemoryBlockStore` has the same pages, so the same walk applies; corruption there means an engine bug,
which is worth finding). `temp` is session-local and not a target.

**Options** (each core's idiomatic struct; the defaults are the zero/omitted values where the
language allows):

| option | default | meaning |
|---|---|---|
| `level` | `full` | `quick` or `full` (§3) |
| `constraints` | `false` | also re-check declared constraints against stored rows (§3.4) |
| `max_findings` | `100` | stop after this many findings; `0` means no limit |

SQLite's limit is also 100. The defaults make the plain call `integrity_check("main", default)` the
thorough structural check without the logical constraint pass, which is the one that may need a host
function or loaded data the handle lacks.

**Result:**

```
IntegrityReport {
  findings:  [Finding]     // in the contract order of §4; empty means the check found nothing
  truncated: bool          // max_findings was reached; more findings may exist
  skipped:   [Skip]        // objects a requested check could not cover, and why (§3.5)
  pages:     u64           // distinct pages the walk read and verified
  rows:      u64           // table rows decoded
}

Finding {
  kind:     string         // a registered kind (§4), stable snake_case
  database: string         // the `name` checked
  object:   string?        // the table, index, sequence, or constraint the finding is about
  page:     u32?           // the page the finding is about, when there is one
  key:      bytes?         // the row's storage key, when the finding is about one row
  detail:   string         // human-readable; NOT part of the contract
}

Skip { object: string, reason: string /* a registered skip reason (§3.5) */ }
```

Rust and TS add an `ok()` / `ok` convenience: `findings` is empty and `truncated` is false.

**Errors, as opposed to findings.** A finding is something wrong with the database. The call itself
fails only for:

- `42704` — `name` is neither `main` nor a current attachment;
- `57014` — canceled (Go `ctx`, Rust token, TS `AbortSignal`; §5.3);
- an I/O error from the block store, as the same read would fail a query (a device read error is not
  a statement about the bytes, so it is not a finding);
- `08006` — a page client lost its owner mid-check (page-service.md); the host reconnects and retries;
- the handle's own poison state (a handle poisoned by a failed commit refuses every operation, api.md
  §3).

`XX001` raised while reading a page is **never** a call error: it becomes a finding and the walk
continues (§5.2).

## 3. What is checked

The checks are grouped by level. `quick` is SQLite's `quick_check` boundary: everything except
agreement between an index and its table. Each check names the invariant's source of truth.

### 3.1 Generation (both levels)

The check pins the database's committed snapshot exactly as a read session does (§5.1) and examines
the generation that snapshot reads:

- **Meta.** The selected meta slot decodes and its checksum is valid; the commit manifest of the
  generation decodes, and each page it lists carries its recorded CRC-64 identity
  ([validated-cow.md](validated-cow.md) §1). On a live handle these already passed at open or commit;
  re-checking them finds a meta or manifest page damaged since.
- **Catalog.** Every catalog page verifies (checksum or tag) and decodes, and re-encoding the
  snapshot's catalog reproduces the stored chain byte for byte. The check then continues from the
  snapshot's catalog (the handle's loaded form), so a damaged catalog page is reported without hiding
  every table behind it.
- **Sequences.** Each sequence's current value lies within its bounds ([sequences.md](sequences.md)).

### 3.2 Trees and pages (both levels)

For each table (in the order of §4) and then each of its indexes:

- every page of the tree verifies and decodes as its declared page type ([format.md](../fileformat/format.md));
- B+tree shape: every leaf is at the same depth; within a node keys ascend strictly; every key lies
  within the separator bounds its parent gives the child (the check the simulation leaves to its
  digest, simulation-testing.md §8.3);
- table rows: storage keys ascend strictly across the whole tree, and for a table with a primary key
  each key equals the primary-key encoding of its row ([encoding.md](encoding.md)); every value
  decodes as its column's type, collation, and typmod-independent form;
- the table's persisted row count equals its leaf total (format v28);
- overflow chains: every out-of-line value's chain verifies, decodes (including LZ4,
  [../fileformat/lz4.md](../fileformat/lz4.md)) to the recorded length, and no overflow page is named
  by two stored values ([large-values.md](large-values.md) §15);
- index trees: entry keys ascend strictly; a GiST R-tree's node bounding boxes contain their
  children's ([gist.md](gist.md) §4).

### 3.3 Space accounting (both levels)

One reachability bitmap is filled during §3.1–§3.2 (§5.4). At the end:

- no page was reached twice (two trees sharing a node, a chain inside a tree);
- every reached page lies in the body range (`[2, page_count)`, `[4, page_count)` in an encrypted file);
- the live page count at meta offset 56 equals the number of reached pages (format v34);
- the free list (meta offset 28) verifies and decodes, has no duplicates, lies in the body range, and
  is disjoint from the reached pages and from the generation's manifest dependencies.

Free pages themselves are not read: their contents are dead by definition.

### 3.4 Index agreement (`full`) and constraints (`constraints`)

**`full`** adds that every index holds exactly the entries its table derives:

- for each table row, every entry the index definition derives from it (column keys, expression keys,
  the partial predicate's filter, GIN terms, GiST keys; [indexes.md](indexes.md), [gin.md](gin.md),
  [gist.md](gist.md)) is present in the index, found through the index's own lookup;
- the index holds no other entries: its entry count equals the total derived.

Presence of every derived entry plus equal counts plus strictly ascending, storage-key-suffixed
entries is exact equality, without materializing either side (§5.4). This is PostgreSQL's
`heapallindexed` and SQLite's per-row index probe. It covers every index kind, not only the column
B-tree indexes the simulation checker covers today.

**`constraints`** (independent of `level`) re-checks declared constraints against the stored rows,
which a valid file always satisfies:

- `NOT NULL` and column typmods (`varchar(n)` length, `numeric(p,s)`, temporal precision);
- `CHECK` constraints, evaluated as at write time ([constraints.md](constraints.md) §4);
- `UNIQUE` / primary-key uniqueness under the index's NULLS [NOT] DISTINCT rule, found as adjacent
  index entries with equal key prefixes;
- `FOREIGN KEY`: each referencing row's key, under its `MATCH` rule, is present in the referenced
  table's key index (constraints.md §6);
- exclusion constraints are a follow-on (§8).

Expression evaluation (expression indexes, partial predicates, `CHECK`) runs in the handle's
environment ([environment.md](environment.md)): its loaded collations and time zones and its
registered host functions. An evaluation that fails where the write succeeded is a finding
(`index_entry_error`, `check_error`), because the stored entry or row was accepted under a value that
no longer evaluates.

### 3.5 Skips

A requested check that cannot run on an object is a `Skip`, not a finding and not an error, so the
rest of the check proceeds. Registered reasons:

| reason | when |
|---|---|
| `unparsable_definition` | the object's stored text does not parse in this binary (`XX003`, [unparsable-definitions.md](unparsable-definitions.md)) |
| `missing_host_function` | an expression names a host function the environment does not register |
| `missing_loaded_data` | a collation or time zone the object pins is not loaded or is at another version (`XX002`, [collation.md](collation.md)) |
| `table_unreadable` | the table's tree had a finding, so index agreement and constraints for it are not checked (§5.2) |

## 4. Findings as shared data

Finding kinds and skip reasons are a registry, `spec/integrity/findings.toml`, authored once (CLAUDE.md
§5) and cross-checked by a unit test in each core, as the error registry is. Each entry gives the
kind, the check (§3.x) it belongs to, which of `object` / `page` / `key` it sets, and a description.
The initial kinds, by check:

| check | kinds |
|---|---|
| generation | `meta_invalid`, `manifest_page_mismatch`, `catalog_page_corrupt`, `catalog_mismatch`, `sequence_out_of_range` |
| trees and pages | `page_corrupt`, `page_type_wrong`, `tree_depth_uneven`, `node_keys_unordered`, `separator_violation`, `row_keys_unordered`, `row_key_not_primary_key`, `value_undecodable`, `row_count_mismatch`, `overflow_chain_corrupt`, `overflow_length_mismatch`, `overflow_page_shared`, `index_keys_unordered`, `gist_bbox_violation` |
| space accounting | `page_reached_twice`, `page_out_of_range`, `live_count_mismatch`, `free_list_corrupt`, `free_page_duplicate`, `free_page_out_of_range`, `free_page_live`, `free_page_manifest_dependency` |
| index agreement | `index_entry_missing`, `index_entry_count_mismatch`, `index_entry_error` |
| constraints | `not_null_violation`, `typmod_violation`, `check_violation`, `check_error`, `unique_violation`, `foreign_key_violation` |

**The contract.** For the same file bytes, credential, environment, and options, every core returns
the same `findings` (kind, database, object, page, key — everything but `detail`), in the same order,
with the same `truncated`, `skipped`, `pages`, and `rows`. `detail` is free text for humans. This is
what lets one damaged fixture test all cores at once (§6), and it is achievable because every core
walks the same bytes with the same rules in the same order:

1. generation (§3.1);
2. each table in name byte order: its tree (§3.2), then each of its indexes in index-name byte order
   (structure, then agreement under `full`), then its constraints under `constraints`, each in
   constraint-name byte order;
3. space accounting (§3.3).

Within a tree, findings follow a depth-first walk in child order, so page and row findings come in
key order. Per row, a constraint pass reports in column ordinal order. `max_findings` truncates this
sequence, so a truncated report is a prefix of the full one.

## 5. Execution

### 5.1 Snapshot and concurrency

The check pins the database's committed snapshot as a read-only session does
([transactions.md](transactions.md) §8) and holds the pin until it returns. It never takes the
writer gate: commits proceed during a check and the check does not see them. While the pin is held,
reclamation of pages freed after the pinned version waits, so a long check on a busy database lets the
file grow, as a long reader does. Under the page service ([page-service.md](page-service.md)) the
owner and page clients may both check; a client reads its pages through the owner and checks the
generation it pinned.

Pages are read through the handle's pager, so a page already resident was verified when it was
faulted and is not re-read from the device. A host that wants every byte read from storage checks a
freshly opened handle, which is what the CLI does (§7). The check's own faults are not admitted to the
cache beyond what the handle's ordinary cache policy does, so a check does not evict a working set
any more than a full scan would ([pager.md](pager.md)).

### 5.2 Containment

A page that fails to verify or decode is reported once, at the point the walk reaches it, and its
subtree is not walked: its rows and descendants are unknown, so the walk records neither them nor
the findings they would cascade into. A table with any §3.2 finding is reported with
`table_unreadable` in `skipped` for its index-agreement and constraint checks, since every derived
comparison against a partly read table would be noise. Its indexes are still checked structurally.
Space accounting runs after a damaged walk too, but `live_count_mismatch` and `free_page_live` are
then suppressed, because pages under an unwalked subtree are live but unreached; damaged subtrees
were already reported.

### 5.3 Cancellation

The check polls its cancellation handle at every page it reads and every row it decodes. A canceled
check returns `57014` and no report: a partial report would look like a clean one for the part not
checked.

### 5.4 Cost and memory

The check is a host operation, like `compact`: it is not metered, charges no session's cost or memory
account, and has no ceiling. Its memory is bounded and does not depend on rows:

- one reachability bitmap of `page_count` bits (a 1 TB file of 4 KiB pages is 32 MiB);
- one root-to-leaf path per tree being walked, plus the pages an index lookup faults through the
  pager;
- the derived entries of one row at a time.

Index agreement probes the index once per derived entry instead of sorting either side, so its work
is `O(entries × log entries)` page lookups and no spill is needed. This is the non-foreclosure rule of
CLAUDE.md §9: nothing in the check requires a table to fit in memory.

## 6. Testing

The check is host API, so it is not in the SQL corpus. Its contract is tested with:

- **Damaged fixtures.** `spec/fileformat/verify.rb` gains `--integrity-check FILE…`, a fourth
  voice that prints the report (§2 minus `detail`) as JSON, and a generator of damaged images under
  `spec/fileformat/integrity/`. Each fixture starts from a golden and applies one deterministic damage
  with its checksums *recomputed* where the damage is semantic, so the logical checks are what catch
  it: a bit flip in a leaf (`page_corrupt`), two rows' keys swapped (`row_keys_unordered`), a row
  re-keyed (`row_key_not_primary_key`), a dropped index entry (`index_entry_missing`), an extra one
  (`index_entry_count_mismatch`), a wrong row count, a wrong live count, a live page on the free list,
  a shared overflow page, a constraint-violating row. Each fixture carries its expected report; every
  core must reproduce it exactly, and so must `verify.rb`.
- **Clean goldens.** Every existing golden file and encrypted golden checks with no findings at both
  levels with `constraints`.
- **Simulation.** The runners' image checker becomes a call to the engine's check on the opened copy
  (asserting no findings, plus the expected-version check that stays in the runner); the `corrupt`
  step's oracle gains "the check reports a finding naming the flipped page, or the flip was harmless".
  The live checker stays as it is.
- **Per-core unit tests** for the host surface only: `42704`, cancellation, `max_findings`
  truncation, and running beside an open writer.

## 7. CLI

`jed check` is a reserved first-token verb, like `migrate` and `pages` ([cli.md](cli.md) §3.1):

```
jed check [--quick] [--constraints] [--max-findings N] [--format text|json] [credential flags] DBFILE
```

It opens `DBFILE` read-only (the existing credential flags apply to an encrypted file), checks `main`,
prints one line per finding and skip (`json`: one JSON object, the report of §2), and exits `0` with
no findings, `1` with findings, `2` on a usage or open error. An open that fails with `XX001` prints a
single `open_failed` pseudo-finding carrying the error and exits `1`, since that is the answer to "is
this file sound?". Attachments are checked by opening them directly.

## 8. Follow-ons

- exclusion-constraint re-checking (GiST probe per row);
- a metered SQL form, if a use appears;
- checking all attached databases in one call (today the host loops);
- an offline mode that checks every recoverable generation, not only the one recovery selects
  (`verify.rb --check-image` covers that for diagnosis).

## 9. Slices

| slice | delivers |
|---|---|
| **I1** | `spec/integrity/findings.toml`; `verify.rb --integrity-check`; the damaged-fixture generator and expected reports |
| **I2** | `quick` level in Rust, Go, and TS (§3.1–§3.3, §5): the sim checkers' logic moved into the engine, extended with separator, depth, overflow-decoding, and catalog checks; the host API in all cores and the TS async API; the simulation image checker rebuilt on it; fixtures for `quick` pass in every core |
| **I3** | `full` level: index agreement for every index kind (column, expression, partial, GIN, GiST) |
| **I4** | `constraints` option |
| **I5** | `jed check` in the CLI and cli.md; `/web` docs (each `api/` language page) |

I2 is the largest, and I3–I5 each depend only on it, so they can run in parallel once it lands.
