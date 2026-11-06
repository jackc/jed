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
  Swift's current target-only classifier does not yet cover that case.
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
