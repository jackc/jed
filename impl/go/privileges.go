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

// Privilege is one privilege a session may hold on an object: SELECT, INSERT, UPDATE or DELETE on
// a table or view, or EXECUTE on a function. jed has no users or roles; the host sets a session's
// privileges with [Session.Grant], [Session.Revoke] and [Session.SetDefaultPrivileges], and a
// statement that needs a privilege the session lacks fails with SQLSTATE 42501.
type Privilege int

// The privileges. PrivSelect through PrivDelete apply to tables and views; PrivExecute applies to
// functions.
const (
	PrivSelect Privilege = iota
	PrivInsert
	PrivUpdate
	PrivDelete
	PrivExecute
)

// bit is the bit this privilege occupies in a PrivilegeSet.
func (p Privilege) bit() uint8 { return 1 << uint(p) }

// PrivilegeFromName returns the privilege named by a SQL keyword ("SELECT", "INSERT", "UPDATE",
// "DELETE" or "EXECUTE"), ignoring case and surrounding space. ok is false for any other name.
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

// PrivilegeSet is a set of [Privilege] values. It is a small comparable value; the zero
// PrivilegeSet is empty.
type PrivilegeSet uint8

// Common privilege sets.
const (
	// PrivSetEmpty is the empty set.
	PrivSetEmpty PrivilegeSet = 0
	// PrivSetAllTable holds the four table privileges SELECT, INSERT, UPDATE and DELETE. It is a
	// session's default set for every table.
	PrivSetAllTable = PrivilegeSet(1<<uint(PrivSelect) | 1<<uint(PrivInsert) | 1<<uint(PrivUpdate) | 1<<uint(PrivDelete))
	// PrivSetExecute holds only EXECUTE. Every function has it by default.
	PrivSetExecute = PrivilegeSet(1 << uint(PrivExecute))
)

// Contains reports whether p is in the set.
func (s PrivilegeSet) Contains(p Privilege) bool { return uint8(s)&p.bit() != 0 }

// With returns s with p added.
func (s PrivilegeSet) With(p Privilege) PrivilegeSet { return s | PrivilegeSet(p.bit()) }

// Union returns the privileges in s, other, or both.
func (s PrivilegeSet) Union(other PrivilegeSet) PrivilegeSet { return s | other }

// Minus returns s with the privileges in other removed.
func (s PrivilegeSet) Minus(other PrivilegeSet) PrivilegeSet { return s &^ other }

// IsEmpty reports whether the set is empty.
func (s PrivilegeSet) IsEmpty() bool { return s == 0 }

// Privileges is a session's privilege settings: a default set for all tables, plus privileges
// granted and revoked on individual tables, views and functions. A privilege on an object is held
// when it is in the default set (for a table) or always (EXECUTE on a function), or granted on the
// object, and has not been revoked on it; a revoke wins over both. Object names match the name as
// stored in the catalog, exactly. A session starts with every privilege; read the current
// settings with [Session.Privileges].
type Privileges struct {
	defaultTable PrivilegeSet
	grant        map[string]PrivilegeSet
	revoke       map[string]PrivilegeSet
}

// newPrivileges builds a fresh, fully-permissive envelope.
func newPrivileges() Privileges {
	return Privileges{defaultTable: PrivSetAllTable}
}

// IsPermissive reports whether pv grants everything: the default table set is
// [PrivSetAllTable] and there are no per-object grants or revokes.
func (pv *Privileges) IsPermissive() bool {
	return pv.defaultTable == PrivSetAllTable && len(pv.grant) == 0 && len(pv.revoke) == 0
}

// setDefaultTable replaces the default table-privilege set (the GRANT … ON ALL TABLES default). A
// read-only session is PrivSetEmpty.With(PrivSelect).
func (pv *Privileges) setDefaultTable(privs PrivilegeSet) { pv.defaultTable = privs }

// grantObject adds privs to object's grant delta (beyond the default).
func (pv *Privileges) grantObject(privs PrivilegeSet, object string) {
	if pv.grant == nil {
		pv.grant = map[string]PrivilegeSet{}
	}
	key := object
	pv.grant[key] = pv.grant[key].Union(privs)
}

// revokeObject adds privs to object's revoke delta (revoke wins over grant and the default).
func (pv *Privileges) revokeObject(privs PrivilegeSet, object string) {
	if pv.revoke == nil {
		pv.revoke = map[string]PrivilegeSet{}
	}
	key := object
	pv.revoke[key] = pv.revoke[key].Union(privs)
}

// effective is the effective privilege set for object name (the exact name) over a base default.
func (pv *Privileges) effective(name string, base PrivilegeSet) PrivilegeSet {
	return base.Union(pv.grant[name]).Minus(pv.revoke[name])
}

// AllowsTable reports whether the session holds privilege p on the table or view name.
func (pv *Privileges) AllowsTable(name string, p Privilege) bool {
	return pv.effective(name, pv.defaultTable).Contains(p)
}

// AllowsFunction reports whether the session holds EXECUTE on the function name. Every function
// has EXECUTE unless it was revoked.
func (pv *Privileges) AllowsFunction(name string) bool {
	return pv.effective(name, PrivSetExecute).Contains(PrivExecute)
}
