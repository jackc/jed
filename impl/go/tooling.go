//go:build jedtooling

package jed

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
