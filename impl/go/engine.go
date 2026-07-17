package jed

import (
	"fmt"
	"strings"
)

// The engine handle: session/transaction state, snapshot routing, host config, and reference-data
// loading. This file holds the engine/sessionState/activeTx structs and SessionOptions/TxStatus, the
// constructors (newEngine/newSession), the snapshot routing that resolves a name to the right store —
// main vs session-temp vs attached database (working/tempSnap/attach*Snap, the scoped lkp*/write*
// helpers), the host configuration accessors (cost/work_mem/privileges/vars/timezone/DDL gates), and
// Unicode/time-zone/collation loading (LoadUnicodeData/LoadTimeZoneData/UpgradeCollations).

// Engine is the database handle: the last committed Snapshot plus, while a transaction is open,
// the writer's working snapshot (CLAUDE.md §3, transactions.md §2). Reads run against the visible
// snapshot — the open transaction's working if any, else committed; a write mutates working and
// commit swaps committed := working (rollback drops working, since committed was never touched).
// Every write — autocommit included — runs as a transaction, which unifies the two paths.
type engine struct {
	committed *snapshot
	// explainActual is non-nil only while EXPLAIN ANALYZE executes its inner statement. Execution
	// records exact operator sub-meter snapshots here; ordinary statements pay only this nil check.
	explainActual *actualCostProfile
	// session is the DEFAULT SESSION (spec/design/session.md §2.1): the per-connection state this
	// handle runs statements through — the open transaction (the Idle/Open/Failed machine, §2.2),
	// the relocated settings (maxCost/maxSQLLength/workMem, the entropy/clock seam), and the
	// currval/lastval session state. A bare Engine IS committed storage + this one long-lived
	// stateful default session; the convenience methods operate on it. NewSession mints additional
	// independent sessions (run sequentially on this single-threaded handle by swapping in here).
	session sessionState
	// path is the backing file (empty for an in-memory database). Set by the host API
	// Open/Create (spec/design/api.md §2); Commit writes here.
	path string
	// spillDir is the host scratch directory for external-sort runs. File hosts set it independently
	// of path (normally to os.TempDir), so read-only database directories are never spill targets.
	// Empty for hosts with no spill backing (in-memory / OPFS).
	spillDir string
	// pageSize is the page size this database serializes with (fixed for the life of a file).
	pageSize uint32
	// pageCount is the on-disk page high-water — the index an incremental commit extends at when the
	// free-list is exhausted (spec/fileformat/format.md). Set from the file's meta on Open, from the
	// initial image on Create; 0 (unused) for an in-memory database.
	pageCount uint32
	// freePages is the free-list (P6.2 + v25): page indices a prior root abandoned, reusable by the
	// next incremental commit (spec/fileformat/format.md *Reclamation*). Read from the persisted chain
	// on Open (v25 — meta offset 28), and returned to within-session by periodic compaction; drawn
	// lowest-first before the file is extended. A page leaves the list only by being allocated into a
	// new committed version, so it is reachable from no live snapshot and reuse is torn-write-safe. nil
	// for a freshly-created file (a from-scratch image leaks nothing).
	freePages []uint32
	// liveAtCompaction is the live (reachable) page count recorded at this handle's last within-session
	// compaction — the cheap periodic trigger basis (v25): a bare-engine file commit re-runs the
	// reclamation walk only once the high-water passes ~2× it, mirroring storage (shared.go). 0 for an
	// in-memory database (no persistence).
	liveAtCompaction uint32
	// freeGenTxid is the version the current freePages list is "as of" — the last compaction's txid, or
	// the committed version at open (every persisted free page is dead at the committed version). It gates
	// reuse under the reader-liveness watermark (transactions.md §8): a page dead at generation G is safe
	// to reuse only once no reader pins a version older than G. A bare single-handle engine has no live
	// registry (oldest_live == committed), so the gate always passes and the byte layout is unchanged.
	freeGenTxid uint64
	// paging is the shared paging context for a file-backed database (spec/design/pager.md): the open
	// pager (kept for the handle's life) + the bounded leaf buffer pool, shared with every table store
	// so reads fault OnDisk leaves through the one pool. The load reads pages through it and every
	// commit writes through it. nil for an in-memory database (persist is then a no-op); set by
	// Open/Create, dropped by Close.
	paging *sharedPaging
	// readOnly marks a handle opened read-only (spec/design/api.md §2.1, OpenOptions.ReadOnly).
	// A read-only handle behaves like PostgreSQL hot standby: every transaction defaults to READ
	// ONLY, an explicit READ WRITE request and any write statement are 25006, and the file is
	// opened without write access, so it is never written. Always false for an in-memory or
	// normally-opened database.
	readOnly bool
	// tempStorage is the SESSION-LOCAL temp domain's storage identity (temp-tables.md §6): the private
	// in-RAM memoryBlockStore + pager + pinned pool its temp tables ride, with within-session compaction
	// on. Created lazily on the first session-local temp DDL (newTempStorage); nil until then. Its
	// pageCount is the domain's footprint — the page-based temp budget.
	tempStorage *storage
	// openStreams counts this handle's live streaming cursors (Query's pull source, not a materialized
	// result). A streaming cursor pins a snapshot it faults lazily, so while one is open a temp-domain
	// compaction (persistTemp → maybeCompact) must NOT reclaim pages — it could free one the cursor still
	// faults. Incremented when a streaming Rows opens, decremented on Close (single-threaded per handle).
	openStreams int
	// core is the shared core this engine's session belongs to (attached-databases.md §5), or nil for a
	// bare/transient engine (a test engine, a snapshotEngine, committedEngine — none of which see
	// attachments). It is the engine's route to the core-owned attachment registry (core.attachments)
	// during a commit persist; the READ view of attachments is the pinned attachedCommitted below.
	core *sharedCore
	// attachedCommitted is the PINNED committed root of every host-attached DATABASE-scoped database
	// (attached-databases.md §5), keyed by lowercased name — this session's stable read view, snapshot
	// isolated: refreshed from core.roots.attached at each autocommit statement (refreshCommitted) and
	// pinned for the life of an explicit BEGIN block. nil/empty when nothing is attached. Session-local
	// temp is NOT here (it is on sessionState.tempCommitted); this is only the Database-scoped roots.
	attachedCommitted map[string]*snapshot
	// estimatorTouched de-duplicates per-relation revision advances within one top-level statement.
	// A data-modifying CTE may target the same table more than once; P2 advances it exactly once.
	estimatorTouched map[estimatorTouchedRelation]struct{}
}

type estimatorTouchedRelation struct {
	database string
	table    string
}

// SessionOptions are the relocatable session settings (spec/design/session.md §3 — the bucket-A
// envelope subset landed in S1): the cost ceiling, the input-size limit, and the work-memory
// budget. Passed to (*Engine).NewSession. A zero MaxSQLLength or WorkMem takes its default at
// construction (use the setter for the 0 ⇒ unlimited form); a zero MaxCost IS unlimited (the
// genuine default). The entropy/clock seam is injected via Session.SetRandomSource/SetClockSource.
type SessionOptions struct {
	MaxCost int64
	// LifetimeMaxCost is the per-session cumulative cost budget (spec/design/session.md §5.4); 0 ⇒
	// unlimited (the default). Bounds the whole session: the instant the session's running total
	// reaches it, the in-flight statement aborts 54P02 (and once spent, every further statement is
	// rejected at admission). Sibling to MaxCost, which bounds one statement.
	LifetimeMaxCost int64
	MaxSQLLength    int
	LockTimeoutMs   uint64
	WorkMem         int
	// DefaultPrivileges is the table-privilege set granted to every table — the GRANT … ON ALL TABLES
	// default (spec/design/session.md §5.3). nil ⇒ all four (the default), so a fresh session is
	// unrestricted; PrivSetEmpty.With(PrivSelect) is a read-only session. A pointer so the zero
	// SessionOptions stays permissive (the empty set is a meaningful, distinct value).
	DefaultPrivileges *PrivilegeSet
	// AllowDDL governs whether PERSISTENT DDL (CREATE/DROP/ALTER of persistent relations) is permitted;
	// a denied schema change is 42501 (§5.3). nil ⇒ on (the default). A pointer so the zero
	// SessionOptions allows DDL. Its scope narrows with temporary tables (temp-tables.md §5): AllowTempDDL
	// is the temp-scoped sibling gate.
	AllowDDL *bool
	// AllowTempDDL governs whether SESSION-LOCAL temporary-table DDL is permitted
	// (spec/design/temp-tables.md §5); a denied temp DDL is 42501. nil ⇒ INHERIT AllowDDL's value
	// (back-compat: a session left as-is behaves as before, one gate governing all DDL). The
	// untrusted-scratch pattern is AllowDDL=false + AllowTempDDL=&true — private scratch tables only.
	AllowTempDDL *bool
	// TempBuffers is the per-session storage budget for session-local temp tables, in BYTES
	// (spec/design/temp-tables.md §7); 0 ⇒ unlimited; nil ⇒ the engine default (DefaultTempBuffers).
	// Bounds the RETAINED temp storage neither cost ceiling covers — an over-budget temp write aborts 54P03.
	TempBuffers *int
	// TimeZone is the session time zone (spec/design/session.md §6.2, timezones.md §9.4): the zone a
	// timestamptz is decomposed in by date_trunc / EXTRACT / the cross-family casts. "" ⇒ UTC. Accepts
	// UTC, a fixed ±HH:MM offset, or a named IANA zone a loaded JTZ bundle provides; an invalid value
	// here falls back to UTC at mint (the validated setter is Session.SetTimeZone — 22023).
	TimeZone string
}

