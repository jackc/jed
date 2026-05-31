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
set. Class `22` is *data exception*.

> Status: seeded with the single code the integer type semantics require
> (`22003`, numeric value out of range). Grows per subsystem.
