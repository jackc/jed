# Write costs on dense leaves

**Status: measured; #5's direction is format v54 (§6), awaiting the maintainer's go-ahead.** Go prototypes only, every switch off by
default; nothing here is in the format. This note measures two write regressions that came with the
compact leaf of format v52 ([leaf-structural.md](leaf-structural.md)) and recommends what to do
about each (§6). Any format change it proposes is the maintainer's decision; if one is approved,
the follow-on is format v54, with regenerated goldens in all three cores and the Ruby reference
(`spec/fileformat/verify.rb`).

The two issues, from the benchmark regression list:

- **#4, edits that shift arrays.** A v52 leaf holds about 4.8 times as many narrow rows as a v51
  leaf. Inserting or removing one row of a dirty leaf shifts the rest of the leaf's parallel
  in-memory arrays, so each such edit copies about 4.8 times as much.
- **#5, a full leaf splits when one value widens a column.** Leaves are packed against their
  content-dependent size. Updating one integer so that it widens a base-plus-offsets column widens
  every row of that column, so a brim-full leaf overflows and splits where v51 updated it in place.

Leaf evening (format.md *Evening out with a sibling*) is measured with both.

## 1. Method

- **Go core only**, 8 KiB pages. Rust and TS were not measured.
- **Revisions:** v51 is `80092e7b3`; "master" is `3e5962f9a` (format v53); the prototypes are this
  branch, run as one binary with environment switches.
- **Benchmarks:** `bench-jed` from the bench harness, each revision's binary run directly against a
  copy of the current corpus with only the five benchmarks below and twice the iterations
  (`fk_cascade_delete_indexed` 380, `insert_rollback` 60 × 1000 inserts, `delete_range_rollback`
  38, `delete_insert_churn` 60 × 1000 re-keys, `update_untouched_large_rollback` 18,400). v51's own
  corpus has different iteration counts, so both revisions read the current definitions.
- **Datasets:** rebuilt for every revision, and again for every switch that changes bytes (slack,
  exceptions, evening off), with `bench-setup --engine jed`. Only the jed engine ran, so no
  PostgreSQL was needed and none was started for the scratch worktrees.
- **Alternation:** nine rounds; each round runs every variant once, in a fixed order; each run
  pinned to CPUs 2–5 (`taskset`). The tables give the median of the nine runs and the max/min
  spread. `mise run bench:diff` was run on per-variant median runs (§3, §4); every checksum
  matched.
- **Interference:** other sessions shared the machine. The load average reached 10 just before an
  early Phase B run and was 1.6–2.9 during the final runs reported here. v51's
  `insert_rollback` spread (2.71) comes from one 7.7 ms outlier; its median is unaffected.
- **Size** is the **live page count** from the meta page (offset 56), never the file size.
- **No-switch check:** this branch with every switch off against the master binary, nine
  alternating rounds: every benchmark within ±2.3% (cascade 2.708 → 2.730 ms, insert 3.850 →
  3.812, range 10.99 → 11.25, churn 5.093 → 5.130, update 0.042 → 0.041). The hooks the
  prototypes add to the default paths cost nothing measurable.

## 2. Where the time goes (master)

| benchmark | v51 | master | master / v51 | what the profile shows |
|---|---|---|---|---|
| `fk_cascade_delete_indexed` | 1.634 ms | 2.749 ms | 1.68× | `memmove` 32.5% of CPU, almost all `typedslicecopy` from `removeKeyAtMutable` and `removeValAtMutable` |
| `insert_rollback` | 2.911 ms | 3.725 ms | 1.28× | the insert shift is 10%; evening 5%; splits 8%; GC 20% |
| `delete_range_rollback` | 7.693 ms | 11.275 ms | 1.47× | shifting, plus merging and rebuilding leaves that are 4.8× larger |
| `delete_insert_churn` | 4.376 ms | 5.111 ms | 1.17× | little shifting (see the counts below) |
| `update_untouched_large_rollback` | 0.018 ms | 0.042 ms | 2.33× | the split: decoding every row of the leaf 23%, `buildLeaf` 17%, evening 9%, sizing 10% |

