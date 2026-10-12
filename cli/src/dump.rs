//! `.dump`-style SQL export (spec/design/cli.md §3): `--dump` writes a SQL script that
//! recreates the database — schema (CREATE TABLE with PRIMARY KEY / NOT NULL / DEFAULT /
//! CHECK), one INSERT per row, then CREATE INDEX statements (after the rows, so a replay
//! builds each index once), then every view (spec/design/views.md §11) — wrapped in one
//! BEGIN/COMMIT so a file-backed replay commits durably once. Replaying the script into a fresh
//! database yields the same logical state: dump → replay → dump is byte-identical.

use std::io::{self, Write};

use jed::Value;
use jed::tooling::{CheckOption, Table};

/// Write the whole database as SQL. Tables come out in the catalog's standing order
/// (sorted by lowercased name — api.md §6); rows in primary-key order via `ORDER BY`
/// (a no-PK table dumps in storage order, which replays into the same rowid order).
pub fn dump(db: &mut jed::Session, out: &mut dyn Write) -> io::Result<()> {
    writeln!(out, "BEGIN;")?;
    let names = db.table_names();
    for name in &names {
        let (create, order_by, n_indexes) = {
            let table = db.table(name).expect("listed table exists");
            (
                create_table_sql(table),
                pk_order_by(table),
                table.indexes.len(),
            )
        };
        writeln!(out, "{create}")?;
        let rows = db
            .query(&format!("SELECT * FROM {}{order_by}", q(name)), &[])
            .expect("a full-table scan of a cataloged table cannot fail");
        for row in rows {
            let values: Vec<String> = row.iter().map(Value::sql_literal).collect();
            writeln!(
                out,
                "INSERT INTO {} VALUES ({});",
                q(name),
                values.join(", ")
            )?;
        }
        for i in 0..n_indexes {
            let table = db.table(name).expect("listed table exists");
            let idx = &table.indexes[i];
            let unique = if idx.unique { "UNIQUE " } else { "" };
            // A column key dumps its column name; an EXPRESSION key its canonical text, wrapped in
            // parentheses so the emitted CREATE INDEX re-parses (a bare operator expression is a
            // syntax error — indexes.md §1).
            let cols: Vec<String> = idx
                .keys
                .iter()
                .map(|k| match k {
                    jed::tooling::IndexKey::Column(c) => q(&table.columns[*c].name),
                    jed::tooling::IndexKey::Expr(e) => format!("({})", e.expr_text),
                })
                .collect();
            // A PARTIAL index appends its WHERE predicate (canonical text, indexes.md §9) so a
            // replay rebuilds the same partial index; a full index has none.
            let where_clause = match &idx.predicate {
                Some(p) => format!(" WHERE {}", p.expr_text),
                None => String::new(),
            };
            writeln!(
                out,
                "CREATE {unique}INDEX {} ON {} ({}){where_clause};",
                q(&idx.name),
                q(&table.name),
                cols.join(", ")
            )?;
        }
    }
    // Views come after every table, its rows, and its indexes, in dependency order (a view after
    // every view it reads; ties in name byte order). The output column list is always written, so
    // the recreated view keeps its column names even when the body's own names differ. A check
    // option follows the body (views.md §15.6).
    for view in db.views_in_dependency_order() {
        let cols: Vec<String> = view.columns.iter().map(|c| q(c)).collect();
        let check = match view.check_option {
            None => "",
            Some(CheckOption::Local) => " WITH LOCAL CHECK OPTION",
            Some(CheckOption::Cascaded) => " WITH CASCADED CHECK OPTION",
        };
        writeln!(
            out,
            "CREATE VIEW {} ({}) AS {}{check};",
            q(&view.name),
            cols.join(", "),
            view.definition
        )?;
    }
    writeln!(out, "COMMIT;")
}

/// The CREATE TABLE statement for a catalog entry: columns with type (incl. the
/// `numeric(p,s)` typmod), NOT NULL (skipped on PK members — implied), DEFAULT; then a
/// table-level PRIMARY KEY in key order and each CHECK with its persisted name and text.
fn create_table_sql(table: &Table) -> String {
    let mut parts: Vec<String> = Vec::new();
    for col in &table.columns {
        let ty = match &col.decimal {
            Some(m) => format!("numeric({},{})", m.precision, m.scale),
            None => col.ty.canonical_name().to_string(),
        };
        let mut line = format!("  {} {ty}", q(&col.name));
        if col.not_null && !col.primary_key {
            line.push_str(" NOT NULL");
        }
        // The canonical DEFAULT text jed_columns.column_default shows (introspection.md §5): an
        // expression default's persisted expr-text (it re-parses to the same expression on replay),
        // or a constant's SQL literal. `DEFAULT NULL` is omitted — it is the same as no default.
        if let Some(default) = col.default_text() {
            line.push_str(" DEFAULT ");
            line.push_str(&default);
        }
        parts.push(line);
    }
    if !table.pk.is_empty() {
        let cols: Vec<String> = table
            .pk
            .iter()
            .map(|&i| q(&table.columns[i].name))
            .collect();
        parts.push(format!("  PRIMARY KEY ({})", cols.join(", ")));
    }
    for check in &table.checks {
        parts.push(format!(
            "  CONSTRAINT {} CHECK ({})",
            q(&check.name),
            check.expr_text
        ));
    }
    format!(
        "CREATE TABLE {} (\n{}\n);",
        q(&table.name),
        parts.join(",\n")
    )
}

