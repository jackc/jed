# Native Swift attachment and temporary-domain integration

Implementation plan against the frozen baseline, not a new SQL design. The
canonical contracts are `attached-databases.md`, `temp-tables.md`, `transactions.md`,
`page-service.md`, and `memory.md`.

- Keep every catalog/tree/pager/txid in its own `DomainSnapshot`. Query routing
  carries an immutable named map; it never constructs a merged storage snapshot.
  Bare relation names resolve temp then main, explicit names resolve exactly one
  domain, and stored definitions resolve in their owning domain. Composite
  identities keep their original catalog identity.
- `DatabaseDomains` owns handle-scoped attachments, a logical writer gate and a
  resident root-publication boundary. It owns attached host lifetimes, so an
  attachment is not an alias to a user-managed Database handle. Attach/detach
  serialize with the logical writer; detach fails while its core has pins.
- A read bundle pins main and attached roots under the publication boundary.
  Cursor retention duplicates pins independently. The session adds its private
  temp pin; temp never enters the handle-wide attachment registry. No owner
  foreground operation requires a coordination syscall or fresh file meta read.
- A writable transaction holds the logical writer and all writable file gates in
  canonical real-path order, plus memory-domain leases. Read-only attachments
  contribute pins only. The transaction retains a working snapshot per domain;
  savepoints restore the full map and sequence history. Private-temp-only work
  requires no handle-wide writer, including in a read-only session/transaction.
- `DomainCommitGroup` prepares every changed domain before writes, stages memory
  domains without changing published accounting, persists at most one changed
  durable domain, then adopts/publishes all roots under the handle boundary.
  Unchanged domains do not advance txid or issue a barrier. Memory adoptions are
  validated before the durable point and cannot fail after it. Failed preparation
  or memory staging leaves published roots and accounting intact.
- SQL settings carry a query-domain map and a target-domain name. A mutation gets
  the actual target snapshot, while source subqueries resolve against the named
  pre-statement read map. Writable CTEs preserve this distinction across parts.
  Names are not rewritten to impersonate main. Cross-domain persisted references
  and frozen attachment narrowings reject at their specified admission phase.
- Temp catalog metadata records ON COMMIT actions on the session, outside every
  persisted catalog. The temp storage uses the existing memory-to-scratch seam,
  its own cursor watermark, `temp_buffers`, and the exact `54P03` page budget.
- Extend the whole-file adapter with ordered host actions. Attach actions execute
  between records where authored; they must not all be moved to file setup. Add
  every frozen attachment/temp file whole, retaining explicit failures.

Ownership: root owns the registry, session/read routing and adapter integration;
host agent owns commit-group staging and storage review; query/DML agents keep
writable-CTE scope and target resolution compatible. Integrate after a checkpoint,
then run shared SQL/process/concurrency suites and host atomicity/failure tests.