// TxStatus is the session transaction status (spec/design/session.md §2.2) — PostgreSQL's three
// connection states made explicit on the session, derived from the open transaction: no
// transaction ⇒ Idle (autocommit); an open clean block ⇒ Open; an open block a statement aborted ⇒
// Failed (only ROLLBACK/COMMIT accepted, everything else 25P02).
type TxStatus int

const (
	TxIdle TxStatus = iota
	TxOpen
	TxFailed
)

func (s TxStatus) String() string {
	switch s {
	case TxOpen:
		return "Open"
	case TxFailed:
		return "Failed"
	default:
		return "Idle"
	}
}

func txStatusOf(tx *activeTx) TxStatus {
	switch {
	case tx == nil:
		return TxIdle
	case tx.failed:
		return TxFailed
	default:
		return TxOpen
	}
}

// sessionState is the per-connection SESSION envelope (spec/design/session.md §2.1/§2.4): the
// configured, stateful context a host runs statements through, un-fused from the committed storage on
// Engine. It owns the open transaction (the Idle/Open/Failed machine), the relocated handle settings,
// the entropy/clock seam, and the currval/lastval session state. An Engine holds one as its default
// session; the host-facing Session (shared.go) wraps an Engine and exposes this envelope, delegating
// its setters/getters here. (Pre-§2.4 this type was the exported `Session`; the convergence renamed
// it and made the per-caller handle the public `Session`.)
type sessionState struct {
	// tx is the open transaction, or nil under autocommit (transactions.md §4.1); a single-statement
	// autocommit write opens one implicitly for its duration. The Idle/Open/Failed status (session.md
	// §2.2) is derived from this (txStatusOf).
	tx *activeTx
	// maxCost is the execution-cost ceiling (CLAUDE.md §13; spec/design/api.md §8), or 0 for
	// unlimited. Bounds every statement run on this session: its Meter aborts 54P01 the instant
	// accrued cost reaches it. The primary guard for untrusted queries.
	maxCost int64
	// fkActionDepth bounds recursive generated referential-action statements (§6.6).
	fkActionDepth int
	// fkDeferredChecks holds inbound NO ACTION/RESTRICT probes until the outermost generated
	// referential-action closure reaches its fixed point (§6.6). It is empty between statements.
	fkDeferredChecks []fkDeferredCheck
	// lifetimeMaxCost is the per-session cumulative cost budget (spec/design/session.md §5.4), or 0
	// for unlimited. Bounds the whole session: the instant lifetimeTotal reaches it the in-flight
	// statement aborts 54P02, and once spent every further statement is rejected 54P02 at admission.
	// Sibling to maxCost (one statement).
	lifetimeMaxCost int64
	// lifetimeTotal points at the session's running CUMULATIVE execution cost (spec/design/session.md
	// §5.4) — the gauge LifetimeCost reads and the 54P02 budget bounds. A *int64 (heap) shared with
	// every statement Meter, which live-charges into it, so partial cost of an aborted statement
	// counts; a pointer so the activate() VALUE swap of the session keeps the same counter. SESSION
	// state, not snapshot state: it does NOT roll back when a transaction rolls back.
	lifetimeTotal *int64
	// cancel is the per-statement cancellation poll the ergonomic API arms for one statement
	// (spec/design/api.md §11.4): nil unless a host cancellation handle (Go context.Context, …) is
	// active. newMeter copies it into the statement's meter, whose Guard() polls it at each metering
	// checkpoint, so a flipped handle aborts a long-running statement (57014) — not only at the
	// cursor boundary. Set/cleared by engine.armCancel around a single statement (ergonomic.go); a
	// single atomic load on the hot path.
	cancel func() bool
	// maxSQLLength is the maximum input SQL length in bytes (CLAUDE.md §13; cost.md §7); 0 =
	// unlimited; default DefaultMaxSQLLength (1 MiB). Over-limit input is rejected 54000 at parse,
	// before lexing.
	maxSQLLength int
	// lockTimeoutMs bounds the shared cross-process writer gate; zero waits indefinitely.
	lockTimeoutMs uint64
	// workMem is the work-memory budget in bytes (spec/design/spill.md §2): the memory a blocking
	// operator (the ORDER BY external merge sort) holds before it spills. 0 = unlimited; default
	// DefaultWorkMem. Never changes what a query observes (spill.md §6); an in-memory database
	// ignores it.
	workMem int
	// seam is the entropy + clock seam for the uuid generators / clock functions (entropy.md): two
	// host-injectable functions (a random source + a clock), each nil ⇒ the platform primitive.
	// Tests inject SeededRandomSource + FixedClock (the # seed: / # clock: directives) for
	// byte-identical cross-core output.
	seam seam
	// sessionSeq is the SESSION currval state (sequences.md §6): the last value nextval/setval(…,true)
	// produced IN THIS SESSION for each sequence (lowercased name). NOT in the snapshot, NOT persisted.
	sessionSeq map[string]int64
	// sessionLastName is the SESSION lastval state (sequences.md §6): the lowercased name of the
	// sequence the most recent nextval (of any sequence) ran on — "" before the first nextval.
	sessionLastName string
	// pendingSeq is the per-STATEMENT running sequence advances (sequences.md §4); flushed into the
	// working snapshot on success, discarded on error (the transactional rollback of the advance, §5).
	pendingSeq map[string]*sequenceDef
	// pendingCurrval is the per-STATEMENT running currval updates → flushed into sessionSeq on success.
	pendingCurrval map[string]int64
	// pendingLastName is the per-STATEMENT running lastval update → flushed into sessionLastName.
	pendingLastName string
	// privileges is the authorization envelope (spec/design/session.md §5.3): the GRANT/REVOKE-style
	// per-object privilege model the host configures and the engine enforces (42501) at name
	// resolution. A fresh session is fully permissive (every table privilege, every function EXECUTE).
	privileges Privileges
	// allowDDL governs whether PERSISTENT DDL (CREATE/DROP/ALTER of persistent relations) is permitted
	// on this session (§5.3); a denied schema change is 42501. Default on. Its scope narrows with
	// temporary tables (temp-tables.md §5): allowTempDDL is the temp-scoped sibling gate.
	allowDDL bool
	// allowTempDDL governs whether session-local TEMPORARY-table DDL is permitted
	// (spec/design/temp-tables.md §5); a denied temp DDL is 42501. Resolved at session creation from
	// SessionOptions.AllowTempDDL (defaulting to allowDDL's value when unset).
	allowTempDDL bool
	// tempBuffers is the per-session temp-table storage budget in BYTES (temp-tables.md §7); 0 ⇒
	// unlimited. An over-budget temp write aborts 54P03.
	tempBuffers int
	// tempCommitted is the session-local temporary-table catalog + stores (spec/design/temp-tables.md
	// §2): a Snapshot holding only this session's temp tables, their stores, and their (UNIQUE) index
	// stores. NEVER serialized — only Engine.committed is written to the file, so a temp table makes
	// ZERO file writes. Private to this Session (it carries across the additional-session swap and is
	// invisible to other sessions), and dropped wholesale when the session is. Transactional like the
	// main snapshot: an open transaction clones it into activeTx.tempWorking, which a successful COMMIT
	// adopts back here and a ROLLBACK discards.
	tempCommitted *snapshot
	// vars are the session variables (spec/design/session.md §6.1): PostgreSQL's GUC model scoped to
	// the session — a string→string map (PG GUCs are all text) the host sets (SetVar/ResetVar) and SQL
	// reads with current_setting. Custom (dotted) names only in v1. SESSION state, not snapshot state:
	// it does NOT roll back with a transaction (PG SET SESSION). The map is a reference type, so the
	// activate() value swap keeps each session's own map (like the privilege envelope).
	vars map[string]string
	// timeZone is the resolved session time zone (spec/design/session.md §6.2, timezones.md §9.4): the
	// zone a timestamptz is decomposed in by date_trunc / EXTRACT / the cross-family casts. Resolved
	// once (from SessionOptions.TimeZone at mint, or SetTimeZone) to a cheap ZoneRef (UTC = Fixed 0);
	// the evaluator reads it via the active session. SESSION state (no storage effect).
	timeZone ZoneRef
	// readPin is the read pin for a data-modifying WITH statement (spec/design/writable-cte.md §2):
	// the single pre-statement snapshot every sub-statement reads, so the data-modifying CTEs and the
	// primary cannot observe each other's table writes (their writes still accumulate into the
	// transaction's working). Set by the writable-CTE orchestrator before the first sub-statement runs
	// and cleared when it finishes (success or error); nil for every other statement, where reads fall
	// through to working/committed as usual (readSnap).
	readPin *snapshot
}

// requireCustomVarName validates + canonicalizes a session-variable name (spec/design/session.md
// §6.1). A variable must be namespaced like a PostgreSQL custom GUC — a dotted name (myapp.tenant);
// a non-dotted name would be a built-in setting, and v1 exposes none through this map (the time_zone
// built-in is a separate slice), so it is 42704. Returns the case-folded (lowercase, PG GUC names are
// case-insensitive) map key.
func requireCustomVarName(name string) (string, error) {
	if strings.Contains(name, ".") {
		return strings.ToLower(name), nil
	}
	return "", newError(UndefinedObject, "unrecognized configuration parameter: "+name)
}

