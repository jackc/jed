//! Library-level multi-statement splitter (spec/design/session.md §4.1). A pure, streaming
//! statement scanner that depends on **neither `Session` nor `Engine`** — a top-level core export,
//! conceptually part of the lexer surface (CLAUDE.md §5: parsers are per-language, not codegen'd),
//! callable before any database is opened. It yields one statement's source text at a time, lazily,
//! buffering nothing across statements (an O(n) scan, no parse tree).
//!
//! It scans at the lexer level so a `;` inside a **string literal**, a **dollar-quoted string**, or
//! a **line/block comment** is never a statement boundary. It does *not* validate tokens (a lone `!`
//! is just a byte to the splitter — the error surfaces when the host feeds the span to `parse`), so
//! it never fails: the boundary scan is total. Empty spans — a leading/standalone `;`, or
//! whitespace/comment-only text between separators — are skipped, so every yielded span has
//! significant content. Each span carries its source text (leading/trailing whitespace and comments
//! trimmed; interior comments kept) and the byte offset of its first significant byte (for the host's
//! error reporting).

/// One statement carved out of a multi-statement string (spec/design/session.md §4.1). `text` is the
/// statement's source — feed it to `session.execute`/`session.query`/`prepare` — and `offset` is the
/// byte offset of its first significant byte in the original input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatementSpan {
    text: String,
    offset: usize,
}

impl StatementSpan {
    /// The statement's source text (never empty; leading/trailing whitespace and comments trimmed).
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The byte offset of the statement's first significant byte in the original input.
    pub fn offset(&self) -> usize {
        self.offset
    }
}

/// A lazy iterator over the statements in `sql` (spec/design/session.md §4.1). Holds only a cursor
/// into the borrowed input — no statement is materialized until `next` yields it, and nothing is
/// buffered across statements.
pub struct SplitStatements<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

/// Split `sql` into its top-level statements, lazily (spec/design/session.md §4.1). The returned
/// iterator yields one [`StatementSpan`] per non-empty statement, splitting on top-level `;` while
/// respecting string literals, dollar-quoted strings, and line/block comments. Pure and total — no
/// `Session`/`Engine`, never an error.
pub fn split_statements(sql: &str) -> SplitStatements<'_> {
    SplitStatements {
        src: sql,
        bytes: sql.as_bytes(),
        pos: 0,
    }
}

impl Iterator for SplitStatements<'_> {
    type Item = StatementSpan;

    fn next(&mut self) -> Option<StatementSpan> {
        let bytes = self.bytes;
        let n = bytes.len();
        let mut i = self.pos;
        // `start` = the first significant byte of the current statement (None until one is seen);
        // `last_end` = one past the last significant byte (so trailing whitespace/comments trim off).
        let mut start: Option<usize> = None;
        let mut last_end = i;

        while i < n {
            let c = bytes[i];
            match c {
                b' ' | b'\t' | b'\r' | b'\n' => i += 1,
                b';' => {
                    i += 1;
                    if let Some(s) = start {
                        self.pos = i;
                        return Some(StatementSpan {
                            text: self.src[s..last_end].to_string(),
                            offset: s,
                        });
                    }
                    // An empty span (leading or standalone `;`) — keep scanning for the next one.
                }
                b'-' if bytes.get(i + 1) == Some(&b'-') => {
                    // `--` line comment to end of line (non-significant — never sets `start`).
                    i += 2;
                    while i < n && bytes[i] != b'\n' && bytes[i] != b'\r' {
                        i += 1;
                    }
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    // `/* … */` block comment; blocks NEST (PG / the lexer). An unterminated comment
                    // runs to EOF and stays non-significant (a comment-only tail is an empty span).
                    i += 2;
                    let mut depth = 1usize;
                    while depth > 0 && i < n {
                        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                            depth += 1;
                            i += 2;
                        } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                            depth -= 1;
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                }
                b'\'' => {
                    // Single-quoted string literal; `''` is an embedded quote. A `;` inside is not a
                    // boundary. An unterminated literal runs to EOF (the parse error is the host's).
                    if start.is_none() {
                        start = Some(i);
                    }
                    i += 1;
                    while i < n {
                        if bytes[i] == b'\'' {
                            if bytes.get(i + 1) == Some(&b'\'') {
                                i += 2;
                            } else {
                                i += 1;
                                break;
                            }
                        } else {
                            i += 1;
                        }
                    }
                    last_end = i;
                }
                b'$' => {
                    // A `$tag$ … $tag$` dollar-quoted string (PG): `$` + an optional identifier tag
                    // + `$`, closed by the same delimiter. `$1` (a digit follows) is a bind parameter,
                    // not a dollar-quote — treated as one ordinary significant byte. A `;` inside a
                    // dollar-quote is never a boundary.
                    if start.is_none() {
                        start = Some(i);
                    }
                    if let Some(tag_len) = dollar_tag_len(bytes, i) {
                        let open = &bytes[i..i + tag_len];
                        let mut j = i + tag_len;
                        loop {
                            if j >= n {
                                j = n; // unterminated — consume to EOF
                                break;
                            }
                            if bytes[j] == b'$'
                                && j + tag_len <= n
                                && &bytes[j..j + tag_len] == open
                            {
                                j += tag_len; // matched closing delimiter
                                break;
                            }
                            j += 1;
                        }
                        i = j;
                    } else {
                        i += 1;
                    }
                    last_end = i;
                }
                _ => {
                    if start.is_none() {
                        start = Some(i);
                    }
                    i += 1;
                    last_end = i;
                }
            }
        }

        // End of input: emit the trailing statement if it had significant content.
        self.pos = n;
        start.map(|s| StatementSpan {
            text: self.src[s..last_end].to_string(),
            offset: s,
        })
    }
}

