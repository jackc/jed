//! The session authorization envelope (spec/design/session.md §5.3) — jed's GRANT/REVOKE-style
//! privilege model. CLAUDE.md §3 deletes in-database users/roles/RBAC, so this is **not** a
//! permission catalog: the *host* holds the envelope on the [`Session`](crate::Session) and the
//! engine mechanically enforces it (`42501`) at name resolution.
//!
//! Two object kinds, exactly PostgreSQL's, minus the privileges jed has no feature for:
//!
//! - **Tables** — the four DML privileges `SELECT` / `INSERT` / `UPDATE` / `DELETE`.
//! - **Functions** — a single `EXECUTE` privilege.
//!
//! Three layers compose into an effective privilege set per object:
//!
//! 1. **`default_table`** — the table-privilege set granted to *every* table (the `GRANT … ON ALL
//!    TABLES` default). A read-only session is `{SELECT}`. Functions default to `EXECUTE` on all.
//! 2. **`grant`** — per-object additions beyond the default (the whitelist).
//! 3. **`revoke`** — per-object removals (the blacklist).
//!
//! Effective for an operation `OP` on object `X`: `OP ∈ (default(kind) ∪ grant[X]) \ revoke[X]` —
//! **revoke wins** (deny is order-independent, the safe default).

use std::collections::HashMap;

/// One privilege — a table DML privilege or the function `EXECUTE` privilege (spec/design/session.md
/// §5.3). The four table privileges and `EXECUTE` are disjoint by object kind, so one flat enum (and
/// one [`PrivilegeSet`]) covers both: a table check never asks about `Execute`, a function check asks
/// only about it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Privilege {
    Select,
    Insert,
    Update,
    Delete,
    Execute,
}

impl Privilege {
    /// The bit this privilege occupies in a [`PrivilegeSet`].
    const fn bit(self) -> u8 {
        match self {
            Privilege::Select => 1,
            Privilege::Insert => 1 << 1,
            Privilege::Update => 1 << 2,
            Privilege::Delete => 1 << 3,
            Privilege::Execute => 1 << 4,
        }
    }

    /// Parse a privilege keyword (case-insensitive) — the spelling used by the `# grant:` /
    /// `# revoke:` / `# default_privileges:` conformance directives and the host API.
    pub fn from_name(name: &str) -> Option<Privilege> {
        match name.to_ascii_uppercase().as_str() {
            "SELECT" => Some(Privilege::Select),
            "INSERT" => Some(Privilege::Insert),
            "UPDATE" => Some(Privilege::Update),
            "DELETE" => Some(Privilege::Delete),
            "EXECUTE" => Some(Privilege::Execute),
            _ => None,
        }
    }
}

/// A small set of [`Privilege`]s, held as a bitmask. `Copy` and allocation-free — the privilege
/// model threads these through the session and the resolve-time check with no heap traffic.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PrivilegeSet(u8);

impl PrivilegeSet {
    /// The empty set.
    pub const EMPTY: PrivilegeSet = PrivilegeSet(0);
    /// The four table DML privileges — the default table envelope (`GRANT … ON ALL TABLES`).
    pub const ALL_TABLE: PrivilegeSet = PrivilegeSet(
        Privilege::Select.bit()
            | Privilege::Insert.bit()
            | Privilege::Update.bit()
            | Privilege::Delete.bit(),
    );
    /// Just `EXECUTE` — the default function envelope (`GRANT EXECUTE ON ALL FUNCTIONS`).
    pub const EXECUTE: PrivilegeSet = PrivilegeSet(Privilege::Execute.bit());

    /// Whether `p` is in the set.
    pub fn contains(self, p: Privilege) -> bool {
        self.0 & p.bit() != 0
    }

    /// This set with `p` added.
    pub fn with(self, p: Privilege) -> PrivilegeSet {
        PrivilegeSet(self.0 | p.bit())
    }

    /// The union of two sets.
    pub fn union(self, other: PrivilegeSet) -> PrivilegeSet {
        PrivilegeSet(self.0 | other.0)
    }

    /// This set with `other`'s members removed.
    pub fn minus(self, other: PrivilegeSet) -> PrivilegeSet {
        PrivilegeSet(self.0 & !other.0)
    }

    /// Whether the set is empty.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// The session's authorization envelope (spec/design/session.md §5.3): the default table-privilege
/// set plus per-object grant / revoke deltas. Object names are keyed **lowercased** (matched against
/// the canonical catalog name). A fresh envelope is fully permissive (every table privilege, every
/// function), so a default session behaves as before the privilege model landed.
#[derive(Clone, Debug)]
pub struct Privileges {
    /// The table-privilege set granted to **every** table (the "all tables" default). A read-only
    /// session is `{SELECT}`. Default: all four.
    default_table: PrivilegeSet,
    /// Per-object additions beyond the default (the whitelist), keyed by lowercased object name.
    grant: HashMap<String, PrivilegeSet>,
    /// Per-object removals (the blacklist), keyed by lowercased object name. Revoke wins over grant.
    revoke: HashMap<String, PrivilegeSet>,
}

impl Default for Privileges {
    fn default() -> Self {
        Privileges {
            default_table: PrivilegeSet::ALL_TABLE,
            grant: HashMap::new(),
            revoke: HashMap::new(),
        }
    }
}

impl Privileges {
    /// Whether this envelope is fully permissive — the default table set, no grants, no revokes — so
    /// a statement needs no per-object check (the resolve-time fast path).
    pub fn is_permissive(&self) -> bool {
        self.default_table == PrivilegeSet::ALL_TABLE
            && self.grant.is_empty()
            && self.revoke.is_empty()
    }

    /// Replace the default table-privilege set (the `GRANT … ON ALL TABLES` default). This is what
    /// expresses a read-only session: `{SELECT}`.
    pub fn set_default_table(&mut self, privs: PrivilegeSet) {
        self.default_table = privs;
    }

    /// Add `privs` to `object`'s grant delta (beyond the default).
    pub fn grant(&mut self, privs: PrivilegeSet, object: &str) {
        let e = self.grant.entry(object.to_ascii_lowercase()).or_default();
        *e = e.union(privs);
    }

    /// Add `privs` to `object`'s revoke delta (revoke wins over grant and the default).
    pub fn revoke(&mut self, privs: PrivilegeSet, object: &str) {
        let e = self.revoke.entry(object.to_ascii_lowercase()).or_default();
        *e = e.union(privs);
    }

    /// The effective privilege set for object `name` (already lowercased) over a `base` default.
    fn effective(&self, name: &str, base: PrivilegeSet) -> PrivilegeSet {
        let granted = self.grant.get(name).copied().unwrap_or_default();
        let revoked = self.revoke.get(name).copied().unwrap_or_default();
        base.union(granted).minus(revoked)
    }

    /// Whether table `name` (lowercased) holds privilege `p`.
    pub fn allows_table(&self, name: &str, p: Privilege) -> bool {
        self.effective(name, self.default_table).contains(p)
    }

    /// Whether function `name` (lowercased) holds `EXECUTE` (functions default to `EXECUTE` on all).
    pub fn allows_function(&self, name: &str) -> bool {
        self.effective(name, PrivilegeSet::EXECUTE)
            .contains(Privilege::Execute)
    }
}
