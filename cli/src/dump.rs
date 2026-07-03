//! `.dump`-style SQL export (spec/design/cli.md §3): `--dump` writes a SQL script that
//! recreates the database — schema (CREATE TABLE with PRIMARY KEY / NOT NULL / DEFAULT /
//! CHECK), one INSERT per row, then CREATE INDEX statements (after the rows, so a replay
//! builds each index once) — wrapped in one BEGIN/COMMIT so a file-backed replay commits
//! durably once. Replaying the script into a fresh database yields the same logical state:
//! dump → replay → dump is byte-identical.

use std::io::{self, Write};

use jed::Value;
use jed::tooling::Table;

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
            .query(&format!("SELECT * FROM {name}{order_by}"), &[])
            .expect("a full-table scan of a cataloged table cannot fail");
        for row in rows {
            let values: Vec<String> = row.iter().map(value_literal).collect();
            writeln!(out, "INSERT INTO {name} VALUES ({});", values.join(", "))?;
        }
        for i in 0..n_indexes {
            let table = db.table(name).expect("listed table exists");
            let idx = &table.indexes[i];
            let unique = if idx.unique { "UNIQUE " } else { "" };
            let cols: Vec<&str> = idx
                .columns
                .iter()
                .map(|&c| table.columns[c].name.as_str())
                .collect();
            writeln!(
                out,
                "CREATE {unique}INDEX {} ON {} ({});",
                idx.name,
                table.name,
                cols.join(", ")
            )?;
        }
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
        let mut line = format!("  {} {ty}", col.name);
        if col.not_null && !col.primary_key {
            line.push_str(" NOT NULL");
        }
        if let Some(default) = &col.default {
            line.push_str(" DEFAULT ");
            line.push_str(&value_literal(default));
        } else if let Some(de) = &col.default_expr {
            // An EXPRESSION default (constraints.md §2) dumps its persisted expr-text verbatim —
            // it re-parses to the same expression on replay (the same token contract a CHECK uses).
            line.push_str(" DEFAULT ");
            line.push_str(&de.expr_text);
        }
        parts.push(line);
    }
    if !table.pk.is_empty() {
        let cols: Vec<&str> = table
            .pk
            .iter()
            .map(|&i| table.columns[i].name.as_str())
            .collect();
        parts.push(format!("  PRIMARY KEY ({})", cols.join(", ")));
    }
    for check in &table.checks {
        parts.push(format!(
            "  CONSTRAINT {} CHECK ({})",
            check.name, check.expr_text
        ));
    }
    format!("CREATE TABLE {} (\n{}\n);", table.name, parts.join(",\n"))
}

/// `ORDER BY` over the primary-key columns in key order, or empty for a no-PK table.
fn pk_order_by(table: &Table) -> String {
    if table.pk.is_empty() {
        return String::new();
    }
    let cols: Vec<&str> = table
        .pk
        .iter()
        .map(|&i| table.columns[i].name.as_str())
        .collect();
    format!(" ORDER BY {}", cols.join(", "))
}

/// A SQL literal that parses back to `v`: ints/decimals/booleans as bare tokens, NULL as
/// the keyword, everything else as a quoted string the engine coerces by column context
/// (text, `\x`-hex bytea, uuid, timestamps — their canonical `render()` forms all re-parse).
pub fn value_literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Int(_) | Value::Bool(_) | Value::Decimal(_) => v.render(),
        other => format!("'{}'", other.render().replace('\'', "''")),
    }
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
            "INSERT INTO users (id, age, name, blob) VALUES (2, 30, 'it''s bob', '\\x6869')",
            "INSERT INTO users (id, age, name, score) VALUES (1, 40, 'alice', 9.50)",
            "CREATE TABLE nopk (v i64)",
            "INSERT INTO nopk VALUES (10), (20)",
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