// newSession builds a fresh default session: no open transaction, default settings, empty state.
func newSession() sessionState {
	return newSessionWithOptions(SessionOptions{})
}

// newSessionWithOptions builds a session configured from opts (spec/design/session.md §2.1). A zero
// MaxSQLLength or WorkMem takes its default; the rest of the per-connection state starts empty.
func newSessionWithOptions(opts SessionOptions) sessionState {
	if opts.MaxSQLLength == 0 {
		opts.MaxSQLLength = DefaultMaxSQLLength
	}
	if opts.WorkMem == 0 {
		opts.WorkMem = defaultWorkMem
	}
	s := sessionState{
		maxCost:         opts.MaxCost,
		lifetimeMaxCost: opts.LifetimeMaxCost,
		lifetimeTotal:   new(int64),
		maxSQLLength:    opts.MaxSQLLength,
		lockTimeoutMs:   opts.LockTimeoutMs,
		workMem:         opts.WorkMem,
		privileges:      newPrivileges(),
		allowDDL:        true,
		tempBuffers:     defaultTempBuffers,
		tempCommitted:   newSnapshot(),
		vars:            map[string]string{},
	}
	if opts.DefaultPrivileges != nil {
		s.privileges.SetDefaultTable(*opts.DefaultPrivileges)
	}
	if opts.AllowDDL != nil {
		s.allowDDL = *opts.AllowDDL
	}
	// Back-compat default-inheritance (temp-tables.md §5): an unset AllowTempDDL takes allowDDL's value
	// (resolved above), so a session configured before temp tables existed behaves exactly as it did
	// (one gate governing all DDL).
	s.allowTempDDL = s.allowDDL
	if opts.AllowTempDDL != nil {
		s.allowTempDDL = *opts.AllowTempDDL
	}
	if opts.TempBuffers != nil {
		s.tempBuffers = *opts.TempBuffers
	}
	// Resolve the configured zone once; an invalid value falls back to UTC at mint (the validated
	// path is SetTimeZone, which surfaces 22023). timezones.md §9.4.
	tzName := opts.TimeZone
	if tzName == "" {
		tzName = "UTC"
	}
	if zr, ok := ResolveZone(tzName); ok {
		s.timeZone = zr
	} else {
		s.timeZone = ZoneRef{Fixed: true, Off: 0}
	}
	return s
}

// SetTimeZone sets the session time zone (spec/design/session.md §6.2, timezones.md §9.4): the zone a
// timestamptz is decomposed in. Accepts UTC, a fixed ±HH:MM offset, or a named IANA zone a loaded JTZ
// bundle provides; a name no bundle provides (and not a built-in) is 22023, the value unchanged.
func (s *sessionState) SetTimeZone(zone string) error {
	zr, ok := ResolveZone(zone)
	if !ok {
		return newError(InvalidParameterValue, fmt.Sprintf("time zone %q not recognized", zone))
	}
	s.timeZone = zr
	return nil
}

// activeTx is an open transaction (spec/design/transactions.md §4.2). writable is the access mode
// (READ WRITE vs READ ONLY — a write in a READ ONLY block is 25006); failed marks an aborted block
// (every later statement but COMMIT/ROLLBACK is 25P02 — §6). working is the transaction's snapshot:
// a writable tx mutates it in place and publishes it at commit; a read-only tx reads it unchanged
// (read-your-snapshot, §4.3). committed is untouched until commit, so ROLLBACK just drops this.
type activeTx struct {
	writable bool
	failed   bool
	working  *snapshot
	// savedSessionSeq / savedSessionLastName capture the handle's currval/lastval session state
	// (spec/design/sequences.md §6) when this transaction opened. A nextval/setval inside the block
	// updates the handle's session state per-statement (so an in-block currval sees its own
	// advance), but those updates must ROLL BACK with the transaction (§5) — so ROLLBACK (and a
	// failed/read-only COMMIT) restores these, while a successful COMMIT keeps the advanced state.
	savedSessionSeq      map[string]int64
	savedSessionLastName string
	// tempWorking is the transaction's working copy of the session's temp-table snapshot
	// (spec/design/temp-tables.md §5): cloned from Session.tempCommitted at tx open (cheap — persistent
	// stores clone O(1)), mutated by temp DDL/DML, adopted back into tempCommitted on a successful COMMIT
	// and discarded on ROLLBACK. The temp analogue of working, kept SEPARATE so it is never serialized.
	tempWorking *snapshot
	// mainDirty is whether this transaction mutated the MAIN (persistent) snapshot — set by
	// (*Engine).workingMut. Drives the commit's persist decision so a transaction that touched ONLY
	// temp tables makes zero file writes (temp-tables.md §2).
	mainDirty bool
	// tempDirty is whether this transaction mutated the SESSION-LOCAL TEMP snapshot — set by the temp
	// write funnels. With mainDirty it decides whether COMMIT persists the main image (a pure-temp
	// commit skips it; an empty block still persists, preserving prior behavior).
	tempDirty bool
	// attachWorking is the transaction's working copy of a host-attached database's snapshot
	// (attached-databases.md §5), keyed by lowercased attachment name — the attachment analogue of
	// tempWorking. Cloned lazily from engine.attachedCommitted[name] on the first write to that
	// attachment (attachWriteSnap), so a read-only cross-attachment query allocates nothing here.
	// Adopted into engine.attachedCommitted + persisted+published on a successful COMMIT, discarded on
	// ROLLBACK. nil until an attachment is written.
	attachWorking map[string]*snapshot
	// attachDirty records which attachments this transaction mutated (lowercased name → true), the
	// per-attachment analogue of mainDirty/tempDirty — the set the commit persists + publishes.
	attachDirty map[string]bool
}

// NewEngine builds an empty in-memory database.
func newEngine() *engine {
	return &engine{committed: newSnapshot(), pageSize: DefaultPageSize, session: newSession()}
}

// WithPageSize returns an in-memory handle that serializes at pageSize. The page-backed B-tree's
// fan-out tracks the page size (spec/fileformat/format.md), so the in-memory tree must be built at
// the size it will serialize to — this builds fixtures / tests a non-default page size; a normal
// in-memory database uses NewEngine (the default page size).
func withPageSize(pageSize uint32) *engine {
	return &engine{committed: newSnapshot(), pageSize: pageSize, session: newSession()}
}

// readSnap is the snapshot a read sees: the read pin if one is set (a data-modifying WITH statement
// pins the pre-statement snapshot so every sub-statement reads it — writable-cte.md §2), else the
// open transaction's working (read-your-writes for a writable tx; the pinned snapshot for a
// read-only tx), else the committed snapshot.
func (db *engine) readSnap() *snapshot {
	if db.session.readPin != nil {
		return db.session.readPin
	}
	if db.session.tx != nil {
		return db.session.tx.working
	}
	return db.committed
}

// columnCollations resolves each column's frozen collation (Column.Collation, the name) to its
// baked table, indexed by column ordinal — nil for a C / non-text column (the fast path). The key
// encoders (§2.12) consult colls[ci] to pick a text column's key form.
func (db *engine) ensureCollationsWritable(columns []catColumn) error {
	// Refuse a WRITE that would maintain a collated B-tree under a version-skewed collation (the
	// slice-2d verdict, spec/design/collation.md §12/§14): if any of columns carries a collation the
	// file pinned to a different (unicode, cldr) than the loaded bundle provides, an
	// insert/update/delete/index-build would mix two orderings in one tree and corrupt it, so the
	// whole table is read-only until a REINDEX migration (deferred) rebuilds + re-pins it. XX002,
	// naming the collation + both versions. Reads never call this — they recompute against the loaded
	// table (the heap-scan fallback, compatibility.md §8). Per-table granularity: one skewed column
	// collation makes the table read-only (finer per-index gating is a follow-on).
	snap := db.readSnap()
	for i := range columns {
		if columns[i].Collation == "" {
			continue
		}
		if fu, fc, lu, lc, skewed := snap.collationSkew(columns[i].Collation); skewed {
			return newError(CollationVersionMismatch, fmt.Sprintf(
				"collation %q version mismatch: this database's keys were built under %s/%s but the "+
					"loaded bundle is %s/%s; tables using it are read-only until a REINDEX migration rebuilds them",
				columns[i].Collation, fu, fc, lu, lc,
			))
		}
	}
	return nil
}

func (db *engine) columnCollations(columns []catColumn) []*Collation {
	snap := db.readSnap()
	out := make([]*Collation, len(columns))
	for i := range columns {
		if columns[i].Collation != "" {
			out[i] = snap.resolveCollation(columns[i].Collation)
		}
	}
	return out
}

// relationSnap is the relation-owning read snapshot for planner/ANALYZE metadata. A bare name uses
// the same temp-first rule as lkpTableScoped/lkpStoreScoped.
func (db *engine) relationSnap(scope *string, table string) *snapshot {
	if scope != nil {
		return db.snapForScope(*scope)
	}
	if _, ok := db.tempSnap().table(table); ok {
		return db.tempSnap()
	}
	return db.readSnap()
}

func (db *engine) columnCollationsScoped(scope *string, table string, columns []catColumn) []*Collation {
	snap := db.relationSnap(scope, table)
	out := make([]*Collation, len(columns))
	for i := range columns {
		if columns[i].Collation != "" {
			out[i] = snap.resolveCollation(columns[i].Collation)
		}
	}
	return out
}

