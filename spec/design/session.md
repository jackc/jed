# Sessions — the configured host context

> The **session**: the explicit, stateful, capability-bearing context a host runs statements
> through. This doc un-fuses two jobs the first host API fused into `Database` — **storage
> identity** (the file / committed state) and the **configured handle** (cost ceilings, the
> entropy/clock seam, read-only, …) — and makes the second a first-class concept that also
> carries the *new* host controls — a GRANT/REVOKE-style per-table (and per-function) privilege
> model, a per-session lifetime cost budget, session variables, a session time zone, and an
> `execute_script` convenience — over an explicit transaction state machine. It also specs a
> **library-level** multi-statement splitter (`split_statements`) that depends on neither `Session`
> nor `Database` and feeds the single-statement path. [api.md](api.md) owns the bare
> prepare/execute/row-cursor surface and the idiomatic
> per-core mapping; this doc owns the **session** above it. When a decision here changes, update
> [CLAUDE.md](../../CLAUDE.md) §3/§13, [api.md](api.md) §1/§8/§10, and [cost.md](cost.md) §6 in
> the same edit.

## 1. What this refines, and the accident it corrects

The first host API ([api.md §1](api.md)) made **`Database`** do two unrelated jobs at once:

1. **Storage identity** — the committed in-memory state plus the persistence identity (`path`,
   the monotonic `txid`, the `page_size` the file is serialized with, the buffer pool).
2. **The configured handle** — `max_cost`, `work_mem`, `read_only`, the entropy/clock sources
   ([api.md §8/§10](api.md)): per-caller policy that has nothing to do with *which file* is open.

This is the same fusion [transactions.md §1](transactions.md) already diagnosed and un-fused
once (transaction boundary vs. durability). The fix is identical in spirit: **separate storage
identity from the session context.**

