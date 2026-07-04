# Attached (linked) databases — design

> One query, several databases. A host **attaches** additional jed databases — other single-file
> databases on disk, or pure in-memory ones — to an open `Database` handle under a name, and SQL can
> then reference their tables with a **database-qualified name** (`reports.sales`), including joins
> across them. This is the SQLite `ATTACH` capability (CLAUDE.md §9's "one query can reference data in
> multiple files"), reshaped to jed's seams and its untrusted-safety guarantee.
>
> **The load-bearing decision: attaching is a *host-API* act, never a SQL statement.** Opening a file
> reaches the filesystem, and jed's SQL surface is **pure** (CLAUDE.md §13) — there is no
> `pg_read_file`, no `COPY … FROM`, and there will be no SQL `ATTACH 'path'`. So the *host* attaches
> and detaches (like `db.open`/`db.load_unicode_data`, [api.md](api.md), [hosts.md](hosts.md) §4); the
> **SQL** surface gains only the power to *name* what the host already attached. Qualified names are
> pure — they can reference attached data but cannot attach anything or reach the filesystem — so the
> §13 guarantee is preserved unchanged.
>
> **The seam already exists in embryo.** jed's transaction already carries **several** committed
> databases at once — `main`, session-local `temp`, and database-wide shared temp are three parallel
> snapshots committed together ([transactions.md](transactions.md) §2, [temp-tables.md](temp-tables.md)
> §2/§5). Attachment **generalizes "a few hardcoded extra snapshots" into "N named attachments,"** and
> in doing so lets temp stop being bespoke machinery and become *an attached in-memory database* (§6).
>
> **Status: Slice 0 + Slice 1a + Slice 1b + Slice 1b-3 + Slice 2 landed (all three cores); Slice 1c
> (reframe temp) resolved at the resolution-funnel level — see §6/§13. Slice 2 (host-API *file* attach +
> cross-file read + single-durable write) is the first with real cross-file durability: a file attachment
> commits durably through its own pager, and a tx may write at most one file-backed database (0A000
> otherwise) until multi-file atomic write (Slice 3) lands.**
> This doc fixed the model and the decisions before any code, spec-first (CLAUDE.md §11). The two
> decisions that were open at first draft are settled (maintainer, 2026-07-03): **attach is host-API
> only — no SQL attach in any form** (§4), and the **`SHARED TEMP` surface is retired** in favor of a
> `Database`-scoped in-memory attachment (§6). **Slice 0** (retire `SHARED`, §13) has **landed** (all
> three cores). **Slice 1** — attached in-memory databases + qualified names + reframe temp — is
> building in sub-slices: **1a** the `qualified_table` grammar + parser + resolution against the
> implicit `main`/`temp` scope (the SQL surface, no registry yet) — **landed, all three cores**; **1b**
> the name→attachment registry + host-API in-memory `db.attach`/`detach` + the CREATE TABLE / CREATE
> INDEX qualifier (create-into-attachment) + N-root commit + cross-attachment joins + read-only mode
> (`25006`) + detach-in-use (`55006`) — the whole capability, **landed, all three cores**; **1b-3**
> pulls attachments inside the concurrency differential net — the `# format: concurrency` runner learns
> the file-level `# attach:` directive so a schedule exercises concurrent readers/writer over a
> host-attached in-memory database (`suites/concurrency/attach_snapshot_isolation.test`), asserting
> cross-database snapshot isolation + the watermark (the threaded cores drive it under the race
> detector — [concurrency-testing.md](concurrency-testing.md) §4/§9), **landed, all three cores**; **1c**
> reframes session-local temp as an implicit in-memory attachment — **resolved** at the resolution-funnel
> level (1b already routes `temp`/`main`/attachments through one scoped-routing path), with temp's
> **session-scoped** home kept as a deliberate divergence (§6). The grammar ([../grammar/grammar.ebnf](../grammar/grammar.ebnf)) and error registry
> ([../errors/registry.toml](../errors/registry.toml)) are authoritative for the surface and codes;
> when a decision here changes, change them in the same edit.

---

## 1. Why this, why now, and what it is not

PostgreSQL has **no** embedded multi-file model — cross-database access there is a client/server
affair (`postgres_fdw`, `dblink`), outside jed's deployment model. So this is a **SQLite-lineage**
feature, and CLAUDE.md §1 governs the split cleanly: the *feature shape* (attach a file, qualify a
name, join across files) comes from SQLite — jed's **deployment-model north star** — because PG offers
no analog and we **own our surface**; but every *semantic* question the feature raises (name-resolution
ambiguity, NULL/3VL across a cross-file join, ordering, errors) still defaults to **PG behavior**,
because those are the same semantics jed already implements and a join does not change them.

Three motivations, in priority order:

1. **A real capability: cross-file queries.** Join a read-only *reference* database against a working
   one; run an ETL read across two files; keep a large immutable "library" database attached beside a
   small mutable one. This is the feature users come for, and the reason to build the mechanism at all
   — you do **not** build attachment merely to tidy up temp (§6); that tidy-up is a *free consequence*.
2. **Unifying temp.** Once "a database that lives in RAM and is attached with a scope" exists,
   session-local temp *is* an implicitly-attached session-scoped in-memory database, and the awkward
   "shared temp" concept collapses into "a `Database`-scoped in-memory attachment" (§6). One mechanism
   replaces two special cases.
3. **Spill-to-disk falls out.** A temp/attached in-memory database's `BlockStore` swaps memory→file
   ([hosts.md](hosts.md) §2); temp spill (temp-tables.md slice 3) stops being bespoke.

**What it is *not*:** not a distributed database, not two-phase commit across machines, not a virtual/
external row source (each attachment is a **first-class self-describing jed database**, not a CSV or a
network table — the CLAUDE.md §9 "no external/virtual row sources" line is untouched), and not a
schema system (jed still has no schemas *within* a database; the qualifier is a **database** name — §3).

**A deliberate CLAUDE.md amendment this implies.** "One database = one file" (§9) stays true **per
database**; what changes is that a *single query* may span several attached databases. That is an
architectural decision to make in the open, not a violation — recorded in §12 and to be reflected in
CLAUDE.md §9 when the first slice lands.

## 2. The model — a database is a `(BlockStore, Pager, catalog, snapshot-line)` quad

Everything jed needs to *be* a database is already bundled per-database and host-agnostic above the
storage seam:

- a **`BlockStore`** (the byte backing — file or memory, [hosts.md](hosts.md) §2),
- a **`Pager`** over it (buffer pool, page math — [pager.md](pager.md)),
- a **catalog** (tables/indexes/types/sequences), and
- a **committed `Snapshot`** plus, during a write, a **`working`** snapshot
  ([transactions.md](transactions.md) §2).

An **attachment** is exactly one such quad, held under a name on the `Database` handle. The handle
already holds several: `main` is the file quad; session-local `temp` and shared temp are in-memory
quads carried as parallel snapshots in the transaction (temp-tables.md §2). This doc's core move is to
replace "a fixed set of hardcoded extra snapshots" with **a name→quad registry**, and to generalize
the machinery that already commits `main` + shared-temp **two roots atomically** (temp-tables.md §5)
into an **N-root** commit (§5).

```
Database handle = { attachments: Map<name → Attachment>,   # main + temp + shared + any host-attached
                    write_lock, live_snapshots, synchronous, … }   # unchanged (transactions.md §2)
Attachment      = { name, kind: file | memory, mode: read_only | read_write,
                    blockstore, pager, committed: ref<Snapshot>, durable: bool }
Transaction     = { per-attachment working/read snapshots, base_txids, … }
```

Page ids are **per-`Pager`**, so each attachment is its own page space — a page number only means
something relative to its attachment. Snapshots, txids, free-lists, and the reader-liveness watermark
are likewise **per-attachment** (§5). Nothing about one attachment's on-disk bytes depends on another.

**Implicit vs. explicit attachments.** `main` and the temp databases are **implicit** — always present
in a handle/session, always in unqualified name scope (§3). Everything a host attaches is **explicit**
— reached by qualifier only. This is what lets attachment be purely *additive*: existing single-file
name resolution is unchanged, and no attached table can silently shadow a `main` table.

## 3. Naming & resolution — the qualifier is a *database*, not a schema

jed today has a **single flat namespace, no schemas, no `search_path`** (temp-tables.md §3); a table
reference is a bare `identifier` (`table_ref ::= identifier ("AS"? identifier)?`) and the only dotted
name is a **column** reference `rel.col` (`column_ref ::= identifier ("." identifier)?`, grammar.md
§15). Attachment introduces jed's **first multi-part name in table position**, and the grammar slot is
free because `table_ref` is currently unqualified:

- **Table position** (FROM/JOIN targets, DML targets, `CREATE INDEX ON`, `REFERENCES`) grows an
  optional database qualifier: `qualified_table ::= (identifier ".")? identifier`. `reports.sales`
  names table `sales` in the attachment `reports`; a bare `sales` resolves in **implicit** scope
  (§2). An unknown qualifier is **`42P01`** (`undefined_table`, PG's own, reused with the message
  *"database \"reports\" is not attached"* — see §11 on whether a dedicated code is warranted).
- **Column position is unchanged — `rel.col` stays 2-part**, where `rel` is the relation's **label**
  (its alias, or its table name when unaliased — grammar.md §15). To reference a column of an attached
  table, you use the relation's label exactly as today: `FROM reports.sales s` then `s.amount`, or
  unaliased `FROM reports.sales` then `sales.amount`. The **database qualifier never appears in column
  position** in v1 (no 3-part `reports.sales.amount`) — the same rule by which the qualifier never
  appears in a column's output name (grammar.md §15). 3-part column refs are a deferred ergonomic
  follow-on (§14), not a semantic gap.
- **No implicit search into explicit attachments — no silent shadowing, and no search path, ever.** An
  unqualified name resolves **only** in implicit scope (`main` + temp), under the existing
  **preclude-overlap** rule (temp-tables.md §3, `42P07`). Two attached databases may each contain a
  `users` — that is *expected* and is **not** an error, because you always reach an explicit attachment
  by qualifier, so the two never compete. This is a deliberate divergence from SQLite, which searches
  attachments in attach order and lets an earlier attachment **silently shadow** a later one (§12,
  D-ATTACH-2). jed's rule — *qualify to reach an attachment; unqualified means the local database* — is
  more explicit, determinism-friendly (no attach-order dependence in name resolution), and consistent
  with jed's standing "preclude overlaps, no silent shadowing" posture. jed has **no `search_path`** for
  attachments and **none is planned** — a query reaches an attached database by qualifier, full stop.
  This is a firm non-goal, not a deferral: adding an ordered search list would reintroduce exactly the
  order-dependent, silently-shadowing resolution this rule exists to avoid.
- **Cross-database self-collision** is the ordinary duplicate-label case: `FROM main.t JOIN reports.t`
  gives **two relations both labeled `t`** → **`42712`** (`duplicate_alias`, grammar.md §15), resolved
  by aliasing one (`FROM main.t a JOIN reports.t b`). No new rule.

## 4. Attach / detach is a host-API act — the safety spine

This is the decision that keeps §13 intact, so it gets its own section.

Opening a database **file** reaches the filesystem. jed's SQL surface is **pure and side-effect-free
by construction** (CLAUDE.md §13): it has no function or statement that touches the filesystem, and
the sanctioned host-supplied inputs (storage bytes, unicode data, entropy/clock) all enter through
**host-API seams, never SQL** (hosts.md §4, session.md §5.3). Attaching a file is exactly such an act,
so it lands the same way:

- **`db.attach(name, source, mode)`** / **`db.detach(name)`** are **host-API** methods on the
  `Database` handle ([api.md](api.md) is authoritative for their signatures). `source` is either an
  already-open file `BlockStore` (the host does the `open`, mapping `58P01`/`58P02` as for `main`,
  hosts.md §4) or a request for a fresh **in-memory** attachment. These sit **outside** the SQL
  capability envelope entirely (session.md §5.3) — an untrusted, `{SELECT}`-only session **cannot**
  attach or detach anything, for the same reason it cannot `load_unicode_data` or open a file.
- **A database may be attached read-only** (`mode`, per attachment). A **read-only** attachment rejects
  every write to its objects — DML and DDL alike — **deterministically, before any I/O**, with
  **`25006`** (the read-only-context family, §11); it is the per-attachment generalization of the
  read-only *handle* (a read-only `main`, api.md) and the read-only *transaction* (transactions.md
  §4.3). This is the natural mode for an attached **reference** database: attach it read-only and it
  can be joined but never mutated, whatever the session's privileges. The host should additionally open
  the file `BlockStore` `O_RDONLY` (defense in depth), but the engine's deterministic rejection is the
  contract, not the host's file mode. A read-only attachment also **never competes for the
  one-durable-writer slot** (§5), so any number may be attached alongside a writable database. The host
  picks the mode explicitly at attach time; there is no implied default in this doc (api.md fixes it).
- **The SQL surface gains only qualified *names* (§3)** — the pure power to reference an attachment the
  host chose to expose. Naming data cannot reach the filesystem, so it adds **no** new capability to an
  untrusted query.
- **There is deliberately no SQL `ATTACH DATABASE 'path'`.** A SQLite-style SQL attach would be a
  `COPY … FROM`-class hole in the pure-SQL guarantee, and jed does not add one. A host that wants
  *user-driven* attach (e.g. a REPL `\attach` command) implements it **above** the engine and calls
  `db.attach` itself — that host owns the filesystem-reach decision (the host-extension boundary,
  CLAUDE.md §13, [extensibility.md](extensibility.md) §2).

> **Decided (§4, 2026-07-03): no SQL-level attach in any form — file *or* memory.** A gated,
> in-`:memory:`-only SQL attach would not breach §13 (a fresh in-memory attachment reaches no
> filesystem), but the pure-host-API model needs none — `CREATE TEMP TABLE` already creates into an
> implicit in-memory attachment (§6). Keeping attach *entirely* out of SQL keeps the surface uniform
> and the one filesystem-reach seam in one place. Revisit only if a concrete host need appears.

## 5. Transactions & the commit boundary — N roots, one durable writer

The existing model generalizes with one genuinely new constraint.

- **Snapshot isolation across all attachments.** A transaction pins a committed `Snapshot` **per
  attachment it reads**, and a write transaction builds a `working` snapshot **per attachment it
  writes** — the exact generalization of temp's "a reader pins both roots together" (temp-tables.md
  §5) from two roots to N. Read-your-writes holds within the transaction for every attachment; a
  reader's view is consistent across all of them.
