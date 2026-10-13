//! Engine auto-names (spec/design/grammar.md §3.1): the names jed derives for an unnamed
//! constraint, index, or owned sequence — `<table>_pkey`, `<table>_<cols>_key`, `…_fkey`,
//! `…_check`, `…_excl`, `…_idx`, `<table>_<col>_seq`. A port of PostgreSQL's `makeObjectName` /
//! `ChooseRelationName` / `ChooseConstraintName` / `ChooseIndexNameAddition` /
//! `ChooseIndexColumnNames` (indexcmds.c, pg_constraint.c), so a derived name always fits the
//! 63-byte identifier limit (`MAX_IDENTIFIER_LENGTH`) and stays referenceable from SQL, byte for
//! byte as PostgreSQL chooses it. Lengths are UTF-8 bytes; a cut never splits a character.

use crate::parser::MAX_IDENTIFIER_LENGTH;

/// The longest prefix of `s` that is at most `limit` bytes and ends on a character boundary
/// (PostgreSQL's `pg_mbcliplen` over UTF-8).
fn clip(s: &str, limit: usize) -> &str {
    if s.len() <= limit {
        return s;
    }
    let mut end = limit;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// PostgreSQL's `makeObjectName`: `name1[_name2][_label]`, shortening the longer of `name1` /
/// `name2` one byte at a time until the whole fits in `MAX_IDENTIFIER_LENGTH` bytes (each part
/// then clipped back to a character boundary). The label is never shortened.
pub(crate) fn make_object_name(name1: &str, name2: Option<&str>, label: &str) -> String {
    let mut overhead = label.len() + 1;
    let mut name1_len = name1.len();
    let mut name2_len = 0;
    if let Some(n2) = name2 {
        name2_len = n2.len();
        overhead += 1;
    }
    let avail = MAX_IDENTIFIER_LENGTH - overhead;
    while name1_len + name2_len > avail {
        if name1_len > name2_len {
            name1_len -= 1;
        } else {
            name2_len -= 1;
        }
    }
    let mut out = String::from(clip(name1, name1_len));
    if let Some(n2) = name2 {
        out.push('_');
        out.push_str(clip(n2, name2_len));
    }
    out.push('_');
    out.push_str(label);
    out
}

/// PostgreSQL's `ChooseRelationName` / `ChooseConstraintName` loop: the first of
/// `make_object_name(name1, name2, label)`, then with `label1`, `label2`, … — each re-truncated
/// to fit — that `taken` reports free.
pub(crate) fn choose_object_name(
    name1: &str,
    name2: Option<&str>,
    label: &str,
    mut taken: impl FnMut(&str) -> bool,
) -> String {
    let mut name = make_object_name(name1, name2, label);
    let mut pass = 0u32;
    while taken(&name) {
        pass += 1;
        name = make_object_name(name1, name2, &format!("{label}{pass}"));
    }
    name
}

/// PostgreSQL's `ChooseIndexNameAddition` (and the identical `ChooseForeignKeyConstraintNameAddition`):
/// the column-name parts joined by `_`, stopping once the result reaches 64 bytes (the
/// `make_object_name` cut does the rest).
pub(crate) fn name_addition<S: AsRef<str>>(names: &[S]) -> String {
    let mut out = String::new();
    for name in names {
        if !out.is_empty() {
            out.push('_');
        }
        out.push_str(clip(name.as_ref(), MAX_IDENTIFIER_LENGTH));
        if out.len() > MAX_IDENTIFIER_LENGTH {
            break;
        }
    }
    out
}

/// PostgreSQL's `ChooseIndexColumnNames` de-duplication: a part repeating an earlier one becomes
/// `part1`, `part2`, … (the part clipped so the suffixed form fits `MAX_IDENTIFIER_LENGTH`) —
/// `(a, a)` names `a`, `a1`.
pub(crate) fn index_column_names(parts: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(parts.len());
    for orig in parts {
        let mut cur = orig.clone();
        let mut i = 1u32;
        while out.contains(&cur) {
            let n = i.to_string();
            cur = format!("{}{n}", clip(&orig, MAX_IDENTIFIER_LENGTH - n.len()));
            i += 1;
        }
        out.push(cur);
    }
    out
}

/// The derived primary-key constraint name: `make_object_name(table, None, "pkey")`. jed persists
/// no PK object (constraints.md §5.4), so this is computed wherever the name surfaces and takes no
/// collision suffix.
pub(crate) fn pkey_name(table: &str) -> String {
    make_object_name(table, None, "pkey")
}