How many array slots each edit moved, counted in a Go test that runs the same workloads three
times each and commits (`TestLeafEditModes` with `JED_LEAF_EDIT_STATS=1`; keys array only;
transactions only, not the load):

| workload | edits | shift the tail (master) | shorter side | gap |
|---|---|---|---|---|
| cascade | 6,300 | 833 per edit | 301 | 12, plus 69 gap closes moving 49,820 slots in total |
| insert (random index positions) | 6,000 | 153 | 85 | 112 |
| range delete | 30,000 | 321 | 0.4 | 3.8 |
| churn | 6,000 | 172 | 0 | 4.2 |

The bulk-loaded leaves are full: the cascade's child table holds 1,961 rows per leaf at 99.6% of
`C`, its index 730 at 99.6%, `del_orders` 476 at 99.5%, `wide` 143 at 94% (the last leaf is
partly empty).

## 3. Phase A (#4): the edit path

Two prototypes behind `JED_LEAF_EDIT` (`impl/go/leaf_edit.go`), with no change to bytes, results
or costs:

- **`short`**: shift whichever side of the edited row is shorter. A removal on the front side
  shifts the front rows up and starts the slices one slot later. An insertion on the front side
  needs room before the first row: each dirty leaf records the allocation its arrays were cut from
  and their offset in it (`frontRoom`), and regrows with room at both ends when there is none.
- **`gap`**: a movable gap. The arrays hold the rows plus a run of empty slots; an edit moves the gap
  to the edited row, copying only the rows in between, then grows or shrinks it. Search, the row,
  key and weight accessors, the in-place edits and the lazy leaf's size facts read through the gap.
  Every other site that indexes the arrays closes the gap first, and a mutation generation closes
  all its gaps when it is frozen, so only the owning writer ever sees one. An empty lazy slot holds
  a poison index that makes any unconverted reader panic, which is how the corpus found the sites.

Both pass the full Go corpus in memory and disk modes with the lazy-leaf cross-checks on
(`JED_LEAF_LAZY=check`), and `TestLeafEditModes` checks that all three modes write the same file
bytes.

Medians, nine rounds (ms; spread in brackets):

| benchmark | v51 | master | short | gap |
|---|---|---|---|---|
| `fk_cascade_delete_indexed` | 1.634 (1.33) | 2.749 (1.02) | 1.951 (1.11) | **1.466** (1.22) |
| `insert_rollback` | 2.911 (2.71) | 3.725 (1.17) | 3.859 (1.12) | 4.169 (1.13) |
| `delete_range_rollback` | 7.693 (1.33) | 11.275 (1.07) | 10.340 (1.21) | 10.507 (1.06) |
| `delete_insert_churn` | 4.376 (1.28) | 5.111 (1.06) | 5.038 (1.07) | 5.004 (1.11) |

`bench:diff` against master: shorter side −29.0% cascade, +3.6% insert, −8.3% range, −1.4%
churn; gap −46.7% cascade, +11.9% insert, −6.8% range, −2.1% churn.

- **The gap removes the cascade regression** (0.90× v51); the shorter side removes 70% of it (1.19×
  v51).
- **Neither helps `insert_rollback`.** The shift is a tenth of its time; the rest is splits,
  evening (§5) and allocation in leaves that are 4.8 times larger. The gap is 12% slower there:
  random positions in an index leaf move the gap about as far as the tail shift copies, every move
  copies four arrays, and the gap's arrays are allocated larger. Zeroing the vacated slots on every
  move, which the first version did, made it worse; the version measured zeroes only the weights
  and the lazy index.
- **Range delete and churn gain little.** The range delete's remaining 1.34× (shorter side) is in
  merging and rebuilding the larger leaves, not in shifting; churn already removes from the front
  and appends at the end.

**What a port costs.**

- **Gap:** every site that indexes a dirty leaf's arrays must map through the gap. In Go that is
  about 45 functions and 200 field references across `pmap.go`, `leaf_lazy.go`, `leaf_even.go`,
  `format.go`, `storage.go` and `remap.go`. The prototype avoids converting them by closing the gap
  on escape and at freeze; a port would convert them all, in three cores.
