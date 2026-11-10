# Deferred interior pages (proposal)

**Status: proposal, prototyped in the Go core only**, behind `JED_DEFER_INTERIOR=on`. Nothing here is
in `format.md`, the Ruby reference, or the Rust and TS cores, and the prototype does not bump the
format version. It builds on the kept-chain free list ([free-list-kept-chain.md](free-list-kept-chain.md)).

## 1. The problem

A copy-on-write commit rewrites every page on the path from a changed leaf to the meta: the leaf, each
interior page above it, and the catalog page that holds the tree's root pointer. For a one-row change
only the leaf carries new information. On the 188 MB test database (8 KiB pages), after the kept-chain
change, per single-row commit:

| commit | leaf | interior | catalog | meta | bytes |
|---|---|---|---|---|---|
| update a non-indexed column | 1.0 | 2.0 | 1.0 | 1 | 47 KB |
| update an indexed column | 3.1 | 4.9 | 1.0 | 1 | 82 KB |
| insert (append) | 3.0 | 6.0 | 1.0 | 1 | 96 KB |

SQLite in WAL mode writes 8, 26, and 27 KB at commit for the same statements, and PostgreSQL 21, 19,
and 9 KB; neither writes interior pages or an equivalent of the catalog page.

## 2. The idea

A commit writes only the nodes whose own content changed. An interior node whose only change is
*which page a child lives at* is not written; it keeps its page, and the meta page records the
difference in a **remap list**: "the node an on-disk pointer calls page `D` is now at page `C`".

- A leaf rewritten from page `X` to `Y` under an otherwise unchanged parent adds `X → Y`. Rewritten
  again to `Z`, the entry becomes `X → Z` (the key is always what the on-disk pointer says).
- An interior whose separators changed (a split or merge below it) is written fresh, and its own
  parent, if otherwise unchanged, records the interior's move the same way.
- A tree root that is written fresh moves the root pointer, so the catalog is written. A root that
  keeps its page leaves the catalog untouched, and a catalog chain whose bytes are unchanged is not
  rewritten.
- **Fold.** When the list would overflow, or a commit cannot defer, the commit writes every stale
  interior and its ancestors fresh and publishes an empty list. This is the work a commit does today,
  paid once per fold instead of once per commit.

The tree stays copy-on-write: no page a recoverable meta depends on is overwritten, readers still pin
immutable roots, and a commit is still one meta swap behind one barrier. In memory nothing changes
shape, since resident interiors already hold direct child pointers; only code that reads an interior
page from disk consults the list.

## 3. What the prototype does

**Meta page.** The inline manifest area of a meta ends with a 4-byte marker, a 4-byte count, and
before them the entries, sorted by disk page: `disk page`, `current page` (8 bytes), and in an
encrypted file the current page's tag as well (24 bytes), since every pointer there binds the page it
names. The region is 2,728 bytes from 4 KiB up: 340 entries in a plaintext meta, 113 in an encrypted
one, leaving 450 inline manifest entries instead of 677 at 8 KiB. Below 4 KiB the list takes a third
of the area, so the simulator's small pages carry one (7 entries at 256 bytes plaintext, 12 at 1,024
encrypted). In an encrypted meta the region sits before the reserve and is covered by the meta's
seal like the rest of the page. A meta without the marker has the v50 layout.

**Commit.** Tree mutation records, on each dirty node, the persisted page it replaces one for one
(`origin`) and whether an interior's separators are still that page's (`sameKeys`). At serialization
an interior with both, whose rewritten children all have origins, keeps its page; its moved children
become entries. Otherwise it is written and names its children directly, so their entries are
dropped. A retained interior keeps its origin's page and tag. The commit defers when the list has room for
every dirty node; otherwise it folds. Encrypted files, page clients, and commits inside a
`synchronous = off` batch all defer (§6).

**Recovery and validation.** The manifest no longer has to name the catalog root, which an earlier
commit may have written (the same relaxation the kept chain needed for the free-list head).

**Open** applies the list as it reads child pointers into the skeleton. The integrity check, the
free-list rebuild's overflow walk, and backup's walk apply it too. A page-identical backup copies the
pinned generation's list into its bootstrap meta.

**Free-list rebuild.** An on-disk page id that keys an entry must not be reused while the entry
exists, or a pointer legitimately holding that id would be redirected. The rebuild keeps those ids off
the free list, and first drops entries whose current page is no longer reachable (a merged-away
leaf), before that page can be listed and reused.

**Live pages.** The delta counts a retained interior and a kept catalog page as replaced, so the
commit adds them back.

## 4. Results

