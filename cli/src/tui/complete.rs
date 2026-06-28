//! Editor autocomplete (spec/design/cli.md §6): Tab at a partial word offers completions
//! drawn from the live catalog (table names, column names) plus the SQL keywords, type
//! names, and function names of the grammar. Pure candidate logic — the popup state and
//! key handling live in `app.rs`, the rendering in `draw.rs`.

use jed::tooling::Engine;

/// The grammar's word list (keywords, canonical type names, aggregate functions) —
/// completed in the case style of the typed prefix (all-uppercase prefix → uppercase).
/// Shared with the editor's syntax highlighter (highlight.rs).
pub(crate) const WORDS: &[&str] = &[
    "all",
    "and",
    "as",
    "asc",
    "avg",
    "begin",
    "between",
    "boolean",
    "by",
    "bytea",
    "check",
    "commit",
    "constraint",
    "count",
    "create",
    "cross",
    "default",
    "delete",
    "desc",
    "distinct",
    "drop",
    "except",
    "exists",
    "false",
    "from",
    "full",
    "group",
    "having",
    "in",
    "index",
    "inner",
    "insert",
    "i16",
    "i32",
    "i64",
    "integer",
    "intersect",
    "into",
    "is",
    "join",
    "key",
    "left",
    "like",
    "limit",
    "max",
    "min",
    "not",
    "null",
    "numeric",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "outer",
    "primary",
    "read",
    "returning",
    "right",
    "rollback",
    "select",
    "set",
    "smallint",
    "start",
    "sum",
    "table",
    "text",
    "timestamp",
    "timestamptz",
    "transaction",
    "true",
    "union",
    "unique",
    "update",
    "uuid",
    "values",
    "where",
    "work",
    "write",
];

/// The identifier-shaped word ending at character column `col` of `line`: its start
/// column and the word itself (empty when the cursor does not follow a word character).
pub fn current_word(line: &str, col: usize) -> (usize, String) {
    let chars: Vec<char> = line.chars().collect();
    let col = col.min(chars.len());
    let mut start = col;
    while start > 0 && (chars[start - 1].is_ascii_alphanumeric() || chars[start - 1] == '_') {
        start -= 1;
    }
    (start, chars[start..col].iter().collect())
}

/// Completion candidates for `prefix` (case-insensitive), deduplicated and sorted:
/// catalog names first (tables, then columns — completed in their canonical spelling),
/// then grammar words (case-styled after the prefix). An empty prefix matches nothing —
/// Tab at a non-word position should stay an ordinary key.
pub fn candidates(db: &Engine, prefix: &str) -> Vec<String> {
    if prefix.is_empty() {
        return Vec::new();
    }
    let lower = prefix.to_ascii_lowercase();
    let matches =
        |name: &str| name.to_ascii_lowercase().starts_with(&lower) && name.len() > prefix.len();

    let mut tables: Vec<String> = Vec::new();
    let mut columns: Vec<String> = Vec::new();
    for name in db.table_names() {
        if matches(&name) {
            tables.push(name.clone());
        }
        let Some(table) = db.table(&name) else {
            continue;
        };
        for col in &table.columns {
            if matches(&col.name) {
                columns.push(col.name.clone());
            }
        }
    }
    columns.sort_by_key(|c| c.to_ascii_lowercase());
    columns.dedup();

    // Keywords follow the typed case: an all-uppercase prefix completes uppercase.
    let upper = !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_uppercase());
    let words = WORDS.iter().filter(|w| matches(w)).map(|w| {
        if upper {
            w.to_ascii_uppercase()
        } else {
            (*w).to_string()
        }
    });

    let mut out: Vec<String> = Vec::new();
    for c in tables.into_iter().chain(columns).chain(words) {
        if !out.iter().any(|x| x.eq_ignore_ascii_case(&c)) {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use jed::tooling::execute;

    fn db() -> Engine {
        let mut db = Engine::new();
        execute(
            &mut db,
            "CREATE TABLE Users (id i32 PRIMARY KEY, score i32)",
        )
        .unwrap();
        execute(&mut db, "CREATE TABLE selections (sel i32 PRIMARY KEY)").unwrap();
        db
    }

    #[test]
    fn current_word_scans_identifier_chars() {
        assert_eq!(current_word("SELECT na", 9), (7, "na".to_string()));
        assert_eq!(current_word("SELECT na", 7), (7, String::new()));
        assert_eq!(current_word("a_1b", 4), (0, "a_1b".to_string()));
        assert_eq!(current_word("x + y", 3), (3, String::new()));
        assert_eq!(current_word("", 0), (0, String::new()));
    }

    #[test]
    fn candidates_merge_catalog_then_keywords() {
        let db = db();
        // `sel` hits the table `selections` (catalog first) and the keyword `select`.
        assert_eq!(candidates(&db, "sel"), vec!["selections", "select"]);
        // Catalog names complete in canonical spelling regardless of typed case.
        assert_eq!(candidates(&db, "use"), vec!["Users"]);
        // Columns complete too.
        assert_eq!(candidates(&db, "sco"), vec!["score"]);
        // An uppercase prefix completes keywords uppercase.
        assert_eq!(candidates(&db, "SEL"), vec!["selections", "SELECT"]);
        // A full word offers nothing further (but a longer catalog name still can:
        // `select` → `selections`); an empty prefix offers nothing at all.
        assert!(candidates(&db, "where").is_empty());
        assert_eq!(candidates(&db, "select"), vec!["selections"]);
        assert!(candidates(&db, "").is_empty());
    }
}