- **Single writer, unchanged.** One write lock per `Database` handle (transactions.md §10) still
  serializes all writers; there are still no write-write conflicts and no retry loop. Cross-attachment
  writes in one transaction are serialized like any other write.
- **The one new rule — at most one *durable* (file-backed) database written per transaction.** Making
  **N file** databases commit **atomically** across a crash is the genuinely hard problem (a
  super-journal / two-phase protocol with crash-recovery — SQLite's territory), and it is **deferred**
  (§14). Until it lands, a single transaction may **write at most one file-backed database** (`main`
  *or* one attached file), plus **any number of in-memory attachments** (temp and memory attachments),
  and may **read** any number of file databases. This covers the two headline uses with **zero** new
  durability hazard:
  - *cross-file query*: read a reference file DB, write the working file DB — one durable writer. ✓
  - *temp unification*: write `main` (or nothing durable) plus in-memory temp — one durable writer. ✓
  In-memory attachments carry **no** crash-atomicity obligation (their commit is a **pointer swap**,
  crash-free, and their data is gone on crash anyway — there is no cross-file inconsistency to create),
  so they are always joinable. A transaction that attempts to write a **second** file-backed database
  is **`0A000`** (`feature_not_supported`, message *"a transaction may modify at most one durable
  database"*) — the honest, forward-compatible narrowing (§11). The slot counts only **writable**
  file databases: a **read-only** attachment (§4) can never be written, so it never occupies the slot —
  attach as many read-only reference files as you like beside the one writable database.
- **Commit publishes N roots.** Commit fsyncs the one durable writer's dirty pages per `synchronous`
  (transactions.md §9), then swaps **every** touched attachment's committed root (file root(s) + each
  in-memory root) — the two-root swap of temp-tables.md §5 widened to N. Because ≤1 root is durable,
  there is no multi-file crash window. Rollback discards every attachment's working root.
- **Per-attachment watermark.** The reader-liveness watermark (transactions.md §8) is **per
  attachment** — a reader pins a version in each attachment it touches, and each attachment's page
  reclamation (file: the P6.2 free-list; memory/temp: the within-session compaction the temp-blockstore
  slice landed, temp-tables.md §6) gates on **its own** oldest live version. This is a clean
  generalization of what temp already does.

## 6. Temp, reframed — the point-1 resolution

Under this model, temp is **not a separate subsystem** — it is attachment applied to in-memory
databases with a scope:

| today | reframed as |
|---|---|
| session-local temp | an **implicit**, **session-scoped**, in-memory attachment (created lazily on first `CREATE TEMP TABLE`) |
| shared temp | a **`Database`-scoped**, in-memory attachment shared by the handle's sessions |
| `main` | the file (or in-memory-DB) attachment, always present |

Session-local temp already rides a per-domain `MemoryBlockStore` + pinned pager with within-session
compaction (the temp-blockstore slice, temp-tables.md §6) — i.e. it is **already an in-memory
attachment in all but name**. Folding it under this doc is largely an *internal* refactor (the temp
snapshots become named attachments; the `54P03` page-budget, the compaction watermark, and the
zero-file-writes invariant carry over unchanged), with the one *surface* addition that temp tables
become referenceable by the reserved qualifier `temp.` if desired (unqualified still works, since temp
is in implicit scope, §3).

> **Resolved (§6, Slice 1c, 2026-07-03): the reframe is realized at the *resolution-funnel* level, and
> temp keeps its session-scoped home.** In implementation, "temp is an in-memory attachment" is true
> where it *observably matters* — **name resolution**. Slice 1b's scoped funnels (`snapForScope`,
> `lkpTableScoped`/`lkpStoreScoped`/`writeStoreScoped` and their index analogues) route `main`, `temp`,
> and every host attachment through **one** scoped-routing path, so `temp` is already a citizen of the
> same mechanism an attachment is: a `temp.` qualifier resolves through the same funnel, and the temp
> table it names lives in the same `Snapshot`/storage-quad shape (§2) as an attachment. What is
> **deliberately not** unified is temp's *lifecycle*, because it genuinely differs from a host
> attachment and folding it away would add complexity, not remove it: temp is **session-scoped** (a
> session-private domain, invisible to other sessions), so its committed root lives on the session (no
> cross-session `roots` publish — the atomic swap an attachment needs to become visible handle-wide has
> nothing to publish *to* for a private domain), and its reclamation watermark is the session-private
> open-cursor count, not the `Database`-wide reader-liveness registry an attachment gates on. Physically
> relocating temp's fields into the host-attachment registry (which is `Database`-scoped) would therefore
> be **wrong** (it would re-share temp across sessions — exactly what Slice 0 removed); a session-local
> mirror-registry would be lateral field movement whose commit path still needs a session-vs-database
> branch to pick the right watermark. So 1c is **resolved, not deferred**: the unification that pays off
> (one resolution mechanism, `temp` reachable by qualifier) is done; temp's distinct session-scoped
> lifecycle is the **correct divergence** from a `Database`-scoped attachment, recorded here rather than
> engineered away. This preserves temp's zero-file-writes, `54P03` page budget, and within-session
> compaction unchanged (temp-tables.md §6).

