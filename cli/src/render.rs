//! Output formatters (spec/design/cli.md §5). Every cell renders through the engine's
//! canonical `Value::render()` — byte-identical to the conformance corpus — in every
//! format; the only CLI-specific value handling is the per-format NULL policy and the
//! JSON scalar mapping.

use std::io::{self, Write};

use jed::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Aligned,
    Box,
    Markdown,
    Csv,
    Json,
}

impl Format {
    pub fn parse(name: &str) -> Option<Format> {
        match name {
            "aligned" => Some(Format::Aligned),
            "box" => Some(Format::Box),
            "markdown" => Some(Format::Markdown),
            "csv" => Some(Format::Csv),
            "json" => Some(Format::Json),
            _ => None,
        }
    }
}

/// Write one query result. The footer `(N rows, cost C)` prints only for the human
/// formats (`aligned`, `box`) — markdown/csv/json are pure data, always (cli.md §5).
pub fn write_query(
    out: &mut dyn Write,
    format: Format,
    columns: &[String],
    rows: &[Vec<Value>],
    cost: i64,
) -> io::Result<()> {
    match format {
        Format::Aligned => write_aligned(out, columns, rows, cost),
        Format::Box => write_box(out, columns, rows, cost),
        Format::Markdown => write_markdown(out, columns, rows),
        Format::Csv => write_csv(out, columns, rows),
        Format::Json => write_json(out, columns, rows),
    }
}

/// The aligned-table cell grid, rendered: header row first, then one row per result row.
/// Shared by the script-mode writer and the TUI results grid.
pub fn rendered_grid(columns: &[String], rows: &[Vec<Value>]) -> Vec<Vec<String>> {
    let mut grid = Vec::with_capacity(rows.len() + 1);
    grid.push(columns.to_vec());
    for row in rows {
        grid.push(row.iter().map(Value::render).collect());
    }
    grid
}

/// Per-column right-alignment: a column whose values are all int/decimal (NULLs allowed)
/// reads as numeric. Shared by the script-mode writer and the TUI results grid.
pub fn numeric_columns(columns: &[String], rows: &[Vec<Value>]) -> Vec<bool> {
    (0..columns.len())
        .map(|c| {
            !rows.is_empty()
                && rows
                    .iter()
                    .all(|r| matches!(r[c], Value::Int(_) | Value::Decimal(_) | Value::Null))
        })
        .collect()
}