- **Shorter side:** the two edit helpers and one small record per dirty leaf. In Go a removal on
  the front side needs no record at all (it reslices); only a front-side insertion needs the
  headroom. In Rust, `VecDeque::insert` and `remove` already shift the shorter side, but
  changing the leaf vectors' type reaches every reader that wants a contiguous slice; an offset
  into a `Vec` is the contained alternative. TS arrays would need the same offset. Not measured
  outside Go.

## 4. Phase B (#5): when one value widens a column

Three options, the first two as Go prototypes:

- **(a) Slack** (`JED_LEAF_SLACK=d`, `impl/go/leaf_slack.go`): a right-edge split gives its left
  leaf at most `C − ⌊C/d⌋` bytes instead of the most that fits. The fit rule is unchanged, so a
  leaf can still grow to `C` in place. Right-edge splits are how a table loaded in key order is
  filled; elsewhere the balanced split already leaves room. Measured at `d` = 32 (255 bytes) and
  16 (511 bytes).
- **(b) One exception** (`JED_LEAF_FORX=on`, `impl/go/leaf_forx.go`): a base-plus-offsets column
  may store one value whole instead of widening every offset for it. Region kind 6: null bitmap,
  base (`w`), offset width `bw` (u8), exception row (u16), exception body (`w`), `N` offsets of
  `bw` bytes, the exception's offset zero. The excepted value is the column's single largest
  present value, or its single smallest when that gives a narrower width; it must be unique. The
  saving over plain is `N·(w − bw) − (2w + 3)` against kind 1's `N·(w − bw₁) − (w + 1)`; the region
  takes the larger, kind 1 on a tie. The size facts keep the second-smallest and second-largest
  values beside the range, and removing any of the four marks the range stale. The prototype
  encodes and decodes the region: `TestLeafFORXRoundTrip` writes, reopens, updates and deletes
  rows with outliers under the size and patch cross-checks and compares the results with the same
  workload without exceptions, and the full corpus passes with the switch on.
- **(c) Accept the cost.**

### Write time

Medians, nine rounds (ms), with the Phase A benchmarks under each option:

| benchmark | master | slack C/32 | slack C/16 | exception | gap + slack C/32 |
|---|---|---|---|---|---|
| `update_untouched_large_rollback` | 0.042 | **0.011** | **0.011** | **0.011** | 0.011 |
| `fk_cascade_delete_indexed` | 2.749 | 2.939 | 2.723 | 2.739 | 1.675 |
| `insert_rollback` | 3.725 | 3.822 | 4.057 | 3.728 | 4.142 |
| `delete_range_rollback` | 11.275 | 11.463 | 11.581 | 11.420 | 10.534 |
| `delete_insert_churn` | 5.111 | 5.221 | 5.121 | 5.111 | 5.075 |

Every option that keeps the leaf from splitting takes the update to 0.011 ms, faster than v51's
0.018 ms: the leaf's rows are only replaced, so the commit path patches its page instead of
decoding and splitting it. Slack does not measurably reduce the Phase A costs (C/32's +6.9% on the
cascade is within that variant's 1.21 spread; C/16's +8.9% on insert is not explained): it removes
only 3% to 7% of the rows of a leaf. The exception changes nothing outside the update.

### Size

Live pages on the bench datasets, each rebuilt under its switch:

| dataset | v51 | master (v53) | slack C/32 | slack C/16 | exception |
|---|---|---|---|---|---|
| `large` | 13,449 | 9,346 | 9,643 (+3.2%) | 9,957 (+6.5%) | 9,346 (0) |
| `gin` | 1,796 | 475 | 490 (+3.2%) | 507 (+6.7%) | 475 (0) |
| `text` | 5,556 | 5,538 | 5,561 (+0.4%) | 5,589 (+0.9%) | 5,538 (0) |
| `small` | 93 | 56 | 58 | 59 | 56 |

