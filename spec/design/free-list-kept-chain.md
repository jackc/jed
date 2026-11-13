# Free list: keep the chain between rebuilds (closed)

**Status: closed, 2026-11-13. Superseded by [incremental-reclaim.md](incremental-reclaim.md);** the Go
prototype and its `JED_FREE_KEPT_CHAIN` gate were removed when that rule was adopted, and meta offset
60 is reserved (0) again. Nothing here ever reached `format.md`, the Ruby reference, or the Rust and TS
cores. This document is kept as the research record: the measured cost of the per-commit free-list
rewrite (§1), the suffix observation the design rested on (§2), the design itself (§3 to §4), and why
it was closed (§8). Its residual — a list that takes insertions in the middle without a rewrite —
is [incremental-reclaim.md](incremental-reclaim.md) §8 open question 2.

## 1. The problem

A file commit persists the whole free list into fresh `page_type 7` pages
([../fileformat/format.md](../fileformat/format.md) *Free-list page*: "rewritten fresh every commit").
The list holds one `u32` per free page, and between reclamation rebuilds it sits anywhere from empty
to about one entry per live page (the `compact_growth = 2` trigger). So the bytes a commit writes for
the free list are proportional to the size of the database, not to the size of the change.

Measured on a 188 MB database (1.3M rows, 8 KiB pages, 24,000 live pages), single-row commits:

| commit | leaf | interior | catalog | free list | meta |
|---|---|---|---|---|---|
| update a non-indexed column | 1.0 | 2.0 | 1.0 | 4.6 | 1 |
| update an indexed column | 3.1 | 4.9 | 1.0 | 5.1 | 1 |
| insert (append) | 3.0 | 6.0 | 1.0 | 6.7 | 1 |

By the same arithmetic (average list ≈ half the live page count × 4 bytes) a 10 GB database writes
about 2.4 MB of free list per commit and a 1 TB database about 240 MB. That is at odds with the
larger-than-RAM non-foreclosure rule (CLAUDE.md §9).

## 2. The observation

Between rebuilds the list only shrinks, and it shrinks from its ends:

- allocation takes the **lowest** free index first, so tree, catalog, and reserved manifest pages
  consume a **prefix** of the ascending list;
- trailing truncation removes the top of the file, a **suffix**;
- a commit's own orphans are not added to the list; they wait for the next rebuild.

So in a non-rebuild commit the new list is normally one contiguous run of the old one, and the old
chain already contains exactly those entries.

Checked with a hook on every file commit:

| run | commits | rebuilds | list unchanged | prefix consumed | prefix + tail | not a run |
|---|---|---|---|---|---|---|
| the Go core's whole test suite | 8,394 | 867 | 6,410 | 697 | 111 | 309 |
| bulk load + 12,000 single-row commits | 14,610 | 115 | 3,947 | 10,546 | 2 | 0 |

Every "not a run" case was a commit inside a `synchronous = off` batch, where reuse is restricted to
pages free at the floor ([validated-cow.md](validated-cow.md) §4 rule 3) and the allocator skips the
rest. After a rebuild inside or just before a batch, the allowed pages are scattered through the new
list. §3 handles this by falling back to today's behavior.

## 3. The design

### 3.1 Meta field

One reserved field of the meta page takes a meaning, at the same offset in the plaintext and
encrypted layouts:

| offset | size | field |
|---|---|---|
| 60 | 4 | `free_list_low` (u32) — the lowest free page index, or `0` when `free_list_head = 0` |

The free list is every entry `≥ free_list_low` of the chain from `free_list_head` to its end.
Entries below `free_list_low` (all in the head page) were consumed since the chain was written and
mean nothing. `free_list_low` must equal the head page's first entry at or above it.

An earlier draft used a positional window (`skip`, `count`), which also expresses a truncated tail.
It needs six bytes; the encrypted meta has only these four spare without moving the inline manifest.
The value form costs one fresh chain per truncating commit (§3.2).

### 3.2 The commit rule

Let `P` be the committed free list and `L` the list this commit must persist: `P` after the
commit's tree, GiST, and catalog allocation, manifest reservation, and trailing truncation, **with no
free-list pages drawn**.

- **Rebuild commits** (the reclamation trigger fired) serialize a fresh chain exactly as today.
- **Otherwise, if `L` is empty:** `free_list_head = 0`, no page written.
- **Otherwise, if `L` is a suffix of `P`:** the chain is **kept**. No free-list page is written or
  allocated. `free_list_head` is the kept chain's page holding `L[0]`, and `free_list_low = L[0]`.
