//! CSV import (spec/design/cli.md §3): `--import-csv TABLE=FILE` parses an RFC 4180 file
//! (the same dialect `--format csv` writes), maps its header row to the table's columns,
//! and synthesizes ONE multi-row `INSERT INTO t (cols) VALUES ...` statement — so an import
//! is atomic (the engine's per-statement all-or-nothing) and reports through the ordinary
//! `OK, N rows (cost C)` path. Every value reaches the engine as a typed SQL literal built
//! from the column's declared type; the engine's own coercion/constraint checks still run.

use jed::tooling::ScalarType;
use jed::tooling::Table;

/// One parsed CSV field. `quoted` distinguishes `""` (the empty string) from a bare empty
/// field, which imports as NULL — the PG `COPY ... CSV` convention, the inverse of the
/// `--format csv` writer's NULL policy (cli.md §5).
#[derive(Debug, PartialEq, Eq)]
pub struct Field {
    pub value: String,
    pub quoted: bool,
}

/// Parse RFC 4180 text: `,` separators, `"` quoting with `""` escaping, LF or CRLF row
/// terminators, quoted fields may span lines. Returns the records (including the header);
/// a trailing newline does not produce an empty record.
pub fn parse(text: &str) -> Result<Vec<Vec<Field>>, String> {
    let mut records = Vec::new();
    let mut record: Vec<Field> = Vec::new();
    let mut field = String::new();
    let mut quoted = false; // the CURRENT field was opened with a quote
    let mut in_quotes = false;
    let mut line = 1usize;
    let mut chars = text.chars().peekable();

    // A record ends at a newline outside quotes; a lone \r before \n is consumed with it.
    let mut field_started = false;
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        field.push('"');
                    } else {
                        in_quotes = false;
                    }
                }
                '\n' => {
                    line += 1;
                    field.push(c);
                }
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if !field_started => {
                quoted = true;
                field_started = true;
                in_quotes = true;
            }
            '"' => return Err(format!("line {line}: unexpected quote inside a field")),
            ',' => {
                record.push(Field {
                    value: std::mem::take(&mut field),
                    quoted,
                });
                quoted = false;
                field_started = false;
            }
            '\r' if chars.peek() == Some(&'\n') => {} // CRLF: handled at the \n
            '\n' => {
                record.push(Field {
                    value: std::mem::take(&mut field),
                    quoted,
                });
                quoted = false;
                field_started = false;
                records.push(std::mem::take(&mut record));
                line += 1;
            }
            _ => {
                field_started = true;
                field.push(c);
            }
        }
    }
    if in_quotes {
        return Err(format!("line {line}: unterminated quoted field"));
    }
    if field_started || !record.is_empty() {
        record.push(Field {
            value: field,
            quoted,
        });
        records.push(record);
    }
    Ok(records)
}

/// Build the single atomic `INSERT` for `records` (header first) against `table`. The
/// header's names map case-insensitively to table columns (unknown or duplicate names are
/// errors); table columns absent from the CSV take their DEFAULT/NULL through the INSERT
/// column list. Returns `None` for a header-only file (nothing to import).
pub fn import_statement(table: &Table, records: &[Vec<Field>]) -> Result<Option<String>, String> {
    let Some((header, data)) = records.split_first() else {
        return Err("empty file (a header row is required)".to_string());
    };
    let mut cols: Vec<usize> = Vec::with_capacity(header.len());
    for h in header {
        let idx = table
            .column_index(&h.value)
            .ok_or_else(|| format!("column {} does not exist in table {}", h.value, table.name))?;
        if cols.contains(&idx) {
            return Err(format!("duplicate column {} in the header", h.value));
        }
        cols.push(idx);
    }
    if data.is_empty() {
        return Ok(None);
    }

    let mut sql = String::from("INSERT INTO ");
    sql.push_str(&table.name);
    sql.push_str(" (");
    let names: Vec<&str> = cols
        .iter()
        .map(|&i| table.columns[i].name.as_str())
        .collect();
    sql.push_str(&names.join(", "));
    sql.push_str(") VALUES ");
    for (r, record) in data.iter().enumerate() {
        if record.len() != header.len() {
            return Err(format!(
                "row {}: {} fields, header has {}",
                r + 1,
                record.len(),
                header.len()
            ));
        }
        if r > 0 {
            sql.push_str(", ");
        }
        sql.push('(');
        for (f, field) in record.iter().enumerate() {
            if f > 0 {
                sql.push_str(", ");
            }
            let lit = literal(&table.columns[cols[f]].ty.scalar(), field)
                .map_err(|e| format!("row {}, column {}: {e}", r + 1, names[f]))?;
            sql.push_str(&lit);
        }
        sql.push(')');
    }
    Ok(Some(sql))
}

