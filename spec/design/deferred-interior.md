# Deferred interior pages (proposal)

**Status: proposal, prototyped in the Go core only**, behind `JED_DEFER_INTERIOR=on`. Nothing here is
in `format.md`, the Ruby reference, or the Rust and TS cores, and the prototype does not bump the
format version.

**Its baseline moved.** The prototype was measured on top of the kept-chain free list
([free-list-kept-chain.md](free-list-kept-chain.md)), which has since been closed and removed;
[incremental-reclaim.md](incremental-reclaim.md) took its place (2026-11-13) and banked part of the
same saving by a different route. §8 is the re-baselining against that rule: what composing the two
required, the numbers against today's commit (deferral still halves a single-row commit), and the bug
it exposed and fixed — a remap entry could outlive the page it names, which the old rule's periodic
rebuild had been quietly healing.

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
commit may have written (a relaxation the closed kept-chain prototype needed for the free-list head
too, and which does not depend on it).

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

- ~~**One backup unit test** fails with the switch on because it builds its expected bootstrap meta
  without a list.~~ Closed 2026-11-13: the test now builds the expected meta the way `backup.go`
  already did, through `encodeRemap` (§8.4).
- ~~**Trailing truncation** is weaker: held page ids and stale interiors can sit at the top of the
  file (one truncation test fails with the switch on).~~ Closed 2026-11-13 by migrating the blocking
  page, one per commit (§8.4). Held ids turned out not to be the problem.
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
  Concretely, `mise run sim:regress` fails 40 checks with the switch on, every one of them
  `verify.rb --check-image` reading a deferred meta (33 "no valid meta page", 7 picking a different
  generation). That is the independent checker, not the engine: the same run with migration (§8.4)
  disabled fails the identical 40, and `mise run sim:sweep 1..20 all go` — which does not re-decide
  recovery in Ruby — passes all 440 traces. Teaching the shared reference checker a Go-only
  prototype's bytes is the wrong direction, so this stays open by choice.

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

## 8. Re-baselined against incremental reclamation (2026-11-13)

§4's columns were measured over the closed kept-chain prototype. Incremental reclamation
([incremental-reclaim.md](incremental-reclaim.md)) has since landed in all three cores and banked the
free-list part of that saving by a different route, so the question is what deferral is still worth on
top of it. The Go prototype no longer turns incremental reclamation off (`incrOn`); the two run
together.

### 8.1 What composing them required

Incremental reclamation lists the pages a commit orphans in that commit's own free list. Deferral
keeps pages that an ordinary commit would replace, so two sets have to be withheld from that list:

- **The ids the remap list is keyed by.** A pointer legitimately holding a reused id would be
  redirected, so a keyed id stays off the list while its entry exists (`incrState.remapHeld`), and the
  commit that drops the entry — a fold, or a rebuild dropping an entry whose node is gone — lists it
  like any other orphan. Under the old rule only the periodic rebuild had to honor this; now every
  commit does. The leak measure (§3.5 there) subtracts them, and the simulator's bounded-growth oracle
  allows them, which is the exact form of the allowance §5 above already needed.
- **The pages retained interiors kept.** `write.retainedIDs`, new: a retained interior is still live at
  the page the *previous* snapshot's node holds, and the orphan walk would otherwise list a live page.
  Before this, the live-page delta corrected for them with a count (`write.retained`) and nothing
  needed their ids.

With both, the Go unit tests and the conformance corpus pass with the switch on, except the two
trailing-truncation tests already predicted by §6 (held ids and stale interiors sit at the top of the
file: 1,673 pages at the peak against 1,566 after churn, where the test wants half).

### 8.2 The numbers

Same database and workloads as §4, 2,000 single-row commits per case after a compaction, `fsync` off,
one run each. "Today" is the committed engine (incremental reclamation, no deferral):

| commit | today | + deferred interiors | time | pages written today → deferred (leaf / interior / catalog / free-list) |
|---|---|---|---|---|
| update a non-indexed column | 48.5 KB | **24.0 KB** | 256 → 193 µs | 1.0 / 2.0 / 1.0 / 1.0 → 1.0 / 0.0 / 0.0 / 0.8 |
| update an indexed column | 84.4 KB | **39.9 KB** | 415 → 291 µs | 2.9 / 4.6 / 1.0 / 1.0 → 2.9 / 0.1 / 0.0 / 1.0 |
| insert (append) | 96.0 KB | **48.0 KB** | 593 → 429 µs | 3.0 / 6.0 / 1.0 / 1.0 → 3.0 / 0.0 / 1.0 / 1.0 |