The generated datasets have no lone outliers, so the exception never applies to them and costs
nothing.

What the options buy in a committed workload (`TestLeafWidenGrowth`, `JED_LEAF_SHAPES=1`): a table
loaded in key order, then an UPDATE that widens one value in about every leaf, then another that
widens a second value in about every leaf. Live pages:

| table (rows per leaf) | v51 | master | slack C/32 | slack C/16 | exception |
|---|---|---|---|---|---|
| `wide` (143): `n` 0 → 1 | 31 → 31 → 31 | 25 → 31 → 31 (+24%) | 26 → 26 → 26 | 26 → 26 → 26 | 25 → 25 → 30 (+20%) |
| `d` (476): `v` widened to 4-byte offsets | 559 → 559 → 559 | 212 → 276 → 293 (+38%) | 218 → 287 → 290 | 226 → 292 → 293 | 212 → 214 → 274 (+29%) |
| `c` (1,961): `parent_id` far outside the range | 350 → 350 → 350 | 53 → 84 → 96 (+81%) | 55 → 83 → 96 | 57 → 82 → 100 | 53 → 60 → 72 (+36%) |

- **Slack covers only growth smaller than the slack.** One widening step adds `N` bytes per byte of
  offset width: 143 bytes in `wide`, 952 in `d` (two to four bytes), about 2 KB per byte in `c`.
  Slack large enough for dense leaves would give back much of v52's saving.
- **The exception covers the first widening per leaf and column whatever the leaf's width**, and
  costs nothing when there is no outlier. A second outlier in the same leaf widens the column as
  before.
- **Even after the splits, v53 stays far smaller than v51** (`d` 293 pages against 559, `c` 96 against
  350). A split leaves two half-full leaves, which then take later edits in place.

### What each option changes in the specification

- **(a) Slack.** `format.md` *Fan-out*: the right-edge arm of the split point becomes the largest
  `m` with `leftpayload(m) ≤ C − ⌊C/d⌋` (falling back to `m_max` when no such `m` lets the right
  half fit), with `d` a format constant, and a *Why* note. `leaf-structural.md` gains a section
  for it. The tree shape of every table loaded in key order changes: every multi-leaf golden, and
  the pins the corpus records over page counts (with C/16: `query/index_key_set.test` cost,
  `query/statistics_estimates_p9.test` EXPLAIN estimates, `query/statistics_gin_terms.test`
  ANALYZE cost, and the `54P06` trip points of `resource/storage_memory.test` and
  `resource/storage_file.test`; C/32 changed none of these). Evening's room threshold (C/8) must
  stay above the slack, or a bulk-loaded sibling would qualify and every overflow would rewrite
  two leaves.
- **(b) Exception.** `format.md` *Leaf node*: a kind 6 bullet (layout, which value is excepted,
  saving formula), kind 6 added to the choice rule (smallest kind, the lower kind on a tie) and to
  `region saving` in *Fan-out*; the record-set monotonicity argument still holds (a record adds `w`
  bytes to the column and can raise its saving by at most `w`). `leaf-structural.md` §8 gains the
  exception variant. Encoders and decoders in three cores, the Ruby reference and `verify.rb`; a
  golden with a kind 6 region. Existing goldens and pins are unchanged (the corpus passes with the
  switch on).
- **(c) Accept.** No specification change.

## 5. Leaf evening

`JED_LEAF_EVEN=off` (measurement only; it changes split decisions) against master: `insert_rollback`
−10.9% (3.725 → 3.320 ms), `update_untouched_large_rollback` −12.4% (0.042 → 0.037 ms), the other
three within ±1.5%. That matches the 12% reported when evening landed.

