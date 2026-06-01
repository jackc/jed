# spec/design/ — subsystem design docs (the "why")

Prose rationale for each subsystem. A design doc plus the relevant conformance corpus is
what an agent needs to work a subsystem without holding the whole engine in context
(CLAUDE.md §10).

Each doc explains *why* a decision was made and points at the **data** that encodes it
(the TOML tables, the fixtures). The data is authoritative; these docs are the reasoning.

## Contents

- [types.md](types.md) — the type system: scalar set, comparison/coercion/promotion,
  three-valued NULL logic, integer overflow, and order-preserving key encoding.
- [conformance.md](conformance.md) — the conformance corpus: sqllogictest-style format,
  structured-error matching, the tier + capability-flag system, and determinism rules.
- [encoding.md](encoding.md) — order-preserving key encoding: the `int-be-signflip` rule,
  the nullable presence tag (NULLs-first), composition, and descending order.
- [storage.md](storage.md) — the storage seam: block interface, page model, and the
  root-pointer-swap commit model (CLAUDE.md §3/§9).
- [cores.md](cores.md) — what counts as a core vs. a wrapper, when to add the next core,
  and which languages add new divergence (the selection rule behind CLAUDE.md §2).
