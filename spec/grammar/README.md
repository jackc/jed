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
| DDL | `CREATE TABLE` (typed columns; column- and table-level `PRIMARY KEY` incl. composite; `NOT NULL`; `DEFAULT` constant or expression; `CHECK`; `UNIQUE`), `DROP TABLE`, `CREATE [UNIQUE] INDEX` / `DROP INDEX` |
| DML | `INSERT … VALUES` (positional, explicit column list, multi-row, the `DEFAULT` keyword) and `INSERT … SELECT`, `UPDATE … SET`, `DELETE`, the `RETURNING` clause (incl. `old.`/`new.` qualifiers) |
| Query | `SELECT` (`*` / `expr` list / `AS` aliases) `FROM` (tables with aliases; `[INNER\|LEFT\|RIGHT\|FULL] JOIN … ON` / `CROSS JOIN`; the set-returning `generate_series`; FROM-less); `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY` (multi-key, per-key `ASC\|DESC`, `NULLS FIRST\|LAST`), `LIMIT`, `OFFSET`, `DISTINCT`; set operations `UNION`/`INTERSECT`/`EXCEPT` (each `[ALL]`) |
| Expression | the full precedence tower: `OR`/`AND`/`NOT`, comparisons, `IS [NOT] NULL`, `IS [NOT] DISTINCT FROM`, `+ - * / %` and unary `-`, `IN`/`BETWEEN`/`LIKE`, `CASE` (searched + simple), function calls (named + `DEFAULT` arguments), scalar / `IN` / `EXISTS` subqueries (correlated), `$N` bind parameters, `CAST(expr AS type)` and `expr::type`, typed string literals (`type 'string'`, `TIMESTAMP '…'`, …), and integer/decimal literals (incl. scientific `e`-notation) |
| Transactions | `BEGIN`/`START TRANSACTION` (`READ ONLY`/`READ WRITE`) … `COMMIT`/`END` / `ROLLBACK` |

> Status: descriptive of the **full implemented surface** across all three cores —
> Phases 1–8 (CLAUDE.md §11 + [../../TODO.md](../../TODO.md)). It grows one production at a
> time as features land. Constructs still deferred (added here *first* when they land) —
> e.g. `COUNT(DISTINCT x)`, LATERAL / select-list set-returning functions, and the further
> surfaces enumerated in [../design/grammar.md](../design/grammar.md).
