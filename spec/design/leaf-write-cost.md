# Write costs on dense leaves

**Status: #5 landed as format v54 (§8), one exception per column per leaf; #4 awaits a decision
(§6).** The other prototypes are Go only, every switch off by default, and not in the format. This note measures two write regressions that came with the
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
  prototypes add to the default paths cost nothing measurable. Re-checked after the runs and blocks
  prototypes (§4a, §4c), nine rounds: cascade +1.2%, insert −2.8%, range +2.8% (spread 1.19), churn
  +0.9%, update 0.0%. A first version that kept the two in separate fields made the range struct
  larger and cost the cascade about 3%; they now share one pointer.

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

## 4a. Option (d): runs, each with its own base

Proposed by the maintainer after §4: let a base-plus-offsets column change its base and offset width
partway through the leaf, so an outlier, or a column that drifts with the key, costs only the rows
near it. Prototyped as region kind 7 (`JED_LEAF_RUNS`, `impl/go/leaf_runs.go`):

```
null bitmap | run count R (u16) | [rank index] | R × (start row u16, data offset u16, base w, width u8) | offsets
```

A run's width is one of 0, 1, 2, 4 or 8 bytes below `w`, or `w` itself. The runs are the cheapest
division of the leaf's rows, found by dynamic programming over the rows in order (exact, amortized
constant work per row with sliding-window minima per width; ties to the narrowest last run, then the
earliest start). The region costs `F + Σ (w + 5 + len·width)` and is chosen only when smaller than
every other kind. A leaf's payload stays nondecreasing in its rows, which the split scan needs: the
cheapest division of a set of rows is never dearer than that of a subset.

Two lookup layouts, so a reader can find row `i`'s run without decoding the region (the maintainer's
"additional run data inline so a read could quickly skip"):

- **search** (`F = 2`): binary search over the run starts.
- **rank** (`F = 2 + ⌈N/8⌉ + 2⌈N/64⌉`): a bit per row set where a run starts, plus per 64 rows the number
  of runs started before them; row `i`'s run is that count plus one popcount. 76 bytes at 476 rows.

`TestLeafRunsRoundTrip` passes in both layouts (writes, reopens, updates and deletes rows with
outliers and a drifting column, under the size and patch cross-checks, against the same workload
without runs). The Go corpus passes in memory and disk mode except for page-count pins: costs and
EXPLAIN page estimates fall in eight files (`query/index_key_set`, `query/inl_key_set`,
`query/subquery_point_set` and their EXPLAIN files, `query/statistics_estimates_p9`,
`ddl/foreign_key_keyed_actions`, and `query/composite_key_set` with search), because the corpus
tables shrink. No row differs.

