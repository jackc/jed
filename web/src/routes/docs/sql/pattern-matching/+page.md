<script>
	import LiveSql from '$lib/components/LiveSql.svelte';

	const seed = `CREATE TABLE account (
  id    i32 PRIMARY KEY,
  email text NOT NULL
);
INSERT INTO account VALUES
  (1, 'alice@example.com'),
  (2, 'bob@work.example.org'),
  (3, 'carol+news@example.com'),
  (4, 'dave@nope.test');`;

	const like = `SELECT id, email FROM account
WHERE email LIKE '%@example.com'
ORDER BY id;`;

	const likeEscape = `SELECT '50% off' LIKE '%!%%' ESCAPE '!' AS has_percent,
       '50 off'  LIKE '%!%%' ESCAPE '!' AS plain;`;

	const regexOp = `SELECT id, email FROM account
WHERE email ~ '@example\\.(com|org)$'
ORDER BY id;`;

	const insensitive = `SELECT 'Hello, WORLD' ~* 'world' AS shouty;`;

	const negated = `SELECT id, email FROM account
WHERE email !~ '\\+'
ORDER BY id;`;

	const replace = `SELECT regexp_replace('2024-01-31', '-', '/', 'g') AS slashed;`;

	const replaceCapture = `SELECT regexp_replace('Doe, Jane', '(\\w+), (\\w+)', '\\2 \\1') AS name;`;

	const match = `SELECT regexp_match('carol+news@example.com', '^([^@]+)@(.+)$') AS parts;`;

	const substringRegex = `SELECT substring('order-1234' FROM '[0-9]+')       AS digits,
       substring('key=value' FROM '=(.*)')          AS after_eq,
       substring('nothing here' FROM '[0-9]+')      AS no_match;`;

	const matches = `SELECT m[1] AS key, m[2] AS value
FROM regexp_matches('host=db1 port=5432 user=app', '(\\w+)=(\\w+)', 'g') AS m;`;

	const splitTable = `SELECT word
FROM regexp_split_to_table('the quick  brown fox', '\\s+') AS word;`;

	const splitArray = `SELECT id, regexp_split_to_array(email, '[@.]') AS parts
FROM account
ORDER BY id;`;

	const redos = `SELECT 'aaaaaaaaaaaaaaaaaaaa!' ~ '(a+)+\$' AS matched;`;
</script>

# Pattern matching

jed offers three ways to match text against a pattern: SQL `LIKE` / `ILIKE`, the
regular-expression operators `~` `~*` `!~` `!~*`, and the `regexp_*` functions
(`regexp_replace`, `regexp_match`, `regexp_matches`, the split functions, and the regex form of
`substring`).

## `LIKE` / `ILIKE`

`LIKE` is the SQL wildcard match: `%` matches any run of characters, `_` matches exactly one.
`ILIKE` is the case-insensitive form. The match is by Unicode code point.

<LiveSql {seed} query={like} rows={4} />

To match a literal `%` or `_`, escape it. The default escape character is backslash (`\%`,
`\_`, `\\`). `ESCAPE` picks a different one, which is handy when the pattern itself contains
backslashes; `ESCAPE ''` turns escaping off. The escape must be a single character (`22025`
otherwise).

<LiveSql query={likeEscape} rows={1} />

## Regular expressions

The `~` operator is TRUE when the pattern matches **somewhere** in the subject (it is
unanchored — anchor with `^` / `$` for a whole-string match). `~*` is case-insensitive, and
`!~` / `!~*` are their negations.

jed's regex flavor is its **own**, deliberately **not** PostgreSQL-compatible: it is a clean
[RE2](https://github.com/google/re2)-style subset run by a linear-time engine, so it is immune
to catastrophic-backtracking ("ReDoS") attacks — a pattern that would hang a backtracking engine
runs in linear time here. `SIMILAR TO`, backreferences, and lookaround are intentionally absent.

<LiveSql {seed} query={regexOp} rows={3} />

The pattern surface: literals, `.` (any code point except newline), character classes
`[...]` / `[^...]`, the shorthands `\d` `\w` `\s` (and `\D` `\W` `\S`), anchors `^` `$` and
the string-boundary escapes `\A` (start) / `\z` (absolute end), alternation `|`, groups
`(...)` / `(?:...)`, and the quantifiers `* + ? {n} {n,} {n,m}` (each with a lazy `?`-suffixed
form). Matching is greedy and leftmost-first.

Case-insensitive matching:

<LiveSql query={insensitive} rows={1} />

Negation — accounts whose address has no `+` tag:

<LiveSql {seed} query={negated} rows={3} />

### Linear-time by construction

The classic catastrophic-backtracking pattern `(a+)+$` is harmless here — it matches (or fails)
in time proportional to the input, never exponentially:

<LiveSql query={redos} rows={1} />

## `regexp_replace`

`regexp_replace(source, pattern, replacement [, flags])` replaces the first match (or all matches
with the `g` flag). The replacement is a template: `\1`…`\9` splice in capture groups, `\&` the
whole match, and `\\` a literal backslash.

<LiveSql query={replace} rows={1} />

Reordering with capture groups:

<LiveSql query={replaceCapture} rows={1} />

## `regexp_match`

`regexp_match(source, pattern [, flags])` returns a `text[]` of the first match's capture groups
(or the whole match when the pattern has no group), or `NULL` when there is no match.

<LiveSql query={match} rows={1} />

## `substring(… FROM pattern)`

`SUBSTRING(source FROM pattern)`, or the function form `substring(source, pattern)`, returns the
text of the first match: the first capture group when the pattern has one, otherwise the whole
match, and `NULL` when nothing matches. A quoted second argument is a pattern, as in PostgreSQL, so
`substring('hello' FROM '2')` searches for `2`; pass an integer (`FROM 2`) for the position form.
The SQL-standard `SIMILAR` form (`FROM pattern FOR escape`) is not supported.

<LiveSql query={substringRegex} rows={1} />

## `regexp_matches`

`regexp_matches(source, pattern [, flags])` is the set-returning form: one `text[]` row per match,
each shaped like `regexp_match`'s result. Without the `g` flag it returns at most one row (zero
rows when there is no match); with `g` it returns every match. It is a table function, so call it
in `FROM` (a `FROM t CROSS JOIN regexp_matches(t.col, …)` runs it once per row of `t`).

<LiveSql demo="regexpMatches" query={matches} rows={2} />

## `regexp_split_to_table` / `regexp_split_to_array`

These split the source at every match of the pattern. `regexp_split_to_table` returns one row per
piece, and `regexp_split_to_array` returns the pieces as a `text[]`. As in PostgreSQL, an empty match
at the start or end of the string, or right after the previous match, does not split, so the empty
pattern `''` splits a string into its characters.

<LiveSql demo="regexpSplitTable" query={splitTable} rows={2} />

<LiveSql demo="regexpSplitArray" {seed} query={splitArray} rows={3} />

The `i` flag makes any of these case-insensitive. `g` makes `regexp_replace` replace every match
and `regexp_matches` return every match. The other functions reject it with `22023`, as PostgreSQL
does. A malformed pattern raises `2201B` (`invalid_regular_expression`).