/// `ORDER BY` over the primary-key columns in key order, or empty for a no-PK table.
fn pk_order_by(table: &Table) -> String {
    if table.pk.is_empty() {
        return String::new();
    }
    let cols: Vec<String> = table
        .pk
        .iter()
        .map(|&i| q(&table.columns[i].name))
        .collect();
    format!(" ORDER BY {}", cols.join(", "))
}

/// A name as SQL identifier text (grammar.md §3's quoting rule), so a mixed-case or otherwise
/// non-plain name round-trips through the dump.
fn q(name: &str) -> String {
    jed::tooling::quote_identifier(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dump_to_string(db: &mut jed::Session) -> String {
        let mut buf = Vec::new();
        dump(db, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn rich_db() -> jed::Session {
        let mut db = jed::Database::create(jed::CreateOptions::default())
            .expect("in-memory create is infallible")
            .session(jed::SessionOptions::default());
        for sql in [
            "CREATE TABLE users (
                id i32,
                age i32 UNIQUE,
                name text NOT NULL,
                score numeric(5,2) DEFAULT 0.50,
                ok boolean DEFAULT true,
                n i32 DEFAULT 1 + 1,
                blob bytea,
                PRIMARY KEY (id),
                CONSTRAINT score_pos CHECK (score >= 0)
             )",
            "CREATE INDEX users_age_idx ON users (age)",
            "CREATE INDEX users_ok_idx ON users (age) WHERE ok",
            "INSERT INTO users (id, age, name, blob) VALUES (2, 30, 'it''s bob', '\\x6869')",
            "INSERT INTO users (id, age, name, score) VALUES (1, 40, 'alice', 9.50)",
            "CREATE TABLE nopk (v i64)",
            "INSERT INTO nopk VALUES (10), (20)",
            // `a_top` reads `z_base`, so dependency order puts it after `z_base` even though it
            // sorts first by name; `Mixed` leads by name byte order.
            "CREATE VIEW z_base AS SELECT id, name FROM users WHERE age > 18 WITH LOCAL CHECK OPTION",
            "CREATE VIEW a_top (n) AS SELECT name FROM z_base",
            "CREATE VIEW \"Mixed\" AS SELECT * FROM nopk",
        ] {
            db.execute(sql, &[]).unwrap();
        }
        db
    }

    #[test]
    fn dump_emits_schema_rows_and_indexes_in_order() {
        let mut db = rich_db();
        assert_eq!(
            dump_to_string(&mut db),
            "BEGIN;\n\
             CREATE TABLE nopk (\n  v i64\n);\n\
             INSERT INTO nopk VALUES (10);\n\
             INSERT INTO nopk VALUES (20);\n\
             CREATE TABLE users (\n\
             \x20 id i32,\n\
             \x20 age i32,\n\
             \x20 name text NOT NULL,\n\
             \x20 score numeric(5,2) DEFAULT 0.50,\n\
             \x20 ok boolean DEFAULT true,\n\
             \x20 n i32 DEFAULT 1 + 1,\n\
             \x20 blob bytea,\n\
             \x20 PRIMARY KEY (id),\n\
             \x20 CONSTRAINT score_pos CHECK (score >= 0)\n\
             );\n\
             INSERT INTO users VALUES (1, 40, 'alice', 9.50, true, 2, NULL);\n\
             INSERT INTO users VALUES (2, 30, 'it''s bob', 0.50, true, 2, '\\x6869');\n\
             CREATE INDEX users_age_idx ON users (age);\n\
             CREATE UNIQUE INDEX users_age_key ON users (age);\n\
             CREATE INDEX users_ok_idx ON users (age) WHERE ok;\n\
             CREATE VIEW \"Mixed\" (v) AS select \"nopk\" . \"v\" from nopk;\n\
             CREATE VIEW z_base (id, name) AS select id , name from users where age > 18 WITH LOCAL CHECK OPTION;\n\
             CREATE VIEW a_top (n) AS select name from z_base;\n\
             COMMIT;\n"
        );
    }

    #[test]
    fn dump_replays_to_an_identical_dump() {
        let mut db = rich_db();
        let first = dump_to_string(&mut db);
        let mut replayed = jed::Database::create(jed::CreateOptions::default())
            .expect("in-memory create is infallible")
            .session(jed::SessionOptions::default());
        for stmt in crate::splitter::split(&first).unwrap() {
            replayed.execute(&stmt.sql, &[]).unwrap();
        }
        assert_eq!(dump_to_string(&mut replayed), first);
    }

    #[test]
    fn empty_database_dumps_an_empty_transaction() {
        let mut db = jed::Database::create(jed::CreateOptions::default())
            .expect("in-memory create is infallible")
            .session(jed::SessionOptions::default());
        assert_eq!(dump_to_string(&mut db), "BEGIN;\nCOMMIT;\n");
    }
}
