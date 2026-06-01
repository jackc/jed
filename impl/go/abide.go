package abide

// SupportedCapabilities lists the capabilities this implementation currently
// supports (spec/conformance: the gating axis). The harness runs a corpus file iff
// every capability in the file's `# requires:` header is in this set. GROWS as
// Phases B–E land. A whole corpus file only runs once all its required capabilities
// are present, so the harness stays all-skip until the `core` profile is complete
// (Phase E); per-phase correctness is driven by the Go unit tests until then.
var SupportedCapabilities = []string{
	// Phase B — CREATE TABLE with typed columns + single-column PRIMARY KEY.
	"ddl.create_table",
	"ddl.primary_key",
	// Phase C — INSERT ... VALUES with positional type-checking + overflow trap.
	"dml.insert",
	"error.overflow_trap",
	// Step 6 — row mutation: UPDATE (in-place) + DELETE.
	"dml.update",
	"dml.delete",
	// Phase D/E — SELECT, WHERE (=, ordering), ORDER BY, IS [NOT] NULL, 3VL, casts,
	// cross-type comparison via the promotion tower, and all three integer types.
	"query.select",
	"query.where_eq",
	"query.comparison_order",
	"query.is_null",
	"query.order_by",
	"null.three_valued",
	"compare.promotion",
	"cast.explicit",
	"types.int16",
	"types.int32",
	"types.int64",
}

// Execute parses and executes one SQL statement against db.
func Execute(db *Database, sql string) (Outcome, error) {
	stmt, err := ParseSQL(sql)
	if err != nil {
		return Outcome{}, err
	}
	return db.ExecuteStmt(stmt)
}
