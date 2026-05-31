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
