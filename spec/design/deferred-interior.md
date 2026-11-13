# Deferred interior pages (ratified 2026-11-13)

**Status: ratified, landing as `format_version` 53 together with
[manifest-crc32.md](manifest-crc32.md).** The two were sequenced against each other and the answer
is that they are one change: both rewrite the meta's manifest area, and the narrower manifest entry
that change brings buys back almost exactly what this one's remap region spends (§9.2). Landing them
apart would spend two format bumps and write this section's capacity arithmetic twice.

**Prototyped in the Go core only.** The prototype runs behind
`JED_DEFER_INTERIOR=on`; nothing is in `format.md`, the Ruby reference, or the Rust and TS cores, and
the prototype does not bump the format version. **§9 is the byte specification** the other cores
implement — ratified 2026-11-13, answering §7's questions 1 and 5, and *not* the prototype's layout:
it keys an entry by the parent page and child slot rather than the child's page id, which removes the
held-id rule and §8.3's bug class by construction. Where §3 and §9 differ, §9 is the spec and §3 is
the record of what was measured.

**Its baseline moved.** The prototype was measured on top of the kept-chain free list
([free-list-kept-chain.md](free-list-kept-chain.md)), which has since been closed and removed;
[incremental-reclaim.md](incremental-reclaim.md) took its place (2026-11-13) and banked part of the
same saving by a different route. §8 is the re-baselining against that rule: what composing the two
required, the numbers against today's commit (deferral still halves a single-row commit), and the bug
it exposed and fixed — a remap entry could outlive the page it names, which the old rule's periodic
rebuild had been quietly healing. It then moved **again**, under leaf evening (`cb56b480e`), which
adds a third site that forbids retention: §8.5 re-measures §8.2's three workloads at the tip and
finds them **unchanged** — deferral still halves a single-row commit — and says why these workloads
never reach the new path.

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