/// If `bytes[i..]` (where `bytes[i] == b'$'`) opens a dollar-quote delimiter `$tag$`, return its
/// total length (including both `$`); otherwise `None`. A tag is empty (`$$`) or
/// `[A-Za-z_][A-Za-z0-9_]*` (PG); a `$` followed by a digit (`$1`) or with no terminating `$` is not
/// a dollar-quote opener.
fn dollar_tag_len(bytes: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if bytes.get(j) == Some(&b'$') {
        return Some(2); // empty tag: `$$`
    }
    // First tag character must be a letter or underscore (not a digit — that is a bind parameter).
    match bytes.get(j) {
        Some(&b) if b.is_ascii_alphabetic() || b == b'_' => j += 1,
        _ => return None,
    }
    while let Some(&b) = bytes.get(j) {
        if b.is_ascii_alphanumeric() || b == b'_' {
            j += 1;
        } else {
            break;
        }
    }
    if bytes.get(j) == Some(&b'$') {
        Some(j + 1 - i)
    } else {
        None // no terminating `$` for the tag
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect the yielded statements as `(text, offset)` pairs — the splitter buffers nothing, so
    /// the test does the collecting.
    fn split(sql: &str) -> Vec<(String, usize)> {
        split_statements(sql)
            .map(|s| (s.text().to_string(), s.offset()))
            .collect()
    }

    fn texts(sql: &str) -> Vec<String> {
        split_statements(sql)
            .map(|s| s.text().to_string())
            .collect()
    }

    #[test]
    fn basic_split_and_offsets() {
        assert_eq!(
            split("SELECT 1; SELECT 2"),
            vec![("SELECT 1".to_string(), 0), ("SELECT 2".to_string(), 10)],
        );
    }

    #[test]
    fn empty_spans_are_skipped() {
        // A trailing `;`, leading `;`, and whitespace/comment-only fragments yield nothing.
        assert_eq!(texts("SELECT 1;"), vec!["SELECT 1"]);
        assert_eq!(texts(";;; SELECT 1 ;;;"), vec!["SELECT 1"]);
        assert_eq!(texts(""), Vec::<String>::new());
        assert_eq!(texts("   \n\t  "), Vec::<String>::new());
        assert_eq!(texts(";"), Vec::<String>::new());
        assert_eq!(texts("-- just a comment\n"), Vec::<String>::new());
        assert_eq!(texts("/* block only */"), Vec::<String>::new());
    }

    #[test]
    fn semicolon_in_string_is_not_a_boundary() {
        assert_eq!(
            texts("INSERT INTO t VALUES ('a;b'); SELECT 1"),
            vec!["INSERT INTO t VALUES ('a;b')", "SELECT 1"],
        );
        // Embedded `''` quote, with a `;` between the two quotes.
        assert_eq!(texts("SELECT 'it''s; ok'"), vec!["SELECT 'it''s; ok'"]);
    }

    #[test]
    fn semicolon_in_comment_is_not_a_boundary() {
        assert_eq!(
            texts("SELECT 1 -- a; b\n; SELECT 2"),
            vec!["SELECT 1", "SELECT 2"],
        );
        assert_eq!(
            texts("SELECT /* a; b */ 1; SELECT 2"),
            vec!["SELECT /* a; b */ 1", "SELECT 2"],
        );
        // Nested block comment, with a `;` inside.
        assert_eq!(
            texts("SELECT /* /* ; */ */ 1"),
            vec!["SELECT /* /* ; */ */ 1"]
        );
    }

    #[test]
    fn dollar_quote_semicolon_is_not_a_boundary() {
        assert_eq!(
            texts("SELECT $$a;b$$; SELECT 2"),
            vec!["SELECT $$a;b$$", "SELECT 2"],
        );
        // Tagged dollar-quote; an inner `$$` does not close a `$tag$`.
        assert_eq!(
            texts("SELECT $tag$a;$$;b$tag$; SELECT 2"),
            vec!["SELECT $tag$a;$$;b$tag$", "SELECT 2"],
        );
        // `$1` is a bind parameter, not a dollar-quote — the `;` after it splits.
        assert_eq!(texts("SELECT $1; SELECT 2"), vec!["SELECT $1", "SELECT 2"]);
    }

    #[test]
    fn trailing_whitespace_and_interior_comment_handling() {
        // Leading whitespace excluded from the offset, trailing whitespace trimmed; an interior
        // comment is preserved inside the span.
        let parts = split("  SELECT 1  ;  SELECT /* x */ 2  ");
        assert_eq!(parts[0], ("SELECT 1".to_string(), 2));
        assert_eq!(parts[1].0, "SELECT /* x */ 2");
        assert_eq!(parts[1].1, 15);
    }

    #[test]
    fn no_trailing_semicolon_still_yields_last() {
        assert_eq!(
            texts("SELECT 1; SELECT 2; SELECT 3"),
            vec!["SELECT 1", "SELECT 2", "SELECT 3"]
        );
    }
}