func (db *engine) columnStatisticsScoped(scope *string, table string, column int) *columnStatistics {
	snap := db.relationSnap(scope, table)
	definition := snap.tables[strings.ToLower(table)]
	if definition == nil || column < 0 || column >= len(definition.Columns) {
		return nil
	}
	if name := definition.Columns[column].Collation; name != "" {
		if _, _, _, _, skewed := snap.collationSkew(name); skewed {
			return nil
		}
	}
	return snap.columnStatistics(table, column)
}

// collatedTextKey is the order-preserving key body for a text value (encoding.md §2.12): the
// collation's UCA sort key when coll is non-nil (a non-C collated column), else the C
// text-terminated-escape body (§2.4). The sort key can fail (0A000) on a code point the collation
// does not map — propagated, so a collated INSERT of an unmapped string aborts the write.
func collatedTextKey(coll *Collation, s string) ([]byte, error) {
	if coll != nil {
		return sortKey(coll, s)
	}
	return encodeTerminated([]byte(s)), nil
}

// tempDomainPaging returns the MemoryBlockStore paging context for the session-local temp domain
// (temp-tables.md §6), lazily creating the domain's storage identity (newTempStorage — a private
// in-RAM store + pinned pool with within-session compaction on) on first use.
func (db *engine) tempDomainPaging() *sharedPaging {
	if db.tempStorage == nil {
		db.tempStorage = newTempStorage(db.pageSize)
	}
	return db.tempStorage.paging
}

// working is the snapshot a write mutates — the open transaction's working. A write only ever runs
// with a transaction open (autocommit opens one implicitly), so tx is non-nil here.
func (db *engine) working() *snapshot {
	// Mark the main image dirty so the commit knows to persist it; a temp-only transaction never
	// reaches here (it writes via the temp funnels) and so makes zero file writes (temp-tables.md §2).
	db.session.tx.mainDirty = true
	return db.session.tx.working
}

// tempSnap is the session's temp-table snapshot for READS (spec/design/temp-tables.md §2): the open
// transaction's tempWorking, else the session's committed temp state. The temp analogue of readSnap
// (it does not consult readPin — a writable-CTE pins only the main snapshot).
func (db *engine) tempSnap() *snapshot {
	if db.session.tx != nil {
		return db.session.tx.tempWorking
	}
	return db.session.tempCommitted
}

// isTempTable reports whether name resolves to a SESSION-LOCAL temporary table in the visible temp
// snapshot (spec/design/temp-tables.md §3). Preclude-overlaps guarantees a name is temp XOR
// persistent, so this is the routing predicate the table/store funnels use.
func (db *engine) isTempTable(name string) bool {
	_, ok := db.tempSnap().table(name)
	return ok
}

// checkTableQualifier validates an optional database qualifier on a table reference against the
// implicit scope (spec/design/attached-databases.md §3, Slice 1a). A qualified name reaches a specific
// database: `main` (the file / persistent database) or `temp` (the session-local domain) — the two
// reserved implicit qualifiers this slice recognizes; a host-attached database arrives in Slice 1b, so
// any other qualifier is 42P01 "database … is not attached". Because jed precludes overlaps (a name is
// temp XOR persistent within a session, §3), a valid qualifier resolves to the SAME store the bare name
// would, so this is a VALIDATION GATE, not a routing change: it asserts the named relation lives in the
// claimed database (else 42P01), and the downstream temp-first funnel then resolves it to the matching
// scope. A nil qualifier (a bare, implicit-scope name) always passes. The qualifier is matched
// case-insensitively (unquoted identifiers fold to lower case).
func (db *engine) checkTableQualifier(qualifier *string, name string) error {
	if qualifier == nil {
		return nil
	}
	switch strings.ToLower(*qualifier) {
	case "temp":
		if !db.isTempTable(name) {
			return newError(UndefinedTable, `relation "temp.`+name+`" does not exist`)
		}
	case "main":
		if _, ok := db.readSnap().table(name); !ok {
			return newError(UndefinedTable, `relation "main.`+name+`" does not exist`)
		}
	default:
		snap := db.attachReadSnap(strings.ToLower(*qualifier))
		if snap == nil {
			return newError(UndefinedTable, `database "`+*qualifier+`" is not attached`)
		}
		if _, ok := snap.table(name); !ok {
			return newError(UndefinedTable, `relation "`+*qualifier+`.`+name+`" does not exist`)
		}
	}
	return nil
}

// checkAttachmentWritable rejects a WRITE (DML or DDL) targeting a READ-ONLY host attachment with 25006
// (attached-databases.md §4), before any I/O. A nil scope, or `main`/`temp` (never read-only via a
// qualifier — the read-only handle path is separate), or a read-write attachment passes. Unknown
// attachments are caught by the qualifier gate, so this only inspects the attachment's mode.
func (db *engine) checkAttachmentWritable(scope *string) error {
	if scope == nil || db.core == nil {
		return nil
	}
	name := strings.ToLower(*scope)
	if name == "main" || name == "temp" {
		return nil
	}
	if att := db.core.attachment(name); att != nil && att.mode == attachReadOnly {
		return newError(ReadOnlySqlTransaction,
			`cannot write to read-only database "`+*scope+`"`)
	}
	return nil
}

// isReservedScope reports whether a database qualifier names one of the two implicit reserved scopes
// `main` / `temp` (attached-databases.md §3), which resolve to the SAME store the bare name would — so
// a qualified reference to one keeps every existing fast path. A nil qualifier (a bare implicit-scope
// name) counts as reserved for routing: it too keeps the temp-first funnels.
func isReservedScope(q *string) bool {
	if q == nil {
		return true
	}
	switch strings.ToLower(*q) {
	case "main", "temp":
		return true
	}
	return false
}

// isAttachmentScope reports whether a database qualifier names a HOST-ATTACHED database (not nil, not
// reserved main/temp) — the case that routes to the attachment registry rather than the implicit
// temp-first funnels, and the case that gates off index-bound pushdown / cross-scope catalog lookups
// this slice (attached-databases.md §8).
func isAttachmentScope(q *string) bool { return !isReservedScope(q) }

// isAttachment reports whether this relation targets a host-attached database (attached-databases.md
// §3) rather than the implicit main/temp scope. Index/PK/GiST/GIN bound pushdown is gated off for
// attachment relations this slice: the bounded-scan exec path resolves index stores through the
// UNSCOPED lkpIndexStore funnel, so an attachment relation must full-scan (correct, perf-only — index
// acceleration for attachments is a Slice 1b perf follow-on). A full scan reads the scoped store.
func (rel scopeRel) isAttachment() bool { return isAttachmentScope(rel.db) }

// attachReadSnap returns the READ snapshot of a host-attached database (attached-databases.md §5) — the
// transaction's working clone if this tx wrote it, else the pinned committed root (attachedCommitted).
// nil when no attachment is named `name` (the caller raises 42P01). name is expected lowercased.
func (db *engine) attachReadSnap(name string) *snapshot {
	if db.session.tx != nil {
		if ws := db.session.tx.attachWorking[name]; ws != nil {
			return ws
		}
	}
	return db.attachedCommitted[name]
}

// attachWriteSnap returns the WRITE snapshot of a host-attached database, cloning the pinned committed
// root into the transaction's per-attachment working set on first write and marking it dirty (the
// attachment analogue of working()/tempWorking). Returns nil if the attachment is unknown (unreachable
// after the qualifier gate). name is expected lowercased.
func (db *engine) attachWriteSnap(name string) *snapshot {
	tx := db.session.tx
	if tx.attachWorking == nil {
		tx.attachWorking = make(map[string]*snapshot)
		tx.attachDirty = make(map[string]bool)
	}
	if ws := tx.attachWorking[name]; ws != nil {
		tx.attachDirty[name] = true
		return ws
	}
	base := db.attachedCommitted[name]
	if base == nil {
		return nil
	}
	ws := base.clone()
	tx.attachWorking[name] = ws
	tx.attachDirty[name] = true
	return ws
}

// attachPageSize is the page size of a host attachment's OWN page space (attached-databases.md §2) —
// used to build its NEW stores (CREATE TABLE / CREATE INDEX) at the size its commit serializes to. A
// FILE attachment carries its own page size, baked into the file, which may differ from main's; an
// in-memory attachment matches main. The attachment is known to exist (the qualifier gate passed).
func (db *engine) attachPageSize(name string) uint32 {
	return db.core.attachment(name).storage.pageSize
}

// attachReadView returns the current READ view of every attached database — the transaction's working
// clone where this tx wrote it, else the pinned committed root — as one frozen map. Used to freeze a
// snapshotEngine's attachment view (whose own tx is nil, so it reads straight from this map). Returns
// attachedCommitted directly when no attachment has been written this tx (the common case, no alloc).
func (db *engine) attachReadView() map[string]*snapshot {
	tx := db.session.tx
	if tx == nil || len(tx.attachWorking) == 0 {
		return db.attachedCommitted
	}
	view := make(map[string]*snapshot, len(db.attachedCommitted)+len(tx.attachWorking))
	for k, v := range db.attachedCommitted {
		view[k] = v
	}
	for k, v := range tx.attachWorking {
		view[k] = v
	}
	return view
}