So **deferral still halves the bytes of a single-row commit**, and the gap it attacks was not eroded
by incremental reclamation: the two remove disjoint pages — incremental reclamation shortened the
free-list chain each commit writes, deferral removes the interior and catalog pages. Today's figures
are close to §4's "kept chain" column (47 / 82 / 96 KB), as expected, since that is the saving
incremental reclamation banked another way. What is left after deferral is one leaf per tree touched,
the free-list chain page, and the meta; the insert still writes the catalog page for the row count
(§7 question 2).

Both runs were taken with the machine otherwise quiet, one run each; an earlier contended baseline
produced the same byte and page counters (they are deterministic) but times up to 5× higher, so the
latency column is worth no more than one quiet run of a `fsync`-off workload — about a quarter off,
in line with writing half the pages.

### 8.3 The bug found: a remap entry outliving the page it names

Composing the two first produced corrupt files, intermittently: `free page N is reachable`, an index
one entry longer than its rows derive, `page_reached_twice`, `separator_violation`. The cause was in
**this prototype's bookkeeping**, not in incremental reclamation, and it is fixed.

**Narrowing it.** Three probes, deferral on in all three (`TestDeferInteriorStress`, plaintext and
encrypted):

| probe | per-commit orphan listing | an extra reachability walk every commit | result |
|---|---|---|---|
| A | off | on | **passes** |
| B | on | on | corrupt |
| C | on | off | corrupt |

