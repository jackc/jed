# Incremental free-list reclamation

**Status: adopted in the Rust, Go and TypeScript cores** (2026-11-13). The normative rule is
[format.md](../fileformat/format.md) *Reclamation*; this document is its design record: the problem,
the rule's derivation and safety argument, and the measurements taken on the Go prototype before
adoption (§7). **It needed no format change**: a file written under it is an ordinary v52 file whose
free list holds more entries, and every older binary reads it (it only stops listing orphans). What
changed is the content of each commit's free list, which is part of the cross-core byte contract, so
the three cores adopted it together.

## 1. The problem

Between reclamation rebuilds a commit's orphans are listed nowhere. They wait for the periodic
rebuild, which fires once the pages off the free list exceed `compact_growth × L` (`L` the live count
the last rebuild found; `compact_growth = 2`, `spec/cost/schedule.toml`; format.md *Reclamation*). So:

- **A file sits between 1× and about 2× its live size.** On mixed data a compacted jed file is
  already about half the size of SQLite's or PostgreSQL's for the same rows; the 2× slack is now the
  largest file-size cost.
- **Every rebuild walks the whole reachable database**: the catalog, every interior page, and every
  leaf of every table with a spillable (text, bytea, json, …) column, to find its overflow chains. At
  TB scale that is one commit that reads the database.

Lowering the constant was measured and rejected as the main fix. At 1.25 a 300k-row random-key bulk
load walked on every one of its 600 commits; at 1.1 steady churn walked about 10× as often as at 2.0.
The walk's cost is per rebuild; the constant only trades file size against how often it is paid.

## 2. What already exists

Every file commit already enumerates its orphans, to keep the exact live-page count at meta offset 56
([memory.md](memory.md) §8.7): `liveCount.after` subtracts the previous catalog chain, the previous
GiST forest, and `orphanedTreePages` — each B-tree node of the previous snapshot outside the subtrees
the new snapshot shares (`incrementalWrite.kept`), plus the overflow pages of each orphaned leaf's
external values except the chains a rewritten record kept (`sharedChains`). The walk reads only the
orphaned leaves of tables with spillable columns. It counts overflow chains from their stored length;
listing them needs their page ids, which means reading each chain page's header
([large-values.md](large-values.md) §14 `chainPages`): one page read per orphaned overflow page,
bounded by the bytes the commit's deletes and rewrites released.

So the commit already knows, by count, everything it orphans. The proposal is to record the ids and
list them.

## 3. The design

### 3.1 A commit's orphans

Commit `N`'s **orphan set** is every page reachable from `N−1`'s meta that `N`'s meta does not reach:

1. the B-tree nodes and overflow chains of §2 (the live-delta walk, listing ids);
2. `N−1`'s catalog chain, unless `N` kept it byte-identical (only Go's deferred-interior prototype
   does, and it turns this rule off);
3. `N−1`'s GiST R-tree pages (rewritten whole by every commit, gist.md §4.1);
4. `N−1`'s free-list chain pages;
5. `N−1`'s manifest-overflow pages ([validated-cow.md](validated-cow.md) §2).

Items 1–3 are live pages of `N−1`; 4–5 are its non-live dependencies. The sets are disjoint from each
other and from `N−1`'s free list.

The ids of 2, 3 and 5 are not derivable from `N`'s snapshot, so the writer keeps them from the commit
that wrote them. At open they come from what the load reads anyway: the catalog chain, the eagerly
loaded GiST pages, and the selected meta's manifest chain (its length is in the meta; reading it costs
`manifest_pages` page reads, zero for small commits). Item 4 is the chain the open just read.

### 3.2 The rule

**Commit `N` lists its orphan set in its own persisted free list, and does not reuse any of it.**
From `N+1` on they are ordinary free pages, subject to the reader watermark (§3.3) and, under
`synchronous = off`, to the batch rule (§3.4).

- **Why `N` must not reuse them.** `N−1` is `N`'s fallback. Recovery after a torn `N` reads `N−1`'s
  tree, catalog, GiST, free-list chain, and manifest. All five classes are reachable from `N−1`'s
  meta, so overwriting one before `N`'s meta is durable could corrupt the fallback. This is today's
  rule for a rebuild's fresh orphans (format.md *Reclamation*), applied to every commit. In the
  allocator the orphans join the candidates `N` persists but not the *safe* pages it may draw its
  tree, catalog, free-list and manifest pages from (`planValidatedCommit`'s `candidates` versus
  `safe`).
