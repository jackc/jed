//! SQL comments are lexer whitespace (spec/design/grammar.md §33): `--` line comments
//! run to end of line (and ALWAYS start outside a string, even abutting a token —
//! `1--2` is `1`); `/* */` block comments NEST per PG / the SQL standard; an
//! unterminated block is 42601; comment openers inside a string literal are text.

use jed::{Database, Outcome, execute};

fn setup() -> Database {
    let mut db = Database::new();
    execute(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32, s text)",
    )
    .unwrap();
    execute(&mut db, "INSERT INTO t VALUES (1, 10, '--x /*y*/')").unwrap();
    db
}

/// Run a query expected to produce exactly one value; return it rendered.
fn one(db: &mut Database, sql: &str) -> String {
    match execute(db, sql).unwrap() {
        Outcome::Query { rows, .. } => {
            assert_eq!(rows.len(), 1, "{sql}");
            assert_eq!(rows[0].len(), 1, "{sql}");
            rows[0][0].render()
        }
        other => panic!("expected a query result for {sql}, got {other:?}"),
    }
}

#[test]
fn line_comments_run_to_end_of_line() {
    let mut db = setup();
    // Trailing comment; the statement continues on the next line.
    assert_eq!(
        one(&mut db, "SELECT v -- trailing\nFROM t WHERE id = 1"),
        "10"
    );
    // Leading comment line.
    assert_eq!(
        one(&mut db, "-- leading\nSELECT v FROM t WHERE id = 1"),
        "10"
    );
    // A comment at the very end of input (no newline) is fine.
    assert_eq!(one(&mut db, "SELECT v FROM t WHERE id = 1 -- done"), "10");
}

#[test]
fn two_hyphens_start_a_comment_even_abutting_a_token() {
    let mut db = setup();
    // `v--1` is `v` then a comment (PG) — NOT `v - (-1)`.
    assert_eq!(one(&mut db, "SELECT v--1\nFROM t WHERE id = 1"), "10");
    // Separated operators still mean double negation.
    assert_eq!(one(&mut db, "SELECT v - -1 FROM t WHERE id = 1"), "11");
}

#[test]
fn block_comments_separate_tokens_and_nest() {
    let mut db = setup();
    // A block comment is a token separator.
    assert_eq!(one(&mut db, "SELECT/*c*/v/*c*/FROM t WHERE id = 1"), "10");
    // Blocks nest: the comment ends only when the depth returns to zero.
    assert_eq!(
        one(
            &mut db,
            "SELECT /* a /* b */ still comment */ v FROM t WHERE id = 1"
        ),
        "10"
    );
    // A quote inside a block comment is ordinary comment text.
    assert_eq!(
        one(&mut db, "SELECT /* it's fine */ v FROM t WHERE id = 1"),
        "10"
    );
}

#[test]
fn comment_openers_inside_a_string_are_text() {
    let mut db = setup();
    assert_eq!(one(&mut db, "SELECT s FROM t WHERE id = 1"), "--x /*y*/");
}

#[test]
fn unterminated_block_comment_is_42601() {
    let mut db = setup();
    for sql in [
        "SELECT v FROM t /* unterminated",
        "SELECT v FROM t /* outer /* inner */ still open",
        "SELECT v FROM t /*/", // the close cannot overlap the open
    ] {
        assert_eq!(execute(&mut db, sql).unwrap_err().code(), "42601", "{sql}");
    }
}

#[test]
fn stray_close_is_not_comment_syntax() {
    let mut db = setup();
    // `*/` with no opener lexes as `*` `/` and fails at parse.
    assert_eq!(
        execute(&mut db, "SELECT v */ 1 FROM t").unwrap_err().code(),
        "42601"
    );
}

#[test]
fn comment_only_input_is_no_statement() {
    let mut db = setup();
    for sql in ["-- nothing here", "/* nothing here */", "  /* a */ -- b"] {
        assert_eq!(execute(&mut db, sql).unwrap_err().code(), "42601", "{sql}");
    }
}
