//! Cross-core regex compile-determinism check (spec/design/regex.md §9). Reads the authored
//! `spec/regex/program_vectors.toml` and asserts this core compiles each pattern to the exact
//! instruction listing + class table + count (= `regex_compile` cost). The Go and TS cores run the
//! equivalent check against the SAME file, so the three compilations are pinned identical (CLAUDE.md
//! §2/§8 — this is the byte-level contract the SQL conformance corpus cannot express, §10).

use jed::regex::compile;

struct Case {
    pattern: String,
    flags: String,
    prog: Vec<String>,
    count: usize,
    classes: Vec<String>,
}

/// Minimal reader for the regular structure of program_vectors.toml (the cores carry no TOML
/// dependency — §14). Handles `[[case]]` blocks and `key = "str" | N | ["a", "b"]` lines, with TOML
/// basic-string `\\` / `\"` unescaping. The fixture writes every array on one line.
fn parse_cases(text: &str) -> Vec<Case> {
    let mut cases: Vec<Case> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line == "[[case]]" {
            cases.push(Case {
                pattern: String::new(),
                flags: String::new(),
                prog: Vec::new(),
                count: 0,
                classes: Vec::new(),
            });
            continue;
        }
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let Some(c) = cases.last_mut() else { continue };
        let (key, val) = line.split_once('=').unwrap();
        let (key, val) = (key.trim(), val.trim());
        match key {
            "pattern" => c.pattern = unquote(val),
            "flags" => c.flags = unquote(val),
            "count" => c.count = val.parse().unwrap(),
            "prog" => c.prog = parse_str_array(val),
            "classes" => c.classes = parse_str_array(val),
            _ => {}
        }
    }
    cases
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let inner = &s[1..s.len() - 1]; // strip the surrounding quotes
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_str_array(val: &str) -> Vec<String> {
    let val = val.trim();
    let inner = &val[1..val.len() - 1]; // strip [ ]
    if inner.trim().is_empty() {
        return Vec::new();
    }
    inner.split(',').map(|p| unquote(p.trim())).collect()
}

/// Compile a vector's pattern; `flags = "m"` selects the line-anchor mode (regex.md §8d).
fn compile_case(pat: &str, flags: &str) -> jed::error::Result<jed::regex::Program> {
    if flags.contains('m') {
        jed::regex::compile_mode(
            pat,
            jed::regex::Mode {
                nl_anchor: true,
                ..Default::default()
            },
        )
    } else {
        compile(pat)
    }
}

#[test]
fn program_vectors_match() {
    let text = std::fs::read_to_string("../../spec/regex/program_vectors.toml")
        .expect("read program_vectors.toml");
    let cases = parse_cases(&text);
    assert!(
        cases.len() >= 25,
        "expected the full vector set, got {}",
        cases.len()
    );
    for c in &cases {
        // `flags = "i"` folds the pattern's literals with simple lowercasing before compiling (the ~*
        // path, regex.md §1).
        let pat = if c.flags.contains('i') {
            jed::regex::fold_pattern(&c.pattern, None)
        } else {
            c.pattern.clone()
        };
        let prog = compile_case(&pat, &c.flags)
            .unwrap_or_else(|e| panic!("compile {:?}: {}", c.pattern, e.message));
        assert_eq!(
            prog.listing(),
            c.prog,
            "program for pattern {:?}",
            c.pattern
        );
        assert_eq!(
            prog.class_listing(),
            c.classes,
            "classes for pattern {:?}",
            c.pattern
        );
        assert_eq!(prog.ninst(), c.count, "count for pattern {:?}", c.pattern);
    }
}

struct MatchCase {
    pattern: String,
    flags: String,
    input: String,
    matched: bool,
    caps: Vec<(i64, i64)>,
    steps: i64,
}

fn parse_match_cases(text: &str) -> Vec<MatchCase> {
    let mut cases: Vec<MatchCase> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line == "[[case]]" {
            cases.push(MatchCase {
                pattern: String::new(),
                flags: String::new(),
                input: String::new(),
                matched: false,
                caps: Vec::new(),
                steps: 0,
            });
            continue;
        }
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let Some(c) = cases.last_mut() else { continue };
        let (key, val) = line.split_once('=').unwrap();
        let (key, val) = (key.trim(), val.trim());
        match key {
            "pattern" => c.pattern = unquote(val),
            "flags" => c.flags = unquote(val),
            "input" => c.input = unquote(val),
            "matched" => c.matched = val == "true",
            "steps" => c.steps = val.parse().unwrap(),
            "caps" => c.caps = parse_pairs(val),
            _ => {}
        }
    }
    cases
}

/// Parse `[[0, 1], [2, 5]]` (or `[]`) into `(start, end)` pairs.
fn parse_pairs(val: &str) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut cur: Vec<i64> = Vec::new();
    let mut num = String::new();
    for ch in val.chars() {
        match ch {
            '[' => depth += 1,
            ']' => {
                if !num.is_empty() {
                    cur.push(num.trim().parse().unwrap());
                    num.clear();
                }
                if depth == 2 && cur.len() == 2 {
                    out.push((cur[0], cur[1]));
                    cur.clear();
                }
                depth -= 1;
            }
            ',' => {
                if !num.is_empty() {
                    cur.push(num.trim().parse().unwrap());
                    num.clear();
                }
            }
            c if c.is_ascii_digit() || c == '-' => num.push(c),
            _ => {}
        }
    }
    out
}