**This is where point 1 (shared temp) resolves — the `SHARED` surface is retired.** "Shared temp"
stops being a coined feature with its own `SHARED` keyword; a host that wants cross-session in-RAM
scratch **attaches a `Database`-scoped in-memory database** instead. The reframing also names *why* the
`SHARED` surface was uncomfortable: its sharing boundary is an **in-process object** (`Database`
lifetime), which would visibly clash with any future **multi-process** file access, where the *file* is
the sharing boundary — two processes on the same file would not see each other's "shared" temp. An
attachment makes the scope **explicit at attach time** and decoupled from the file axis, which is the
honest model. (Multi-process file access is now spec'd in [locking.md](locking.md): a file attachment
takes the same default-exclusive per-file lock as `main` at attach time; shared multi-process access
is the recorded follow-on there.)

> **Decided (§6, 2026-07-03): retire the `SHARED TEMP` surface.** The `SHARED` keyword,
> `allow_shared_temp_ddl`, `shared_temp_mem`, the `Database`-level shared-temp snapshot, and the
> shared-temp corpus/concurrency tests are **removed** — cheap and reversible (gated off by default,
> never persisted, 0.x preview). Its capability (cross-session in-RAM scratch shared by a handle's
> sessions) is re-provided, if needed, by a `Database`-scoped in-memory **attachment**. Removing it
> also simplifies the commit path — the two-root commit (main + shared-temp) collapses back toward
> **one durable root + the session-local temp attachment** until this doc's N-root generalization (§5)
> lands. The removal is **Slice 0** (§13) and **precedes** the attachment build, so the bespoke
> shared-temp `MemoryBlockStore` flip (temp-tables.md §14) is **never built** — it would have been sunk
> cost against a retired surface. This is the direct payoff of doing the attach design first.

