//! Error `Detail` — PostgreSQL's DETAIL sentence naming the values that broke a constraint
//! (spec/design/error-fields.md §9). Built only after the error is decided, from values the
//! statement already holds: unmetered, so it can never change the code, the cost, or which error
//! fires. Every helper applies the §9.4 privilege gate and returns `None` when the detail is
//! withheld or cannot be rendered.

use super::*;

/// The 23505 endings: a write's duplicate (`already exists.`) and a unique-index build over
/// existing duplicates (`is duplicated.` — error-fields.md §9.2).
pub(crate) const EXISTS: &str = "already exists.";
pub(crate) const DUPLICATED: &str = "is duplicated.";

/// The per-value cap, in bytes, before a rendered value is cut and suffixed `...` (§9.3 — PG's
/// `ExecBuildSlotValueDescription` `maxfieldlen`).
const DETAIL_VALUE_MAX: usize = 64;

/// Cut `s` to at most [`DETAIL_VALUE_MAX`] bytes at a UTF-8 character boundary, appending `...`
/// when anything was cut (§9.3).
fn clip(s: &str) -> String {
    if s.len() <= DETAIL_VALUE_MAX {
        return s.to_string();
    }
    let mut end = DETAIL_VALUE_MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// A column label: jed's identifier quoting rule (grammar.md §3).
fn column_label(columns: &[Column], ordinal: usize) -> String {
    crate::parser::quote_identifier(&columns[ordinal].name)
}

/// `Key (a, b)=(1, x)` — the shared key phrase of the 23505/23503/23P01 forms.
fn key_phrase(labels: &[String], values: &[String]) -> String {
    format!("Key ({})=({})", labels.join(", "), values.join(", "))
}

impl Engine {
    /// The §9.4 gate: whether this session may see `relation`'s values.
    fn detail_visible(&self, relation: &str) -> bool {
        let p = &self.session.privileges;
        p.is_permissive() || p.allows_table(relation, Privilege::Select)
    }

    /// The columns of `table` (in `db_scope`) — for a raise site that does not otherwise hold them.
    pub(crate) fn relation_columns(&self, db_scope: Option<&str>, table: &str) -> Vec<Column> {
        self.table_scoped(db_scope, table)
            .map(|t| t.columns.clone())
            .unwrap_or_default()
    }

    /// One value through the result-cell renderer in the session zone (§9.3): a top-level NULL is
    /// `null`, and the text is clipped. `None` if a deferred value cannot be resolved or the value
    /// cannot render (an out-of-range zoned instant) — the detail is then omitted, never wrong.
    fn detail_value(&self, v: &Value) -> Option<String> {
        let resolved;
        let v = match v {
            Value::Unfetched(u) => {
                resolved = crate::format::resolve_unfetched_self(u).ok()?;
                &resolved
            }
            v => v,
        };
        match v {
            Value::Null => Some("null".to_string()),
            // Clip a long text before copying it (only the first bytes are kept).
            Value::Text(s) => Some(clip(s)),
            v => v.render_in(&self.session.time_zone).ok().map(|s| clip(&s)),
        }
    }

    fn detail_values<'a>(&self, values: impl Iterator<Item = &'a Value>) -> Option<Vec<String>> {
        values.map(|v| self.detail_value(v)).collect()
    }

    /// The key phrase over plain-column `ordinals` of `row`.
    fn column_key_phrase(
        &self,
        columns: &[Column],
        ordinals: &[usize],
        row: &Row,
    ) -> Option<String> {
        let labels: Vec<String> = ordinals.iter().map(|&o| column_label(columns, o)).collect();
        let values = self.detail_values(ordinals.iter().map(|&o| &row[o]))?;
        Some(key_phrase(&labels, &values))
    }

    /// 23505 on the primary key: `Key (id)=(1) already exists.` (or `is duplicated.` from a DDL
    /// build — pass `ending`).
    pub(crate) fn pk_detail(
        &self,
        relation: &str,
        columns: &[Column],
        pk: &[usize],
        row: &Row,
        ending: &str,
    ) -> Option<String> {
        if !self.detail_visible(relation) {
            return None;
        }
        Some(format!(
            "{} {ending}",
            self.column_key_phrase(columns, pk, row)?
        ))
    }

    /// 23505 on a unique index: the index's key labels (a column's quoted name, an expression's
    /// persisted canonical text) and the row's key values, evaluated afresh for an expression key.
    pub(crate) fn index_detail(
        &self,
        relation: &str,
        columns: &[Column],
        def: &IndexDef,
        rindex: &ResolvedIndex,
        row: &Row,
        env: &EvalEnv,
        ending: &str,
    ) -> Option<String> {
        if !self.detail_visible(relation) {
            return None;
        }
        let mut labels = Vec::with_capacity(def.keys.len());
        let mut values = Vec::with_capacity(def.keys.len());
        for (k, rk) in def.keys.iter().zip(&rindex.keys) {
            labels.push(match k {
                IndexKey::Column(o) => column_label(columns, *o),
                IndexKey::Expr(e) => e.expr_text.clone(),
            });
            let v = match rk {
                ResolvedKey::Column(o) => self.detail_value(&row[*o])?,
                ResolvedKey::Expr(rx, ..) => {
                    let mut m = self.session.scratch_meter();
                    self.detail_value(&rx.eval(row, env, &mut m).ok()?)?
                }
            };
            values.push(v);
        }
        Some(format!("{} {ending}", key_phrase(&labels, &values)))
    }

    /// 23505 on a unique index found by name on `relation`'s catalog entry (the raise sites that
    /// hold only the resolved index).
    pub(crate) fn index_detail_by_name(
        &self,
        db_scope: Option<&str>,
        relation: &str,
        rindex: &ResolvedIndex,
        row: &Row,
        env: &EvalEnv,
        ending: &str,
    ) -> Option<String> {
        let table = self.table_scoped(db_scope, relation)?;
        let def = table.indexes.iter().find(|d| d.name == rindex.name)?;
        self.index_detail(relation, &table.columns, def, rindex, row, env, ending)
    }

    /// 23514 / 23502: `Failing row contains (…).` — every column of the candidate row.
    pub(crate) fn failing_row_detail(&self, relation: &str, row: &Row) -> Option<String> {
        if !self.detail_visible(relation) {
            return None;
        }
        let values = self.detail_values(row.iter())?;
        Some(format!("Failing row contains ({}).", values.join(", ")))
    }

    /// 23503 child side: `Key (p)=(99) is not present in table "t".` — degraded to the value-free
    /// `Key is not present in table "t".` without SELECT on the child (§9.4).
    pub(crate) fn fk_child_detail(
        &self,
        relation: &str,
        columns: &[Column],
        fk: &ForeignKeyConstraint,
        row: &Row,
    ) -> Option<String> {
        let tail = format!("is not present in table \"{}\".", fk.ref_table);
        if !self.detail_visible(relation) {
            return Some(format!("Key {tail}"));
        }
        Some(format!(
            "{} {tail}",
            self.column_key_phrase(columns, &fk.columns, row)?
        ))
    }

    /// 23503 parent side: `Key (id)=(1) is still referenced from table "c".` — the parent's
    /// referenced columns over `values`, the removed/changed parent tuple in FK slot order;
    /// value-free without SELECT on the parent (§9.4).
    pub(crate) fn fk_parent_detail(
        &self,
        parent: &Table,
        fk: &ForeignKeyConstraint,
        child: &str,
        values: &[Value],
    ) -> Option<String> {
        let tail = format!("is still referenced from table \"{child}\".");
        if !self.detail_visible(&parent.name) {
            return Some(format!("Key {tail}"));
        }
        let labels: Vec<String> = fk
            .ref_columns
            .iter()
            .map(|&o| column_label(&parent.columns, o))
            .collect();
        let values = self.detail_values(values.iter())?;
        Some(format!("{} {tail}", key_phrase(&labels, &values)))
    }

    /// 23P01: `Key (r)=(new) conflicts with existing key (r)=(other).` (`existing` false gives the
    /// DDL build's `conflicts with key`).
    pub(crate) fn exclusion_detail(
        &self,
        relation: &str,
        columns: &[Column],
        exc: &ExclusionConstraint,
        row: &Row,
        other: &Row,
        existing: bool,
    ) -> Option<String> {
        if !self.detail_visible(relation) {
            return None;
        }
        let ordinals: Vec<usize> = exc.elements.iter().map(|e| e.column).collect();
        let new = self.column_key_phrase(columns, &ordinals, row)?;
        let old = self.column_key_phrase(columns, &ordinals, other)?;
        let old = old.strip_prefix("Key ").unwrap_or(&old);
        let with = if existing { "existing key" } else { "key" };
        Some(format!("{new} conflicts with {with} {old}."))
    }
}