- In `insert_rollback` the cost is mostly evening that works: random inserts into the customer
  index find a sibling with room and rebuild the pair (`buildLeaf` 66% of evening's time).
- In the update the cost is evening that fails: the split sizes a full sibling, which for a packed
  sibling means scanning its rows (`payloadAtMost` 60% of evening's time), and the sibling never
  has `C/8` free.

Evening holds index fill near 84%, which is why it was adopted; neither cost argues for removing
it. The failed attempt could be made cheap without a format change: a packed sibling's saving can
be read from its page's regions (the invariant the lazy leaf already relies on) instead of from its
rows.

## 6. Recommendation

**#4: port the shorter side; do not port the gap.**

- The gap is the larger gain on one shape (cascades and similar runs of nearby deletes, −47%), is
  12% slower on random inserts, and a port touches every site that indexes a dirty leaf in three
  cores.
- The shorter side takes 70% of the cascade regression and 8% off range deletes, is neutral
  elsewhere, and is confined to the edit helpers. Port it to Go as is. For Rust and TS, profile the
  cascade first and port only if shifting is as large a share as in Go (Rust regressed 1.3×, TS
  1.5×).
- What remains of #4 after it is not shifting: the per-leaf work of splitting, merging, evening and
  copying a leaf that is 4.8 times larger (§2). That is the price of density and no edit-path change
  removes it.

**#5: take the exception (b) as format v54; do not adopt slack (a).**

The maintainer confirmed (2026-11-14) that the project is still in development and a format version
is cheap, which removes the reason given earlier for waiting.

- Slack costs 3% to 7% of every key-ordered table and only helps leaves of wide rows. The exception
  costs nothing on the datasets, helps leaves of any width, and fixes the benchmark (0.042 to
  0.011 ms, v51 0.018).
- **Open choice before porting: one exception or a few.** One exception absorbs only the first
  outlier per leaf and column; a second in the same leaf widens the column and splits the leaf, and
  in `TestLeafWidenGrowth` that still grew the tables 20% to 36%. A region with up to `k` exceptions
  would absorb more, at the price of tracking the `k + 1` smallest and largest values in the size
  facts and a choice rule over them. Not prototyped.
- Independently of the format, the split itself can be made cheaper. Split a lazy leaf from its
  page's stored spans instead of decoding every row (decoding is 23% and `buildLeaf` 17% of the
  update), and size a failed evening candidate from its page (§5). Neither is prototyped.
- For scale: without any change, widening one value in every leaf grows the tables 24% to 58%,
  still 1.9 to 3.6 times smaller than in v51.

**How the two interact.**

- Slack does not help #4: it removes 3% to 7% of a leaf's rows and the Phase A benchmarks did not
  move. The premise that slack would also cut the shifting does not hold at any slack small enough
  to be worth its density.
- The edit path and the exception are independent: one changes how a dirty leaf's arrays move, the
  other how a column is coded. Combined runs (gap with slack) add up.
- Evening interacts with slack through its room threshold (§4, option a) and with the exception only
  by not being reached: when the leaf does not split, nothing is evened.

## 7. The prototypes

All off by default; the corpus, goldens and `mise run test` pass with them off.

| switch | file | changes bytes? | what it does |
|---|---|---|---|
| `JED_LEAF_EDIT=short` | `leaf_edit.go` | no | shift the shorter side |
| `JED_LEAF_EDIT=gap` | `leaf_edit.go` | no | movable gap |
| `JED_LEAF_EDIT_STATS=1` | `leaf_edit.go` | no | count the slots edits move (tests) |
| `JED_LEAF_SLACK=d` | `leaf_slack.go` | yes (split points) | slack C/d on right-edge splits |
| `JED_LEAF_FORX=on` | `leaf_forx.go` | yes (region kind 6) | one exception per base-plus-offsets region |
| `JED_LEAF_EVEN=off` | `leaf_even.go` | yes (split points) | no evening, to measure its cost |

Tests: `TestLeafEditModes` (the modes write identical bytes), `TestLeafFORXRoundTrip`,
`TestLeafShapes` and `TestLeafWidenGrowth` (the last two log only, under `JED_LEAF_SHAPES=1`).

**Not verified.** Rust and TS were not measured. The gap prototype is safe only because one writer
owns every dirty node; its close-on-freeze path was exercised by the corpus but not under the race
detector. Kind 6 was not run through the simulation sweep, the Ruby reference or `verify.rb`.
