# Frozen baseline findings

Baseline: `362e0798ac3a8bd04bd2da556044b6ec73bf468a`. These observations do
not amend shared expected results or declare Swift acceptance complete.

- **Private temp writes and host admission:** temp-tables.md §5 explicitly says
  private temp writes require no global writer gate and are available to a
  read-only session. The frozen Go `shared.go` dispatch and `execute.go` write
  classification instead gate every write and reject writes in read-only modes;
  the Rust and TS dispatches use the same broad classification. Swift currently
  follows the canonical private-temp contract, with host-boundary tests. Explicit
  read-only COMMIT still discards private changes, as §5.1 and all three cores
  specify. Broader shared-host reconciliation remains separate work. A temp DML
  expression/default advancing a main sequence must still select a shared writer;
  Swift now includes a metadata-only expression/default/CHECK/FK-effect classifier;
  its dedicated host boundary tests pass in the281-file integration gate.
- **Mixed DROP capability:** the frozen cores select `allow_temp_ddl` for an
  entire DROP when any target is temporary, including its persistent targets.
  Swift preserves that behavior pending a separate shared correction; it does
  not silently strengthen the gate in one core. Ordinary persistent DDL still
  uses `allow_ddl`, and temp ANALYZE/REINDEX remain maintenance under that gate.
- **Writable CTE parameters:** the frozen cores bind query/RETURNING metadata
  before independently binding each DML part. Parameters used only in DML may
  therefore produce the existing parameter-count/gap errors. Swift retains the
  same preparation boundary; no new inference policy is selected here.
- **Second durable writer:** the process corpus stages both file changes and
  requires `0A000` at COMMIT. Swift's initial statement-level rejection was too
  early and has been removed. Commit still rejects before any staged or durable
  storage write, and the unchanged attachment-gate-order scenario now passes.
- **Historical nosort render tags:** frozen `query/statistics_p9.test:43` declares
  nine render tags for ten selected columns and twenty expected cells. The shared
  runner treats those tags as rendering hints and compares every nosort cell as
  one flat array; precise shape assertions use `#names` and `#types`. Swift now
  follows that behavior while retaining complete-row validation for `rowsort`.
  The historical header is unchanged, and no expected cell is discarded. This
  adapter correction was independently checked against the frozen Go runner.
- **Exact KMV capacity:** all three frozen collectors entered estimation when the
  retained heap reached4,096, even if no additional distinct hash had been seen.
  A4,096-distinct population with duplicate rows therefore disagreed with
  statistics.md§4.2. Local-only shared correction `616b3a3e` remembers overflow
  explicitly; all three supported cores pass the new boundary regression and
  their statistics suites. Swift implements the canonical rule and exercises
  the new shared case separately from its frozen baseline corpus.