- **Why `N+1` may.** Its fallback is `N`, which reaches none of them. Under `synchronous = on` `N` is
  durable before `N+1` begins.
- **Why list at `N` rather than at `N+1`.** The reuse timing is the same either way. Listing in `N`'s
  own list means the orphans are persisted with the commit that created them, so a close or a crash
  after `N` leaks none of them, and a reopen needs no in-memory carry-over.

The rule is a pure function of the committed state, the commit, and the watermark, like every other
allocation rule, so a core that implements it produces the same bytes as another.

### 3.3 Readers

A reader pinned at version `v` may read any page reachable at `v`. A page listed by commit `N` is dead
from `N` on, so it may be reused only once `oldest_live_txid ≥ N`.

Today the list has one generation, `free_gen_txid`, and the gate is all or nothing
(transactions.md §8). With a listing per commit, one generation for the whole list would freeze reuse
whenever any reader is older than the last commit. Instead the writer keeps **per-commit generations
in memory**: the base list (dead at `free_gen_txid`) plus, for each later commit, the set it listed and
its txid. Before planning, every generation with `txid ≤ oldest_live_txid` merges into the base
(`free_gen_txid` rises to the largest merged txid); the allocator may reuse only the base. The
generations are never persisted: at open every listed page is dead at the committed version and no
reader exists, so the whole list is one generation again. When every reader is current
(`oldest_live == committed`, the single-handle case) everything merges and nothing is restricted.

**Page clients.** A client cannot tell generations apart: it allocates from the free list it loaded
from the owner's meta. The owner therefore withholds the reuse grant (page-service.md §6) while any
unmerged generation remains; the client then allocates from the high-water, as it does today under an
older reader. With every reader current the grant is unchanged.

### 3.4 Batched durability (`synchronous = off`)

[validated-cow.md](validated-cow.md) §4 keeps two facts: the **floor** (newest durable generation)
and the **pending set** (every body page written since the floor, listed in each pending commit's
cumulative manifest). Its rule 3 lets a commit inside a batch reuse only pages the floor had free; its
rule 4 keeps every pending page off the free list. Orphans split by that same line:

- **An orphan that is not pending** was written at or before the floor. It is listed by the commit
  that orphans it, exactly as under `synchronous = on`. It is not on the floor's recorded free list,
  so rule 3 already keeps the batch from reusing it; it becomes reusable when the next flush moves the
  floor past the commit that listed it.
- **An orphan that is pending** — written inside the batch and orphaned inside it, as most catalog,
  free-list-chain, and hot-leaf pages of a relaxed churn are — is a dependency of the cumulative
  manifest of every later pending commit **and of the floor the next flush makes**. Listing it would
  put it on that floor's free list, which the following batch records as reusable (rule 3) while the
  floor is still the fallback whose manifest validates it. So it is **held** in memory and listed by
  the first commit that plans with nothing pending — after the flush, before that commit records its
  own floor list, which therefore does not contain it. It becomes reusable at the flush after that.

Rule 4 is unchanged; the held pages are what it used to leak until the next rebuild.

### 3.5 Leaks and the residual rebuild

With the rule, an owner's commit leaves no dead page unlisted. Some remain possible:

- **A close or crash with held pages**: they lived only in memory. Bounded by one batch's pages
  (the batch cap, about one page of manifest entries). A close-time maintenance commit that lists
  them would remove this case; not prototyped.
- **A one-entry list** is kept in memory and not persisted today (`serializeFreeList`: a chain page
  for one entry is not worth it), so a reopen loses that page.
- **Page-client commits.** A client plans against its owner's free list and does not list its
  orphans (rebuilds are owner-only, page-service.md §6), and the owner's next commit does not know
  the client commit's catalog, GiST and manifest pages. Letting the client list its orphans needs the
  owner to verify them, which is the reachability work rebuilds avoid; not prototyped.
- **Files written under today's rule** (or by another core) carry their unlisted orphans.