/// Render one CSV field as a SQL literal for a column of type `ty`. An UNQUOTED empty
/// field is NULL (PG `COPY ... CSV`); a quoted one is the empty string / an invalid
/// numeric. Numeric and boolean tokens are validated here (so a malformed field fails
/// with the row/column position, not an opaque parse error from the synthesized SQL);
/// everything else is a quoted string literal the engine coerces per its type rules.
fn literal(ty: &ScalarType, field: &Field) -> Result<String, String> {
    if field.value.is_empty() && !field.quoted {
        return Ok("NULL".to_string());
    }
    let v = field.value.as_str();
    match ty {
        ScalarType::Int16 | ScalarType::Int32 | ScalarType::Int64 => {
            let digits = v.strip_prefix(['+', '-']).unwrap_or(v);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("not a valid integer: {v}"));
            }
            Ok(v.to_string())
        }
        ScalarType::Decimal => {
            let digits = v.strip_prefix(['+', '-']).unwrap_or(v);
            let (int, frac) = match digits.split_once('.') {
                Some((i, f)) => (i, f),
                None => (digits, ""),
            };
            let all_digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
            if (int.is_empty() && frac.is_empty()) || !all_digits(int) || !all_digits(frac) {
                return Err(format!("not a valid numeric: {v}"));
            }
            Ok(v.to_string())
        }
        ScalarType::Bool => match v.to_ascii_lowercase().as_str() {
            // `t`/`f` accepted for PG COPY interop; jed's own export writes true/false.
            "true" | "t" => Ok("true".to_string()),
            "false" | "f" => Ok("false".to_string()),
            other => Err(format!("not a valid boolean: {other}")),
        },
        // Text and the string-literal-typed scalars (bytea, uuid, timestamp[tz]): a quoted
        // SQL string; the engine's context coercion + validity checks (22P02, 22007) run.
        _ => Ok(format!("'{}'", v.replace('\'', "''"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(value: &str) -> Field {
        Field {
            value: value.to_string(),
            quoted: false,
        }
    }
    fn q(value: &str) -> Field {
        Field {
            value: value.to_string(),
            quoted: true,
        }
    }

    #[test]
    fn parses_quotes_escapes_newlines_and_crlf() {
        let records = parse("a,b\r\n1,\"x,\"\"y\"\"\nz\"\n,\"\"\n").unwrap();
        assert_eq!(records[0], vec![f("a"), f("b")]);
        assert_eq!(records[1], vec![f("1"), q("x,\"y\"\nz")]);
        // Bare empty vs quoted empty survive distinctly (NULL vs '').
        assert_eq!(records[2], vec![f(""), q("")]);
        // No trailing empty record from the final newline.
        assert_eq!(records.len(), 3);
    }

    #[test]
    fn parse_rejects_malformed_input() {
        assert!(parse("a,\"unterminated").is_err());
        assert!(parse("a,b\"c").is_err());
    }

    fn demo_table() -> jed::Session {
        let mut db = jed::Database::create(jed::CreateOptions::default())
            .expect("in-memory create is infallible")
            .session(jed::SessionOptions::default());
        db.execute("CREATE TABLE t (id i32 PRIMARY KEY, name text, score numeric(5,2), ok boolean DEFAULT true)", &[])
        .unwrap();
        db
    }

    #[test]
    fn builds_one_atomic_insert_with_typed_literals() {
        let db = demo_table();
        let table = db.table("t").unwrap();
        let records = parse("id,name,score\n1,alice,9.50\n2,,\n3,\"\",1.00\n").unwrap();
        let sql = import_statement(table, &records).unwrap().unwrap();
        assert_eq!(
            sql,
            "INSERT INTO t (id, name, score) VALUES \
             (1, 'alice', 9.50), (2, NULL, NULL), (3, '', 1.00)"
        );
    }

    #[test]
    fn header_maps_case_insensitively_and_rejects_unknowns() {
        let db = demo_table();
        let table = db.table("t").unwrap();
        let ok = parse("ID,NAME\n1,x\n").unwrap();
        assert!(import_statement(table, &ok).unwrap().is_some());
        let unknown = parse("id,nope\n1,x\n").unwrap();
        assert!(
            import_statement(table, &unknown)
                .unwrap_err()
                .contains("nope")
        );
        let dup = parse("id,ID\n1,2\n").unwrap();
        assert!(
            import_statement(table, &dup)
                .unwrap_err()
                .contains("duplicate")
        );
    }

    #[test]
    fn header_only_imports_nothing_and_bad_fields_carry_positions() {
        let db = demo_table();
        let table = db.table("t").unwrap();
        assert_eq!(
            import_statement(table, &parse("id,name\n").unwrap()).unwrap(),
            None
        );
        let bad_int = parse("id\nseven\n").unwrap();
        assert!(
            import_statement(table, &bad_int)
                .unwrap_err()
                .contains("row 1, column id")
        );
        let short = parse("id,name\n1\n").unwrap();
        assert!(
            import_statement(table, &short)
                .unwrap_err()
                .contains("row 1")
        );
    }

    #[test]
    fn booleans_accept_pg_copy_spellings() {
        let db = demo_table();
        let table = db.table("t").unwrap();
        let records = parse("id,ok\n1,t\n2,FALSE\n").unwrap();
        let sql = import_statement(table, &records).unwrap().unwrap();
        assert_eq!(sql, "INSERT INTO t (id, ok) VALUES (1, true), (2, false)");
    }

    #[test]
    fn imported_statement_round_trips_through_the_engine() {
        let mut db = demo_table();
        let records = parse("id,name,score\n1,\"a,b\",1.50\n2,\"say \"\"hi\"\"\",\n").unwrap();
        let sql = {
            let table = db.table("t").unwrap();
            import_statement(table, &records).unwrap().unwrap()
        };
        db.execute(&sql, &[]).unwrap();
        // Drain the total `query` seam (a statement is a no-column cursor; a SELECT carries rows).
        let counted: Vec<Vec<jed::Value>> =
            db.query("SELECT count(*) FROM t", &[]).unwrap().collect();
        assert_eq!(counted[0][0], jed::Value::Int(2));
    }
}
