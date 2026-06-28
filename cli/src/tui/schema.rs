//! The schema sidebar: a flat, selectable rendering of the catalog via the host
//! introspection surface — `table_names()` + `table(name)` (api.md §6; cli.md §6).

use jed::tooling::Engine;

pub struct SchemaLine {
    pub text: String,
    /// `Some(name)` on a table-header line — Enter inserts the name into the editor.
    pub table: Option<String>,
}

#[derive(Default)]
pub struct SchemaPane {
    pub lines: Vec<SchemaLine>,
    pub selected: usize,
    pub off: usize,
}

impl SchemaPane {
    /// Rebuild from the visible catalog (called after every successful statement batch).
    pub fn refresh(&mut self, db: &Engine) {
        self.lines.clear();
        for name in db.table_names() {
            let Some(t) = db.table(&name) else { continue };
            self.lines.push(SchemaLine {
                text: name.clone(),
                table: Some(name.clone()),
            });
            for (i, col) in t.columns.iter().enumerate() {
                let ty = match (&col.ty, &col.decimal) {
                    (jed::tooling::Type::Scalar(jed::tooling::ScalarType::Decimal), Some(m)) => {
                        format!("numeric({},{})", m.precision, m.scale)
                    }
                    (ty, _) => ty.canonical_name().to_string(),
                };
                let mut tags = String::new();
                if t.pk.contains(&i) {
                    tags.push_str(" PK");
                } else if col.not_null {
                    tags.push_str(" NN");
                }
                self.lines.push(SchemaLine {
                    text: format!("  {} {}{}", col.name, ty, tags),
                    table: None,
                });
            }
            for idx in &t.indexes {
                let unique = if idx.unique { " UNIQUE" } else { "" };
                self.lines.push(SchemaLine {
                    text: format!("  idx {}{}", idx.name, unique),
                    table: None,
                });
            }
            for chk in &t.checks {
                self.lines.push(SchemaLine {
                    text: format!("  chk {}", chk.name),
                    table: None,
                });
            }
        }
        if self.selected >= self.lines.len() {
            self.selected = self.lines.len().saturating_sub(1);
        }
    }

    pub fn move_sel(&mut self, delta: isize) {
        if self.lines.is_empty() {
            return;
        }
        let max = self.lines.len() - 1;
        self.selected = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            (self.selected + delta as usize).min(max)
        };
    }

    pub fn selected_table(&self) -> Option<String> {
        self.lines.get(self.selected)?.table.clone()
    }
}