- **Otherwise** — a truncating commit, or the relaxed-batch case of §2 — serialize a fresh chain as
  today.

Every fresh chain also records `free_list_low` = its first entry.

The rule is a pure function of the committed state and the commit, like every other allocation rule,
so the bytes stay identical across cores. Reserved manifest ids come off the front of the safe list,
so they extend the consumed prefix and never change whether `L` is a suffix.

In an encrypted file the meta's `free_list_tag` must be the tag of the new head page. That tag is in
the `next_page` pointer of the page before it, so the writer keeps the chain's page references in
memory (read at open, or produced when it wrote the chain).

**The manifest no longer names the free-list head in every commit.** Today recovery rejects a
candidate whose manifest does not list its `free_list_head` (the `cow_free_head_manifest` fixture). A
kept head was written, and listed, by an earlier commit, so that rule cannot hold. A head that is not
a valid free-list page is then `XX001` at load rather than a fallback to the older candidate. The
fixture and its conformance test need redefining.

### 3.3 Which chain pages are live

"The current free-list pages", for the rebuild's reachability walk, the integrity check, backup, and
the page service's pinned-generation copy, stay what they are today: the chain from `free_list_head`
to its end. Pages of the old chain before the new head are dead and unlisted, like any other orphan,
until a rebuild lists them.

## 4. Why it is safe

- **Torn writes and fallback.** A kept-chain commit writes nothing to the chain, so the fallback meta
  still finds its own window intact. The pages the new commit consumed were free at the fallback, so
  the fallback neither reads them nor minds that they were overwritten. This is today's argument for
  reusing a free page, unchanged.
- **Chain pages are never reused while a recoverable meta names them.** A chain page that drops out
  of the window becomes an unlisted orphan. It can only return to a free list at a rebuild, as one of
  that commit's fresh orphans, and fresh orphans are not reused until the following commit
  (format.md *Reclamation*), by which time the fallback is the rebuild commit, whose meta names the
  new chain.
- **Manifest and recovery.** A kept chain's pages are inherited pages: they were listed in the
  manifest of the rebuild commit that wrote them and are protected by their page checksum afterward,
  as an inherited tree page is. Under `synchronous = off` a commit's manifest lists every page written
  since the floor, so a rebuild's chain pages are covered until the flush.
- **Reader watermark.** When reuse is gated off the list is unchanged, so the chain is kept and the
  commit writes no free-list page. Today that commit rewrites the chain at the high-water, growing
  the file for as long as an old reader stays pinned.
- **Trailing truncation.** A truncating commit removes the top of the list, so its list is not a
  suffix and it writes a fresh chain. A kept chain therefore never holds an entry at or beyond
  `page_count`.
- **Page clients.** A client's planned delta applies the same rule and normally writes no free-list
  page. Rebuilds stay owner-only ([page-service.md](page-service.md) §6). The owner's check of a
  client's delta drops the free-list-head requirement. `begin_read` already carries the meta.
- **Compaction, rekey, `create`.** From-scratch images have an empty list. Goldens change only by the
  version byte and meta checksum, as at v25.

## 5. Effect

Replaying the second run of §2 under the rule: **55,397 free-list pages written today, 491 under the
rule** (only the 115 rebuild commits write any). A single-row update then writes 4 body pages and the
meta instead of about 9.

Not measured: commit latency with the change, and the real-device effect with `fsync` on.

## 6. What it does not fix

- **The rebuild itself.** A rebuild still walks every reachable page and writes the whole list. Its
  cost is amortized across the commits between rebuilds, but it is one commit that reads the entire
  database, which at TB scale is a long pause.
- **File size.** The file still sits between 1× and about 2× its live size by design.
- **Relaxed batches after a rebuild** still rewrite the chain per commit until the flush.
- **Open** still reads the whole list.

A larger follow-on would address the first two together: every commit already walks its own orphans
to maintain `live_pages` ([memory.md](memory.md) §8.7), so it could record them instead of only
counting them. Orphans would then join the free list a commit or two after they are created, with no
reachability walk, and the file would stay near its live size. That changes the list from one sorted
run to several generations and needs its own design for allocation order, truncation, and the
batched-durability reuse rule. It is not specified here.

**That follow-on is what landed** ([incremental-reclaim.md](incremental-reclaim.md), 2026-11-13), and
it is why this proposal is closed — see §8.

