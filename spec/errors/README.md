# spec/errors/ — the error-code registry

Errors are **structured data, not free text** (CLAUDE.md §5, §10). A machine-legible
registry is what makes `statement error <pattern>` matching in the conformance corpus
stable across every implementation.

## Files

- [registry.toml](registry.toml) — the registry. Each entry has a stable `code`, a
  `name`, and a message template.

## Code scheme

Codes follow **SQLSTATE** (the 5-character SQL-standard class/subclass scheme), borrowed
because it is principled and well-known — consistent with "PG as inspiration where its
behavior is principled" (CLAUDE.md §1). We own the registry; we are not bound to PG's full
set. The first two characters are the class (e.g. `22` is *data exception*).

> Status: the registry now spans **56 codes** across eight SQLSTATE classes —
> feature-not-supported (`0A`), cardinality violation (`21`), data exception (`22`),
> integrity-constraint violation (`23`), invalid-transaction-state (`25`),
> syntax-error-or-access-rule-violation (`42`), program-limit-exceeded (`54`), and
> system/internal errors (`58`/`XX`). It grows per subsystem as features land.
