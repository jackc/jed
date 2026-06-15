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
- [storage.md](storage.md) — the storage seam: block interface, page model, and the
  root-pointer-swap commit model (CLAUDE.md §3/§9).
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
