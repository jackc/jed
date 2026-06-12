//! Statement splitter (spec/design/cli.md §4).
//!
//! The engine parses exactly one statement per call, with no terminator (grammar.md §1).
//! The CLI owns splitting: `;` outside strings and comments terminates a statement; the
//! semicolon is stripped and everything else — including comments, which the engine
//! accepts (grammar.md §33) — passes through verbatim. The state machine mirrors the
//! engine lexer's rules exactly: `'...'` with `''` escaping is the only quoting; `--`
//! runs to end of line; `/* */` nests. Whitespace-/comment-only statements are skipped.

/// One split statement: its SQL text (terminator stripped, outer whitespace trimmed)
/// and the 1-based input line where its first non-comment content begins.
#[derive(Debug, PartialEq, Eq)]
pub struct Stmt {
    pub sql: String,
    pub line: usize,
}

/// A framing error the splitter itself detects at end of input.
#[derive(Debug, PartialEq, Eq)]
pub struct SplitError {
    pub message: String,
    pub line: usize,
}

enum State {
    Normal,
    InString,
    LineComment,
    BlockComment(u32),
}

/// Split `input` into complete statements. The final statement needs no `;` — a
/// non-empty remainder at end of input is a statement. An unterminated string or
/// block comment is a `SplitError` (the engine would reject it too; reporting it at
/// split time gives an input line number).
///
/// The scan is byte-wise: every state transition happens on an ASCII byte, and the
/// bytes of a multibyte char are all non-ASCII, so copying bytes verbatim never splits
/// a char and the accumulated statement stays valid UTF-8.
pub fn split(input: &str) -> Result<Vec<Stmt>, SplitError> {
    let bytes = input.as_bytes();
    let mut state = State::Normal;
    let mut stmts = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    // The line where the current statement's first CONTENT (non-comment, non-whitespace)
    // byte appeared; None while the buffer holds only whitespace/comments — such a
    // "statement" is skipped at finalize, so `;;` and comment-only input emit nothing.
    let mut content_line: Option<usize> = None;
    // The opener line of an in-flight string/block comment (the better error location).
    let mut opener_line = 1;
    let mut line = 1;
    let mut i = 0;

    let mut finalize = |buf: &mut Vec<u8>, content_line: &mut Option<usize>| {
        if let Some(start) = content_line.take() {
            let text = String::from_utf8(std::mem::take(buf)).expect("copied from valid UTF-8");
            let sql = text.trim();
            if !sql.is_empty() {
                stmts.push(Stmt {
                    sql: sql.to_string(),
                    line: start,
                });
            }
        }
        buf.clear();
    };

    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\n' {
            line += 1;
        }
        match state {
            State::Normal => match c {
                b';' => {
                    finalize(&mut buf, &mut content_line);
                    i += 1;
                }
                b'\'' => {
                    content_line.get_or_insert(line);
                    opener_line = line;
                    state = State::InString;
                    buf.push(c);
                    i += 1;
                }
                b'-' if bytes.get(i + 1) == Some(&b'-') => {
                    state = State::LineComment;
                    buf.extend_from_slice(b"--");
                    i += 2;
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    opener_line = line;
                    state = State::BlockComment(1);
                    buf.extend_from_slice(b"/*");
                    i += 2;
                }
                _ => {
                    if !c.is_ascii_whitespace() {
                        content_line.get_or_insert(line);
                    }
                    buf.push(c);
                    i += 1;
                }
            },
            State::InString => match c {
                b'\'' if bytes.get(i + 1) == Some(&b'\'') => {
                    buf.extend_from_slice(b"''");
                    i += 2;
                }
                b'\'' => {
                    state = State::Normal;
                    buf.push(c);
                    i += 1;
                }
                _ => {
                    buf.push(c);
                    i += 1;
                }
            },
            State::LineComment => {
                if c == b'\n' || c == b'\r' {
                    state = State::Normal;
                }
                buf.push(c);
                i += 1;
            }
            State::BlockComment(depth) => {
                if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    state = State::BlockComment(depth + 1);
                    buf.extend_from_slice(b"/*");
                    i += 2;
                } else if c == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    state = if depth == 1 {
                        State::Normal
                    } else {
                        State::BlockComment(depth - 1)
                    };
                    buf.extend_from_slice(b"*/");
                    i += 2;
                } else {
                    buf.push(c);
                    i += 1;
                }
            }
        }
    }

    match state {
        State::InString => {
            return Err(SplitError {
                message: "unterminated string literal".to_string(),
                line: opener_line,
            });
        }
        State::BlockComment(_) => {
            return Err(SplitError {
                message: "unterminated /* comment".to_string(),
                line: opener_line,
            });
        }
        _ => {}
    }
    finalize(&mut buf, &mut content_line);
    Ok(stmts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(input: &str) -> Vec<(String, usize)> {
        split(input)
            .unwrap()
            .into_iter()
            .map(|s| (s.sql, s.line))
            .collect()
    }

    #[test]
    fn splits_statements() {
        let cases: &[(&str, &[(&str, usize)])] = &[
            // A final statement needs no semicolon.
            ("SELECT 1 FROM t", &[("SELECT 1 FROM t", 1)]),
            ("SELECT 1 FROM t;", &[("SELECT 1 FROM t", 1)]),
            // Two on one line.
            (
                "SELECT 1 FROM t; SELECT 2 FROM t;",
                &[("SELECT 1 FROM t", 1), ("SELECT 2 FROM t", 1)],
            ),
            // A statement spanning lines keeps its text verbatim; line = first content.
            (
                "SELECT v\nFROM t\nWHERE id = 1;\nSELECT 2 FROM t;",
                &[
                    ("SELECT v\nFROM t\nWHERE id = 1", 1),
                    ("SELECT 2 FROM t", 4),
                ],
            ),
            // Blank lines before a statement move its line number, not its text.
            ("\n\n  SELECT 1 FROM t;", &[("SELECT 1 FROM t", 3)]),
            // `;` inside a string is text; `''` doubling stays inside the string.
            (
                "INSERT INTO t VALUES ('a;b');",
                &[("INSERT INTO t VALUES ('a;b')", 1)],
            ),
            (
                "INSERT INTO t VALUES ('it''s; fine'); SELECT 1 FROM t",
                &[
                    ("INSERT INTO t VALUES ('it''s; fine')", 1),
                    ("SELECT 1 FROM t", 1),
                ],
            ),
            // `--` inside a string is text, not a comment.
            (
                "INSERT INTO t VALUES ('--not a comment;')",
                &[("INSERT INTO t VALUES ('--not a comment;')", 1)],
            ),
            // A `;` inside a line comment is not a terminator; the comment passes through.
            (
                "SELECT 1 -- c; not a terminator\nFROM t;",
                &[("SELECT 1 -- c; not a terminator\nFROM t", 1)],
            ),
            // A `;` inside a (nested) block comment is not a terminator.
            (
                "SELECT /* a; /* b; */ c; */ 1 FROM t;",
                &[("SELECT /* a; /* b; */ c; */ 1 FROM t", 1)],
            ),
            // Leading comment lines stay attached; line = the first CONTENT line.
            (
                "-- header\nSELECT 1 FROM t;",
                &[("-- header\nSELECT 1 FROM t", 2)],
            ),
            // Empty / whitespace-only / comment-only statements are skipped.
            ("", &[]),
            ("  \n\t ", &[]),
            (";;;", &[]),
            ("SELECT 1 FROM t;;", &[("SELECT 1 FROM t", 1)]),
            ("-- only a comment", &[]),
            ("/* only a comment */", &[]),
            ("/* a */ -- b", &[]),
            ("SELECT 1 FROM t; -- trailing", &[("SELECT 1 FROM t", 1)]),
        ];
        for (input, want) in cases {
            let want: Vec<(String, usize)> =
                want.iter().map(|(s, l)| (s.to_string(), *l)).collect();
            assert_eq!(ok(input), want, "input: {input:?}");
        }
    }

    #[test]
    fn framing_errors_carry_the_opener_line() {
        let e = split("SELECT 1;\n'oops").unwrap_err();
        assert_eq!(
            (e.message.as_str(), e.line),
            ("unterminated string literal", 2)
        );
        let e = split("SELECT /* outer /* inner */ still open").unwrap_err();
        assert_eq!((e.message.as_str(), e.line), ("unterminated /* comment", 1));
        // Statements before the broken one are NOT returned — the input is rejected whole
        // (script mode must not half-run a malformed file).
        assert!(split("SELECT 1; SELECT '").is_err());
    }

    #[test]
    fn multibyte_text_passes_through() {
        assert_eq!(
            ok("INSERT INTO t VALUES ('héllo; wörld');"),
            vec![("INSERT INTO t VALUES ('héllo; wörld')".to_string(), 1)]
        );
    }
}
