//! `.dump`-style SQL export (spec/design/cli.md §3): `--dump` writes a SQL script that
//! recreates the database — schema (CREATE TABLE with PRIMARY KEY / NOT NULL / DEFAULT /
//! CHECK), one INSERT per row, then CREATE INDEX statements (after the rows, so a replay
//! builds each index once), then every view (spec/design/views.md §11) — wrapped in one
//! BEGIN/COMMIT so a file-backed replay commits durably once. Replaying the script into a fresh
//! database yields the same logical state: dump → replay → dump is byte-identical.

use std::io::{self, Write};

use jed::Value;
use jed::tooling::{CheckOption, IdentityKind, Table};

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
        let identity: Vec<String> = {
            let table = db.table(name).expect("listed table exists");
            identity_sql(db, table)
        };
        for stmt in identity {
            writeln!(out, "{stmt}")?;
        }
        for i in 0..n_indexes {
            let table = db.table(name).expect("listed table exists");
            let idx = &table.indexes[i];
            let unique = if idx.unique { "UNIQUE " } else { "" };
            // A column key dumps its column name; an EXPRESSION key its canonical text, wrapped in
            // parentheses so the emitted CREATE INDEX re-parses (a bare operator expression is a
            // syntax error — indexes.md §1).
            // Each key's non-default sort order follows it as PostgreSQL's pg_get_indexdef prints it
            // (indexes.md §11): ` DESC`, plus ` NULLS LAST` on a DESC key or ` NULLS FIRST` on an
            // ASC key when the placement is not the direction's default.
            let cols: Vec<String> = idx
                .keys
                .iter()
                .enumerate()
                .map(|(i, k)| {
                    let key = match k {
                        jed::tooling::IndexKey::Column(c) => q(&table.columns[*c].name),
                        jed::tooling::IndexKey::Expr(e) => format!("({})", e.expr_text),
                    };
                    let order = idx.key_order(i);
                    match (order.desc, order.nulls_first) {
                        (false, false) => key,
                        (false, true) => format!("{key} NULLS FIRST"),
                        (true, true) => format!("{key} DESC"),
                        (true, false) => format!("{key} DESC NULLS LAST"),
                    }
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
    // option follows the body (views.md §15.6); the other options precede it as a WITH list, and
    // each column default follows as an ALTER VIEW (§16).
    for view in db.views_in_dependency_order() {
        let cols: Vec<String> = view.columns.iter().map(|c| q(c)).collect();
        let check = match view.check_option {
            None => "",
            Some(CheckOption::Local) => " WITH LOCAL CHECK OPTION",
            Some(CheckOption::Cascaded) => " WITH CASCADED CHECK OPTION",
        };
        let mut options: Vec<&str> = Vec::new();
        if view.security_barrier {
            options.push("security_barrier");
        }
        if view.security_invoker {
            options.push("security_invoker");
        }
        let options = if options.is_empty() {
            String::new()
        } else {
            format!(" WITH ({})", options.join(", "))
        };
        writeln!(
            out,
            "CREATE VIEW {} ({}){options} AS {}{check};",
            q(&view.name),
            cols.join(", "),
            view.definition
        )?;
        for (col, default) in view.columns.iter().zip(&view.defaults) {
            if let Some(default) = default {
                writeln!(
                    out,
                    "ALTER VIEW {} ALTER COLUMN {} SET DEFAULT {};",
                    q(&view.name),
                    q(col),
                    default.expr_text
                )?;
            }
        }
    }
    writeln!(out, "COMMIT;")
}

/// The CREATE TABLE statement for a catalog entry: columns with their canonical type text
/// (introspection.md §5, typmods included: `decimal(p,s)`, `varchar(n)`), NOT
/// NULL (skipped on PK members — implied), DEFAULT; then a table-level PRIMARY KEY in key
/// order and each CHECK with its persisted name and text.
fn create_table_sql(table: &Table) -> String {
    let mut parts: Vec<String> = Vec::new();
    for col in &table.columns {
        let mut line = format!("  {} {}", q(&col.name), col.type_text());
        if col.not_null && !col.primary_key {
            line.push_str(" NOT NULL");
        }
        // The canonical DEFAULT text jed_columns.column_default shows (introspection.md §5): an
        // expression default's persisted expr-text (it re-parses to the same expression on replay),
        // or a constant's SQL literal. `DEFAULT NULL` is omitted — it is the same as no default. An
        // identity column's synthesized `nextval` default is not written: `identity_sql` adds the
        // identity back after the rows.
        if col.identity.is_none()
            && let Some(default) = col.default_text()
        {
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

/// The statements that make each identity column of `table` an identity column again once its rows
/// are in (cli.md §3): `ALTER TABLE … ADD GENERATED … AS IDENTITY` naming the existing sequence and
/// spelling out every option, then — unless the counter is untouched — a `setval` that restores it.
/// Adding the identity after the INSERTs lets a GENERATED ALWAYS column take its dumped values.
fn identity_sql(db: &jed::Session, table: &Table) -> Vec<String> {
    let mut out = Vec::new();
    for col in &table.columns {
        let Some(kind) = col.identity else {
            continue;
        };
        let Some(seq) = col
            .default_expr
            .as_ref()
            .and_then(|d| nextval_sequence_name(&d.expr_text))
            .and_then(|n| db.sequence(&n))
        else {
            continue;
        };
        let when = match kind {
            IdentityKind::Always => "ALWAYS",
            IdentityKind::ByDefault => "BY DEFAULT",
        };
        out.push(format!(
            "ALTER TABLE {} ALTER COLUMN {} ADD GENERATED {when} AS IDENTITY (SEQUENCE NAME {} \
             START WITH {} INCREMENT BY {} MINVALUE {} MAXVALUE {} CACHE {} {});",
            q(&table.name),
            q(&col.name),
            q(&seq.name),
            seq.start,
            seq.increment,
            seq.min_value,
            seq.max_value,
            seq.cache,
            if seq.cycle { "CYCLE" } else { "NO CYCLE" },
        ));
        if seq.is_called || seq.last_value != seq.start {
            out.push(format!(
                "SELECT setval('{}', {}, {});",
                q(&seq.name).replace('\'', "''"),
                seq.last_value,
                seq.is_called
            ));
        }
    }
    out
}

/// The sequence an identity column's synthesized default names: the name text inside its
/// `nextval ( '…' )` literal (`''` undoubled), parsed by grammar.md §3's name rule.
fn nextval_sequence_name(default_text: &str) -> Option<String> {
    let start = default_text.find("nextval ( '")? + "nextval ( '".len();
    let mut text = String::new();
    let mut chars = default_text[start..].chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            if chars.peek() == Some(&'\'') {
                chars.next();
            } else {
                return jed::tooling::parse_name_text(&text).ok();
            }
        }
        text.push(c);
    }
    None
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
            "CREATE INDEX users_ord_idx ON users (age DESC, name NULLS FIRST, id DESC NULLS LAST)",
            "INSERT INTO users (id, age, name, blob) VALUES (2, 30, 'it''s bob', '\\x6869')",
            "INSERT INTO users (id, age, name, score) VALUES (1, 40, 'alice', 9.50)",
            "CREATE TABLE nopk (v i64)",
            "INSERT INTO nopk VALUES (10), (20)",
            "CREATE TABLE typed (
                id i32 PRIMARY KEY,
                code varchar(10),
                tags text[],
                amounts decimal[],
                span i32range
             )",
            "INSERT INTO typed VALUES (1, 'abc', ARRAY['x', 'yz'], ARRAY[1.50, 2.00], '[1,5)')",
            // `a_top` reads `z_base`, so dependency order puts it after `z_base` even though it
            // sorts first by name; `Mixed` leads by name byte order.
            "CREATE VIEW z_base AS SELECT id, name FROM users WHERE age > 18 WITH LOCAL CHECK OPTION",
            "CREATE VIEW a_top (n) AS SELECT name FROM z_base",
            "CREATE VIEW \"Mixed\" WITH (security_barrier) AS SELECT * FROM nopk",
            "ALTER VIEW z_base ALTER COLUMN name SET DEFAULT 'anon', ALTER id SET DEFAULT 1 + 1",
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
             CREATE TABLE typed (\n\
             \x20 id i32,\n\
             \x20 code varchar(10),\n\
             \x20 tags text[],\n\
             \x20 amounts decimal[],\n\
             \x20 span i32range,\n\
             \x20 PRIMARY KEY (id)\n\
             );\n\
             INSERT INTO typed VALUES (1, 'abc', '{x,yz}', '{1.50,2.00}', '[1,5)');\n\
             CREATE TABLE users (\n\
             \x20 id i32,\n\
             \x20 age i32,\n\
             \x20 name text NOT NULL,\n\
             \x20 score decimal(5,2) DEFAULT 0.50,\n\
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
             CREATE INDEX users_ord_idx ON users (age DESC, name NULLS FIRST, id DESC NULLS LAST);\n\
             CREATE VIEW \"Mixed\" (v) WITH (security_barrier) AS select \"nopk\" . \"v\" from nopk;\n\
             CREATE VIEW z_base (id, name) AS select id , name from users where age > 18 WITH LOCAL CHECK OPTION;\n\
             ALTER VIEW z_base ALTER COLUMN id SET DEFAULT 1 + 1;\n\
             ALTER VIEW z_base ALTER COLUMN name SET DEFAULT 'anon';\n\
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
        // The replayed `varchar(10)` keeps its length check.
        let err = replayed
            .execute(
                "INSERT INTO typed (id, code) VALUES (2, 'abcdefghijk')",
                &[],
            )
            .unwrap_err();
        assert_eq!(err.code(), "22001");
    }

    #[test]
    fn empty_database_dumps_an_empty_transaction() {
        let mut db = jed::Database::create(jed::CreateOptions::default())
            .expect("in-memory create is infallible")
            .session(jed::SessionOptions::default());
        assert_eq!(dump_to_string(&mut db), "BEGIN;\nCOMMIT;\n");
    }

    // Identity columns dump as plain NOT NULL columns, get their rows, and then become identity
    // columns again under their own sequence name and state (cli.md §3) — so an ALWAYS column takes
    // its dumped values, a renamed sequence keeps its name, and the counter carries over.
    #[test]
    fn identity_columns_round_trip_with_their_sequences() {
        let mut db = jed::Database::create(jed::CreateOptions::default())
            .expect("in-memory create is infallible")
            .session(jed::SessionOptions::default());
        for sql in [
            "CREATE TABLE ids (id i32 GENERATED ALWAYS AS IDENTITY (START 5 INCREMENT 5) PRIMARY KEY, \
             b i16 NOT NULL, fresh i64 GENERATED BY DEFAULT AS IDENTITY)",
            "INSERT INTO ids (b) VALUES (1), (2)",
            "ALTER TABLE ids ALTER b ADD GENERATED BY DEFAULT AS IDENTITY (SEQUENCE NAME \"B's seq\" CYCLE MAXVALUE 9)",
            "ALTER TABLE ids ALTER b RESTART WITH 7",
        ] {
            db.execute(sql, &[]).unwrap();
        }
        let first = dump_to_string(&mut db);
        assert_eq!(
            first,
            "BEGIN;\n\
             CREATE TABLE ids (\n\
             \x20 id i32,\n\
             \x20 b i16 NOT NULL,\n\
             \x20 fresh i64 NOT NULL,\n\
             \x20 PRIMARY KEY (id)\n\
             );\n\
             INSERT INTO ids VALUES (5, 1, 1);\n\
             INSERT INTO ids VALUES (10, 2, 2);\n\
             ALTER TABLE ids ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (SEQUENCE NAME ids_id_seq \
             START WITH 5 INCREMENT BY 5 MINVALUE 1 MAXVALUE 2147483647 CACHE 1 NO CYCLE);\n\
             SELECT setval('ids_id_seq', 10, true);\n\
             ALTER TABLE ids ALTER COLUMN b ADD GENERATED BY DEFAULT AS IDENTITY (SEQUENCE NAME \"B's seq\" \
             START WITH 1 INCREMENT BY 1 MINVALUE 1 MAXVALUE 9 CACHE 1 CYCLE);\n\
             SELECT setval('\"B''s seq\"', 7, false);\n\
             ALTER TABLE ids ALTER COLUMN fresh ADD GENERATED BY DEFAULT AS IDENTITY (SEQUENCE NAME ids_fresh_seq \
             START WITH 1 INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 CACHE 1 NO CYCLE);\n\
             SELECT setval('ids_fresh_seq', 2, true);\n\
             COMMIT;\n"
        );
        let mut replayed = jed::Database::create(jed::CreateOptions::default())
            .expect("in-memory create is infallible")
            .session(jed::SessionOptions::default());
        for stmt in crate::splitter::split(&first).unwrap() {
            replayed.execute(&stmt.sql, &[]).unwrap();
        }
        assert_eq!(dump_to_string(&mut replayed), first);
        // The replayed ALWAYS column still refuses an explicit value and continues its sequence.
        let err = replayed
            .execute("INSERT INTO ids (id, b, fresh) VALUES (99, 1, 1)", &[])
            .unwrap_err();
        assert_eq!(err.code(), "428C9");
        replayed
            .execute("INSERT INTO ids (b, fresh) VALUES (3, 3)", &[])
            .unwrap();
        let rows = replayed
            .query("SELECT id FROM ids WHERE b = 3", &[])
            .unwrap();
        let ids: Vec<Vec<jed::Value>> = rows.collect();
        assert_eq!(ids, vec![vec![jed::Value::Int(15)]]);
    }
}