## 7. Self-describing files — the linkage is never persisted

A jed file must be reopenable by **any** core with **no external code and no external row sources**
(CLAUDE.md §9/§13, the self-describing guarantee, `XX002`). Therefore:

- **No attachment linkage is ever written into any file.** A jed file **never** records "I depend on
  file X." Attachments are a **runtime** property of an open handle; the **host re-establishes** them
  each session by calling `db.attach` (exactly as it re-supplies unicode data and the entropy seam).
  A file opened standalone is complete and self-describing; opened with attachments, it is unchanged on
  disk.
- **Consequence — attachments cannot be referenced by persisted objects.** A view definition, a
  `CHECK`, a `DEFAULT`, a generated column, an FK — anything the catalog *persists* — must **not**
  reference an attached database by name, or the file would stop being self-describing. Cross-database
  references are therefore confined to **ephemeral** statements (a `SELECT`/DML issued at runtime), not
  persisted DDL (§8). This falls out of the self-describing rule and needs no separate enforcement
  beyond rejecting a qualified name in a persisted-object definition (`0A000`).

## 8. Cross-database restrictions (v1)

Kept deliberately narrow, and each consistent with an existing jed deferral:

- **No cross-database foreign keys.** An FK's parent and child live in the **same** database. SQLite
  forbids cross-database FKs; jed already defers FK-on-temp (temp-tables.md §8); the persisted-linkage
  rule (§7) forbids it structurally anyway. → `0A000`.