A **rare rebuild** stays as the backstop, with a trigger that no longer estimates. The committed state
determines exactly how many dead pages no list holds:

    leaked = page_count − first_body_page − |free list| − live_pages
             − free-list chain pages − manifest-overflow pages − held pages

A commit rebuilds when it may (no older reader, owner-only as today) and `leaked > compact_min_pages`
and `leaked × leak_den > live_pages` (`leak_den = 16`, `[reclamation]` in `spec/cost/schedule.toml`). The
page-service owner's maintenance-commit trigger uses the same test. A rebuild clears the generations
(every listed page is then dead at the rebuild's txid, which `oldest_live` equals) and keeps held only
the pages still pending, which the rebuild excludes as today.

### 3.6 Trailing truncation

Unchanged in rule: the trailing run is taken from the safe free pages (format.md *Trailing
truncation*). A commit's own orphans are not safe for that commit, so the top of the file it frees is
cut by the next commit. A page held by an unmerged generation or by the batch is not safe either, so it
blocks the run until it is. Truncation still removes only safe pages, so it never touches an unmerged
generation or the held set (held pages are listed, unsafe, by the very commit that may truncate); the
writer trims the generations to the new high-water as a precaution.

### 3.7 The kept-chain prototype

[free-list-kept-chain.md](free-list-kept-chain.md) keeps the persisted chain when a commit's list is a
suffix of the previous one: it relies on the list shrinking only from its ends between rebuilds (§2
there). Incremental reclamation adds entries in the middle of the list at almost every commit, so the
suffix test fails on almost every commit and the kept chain degenerates to today's fresh chain. The two
do not compose; a commit under this rule writes a fresh chain. The Go prototype and its gate were
removed when this rule was adopted, and [free-list-kept-chain.md](free-list-kept-chain.md) is now a
closed record (its §8). The *deferred-interior* prototype is the opposite case: it attacks pages this
rule does not touch and still halves a single-row commit's bytes over it. Composing them exposed a
bug in that prototype — a remap entry could outlive the page it names, which this rule is the first
thing to reuse, and which the old rebuild had been healing — fixed in
[deferred-interior.md](deferred-interior.md) §8.3. It also showed where this rule and that prototype
pull against each other: trailing truncation cuts only a contiguous run of free pages, so one retained
interior at the top pins a file's length. The prototype now moves that one page per commit
([deferred-interior.md](deferred-interior.md) §8.4); nothing in this rule changes.

The cost they each attack shrinks under this proposal anyway: the list is now about the size of the
pages recent commits orphaned (plus whatever a delete-heavy commit freed), not up to one entry per
live page, so the fresh chain a commit writes is small. A large free list (after a big delete) is still
rewritten whole by every commit until it is consumed or truncated. A persistent structure that takes
insertions in the middle without a rewrite — a list of freed runs appended per commit and
consolidated occasionally, or a free-space B-tree — would serve both; it needs a format change and is
not specified here.

### 3.8 Cost

The extra work per commit is: listing ids instead of counting them (the same walk); one header read
per orphaned overflow page (`chainPages`); a sorted merge of the orphans into the persisted list; and
the free-list chain, now holding the orphans until the next commit consumes them. In return the
periodic rebuild, which reads the whole reachable database, runs only to recover leaks (§3.5).

## 4. Why it is safe

- **Torn writes and fallback (§3.2).** No commit overwrites a page its fallback reaches: its own
  orphans are not safe for it, and every page of the base list is dead at the fallback.
- **Readers (§3.3).** A page reaches the base list only when `oldest_live` is at or past the commit
  that listed it. A reader pins the committed version or newer, so the watermark only rises.
- **Batches (§3.4).** A pending page is never listed while a meta whose manifest names it can be a
  recovery candidate: not by a pending commit (rule 4), and not by the floor, since held pages are
  listed only after the floor moves. A non-pending orphan is listed by a pending commit but never
  reused before the next flush (rule 3).
- **Recovery to any generation between the floor and the newest attempted commit** finds that
  generation's own list, every entry of which is dead at it and was not reused by any later commit of
  the batch (rule 3).
- **Page clients.** A client uses the grant only when every listed page is in the base generation.

## 5. The simulator's growth oracle

`checkGrowth` (`impl/go/sim_runner_test.go`) asserts today only after a commit whose trigger was due:
then every page below the high-water is meta, live, free, or written by the commit. Under the rule it
holds after **every** quiet commit (no reader pinned, no other process, no fault, not inside a relaxed
batch): `page_count − first_body − live − free ≤ pages the commit wrote + dead pages the lifetime began
with unlisted`. The second term is what the open inherited (§3.5) and is zero on a fresh file; a
rebuild resets it. Its follow-on check — the next quiet commit reuses that free list before it grows
the file — then applies after every quiet commit too. So the file is its live pages plus the pages the
last commit wrote, plus what readers and batches legitimately hold back.

The prototype also found that the corruption step (`stepCorrupt`) reopens the database without
starting a new growth lifetime; under the tightened check that reads as a growth violation (the
one-entry in-memory list of §3.5 did not survive the close). It now calls `observeGrowth` after its
reopen, as the replay step does.

## 6. Interaction with the reopen-seed fix

A separate change seeds the in-memory/periodic trigger at open with the meta's exact `live_pages`
instead of `page_count − |free list|`, which had let a file reach about 4× live across reopens. The
leak trigger of §3.5 uses `live_pages` directly and needs no seed; file commits no longer use the
periodic trigger at all. The measurements in §7 show the periodic rule both with and without that
fix.

## 7. The Go prototype and its measurements

This section records the prototype as it stood before adoption, when it was Go-only and behind
`JED_INCR_RECLAIM=on`; the switch is gone and the rule is now on in every core. In `impl/go`:

- `incr_reclaim.go`: the in-memory state (`incrState`: the
  committed state's catalog, GiST and manifest pages; the generations; the held set), the orphan-id
  walk (`orphanedTreePageIDs`, `liveCount.afterIDs`), the leak count and trigger, and a verifier
  (`JED_INCR_VERIFY=on`: after every commit, a reachability walk checks that no listed page is live or
  written by the commit, and with `JED_INCR_VERIFY_STRICT=on` panics if a dead page is unlisted beyond
  what the lifetime began with).
- `format.go`: `incrementalWrite` carries the catalog and GiST page ids it wrote; the load recovers
  the committed state's; `freeListCandidates` merges the orphans into the persisted list and uses the
  leak trigger.
- `validated_commit.go`: `planValidatedCommit` takes the orphans as candidates that are not safe, and
  always writes a fresh chain.
- `shared.go`: the commit merges the generations against the watermark (the reader watermark rides in
  `budgetCtx`), restricts reuse to the base, computes and records the orphans; transfers on reload.
- `pageservice_owner.go`: the reuse grant (§3.3), a client commit's foreign state, the maintenance
  trigger.
- `file.go`: the bare-engine commit path.

Not combined with Go's deferred-interior or symbol-table prototypes (`incrOn` is false when either is
on, and the commit falls back to the `compact_growth` rebuild): both keep pages off the free list by
rules of their own. In-memory databases and temp domains keep
their post-commit rebuild (`maybeCompact`); they have no file to grow.

### 7.1 Run against it

- **The Go core's unit tests** with the switch on, and again with `JED_INCR_VERIFY=on
  JED_INCR_VERIFY_STRICT=on`: everything passes except two tests whose yardstick is today's mechanism.
  `TestReclaimTriggerFileCommitsDoNotWalkEveryTime` counts walks by changes of `free_gen_txid`, which
  now advances at every commit as generations merge (the real walk counter, `reclaimWalks`, stays at
  0). `TestTrailingTruncationWaitsForTheFlushUnderSynchronousOff` requires the file to end below a third
  of its peak; incremental reclamation lowers the peak itself (1,903 pages against 3,236; 1 rebuild walk
  against 863 over the 1,500 relaxed commits), and both modes end at the same ~674 pages.
  `TestFallbackReaderSnapshotIsolationUnderReclamation` waits for a rebuild-path commit that never
  comes, so it is skipped under the switch; `TestIncrReclaimReaderHoldsOnlyNewerGenerations` (new) covers
  the reader case instead: a pinned reader keeps its snapshot, holds back only newer generations, and
  once it closes the generations merge and the file stops growing with no page left unlisted.
- **The conformance corpus** on Go with the switch and the strict verifier: memory 761 passed, disk 677
  passed, 0 failed.
- **`mise run sim:sweep 1..60 all go`** with the switch, the verifier and strict mode: 1,320 traces over
  every profile (encryption, the page service, relaxed durability, truncation, reuse under pins, crash
  storms, corruption, …) pass every oracle, including the tightened growth check of §5. The first sweep
  found two harness problems, both fixed: the verifier panicked on an injected read fault instead of
  skipping its walk (9 `resource` traces), and the `stepCorrupt` lifetime gap of §5 (6 `corruption`
  traces). With the switch off the same sweep, with the oracle change, also passes all 1,320.
- **`mise run sim:regress go`**, both ways: the 19 traces pass, and the independent Ruby checker
  (`verify.rb --check-image`) decoded all 153 crash and backup images written under the switch, which
  confirms the files need no format change.
- **`mise run concurrency:process`** (real Rust, Go and Node processes in every owner/client pairing)
  with the switch and the verifier on the Go actors: 260 pairings pass, including every page-service,
  promotion, crash-release and relaxed scenario. The 19 that fail are all pinned bytes or page counts
  wherever a Go actor commits: "final bytes differ across core pairings" (13), an expected page count
  (`storage_limit`: 27 pages where 33 are pinned), and the backup scenario's expected
  `txid:live:pages:free` (a page-identical copy is `10:12:83:68` where `10:12:166:51` is pinned: the
  source file is half the size). Each such image still passes `verify.rb --check-image`. This is the
  cross-core byte contract of §9, which needs all three cores to adopt the rule; no failure was a
  wrong row, an `XX001`, or a protocol error.

Not run: anything in Rust or TS (they would read these files, but their own commits follow today's
rule), the race detector, and real-device `fsync`.

### 7.2 Measurements

Two workloads, `SkipFsync` (no barrier), 8 KiB pages, one machine, two alternating runs of each mode.
The modes: **2×** is master; **2× + seed** adds the reopen-seed fix of §6, the fair baseline;
**incr** is this prototype (`JED_INCR_LEAK_DEN` 16). Page counts are identical in both runs (the
workloads are deterministic); latencies are the first run's and the second agrees within a few
percent. "Walks" is reachability rebuilds; ratios are `page_count / live_pages` sampled after every
commit. The `EXPERIMENT` harness (`impl/go/zz_incr_measure_test.go`, `JED_INCR_MEASURE=a|b`) was removed at adoption; it is in commit `71a58efb`.

**(a)** A 300k-row table with a secondary index (`id i64 PRIMARY KEY, k i64, pad text` of 40–80
bytes, index on `k`), loaded by 600 random-key 500-row inserts, then 25,000 single-row updates of the
indexed column across 5 reopens, 8 MiB cache. 4,953 live pages; compaction leaves 4,955.

| phase | mode | walks | mean ratio | end ratio | mean latency | p50 | p99 | max |
|---|---|---|---|---|---|---|---|---|
| load (600) | 2× | 193 | 2.15 | 2.04 | 36.3 ms | 37.2 ms | 68.2 ms | 72.6 ms |
| | 2× + seed | 193 | 2.15 | 2.04 | 36.5 ms | 36.9 ms | 73.7 ms | 82.9 ms |
| | incr | 0 | 1.38 | 1.17 | 33.3 ms | 35.7 ms | 45.4 ms | 48.3 ms |
| updates (25,000) | 2× | 63 | 3.25 | 2.80 | 489 µs | 410 µs | 1.01 ms | 30.3 ms |
| | 2× + seed | 53 | 2.00 | 2.00 | 397 µs | 322 µs | 0.91 ms | 31.4 ms |
| | incr | 0 | 1.02 | 1.003 | 342 µs | 310 µs | 0.74 ms | 1.1 ms |

Closed file: 114.6 MB (2×), 81.6 MB (2× + seed), **41.5 MB** (incr; 4,967 pages: 4,953 live, 10 free,
2 meta, 2 free-list chain).

**(b)** The five-table database of `expBuild` (`zz_formatexp_test.go`: 1.3M rows in 2,610 500-row
commits, with text, decimal, timestamptz, uuid, f64 and jsonb columns and five secondary indexes), then
10,000 single-row updates across `orders` and `users` (two of the three statement shapes change an
indexed column) over 2 reopens,
8 MiB cache. 11,760 live pages; compaction leaves 11,762.

| phase | mode | walks | mean ratio | end ratio | mean latency | p50 | p99 | max |
|---|---|---|---|---|---|---|---|---|
| build (2,610) | 2× | 220 | 1.99 | 1.99 | 15.9 ms | 12.6 ms | 85.3 ms | 156 ms |
| | 2× + seed | 220 | 1.99 | 1.99 | 15.9 ms | 12.5 ms | 86.0 ms | 158 ms |
| | incr | 1 | 1.09 | 1.04 | 13.0 ms | 13.1 ms | 32.7 ms | 40.8 ms |
| updates (10,000) | 2× | 8 | 2.51 | 3.09 | 553 µs | 411 µs | 1.39 ms | 126 ms |
| | 2× + seed | 8 | 1.99 | 1.995 | 507 µs | 382 µs | 1.21 ms | 132 ms |
| | incr | 0 | 1.02 | 1.006 | 328 µs | 296 µs | 0.98 ms | 3.0 ms |

Closed file: 298.4 MB (2×), 192.5 MB (2× + seed), **97.8 MB** (incr; compacted 96.4 MB). The one walk in
the incremental build is the fresh file's first commit (the `liveAtCompaction == 0` sentinel), 0.6 ms.

What the numbers say:

- **File size.** The file stays at about 1.0× its live size in steady churn (1.003× and 1.006× at the
  end; 1.02× on average), against exactly the 2× the rule targets with the seed fix, and up to 3.4× on
  master. A random-key bulk load runs at 1.4× on average (each 500-row commit orphans many leaves, which
  the next commit reuses) instead of 2.1×.
- **Walks.** None after the first commit. Today's rule walked on a third of the random-key load's
  commits (193 of 600) and on 220 of the build's 2,610.
- **Latency.** The tail is the walk: the worst single-row update falls from 30 ms (a) and 126–132 ms
  (b) to 1–3 ms, and the bulk-load p99 from 68–86 ms to 33–45 ms. The mean falls too (342 vs 397 µs,
  328 vs 507 µs), because the walks' amortized cost is gone. The per-commit cost of listing is
  visible only in the build's median, 13.1 ms against 12.5 ms (+5%): its commits orphan the most pages
  and rewrite the largest free-list chains.
- These are page-cache numbers with `fsync` off. A walk reads the whole reachable database, so with
  real I/O and a cache smaller than the file it would cost more, not less; the per-commit listing reads
  only orphaned overflow headers (none in these workloads).

## 8. Open questions

1. Whether to list held pages at close with a maintenance commit (§3.5), and whether a page client
   should list its own orphans for the owner to check.
2. Whether the free list wants a structure that takes middle insertions without a rewrite (§3.7),
   folding in the kept-chain idea.
3. Whether in-memory databases should follow the same rule, retiring `maybeCompact`'s walk there too.

## 9. Adoption

The rule is specified in `format.md` *Reclamation* and *Trailing truncation*, `validated-cow.md` §2
and §4 rule 4 (held pages), `transactions.md` §8 (per-commit generations), `page-service.md` §6 (the
grant, client orphans as leaks), `pageservice/README.md` (the maintenance trigger), `memory.md` §8.4
and `spec/cost/schedule.toml` (`leak_den`), `storage.md` §6, and `simulation-testing.md` §8.3 (the
growth oracle of §5, in all three runners). The bench dataset `generator_version` was bumped (17).
The Ruby reference writes only from-scratch images, so the goldens are unchanged. The process
corpus's pinned page counts and backup tuple were updated for the smaller files. A page client's
commit now often makes the owner's maintenance commit due (its orphans are leaks), and an owner commit
depends on the reader watermark more often than before (generations merge by it; a pin on the
committed version defers a rebuild), while a client's `end_read` and EOF reach the owner
asynchronously (pageservice/README.md §6.3). So two byte-identical scenarios (`cow_overflow`,
`cow_stale_orphan`) were reordered: the owner takes the writer gate after a client commit, which waits
for its maintenance decision, before its pinned reader closes; and the replacement client starts after
the owner's commit, not before it.
