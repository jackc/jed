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

	const redos = `SELECT 'aaaaaaaaaaaaaaaaaaaa!' ~ '(a+)+\$' AS matched;`;
</script>

# Pattern matching

jed offers three ways to match text against a pattern: SQL `LIKE` / `ILIKE`, the
regular-expression operators `~` `~*` `!~` `!~*`, and the `regexp_replace` / `regexp_match`
functions.

## `LIKE` / `ILIKE`

`LIKE` is the SQL wildcard match: `%` matches any run of characters, `_` matches exactly one.
`ILIKE` is the case-insensitive form. The match is by Unicode code point.

<LiveSql {seed} query={like} rows={4} />

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

The `i` flag makes any of these case-insensitive; for `regexp_replace`, `g` makes it global.
A malformed pattern raises `2201B` (`invalid_regular_expression`).