- **No cross-database catalog references.** A table in database A may not use a **composite type** or
  **sequence** defined in database B; types/sequences resolve within the table's own database. (Same
  self-describing reason as §7.)
- **DDL targets a single database.** `CREATE TABLE reports.t` creates in `reports` (subject to the
  one-durable-writer rule, §5, and `allow_ddl`, session.md §5.3, which still governs — DDL on an
  attachment is DDL). A persisted definition inside that DDL may not name a *different* database (§7).
- **Detach-while-referenced.** `db.detach(name)` fails (`55006`-style *object_in_use*, §11) while any
  live transaction/cursor pins a snapshot of that attachment — the watermark (§5) already tracks this.

## 9. Untrusted-query safety (§13) — unchanged, by construction

The whole design is arranged so the §13 guarantee needs **no new exception**:

- **The SQL surface stays pure.** Qualified names reference already-attached data; they cannot attach,
  detach, or reach the filesystem (§4). The built-in catalog gains no impure function.
- **The dangerous act is host-API + privileged.** File attach lives with `open`/`load_unicode_data`,
  outside the SQL capability envelope; an untrusted session can never trigger it (§4).
- **The cost meter spans attachments uniformly.** A page read is a page read whichever attachment's
  pager served it; `page_read` and the per-statement/lifetime cost ceilings (cost.md, session.md §5)
  bound a cross-file query exactly as a single-file one — no new metering seam.
