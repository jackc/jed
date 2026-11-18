//go:build jedtooling

package jed

import "slices"

// These entry points exist only for repository tooling built with -tags jedtooling.
// They are excluded from ordinary library builds and are not an embedding API.

// BackupForTesting writes a consistent backup in the requested test format.
func BackupForTesting(db *Database, dest string, pageIdentical bool) error {
	return backupForTesting(db, dest, pageIdentical)
}

// CrashBeforeNextPageServiceAckForTesting injects an owner crash after durability.
func CrashBeforeNextPageServiceAckForTesting(db *Database) {
	crashBeforeNextPageServiceAckForTesting(db)
}

// HangUpNextPageServiceAckForTesting drops the next durable commit acknowledgement.
func HangUpNextPageServiceAckForTesting(db *Database) { hangUpNextPageServiceAckForTesting(db) }

// PageServiceRoleForTesting reports a handle's page-service role.
func PageServiceRoleForTesting(db *Database) string { return pageServiceRoleForTesting(db) }

// PageServiceAttachmentRoleForTesting reports an attachment's page-service role.
func PageServiceAttachmentRoleForTesting(db *Database, name string) string {
	return pageServiceAttachmentRoleForTesting(db, name)
}

// SetPageClientReadAheadForTesting toggles page-client read-ahead.
func SetPageClientReadAheadForTesting(enabled bool) { setPageClientReadAheadForTesting(enabled) }

// SetSpillDirForTooling supplies a scratch directory to the conformance harness.
func SetSpillDirForTooling(db *Database, dir string) { setSpillDirForTooling(db, dir) }

// QueryMemoryPeak returns the process-wide test peak since the last reset.
func QueryMemoryPeak() int64 { return queryMemoryPeakForTooling() }

// QueryMemoryUnderflows returns the number of accounting invariant violations.
func QueryMemoryUnderflows() uint64 { return queryMemoryUnderflowsForTooling() }

// ResetQueryMemoryPeak resets the test peak before a conformance record.
func ResetQueryMemoryPeak() { resetQueryMemoryPeak() }

// SetVerifyLivePages enables the expensive commit-time storage invariant check.
func SetVerifyLivePages(on bool) { setVerifyLivePages(on) }

// TempSpills returns the process-wide count used to verify forced spilling.
func TempSpills() uint64 { return tempSpillsForTooling() }

// ReadSessionForTesting mints a session pinned READ ONLY to the committed snapshot for its life —
// session.md §2.4's read_session, which the concurrency harness drives (the public API spells it
// Session + Begin with ReadOnly).
func ReadSessionForTesting(db *Database) *Session { return db.readSession() }

// WriteSessionForTesting mints a session holding the writer gate with an open READ WRITE block —
// session.md §2.4's write_session (blocks until the gate is free).
func WriteSessionForTesting(db *Database) *Session { return db.writeSession() }

// CommitSessionForTesting ends the session's open block the way SQL COMMIT does: a failed block
// rolls back without an error (transactions.md §6). No open block is a no-op.
func CommitSessionForTesting(s *Session) error { return s.commitBlock() }

// RollbackSessionForTesting rolls back the session's open block; no open block is a no-op.
func RollbackSessionForTesting(s *Session) error { return s.rollbackBlock() }

// OldestLiveTxidForTesting is the reader-liveness watermark (transactions.md §8): the oldest snapshot
// version a live reader pins, or the committed version with none.
func OldestLiveTxidForTesting(db *Database) uint64 { return db.oldestLiveTxid() }

// SupportedCapabilitiesForTesting lists the corpus capabilities this core implements (the
// conformance harness's gate).
func SupportedCapabilitiesForTesting() []string { return slices.Clone(supportedCapabilities) }

// PageCountForTesting is the file's page high-water (0 in memory).
func PageCountForTesting(db *Database) uint32 { return db.pageHighWater() }