fn write_aligned(
    out: &mut dyn Write,
    columns: &[String],
    rows: &[Vec<Value>],
    cost: i64,
) -> io::Result<()> {
    let grid = rendered_grid(columns, rows);
    let numeric = numeric_columns(columns, rows);
    let widths: Vec<usize> = (0..columns.len())
        .map(|c| grid.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();

    let write_row = |out: &mut dyn Write, cells: &[String]| -> io::Result<()> {
        let mut parts = Vec::with_capacity(cells.len());
        for (c, cell) in cells.iter().enumerate() {
            let pad = widths[c] - cell.chars().count();
            let padded = if numeric[c] {
                format!("{}{}", " ".repeat(pad), cell)
            } else {
                format!("{}{}", cell, " ".repeat(pad))
            };
            parts.push(padded);
        }
        // Trailing spaces on the last column are noise; trim the joined line's right edge.
        writeln!(out, " {}", parts.join(" | ").trim_end())
    };

    write_row(out, &grid[0])?;
    let rule: Vec<String> = widths.iter().map(|w| "-".repeat(w + 2)).collect();
    writeln!(out, "{}", rule.join("+"))?;
    for row in &grid[1..] {
        write_row(out, row)?;
    }
    let noun = if rows.len() == 1 { "row" } else { "rows" };
    writeln!(out, "({} {noun}, cost {cost})", rows.len())
}

/// Unicode box-drawing table (cli.md §5): the aligned layout framed with `┌─┬─┐` /
/// `├─┼─┤` / `└─┴─┘` rules, same alignment policy and footer as `aligned`.
fn write_box(
    out: &mut dyn Write,
    columns: &[String],
    rows: &[Vec<Value>],
    cost: i64,
) -> io::Result<()> {
    let grid = rendered_grid(columns, rows);
    let numeric = numeric_columns(columns, rows);
    let widths: Vec<usize> = (0..columns.len())
        .map(|c| grid.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();

    let rule = |out: &mut dyn Write, left: &str, mid: &str, right: &str| -> io::Result<()> {
        let bars: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        writeln!(out, "{left}{}{right}", bars.join(mid))
    };
    let write_row = |out: &mut dyn Write, cells: &[String]| -> io::Result<()> {
        let mut parts = Vec::with_capacity(cells.len());
        for (c, cell) in cells.iter().enumerate() {
            let pad = widths[c] - cell.chars().count();
            let padded = if numeric[c] {
                format!("{}{}", " ".repeat(pad), cell)
            } else {
                format!("{}{}", cell, " ".repeat(pad))
            };
            parts.push(padded);
        }
        writeln!(out, "│ {} │", parts.join(" │ "))
    };

    rule(out, "┌", "┬", "┐")?;
    write_row(out, &grid[0])?;
    rule(out, "├", "┼", "┤")?;
    for row in &grid[1..] {
        write_row(out, row)?;
    }
    rule(out, "└", "┴", "┘")?;
    let noun = if rows.len() == 1 { "row" } else { "rows" };
    writeln!(out, "({} {noun}, cost {cost})", rows.len())
}

/// GitHub-flavored markdown table (cli.md §5): pure data, no footer. Pipes are escaped
/// and embedded newlines become `<br>` so a cell cannot break the table; the alignment
/// row carries `---:` for numeric columns.
fn write_markdown(out: &mut dyn Write, columns: &[String], rows: &[Vec<Value>]) -> io::Result<()> {
    let escape = |s: &str| {
        s.replace('|', "\\|")
            .replace("\r\n", "<br>")
            .replace(['\n', '\r'], "<br>")
    };
    let mut grid = rendered_grid(columns, rows);
    for row in &mut grid {
        for cell in row.iter_mut() {
            *cell = escape(cell);
        }
    }
    let numeric = numeric_columns(columns, rows);
    let widths: Vec<usize> = (0..columns.len())
        .map(|c| grid.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();

    let write_row = |out: &mut dyn Write, cells: &[String]| -> io::Result<()> {
        let mut parts = Vec::with_capacity(cells.len());
        for (c, cell) in cells.iter().enumerate() {
            let pad = widths[c] - cell.chars().count();
            let padded = if numeric[c] {
                format!("{}{}", " ".repeat(pad), cell)
            } else {
                format!("{}{}", cell, " ".repeat(pad))
            };
            parts.push(padded);
        }
        writeln!(out, "| {} |", parts.join(" | "))
    };

    write_row(out, &grid[0])?;
    let seps: Vec<String> = widths
        .iter()
        .zip(&numeric)
        .map(|(w, n)| {
            if *n {
                format!("{}:", "-".repeat(w + 1))
            } else {
                "-".repeat(w + 2)
            }
        })
        .collect();
    writeln!(out, "|{}|", seps.join("|"))?;
    for row in &grid[1..] {
        write_row(out, row)?;
    }
    Ok(())
}

fn write_csv(out: &mut dyn Write, columns: &[String], rows: &[Vec<Value>]) -> io::Result<()> {
    let field = |s: &str| -> String {
        if s.contains([',', '"', '\n', '\r']) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    };
    let header: Vec<String> = columns.iter().map(|c| field(c)).collect();
    writeln!(out, "{}", header.join(","))?;
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .map(|v| match v {
                // NULL is an EMPTY field (the PG `COPY ... CSV` convention; the
                // NULL-vs-empty-text ambiguity is accepted — cli.md §5).
                Value::Null => String::new(),
                other => field(&other.render()),
            })
            .collect();
        writeln!(out, "{}", cells.join(","))?;
    }
    Ok(())
}

fn write_json(out: &mut dyn Write, columns: &[String], rows: &[Vec<Value>]) -> io::Result<()> {
    if rows.is_empty() {
        return writeln!(out, "[]");
    }
    writeln!(out, "[")?;
    for (i, row) in rows.iter().enumerate() {
        let fields: Vec<String> = columns
            .iter()
            .zip(row)
            .map(|(name, v)| format!("{}:{}", json_string(name), json_value(v)))
            .collect();
        let comma = if i + 1 < rows.len() { "," } else { "" };
        writeln!(out, "{{{}}}{comma}", fields.join(","))?;
    }
    writeln!(out, "]")
}

/// The JSON scalar mapping (cli.md §5): int → number (exact — JSON's grammar has
/// arbitrary-precision integers), bool → bool, NULL → null, decimal → STRING (a JSON
/// number would round-trip through f64 in most readers and betray the exact-decimal
/// contract), everything else → its canonical `render()` string.
fn json_value(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Int(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => json_string(&other.render()),
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use jed::decimal::Decimal;

    fn cols(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn render_to_string(format: Format, columns: &[String], rows: &[Vec<Value>]) -> String {
        let mut buf = Vec::new();
        write_query(&mut buf, format, columns, rows, 11).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn sample() -> (Vec<String>, Vec<Vec<Value>>) {
        (
            cols(&["id", "name", "score"]),
            vec![
                vec![
                    Value::Int(1),
                    Value::Text("alice".to_string()),
                    Value::Decimal(Decimal::from_digits_scale(false, "950", 2)),
                ],
                vec![Value::Int(2), Value::Text("bob".to_string()), Value::Null],
            ],
        )
    }

    #[test]
    fn aligned_right_aligns_numeric_columns_and_renders_null() {
        let (columns, rows) = sample();
        assert_eq!(
            render_to_string(Format::Aligned, &columns, &rows),
            " id | name  | score\n\
             ----+-------+-------\n  \
              1 | alice |  9.50\n  \
              2 | bob   |  NULL\n\
             (2 rows, cost 11)\n"
        );
    }

    #[test]
    fn aligned_empty_result_keeps_header_and_footer() {
        let columns = cols(&["v"]);
        assert_eq!(
            render_to_string(Format::Aligned, &columns, &[]),
            " v\n---\n(0 rows, cost 11)\n"
        );
    }

    #[test]
    fn aligned_singular_row_noun() {
        let columns = cols(&["v"]);
        let rows = vec![vec![Value::Int(7)]];
        assert!(render_to_string(Format::Aligned, &columns, &rows).ends_with("(1 row, cost 11)\n"));
    }

    #[test]
    fn box_frames_the_aligned_layout() {
        let (columns, rows) = sample();
        assert_eq!(
            render_to_string(Format::Box, &columns, &rows),
            "┌────┬───────┬───────┐\n\
             │ id │ name  │ score │\n\
             ├────┼───────┼───────┤\n\
             │  1 │ alice │  9.50 │\n\
             │  2 │ bob   │  NULL │\n\
             └────┴───────┴───────┘\n\
             (2 rows, cost 11)\n"
        );
    }

    #[test]
    fn markdown_aligns_escapes_and_skips_the_footer() {
        let columns = cols(&["id", "note"]);
        let rows = vec![
            vec![Value::Int(1), Value::Text("a|b".to_string())],
            vec![Value::Int(2), Value::Text("two\nlines".to_string())],
        ];
        assert_eq!(
            render_to_string(Format::Markdown, &columns, &rows),
            "| id | note         |\n\
             |---:|--------------|\n\
             |  1 | a\\|b         |\n\
             |  2 | two<br>lines |\n"
        );
    }

    #[test]
    fn csv_quotes_and_empties_null() {
        let columns = cols(&["a", "b"]);
        let rows = vec![
            vec![Value::Text("x,y".to_string()), Value::Null],
            vec![Value::Text("say \"hi\"".to_string()), Value::Int(5)],
        ];
        assert_eq!(
            render_to_string(Format::Csv, &columns, &rows),
            "a,b\n\"x,y\",\n\"say \"\"hi\"\"\",5\n"
        );
    }

    #[test]
    fn json_maps_scalars() {
        let columns = cols(&["i", "d", "t", "n", "b"]);
        let rows = vec![vec![
            Value::Int(9007199254740993), // exact past 2^53 — stays a JSON number
            Value::Decimal(Decimal::from_digits_scale(false, "150", 2)),
            Value::Text("a\"b\n".to_string()),
            Value::Null,
            Value::Bool(true),
        ]];
        assert_eq!(
            render_to_string(Format::Json, &columns, &rows),
            "[\n{\"i\":9007199254740993,\"d\":\"1.50\",\"t\":\"a\\\"b\\n\",\"n\":null,\"b\":true}\n]\n"
        );
    }

    #[test]
    fn json_empty_is_bare_array() {
        assert_eq!(render_to_string(Format::Json, &cols(&["v"]), &[]), "[]\n");
    }
}