**Size.** Runs pay off on a column that drifts with the key: the cascade's child table (`parent_id =
id mod 10000 + 1`) loads in 42 live pages instead of 53 (−21%), `del_orders` (`v = 7·id`) in 204
instead of 212 (−4%). The bench datasets are uniform random, so only `large` moves (9,346 → 9,290
with search, −0.6%; 9,326 with rank).

**Outliers.** Runs absorb an outlier worse than the exception. Isolating one costs two run headers
(26 bytes for a `bigint`) against the exception's 10, and a leaf loaded in key order has only a few
spare bytes. In `TestLeafWidenGrowth`:

| table | master | exception | runs, search | runs, rank |
|---|---|---|---|---|
| `wide` | 25 → 31 → 31 | 25 → 25 → 30 | 25 → 25 → 30 | 25 → 30 → 30 |
| `d` | 212 → 276 → 293 | 212 → 214 → 274 | 204 → 270 → 270 | 206 → 273 → 273 |
| `c` | 53 → 84 → 96 | 53 → 60 → 72 | 42 → 54 → 54 | 44 → 58 → 58 |

(live pages: loaded, after one widening per leaf, after a second.)

**Write time.** As built, every edit to a leaf measures each such column again from all its rows:
the cheapest division depends on every row, while kind 1 keeps a running minimum and maximum. Medians
of seven alternating rounds (ms):

| benchmark | master | exception | runs, search | runs, rank |
|---|---|---|---|---|
| `fk_cascade_delete_indexed` | 2.734 | 2.736 | 5.741 | 5.636 |
| `insert_rollback` | 3.836 | 3.666 | 12.230 | 11.673 |
| `delete_range_rollback` | 11.156 | 11.235 | 72.596 | 74.177 |
| `delete_insert_churn` | 5.164 | 5.250 | 21.845 | 21.691 |
| `update_untouched_large_rollback` | 0.043 | 0.011 | 0.019 | 0.057 |

Re-measuring is 52% of the range delete's profile. Loading the four bench datasets takes 12.7 s
instead of 6.3 s, because the split scan evaluates every candidate half. Some of this is prototype
overhead (each re-measure allocates afresh), but most of it is inherent to an optimal division. A
cheaper write path would need a rule that an edit can repair locally, and such a rule does not
guarantee the monotone size the split scan relies on.

## 4b. Reading a value in place

When this was measured every core rebuilt a base-plus-offsets column, and an elided key column, as a
full-width copy for the leaf and read from the copy: Go when it parsed the page, Rust and TS on the
first read of that column in that leaf. The copy cost an O(N) loop and kept a resident leaf larger
than its page. Every core now reads in place (leaf-structural.md §12 item 3), so the region kinds are
compared on that basis too.

`BenchmarkLeafColumnRead` (`impl/go/leaf_read_bench_test.go`) reads one `bigint` column of 476 rows
straight from each region's bytes, pinned to one CPU, three runs each (ns; medians). Every reader
decodes a 1-, 2-, 4- or 8-byte integer with one load; an earlier run with a byte-at-a-time decode
favoured the narrow forms and is superseded. "Bytes" is the region's size; a page holds 8,176.

| column / region | bytes | read one row | scan, a lookup per row | scan, run by run |
|---|---|---|---|---|
| drift, kind 1 | 1,022 | 2.3 | 498 | |
| drift, exception | 1,032 | 2.4 | 537 | |
| drift, runs search | 708 | 11.9 | 4,288 | 323 |
| drift, runs rank | 784 | 9.7 | 3,182 | 326 |
| drift, blocks of 64 | 1,074 | 6.2 | 1,758 | |
| drift, blocks of 32 + exception | 703 | 5.5 | 1,668 | |
| outlier, kind 1 | 3,869 | 2.2 | 535 | |
| outlier, exception | 556 | 2.3 | 499 | |
| outlier, runs search | 577 | 7.3 | 2,738 | 201 |
| outlier, blocks of 64 + exception | 636 | 5.6 | 1,680 | |
| outlier, blocks of 32 + exception | 713 | 5.6 | 1,685 | |
| random, kind 1 | 1,974 | 2.3 | 492 | |
| random, blocks of 64 | 2,054 | 5.6 | 1,736 | |
| the full-width copy | (3,808) | 2.0 | 240 | |

Building the copy costs about 1.9 µs per column per leaf from kind 1 or the exception, 2.9 µs from
blocks, and 4.5–6 µs from runs.

- **Kind 1 and the exception read one row in place as fast as the copy** (2.2–2.4 ns against 2.0). A
  scan in place takes about twice as long as scanning the copy (≈500 ns against 240), because each
  value is a base plus an offset of a width known only at run time; a scan loop specialised per
  width would close most of that. Reading in place would remove the copy's 1.9 µs per column per leaf
  and its memory.
- **Blocks read in constant time with no index** but pay an extra header read per value: 5.5–6.2 ns
  a row, about 2.5 times kind 1. A scan that walks block by block, as runs do, would avoid it.
- **Runs cost 3–5 times as much per random read** (7–12 ns): finding the run. The rank index is not
  reliably faster than binary search at these run counts, and costs 76 more bytes a column. A scan
  that walks run by run is the fastest of all (200–430 ns), but every reader must then be run-aware.

## 4c. Option (e): fixed blocks

Suggested after §4a: give each block of `B` rows its own base and width, so row `i`'s block is `i / B`
and a read in place needs no index, and add the exception on top for a single outlier. Prototyped as
region kind 8 (`JED_LEAF_BLOCKS=B`, `JED_LEAF_BLOCKS_EXC=on`, `impl/go/leaf_blocks.go`):

```
null bitmap | exception flag (u8) | [exception row (u16), body (w)] | ⌈N/B⌉ × (base w, width u8, data offset u16) | offsets
```

A block's width is the narrowest of 0, 1, 2, 4 below `w` that holds its present values, or `w`. The
excepted value is the column's single largest (or smallest) present value, taken when it makes the
region smaller. Size `1 + [2 + w] + Σ (w + 3 + len·width)`: a closed formula per block, with no
search. Measured with the plain exception kind (option b) also enabled, so each leaf takes whichever
is smallest.

`TestLeafBlocksRoundTrip` passes for blocks of 16 and 64, with and without the exception. The Go
corpus passes in full in memory and disk mode with blocks of 64 and the exception. With blocks of
32, four page-count pins move (`ddl/foreign_key_keyed_actions`, `query/index_key_set` and its
EXPLAIN file, `query/statistics_estimates_p9`) and no row differs.

**What changes in the specification beyond a closed formula.** A block belongs to its row's position,
so a half produced by a split has its blocks aligned to its own first row. A left half (rows
`[0, m)`) only grows at its end, so its size stays nondecreasing in `m`. A right half grows at its
front, which moves every row into a different block, so its size is **not** monotone in `m`, which the
`m_min` scan of *Fan-out* assumes. The rule would have to be restated as the scan itself (stop at the
first boundary that does not fit). The prototype's two split-point implementations agree under that
reading on every workload run, but it is a weaker property than the one the format states today.

**Size** (live pages, `TestLeafWidenGrowth`):

| table | master | exception | blocks of 64 + exception | blocks of 32 + exception |
|---|---|---|---|---|
| `c` (drifting `parent_id`), loaded | 53 | 53 | 43 (−19%) | 45 (−15%) |
| `c` after one / two widenings per leaf | 84 / 96 | 60 / 72 | 53 / 53 | 56 / 56 |
| `d` (`v = 7·id`), loaded | 212 | 212 | 212 | 204 (−4%) |
| `d` after one / two widenings per leaf | 276 / 293 | 214 / 274 | 218 / 273 | 208 / 271 |
| `wide`, loaded / one / two | 25 / 31 / 31 | 25 / 25 / 30 | 25 / 25 / 30 | 25 / 25 / 30 |

Blocks of 64 match runs on the strongly drifting column (43 against 42 pages) but not on `7·id`,
whose 64-row span still needs 2-byte offsets; blocks of 32 get it. A second outlier in a block widens
that block (64 rows × 2 more bytes for `c`), so after two widenings `c` is smaller than master but
still 25% above its loaded size. The bench datasets are uniform random and never choose blocks: their
live pages are unchanged, and loading them takes the same time as master (6.3–6.4 s, against 12.7 s
for runs).

**Write time** (medians of seven alternating rounds, ms; the exception kind is on in both blocks
columns):

| benchmark | master | exception | blocks of 64 + exception | blocks of 32 + exception |
|---|---|---|---|---|
| `fk_cascade_delete_indexed` | 2.817 | 2.776 | 3.523 | 3.388 |
| `insert_rollback` | 3.762 | 3.626 | 4.897 | 4.831 |
| `delete_range_rollback` | 11.517 | 11.474 | 18.682 | 18.217 |
| `delete_insert_churn` | 5.153 | 5.214 | 7.122 | 7.438 |
| `update_untouched_large_rollback` | 0.044 | 0.011 | 0.013 | 0.012 |

The update is fixed as by the exception. Every insert or delete is 25% to 62% slower, on columns that
end up not using blocks as well: an insert or delete moves every later row to another block, so each
edit measures the column again (a min and max per block, far cheaper than runs' dynamic program, but
over every row). Keeping blocks current incrementally would mean a sliding window per block for each
shift, which the prototype does not attempt.

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

**#5: take the exception (b) as format v54; adopt neither slack (a), runs (d) nor fixed blocks (e) now.**

The maintainer confirmed (2026-11-14) that the project is still in development and a format version
is cheap, which removes the reason given earlier for waiting.

- Slack costs 3% to 7% of every key-ordered table and only helps leaves of wide rows. The exception
  costs nothing on the datasets, helps leaves of any width, and fixes the benchmark (0.042 to
  0.011 ms, v51 0.018).
- **Runs (§4a) lose on what matters most here.** They are the only option that shrinks a drifting
  column (−21% on the child table) and they handle any number of outliers, but they absorb an
  outlier worse than the exception (two run headers against 10 bytes), make every write that touches
  a leaf re-measure it (range delete 6.5×, bulk load 2×, as built), and make a random read in place
  3–5 times as costly (§4b).
- **Fixed blocks (§4c) are the better form of the same idea**: nearly runs' saving on a drifting column
  (−19% on the child table at 64 rows), a constant-time read in place with no index (about 2.5 times
  kind 1's, and a block-by-block scan would close that), no bulk-load cost, and the exception on top
  for outliers. Two costs keep them out of v54: every insert or delete re-measures the column (writes
  25–62% slower as built, on any column eligible for base plus offsets), and a right half's size is no
  longer monotone in the split point, which weakens a guarantee *Fan-out* states today. If drift
  compression is wanted, blocks are the option to develop, starting with incremental size facts.
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

**Separately: read base-plus-offsets columns in place** (§4b). Kind 1 and the exception read one row
in place as fast as from the full-width copy every core builds today; a scan needs a loop specialised
per offset width to match the copy. Dropping the copy saves its O(N) build per column per leaf and
keeps a resident leaf the size of its page. It is the open follow-on "keep a resident leaf
the size of its page" (leaf-structural.md §12), now with a measurement. No format change.

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
| ~~`JED_LEAF_FORX=on`~~ | `leaf_forx.go` | — | one exception per base-plus-offsets region: format v54 since 2026-11-14 (§8), no longer a switch |
| `JED_LEAF_EVEN=off` | `leaf_even.go` | yes (split points) | no evening, to measure its cost |
| `JED_LEAF_RUNS=search\|rank` | `leaf_runs.go` | yes (region kind 7) | base-plus-offsets in runs, with a lookup index |
| `JED_LEAF_BLOCKS=B` | `leaf_blocks.go` | yes (region kind 8) | base-plus-offsets in fixed blocks of `B` rows |
| `JED_LEAF_BLOCKS_EXC=on` | `leaf_blocks.go` | yes | one exception on top of the blocks |

Tests: `TestLeafEditModes` (the modes write identical bytes), `TestLeafFORXRoundTrip`,
`TestLeafRunsRoundTrip`, `TestLeafBlocksRoundTrip`, `TestLeafShapes` and `TestLeafWidenGrowth` (the last two log only, under
`JED_LEAF_SHAPES=1`); `BenchmarkLeafColumnRead` (§4b).

**Not verified.** Rust and TS were not measured. The gap prototype is safe only because one writer
owns every dirty node; its close-on-freeze path was exercised by the corpus but not under the race
detector. Kinds 7 and 8 were not run through the simulation sweep, the Ruby reference or `verify.rb`;
kind 6 now is (§8).
Since 2026-11-14 every core reads kinds 1 and 6 in place (§8; leaf-structural.md §12 item 3); the
other readers of §4b exist only in the benchmark.

## 8. The exception region, format v54 (2026-11-14; superseded by §9.2)

> Kind 6 as described here — one exception — was replaced before v54 was published by up to 16
> exceptions (§9.2). This section is the record of the first landing and its measurements.

**Decision: one exception per column per leaf.** §6 left the choice between one exception and up to
`k`. One is what was prototyped and measured: it takes the update benchmark from 0.042 to 0.011 ms
and costs nothing where no column has a lone outlier, and its size is a closed formula over the
column's two smallest and two largest values, so the split scan's monotonicity argument carries over
(format.md *Fan-out*). Up to `k` would need the `k + 1` smallest and largest values in every leaf's
size facts and a rule choosing which of them to except, and was neither prototyped nor read in place;
the brief that asked for this landing required that it be measured before it is chosen, so it stays an
open alternative. In `TestLeafWidenGrowth` a second outlier in the same leaf still widens the column
(the tables grow 20–36% after two widenings, §4).

**What landed.** Region kind 6 (format.md *Leaf node*) in the Go, Rust and TypeScript cores and the Ruby
reference (`verify.rb`, which also decodes it for `--check-image`), with no switch: a column eligible
for base plus offsets takes kind 6 when it saves more than kind 1, kind 1 on a tie. Every core reads it
in place on top of the in-place kind 1 reader (leaf-structural.md §12 item 3): a value is the
exception's stored body when its row is the exception row, else the base plus its offset. The engines
read values one at a time, so there is no separate scan loop to patch; the row test is one compare.
Every golden and integrity fixture is regenerated for the version byte. The generated datasets have no
lone outliers, so their files are unchanged but for the version. `multiway_split.jed` had to change: a
single distant value no longer forces a three-way split, since kind 6 absorbs it, so table `a` now
carries one distant value per column on either side of each edited row (format.md fixture table), and
its leaves hold kind 6 regions.

**Measured** (`update_untouched_large_rollback`, alternating runs pinned to one CPU against `a83368a75`,
medians of p50): Go 0.039 → 0.010 ms, Rust 0.035 → 0.0085 ms, TypeScript 0.130 → 0.042 ms (spread
1.5). The prototype's 0.042 → 0.011 ms in Go is reproduced. Verified with `mise run ci` (new goldens,
`sim:regress`, `sim:sweep 1..8`) and a further `sim:sweep 1..24` over every profile (528 traces).

## 9. Patched base plus offsets (PFOR), 2026-11-14: investigated, then landed as v54 (§9.2)

**Question.** Is one exception (§8) the right rule, or should a column keep any number of outlying
values whole (PFOR, patched frame of reference), either sized exactly or under a fixed cap? Measured on
column distributions shaped like application data rather than the synthetic lone outlier.

**Prototype** (Go only, `impl/go/leaf_pfor.go`, `JED_LEAF_PFOR=K` or `exact`; off by default, nothing
on disk changes). Region kind 9: null bitmap, base, offset width `bw`, a count `E`, `E` ascending u16
exception rows, their `E` whole bodies, then `N` offsets. The encoder takes the layout of least size
over `bw ∈ {0, 1, 2, 4}` and every present value as the base; a value outside the base's window is an
exception (offset zero). It is chosen only when it saves more than kind 1 (and kind 6), so a column
with nothing to gain stays kind 1 or plain: the fallback is the existing per-region size choice, not a
separate rule. The base is a present value and the window starts at it, so the excepted values are
always some of the smallest and some of the largest. Under a cap `K` the size facts are therefore the
`K + 1` smallest and `K + 1` largest values, O(K) per added row; uncapped they are every value, sorted.
Adding a row never lowers any layout's size, so the split scan's monotonicity holds. A reader finds an
exception by binary search of the row list. Checked with `JED_LEAF_LAZY=check` (every size fact
compared with a fresh measurement) over the leaf, split, update and reclamation tests, and
`TestLeafPFORRoundTrip`.

**Experiment** (`impl/go/zz_pfor_dist_test.go`, `JED_PFOR_DIST=1`): 200,000-row tables loaded in key
order (leaves 99% full) and in shuffled order (leaves ~81% full, as random-key tables settle), then a
sequence of UPDATE stages, under v53 (kind 1 only), v54 (kind 6), PFOR capped at 4 and 16, PFOR exact,
and the §4c blocks with and without an exception.

- `orders` (`id bigint` key): `created_at` ascending every ~2 s with 1.5 s jitter and 0.3% backfilled
  rows hours to days early; `updated_at` = `created_at` + 0–30 s; `customer_id` Zipf over 500k ids
  scattered by hash; `amount_cents` log-normal (median 2500); `status` 0–3 skewed with 0.1% `99`;
  `discount_cents` 0 for 85% of rows, else 100–5000; `valid_until` `9999-12-31` for 97%, else a real
  date; a short note. Stages: `updated_at` touched for 1 row in 200, another 1 in 200, 10% (spread over
  an hour), the newest 5% (with `status`), `status` for 8%, `amount_cents` ×100 for 0.5%,
  `valid_until` set for 2%.
- `events`: `ts` ascending (exponential gaps), `device_id` uniform over 5000, `reading` normal around
  20000 with 0.5% glitches near 10⁹, a per-device `seq`. Stages: glitches fixed, 0.1% zeroed.

**Region bytes as a share of the plain region, key-order load** (shuffled agrees to a point):

| Column | v53 | v54 | PFOR ≤ 4 | PFOR ≤ 16 | PFOR exact | blocks 64 + exc |
|---|---|---|---|---|---|---|
| `created_at` (ascending, backfill) | 72% | 57% | 52% | 52% | 52% | 55% |
| `updated_at` after the 10% touch | 95% | 95% | 95% | 92% | 62% | 95% |
| `customer_id` (wide) | 51% | 51% | 51% | 51% | 51% | 51% |
| `amount_cents` (skewed) | 33% | 28% | 27% | 27% | 27% | 28% |
| `amount_cents` after the fix | 44% | 31% | 28% | 28% | 28% | 30% |
| `discount_cents` (85% zero) | 52% | 52% | 52% | 52% | 26% | 52% |
| `valid_until` (sentinel) | 100% | 98% | 77% | 8% | 8% | 78% |
| `reading` (0.5% glitches) | 50% | 47% | 32% | 27% | 27% | 34% |
| `ts`, `device_id`, `seq`, `status` | unchanged | | | | | |

**Leaves** (key-order load → after every stage): `orders` v53 1032 → 1373, v54 958 → 1287, PFOR ≤ 4
914 → 1249, PFOR ≤ 16 842 → 1140, exact 816 → 1096, blocks + exc 927 → 1319; `events` 332, 328, 297,
286, 286, 299. PFOR ≤ 16 makes `orders` 18% smaller than v53 and 12% smaller than v54.

**Splits under updates.** Loaded in key order, leaves are 99% full, and the first touch of 1 row in 200
splits about 30% of leaves under every encoding: an exception costs `w + 2` bytes, which a full leaf
does not have. The single-row touch rolled back costs ~105 µs under every encoding. Shuffled-loaded
leaves have slack: the first touch added 40 pages under v53 and v54 (v54 had spent its exception on a
backfilled row in a third of the leaves), none under PFOR ≤ 4 or ≤ 16, and 2 under exact; the second
added 37 under v54, 4 under PFOR ≤ 4 and none under ≤ 16. The rolled-back touch: v53 80 µs, v54 55,
PFOR ≤ 4 33, ≤ 16 38. So an exception avoids a split only where a leaf has room, and the common
append-loaded table has none; the §6 slack option is the independent lever there.

**Costs.** Key-order load time, `orders`: v53 1.71 s, v54 1.78, PFOR ≤ 4 1.93 (+13%), ≤ 16 2.37 (+39%),
exact 5.83 (3.4×); `events` (~650 rows a leaf) 0.80, 0.79, 0.97, 1.18, 10.6 (13×). Splits dominate: the
prototype clones each prefix's and suffix's facts in the split scan, O(N·K) capped and O(N²) exact, and
a split of a full `events` leaf under exact took 3.4 ms. Capped, that can be made incremental; exact
cannot cheaply, since every removal re-measures the column and every prefix needs its own optimum. A
full scan of the measured columns is 4–14% slower than v53 for capped PFOR (`orders` 16–17 ms against
17.5–19.6) and 40–85% slower for exact, whose `discount_cents` and `updated_at` regions hold 20–36 exceptions a leaf and
read each value through the binary search.

**Findings.**

- For the shapes in the question, an exception mechanism pays not where data is clumpy but where it is
  clumpy *with strays*: a roughly ascending timestamp with occasional backfill (half of v53's leaves
  had to store `created_at` plain), a skewed amount's tail, sensor glitches, and above all a sentinel
  with real values mixed in (`valid_until`: 100% → 8%). Widely distributed columns gain nothing from
  any variant; a column whose values are clustered in two places (`updated_at` after many touches)
  needs exceptions by the dozen.
- One exception (v54) captures little of this: it handles a leaf's first stray and nothing after.
  `K = 4` captures most of `created_at`, `amount_cents` and `reading`; `K = 16` also captures the
  sentinel column and is within about 3% of exact on leaf count (`orders` 842 against 816) except where a column needs dozens of
  exceptions a leaf (`discount_cents`, touched `updated_at`), which only exact took.
- Exact is the elegant rule but carries a write cost the capped rule does not; capped at 16 is one rule
  with O(K) size facts and the same monotonicity argument as kind 6, and kind 6 is its `E = 1` case.

**Not decided.** Replacing kind 6 with capped PFOR would be a format change in every core, the Ruby
reference and `verify.rb`. Its open questions are taken up in §9.1.

### 9.1 K = 8 against K = 16 (2026-11-14)

**Prototype changes.** The capped facts are now cheap. The least exception count for each offset width
and the lowest base giving it depend only on the kept extremes, not on the row count, so they are
cached and a saving query costs four products. A value outside both kept sets, which is nearly every
value, is rejected by two compares in an inlined test. The kept values live inside the accumulator
(one allocation). The split scan keeps one running accumulator each way instead of cloning facts per
prefix. Reads test a 64-bit bucket mask of exception rows, built at parse from the row list, before
searching. `JED_LEAF_LAZY=check` passes over the leaf, split, update, delete, reclamation, paging and
merge tests at K = 8 and K = 16. The `sentinels` table (`id` and six dates, `9999-12-31` except for
1, 2, 4, 6, 8 and 12% real dates) joins the experiment.

**Size** (key-order load, share of plain; `→` after the updates):

| Column | v54 | K = 4 | K = 8 | K = 16 |
|---|---|---|---|---|
| `orders.valid_until` (3% real) | 98 → 100% | 77 → 94% | 25 → 52% | 8 → 12% |
| `sentinels.s1` / `s2` / `s4` / `s6` | 82 / 99 / 100 / 100% | 25 / 84 / 100 / 100% | 7 / 32 / 97 / 100% | 5 / 7 / 63 / 98% |
| `events.reading` (0.5% glitches) | 47% | 32% | 27% | 27% |
| `created_at`, `amount_cents` | 57%, 28% | 52%, 27% | 52%, 27% | 52%, 27% |

Leaves: `orders` 958 (v54), 914, 863, 842; `sentinels` 641, 574, 503, 442; `events` 328, 297, 286,
286. K = 8 and K = 16 differ only on columns with between about 8 and 16 strays a leaf. Since a leaf
holds 200–700 rows, K = 16 covers a stray rate of roughly 2.5–7%, and K = 8 half that; compression
raises the rows a leaf holds and with them the strays it sees.

**Time** (pinned to one CPU; key order medians of three runs, shuffled one run):

| | v53 | v54 | K = 8 | K = 16 |
|---|---|---|---|---|
| Load, key order (`orders` / `sentinels` / `events`) | 2.12 / 1.75 / 0.96 s | 2.02 / 1.78 / 0.99 | 2.02 / 1.83 / 1.02 | 2.03 / 1.84 / 1.06 |
| Load, shuffled | 14.9 / 5.9 / 4.9 s | 15.5 / 6.2 / 5.3 | 19.4 / 12.2 / 7.2 | 16.3 / 12.7 / 7.1 |
| Touch + rollback, full leaf (`orders` / `events`) | 107 / 160 µs | 114 / 166 | 102 / 213 | 111 / 246 |
| Touch + rollback, leaf with room (`orders` / `events`) | 80 / 211 µs | 57 / 45 | 33 / 39 | 30 / 34 |
| Full scan of the measured columns | within ±4% of v53 at every K | | | |

The 23-benchmark set (alternating three rounds pinned to one CPU, medians of p50) is within its
run-to-run spread for both K against v54: the largest deviations, `index_range` +9% and
`update_untouched_large_rollback` +6.5% under K = 16, sit inside spreads of 1.16 and 1.12. Its
generated datasets come out byte-identical in all three modes (no column there gains from PFOR), so
it measures only the reader's mask test and the write path's facts.

**The cap is a cliff.** A column at its K-th exception falls back to kind 1 or plain on the next stray:
one insert adds up to N × w bytes (about 1.5 KB for a 375-row date column). The leaf overflows, evens
out with a sibling, and soon does again. Shuffled inserts into `sentinels`, whose columns sweep the
stray rate through the cap, evened leaves out 6,512 times at K = 16 against v53's 840 in a 60,000-row load,
which is the 2× shuffled load time above. v53 has the same cliff at the first stray and v54 at the
second; the cap moves it to the (K+1)-th, so more columns sit near one. The remaining cost of
splitting a full leaf (`events` +34% at K = 8, +54% at K = 16, against v53) is the running facts of
every prefix and suffix. Uncapped PFOR has no cliff (each further exception costs `w + 2`) but, as §9
measured, not cheap facts.

**Reading of the evidence.** K = 16 is never larger than K = 8, gains substantially only on
sentinel-like columns with 2–7% strays, and costs a little more on full-leaf splits; neither moves any
benchmark. Random-order inserts into columns near the cap cost up to 2× in load time under either.

### 9.2 Landed: kind 6 with up to 16 exceptions, format v54 (2026-11-14)

**Decision: K = 16** (maintainer, on §9.1). Kind 6 is redefined in place, since v54 was not
published: `base | bw | E (u8, 1–16) | E rows (u16, ascending) | E bodies | N offsets`, the layout
of least size `(E + 1)·(w + 2) + N·bw` over widths narrower than kind 1's and bases among the present
values, the narrower width and then the lower base on a tie, taken when it saves more than kind 1
(format.md *Leaf node*). Its size is a function of the column's 17 smallest and 17 largest values, and
the split scan's monotonicity argument is restated for it (format.md *Leaf split*). The one-exception
code is gone from every core.

**What landed.** The Ruby reference (`verify.rb`, encoder and decoder) and the Go, Rust and TypeScript
cores. Each core keeps a column's kind 6 size facts as its 17 smallest and 17 largest values with the
per-width least exception count cached, rejects a value outside them with two compares, feeds the
split scan from one running accumulator each way, and reads in place behind a 64-bit bucket mask of
exception rows. `multiway_split.jed` had to be rebuilt: forcing a three-way split now needs 17 distant
values in a half, so each edited leaf holds 16 in `v` before the edited row and 16 in `x` after it,
and 32 stored values beside enough rows need 512-byte pages; table `b` was re-tuned for them
(format.md fixture table). Every golden is regenerated by the reference and matched byte for byte by each core.


**Verified and measured.** `mise run ci` passes (new goldens, `conformance:all --checked`, `sim:regress`,
`sim:sweep 1..8`, the process and e2e suites). Against the one-exception v54 build, alternating three
rounds pinned to one CPU over the 23-benchmark set (medians of p50; the bench datasets are byte-identical
under both, since none of their columns has outliers worth storing whole): Rust is within ±4% on every
benchmark; Go is within each benchmark's run-to-run spread but leans slower on a few
(`int_array_unnest_scan` +9% at spread 1.16, `index_range_nonselective` +6% at 1.15,
`update_untouched_large_rollback` +6% at 1.11, `insert_rollback` +6% at 1.19). The update benchmark keeps
the v54 gain: 0.0099 ms in Go and 0.0081 ms in Rust, against 0.039 and 0.035 ms before v54. TypeScript
was not measured against the one-exception build, whose TS core was not kept.