- **Budgets compose, they don't merge.** A **file** attachment is bounded by its own file (no RAM
  budget). An **in-memory** attachment is bounded by the temp/memory page budget (`54P03`,
  temp-tables.md §7). A host handing an untrusted session a memory scratch DB gets the same deterministic
  ceiling temp already provides.

## 10. Determinism & the cross-core contract

- **File attachments are fully in the §8 byte contract.** An attached file is an ordinary jed database
  in the **same on-disk format** — the cross-core round-trip (a file written by one core is byte-readable
  by another) and the goldens apply to it with **no** change and **no** `format_version` bump (the format
  is untouched; attachment is a runtime relationship, §7).
- **In-memory attachments stay out of the byte contract** (as temp is today, temp-tables.md §10) — never
  serialized, so no physical-layout parity obligation; only their **logical** observables (rows, multiset,
  `ORDER BY`, types, errors, **cost**, `54P03`) are contractual, and those are invariant to core.
- **Qualified-name resolution is deterministic** and identical across cores (name resolution is pure).
- **Cross-file query results are deterministic** — each attachment contributes a **pinned** snapshot
  (§5), so a join across files is a pure function of the pinned states, order-insensitive without
  `ORDER BY` and fully ordered with it (CLAUDE.md §8), byte-identical across Rust/Go/TS.

## 11. Errors

| code | name | raised when | notes |
|---|---|---|---|
| `42P01` | `undefined_table` | a qualified name whose **database** is not attached, or whose **table** is absent | reused (grammar.md §15). **§11 open point:** a *dedicated* `unknown_database` code may read better than overloading `42P01`; decide when the grammar slice lands. |
| `42712` | `duplicate_alias` | two relations resolve to the same label across databases (`main.t`+`reports.t`, unaliased) | reused, no new rule (§3) |
| `42P07` | `duplicate_table` | an overlap **within** one database's implicit namespace | unchanged (temp-tables.md §3) |
| `25006` | `read_only_sql_transaction` | any write (DML or DDL) targeting a **read-only** attachment (§4) | reused — the read-only-context family (also a read-only txn/handle, transactions.md §4.3); attachment-scoped message |
| `0A000` | `feature_not_supported` | writing a **second** durable (file-backed) database in one txn (§5, Slice 2); a cross-database FK/type/sequence ref (§8); an attachment name in a **persisted** definition (§7); on an **attachment table** (1b): FOREIGN KEY, EXCLUDE, a composite-typed / serial / IDENTITY column, `COLLATE`, a non-btree (GIN/GiST) index, `ON CONFLICT` | the honest v1 narrowings |
| `55006` | `object_in_use` | `detach` of an attachment with a live pinned snapshot (§8) | **landed** (registry.toml); PG's `object_in_use` is class 55 |
| `42710` | `duplicate_object` | `db.attach` of a **reserved** name (`main`/`temp`) or an **already-attached** name | host-API; reused |
| `42704` | `undefined_object` | `db.detach` of a name that is **not attached** (`main`/`temp` are not detachable) | host-API; reused |
| `42601` | `syntax_error` | `CREATE TEMP TABLE db.t` — `TEMP` with an explicit database (unless the database *is* `temp`) | 1b; the qualifier and TEMP are contradictory |
| `58P01`/`58P02`/`58030`/`XX001`/`XX002` | host/file errors | attaching a **file** surfaces the same host-layer codes as opening `main` (hosts.md §4) | raised in the host program layer, above the engine |

Host-API attach/detach and their capability posture are a **host-API surface**, asserted by per-core
unit tests (open/attach/detach/close, the one-durable-writer rejection, the read-only-attachment write
rejection, detach-in-use) — out of corpus reach (CLAUDE.md §10). The **SQL** surface (qualified names, cross-file joins, resolution errors) is
corpus-tested across all three cores; cross-**file** durability and reopen behavior need the disk
storage mode (conformance-two-storage-modes) or a per-core host test.

## 12. Recorded divergences

From **SQLite** (whose feature shape we borrow):

- **D-ATTACH-1 — no SQL `ATTACH`; host-API only.** SQLite's `ATTACH DATABASE 'file' AS x` is SQL; jed
  keeps it host-API to preserve pure SQL (§4, CLAUDE.md §13). Deliberate, load-bearing.
- **D-ATTACH-2 — no implicit search into attachments; qualify to reach one.** SQLite searches
  attachments in attach order and lets an earlier one silently shadow a later one; jed reaches an
  explicit attachment **only** by qualifier, so there is no shadow and no attach-order dependence in
  name resolution (§3). More explicit, determinism-friendly.
- **D-ATTACH-3 — one durable writer per transaction (v1).** SQLite (pre-WAL, and with care) can commit
  writes to several attached files atomically via a super-journal; jed defers multi-file atomic write
  and allows ≤1 durable writer per txn until then (§5, §14). A temporary narrowing, not a permanent one.

