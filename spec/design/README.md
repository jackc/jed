# spec/design/ — subsystem design docs (the "why")

Prose rationale for each subsystem. A design doc plus the relevant conformance corpus is
what an agent needs to work a subsystem without holding the whole engine in context
(CLAUDE.md §10).

Each doc explains *why* a decision was made and points at the **data** that encodes it
(the TOML tables, the fixtures). The data is authoritative; these docs are the reasoning.

## Contents

- [types.md](types.md) — the type system: scalar set, comparison/coercion/promotion,
  three-valued NULL logic, integer overflow, and order-preserving key encoding.
- [grammar.md](grammar.md) — the SQL grammar: W3C-style EBNF notation, keywords as
  non-reserved identifiers, the deliberate narrowings, and how the grammar grows.
- [functions.md](functions.md) — the function/operator catalog: the family-based operand
  contract, the truth-value result types, NULL propagation vs detection, and how it grows.
- [codegen.md](codegen.md) — the codegen "middle path": what is generated (data-shaped
  descriptor tables) vs hand-written (parser/executor/evaluator), the drift gate, and the
  per-core cross-check.
- [conformance.md](conformance.md) — the conformance corpus: sqllogictest-style format,
  structured-error matching, the tier + capability-flag system, and determinism rules.
- [determinism.md](determinism.md) — the determinism contract decomposed into four
  guarantees, the taxonomy of sanctioned relaxations (underspecified order, boundary inputs,
  approximate/float, identity, plan/parallelism), the clock/entropy seams, the
  no-contamination invariant, and the exception ledger (CLAUDE.md §2/§8/§10/§13).
- [encoding.md](encoding.md) — order-preserving key encoding: the `int-be-signflip` rule,
  the nullable presence tag (NULLs-last, the PostgreSQL model), composition, and descending
  order.
- [collation.md](collation.md) — linguistic collation (design only): a jed-owned UCA executor
  + compiler with **no tables vendored in the binary** — collations are a first-class **portable
  artifact** (extract from the host / compile / save / open / import-export), **baked into the
  database file** by default (so a collated index can never drift/corrupt across machines/versions)
  with a name+hash **reference mode** opt-out, and an optional **provenance description**; the
  per-database default collation, sort-key key encoding, deterministic-vs-nondeterministic
  collations, and the slice plan.
- [compatibility.md](compatibility.md) — compatibility & versioning (**UNRATIFIED PROPOSAL**, not a
  spec — forward-looking):
  the legible graded-open verdict (full / reduced-read-only / refused-with-a-reason), the in-file
  requirements manifest, read-vs-write dependency tagging, the heap-scan read-degradation, and the
  unification of collation skew + function drift; the model views / functional indexes / generated
  columns / host functions register into (CLAUDE.md §13).
- [timezones.md](timezones.md) — time zones for `timestamptz` (design decided, not yet built):
  the **host-loaded `JTZ` bundle** (manifest + per-zone **RFC 8536 TZif** sections + alias links,
  byte format in [../tz/README.md](../tz/README.md)), the privileged `db.LoadTimeZoneData` seam, the
  per-core TZif reader, and the single `AT TIME ZONE` consumer — copying collation's host-load model.
  `timestamptz` is UTC, so plain indexes are tz-immune: **no format change, no skew verdict** until a
  tz-derived key can be stored (latent into compatibility.md).
- [storage.md](storage.md) — the storage seam: block interface, page model, and the
  root-pointer-swap commit model (CLAUDE.md §3/§9).
- [hosts.md](hosts.md) — the formal storage-host (`BlockStore`) interface: the five-method
  byte device every host implements, the host catalog (in-memory / file / OPFS), and where
  the encryption codec and replication tee sit relative to the seam (CLAUDE.md §9).
- [locking.md](locking.md) — file locking & multi-process access: the exclusive-by-default
  whole-file lock at `open`/`create`/`attach` (`55006`, `lock_timeout_ms`, the per-host
  mechanism table — the decided immediate implementation), and the recorded shared-mode +
  lease-refinement follow-on (CLAUDE.md §9).
- [replication.md](replication.md) — replication by block-shipping the per-commit page-delta
  (no WAL — copy-on-write already gives atomicity + concurrency): the change record, the
  replica apply recipe, keyless replicas, PITR at commit granularity, and the write-amplification
  trade (CLAUDE.md §9).
- [encryption.md](encryption.md) — encryption at rest (a deferred door): a page codec above
  the seam, a standardized AEAD with a deterministic `(page_index, txid)` nonce that keeps §8
  byte-identity, the auth tag closing the CRC tamper gap, and the §14 crypto-dependency gate.
- [api.md](api.md) — the host/embedding API: open/create/commit/close a database file,
  prepare/execute/query, the `Rows` cursor, `$N` bind parameters, and the structured-error
  surface — the same shape across cores (CLAUDE.md §1/§2).
