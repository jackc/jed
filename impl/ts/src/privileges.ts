// The session authorization envelope (spec/design/session.md §5.3) — jed's GRANT/REVOKE-style
// privilege model. CLAUDE.md §3 deletes in-database users/roles/RBAC, so this is NOT a permission
// catalog: the host holds the envelope on the Session and the engine mechanically enforces it (42501)
// at name resolution. Mirrors impl/rust/src/privileges.rs and impl/go/privileges.go.
//
// Two object kinds, exactly PostgreSQL's, minus the privileges jed has no feature for:
//   - Tables    — the four DML privileges SELECT / INSERT / UPDATE / DELETE.
//   - Functions — a single EXECUTE privilege.
//
// Effective privilege for OP on object X: OP ∈ (default(kind) ∪ grant[X]) \ revoke[X] — revoke wins.

// Privilege is one table DML privilege or the function EXECUTE privilege. A string union (the engine
// is the erasable TS subset — no enum, CLAUDE.md §2). The four table privileges and EXECUTE are
// disjoint by object kind, so one union (and one PrivilegeSet) covers both.
export type Privilege = "select" | "insert" | "update" | "delete" | "execute";

// privilegeBit is the bit a privilege occupies in a PrivilegeSet.
function privilegeBit(p: Privilege): number {
  switch (p) {
    case "select":
      return 1 << 0;
    case "insert":
      return 1 << 1;
    case "update":
      return 1 << 2;
    case "delete":
      return 1 << 3;
    case "execute":
      return 1 << 4;
  }
}

// privilegeFromName parses a privilege keyword (case-insensitive) — the spelling used by the
// # grant: / # revoke: / # default_privileges: conformance directives and the host API.
export function privilegeFromName(name: string): Privilege | undefined {
  switch (name.trim().toUpperCase()) {
    case "SELECT":
      return "select";
    case "INSERT":
      return "insert";
    case "UPDATE":
      return "update";
    case "DELETE":
      return "delete";
    case "EXECUTE":
      return "execute";
    default:
      return undefined;
  }
}

// PrivilegeSet is a small set of Privileges, held as a bitmask (immutable value semantics).
export class PrivilegeSet {
  readonly bits: number;
  constructor(bits: number) {
    this.bits = bits;
  }
  // The empty set.
  static empty(): PrivilegeSet {
    return new PrivilegeSet(0);
  }
  // The four table DML privileges — the default table envelope (GRANT … ON ALL TABLES).
  static allTable(): PrivilegeSet {
    return new PrivilegeSet(
      privilegeBit("select") |
        privilegeBit("insert") |
        privilegeBit("update") |
        privilegeBit("delete"),
    );
  }
  // Just EXECUTE — the default function envelope (GRANT EXECUTE ON ALL FUNCTIONS).
  static executeOnly(): PrivilegeSet {
    return new PrivilegeSet(privilegeBit("execute"));
  }
  contains(p: Privilege): boolean {
    return (this.bits & privilegeBit(p)) !== 0;
  }
  with(p: Privilege): PrivilegeSet {
    return new PrivilegeSet(this.bits | privilegeBit(p));
  }
  union(other: PrivilegeSet): PrivilegeSet {
    return new PrivilegeSet(this.bits | other.bits);
  }
  minus(other: PrivilegeSet): PrivilegeSet {
    return new PrivilegeSet(this.bits & ~other.bits);
  }
  isEmpty(): boolean {
    return this.bits === 0;
  }
}

// Privileges is the session's authorization envelope (spec/design/session.md §5.3): the default
// table-privilege set plus per-object grant / revoke deltas (keyed lowercased — the canonical catalog
// name). A fresh envelope is fully permissive (every table privilege, every function).
export class Privileges {
  defaultTable: PrivilegeSet;
  grants: Map<string, PrivilegeSet>;
  revokes: Map<string, PrivilegeSet>;

  constructor() {
    this.defaultTable = PrivilegeSet.allTable();
    this.grants = new Map();
    this.revokes = new Map();
  }

  // isPermissive reports whether this envelope is fully permissive — the default table set, no
  // grants, no revokes — so a statement needs no per-object check (the resolve-time fast path).
  isPermissive(): boolean {
    return (
      this.defaultTable.bits === PrivilegeSet.allTable().bits &&
      this.grants.size === 0 &&
      this.revokes.size === 0
    );
  }

  // setDefaultTable replaces the default table-privilege set (the GRANT … ON ALL TABLES default). A
  // read-only session is PrivilegeSet.empty().with("select").
  setDefaultTable(privs: PrivilegeSet): void {
    this.defaultTable = privs;
  }

  // grant adds privs to object's grant delta (beyond the default).
  grant(privs: PrivilegeSet, object: string): void {
    const key = object.toLowerCase();
    this.grants.set(key, (this.grants.get(key) ?? PrivilegeSet.empty()).union(privs));
  }

  // revoke adds privs to object's revoke delta (revoke wins over grant and the default).
  revoke(privs: PrivilegeSet, object: string): void {
    const key = object.toLowerCase();
    this.revokes.set(key, (this.revokes.get(key) ?? PrivilegeSet.empty()).union(privs));
  }

  // effective is the effective privilege set for object name (already lowercased) over a base default.
  private effective(name: string, base: PrivilegeSet): PrivilegeSet {
    const granted = this.grants.get(name) ?? PrivilegeSet.empty();
    const revoked = this.revokes.get(name) ?? PrivilegeSet.empty();
    return base.union(granted).minus(revoked);
  }

  // allowsTable reports whether table name holds privilege p.
  allowsTable(name: string, p: Privilege): boolean {
    return this.effective(name.toLowerCase(), this.defaultTable).contains(p);
  }

  // allowsFunction reports whether function name holds EXECUTE (functions default to EXECUTE on all).
  allowsFunction(name: string): boolean {
    return this.effective(name.toLowerCase(), PrivilegeSet.executeOnly()).contains("execute");
  }
}