// snapForScope returns the READ snapshot for an explicit database qualifier (attached-databases.md §3):
// `main` / `temp` / a host attachment. Used only when scope != nil; a nil scope keeps the bare
// temp-first funnels (a name is temp XOR persistent). nil for an unknown attachment (the qualifier gate
// already raised 42P01, so unreachable in practice).
//
// This funnel IS where Slice 1c's "temp is an implicit in-memory attachment" reframe is realized
// (attached-databases.md §6): `temp`, `main`, and every host attachment resolve through one
// scoped-routing path, so a temp table is a citizen of the same mechanism an attachment is. What stays
// deliberately distinct is temp's *lifecycle* — it is SESSION-SCOPED (tempSnap reads session-private
// state; commit lands on session.tempCommitted with no cross-session roots publish; its reclamation
// watermark is db.openStreams, not the Database-wide live registry). That divergence is correct, not a
// gap: relocating temp into the Database-scoped attachment registry would re-share it across sessions
// (what Slice 0 removed). So temp routes like an attachment here but keeps its own home.
func (db *engine) snapForScope(scope string) *snapshot {
	switch strings.ToLower(scope) {
	case "temp":
		return db.tempSnap()
	case "main":
		return db.readSnap()
	default:
		return db.attachReadSnap(strings.ToLower(scope))
	}
}

// lkpTableScoped resolves a table's catalog entry honoring an explicit database qualifier
// (attached-databases.md §3); a nil scope keeps the bare temp-first walk.
func (db *engine) lkpTableScoped(scope *string, name string) (*catTable, bool) {
	if scope == nil {
		return db.lkpTable(name)
	}
	snap := db.snapForScope(*scope)
	if snap == nil {
		return nil, false
	}
	return snap.table(name)
}

// lkpStoreScoped resolves a table's READ store honoring an explicit database qualifier; nil scope keeps
// the bare temp-first funnel.
func (db *engine) lkpStoreScoped(scope *string, name string) *tableStore {
	if scope == nil {
		return db.lkpStore(name)
	}
	snap := db.snapForScope(*scope)
	if snap == nil {
		return nil
	}
	return snap.store(name)
}

// writeStoreScoped resolves a table's WRITE store honoring an explicit database qualifier, marking the
// right domain dirty (main / temp / the attachment); nil scope keeps the bare temp-first funnel.
func (db *engine) writeStoreScoped(scope *string, name string) *tableStore {
	if scope == nil {
		return db.writeStore(name)
	}
	switch strings.ToLower(*scope) {
	case "temp":
		db.session.tx.tempDirty = true
		return db.session.tx.tempWorking.store(name)
	case "main":
		return db.working().store(name)
	default:
		ws := db.attachWriteSnap(strings.ToLower(*scope))
		if ws == nil {
			return nil
		}
		return ws.store(name)
	}
}

// markEstimatorMutation advances the target persistent relation's transactional cache revision once
// for this top-level statement. Temp relations remain uncacheable and need no signature. Calling it
// only after phase-2 DML changed at least one row preserves no-op statements' revision.
func (db *engine) markEstimatorMutation(scope *string, table string) {
	database := "main"
	if scope == nil {
		if db.isTempTable(table) {
			db.session.tx.tempDirty = true
			db.session.tx.tempWorking.markStatisticsStale(table)
			return
		}
	} else {
		database = strings.ToLower(*scope)
		if database == "temp" {
			db.session.tx.tempDirty = true
			db.session.tx.tempWorking.markStatisticsStale(table)
			return
		}
	}
	key := estimatorTouchedRelation{database: database, table: strings.ToLower(table)}
	if db.estimatorTouched == nil {
		db.estimatorTouched = make(map[estimatorTouchedRelation]struct{})
	}
	if _, seen := db.estimatorTouched[key]; seen {
		return
	}
	db.estimatorTouched[key] = struct{}{}
	if database == "main" {
		snap := db.working()
		snap.bumpEstimatorRevision(table)
		snap.markStatisticsStale(table)
		return
	}
	if snap := db.attachWriteSnap(database); snap != nil {
		snap.bumpEstimatorRevision(table)
		snap.markStatisticsStale(table)
	}
}

// lkpIndexStoreScoped / writeIndexStoreScoped are the index-store analogues of lkpStoreScoped /
// writeStoreScoped: an index belongs to the same database as its table, so the DML target's scope
// routes them. nil scope keeps the bare temp-first funnel.
func (db *engine) lkpIndexStoreScoped(scope *string, nameKey string) *tableStore {
	if scope == nil {
		return db.lkpIndexStore(nameKey)
	}
	snap := db.snapForScope(*scope)
	if snap == nil {
		return nil
	}
	return snap.indexStore(nameKey)
}

func (db *engine) writeIndexStoreScoped(scope *string, nameKey string) *tableStore {
	if scope == nil {
		return db.writeIndexStore(nameKey)
	}
	switch strings.ToLower(*scope) {
	case "temp":
		db.session.tx.tempDirty = true
		return db.session.tx.tempWorking.indexStore(nameKey)
	case "main":
		return db.working().indexStore(nameKey)
	default:
		ws := db.attachWriteSnap(strings.ToLower(*scope))
		if ws == nil {
			return nil
		}
		return ws.indexStore(nameKey)
	}
}

// compositeDependentAny is the DROP TYPE … RESTRICT dependency check across every visible scope
// (spec/design/temp-tables.md §8): the main image (tables + composite fields), then the visible
// session-local temp snapshot (its tables). A composite type is always persistent, but a TEMP table
// column may reference it, so dropping the type while such a temp table exists is 2BP01 — matching the
// persistent case (PostgreSQL blocks the drop). A session sees only its own session-local temp tables
// (another session's private temp table is invisible by design — and its resolved ColType is
// self-contained, so it keeps working regardless).
func (db *engine) compositeDependentAny(name string) (string, bool) {
	if dep, ok := db.readSnap().compositeDependent(name); ok {
		return dep, true
	}
	return db.tempSnap().compositeDependent(name)
}

// isTempIndex reports whether name is a secondary index on a SESSION-LOCAL temp table
// (spec/design/temp-tables.md §8) — the index analogue of isTempTable, used to gate (allowTempDDL)
// and route a DROP INDEX of a temp index. Preclude-overlaps keeps an index name in one scope.
func (db *engine) isTempIndex(name string) bool {
	_, _, ok := db.tempSnap().findIndex(name)
	return ok
}

// sequence resolves a sequence by name along the resolution walk session-local → persistent
// (spec/design/sequences.md + temp-tables.md §8). Preclude-overlaps keeps a name in at most one scope
// (the shared relation namespace), so this is just "where the sequence lives". Every sequence READ
// (nextval/currval/setval resolution, DROP/ALTER SEQUENCE) goes through here, so a serial/IDENTITY
// column's OWNED temp sequence resolves exactly like a persistent one.
func (db *engine) sequence(name string) *sequenceDef {
	if s := db.tempSnap().sequence(name); s != nil {
		return s
	}
	return db.readSnap().sequence(name)
}

// isTempSequence reports whether name is a sequence in the SESSION-LOCAL temp snapshot
// (temp-tables.md §8) — the sequence analogue of isTempTable. A temp sequence only ever arises from a
// serial/IDENTITY temp column (standalone CREATE SEQUENCE is always persistent), so it is always owned.
func (db *engine) isTempSequence(name string) bool {
	return db.tempSnap().sequence(name) != nil
}

// anyTempSequence reports whether any name in a DROP SEQUENCE list is a session-local temp sequence —
// the gate classifier for a temp DROP SEQUENCE (§5/§8).
func (db *engine) anyTempSequence(names []string) bool {
	for _, n := range names {
		if db.isTempSequence(n) {
			return true
		}
	}
	return false
}

// anyTempTable reports whether any name in a multi-table DROP TABLE resolves to a session-local temp
// table — the DDL capability gate's classification of a mixed list (temp-tables.md §5): if any target
// is temp-scoped the whole statement is gated by the temp-DDL grant.
func (db *engine) anyTempTable(names []string) bool {
	for _, n := range names {
		if db.isTempTable(n) {
			return true
		}
	}
	return false
}

// putSequenceRouted stages a sequence def into whichever scope currently owns its name (flagging the
// matching dirty bit): session-local temp, else the main working set. A serial/IDENTITY temp column's
// owned sequence advances (nextval flush) into its temp snapshot — like the table's rows, zero file
// writes (temp-tables.md §2); a brand-new persistent sequence is absent from the temp scope and lands
// in the main image.
func (db *engine) putSequenceRouted(def *sequenceDef) {
	if db.isTempSequence(def.Name) {
		db.session.tx.tempDirty = true
		db.session.tx.tempWorking.putSequence(def)
	} else {
		db.working().putSequence(def)
	}
}

// removeSequenceRouted removes a sequence from whichever scope owns its name (the routed analogue of
// putSequenceRouted). Used by DROP SEQUENCE and DROP TABLE's owned-sequence auto-drop.
func (db *engine) removeSequenceRouted(name string) {
	key := strings.ToLower(name)
	if db.isTempSequence(name) {
		db.session.tx.tempDirty = true
		db.session.tx.tempWorking.removeSequence(key)
	} else {
		db.working().removeSequence(key)
	}
}