- **`Database`** keeps the storage identity — `path`, `txid`, `page_size`, the committed
  `Snapshot`, the buffer pool, `synchronous` (a property of *this file's* durability), and the
  **physical read-only-file** flag ([api.md §2.1](api.md), OS-enforced, `25006` — distinct from
  session *authorization*, §5.1).
- **`Session`** is the configured, stateful context opened *over* a `Database`. It carries the
  capability envelope, the cost meters, the entropy/clock seam, session variables, and the time
  zone; statements run on a session (autocommit) or on a `Transaction` opened from one.

Session-scoped state already exists implicitly — `currval`/`lastval` raise `55000` "before
`nextval` … in this session" ([sequences.md §6](sequences.md)). The session is simply the
explicit home that state always lacked.

**Why "session," not "connection."** Every new control here is, in PostgreSQL, *session
state*: the time zone is the `TimeZone` GUC, session variables are user GUCs (`SET` /
`current_setting`), read-only is `default_transaction_read_only`, the cost budgets are
`statement_timeout`-style session limits. Since jed's north star is PostgreSQL's behavior
(CLAUDE.md §1), naming this `Session` lets the SQL surface be PG-faithful (`SET` / `SHOW` /
`current_setting()` / `SET TIME ZONE`) rather than inventing parallel vocabulary. jed is
embedded with no wire connection to establish, so "connection" would be less honest.

**Back-compat is total.** A bare `Database` owns **one long-lived default session** (§2.1) with
default settings, so `Database::new()`, `open`/`create`, the free `execute(db, sql)`, and every
conformance harness keep working unchanged ([api.md §2.4](api.md)). The explicit `db.session(opts)`
is additive — reached only when a host wants a second, independently-configured envelope. (There is
no interface-stability constraint yet — nothing depends on this surface — so the refinement takes
the clean shape, not a compatible one.)

## 2. The shape and lifecycle

```
Database ─ committed storage (Snapshot, txid), shared by every session
  │ owns one                                db.session(opts) ─▶ additional Session(s)
  ▼
default Session ─ stateful: txn status · vars · time zone · cost meters · currval
  │   db.execute / db.query / db.execute_script   (run on it; split_statements is library-level)
  └─  begin / view / update  +  SQL BEGIN / COMMIT / ROLLBACK  ─▶  Idle ⇄ Open ⇄ Failed
```

### 2.1 The default session, and where state lives

`Database` owns **exactly one long-lived default `Session`.** The bare convenience methods —
`db.execute`, `db.query`, `db.begin`/`view`/`update`, `db.execute_script` (§4) — are **defined as**
operating on that default session, so it is **stateful**: an open `BEGIN` block, session variables,
the time zone, and the cost meters all persist across calls on it, exactly like a PostgreSQL /
SQLite connection. `db.session(opts) -> Session` mints **additional, independent** sessions with
their own state and envelope (`opts` is the `SessionOptions` of §3; an absent field takes its
default).

State ownership splits cleanly — the load-bearing rule:

- **Committed data state** — the `Snapshot` and `txid` — lives on **`Database`**, shared by every
  session and surviving all of them. An autocommit statement on *any* session publishes here, so a
  `CREATE TABLE` on one session is visible to the next ([api.md §6](api.md) committed-snapshot reads).
- **Session state** — the transaction status (§2.2), session variables, time zone, and the
  per-statement/lifetime cost meters — lives on **`Session`**, private to it, gone when it closes.
  Session *settings* do **not** roll back with a transaction (PG `SET SESSION`; `SET LOCAL` is a
  deferred exception, §10). (Sequence `currval`/counter semantics are [sequences.md](sequences.md)'s
  — the counter is *transactional* and rolls back, a documented PG divergence; the "have I called
  `nextval` this session" state that raises `55000` is session-local.)

Because the default session is single-threaded and stateful, a plain `Database` is **not** safe to
share across threads (unchanged from [api.md §2.5](api.md)); true-concurrent sessions are the
`SharedDb` convergence (§2.3, §10).

### 2.2 The transaction state machine

A session carries an explicit **transaction status**, mirroring PostgreSQL's three connection states:

- **`Idle`** — autocommit; each statement is its own transaction.
- **`Open{writable}`** — inside an explicit transaction.
- **`Failed`** — open but poisoned: only `ROLLBACK` / `COMMIT` (the latter acting as rollback) are
  accepted, every other statement is `25P02` ([transactions.md §6](transactions.md), unchanged).

There is **one** state machine with **three entry points** — this is what removes the
`BEGIN`-vs-`session.begin` overlap:

| entry point | role |
|---|---|
| SQL `BEGIN` / `COMMIT` / `ROLLBACK` | the open-ended / interactive spelling; `Idle ⇄ Open` |
| `session.begin(writable)` / `commit()` / `rollback()` | the **same** transitions, from the host API |
| `session.view(fn)` / `session.update(fn)` | **scoped sugar**: require `Idle`, open → run `fn` → commit/rollback → `Idle`; panic-safe |

`session.begin` and SQL `BEGIN` are two spellings of one transition; the closures are the bounded,
guaranteed-to-close wrapper. Consequently the separate **`Transaction` object** the first
[api.md §6](api.md) draft carried **collapses**: statements run *on the session*, and a
`Transaction` is at most optional per-core RAII sugar (Rust rollback-on-drop) driving the session —
never an independent state holder ([api.md §2.2/§6](api.md) are revised to match when S1 lands, §10).

**Can a session sit in a transaction indefinitely?**

- Via `view` / `update`: **no** — the transaction's lifetime *is* the closure; it cannot outlive `fn`.
- Via `BEGIN` (SQL or `session.begin`): **yes, open-ended** — until `COMMIT` / `ROLLBACK` / `close`.
  Intended for interactive use (the CLI/REPL), but a **writable** open transaction holds the
  single-writer lock ([transactions.md §10](transactions.md)) and **starves every other writer**
  while held. The guard is the Bucket-A **`idle_in_transaction_timeout`** setting (§3; PG's
  `idle_in_transaction_session_timeout`): an open transaction idle past the bound is auto-rolled-back
  and the session returned to `Idle` (`25P03`). Default off (unbounded), like the cost ceilings;
  enforcement is a deferred slice (§11).

**Autocommit and the implicit script transaction are the same mechanism.** A single statement on an
`Idle` session wraps that one statement in a transaction; `execute_script` (§4) on an `Idle` session
wraps the *sequence*. The multi-statement path is **not** a new transaction concept — it is
autocommit generalized to a statement sequence. A statement (or script) run while the session is
already `Open` simply joins that transaction.

### 2.3 Close, and the shared handle

`session.close()` releases the session and **rolls back any open transaction it owns** (mirroring
[api.md §2.3](api.md)); the underlying `Database` and other sessions over it are unaffected.
Idempotent. Closing the default session closes the `Database`.

The `SharedDb` / `ReadHandle` / `WriteHandle` concurrency surface ([api.md §2.5](api.md)) and the
session are **convergent**: a read-only session is the configured form of a `ReadHandle`, a writable
session of the `WriteHandle`. This refinement specifies the settings/state layer; folding it into
the per-caller handles is future work (§10). What binds now: settings that affect results (§6) are
deterministic and cross-core whichever handle ultimately carries them.

## 3. The two buckets — what a session carries

`SessionOptions` splits cleanly into two kinds of setting, and the distinction is **load-bearing
because they have different conformance contracts**:

| setting | bucket | default | affects | corpus |
|---|---|---|---|---|
| `default_privileges` | A — envelope | `{SELECT,INSERT,UPDATE,DELETE}` | per-table DML ops permitted on *every* table (`42501`) | `# default_privileges:` |
| `grant` / `revoke` | A — envelope | empty | per-table & per-function privilege deltas (`42501`) | `# grant:` / `# revoke:` |
| `allow_ddl` | A — envelope | on | whether DDL (schema changes) is permitted (`42501`) | `# allow_ddl:` |
| `max_cost` | A — envelope | `0` (unlimited) | per-statement abort (`54P01`) | `# max_cost:` (existing) |
| `max_sql_length` | A — envelope | 1 MiB | parse-size abort (`54000`) | `# max_sql_length:` (existing) |
| `lifetime_max_cost` | A — envelope | `0` (unlimited) | per-session cumulative abort (`54P02`) | concurrency-style schedule |
| `idle_in_transaction_timeout` | A — envelope | `0` (off) | auto-rollback of an idle open txn (`25P03`, §2.2) | ordered schedule *(deferred, §11)* |
| `work_mem` | A — envelope | 256 MiB | *when* an operator spills (never results) | invariant (spill.md §6) |
| session variables | B — semantic | empty | `current_setting()` / `SHOW` results | `# set:` |
| `time_zone` | B — semantic | `UTC` | `timestamptz`↔`date`/`text` casts, `AT TIME ZONE` | `# timezone:` |
| random / clock source | B — semantic | OS draws | generator values (entropy.md) | `# seed:` / `# clock:` (existing) |

The coarse **physical** read-only file / `BEGIN READ ONLY` transaction (`25006`) is a
`Database`/transaction property, **not** a session setting (§5.1); a read-only *session* is
expressed in the privilege model as `default_privileges = {SELECT}` + `allow_ddl = off`.

- **Bucket A — the safety/authorization envelope.** Governs what a statement is *allowed* to do;
  violations are deterministic errors. This is the **untrusted-query envelope** (CLAUDE.md §13)
  made concrete (§5): a session granted only `SELECT` on the tables it needs (`default_privileges`
  + per-table `grant`) + per-statement-capped + lifetime-budgeted is exactly what a host wraps
  around adversarial SQL.
- **Bucket B — semantic settings.** Feed query *results*, so they are part of the conformance
  contract and must be deterministic and byte-identical across cores (§6). They make the session
  the engine's **fourth host seam**, alongside storage, cost, and entropy/clock (§6).

`work_mem` sits in A by housekeeping (it is an envelope/resource knob) but, like `max_cost`,
**never changes what a query observes** ([spill.md §6](spill.md)) — only when an operator spills.

## 4. Multi-statement input — the splitter, not a buffering batch

Until now jed is strictly single-statement per call ([cost.md §7a](cost.md)). A host still needs to
run a *string of several statements* — a migration, a data import, a `.sql` file. The obvious shape
— an `execute_batch(sql) -> Vec<Outcome>` that buffers every statement's result rows — is **wrong**:
materializing all results *simultaneously* is an unbounded buffer, so the multi-statement interface
would itself violate the §13 "cannot exhaust resources" guarantee. There is therefore **no
simple-protocol batch executor.** The engine provides a **library-level primitive** (no `Session`,
no `Database`) and one thin **session convenience** built on it; the host owns the policy.

### 4.1 The primitive — `split_statements`, a library-level statement iterator

```
split_statements(sql) -> Iterator<Item = StatementSpan>   # top-level core export; no Session/Database
```

A pure, streaming **statement splitter** that operates on a **string and nothing else** — it
depends on neither `Session` nor `Database`, so it is a **top-level core export** (conceptually part
of the parser/lexer surface, [grammar.md](grammar.md)), callable before any database is even opened.
It is documented *here* because the multi-statement *use case* is, not because it belongs to the
session. It scans the input at the **lexer level** — respecting string literals, dollar-quoted
strings, and line/block comments, so a `;` inside them is never a boundary — and **yields one
statement's source text at a time**, lazily. Empty spans (a trailing `;`, blank/comment-only text
between separators) are **skipped**. It allocates **no parse tree** (an O(n) scan) and buffers
**nothing** across statements. Each `StatementSpan` carries its source text and byte offset (for
error reporting).

This is the whole new mechanism. **Execution is unchanged** — the host feeds each span to the
existing single-statement path (`session.execute` / `session.query` / `session.prepare`), so every
existing bound applies per statement *for free*: `max_sql_length` and the `54001` depth limit at
each parse, `max_cost` (`54P01`) per statement, the `lifetime_max_cost` budget (`54P02`) across the
run, the privilege checks (`42501`), and the streaming `Rows` cursor for results. **The host owns
the policy** — wrap the loop in one transaction or autocommit each, drain a statement's rows or drop
them — because it is just a loop over normal single-statement calls:

```
for stmt in split_statements(sql) {
    let rows = session.query(stmt.text());   // or .execute; consume or ignore — the host's call
}
```

The splitter's boundary correctness (a `;` inside a string / dollar-quote / comment is not a split)
is a lexer detail, **per-core unit tested**; it adds no SQL semantics, so it is not itself in the
shared corpus (the *behavior* of what the host runs already is).

### 4.2 The convenience — `session.execute_script`, the migration path

The dominant case — "run this script; I only care that it succeeded, not the rows" — gets a thin
helper so it is a one-liner:

```
session.execute_script(sql) -> ScriptSummary   # split + run each in order, discard result rows
```

It calls the library-level `split_statements` and runs each statement on the session, **discards result-set rows**
(keeping only counts), and — when the session is `Idle` — wraps the run in **one implicit
transaction**, all-or-nothing: any statement's error stops the run, rolls the implicit transaction
back, and returns that `EngineError`. Explicit `BEGIN` / `COMMIT` / `ROLLBACK` in the script
**partition** it (PG simple-query semantics, §2.2); a script run while the session is already `Open`
joins that transaction. `ScriptSummary` is **`O(1)`, not `O(rows)`** — `{ statements_run,
rows_affected_total, cost }` — so memory is bounded by construction (one statement's transient
result at a time, discarded), and `lifetime_max_cost` bounds the total work.

A host that *does* want rows from a multi-statement run does **not** call `execute_script` — it
writes its own §4.1 loop and consumes the cursors it cares about. The `execute_script` transaction
behavior (all-or-nothing, explicit-`BEGIN` partitioning, error-stops-the-run) **is** corpus-tested
(an ordered schedule). Capability: `session.script`.

## 5. The safety envelope (bucket A)

The host is the policy decision point. CLAUDE.md §3 deletes in-database users/roles/RBAC, so
authorization is **not** a permission catalog — it is a capability envelope the host configures on
the session and the engine *mechanically enforces*. This is the host-extension boundary in the
other direction: the engine provides the containment mechanism; the host decides the policy.

### 5.1 The transaction access mode (`25006`) — distinct from privileges

The coarse "this transaction cannot write *anything*" gate stays exactly as it is: a **physically
read-only file** (`Database::open(read_only)`, [api.md §2.1](api.md), OS-enforced — no write
access at all) or a **`BEGIN READ ONLY`** transaction ([transactions.md](transactions.md)) raises
**`25006`** (PG hot-standby behavior) on **any** write, DML or DDL. It is a property of the *file*
/ *transaction*, **not** of authorization, so it is a `Database`/transaction concern, not a
session setting.

The session-level read-only/read-write *option* the first draft carried is **replaced** by the
fine-grained **privilege model** of §5.3: a read-only session is one whose `default_privileges`
is `{SELECT}` (and `allow_ddl` off), so a write resolves to **`42501`** (authorization) rather
than `25006` (access mode). The two gates are **orthogonal and compose** — a write succeeds only
if the transaction is writable (`25006` otherwise) **and** the session holds the privilege
(`42501` otherwise). A writable file may host sessions of any privilege shape.

### 5.2 Per-statement cost ceiling (relocated)

`max_cost` is the existing per-statement ceiling ([api.md §8](api.md), [cost.md §6](cost.md)) —
the instant a statement's accrued cost reaches it, execution aborts `54P01`. Unchanged except
that it lives on the session.

### 5.3 Privileges — the GRANT/REVOKE model

Authorization is **per-object, per-operation**, mirroring PostgreSQL `GRANT`/`REVOKE`. CLAUDE.md
§3 deletes in-database users/roles, so these grants are **not** a privilege catalog — the *host*
holds them on the session and the engine mechanically enforces them. One privilege set per object
kind, exactly PG's:

- **Tables** — the four DML privileges **`SELECT`**, **`INSERT`**, **`UPDATE`**, **`DELETE`** (PG's
  table privileges, minus the ones jed has no feature for — `TRUNCATE`/`TRIGGER`/`REFERENCES`).
- **Functions** — a single **`EXECUTE`** privilege (PG's function privilege).

The session expresses authorization in three layers — the analogue of `GRANT … ON ALL TABLES`
plus per-object `GRANT`/`REVOKE`:

1. **`default_privileges`** — the table-privilege set granted to **every** table (the "all tables"
   default). Default = all four, so a fresh session behaves as today. **This replaces the
   read-only/read-write boolean:** a read-only session is `default_privileges = {SELECT}`. Functions
   default to **`EXECUTE` granted on all**.
2. **`grant`** (the *whitelist*) — `grant[T]` adds privileges to a specific table *beyond* the
   default; `grant[F]` re-grants `EXECUTE` on a function the default withheld.
3. **`revoke`** (the *blacklist*) — `revoke[T]` withholds privileges from a specific table;
   `revoke[F]` withholds `EXECUTE` on a function.

**Effective privilege** for an operation `OP` on object `X`:

> `OP ∈ ( default(kind of X) ∪ grant[X] ) \ revoke[X]`

**revoke takes precedence over grant** (deny wins — order-independent, the safe default). The old
allow-list / deny-list shapes fall out as special cases: a function *allow-list* is "`EXECUTE`
default-off + per-function `grant`"; a *deny-list* is "default-on + per-function `revoke`."

**Which privilege a statement requires** (PG-faithful):

- **`SELECT`** on every table whose **columns it reads** — `FROM`/`JOIN` tables, subquery and
  scalar-subquery tables, an `INSERT … SELECT` source, and the columns an `UPDATE`/`DELETE` reads
  in its `WHERE` / assignment RHS / `RETURNING`.
- **`INSERT` / `UPDATE` / `DELETE`** on the statement's **write target**. A statement that both
  reads and writes a table needs both privileges: `UPDATE t SET … WHERE …` requires `UPDATE` *and*
  `SELECT` on `t`; a bare `INSERT INTO t VALUES …` (no read) requires only `INSERT`.
- **`EXECUTE`** on every **named function** the statement calls. Built-in *operators* are **not**
  gated — they are pure and unavoidable (CLAUDE.md §13); the function privilege is most useful for
  pinning determinism (revoke `uuidv4`/`now()`) or disabling set-returning functions.

**DDL** (CREATE / DROP / ALTER of tables, indexes, types, sequences) is **not** a per-table
privilege — jed has no schema/owner model (§3) — so a single session capability **`allow_ddl`**
(default **on**) governs it; a denied schema change is `42501`. A finer CREATE/ownership privilege
is a deferred follow-on (§11).

**Enforcement** is at **name resolution**, after a name resolves to a catalog object: a missing
privilege raises **`42501 insufficient_privilege`** — PostgreSQL's own permission-denied code
(messages "permission denied for table t" / "for function f"), matched on the canonical
(lowercased) catalog name. The error is **explicit** — the caller learns the object exists but the
operation is off-limits (the chosen behavior over hiding existence behind `42P01`); a host wanting
to hide schema existence from adversarial callers layers that above the engine.

Deterministic and corpus-tested via the GRANT-shaped directives **`# default_privileges:`**,
**`# grant:`**, **`# revoke:`**, and **`# allow_ddl:`** (§6, §9). Capabilities: `session.privileges`
(tables + functions), `session.allow_ddl`. **v1 narrowings (relax later):** base-table and
named-function names only (not PG's per-*column* `GRANT`); `allow_ddl` is one boolean (no
per-object CREATE/ownership privilege).

### 5.4 Session lifetime cost budget

`max_cost` bounds **one statement**; `lifetime_max_cost` bounds the **whole session**. The session
holds a running cumulative cost total; **every** statement (autocommit, batch, or in a
transaction) accrues its metered cost into it. Semantics, mirroring the per-statement ceiling so
the two gates compose:

- `lifetime_max_cost <= 0` (the **default**, `0`) ⇒ **unlimited** (the cumulative total is still
  tracked and readable, nothing aborts).
- `lifetime_max_cost > 0` ⇒ the instant the **session's cumulative** accrued cost reaches it,
  the running statement aborts with **`54P02 session_cost_limit_exceeded`** (a new jed-specific
  `P`-subclass code, sibling to `54P01`). A statement aborts at whichever ceiling it reaches
  first — its own `max_cost` (`54P01`) or the session budget (`54P02`).
- The budget is a hard, monotonic allowance: an aborted statement's **partial** cost still counts
  (the work happened), and once the budget is spent **every** further statement on the session is
  rejected `54P02` at admission (it cannot accrue). The meter is **session state, not snapshot
  state**, so it does **not** roll back when an aborted statement (or an explicit `ROLLBACK`)
  undoes the statement's *effects* — the compute was spent regardless. This is the clean
  "this session has a total compute allowance" model for a multi-tenant / untrusted host.

Determinism: the cumulative total is a deterministic function of the statement sequence against a
given database (each statement's cost is already deterministic and cross-core — [cost.md §1](cost.md)),
so the abort point is itself deterministic and cross-core identical. It is asserted with a
concurrency-style ordered schedule (a sequence of statements on one session, asserting the
`54P02` abort after a known cumulative cost), not the single-record `# cost:` directive (which
is per-statement). Capability: `session.lifetime_cost`.

## 6. Semantic settings (bucket B) — the fourth host seam

Bucket-B settings feed query *results*, so they join the determinism contract (CLAUDE.md §10):
their values must be byte-identical across cores and may **never** be read from the host OS
environment — reading the host locale/zone would be a determinism leak, the
[types.md §11](types.md) ICU-collation cautionary tale. The session thus becomes the engine's
**fourth host-supplied seam**, alongside storage ([storage.md](storage.md)), cost ([cost.md](cost.md)),
and entropy/clock ([entropy.md](entropy.md), [determinism.md §5](determinism.md)).

**A distinction from the clock/entropy seam.** The clock/entropy seam's *production* default
reads the nondeterministic OS (so it earns determinism-ledger entries — `now-clock`,
`uuidv4-entropy`). The session-settings seam's default is a **fixed deterministic value** (`UTC`,
the empty variable map), so it needs **no** exception-ledger entry: it is the seam discipline
(host supplies it, injected for tests) *without* the nondeterminism. A host that points the time
zone at the OS zone owns that determinism consequence (the host-extension boundary, CLAUDE.md §13).

### 6.1 Session variables

PostgreSQL's GUC model, scoped to the session: a **string→string** map (PG GUCs are all text),

- set via the host API (`session.set_var(name, value)` / `session.reset_var(name)`) and via SQL
  `SET name = value` / `RESET name`,
- read via `current_setting('name'[, missing_ok])`, `set_config('name', 'value', is_local)`, and
  SQL `SHOW name`.

Custom variables must be **namespaced** (contain a `.`, e.g. `myapp.tenant`) like PG, to stay
lexically disjoint from built-in settings; an unknown *built-in* setting is an error, an unknown
*custom* (dotted) name reads as empty/`42704` unless `missing_ok`. Values affect results
(`current_setting` returns them), so the corpus pins them with a **`# set: name=value, …`**
directive (a stock-runner-ignored comment bound to the next record and reset after — like
`# seed:`/`# clock:`). Capability: `session.variables`.

**v1 scope (narrow hard, relax later):** the host-API get/set + `current_setting()` read + the
built-in `time_zone` (§6.2). The full SQL `SET`/`RESET`/`SHOW` grammar, `set_config()`, and
**`SET LOCAL`** transaction-scoped variables (which *do* roll back with their transaction) are
follow-ons (§10).

### 6.2 Session time zone

The time zone is the one **built-in** session variable, `TimeZone` (`SET TIME ZONE …` /
`time_zone` in `current_setting`), defaulting to **`UTC`**. The default is deliberately UTC, **not
the host's local zone** — a fixed deterministic value (§6 above).

It is **forward-looking infrastructure.** The cast that consumes it — `timestamptz → date` /
`timestamptz → text`, and `AT TIME ZONE` — is a deferred timestamp follow-on
([TODO.md Phase 3](../../TODO.md); the `date`/`timestamp` slices left it explicit), and jed has no
tz database yet. So v1 lands the **setting slot** (default `UTC`; accepting `UTC` and fixed
numeric offsets like `+05:00`, rejecting named zones `0A000` until a tz database lands), exactly as
the cost seam landed before its ceiling — and the type-cast slices consume it when they land. The
`timestamptz→date` example in the original request is therefore *unblocked by* this slot but
*delivered by* that later cast slice. Capability: `session.timezone`; corpus directive
`# timezone: <zone>`.

## 7. Errors

The session adds one code and reuses the rest:

| code | when |
|---|---|
| **`54P02 session_cost_limit_exceeded`** | the session's cumulative cost reached `lifetime_max_cost` (§5.4) — **new** |
| `54P01 cost_limit_exceeded` | a single statement reached `max_cost` (§5.2, existing) |
| `42501 insufficient_privilege` | a statement lacked a table privilege (`SELECT`/`INSERT`/`UPDATE`/`DELETE`), a function `EXECUTE` privilege, or DDL permission (§5.3) — existing PG code, new use |
| `25006` | a write against a physically read-only file / `READ ONLY` transaction (§5.1, existing) |
| `25P03 idle_in_transaction_session_timeout` | an open transaction sat idle past `idle_in_transaction_timeout` (§2.2) — existing PG code, deferred enforcement |
| `54000` | a statement (or a script's statement) exceeded `max_sql_length` (§4, existing) |
| `42704` | `current_setting` on an unknown setting without `missing_ok` (§6.1, existing) |

`54P02` is registered in [../errors/registry.toml](../errors/registry.toml) when the lifetime-cost
slice lands, modeled on `54P01` (a documented `P`-subclass divergence — PG has no execution-cost
ceiling, CLAUDE.md §1/§13).

## 8. Idiomatic mapping

Extends the [api.md §6](api.md) table; same shape across cores, idiomatic spelling per core.

| Concept / op | Rust | Go | TS |
|---|---|---|---|
| split statements *(top-level — not a `Session`/`Database` method)* | `jed::split_statements(sql) -> impl Iterator<Item = StatementSpan>` | `jed.SplitStatements(sql) iter.Seq[StatementSpan]` | `splitStatements(sql): Iterable<StatementSpan>` |
| open session | `db.session(opts) -> Session` | `db.Session(opts) (*Session, error)` | `session(db, opts): Session` |
| close session | `session.close()` + `Drop` | `session.Close() error` | `session.close(): void` |
| run a script | `session.execute_script(sql) -> Result<ScriptSummary>` | `session.ExecuteScript(sql) (ScriptSummary, error)` | `session.executeScript(sql): ScriptSummary` |
| set lifetime budget | `session.set_lifetime_max_cost(n)` | `session.SetLifetimeMaxCost(n)` | `session.setLifetimeMaxCost(n)` |
| cumulative cost gauge | `session.lifetime_cost() -> i64` | `session.LifetimeCost() int64` | `session.lifetimeCost: number` |
| default privileges | `SessionOptions { default_privileges }` | `SessionOptions{ DefaultPrivileges }` | `{ defaultPrivileges }` |
| grant / revoke | `session.grant(privs, on)` / `session.revoke(privs, on)` | `session.Grant` / `session.Revoke` | `session.grant` / `session.revoke` |
| allow DDL | `SessionOptions { allow_ddl }` | `SessionOptions{ AllowDDL }` | `{ allowDdl }` |
| set variable | `session.set_var(name, val)` / `reset_var` | `session.SetVar` / `ResetVar` | `session.setVar` / `resetVar` |
| read variable | `session.var(name) -> Option<String>` | `session.Var(name) (string, bool)` | `session.var(name): string \| undefined` |
| time zone | `SessionOptions { time_zone }` / `session.set_time_zone(z)` | `SessionOptions{ TimeZone }` / `SetTimeZone` | `{ timeZone }` / `setTimeZone` |

The settings already on the handle today — `max_cost`, `max_sql_length`, `work_mem`, the
random/clock sources ([api.md §6/§8/§10](api.md)) — relocate onto `Session` unchanged in shape
(the bare `Database` proxies them to its default session for back-compat). `read_only` is the one
exception: its *physical* form stays a `Database` open option ([api.md §2.1](api.md), `25006`),
and its session-level *authorization* role is superseded by the privilege model (§5.3, `42501`).

## 9. Determinism & the conformance contract

Both buckets are deterministic and corpus-testable, which is what keeps the session inside the
no-reference-implementation net (CLAUDE.md §2):

- **Envelope errors are deterministic** — a blocked table (`42501`), a per-statement ceiling
  (`54P01`), a lifetime budget (`54P02`) all fire at a deterministic, cross-core-identical point.
  The lifetime budget specifically is pinned by an ordered multi-statement schedule (the
  concurrency-suite style — [conformance.md](conformance.md)), since it is cumulative across
  statements and the single-record `# cost:` directive cannot express it.
- **Semantic settings are deterministic given the seam** — `# set:` / `# timezone:` inject fixed
  values, so a record that depends on a session variable or the zone is byte-identical across
  cores; defaults (`UTC`, empty map) are fixed, so an undecorated record is unaffected.

These are **per-impl API surface for the *mechanism*** (the `Session` object is not in the shared
corpus — [api.md §1](api.md)) but **shared-corpus for the *observable SQL behavior*** (the errors
and the `current_setting`/zone-dependent results). That split mirrors how `max_cost` is a per-impl
setter but `54P01` is a corpus-asserted outcome.

## 10. Slicing / delivery

Not one slice — a sequence of vertical slices (CLAUDE.md §10), each independently testable. Spec
(this doc) lands first; cores follow in lockstep:

1. **Session concept + the one stateful default session** — ✅ **landed (all 3 cores).** Un-fused
   `Database`/`Session`, relocated the settings onto `Session`, made the `Database`-owned default
   session explicit and stateful (§2.1) and the **transaction state machine** explicit on the session
   (`Idle`/`Open`/`Failed` = `TxStatus`/`db.status()`, §2.2) — collapsing the separate `Transaction`
   object into session state + RAII sugar. `db.session(opts)` mints additional sessions that share
   committed storage and run sequentially via a swap. Near-pure refactor — corpus + all suites
   unchanged (162/0 ×3, NoREC 660/660), per-core `session` tests added. (One per-core divergence: the
   TS `Session` exposes `execute` + settings + `status`; its `view`/`update` closure sugar is deferred
   to avoid an `api.ts` module cycle — TS drives an additional session's transactions via SQL
   `BEGIN`/`COMMIT` through `execute`.)
2. **Multi-statement splitter + `execute_script`** (§4) — the **library-level** lexer `split_statements`
   function (no `Session`/`Database`; per-core unit tested) + the session-level discard-rows /
   one-implicit-transaction `execute_script` convenience (corpus-tested ordered schedule). The
   capability `session.script` covers `execute_script` only — the splitter adds no SQL semantics.
3. **Privileges — the GRANT/REVOKE model** (§5.3) — per-table `SELECT`/`INSERT`/`UPDATE`/`DELETE`
   + function `EXECUTE` + `allow_ddl`, resolve-time enforcement with `42501`. Capabilities
   `session.privileges` / `session.allow_ddl`; the `# default_privileges:` / `# grant:` /
   `# revoke:` / `# allow_ddl:` directives.
4. **Lifetime cost budget** (§5.4) — the cumulative meter + `54P02` + the ordered-schedule corpus
   entries. Capability `session.lifetime_cost`; registers `54P02`.
5. **Session variables** (§6.1, v1 scope) — the string map, host get/set, `current_setting()`,
   the `# set:` directive. Capability `session.variables`.
6. **Session time zone slot** (§6.2) — the `time_zone` built-in (default `UTC`, offsets only),
   the `# timezone:` directive. Capability `session.timezone`. (The consuming `timestamptz→date`
   cast is a separate later type slice.)

## 11. Open / deferred (none foreclosed)

- **`idle_in_transaction_timeout` enforcement** — the setting slot is defined (§2.2/§3); the
  background auto-rollback of an idle open transaction + the `25P03` abort is a deferred slice (it
  needs a clock read on the §6 seam, so its trigger stays deterministic-given-the-clock).
- **A streaming multi-result reader is *not* a special API** — a host that wants the rows of every
  statement in a multi-statement run loops `split_statements` (§4.1) and consumes each
  `session.query` cursor itself; nothing further is owed. (The cursor's own pull-streaming is the
  separate [spill.md](spill.md) / [api.md §4](api.md) work.)
- **`SET LOCAL` / transaction-scoped variables** — variables that roll back at transaction end
  (PG `SET LOCAL`); v1 variables are session-scoped only (§6.1).
- **Full SQL `SET`/`RESET`/`SHOW` grammar + `set_config()`** — v1 exposes the host API + the
  `time_zone` built-in; the general SQL surface is a follow-on (§6.1).
- **Named time zones + a tz database** — v1 accepts `UTC` and fixed offsets only (§6.2); named
  zones (`America/New_York`) need a tz database (a separate, large feature).
- **Convergence with the shared handle (api.md §2.5)** — folding `Session` into the per-caller
  `ReadHandle`/`WriteHandle` so a session *is* the configured concurrency handle (§2); kept
  separate this refinement.
- **Column-level privileges + a CREATE/ownership privilege** — PG has per-*column* `GRANT` and
  models DDL via `CREATE`-on-schema + ownership; v1 gates whole base-table + named-function names
  and folds all DDL under one `allow_ddl` boolean (§5.3).
- **Per-statement setting overrides** — an options object on `execute`/`prepare` overriding a
  session setting for one call (the [api.md §8](api.md) "per-call override stays open" note),
  unchanged by this doc.