## 7. Open questions

1. Value form (`free_list_low`, this draft) or positional window. The positional form avoids the
   fresh chain on a truncating commit but needs a meta layout change.
2. What replaces the `cow_free_head_manifest` fixture's guarantee, if anything.
3. Whether to stop rebuilding inside a relaxed batch, which would make the fresh-chain branch rarer.

## 8. Why it was closed

[incremental-reclaim.md](incremental-reclaim.md) took the §6 follow-on instead: a commit lists the
pages it orphans rather than only counting them. That removes both of §6's first two items — the
whole-database rebuild walk becomes a leak backstop, and the file stays near 1× its live size — and in
doing so it **invalidates §2, the observation this design rests on**. Orphans are merged into the
middle of the list at almost every commit, so the list no longer shrinks only from its ends: the
suffix test of §3.2 fails on almost every commit and the kept chain degenerates to today's fresh
chain. The two do not compose (incremental-reclaim.md §3.7).

The cost of §1 shrank with the same change, which is the second reason not to pursue a rework. The
list is now about the size of what recent commits orphaned, not up to one entry per live page, so the
fresh chain each commit writes is small and the TB-scale arithmetic of §1 no longer applies.

What survives is narrower: **a large free list — after a big delete — is still rewritten whole by
every commit until it is consumed or truncated.** Closing that needs a persistent structure that
takes insertions in the middle without a rewrite (a list of freed runs appended per commit and
consolidated occasionally, or a free-space B-tree), which would serve both this and incremental
reclamation. It is a format change and is not specified anywhere;
[incremental-reclaim.md](incremental-reclaim.md) §8 open question 2 is where it is recorded.

Had it been adopted, the work would have been: `format.md` (meta table, *Free-list page*,
*Reclamation*, *Trailing truncation*, version scope), `encrypted.md` §6, `validated-cow.md` §2/§4,
`page-service.md` §6 and the `spec/pageservice/` meta fixtures, `integrity-check.md` §3; the Ruby
reference and `verify.rb`; the three cores; simulator checkers for the free-list oracles; per-core
incremental-commit tests that pin free-list bytes; the bench dataset `generator_version`.

## 9. The Go prototype, as measured before removal

It lived in `impl/go` behind `JED_FREE_KEPT_CHAIN=on`: `format.go` (the meta field, `readFreeList`,
`serializeFreeList`, `freeChain.lasts`/`from`, `freeSuffix`), `validated_commit.go` (the rule in
`planValidatedCommit`), and the plumbing that carried the chain beside `freePages`. Two shortcuts kept
the existing fixtures readable without a version bump, and would not have survived into a real v51: a
chain with `free_list_low = 0` was read whole, and such a file kept the old
manifest-names-the-head rule.

Run against it:

- the Go core's unit tests (one unrelated failure, `TestConcatThreeForms`, which already failed before the
  prototype was written);
- the conformance corpus on Go, memory and disk modes (752 and 668 passed, 0 failed);
- `mise run sim:sweep 1..60 all go`: 1,320 traces over every profile, including encryption, the page
  service, relaxed durability, truncation, and reuse under pinned readers, passed every oracle;
- `mise run sim:regress go`: the 19 traces pass the Go core's own oracles. All 20 reported failures
  are the independent Ruby checker rejecting the images, as expected: `verify.rb` does not know the
  field.

Never run: anything cross-core (no other core could read a kept chain), the page-service process
corpus, the race detector.

Measured on the §1 database, 2,000 single-row commits per case, `fsync` off, one run each — against
the **pre-incremental-reclamation** baseline, which is why these numbers no longer describe a gap that
exists:

| commit | free-list pages, before → after | bytes written, before → after | time, before → after |
|---|---|---|---|
| update a non-indexed column | 4.6 → 0.0 | 78 KB → 47 KB | 744 µs → 710 µs |
| update an indexed column | 5.1 → 0.0 | 130 KB → 82 KB | 1,064 µs → 781 µs |
| insert (append) | 6.7 → 0.0 | 145 KB → 96 KB | 1,010 µs → 856 µs |

The bulk load's free-list pages fell from 3.0 to 0.2 per commit. The file checked clean with a full
integrity check before and after a reopen.

The remaining `freeChain` type in the Go core is not part of this: it is the pages the persisted chain
occupies, which the reclamation leak measure subtracts (incremental-reclaim.md §3.5).