// setColumnDefaultExprRouted rewrites a column's stored DEFAULT expression in whichever scope owns the
// table — the routed analogue used by ALTER SEQUENCE … RENAME of an owned sequence (temp-tables.md §8),
// so a renamed owned TEMP sequence's nextval default is rewritten in the temp snapshot.
func (db *engine) setColumnDefaultExprRouted(tableKey string, column int, de *defaultExprDef) {
	if db.isTempTable(tableKey) {
		db.session.tx.tempDirty = true
		db.session.tx.tempWorking.setColumnDefaultExpr(tableKey, column, de)
	} else {
		db.working().setColumnDefaultExpr(tableKey, column, de)
	}
}

// lkpTable resolves a table by name along the resolution walk session-local → persistent
// (temp-tables.md §3). Preclude-overlaps keeps a name in at most one scope, so this is just "where it lives".
func (db *engine) lkpTable(name string) (*catTable, bool) {
	if t, ok := db.tempSnap().table(name); ok {
		return t, true
	}
	return db.readSnap().table(name)
}

// lkpStore returns a table's store for READS, routing by the resolution walk (session-local temp →
// visible main snapshot — temp-tables.md §2). No dirty flag — reads never persist.
func (db *engine) lkpStore(name string) *tableStore {
	if db.isTempTable(name) {
		return db.tempSnap().store(name)
	}
	return db.readSnap().store(name)
}

// writeStore returns a table's store for MUTATION, routing a session-local temp write to tempWorking
// (flagging tempDirty) and a persistent write to working (which flags mainDirty) — so a pure-temp
// transaction leaves the main image untouched (temp-tables.md §2).
func (db *engine) writeStore(name string) *tableStore {
	if db.isTempTable(name) {
		db.session.tx.tempDirty = true
		return db.session.tx.tempWorking.store(name)
	}
	return db.working().store(name)
}

// lkpIndexStore returns a secondary index's store for READS, walking session-local → main
// (temp-tables.md §8).
func (db *engine) lkpIndexStore(nameKey string) *tableStore {
	if db.tempSnap().hasIndexStore(nameKey) {
		return db.tempSnap().indexStore(nameKey)
	}
	return db.readSnap().indexStore(nameKey)
}

// writeIndexStore returns a secondary index's store for MUTATION, walking session-local → main
// (flagging the matching dirty bit).
func (db *engine) writeIndexStore(nameKey string) *tableStore {
	if db.tempSnap().hasIndexStore(nameKey) {
		db.session.tx.tempDirty = true
		return db.session.tx.tempWorking.indexStore(nameKey)
	}
	return db.working().indexStore(nameKey)
}

// InTransaction reports whether an explicit transaction block is currently open
// (spec/design/transactions.md §4.2). False under autocommit. Used by the host Transaction surface.
func (db *engine) InTransaction() bool { return db.session.tx != nil }

// Txid is the monotonic commit counter (spec/design/api.md §2): the committed snapshot's version.
func (db *engine) Txid() uint64 { return db.committed.txid }

// OldestLiveTxid is the oldest still-live snapshot's txid (spec/design/transactions.md §8) — the
// Phase-6 free-list reclamation gate. Single-handle (P5.3a) it is trivially the committed txid; the
// P5.3b shared read snapshots make it meaningful.
func (db *engine) OldestLiveTxid() uint64 { return db.committed.txid }

// PageSize is the page size this database serializes with (spec/design/api.md §2).
func (db *engine) PageSize() uint32 { return db.pageSize }

// PageCount is the committed logical page high-water — the number of pages the on-disk image
// references (the count the meta records, format.md), the size an incremental commit extends at
// (spec/fileformat/format.md *Reclamation*). It is not the physical file length, which the chunked
// preallocation (pager.go, spec/design/pager.md §7) runs ahead of with trailing zero slack. 0 for a
// fresh in-memory database.
func (db *engine) PageCount() uint32 { return db.pageCount }

// Path is the backing file path, or "" for an in-memory database.
func (db *engine) Path() string { return db.path }

// SetMaxCost sets the execution-cost ceiling for statements run on this handle (CLAUDE.md §13;
// spec/design/api.md §8). A positive limit bounds every subsequent statement: it aborts with
// 54P01 the instant accrued cost reaches limit (spec/design/cost.md §6). limit <= 0 (the default)
// is unlimited. The primary guard for safely evaluating untrusted, user-supplied queries; a handle
// setting, not stored in the file.
func (db *engine) SetMaxCost(limit int64) { db.session.maxCost = limit }

// SetLifetimeMaxCost sets the PER-SESSION cumulative cost budget on the default session
// (spec/design/session.md §5.4); limit <= 0 (the default) is unlimited. Where max_cost bounds one
// statement (54P01), this bounds the whole session: the instant the session's running cumulative
// cost reaches limit the in-flight statement aborts 54P02, and once spent every further statement is
// rejected 54P02 at admission. The multi-tenant / untrusted-host gate atop max_cost; a handle
// setting, not stored in the file.
func (db *engine) SetLifetimeMaxCost(limit int64) { db.session.lifetimeMaxCost = limit }

// LifetimeMaxCost is the default session's per-session cumulative cost budget (0 ⇒ unlimited).
func (db *engine) LifetimeMaxCost() int64 { return db.session.lifetimeMaxCost }

// LifetimeCost is the default session's running CUMULATIVE execution cost so far
// (spec/design/session.md §5.4) — the gauge the lifetime_max_cost budget bounds. Tracked even when
// unlimited; survives a transaction rollback (session state, not snapshot state).
func (db *engine) LifetimeCost() int64 { return *db.session.lifetimeTotal }

// SetDefaultPrivileges replaces the default session's default table-privilege set — the
// GRANT … ON ALL TABLES default (spec/design/session.md §5.3). PrivSetEmpty.With(PrivSelect) makes
// the session read-only (a write resolves to 42501). A handle setting, not stored in the file.
func (db *engine) SetDefaultPrivileges(privs PrivilegeSet) {
	db.session.privileges.SetDefaultTable(privs)
}

// Grant grants privs on a specific object (table or function) on the default session, beyond the
// default (§5.3).
func (db *engine) Grant(privs PrivilegeSet, object string) {
	db.session.privileges.Grant(privs, object)
}

// Revoke revokes privs from a specific object on the default session (revoke wins over grant and the
// default, §5.3).
func (db *engine) Revoke(privs PrivilegeSet, object string) {
	db.session.privileges.Revoke(privs, object)
}

// ResetPrivileges resets the default session's authorization envelope to fully permissive — every
// table privilege, no per-object delta, DDL allowed (§5.3). The conformance harness calls this before
// each record so a # default_privileges: / # grant: / # revoke: / # allow_ddl: directive never leaks
// past the record it decorates.
func (db *engine) ResetPrivileges() {
	db.session.privileges = newPrivileges()
	db.session.allowDDL = true
	// The temp-DDL gate is part of the authorization envelope (temp-tables.md §5); reset it with the
	// rest so a # allow_temp_ddl: directive never leaks past its record. Default-inherit allowDDL=true.
	db.session.allowTempDDL = true
}

// Privileges is read-only access to the default session's authorization envelope (§5.3).
func (db *engine) Privileges() *Privileges { return &db.session.privileges }

// SetAllowDDL sets whether DDL is permitted on the default session (§5.3); a denied schema change is
// 42501.
func (db *engine) SetAllowDDL(allow bool) { db.session.allowDDL = allow }

// AllowDDL reports whether DDL is permitted on the default session.
func (db *engine) AllowDDL() bool { return db.session.allowDDL }

// SetAllowTempDDL sets whether session-local temporary-table DDL is permitted on the default session
// (spec/design/temp-tables.md §5) — the temp-scoped split of AllowDDL; a denied temp DDL is 42501.
func (db *engine) SetAllowTempDDL(allow bool) { db.session.allowTempDDL = allow }

// AllowTempDDL reports whether session-local temporary-table DDL is permitted on the default session.
func (db *engine) AllowTempDDL() bool { return db.session.allowTempDDL }

// SetTempBuffers sets the default session's per-session temp-table storage budget in BYTES
// (spec/design/temp-tables.md §7); 0 ⇒ unlimited. An over-budget temp write aborts 54P03.
func (db *engine) SetTempBuffers(bytes int) { db.session.tempBuffers = bytes }

// TempBuffers reports the default session's per-session temp-table storage budget (0 ⇒ unlimited).
func (db *engine) TempBuffers() int { return db.session.tempBuffers }

// SetVar sets a session variable on the default session (spec/design/session.md §6.1). Custom
// variables must be namespaced (a dotted name); a non-dotted name is 42704. Read it back in SQL with
// current_setting('name'[, missing_ok]).
func (db *engine) SetVar(name, value string) error { return db.session.SetVar(name, value) }

// ResetVar clears a session variable on the default session (§6.1); a non-dotted name is 42704.
func (db *engine) ResetVar(name string) error { return db.session.ResetVar(name) }

// Var reads a session variable's value on the default session (§6.1); ok is false if it is not set.
func (db *engine) Var(name string) (string, bool) { return db.session.Var(name) }

// ResetVars clears every session variable on the default session (§6.1) — PostgreSQL's RESET ALL for
// the variable map (also the conformance harness # set: reset hook).
func (db *engine) ResetVars() { db.session.ResetVars() }

