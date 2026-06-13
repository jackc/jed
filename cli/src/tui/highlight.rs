//! SQL syntax highlighting for the editor (spec/design/cli.md §6). A small line-spanning
//! tokenizer mirroring the engine lexer's lexical rules (grammar.md §33): `'...'` strings
//! with `''` escaping, `--` line comments, nested `/* */` block comments — block comments
//! and strings carry across lines. Pure token classification — no ratatui types here; the
//! class → style mapping lives in draw.rs, which renders the editor content from these
//! spans (tui-textarea offers no per-token styling).

use super::complete::WORDS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Keyword,
    Number,
    Str,
    Comment,
    Plain,
}

/// One run of same-class characters within a line.
#[derive(Debug, PartialEq, Eq)]
pub struct Span {
    pub class: Class,
    pub text: String,
}

/// The cross-line lexer state at a line boundary.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Normal,
    /// Inside a `'...'` string (the engine's strings may span lines).
    InString,
    /// Inside `/* */` block comments, `depth` levels deep (they nest — grammar.md §33).
    InBlockComment(usize),
}

/// Highlight the editor buffer: one span list per line, with string/block-comment state
/// carried across line boundaries.
pub fn highlight(lines: &[String]) -> Vec<Vec<Span>> {
    let mut out = Vec::with_capacity(lines.len());
    let mut state = State::Normal;
    for line in lines {
        let (spans, next) = highlight_line(line, state);
        out.push(spans);
        state = next;
    }
    out
}

fn highlight_line(line: &str, mut state: State) -> (Vec<Span>, State) {
    let chars: Vec<char> = line.chars().collect();
    let mut spans: Vec<Span> = Vec::new();
    let push = |class: Class, text: String, spans: &mut Vec<Span>| {
        if text.is_empty() {
            return;
        }
        if let Some(last) = spans.last_mut()
            && last.class == class
        {
            last.text.push_str(&text);
            return;
        }
        spans.push(Span { class, text });
    };

    let mut i = 0;
    while i < chars.len() {
        match state {
            State::InString => {
                // Consume to the closing quote; '' is an escaped quote, not a close.
                let start = i;
                while i < chars.len() {
                    if chars[i] == '\'' {
                        if chars.get(i + 1) == Some(&'\'') {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        state = State::Normal;
                        break;
                    }
                    i += 1;
                }
                push(Class::Str, chars[start..i].iter().collect(), &mut spans);
            }
            State::InBlockComment(mut depth) => {
                let start = i;
                while i < chars.len() && depth > 0 {
                    if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                        depth += 1;
                        i += 2;
                    } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                state = if depth == 0 {
                    State::Normal
                } else {
                    State::InBlockComment(depth)
                };
                push(Class::Comment, chars[start..i].iter().collect(), &mut spans);
            }
            State::Normal => {
                let c = chars[i];
                if c == '-' && chars.get(i + 1) == Some(&'-') {
                    // A line comment runs to end of line; state resets at the newline.
                    push(Class::Comment, chars[i..].iter().collect(), &mut spans);
                    i = chars.len();
                } else if c == '/' && chars.get(i + 1) == Some(&'*') {
                    state = State::InBlockComment(1);
                    push(Class::Comment, "/*".to_string(), &mut spans);
                    i += 2;
                } else if c == '\'' {
                    state = State::InString;
                    push(Class::Str, "'".to_string(), &mut spans);
                    i += 1;
                } else if c.is_ascii_alphabetic() || c == '_' {
                    let start = i;
                    while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                        i += 1;
                    }
                    let word: String = chars[start..i].iter().collect();
                    let class = if WORDS.iter().any(|w| w.eq_ignore_ascii_case(&word)) {
                        Class::Keyword
                    } else {
                        Class::Plain
                    };
                    push(class, word, &mut spans);
                } else if c.is_ascii_digit() {
                    let start = i;
                    while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                        i += 1;
                    }
                    push(Class::Number, chars[start..i].iter().collect(), &mut spans);
                } else {
                    push(Class::Plain, c.to_string(), &mut spans);
                    i += 1;
                }
            }
        }
    }
    (spans, state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(class: Class, text: &str) -> Span {
        Span {
            class,
            text: text.to_string(),
        }
    }

    fn one(line: &str) -> Vec<Span> {
        highlight(&[line.to_string()]).remove(0)
    }

    #[test]
    fn classifies_keywords_numbers_strings_comments() {
        assert_eq!(
            one("SELECT id, 'a''b' FROM t WHERE v >= 1.5 -- tail"),
            vec![
                s(Class::Keyword, "SELECT"),
                s(Class::Plain, " id, "),
                s(Class::Str, "'a''b'"),
                s(Class::Plain, " "),
                s(Class::Keyword, "FROM"),
                s(Class::Plain, " t "),
                s(Class::Keyword, "WHERE"),
                s(Class::Plain, " v >= "),
                s(Class::Number, "1.5"),
                s(Class::Plain, " "),
                s(Class::Comment, "-- tail"),
            ]
        );
    }

    #[test]
    fn keywords_match_case_insensitively_but_identifiers_stay_plain() {
        assert_eq!(
            one("select Users"),
            vec![s(Class::Keyword, "select"), s(Class::Plain, " Users")]
        );
    }

    #[test]
    fn block_comments_nest_and_span_lines() {
        let lines = vec![
            "a /* one /* two".to_string(),
            "still */ inner */ SELECT".to_string(),
        ];
        let spans = highlight(&lines);
        assert_eq!(
            spans[0],
            vec![s(Class::Plain, "a "), s(Class::Comment, "/* one /* two")]
        );
        assert_eq!(
            spans[1],
            vec![
                s(Class::Comment, "still */ inner */"),
                s(Class::Plain, " "),
                s(Class::Keyword, "SELECT"),
            ]
        );
    }

    #[test]
    fn strings_span_lines_and_escaped_quotes_do_not_close() {
        let lines = vec!["SELECT 'two".to_string(), "lines''x' AS v".to_string()];
        let spans = highlight(&lines);
        assert_eq!(
            spans[0],
            vec![
                s(Class::Keyword, "SELECT"),
                s(Class::Plain, " "),
                s(Class::Str, "'two")
            ]
        );
        assert_eq!(
            spans[1],
            vec![
                s(Class::Str, "lines''x'"),
                s(Class::Plain, " "),
                s(Class::Keyword, "AS"),
                s(Class::Plain, " v"),
            ]
        );
    }
}