- [cost.md](cost.md) — the deterministic cost-accounting seam: the unit schedule as data,
  the cross-core accrual rules, the counter representation, and the deferred ceiling/abort
  (CLAUDE.md §13).
- [cores.md](cores.md) — what counts as a core vs. a wrapper, when to add the next core,
  and which languages add new divergence (the selection rule behind CLAUDE.md §2).
- [extensibility.md](extensibility.md) — host extensibility (Phase 9): the PostgreSQL-style
  catalog type system, host-defined functions, composite types, and core scalar types — and
  why the system/extension line moves to *who owns cross-core determinism* rather than
  vanishing (CLAUDE.md §2/§8/§10/§13).
- [decimal.md](decimal.md) — the exact `decimal`/`numeric` type: representation, PG result-scale
  rules, half-away-from-zero rounding, e-notation literals, and the `decimal_work` cost unit.
- [float.md](float.md) — the binary floats `f32`/`f64`: the PG total order, the trapping
  arithmetic kernel, the order-independent canonical-fold SUM/AVG, and the `R` render tag.
- [timestamp.md](timestamp.md) — `timestamp`/`timestamptz`: the i64-microsecond instant model,
  literal parsing, infinity sentinels, and the (no-time-zone-db) scope.
- [interval.md](interval.md) — the `interval` span (months/days/micros): the input subset, PG
  render, the canonical 128-bit comparison, and interval/timestamp arithmetic.
- [composite.md](composite.md) — composite (row) types (`CREATE TYPE … AS (…)`): the first
  user-defined type, the open `Type { Scalar | Composite }`, the recursive value codec + null
  bitmap, element-wise 3VL comparison, the all-fields `IS NULL` rule, and `record_in`/`record_out`.
- [array.md](array.md) — array types (`T[]`): the second container axis, the **structural**
  `Array(Box<Type>)`, shape-as-a-value-property (PG-faithful), the compact null-bitmap value codec
  (no per-element prefix), btree-NULL element comparison (not composite 3VL), and `array_in`/`array_out`.
- [constraints.md](constraints.md) — column/table constraints: `NOT NULL`, `DEFAULT` (constant +
  expression), `CHECK`, and `UNIQUE` (a UNIQUE constraint *is* its backing unique index).
- [indexes.md](indexes.md) — secondary indexes: the catalog reshape (pk ordinal list + per-table
  index lists), index B-trees, the unique flag, and the planner's first-column pushdown.
- [aggregates.md](aggregates.md) — `COUNT`/`SUM`/`MIN`/`MAX`/`AVG`, `GROUP BY`, and `HAVING`: PG
  widening, the grouping-error rule, NULL handling, and determinism.
- [window.md](window.md) — window functions (`OVER`): the post-aggregation window stage, partition/
  order/peer determinism, ranking/offset/aggregate-window/frame functions, `42P20`, and the
  six-slice ladder (S0 `row_number` → ranking → offset → aggregate-windows → frames → named windows).
- [pager.md](pager.md) — the per-core buffer pool / demand paging (P6.4): a bounded page cache
  with eviction above the block seam, the `cache_bytes` budget, and logical-cost invisibility.
- [spill.md](spill.md) — streaming + spill-to-disk operators: the `ORDER BY` external merge sort
  bounded by `work_mem` (the hash aggregate / DISTINCT / hash join are follow-ons).
- [streaming.md](streaming.md) — the true streaming result cursor (design): making `Rows` a pull
  source — non-blocking pipeline streams lazily, blocking operators buffer-then-stream — with
  PG-faithful snapshot pinning, the cost-invariant-under-full-drain contract, and the VDBE-forward
  pull scan cursor (the lazy page/large-value decode it builds on already landed).
- [large-values.md](large-values.md) — out-of-line overflow chains + transparent LZ4 compression
  for over-`RECORD_MAX` values, and the `value_compress`/`value_decompress` cost units.
- [transactions.md](transactions.md) — the single-writer / immutable-snapshot model and the SQL
  surface (`BEGIN`/`COMMIT`/`ROLLBACK`, `READ ONLY`/`READ WRITE`, failed-block poisoning).
- [entropy.md](entropy.md) — the host-injected random + clock seam behind `uuidv4`/`uuidv7` and
  `now()`/`current_timestamp`/`clock_timestamp()`, kept deterministic-given-the-seam.
- [cli.md](cli.md) — the `jed` CLI host program: the TUI + script runner, output formats, CSV
  import, `--dump`, and `--readonly` (a host of the engine, not a core).
- [benchmarks.md](benchmarks.md) — the wall-clock benchmark harness comparing the three cores
  against PostgreSQL and SQLite (deliberately outside `rake ci`, answers still cross-checked).
- [mutation-testing.md](mutation-testing.md) — mutation-testing the Go core (`rake mutation`,
  `impl/go/cmd/mutate`): inject deliberate bugs and check the conformance corpus catches each;
  a surviving mutant is untested logic, located to a line (an analysis tool, outside `rake ci`).