// SetTimeZone sets the time zone on the default session (spec/design/session.md §6.2, timezones.md
// §9.4): the zone a timestamptz is decomposed in by date_trunc / EXTRACT / the cross-family casts.
// Accepts UTC, a fixed ±HH:MM offset, or a named IANA zone a loaded JTZ bundle provides; else 22023.
func (db *engine) SetTimeZone(zone string) error { return db.session.SetTimeZone(zone) }

// SetMaxSQLLength sets the maximum input SQL length, in bytes, accepted on this handle (CLAUDE.md
// §13; spec/design/api.md §8). A statement whose text exceeds bytes is rejected with 54000 at
// parse entry, before lexing — the §13 input-size gate (cost.md §7). 0 is unlimited (a trusted
// caller's opt-out); the default is DefaultMaxSQLLength (1 MiB). A handle setting, not stored in
// the file (mirrors SetMaxCost).
func (db *engine) SetMaxSQLLength(bytes int) { db.session.maxSQLLength = bytes }

// MaxSQLLength is the current input-SQL byte limit (0 = unlimited). See SetMaxSQLLength.
func (db *engine) MaxSQLLength() int { return db.session.maxSQLLength }

// parse parses one statement from sql, first enforcing this handle's maxSQLLength input-size limit
// (CLAUDE.md §13; spec/design/api.md §8, cost.md §7). The §13 input-size gate: an over-limit
// statement is rejected with 54000 before lexing, so unbounded untrusted input cannot exhaust
// parse memory/CPU (the cost meter cannot catch this — parsing precedes metering). maxSQLLength
// == 0 is unlimited. Every handle-bound parse path routes through here (queryValues/Exec/
// Prepare/the session handles), so the per-handle limit has no hole. The byte length is
// len(sql) (Go strings are UTF-8).
func (db *engine) parse(sql string) (statement, error) {
	if db.session.maxSQLLength > 0 && len(sql) > db.session.maxSQLLength {
		return statement{}, newError(ProgramLimitExceeded, fmt.Sprintf("SQL statement exceeds the maximum length of %d bytes", db.session.maxSQLLength))
	}
	return parseSQL(sql)
}

// SetRandomSource injects a random source for the uuid generators (spec/design/entropy.md §6) — the
// deterministic / reproducible path. Pass SeededRandomSource for a byte-identical cross-core stream
// (the conformance # seed: directive). ClearRandomSource returns to the OS CSPRNG, drawn per value.
func (db *engine) SetRandomSource(f RandomSource) { db.session.seam.SetRandom(f) }
func (db *engine) ClearRandomSource()             { db.session.seam.ClearRandom() }

// SetClockSource injects a clock source for uuidv7 (entropy.md §6) — e.g. FixedClock (the # clock:
// directive). ClearClockSource returns to the wall clock.
func (db *engine) SetClockSource(f ClockSource) { db.session.seam.SetClock(f) }
func (db *engine) ClearClockSource()            { db.session.seam.ClearClock() }

// MaxCost is the current execution-cost ceiling (0 ⇒ unlimited). See SetMaxCost.
func (db *engine) MaxCost() int64 { return db.session.maxCost }

// SetWorkMem sets the work-memory budget (in bytes) for blocking operators run on this handle
// (spec/design/spill.md §3, api.md §2.1): the ORDER BY external merge sort holds at most roughly
// this many bytes of rows resident before it spills sorted runs to disk. 0 is unlimited (never
// spill). It never changes what a query observes (results + cost are invariant — spill.md §6), only
// when an operator spills; an in-memory database ignores it. A handle setting, not stored in the
// file (mirrors SetMaxCost).
func (db *engine) SetWorkMem(bytes int) { db.session.workMem = bytes }

// WorkMem is the current work-memory budget in bytes (0 ⇒ unlimited). See SetWorkMem.
func (db *engine) WorkMem() int { return db.session.workMem }

// Status reports the DEFAULT session's transaction status (Idle/Open/Failed, spec/design/session.md
// §2.2) — the explicit three-state machine the convenience methods drive.
func (db *engine) Status() TxStatus { return txStatusOf(db.session.tx) }

// Status reports this session's transaction status (Idle/Open/Failed, session.md §2.2).
func (s *sessionState) Status() TxStatus { return txStatusOf(s.tx) }

// InTransaction reports whether an explicit transaction block is open on this session.
func (s *sessionState) InTransaction() bool { return s.tx != nil }

// MaxCost / SetMaxCost — the per-statement execution-cost ceiling (0 ⇒ unlimited).
func (s *sessionState) MaxCost() int64         { return s.maxCost }
func (s *sessionState) SetMaxCost(limit int64) { s.maxCost = limit }

// LifetimeMaxCost / SetLifetimeMaxCost — the per-session cumulative cost budget (0 ⇒ unlimited,
// spec/design/session.md §5.4). Bounds the whole session: a statement aborts 54P02 the instant the
// session's cumulative cost reaches limit, and once spent every further statement is rejected 54P02
// at admission.
func (s *sessionState) LifetimeMaxCost() int64         { return s.lifetimeMaxCost }
func (s *sessionState) SetLifetimeMaxCost(limit int64) { s.lifetimeMaxCost = limit }

// LifetimeCost is the session's running CUMULATIVE execution cost so far (spec/design/session.md
// §5.4) — the gauge the lifetime_max_cost budget bounds. Tracked even when unlimited; survives a
// transaction rollback (session state, not snapshot state).
func (s *sessionState) LifetimeCost() int64 { return *s.lifetimeTotal }

// newMeter builds the Meter for a statement run on this session: the per-statement max_cost ceiling
// (54P01) plus a handle to the session's cumulative total + budget (54P02). Every statement's meter
// is minted here, so all execution cost live-charges into the cumulative.
func (s *sessionState) newMeter() *costMeter {
	return &costMeter{Limit: s.maxCost, lifetimeTotal: s.lifetimeTotal, lifetimeLimit: s.lifetimeMaxCost, cancel: s.cancel}
}

// MaxSQLLength / SetMaxSQLLength — the input-SQL byte limit (0 ⇒ unlimited).
func (s *sessionState) MaxSQLLength() int     { return s.maxSQLLength }
func (s *sessionState) SetMaxSQLLength(b int) { s.maxSQLLength = b }

// WorkMem / SetWorkMem — the work-memory budget in bytes (0 ⇒ unlimited).
func (s *sessionState) WorkMem() int     { return s.workMem }
func (s *sessionState) SetWorkMem(b int) { s.workMem = b }

// SetDefaultPrivileges replaces the default table-privilege set — the GRANT … ON ALL TABLES default
// (§5.3). A read-only session is PrivSetEmpty.With(PrivSelect).
func (s *sessionState) SetDefaultPrivileges(privs PrivilegeSet) { s.privileges.SetDefaultTable(privs) }

// Grant grants privs on a specific object (table or function), beyond the default (§5.3).
func (s *sessionState) Grant(privs PrivilegeSet, object string) { s.privileges.Grant(privs, object) }

// Revoke revokes privs from a specific object (revoke wins over grant and the default, §5.3).
func (s *sessionState) Revoke(privs PrivilegeSet, object string) { s.privileges.Revoke(privs, object) }

// Privileges is read-only access to this session's authorization envelope (§5.3).
func (s *sessionState) Privileges() *Privileges { return &s.privileges }

// AllowDDL / SetAllowDDL — whether DDL is permitted on this session (§5.3); a denied change is 42501.
func (s *sessionState) AllowDDL() bool         { return s.allowDDL }
func (s *sessionState) SetAllowDDL(allow bool) { s.allowDDL = allow }

// AllowTempDDL / SetAllowTempDDL — whether session-local temporary-table DDL is permitted on this
// session (spec/design/temp-tables.md §5); a denied temp DDL is 42501.
func (s *sessionState) AllowTempDDL() bool         { return s.allowTempDDL }
func (s *sessionState) SetAllowTempDDL(allow bool) { s.allowTempDDL = allow }

// TempBuffers / SetTempBuffers — the per-session temp-table storage budget in BYTES
// (spec/design/temp-tables.md §7); 0 ⇒ unlimited. An over-budget temp write aborts 54P03.
func (s *sessionState) TempBuffers() int         { return s.tempBuffers }
func (s *sessionState) SetTempBuffers(bytes int) { s.tempBuffers = bytes }

// SetVar sets a session variable (spec/design/session.md §6.1) — PostgreSQL's GUC model, scoped to
// the session. Custom variables must be namespaced (a dotted name like myapp.tenant); a non-dotted
// name is 42704 (no built-in setting is reachable through this map in v1 — the time_zone built-in is
// its own slice). The name is case-insensitive (folded to lowercase, PG); the value is text. Session
// state, not snapshot state — it does NOT roll back with a transaction.
func (s *sessionState) SetVar(name, value string) error {
	key, err := requireCustomVarName(name)
	if err != nil {
		return err
	}
	if s.vars == nil {
		s.vars = map[string]string{}
	}
	s.vars[key] = value
	return nil
}

// ResetVar clears a session variable (§6.1). A non-dotted name is 42704 (as for SetVar); an unset
// name is a no-op success (PG RESET of an unset custom variable).
func (s *sessionState) ResetVar(name string) error {
	key, err := requireCustomVarName(name)
	if err != nil {
		return err
	}
	delete(s.vars, key)
	return nil
}

// Var reads a session variable's value (§6.1); ok is false if it is not set. The host getter never
// errors — it is the SQL current_setting read that raises 42704 on an unset name.
func (s *sessionState) Var(name string) (string, bool) {
	v, ok := s.vars[strings.ToLower(name)]
	return v, ok
}

