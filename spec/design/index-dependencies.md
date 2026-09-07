# Persisted expression dependencies

This is the admission and lifetime contract for B-tree expression keys and partial-index
predicates. It replaces the blanket rejection of expressions mentioning `timestamptz`.

## Admission

Resolution accumulates the dependencies of every operation, including operations nested in
casts, CASE, COALESCE, containers, and function arguments. Column reads and constants are
immutable regardless of their type. An operation's resolved overload, rather than the types
mentioned anywhere in the expression, determines its own dependencies. The shared function
catalog's volatility is authoritative for ordinary scalar calls. Specialized datetime syntax
and overloads apply these argument-sensitive rules:

| Operation | Dependency |
|---|---|
| NULL tests, same-type comparisons, instant subtraction | arguments only |
| EXTRACT / date_part from timestamptz, constant field `epoch` | arguments only |
| Other EXTRACT / date_part from timestamptz | session timezone |
| timestamp/date conversion crossing the timestamptz boundary | session timezone |
| timestamptz interval arithmetic | arguments only under jed's existing UTC arithmetic contract (interval.md) |
| Two-argument date_trunc on timestamptz; six-argument make_timestamptz | session timezone |
| AT TIME ZONE, explicit-zone date_trunc / make_timestamptz | arguments plus the explicit zone |
| UTC or a fixed numeric zone offset | no timezone-data dependency |
| Named zone | loaded zone's name, tzdata version, and TZif CRC-32 |
| Nonconstant zone expression | the complete loaded named-zone set |
| Clock, entropy, sequences, session variables, runtime text-to-date / relative date inputs | nonimmutable |

Session/clock/entropy/state dependencies reject CREATE INDEX with `42P17`, even under a
null test or an unselected CASE arm. This is compositional analysis, not a theorem prover.
The ordinary structural restrictions (no parameters, aggregates, windows, or subqueries)
remain. Errors and dependencies of children are not erased by a parent operation. Host
functions retain their immutable declaration and persisted component/version contract.

An explicitly named zone must be available when the index is created, including on an empty
table (`22023` otherwise). Dynamic zone expressions pin the entire currently loaded named
set, including aliases; UTC and fixed offsets remain available without a bundle. Pinning the
whole set makes arbitrary row-computed zones sound without predicting future row values.
Catalog size limits apply to this dependency list just as to other schema metadata. An empty
loaded version label is unpinnable (`42P17`); dependency counts and individual UTF-8 strings must
fit their u16 representation (`54000` otherwise).

## Persistence and use

Format **32** adds index_flags bit3 `has_timezone_deps`. When set, after the optional host
dependency list, write `u8 dynamic` (0 or 1), `u16 count`, then count entries:
`u16 name_len`, UTF-8 name, `u16 version_len`, UTF-8 tzdata version, `u32 tzif_crc32`.
Names are exact and case-sensitive, sorted by UTF-8 bytes, unique, and nonempty. A static
list must be nonempty; a dynamic list may be empty. Only B-tree indexes admit the flag.
An index with no timezone dependencies writes no extra bytes. The CRC identifies the loaded
TZif content in addition to its release label; it is an integrity stamp, not authentication.

At index resolution, compare its persisted dependencies with the loaded data. Static lists
require every named entry to match. Dynamic lists require exact equality with the loaded set.
A missing or different dependency is `XX002`: read planning excludes that index and uses other
valid access paths; writes to its table fail before mutation. Opening and reading stored rows
does not require timezone data. A query that itself calls an unavailable zone still reports
the normal evaluation error. Dynamic-dependent index plans are not cached across statements,
so additions to the engine-global loaded zone set cannot leave a stale admissibility decision.

## Rebuilding

After loading the desired bundle in a fresh process (loading is additive, first definition
wins), rebuild each affected index with transactional SQL:

```sql
BEGIN;
DROP INDEX active_accounts;
CREATE UNIQUE INDEX active_accounts ON accounts (email)
  WHERE (deleted_at AT TIME ZONE 'America/New_York')::date >= DATE '2026-01-01';
COMMIT;
```

The existing DDL transaction machinery makes replacement, dependency adoption, and uniqueness
validation atomic. A failed build leaves the old committed index and its pins intact after
rollback. DROP INDEX remains possible with missing or mismatched dependencies. A dedicated
REINDEX/upgrade convenience API is not required for this lifecycle.

## Verification

Shared SQL conformance covers positive compositions, unsafe casts and constructors, partial
uniqueness and membership transitions, and matching indexed/full-scan results. PostgreSQL is
the oracle except for documented stronger argument-sensitive admission (epoch extraction and
explicit-zone make_timestamptz) and dependency lifecycle. Per-core host tests cover differing
session zones, missing/skewed bundles, dynamic zone sets, transaction rollback on failed rebuild,
and prepared-plan invalidation. Byte fixtures and cross-core round trips cover the new flags,
dependency ordering and encoding, and corruption checks.
