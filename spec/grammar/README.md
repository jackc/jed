# spec/grammar/ — the SQL grammar

One **EBNF** grammar, language-neutral: [grammar.ebnf](grammar.ebnf). Parsers are
**hand-written per language** from it (the parser is explicitly *not* codegen'd —
CLAUDE.md §5); the grammar is the shared contract they must all accept. It is
**descriptive of the implemented surface**, not aspirational, and grows one production
at a time as features land. The *why* — notation, deliberate narrowings, the growth
rule — lives in [../design/grammar.md](../design/grammar.md).

## Covered surface

| Area | Productions |
|---|---|
| DDL | `CREATE TABLE` (typed columns, single-column `PRIMARY KEY`) |
| DML | `INSERT … VALUES` (positional literals), `UPDATE … SET = expr … [WHERE expr]`, `DELETE FROM … [WHERE expr]` |
| Query | `SELECT` (`*` or an `expr` list) `FROM`, `WHERE expr`, `ORDER BY [ASC\|DESC]` |
| Expression | one `expr` precedence tower: `OR` < `AND` < `NOT` < comparison / `IS [NOT] NULL` < `+ -` < `* / %` < unary `-` < primary; primary = integer / `TRUE` / `FALSE` / `NULL` / column / `CAST(expr AS type)` / `( expr )` |

> Status: covers the step-5 / step-6 surface plus the general expression substrate
> (arithmetic, the expression-only `boolean`, AND/OR/NOT — CLAUDE.md §11, Phase 1).
> Deferred constructs — JOINs, aliases, multi-row VALUES, parameters, LIMIT/OFFSET,
> function-call syntax, string/decimal literals — are added here *first* as their
> features land.
