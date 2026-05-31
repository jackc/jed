# spec/grammar/ — the SQL grammar

One **EBNF** grammar, language-neutral. Parsers are **hand-written per language** from
this grammar (the parser is explicitly *not* codegen'd — CLAUDE.md §5); the grammar is
the shared contract they must all accept.

> Status: empty. Populated when the first vertical slice (CLAUDE.md §11 step 5) needs a
> parseable surface (`CREATE TABLE` / `INSERT` / `SELECT ... WHERE pk = $1`).
