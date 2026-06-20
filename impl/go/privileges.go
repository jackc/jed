package jed

// The session authorization envelope (spec/design/session.md §5.3) — jed's GRANT/REVOKE-style
// privilege model. CLAUDE.md §3 deletes in-database users/roles/RBAC, so this is NOT a permission
// catalog: the host holds the envelope on the Session and the engine mechanically enforces it (42501)
// at name resolution. Mirrors impl/rust/src/privileges.rs.
//
// Two object kinds, exactly PostgreSQL's, minus the privileges jed has no feature for:
//   - Tables    — the four DML privileges SELECT / INSERT / UPDATE / DELETE.
//   - Functions — a single EXECUTE privilege.
//
// Effective privilege for OP on object X: OP ∈ (default(kind) ∪ grant[X]) \ revoke[X] — revoke wins.

import "strings"

// Privilege is one table DML privilege or the function EXECUTE privilege. The four table privileges
// and EXECUTE are disjoint by object kind, so one flat enum (and one PrivilegeSet) covers both.
type Privilege int

const (
	PrivSelect Privilege = iota
	PrivInsert
	PrivUpdate
	PrivDelete
	PrivExecute
)

// bit is the bit this privilege occupies in a PrivilegeSet.
func (p Privilege) bit() uint8 { return 1 << uint(p) }

// PrivilegeFromName parses a privilege keyword (case-insensitive) — the spelling used by the
// # grant: / # revoke: / # default_privileges: conformance directives and the host API.
func PrivilegeFromName(name string) (Privilege, bool) {
	switch strings.ToUpper(strings.TrimSpace(name)) {
	case "SELECT":
		return PrivSelect, true
	case "INSERT":
		return PrivInsert, true
	case "UPDATE":
		return PrivUpdate, true
	case "DELETE":
		return PrivDelete, true
	case "EXECUTE":
		return PrivExecute, true
	default:
		return 0, false
	}
}

// PrivilegeSet is a small set of Privileges, held as a bitmask (allocation-free, comparable).
type PrivilegeSet uint8

const (
	// PrivSetEmpty is the empty set.
	PrivSetEmpty PrivilegeSet = 0
	// PrivSetAllTable is the four table DML privileges — the default table envelope.
	PrivSetAllTable = PrivilegeSet(1<<uint(PrivSelect) | 1<<uint(PrivInsert) | 1<<uint(PrivUpdate) | 1<<uint(PrivDelete))
	// PrivSetExecute is just EXECUTE — the default function envelope.
	PrivSetExecute = PrivilegeSet(1 << uint(PrivExecute))
)

// Contains reports whether p is in the set.
func (s PrivilegeSet) Contains(p Privilege) bool { return uint8(s)&p.bit() != 0 }

// With returns this set with p added.
func (s PrivilegeSet) With(p Privilege) PrivilegeSet { return s | PrivilegeSet(p.bit()) }

// Union returns the union of two sets.
func (s PrivilegeSet) Union(other PrivilegeSet) PrivilegeSet { return s | other }

// Minus returns this set with other's members removed.
func (s PrivilegeSet) Minus(other PrivilegeSet) PrivilegeSet { return s &^ other }

// IsEmpty reports whether the set is empty.
func (s PrivilegeSet) IsEmpty() bool { return s == 0 }

// Privileges is the session's authorization envelope (spec/design/session.md §5.3): the default
// table-privilege set plus per-object grant / revoke deltas. Object names are keyed lowercased
// (matched against the canonical catalog name). The zero value is unusable — build with
// newPrivileges (fully permissive: every table privilege, every function).
type Privileges struct {
	defaultTable PrivilegeSet
	grant        map[string]PrivilegeSet
	revoke       map[string]PrivilegeSet
}

// newPrivileges builds a fresh, fully-permissive envelope.
func newPrivileges() Privileges {
	return Privileges{defaultTable: PrivSetAllTable}
}

// IsPermissive reports whether this envelope is fully permissive — the default table set, no grants,
// no revokes — so a statement needs no per-object check (the resolve-time fast path).
func (pv *Privileges) IsPermissive() bool {
	return pv.defaultTable == PrivSetAllTable && len(pv.grant) == 0 && len(pv.revoke) == 0
}

// SetDefaultTable replaces the default table-privilege set (the GRANT … ON ALL TABLES default). A
// read-only session is PrivSetEmpty.With(PrivSelect).
func (pv *Privileges) SetDefaultTable(privs PrivilegeSet) { pv.defaultTable = privs }

// Grant adds privs to object's grant delta (beyond the default).
func (pv *Privileges) Grant(privs PrivilegeSet, object string) {
	if pv.grant == nil {
		pv.grant = map[string]PrivilegeSet{}
	}
	key := strings.ToLower(object)
	pv.grant[key] = pv.grant[key].Union(privs)
}

// Revoke adds privs to object's revoke delta (revoke wins over grant and the default).
func (pv *Privileges) Revoke(privs PrivilegeSet, object string) {
	if pv.revoke == nil {
		pv.revoke = map[string]PrivilegeSet{}
	}
	key := strings.ToLower(object)
	pv.revoke[key] = pv.revoke[key].Union(privs)
}

// effective is the effective privilege set for object name (already lowercased) over a base default.
func (pv *Privileges) effective(name string, base PrivilegeSet) PrivilegeSet {
	return base.Union(pv.grant[name]).Minus(pv.revoke[name])
}

// AllowsTable reports whether table name (lowercased) holds privilege p.
func (pv *Privileges) AllowsTable(name string, p Privilege) bool {
	return pv.effective(strings.ToLower(name), pv.defaultTable).Contains(p)
}

// AllowsFunction reports whether function name (lowercased) holds EXECUTE (functions default to
// EXECUTE granted on all).
func (pv *Privileges) AllowsFunction(name string) bool {
	return pv.effective(strings.ToLower(name), PrivSetExecute).Contains(PrivExecute)
}