From **PostgreSQL** (the semantic default): PG has **no** embedded multi-file attach at all, so there is
nothing to diverge *from* on feature shape — this is a "we own our surface" case (CLAUDE.md §1, §12).
Every *semantic* the feature touches still tracks PG.

**CLAUDE.md §9 amendment** (made — §9 first bullet): "one database = one file" holds **per database**;
a single *query* may reference several attached databases, and a transaction writes at most one durable
(file-backed) database until Slice 3. Not a relaxation of the self-describing or single-file guarantees
(§7, §10) — a scope clarification.

## 13. Slicing (the build plan — TODO.md will track it)

Sequenced to put the durability-risky part last:

- **Slice 0 — retire the `SHARED TEMP` surface (§6, subtractive, independent of attachment).** Remove
  the `SHARED` keyword from the grammar, `allow_shared_temp_ddl` and `shared_temp_mem` from the session
  surface, the `Database`-level shared-temp snapshot/catalog/budget from all three cores, and the
  shared-temp corpus + concurrency tests. The two-root commit collapses to one durable root + the
  session-local temp domain. Purely subtractive (the surface is gated off by default and never
  persisted, so no `format_version` change, no golden regen); it *simplifies* the ground the attachment
  slices build on. Can land first, on its own.
- **Slice 1 — attached *in-memory* databases + qualified names + reframe temp (no new durability
  risk).** Build the name→attachment registry, `qualified_table` grammar + resolution (§3), and the
  N-root in-memory commit (§5, a generalization of the existing two-root commit — in-memory commits are
  pointer swaps, so **no** multi-file crash problem arises). Reimplement session-local temp on top as
  an implicit in-memory attachment (§6), carrying over the temp-blockstore page budget and compaction
  unchanged. **Point 1 (shared temp) is decided here** per the §6 open decision. Deliverable: SQL can
  join across in-memory attachments; temp is no longer bespoke. All three cores, corpus-tested; the
  in-memory path keeps temp's zero-file-writes and `54P03`. Built in three sub-slices:
  - **1a — `qualified_table` grammar + parser + resolution against the implicit scope.** The
    SQL-surface half, landing *before* the registry so the fiddly cross-core grammar/parser work is
    proven green on its own. `qualified_table ::= (identifier ".")? identifier` is threaded into
    `table_ref` (FROM/JOIN) and the DML targets (`INSERT INTO` / `UPDATE` / `DELETE FROM`); the parser
    grows the one-`.`-lookahead qualifier (mirroring `column_ref`'s only dotted-name precedent) and the
    AST carries it. Only the **implicit** qualifiers `main` (the file database) and `temp` (the
    session-local domain) resolve; any other qualifier is `42P01` "database … is not attached". Because
    jed **precludes overlaps** (a name is temp XOR persistent within a session, §3), a `main.`/`temp.`
    qualifier resolves to the *same* store the bare name would — so 1a's resolution is a **validation
    gate** (assert the named relation is in the claimed implicit scope, else `42P01`), not a routing
    change; the store lookup is untouched. Corpus-tested (qualified reads/joins/DML over `main`/`temp`,
    unknown-database `42P01`); no new error code, no `format_version` change. `CREATE INDEX ON` /
    `REFERENCES` / `CREATE TABLE` qualifiers stay bare this sub-slice (they matter once real
    attachments exist — 1b).
  - **1b — the registry + host-API in-memory attach + N-root commit + the DDL qualifier. LANDED (all
    three cores).** A per-attachment `(storage, published-root)` struct keyed by name
    lives on the shared core; `db.attach(name, memory(), read_only)` / `db.detach(name)` add/remove
    entries (host-API, never SQL, §4); the resolution funnels branch on the resolved attachment (1a's
    validation gate becomes real N-way routing — the scoped `lkp*Store`/`writeStore` funnels + the FROM
    catalog resolution now route by qualifier); the **CREATE TABLE / CREATE INDEX qualifier** creates
    *into* an attachment's working snapshot (the sub-slice that lets SQL populate one); the two-root
    commit widens to N roots published in one atomic `roots{committed, attached}` swap (§5 — in-memory
    attachments persist pointer-swap, no fsync); read-only mode rejects every write (DML + DDL) `25006`
    *before any I/O* (§4); `detach` of a pinned attachment is `55006` (§8, the one new code). An
    attachment table this slice supports plain scalar/array/range columns with PK / NOT NULL / DEFAULT /
    CHECK / UNIQUE / secondary btree indexes; **deferred narrowings, each a clean `0A000`**: FOREIGN
    KEY, EXCLUDE, a composite-typed column, a serial / IDENTITY column, `COLLATE`, a non-btree (GIN /
    GiST) index, and `ON CONFLICT` (no cross-scope catalog / sequence / index-store threading yet). An
    attachment relation **full-scans** (index-bound pushdown into an attachment is a perf follow-on — the
    bounded-scan exec path resolves index stores unscoped). Cross-attachment joins are corpus-tested (a
    new `# attach: <name>` harness directive attaches a fresh empty read-write in-memory db; `# skip:
    disk` — in-memory attachments cannot survive the per-record reopen); the read-only + detach lifecycle
    are per-core host-API unit tests. In-memory attachments carry **no** byte contract (never serialized)
    → no `format_version` change.
  - **1b-3 — pull attachments inside the concurrency differential net (concurrency-testing.md §4/§9).**
    The `# format: concurrency` schedule runner learns the file-level `# attach: <name>` directive
    (attaching a fresh empty read-write in-memory database to the shared handle before the schedule
    runs, host-API — never a schedule step); a new shared schedule
    `suites/concurrency/attach_snapshot_isolation.test` asserts, over an attachment, the same
    cross-database snapshot isolation + watermark + version-advance-on-commit the single-handle 1b path
    was only correct-by-construction for. The threaded cores (Go/Rust) run it under the race detector
    (`rake concurrency:race`) — a reader pinning `roots.attached` on its own goroutine/thread while a
    writer publishes a fresh attached map — the hardening proof; TS runs it stepped-sequentially.
    Additive tests + a harness extension, no engine change, no `format_version` change.
  - **1c — reframe session-local temp as an implicit in-memory attachment (§6). RESOLVED at the
    resolution-funnel level, all three cores.** The substantive reframe — one name-resolution mechanism
    for `main`/`temp`/attachments — **already landed in 1b**: the scoped funnels route all three through
    one path, so `temp` is a citizen of the same routing an attachment is (a `temp.` qualifier resolves
    through it; a temp table lives in the same `Snapshot`/storage-quad shape). What is **deliberately not**
    done is physically relocating temp's fields into the (`Database`-scoped) host-attachment registry:
    temp is **session-scoped** with a session-private reclamation watermark and no cross-session `roots`
    publish, so relocating it would be either wrong (re-sharing temp across sessions — what Slice 0
    removed) or lateral movement whose commit path still branches session-vs-database. Temp's distinct
    session-scoped lifecycle is the **correct divergence** from a `Database`-scoped attachment (§6),
    recorded rather than engineered away; behavior-neutral (temp keeps its zero-file-writes, `54P03`
    page budget, and within-session compaction).
- **Slice 2 — host-API file attach (read-only + read-write) + cross-file *read* + single-database
  *write*. LANDED (all three cores).** `db.attach(name, AttachFile(path), read_only)` opens an existing
  single-file jed database via the same `open` path as `main` (its committed snapshot + storage identity
  become the attachment, its own page size honored — each attachment is its own page space, §2); its
  pages fault through its own pager, so cross-file reads/joins "just work" through the Slice-1b scoped
  funnels. A dirtied file attachment commits **durably** at the N-root commit through a factored-out
  durable-commit path (dirty pages + alternating meta slot + fsync + the residency flip — the same recipe
  as the `main` persist), *before* the roots publish; an in-memory attachment still pointer-swaps. The
  per-attachment **read-only mode** rejects every write (DML + DDL) with `25006`. The **one-durable-writer
  rule** (§5) is enforced at commit — a tx that dirtied more than one *file-backed* database (main or an
  attached file) is `0A000` before any durable page is written, so it commits nothing (in-memory
  attachments + temp are free). Detach/close release the OS file handle. The host-API surface + the
  file-specific behaviors (read-only cross-file join + `25006`, read-write durability across a standalone
  reopen, one-durable-writer `0A000`, page-size independence, missing-file `58P01`) are **per-core host
  tests** (out of corpus reach — cross-file durability/reopen needs the disk storage mode); the in-memory
  SQL routing stays corpus-tested (`suites/attach/in_memory.test`). No `format_version` change (a file
  attachment is an ordinary jed file in the unchanged on-disk format; in-memory attachments are never
  serialized). Deliverable: SQLite-style cross-file queries.
- **Slice 3 — multi-file atomic write (deferred, hard).** A super-journal / two-phase commit + recovery
  to lift the one-durable-writer restriction (§5). Only if a concrete use case demands it.

## 14. Deferred / follow-ons (none foreclosed)

- **Multi-file atomic write** (slice 3) — the two-phase protocol lifting the one-durable-writer rule.
- **SQL-level attach** — deliberately *excluded* in v1 (§4), not merely deferred; revisit only for an
  in-`:memory:`-only, capability-gated form if a host need appears.
- **3-part column references** `db.table.col` (§3) — an ergonomic sugar, not a semantic gap.

(Deliberately **not** a follow-on: an implicit **search path** into attachments — §3 makes it a firm
non-goal, not a deferral.)
- **Cross-database FKs / shared catalog types** (§8) — gated on the self-describing question (§7); a
  genuine design problem, not a quick lift.
- **`temp` spill-to-disk as a `BlockStore` swap** (temp-tables.md slice 3) — now a special case of a
  memory attachment swapping its backing to a file.
