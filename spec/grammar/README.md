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
| DML | `INSERT … VALUES` (positional), `UPDATE … SET … [WHERE]`, `DELETE FROM … [WHERE]` |
| Query | `SELECT` (`*` / column & `CAST` list) `FROM`, `WHERE`, `ORDER BY [ASC\|DESC]` |
| Predicate | single predicate: `col IS [NOT] NULL` or `col <cmp> operand` (no AND/OR/NOT) |
| Expression | `CAST(expr AS type)` (nestable), integer & `NULL` literals, column refs |

> Status: covers the step-5 / step-6 surface (CLAUDE.md §11). Deferred constructs —
> compound predicates, JOINs, aliases, multi-row VALUES, parameters, LIMIT/OFFSET,
> string/decimal/boolean literals — are added here *first* as their features land.