#[test]
fn match_vectors_match() {
    use jed::cost::Meter;
    let text = std::fs::read_to_string("../../spec/regex/match_vectors.toml")
        .expect("read match_vectors.toml");
    let cases = parse_match_cases(&text);
    assert!(
        cases.len() >= 25,
        "expected the full vector set, got {}",
        cases.len()
    );
    for c in &cases {
        let (pat, subj) = if c.flags.contains('i') {
            (
                jed::regex::fold_pattern(&c.pattern, None),
                jed::collation::fold_lower_simple(&c.input, None),
            )
        } else {
            (c.pattern.clone(), c.input.clone())
        };
        let prog = compile_case(&pat, &c.flags)
            .unwrap_or_else(|e| panic!("compile {:?}: {}", c.pattern, e.message));
        let chars: Vec<char> = subj.chars().collect();
        let mut m = Meter::new();
        let caps = prog.run(&chars, &mut m).unwrap();
        assert_eq!(
            caps.is_some(),
            c.matched,
            "matched for {:?}/{:?}",
            c.pattern,
            c.input
        );
        let got: Vec<(i64, i64)> = caps
            .map(|c| prog.slots(&c).chunks(2).map(|p| (p[0], p[1])).collect())
            .unwrap_or_default();
        assert_eq!(got, c.caps, "caps for {:?}/{:?}", c.pattern, c.input);
        assert_eq!(
            m.accrued, c.steps,
            "steps for {:?}/{:?}",
            c.pattern, c.input
        );
    }
}

#[derive(Default)]
struct SimilarCase {
    pattern: String,
    escape: Option<String>,
    input: String,
    error: Option<String>,
    translation: String,
    similar: bool,
    similar_compile: usize,
    similar_steps: i64,
    substring: Option<String>,
    substring_error: Option<String>,
    substring_compile: usize,
    substring_steps: i64,
}

fn parse_similar_cases(text: &str) -> Vec<SimilarCase> {
    let mut cases: Vec<SimilarCase> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line == "[[case]]" {
            cases.push(SimilarCase::default());
            continue;
        }
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let Some(c) = cases.last_mut() else { continue };
        let (key, val) = line.split_once('=').unwrap();
        let (key, val) = (key.trim(), val.trim());
        match key {
            "pattern" => c.pattern = unquote(val),
            "escape" => c.escape = Some(unquote(val)),
            "input" => c.input = unquote(val),
            "error" => c.error = Some(unquote(val)),
            "translation" => c.translation = unquote(val),
            "similar" => c.similar = val == "true",
            "similar_compile" => c.similar_compile = val.parse().unwrap(),
            "similar_steps" => c.similar_steps = val.parse().unwrap(),
            "substring" => c.substring = Some(unquote(val)),
            "substring_error" => c.substring_error = Some(unquote(val)),
            "substring_compile" => c.substring_compile = val.parse().unwrap(),
            "substring_steps" => c.substring_steps = val.parse().unwrap(),
            _ => {}
        }
    }
    cases
}

/// The SIMILAR TO contract (regex.md §8f): the translation, the dot-all match, and the four-search
/// substring, with their compile and step costs.
#[test]
fn similar_vectors_match() {
    use jed::cost::Meter;
    use jed::regex::{SimilarMatcher, compile_similar, similar_to_escape};
    let text = std::fs::read_to_string("../../spec/regex/similar_vectors.toml")
        .expect("read similar_vectors.toml");
    let cases = parse_similar_cases(&text);
    assert!(
        cases.len() >= 15,
        "expected the full vector set, got {}",
        cases.len()
    );
    for c in &cases {
        let what = format!("{:?} escape {:?} on {:?}", c.pattern, c.escape, c.input);
        let escape = c.escape.as_deref();
        let translated = similar_to_escape(&c.pattern, escape);
        if let Some(code) = &c.error {
            assert_eq!(
                translated.unwrap_err().state.code(),
                code,
                "error for {what}"
            );
            continue;
        }
        let translated = translated.unwrap_or_else(|e| panic!("translate {what}: {}", e.message));
        assert_eq!(translated, c.translation, "translation for {what}");
        let chars: Vec<char> = c.input.chars().collect();
        let prog = compile_similar(&translated).unwrap();
        let mut m = Meter::new();
        assert_eq!(
            prog.is_match(&chars, &mut m).unwrap(),
            c.similar,
            "similar for {what}"
        );
        assert_eq!(
            prog.ninst(),
            c.similar_compile,
            "similar_compile for {what}"
        );
        assert_eq!(m.accrued, c.similar_steps, "similar_steps for {what}");
        let matcher = SimilarMatcher::compile(&c.pattern, escape.unwrap_or("\\"));
        if let Some(code) = &c.substring_error {
            assert_eq!(
                matcher.unwrap_err().state.code(),
                code,
                "substring_error for {what}"
            );
            continue;
        }
        let matcher = matcher.unwrap();
        let mut m = Meter::new();
        let got = matcher
            .substring(&chars, &mut m)
            .unwrap()
            .map(|(i, j)| chars[i..j].iter().collect::<String>());
        assert_eq!(got, c.substring, "substring for {what}");
        assert_eq!(
            matcher.ninst(),
            c.substring_compile,
            "substring_compile for {what}"
        );
        assert_eq!(m.accrued, c.substring_steps, "substring_steps for {what}");
    }
}