// ResetVars clears every session variable (§6.1) — PostgreSQL's RESET ALL for the variable map (also
// the per-record reset hook the conformance harness's # set: directive uses).
func (s *sessionState) ResetVars() { s.vars = map[string]string{} }

// SetRandomSource / ClearRandomSource — the uuid-generator entropy seam (entropy.md §6).
func (s *sessionState) SetRandomSource(f RandomSource) { s.seam.SetRandom(f) }
func (s *sessionState) ClearRandomSource()             { s.seam.ClearRandom() }

// SetClockSource / ClearClockSource — the uuidv7 / clock-function clock seam (entropy.md §6).
func (s *sessionState) SetClockSource(f ClockSource) { s.seam.SetClock(f) }
func (s *sessionState) ClearClockSource()            { s.seam.ClearClock() }

// ReadOnly reports whether this handle was opened read-only (spec/design/api.md §2.1): every
// transaction defaults to READ ONLY, writes are 25006, and the file is never written.
func (db *engine) ReadOnly() bool { return db.readOnly }

// Table looks up a table definition by name (case-insensitive) in the visible snapshot.
func (db *engine) Table(name string) (*catTable, bool) {
	return db.readSnap().table(name)
}

// CompositeType looks up a composite type definition by name (case-insensitive) in the visible
// snapshot (spec/design/composite.md); nil if absent.
func (db *engine) CompositeType(name string) *compositeType {
	return db.readSnap().compositeType(name)
}

// putTable registers a new table and its empty store in the working snapshot (DDL is
// transactional — transactions.md §4.5).
func (db *engine) putTable(t *catTable) {
	db.working().putTable(t, db.pageSize)
}

// CollationVerdict is the slice-2d version-skew verdict for one referenced collation
// (spec/design/collation.md §12, compatibility.md §7). VerdictFull ⇒ a loaded bundle provides the
// name at the file's pinned (unicode, cldr), so the collation's objects are read-write. VerdictSkewed
// ⇒ a loaded bundle provides it at a DIFFERENT version, so its objects are read-only (reads recompute
// against the loaded table — the heap-scan fallback; a write raises XX002). A pure comparison of the
// file pin (§5) vs the loaded set — every core computes the identical verdict (the §10 contract).
type collationVerdict int

const (
	verdictFull collationVerdict = iota
	verdictSkewed
)

// CollationInfo is introspection metadata for one loaded collation (db.Collations,
// spec/design/collation.md §1). ContentHash is the CRC-32 of the compiled table (the reference-mode
// stamp, §3/§4); Description is provenance, excluded from the hash. Verdict is the slice-2d
// version-skew verdict (§12) — VerdictFull for the engine-global loaded set (it IS the reference);
// for a database's referenced collations it is VerdictSkewed when the file's pin differs from the
// loaded bundle's.
type collationInfo struct {
	Name           string
	UnicodeVersion string
	CLDRVersion    string
	ContentHash    uint32
	Description    string
	IsDefault      bool
	Verdict        collationVerdict
}

// ImportCollation / ExportCollation are GONE (the reference-only pivot, spec/design/collation.md
// §4.2): a collation is provided by a host-loaded bundle and used by name, never loaded into a
// database. There is no runtime path that constructs or bakes a collation table — the only load is
// LoadUnicodeData of jed's own pinned bundle bytes.

// LoadUnicodeData loads a JUCD Unicode-data bundle (db.LoadUnicodeData, spec/design/collation.md
// §4.2): its collations become resolvable by name for COLLATE, per-column collation, and ORDER BY …
// COLLATE. The loaded set is ENGINE-GLOBAL (§9), so a bundle loaded through any handle is visible
// everywhere — including to a later Engine.Open of a file that REFERENCES one of its collations.
// Privileged host op (not SQL-reachable, no path, no engine I/O — §11); ADDITIVE and idempotent for
// an already-loaded bundle. A malformed bundle is XX001. (Mirrors the package-level LoadUnicodeData,
// which the host may call before opening any file.)
func (db *engine) LoadUnicodeData(data []byte) error {
	return LoadUnicodeData(data)
}

// LoadTimeZoneData loads a JTZ time-zone bundle into the engine-global loaded set
// (db.LoadTimeZoneData, spec/design/timezones.md §3.3). The bytes are jed's own pinned TZif (RFC
// 8536) wrapped in a manifest; the loaded zones become usable by AT TIME ZONE. Like the collation
// seam, this is a privileged host op (not SQL-reachable, no path, no engine I/O — §10), additive and
// idempotent, engine-global so it may be called before open. A malformed bundle is XX001. (UTC and
// fixed offsets are built in and need no load.)
func (db *engine) LoadTimeZoneData(data []byte) error {
	return LoadTimeZoneData(data)
}

// LoadedTimeZones introspects the engine-global loaded zone set (db.LoadedTimeZones, timezones.md
// §3.3) — every named zone (and alias) a loaded bundle provides, ascending by name. A property of the
// running engine, not of this database. UTC and fixed offsets are built in and not listed.
func (db *engine) LoadedTimeZones() []timeZoneInfo {
	return loadedTimeZones()
}

// LoadedCollations introspects the engine-global LOADED collation set (db.LoadedCollations,
// spec/design/collation.md §4.2) — every collation a loaded bundle provides, available to any
// database on this handle, ascending by name. A property of the running ENGINE, not of this database;
// for the collations this database references, use Engine.Collations. IsDefault is always false here
// (that is a per-database property). C is built in and not listed.
func (db *engine) LoadedCollations() []collationInfo {
	colls := loadedCollationTables()
	out := make([]collationInfo, len(colls))
	for i, c := range colls {
		out[i] = collationInfo{
			Name:           c.Name,
			UnicodeVersion: c.UnicodeVersion,
			CLDRVersion:    c.CldrVersion,
			ContentHash:    crc32IEEE(serializeTable(c)),
			Description:    c.Description,
			IsDefault:      false,
			// The loaded set IS the version reference — it can never be skewed against itself.
			Verdict: verdictFull,
		}
	}
	return out
}

// SetDefaultCollation sets the per-database default collation (db.SetDefaultCollation,
// spec/design/collation.md §1). "C" resets to byte order; any other name must be a LOADED collation
// (else 42704). Persisted as the is_default flag on that collation's reference entry at the next
// commit (the entry is emitted because the default references it — §5).
// UpgradeCollations adopts a newly-loaded Unicode version for this database's skewed collations
// (the REINDEX / COLLATION UPGRADE migration, spec/design/collation.md §12). A privileged host op
// like SetDefaultCollation — NOT SQL-reachable, so an untrusted query can never trigger it
// (CLAUDE.md §13). For every collation whose file pin differs from the loaded bundle (Skewed) it
// rebuilds the collated keys (PK + indexes) under the loaded table and re-pins the stamp, clearing
// the skew so the affected tables are read-write again and regain collated-index pushdown.
// Whole-database + atomic (the rebuild stages in a snapshot clone swapped in only on success);
// idempotent (no skew ⇒ a no-op returning 0). Persisted by the next explicit Commit. Returns the
// number of collations re-pinned.
func (db *engine) UpgradeCollations() (int, error) {
	work := db.committed.clone()
	n, err := work.upgradeCollations(db.pageSize)
	if err != nil {
		return 0, err
	}
	if n > 0 {
		work.freezeMutationGenerations()
		db.committed = work
	}
	return n, nil
}

func (db *engine) SetDefaultCollation(name string) error {
	if name == "C" {
		db.committed.defaultCollation = ""
		return nil
	}
	if db.committed.resolveCollation(name) == nil {
		return newError(UndefinedObject, fmt.Sprintf("collation %q does not exist", name))
	}
	db.committed.defaultCollation = name
	return nil
}

// DefaultCollation returns the per-database default collation name — "C" unless SetDefaultCollation
// moved it (db.DefaultCollation, spec/design/collation.md §1).
func (db *engine) DefaultCollation() string {
	if db.committed.defaultCollation == "" {
		return "C"
	}
	return db.committed.defaultCollation
}

// Collations introspects the collations THIS DATABASE references (db.Collations,
// spec/design/collation.md §4.2) — every collation its schema uses (a column's COLLATE, or the
// per-database default), in ascending name order. This is the per-file view; for the engine-global
// LOADED set, use Engine.LoadedCollations. C is built in and not listed.
func (db *engine) Collations() []collationInfo {
	// referencedCollations resolves each referenced name (from a loaded bundle).
	colls, err := db.committed.referencedCollations()
	if err != nil {
		return nil
	}
	out := make([]collationInfo, len(colls))
	for i, c := range colls {
		verdict := verdictFull
		// The slice-2d verdict: Skewed when the file's pin differs from the loaded bundle's version
		// (the object is read-only), else Full (collation.md §12).
		if _, _, _, _, skewed := db.committed.collationSkew(c.Name); skewed {
			verdict = verdictSkewed
		}
		out[i] = collationInfo{
			Name:           c.Name,
			UnicodeVersion: c.UnicodeVersion,
			CLDRVersion:    c.CldrVersion,
			ContentHash:    crc32IEEE(serializeTable(c)),
			Description:    c.Description,
			IsDefault:      db.committed.defaultCollation == c.Name,
			Verdict:        verdict,
		}
	}
	return out
}