**Meta page** (the prototype's; §9.2 is the specified layout). The inline manifest area of a meta
ends with a 4-byte marker, a 4-byte count, and
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
  prototype's bytes is the wrong direction, so this stays open by choice. §9 is what closes it: the
  landing change teaches `verify.rb` a specified format, not a Go-only prototype's bytes.

## 7. Open questions

1. ~~**List capacity against manifest capacity.**~~ **Answered (§9.2, §9.7):** the list takes one
   third of the manifest area at every page size. The two consumers need the meta at disjoint times,
   so neither is squeezed.
2. **Row counts.** Carrying per-table count changes beside the list would stop inserts and deletes
   writing the catalog page (about 8 KB of the 41 KB insert).
3. **Leaf-sized writes remain.** An append still rewrites a whole leaf per tree. Closing that needs
   record-level logging, which this design does not attempt.
4. **Page clients.** A client needs the owner's list to read interiors; `begin_read` already carries
   the meta. §9.9 states what the owner must validate in a client's delta; the wire fixtures are
   still owed.
5. ~~Whether the list should be scoped per parent page, which would remove the held-id rule.~~
   **Answered: yes (§9.1, §9.5, §9.7).** An entry is keyed by `(parent page, child slot)`. The
   held-id rule, the reverse index, the injectivity invariant and §8.3's bug all served the global
   child-page key and are gone with it.

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

### 8.5 The baseline moved a second time: leaf evening

Every number above was taken at `68703736a`, which predates **leaf evening** (`cb56b480e`,
[format.md](../fileformat/format.md) *Evening out with a sibling*): an overflowing leaf away from
the right edge is now evened out with an adjacent sibling that has room, the pair re-split in two
and **the new separator replacing the old one** in the parent, before a two-way split is accepted.

That is a third site that changes a parent's separators, and so a third site that forbids retention.
§9.4's condition (b) already covers it in words — "no split, merge **or rebalance** below it changed
the separators" — and the Go prototype is correct against it today, because the evening path clears
`sameKeys` on both of its branches. Nothing in §9 moves.

What could move is **how often retention fires**, and therefore §8.2's column: each rebalance
retires a parent that a deferred commit would have kept. So §8.2 was re-measured at the tip
(`03cc46acc`), same harness, same three workloads, `-count=1` because `deferInterior` is a
package-level `os.Getenv` initializer and Go's test cache does not key on it:

| commit | today | + deferred interiors | ratio | §8.2's figures |
|---|---|---|---|---|
| update a non-indexed column | 48.5 KB | **24.2 KB** | 0.499 | 48.5 → 24.0 |
| update an indexed column | 84.1 KB | **40.1 KB** | 0.477 | 84.4 → 39.9 |
| insert (append) | 96.1 KB | **48.1 KB** | 0.501 | 96.0 → 48.0 |

**The effect is nil — both columns reproduce to within 0.4%, and interior pages per commit are
unchanged at 0.0 / 0.1 / 0.0.** Deferral still halves a single-row commit on the current engine, and
the paragraph above overestimated: it reasoned from the mechanism (a third site forbidding
retention) without asking how often these three workloads reach it. Three rules keep them away from
it. The non-indexed update replaces a value in place and overflows no leaf at all. The append is at
the right edge, where `cb56b480e`'s own rule tries only the right sibling, so the last leaf splits
exactly as before. And evening requires a sibling with `C/8` bytes free, which a tree freshly built
by a 300k-row bulk load and then compacted — the §8.2 setup — rarely has.

So the mechanism is real but the workloads do not exercise it. **A workload that would** is one
whose leaves are loosely packed and mutated away from the right edge: the DML-maintained index and
random-key table `cb56b480e` reports a quarter smaller. That is not what §8.2 measures, and
measuring it is a question about leaf evening's interaction, not about whether deferral pays.

Latency, worth little as one run each on a machine that was not reserved: 207 → 196, 328 → 293,
505 → 446 µs/commit.

## 9. The format (ratified 2026-11-13)

This section is the **byte specification** the other cores implement. It is not in
[../fileformat/format.md](../fileformat/format.md) yet and must not be until a core implements it:
that doc is the contract the three cores and the Ruby reference already satisfy, and its "readers
accept only the exact current version" rule would be false the moment it claimed a version nobody
writes. The landing change moves this section into format.md as the next `format_version` (**53** as
of this writing), teaches [../fileformat/verify.rb](../fileformat/verify.rb), regenerates the
goldens, and brings all three cores up together.

**It is not the prototype's layout.** §7's questions 1 and 5 are answered here, and answer 5 changes
how an entry is keyed. The Go prototype keys the list by the *child's* on-disk page id, globally;
this spec keys it by **the parent page and the child's slot**. That is the whole of §8.3's bug class
and the held-id rule, removed by construction (§9.5). The prototype stays as it is — it is the
measurement artifact §8.2 reports — and is brought to this keying in the landing change, or
retired then.

### 9.1 What an entry says

> **`(parent, slot) → (page[, tag])`** — in the interior page whose index is `parent`, the child
> pointer in slot `slot` names a page the node has since moved off. The subtree root is at `page`.
> The pointer's own on-disk value is ignored.

An interior node with `item_count = N` has `N + 1` child pointers (*Interior node*, format.md), so
`slot` is in `[0, N]`. Resolution is **per page, one lookup, never transitive**: a reader reading the
interior page at index `p` resolves its slot `i` through the entry `(p, i)` when one exists, and
reads the resulting page's own pointers the same way. A page written fresh names its children
directly and so keys no entries; a page that keys entries may itself be the target of another
entry, which is an independent lookup, not a chain.

**Scope.** Interior pages of a table B+tree and of a B-tree or GIN index (`page_type 3`) only.
Not catalog pages: a tree root that moves rewrites the catalog chain, as today, and a chain whose
bytes are unchanged is not rewritten (which is why a non-indexed single-row update writes no catalog
page, §8.2). Not GiST pages: an R-tree is rewritten whole by every commit that touches it
([incremental-reclaim.md](incremental-reclaim.md) §3). Not leaves — a leaf has no child pointers.

### 9.2 Bytes

Meta offset **60**, reserved zero through v52, becomes `remap_entries` (u32): the number of entries
the slot carries.

> **A gap this field sat on, found while placing it and fixed under v52.** format.md names *reserved
> fields* among what slot selection validates, and [../fileformat/verify.rb](../fileformat/verify.rb)
> and the TS core both rejected a slot whose bytes 60–63 are nonzero — but the **Rust** core never
> read them, so it alone accepted such a corrupt slot as a valid candidate, and a damaged file could
> recover a different generation in Rust than in the reference. Rust now makes the same check (and
> the Go core, which had briefly dropped it, makes it again), with a per-core loader test in each.
> Under v53 that check becomes `remap_entries ≤ cap` (§9.3).

The entries occupy the **tail of the meta's manifest area**, which is
`[64, page_size)` in a plaintext meta and `[96, page_size − 40)` in an encrypted one
([../fileformat/encrypted.md](../fileformat/encrypted.md) §6) — call that area `[lo, hi)` and its
size `A = hi − lo`. An entry is:

| offset | size | field |
|---|---|---|
| 0 | 4 | `parent` (u32) — the interior page the pointer lives in |
| 4 | 2 | `slot` (u16) — which child pointer |
| 6 | 4 | `page` (u32) — where that subtree root is now |
| 10 | 16 | `tag` — **encrypted files only**: the AEAD tag of `page`, as every bound pointer carries ([../fileformat/encrypted.md](../fileformat/encrypted.md) §2) |

So `entry_size` is **10** bytes plaintext, **26** encrypted. A `u16` slot is enough at every legal
page size: an interior payload is `8N + 4 + Σ sep_len ≤ C` (format.md *Interior node*), so
`N ≤ 8189` at the 65,536-byte maximum. The region is the last
`entry_size × cap` bytes of the area, where

    cap = floor(floor(A / 3) / entry_size)

— **one third of the manifest area, every page size** (answer to §7.1; §9.7 says why a third, and
why one rule replaces the prototype's two). Entries are written ascending from `hi − entry_size ×
cap`, sorted by `(parent, slot)`, and the rest of the region is zero. The inline manifest keeps the
front of the area, its capacity now

    I = floor((A − entry_size × cap) / manifest_entry_size)

where `manifest_entry_size` is **8** under v53 ([manifest-crc32.md](manifest-crc32.md) §2), against
12 through v52.

| page size | variant | `cap` | `I` (v53) | v52 | this change alone |
|---|---|---|---|---|---|
| 256 | plaintext | 6 | 16 | 16 | 11 |
| 4096 | plaintext | 134 | 336 | 336 | 224 |
| 8192 | plaintext | 270 | 678 | 677 | 452 |
| 1024 | encrypted | 11 | 75 | 74 | 50 |
| 8192 | encrypted | 103 | 672 | 671 | 448 |

The last column is what this section said before the two changes were sequenced together, and is
why they are: the region costs a third of the area, the narrower entry pays for almost exactly that
third, and the inline capacity comes out **unchanged or one better than v52 at every page size**.
Since the inline capacity is also the `synchronous = off` batch cap
([validated-cow.md](validated-cow.md) §4), deferral lands here at no cost to batch length, where on
its own it would have cut that cap by a third.

The region is inside the encrypted meta's seal, like the rest of the page, and before its reserve.
It is covered by the meta CRC-32 in both variants. There is **no marker byte**: the version fixes
the layout, and a slot with no entries writes `remap_entries = 0` and a zero region — so a
from-scratch image (*From-scratch image*, format.md) changes only by its version byte and meta CRC,
exactly as v51 and v52 did.

> **Corrected 2026-11-13, while implementing.** This section originally extended the **bootstrap**
> rule so that a zero-dirty-entry meta requires `remap_entries = 0`. That contradicts §9.3, which
> requires a page-identical backup to copy the pinned generation's list into the copy's bootstrap
> meta — and a backup cannot fold instead, since it copies interior pages verbatim. The extension is
> dropped: the bootstrap rule keeps its v52 meaning for the manifest (descriptor fields and the inline
> manifest area zero), and the remap region of a bootstrap meta is validated by §9.3 like any other.
> Nothing is lost: a from-scratch image has no list in any case.

**The page-header stamp.** v53 also spends the page header's two reserved bytes at offset 2 on
`txid16`, a second and independent detector for a lost page write
([manifest-crc32.md](manifest-crc32.md) §2.1). It touches nothing in this section: the stamp is
checked only when a manifest entry is validated, and a **retained interior is never written and
never listed**, so it keeps the older stamp of the commit that really wrote it and is never checked
against a window it would fail. That scoping is a precondition of retention, not a consequence of
it — a stamp checked on every page read would make §9.4 impossible.

### 9.3 Validation on open

With the other header bounds, before the area is touched: `remap_entries ≤ cap`. Then, over the
region: entries strictly ascending by `(parent, slot)`; `parent` and `page` both in
`[first_body_page, page_count)`; `page ≠ parent`; `page` values pairwise distinct (every live page
has exactly one parent pointer, so two entries can never name one page). Any failure makes the candidate
structurally invalid and the other slot may be tried — the region is the meta slot's own bytes under
its own CRC, like the inline manifest, not a separate chain validated after selection like the free
list.

A reader does **not** verify that a parent is reachable or is an interior page; an entry whose
parent is never read is inert, and §9.5 is what makes sure there are none. When a pointer *is*
resolved, the parent page must be `page_type 3` with `slot ≤ item_count`, else `XX001`. In an
encrypted file the resolved pointer's tag is the entry's, and the target page must authenticate
under it exactly as a pointer-bound page does.

The list is applied everywhere an interior page's pointers are read from disk: the open-time
skeleton read, a demand-paged interior fault, the integrity check, the free-list backstop rebuild's
walk, and backup's walk. A page-identical backup copies the pinned generation's list into the
copy's bootstrap meta.

### 9.4 What a commit publishes

Let `E₀` be the entry set of the snapshot being committed against. The commit computes, **before
allocation**, in this order:

1. **The migration page `M`** (§9.6): the top page of the file when it is not free and some lower
   page is free, else 0.
2. **Retention.** An interior node keeps its page iff (a) it replaces a persisted page one for one,
   (b) its separator set is still that page's — no split, merge or rebalance below it changed the
   separators, so the slot count and every slot index are fixed for that page's whole lifetime —
   and (c) its page is not `M`.
3. **The entry set `E`**: `E₀`, minus every entry whose `parent` page this commit **retires** — wrote
   fresh elsewhere, merged away, or dropped with its table, i.e. every former parent that is no
   longer a live interior of the new snapshot (§9.5) — plus `(P, i) → C` for every retained interior
   `P` whose slot-`i` child this commit rewrote to `C`. An add overwrites an existing `(P, i)`, so a
   child that moves repeatedly under one parent occupies one entry forever.
4. **The fold test**: if `|E| > cap`, the commit **folds**.

A **fold** writes fresh every **stale** interior — one that keys an entry in `E₀`, which is exactly
an interior whose on-disk pointers disagree with the live tree — and every ancestor of one, along
with everything the commit would write anyway; it retains nothing and publishes `remap_entries = 0`.
A fold therefore costs paths to stale pages anywhere in the tree, not just under this commit's
changes. That is the work today's commit does on every commit, paid once per fold instead.

The decision is a pure function of `E₀`, the dirty-node structure, the committed free list and the
high-water, all known before allocation, so it is identical in every core. After it, allocation,
write order, the manifest, the meta and the barrier are unchanged
([validated-cow.md](validated-cow.md)).

**Relaxed batches.** Under `synchronous = off` a batch is open while its pending set is non-empty, so
a relaxed commit must write at least one page: a commit with no dirty tree node rewrites the catalog
chain instead of keeping it (§6; without the rule the simulator's durability oracle fails).

**Live pages.** The count rule (format.md *Live page count*) is unchanged, but the delta counts a
retained interior and a kept catalog chain as replaced, so the commit adds them back.

### 9.5 Reclamation: no held pages

The orphan set a commit lists ([incremental-reclaim.md](incremental-reclaim.md)) changes in exactly
two ways:

- **A retained interior's page is live**, at the page the *previous* snapshot's node holds, so it is
  not an orphan and the walk must not list it (the prototype's `retainedIDs`). It also must not be
  used as the walk's shared boundary: that boundary stops the walk *descending*, and the leaves and
  overflow chains orphaned under a retained interior are reachable only through it (§8.3's recorded
  wrong shortcut).
- **Every entry whose parent the commit retires is dropped** (§9.4 step 3). The three ways a parent
  leaves the tree — written fresh, merged away, dropped with its table — all orphan its page, so the
  retired parents are exactly `E₀`'s parents that the orphan set contains, and the rule is total.
  Note that the writer computes it from **its own mutation**, not from the orphan walk: every writer
  knows which interiors it wrote fresh and which the tree lost, a page client included, so the drop
  does not depend on enumerating orphans.

Together these give the property the prototype's global keying could not: **no page is held off the
free list by the list.** A parent's entries die in the same commit that orphans its page, and a
commit never reuses a page it orphans (the orphan joins the list this commit persists, not the safe
set it allocates from — format.md *Reclamation*), so a reused page can never be read through a
surviving entry. There is no `held` set, no leak measure correction, and no allowance in the
simulator's bounded-growth oracle (§5 needed one; this spec does not).

The §8.3 bug class goes with it. That bug was the cost of keeping "the key is always what the
on-disk pointer says" true across repeated moves — a reverse index, an injectivity invariant, and
collision retirement. A `(parent, slot)` key is a position in a page whose bytes are fixed for the
entry's lifetime (step 2b), so it never changes and the entry is a plain overwrite.

**A page client** lists no orphans, but it still **drops** — by the rule above, which is local to
its own mutation. It must: a retired parent's page is dead and unlisted only until the owner's
backstop rebuild finds it, after which the page is reusable, and an entry that outlived it would
then be applied to whatever interior landed there. What the client's commit does leave is the
ordinary leak the rebuild already exists for: the retired page itself, on no list.

### 9.6 Trailing truncation

Trailing truncation cuts only the **contiguous** run of free pages below the high-water (format.md
*Trailing truncation*), and deferral's whole point is that an interior does not move — so one
retained interior at the top of the file pins its entire length, where without deferral the commit
that rewrites it lands it on a low free page and the top drains on its own.

So a commit names `M`, the top page of the file when it is not free (which is to say, when it is
what blocks the run) and some lower page is free to land on, and refuses to retain the node whose
page that is (§9.4 step 2c); the catalog chain is likewise rewritten when its root is `M`. The node
is written fresh onto the lowest free page, its parent takes an entry, and the next commit's run
reaches past it.

Its four properties, measured in §8.4: it costs one page write per commit and **only while the file
is blocked** (`M = 0` once the top is free); it is **monotone**, since the allocator draws the
lowest free page and `M` is the top one; it needs **no state**, being computed from the committed
free list and high-water the commit already holds; and it only ever moves a node the commit was
going to serialize anyway. A blocker no statement touches is retried, free, on every commit until
one does.

The honest cost: a deferred file packs about 2% looser, because a retained interior or catalog page
that is not the blocker does not migrate.

### 9.7 Why these two answers

**A third of the area, for the list (§7.1).** The two consumers need the meta at disjoint times. A
deferring commit writes one to three pages, so its manifest has a handful of entries and fits inline
at any sizing; the commits that fill an inline manifest are bulk transactions, which are exactly the
commits that fold, so they publish no list at all. And an overflow manifest page costs one page
write against the hundreds such a commit already writes. The list's own appetite is bounded the same
way from the other side: folds amortize, so at the prototype's 8 KiB capacity they ran about one
commit in a hundred, and doubling the region halves an already-negligible cost. A third of the area
sits where neither consumer is squeezed, and — the reason to prefer it to the prototype's 2,728
bytes — it is **one rule at every page size**, reproducing both the prototype's 8 KiB share and the
"a third" rule it needed below 4 KiB for the simulator's small pages.

**Per-parent keying (§7.5).** §9.5. The held-id rule, the reverse index, the injectivity invariant
and §8.3's bug all exist to serve a global child-page key; a position key needs none of them. It
costs 2 bytes an entry (10 against 8), which is ~20% of the capacity at a fixed region size, against
removing the one correctness rule in the design that reaches outside it into the free list.

### 9.8 The Go implementation (2026-11-13)

`impl/go/remap.go` now implements this section, replacing the prototype of §3 behind the same
`JED_DEFER_INTERIOR=on` switch, still without a version bump. Three choices the spec leaves to the
implementation, recorded so the other cores make the same ones:

- **The decision precedes allocation** (§9.4). `planDeferral` walks the dirty structure without
  allocating, deciding retention exactly as serialization will and collecting the shared boundary
  serialization would record; it then forms `E` and applies the fold test. The prototype's
  conservative pre-check (`|E0| + dirty nodes ≤ cap`) folded where §9.4 does not and could not be
  reproduced byte for byte by another core, so it is gone. A dirty child without an origin under a
  retained parent simply takes an entry — the prototype forbade that, and (parent, slot) keying makes
  the restriction unnecessary.
- **The drop rule is the live-page delta's walk** (§9.5). Every writer, a page client included,
  already walks the previous snapshot's interiors that the new one does not share in order to publish
  its live count. An `E0` parent that walk reaches and the commit does not retain is retired. It is
  total — merged-away nodes and dropped tables and indexes are reached the same way — and needs no
  orphan enumeration. With no previous snapshot to walk, the commit folds rather than guess.
- **Staleness is the list's, not a flag's.** A node is stale iff its page keys an entry; the fold and
  the loader both ask the list.

The backstop free-list rebuild no longer prunes the list (it used to heal the global keying's leaks);
it now **asserts** that every surviving entry's parent and target are reachable, so a drop-rule bug
fails loudly. The simulator's bounded-growth oracle lost its deferral allowance.

Run: unit suite off and on (alone, and with both other switches — the only failures are the v52
fixture byte comparisons the stamp causes); `TestRemap*` for the §9.2 capacity table, the byte round
trip, every §9.3 rejection, the read-time slot check, and an end-to-end file that defers, folds,
merges, drops an index, passes the full integrity check after each phase and reopens to the same
rows; `sim:sweep 1..200 all go` with deferral alone and with all three switches (4,400 traces each,
every oracle, no allowance); `sim:sweep 1..50` across all three cores with every switch off (1,100
traces). The committed regression traces pass in the Go runner with deferral on, but the independent
Ruby image checker disagrees with 61 recoveries: it still reads offset 60 as reserved zero and sizes
the inline manifest the v52 way — the `verify.rb` work below.

**A latent oracle bug, found by deferral and fixed in all three runners.** `large-values-28`, under
`synchronous = off`, failed the trailing-truncation oracle in the old prototype and the new code
alike: two relaxed batches lowered the high-water and were each lost to a power failure before their
flush, recovery adopted the lowered generations over a longer file, and the next lifetime's flush
was blamed for not cutting it. The engine follows format.md (*Trailing truncation*: the file is cut
only by a commit that lowers the high-water against the generation it commits on); the oracle kept
its "shrink owed" flag across the crash. `recover` now clears it in the Go, Rust and TS runners.
Whether recovery itself should cut such a file is a separate question this change does not raise.

**The Ruby reader (2026-11-13).** `spec/fileformat/verify.rb` reads all three parts of v53 ahead of
the version: the remap region (§9.2) with every §9.3 check, resolution at each interior read — the
decoder's table and index walks and the integrity reference (`integrity.rb`) — the narrow manifest,
and the stamp with the same window rule. It takes the layout from the Go core's three switches, so the
simulator's independent checker reads what the core wrote; at v53 the version byte decides instead.
Two details: the selected meta installs its list in `select_meta`, so no list outlives the image it
came from; and an interior's children come back as `(page, tag)` pairs whose tag is bound just before
the child is read, because a retained page's stale pointer may name a page number another pointer now
uses with a different tag. With every switch off `mise run verify` is unchanged (116 file-format
fixtures, 24 integrity fixtures). With each switch alone and all three together the regression traces
agree with the checker (the 61 disagreements are gone), and `sim.rb sweep --seeds 1..25 --profile all
--cores go --check-images` with all three switches re-decided 21,641 crash images, every one agreeing.
Coverage of the list itself is thinner than that count: in a 400-image sample, 15 recoveries (about
4%) selected a meta carrying entries. The rest folded or never deferred.

**The Ruby writer (2026-11-13).** `verify.rb` now writes the `cow_deferred_*` group, plaintext, as
hand-built commit histories over a height-2 tree (root over two interiors), each byte-exact, selected
and re-walked by the reference and integrity-checked clean:

- `cow_deferred_retained` — one retained parent keying one entry, the catalog kept;
- `cow_deferred_overwrite` — the same child moved twice: one entry, overwritten, while the fallback
  slot still carries the entry it replaced;
- `cow_deferred_fold` — a fold: the stale interior and the root written fresh, the catalog rewritten,
  the list empty;
- `cow_deferred_retire` — a parent rewritten fresh drops its entry while another parent's survives,
  and the retained root keys the new page.

A fixture now declares its layout (`deferred:`, `stamped:`), so these and the v52 fixtures verify
side by side whatever the environment says — `mise run verify` passes with the switches off and on.
Sabotaging `retained`'s entry (naming the old leaf, or dropping the list) reverts the row the
fixture's content check pins; keying it to the wrong slot is caught by the integrity reference as a
`separator_violation`. The Go core, reading the layout, recovers the same generation and rows from all
four and checks them clean (`TestRemapRubyHistories`). Without the layout a slot carrying entries is
invalid: `retained` opens at its base generation, `fold`'s newest slot — an empty list, a zero region —
is a valid v52 slot read correctly at txid 3, and `overwrite` and `retire`, with entries in both slots,
open nowhere; Rust and TS list those two as unopenable until they read the layout.

Case (e) is `encrypted_cow_deferred_retained` (page size 1024): history (a) in an encrypted file, one
commit, so slot 1 keeps the base generation and every core opens it with the goldens' key. The
retained root's on-disk pointer still binds the old leaf's tag and the entry carries the new leaf's, so
reading the edited row at all proves the reader took the tag from the entry. Sabotaged and resealed,
an entry with the wrong tag or naming the old leaf is rejected when the page is read; a region edited
without resealing passes keyless slot selection and fails meta authentication — `XX001`, never stale
rows, for this new kind of bound pointer. The Go core reads it at txid 2 with the layout and at the
base generation without (`TestRemapRubyEncryptedHistory`). **Still owed:** the writer's own deferral
planner — these histories are assembled by hand, page by page, rather than planned the way §9.4 plans
a commit.

### 9.9 What the landing change owes

v53 is **one change carrying both this section and [manifest-crc32.md](manifest-crc32.md)**, so this
list is the whole of it. That doc's own share is the 8-byte manifest entry, the `txid16` page-header
stamp and its batch window, and the meta's offset 40 (the prototype's narrow-layout bit drops) and
offset 48 (`manifest_crc64` → `manifest_crc32`, high 4 bytes reserved zero).

**Every golden regenerates regardless of this section**, because the stamp changes the bytes of every
page — so the from-scratch image no longer differs from v52 only by its version byte and meta CRC, as
§9.2 says it would have on its own.

Beyond the three cores and `verify.rb`:

- **Fixtures** (written, §9.8). A from-scratch image has no entries, so no static
  golden can carry the region's bytes; the list needs a commit history, like the `cow_*.jed` family. A
  `cow_deferred.jed` group pinning (a) a single retained parent with one entry, (b) a repeated move overwriting one entry,
  (c) a fold emptying the list, (d) a parent dropped by its own rewrite, and (e) the encrypted entry
  with its tag, each selected and re-walked by the Ruby reference.
- **The page service.** `begin_read` already carries the meta, so a client reads the list it needs;
  the owner must **validate a client delta's list** — count against `cap`, ordering and bounds as
  §9.3, plus: no entry's `parent` may be in the delta's own dirty-page set, since a page written
  fresh names its children directly. The wire fixtures (`spec/pageservice/`) grow with it. A client
  forgets the kept catalog chain on every reload, since another process may have rewritten it.
- **The simulator.** The `replay` step must put an old page back at a remap entry — a new kind of
  bound pointer, and in an encrypted file a case the security review owes an opinion on
  ([encryption.md](encryption.md)). The bounded-growth oracle needs no allowance (§9.5).
- **A race review** of the commit path, which sets a page id on the committing snapshot's own nodes
  and swaps the resident list during planning (§6).
- **`unassignPages`**, or its equivalent, must undo a retained interior when a plan is discarded.
- **The stamp's remaining surface** ([manifest-crc32.md](manifest-crc32.md) §4): give the integrity
  check and the page service's delta check a window, and change the manifest-overflow header check
  from "bytes 1–3 zero" to "flags zero, stamp equals the meta's txid" in Rust, TS and the Ruby
  reference, as the Go core already does.
- ~~A re-measurement of §8.2's three workloads at the tip.~~ **Taken** (§8.5): unchanged at
  `03cc46acc`, so the saving stands as stated. What is still unmeasured is a loosely-packed tree
  mutated away from the right edge, where leaf evening's rebalance is common — a question about that
  change, not a precondition for this one.

What it does **not** owe: row counts in the meta (§7.2), which would stop an insert writing the
catalog page and is a separate change; and record-level logging (§7.3), which this design does not
attempt.