Same database and workloads, 2,000 single-row commits per case, `fsync` off, one run each:

| commit | v50 | kept chain | + deferred interiors | pages written (leaf / interior / catalog) |
|---|---|---|---|---|
| update a non-indexed column | 78 KB | 47 KB | 17 KB | 1.0 / 0.0 / 0.0 |
| update an indexed column | 130 KB | 82 KB | 39 KB | 3.1 / 0.6 / 0.1 |
| insert (append) | 145 KB | 96 KB | 41 KB | 3.0 / 0.0 / 1.0 |

Commit time fell with it (340 → 217 µs, 397 → 344 µs, 485 → 313 µs). The bulk load is unchanged: it
is dominated by leaves.

What is left is one whole leaf per tree touched plus the meta page. The insert still writes the
catalog page, because the table's row count lives there.

## 5. What was run

- A randomized test (`TestDeferInteriorStress`): 6,000 commits of single-row and range updates,
  inserts, and deletes on an indexed table, against an in-memory database running the same
  statements; a full integrity check every 250 commits, a close and reopen every 400 (15 reopens, each
  applying a non-empty list), and a reachability recount of live pages on every commit.
- The Go unit tests and the Go conformance corpus, with the switch off and on.
- The commit-write experiment's integrity check before and after a reopen.

- **The crash simulator, Go core, with the switch on:** `mise run sim:sweep 1..200 all go`, 4,400
  traces over every profile (power loss, process crashes, torn sectors, I/O errors, bit flips,
  compaction, backup, pinned readers, truncation), passed every oracle. Counters over seeds 1..60
  show the list was in play: 9,392 commits left a non-empty list, 2,867 commits folded, and 19,263
  opens (recoveries and image checks among them) applied a non-empty list. The same sweep passes with
  the switch off.
- One oracle needed an allowance. *Bounded growth* requires a rebuild to reclaim every dead page;
  page ids that key a remap entry stay off the free list until a fold, so the check now allows up to
  one list's worth of them (`growth-37` failed without it, with 9 pages held).

**Not run:** the simulator's regression traces, whose independent Ruby checker (`verify.rb`) does not
know the list; anything cross-core; the page-service process corpus; the race detector.

## 6. Gaps in the prototype

- **One backup unit test** fails with the switch on because it builds its expected bootstrap meta
  without a list.
- **Trailing truncation** is weaker: held page ids and stale interiors can sit at the top of the file
  (one truncation test fails with the switch on).
- **Relaxed batches defer.** One rule was needed: a batch is open while its pending set is non-empty,
  so a relaxed commit must write at least one page. A commit with no dirty tree node rewrites the
  catalog instead of keeping it. (Without the rule the simulator's durability oracle failed 2 of
  1,320 traces.)
- **Page clients defer.** Every reload takes the list from the meta it selects, and forgets the kept
  catalog chain, since another process may have written the catalog. The owner does not yet check a
  client's list against the client's pages, and the wire fixtures are unchanged.
- **Encrypted files defer.** The same single-row commits on an encrypted copy of the test database
  write 17, 46, and 45 KB, against 41, 91, and 90 KB without deferral: slightly more than plaintext,
  because a third as many entries fit and folds come sooner. The randomized test passes on an
  encrypted file. The simulator's `replay` step does not yet put an old page back at a remap
  entry, which is a new kind of bound pointer; that case is untested and needs the security review.
- With all three on, `mise run sim:sweep 1..200 all go` (4,400 traces) passes; the four encryption
  profiles (60 seeds each) recorded 1,014 commits that left a non-empty list and 5,556 opens that
  applied one.
- **Fold mutates nothing shared**, but deferral sets a page id on the committing snapshot's own nodes
  and the commit path swaps `paging.remap` during planning; neither was reviewed for races.
- **A discarded plan** (`unassignPages`) does not undo a retained interior. The budget precheck
  avoids this by planning without deferral.
- The other cores, `verify.rb`, the page-service wire fixtures, and replication know nothing of it.

## 7. Open questions

1. **List capacity against manifest capacity.** Both live in the meta page. A larger list folds less
   often; a smaller inline manifest overflows sooner.
2. **Row counts.** Carrying per-table count changes beside the list would stop inserts and deletes
   writing the catalog page (about 8 KB of the 41 KB insert).
3. **Leaf-sized writes remain.** An append still rewrites a whole leaf per tree. Closing that needs
   record-level logging, which this design does not attempt.
4. **Page clients.** A client needs the owner's list to read interiors; `begin_read` already carries
   the meta.
5. Whether the list should be scoped per parent page, which would remove the held-id rule.