A rules out the reads — the prototype tolerates a full reachability walk per commit, which is what
`JED_INCR_VERIFY=on` adds — and C rules out the walk. (The first diagnosis written here, that the
walk's resident/`OnDisk` classification made the orphan ids residency-dependent, was wrong.) With an
integrity check after *every* commit the first failure is stress commit 235, `free page 42 is
reachable` plus `t_k` one entry long, while `incrCheck`'s own oracles — no listed page reachable from
the new root, no dead page left unlisted — pass at that commit and every other. A page dead when
listed, reachable again afterwards.

**The mechanism**, from a trace of every event touching page 42:

1. Page 42 is free, and is reused as the new home of the child whose on-disk id is 271: entry
   `271 → 42`.
2. That child later leaves the tree (a merge), and its parent is written fresh. The write-fresh branch
   drops the entries of the children that **remain** — a child that left is not among them — so
   `271 → 42` survives, while page 42 is orphaned, listed, and reused.
3. Another child moves onto the reused page 42: `replace(246, 42)` adds `246 → 42`. Two keys now name
   one page, and `clone()` rebuilds `rev` from `fwd`, so `rev[42]` is whichever key map iteration
   yields last. **That is the intermittency.**
4. The node at 42 moves to 105. `replace(42, 105)` reads `rev[42]`, gets the stale `271`, and updates
   the wrong key (`271 → 105`), leaving `246 → 42` while page 42 is freed. The on-disk pointer 246 then
   resolves to a free page, and the stale leaf sitting there is the index's extra entry.

**Why it was invisible before.** Under the `compact_growth` rebuild nothing reused an orphan until the
rebuild ran, and the rebuild *prunes* entries whose target is unreachable (§3, "Free-list rebuild"), so
it healed this bookkeeping before it could matter. Incremental reclamation did not break deferral; it
removed the periodic healing that was hiding a bug in it.

**The fix** (`remapState.replace`): retire whatever collides with the new entry — another key already
naming the page, or this key's own previous target — which keeps `fwd` injective on target pages and
`rev` its exact inverse. A stale entry that nothing else claims is then merely a key page held off the
free list until the next fold or prune: a bounded leak, not corruption. Three unit tests in
`impl/go/remap_test.go` pin the invariant; each fails without the fix. They are per-core tests because
the invariant is internal and has no SQL-visible form (CLAUDE.md §10).

**Where that leaves the composition.** The corruption is gone: the stress test passes with a full
integrity check after *every* commit (both subtests, 6,000 commits each), where it used to fail at 235;
the commit-write experiment's closing integrity check and reopen are clean; and the suite with
deferral on is green except three tests, all of them §6's already-recorded gaps — one backup test that
builds its expected bootstrap meta without a list, and the two trailing-truncation tests (held ids and
stale interiors sit at the top of the file: 1,673 pages at the peak against 1,566 after churn, where
the test wants half). The results are identical with and without `JED_INCR_VERIFY_STRICT`.

**Writer-emitted orphan ids are therefore no longer a correctness question.** They remain worth doing
as an efficiency and simplicity change — the orphan walk is the one part of a commit that reads pages
it does not otherwise need — but the walk enumerates four things and `serializeDirty` knows only the
first, so it is not a small change:

1. the persisted page each dirty node replaces (`origin`, already recorded);
2. the nodes a merge or rebalance removes, which only the `pmap` paths see;
3. whole trees a DDL statement drops;
4. the overflow chains a rewritten or deleted record releases (the record-disposition planner already
   computes the complement, `sharedChains`).

**A plausible-looking shortcut that is wrong, recorded so it is not tried twice.** Rather than
filtering the retained pages out of the list (§8.1), record each in `kept`, the shared boundary the
orphan accounting already has. One line, and it reads as obviously right: the new snapshot does keep
that page. It is wrong because `kept` also stops the walk **descending**, and the leaves and overflow
chains orphaned under a retained interior are reachable only through it. `SetVerifyLivePages` catches
it on the second commit (a live count of 25 against the walk's 21), and under relaxed churn the file
grows instead of settling (152 pages at update 200, 280 at 400). Retained pages belong in the *list's*
exclusion, not in the shared boundary; `incrementalWrite.retainedIDs` now says so.

With deferral off, nothing in §8 changes a byte: `retainedIDs` is empty, the remap list is nil, and a
commit is the one [incremental-reclaim.md](incremental-reclaim.md) specifies. The switch is still a
research switch, with §6's gaps open and §7's questions unanswered.

### 8.4 Closing the two test gaps (2026-11-13)

§6's first two bullets were the only Go tests the switch failed. Both are closed.

**The backup test** was simply behind the code. `backup.go` has carried the source's list into the
copy's bootstrap meta since the prototype landed; the test built its expected meta without one. It
now calls `encodeRemap` + `resealRemapMeta` the same way, under the same `mt.remap != nil` guard.

**Trailing truncation** was a real design consequence, and not the one §6 guessed. The §6 note blamed
held page ids; a probe of the file after the failing test's 600 commits showed otherwise — of 169
pages, 147 were free, the 8 held ids sat at pages 2–9, and **one** page, a live interior at 168,
blocked the entire run. Trailing truncation only ever cuts a *contiguous* run of free pages down from
the high-water, so one live page at the top pins the file's whole length. And deferral's whole point
is that an interior does not move: without deferral every commit rewrites the interiors above a
changed leaf onto free pages low in the file, so the top drains on its own.

The fix is to move exactly that one page. `migrateHint` (validated_commit.go) names the top page of
the file when it is not free — which is to say, when it is what blocks truncation — and some lower
page is free to land on; `serializeDirty` then refuses to retain the node whose page that is, so it
is written fresh onto the lowest free page and the next commit's trailing run reaches past it. The
catalog chain gets the same treatment (its `prevCat` reuse is dropped when its root is the blocker).

The properties that make this worth having rather than a hack:

- **It costs one page write per commit, and only while the file is blocked.** Once the top is free,
  `migrateHint` returns 0 and deferral is untouched. The §8.2 workloads are not blocked, so their
  numbers do not move: re-measured after the change, 24.8 / 41.1 / 49.2 KB per commit against the
  24.0 / 39.9 / 48.0 KB recorded above, and 198 / 298 / 408 µs against 193 / 291 / 429 — byte-level
  noise from slightly different page placement, no trend.
- **It is monotone.** Each migration strictly lowers the page, because the allocator draws the lowest
  free page and the blocker is the top page.
- **It needs no state.** The hint is computed from the committed free list and high-water the commit
  already holds.
- **It only moves a node the commit was going to serialize anyway.** A blocker no statement touches is
  retried, free, on every commit until one does. The failing test went from 169 pages after churn to
  26 (a peak of 172), which is better than the test's `pc * 2 <= peak` bar asks for.

One honest cost: a deferred file packs slightly worse, because retained interiors and catalog pages do
not migrate unless they are the blocker. The §8.2 experiment's file ends at 11,762 pages with the
switch on against 11,505 without — about 2%.

**Verification after both fixes.** The whole Go suite passes with `JED_DEFER_INTERIOR=on`, with and
without `JED_INCR_VERIFY_STRICT` — no failures left, where §6 had three. Also with the switch on:
the disk conformance corpus (677 passed, 0 failed), `mise run sim:sweep 1..20 all go` (440 traces,
every oracle and profile, the `truncation` profile included), and `mise run stress` under the strict
verifier (all three cores agree on the answer checksum). The default path — switch off — is
unchanged and green. What remains open is §6's last bullet, now quantified there.
