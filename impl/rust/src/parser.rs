//! Hand-written recursive-descent parser (CLAUDE.md §5, §10).
//!
//! Statement productions are filled in feature-by-feature (Phases B–E). Until a
//! production is implemented it returns a structured `0A000` feature-not-supported
//! error rather than panicking, so the harness reports "not yet" cleanly.

use crate::ast::{
    Assignment, BinaryOp, ColumnDef, CreateTable, Delete, DropTable, Expr, Insert, InsertSource,
    InsertValue, JoinClause, JoinKind, Literal, OrderKey, QueryExpr, Select, SelectItem,
    SelectItems, SetOp, SetOpKind, Statement, TableRef, TypeMod, UnaryOp, Update,
};
use crate::decimal::Decimal;
use crate::error::{EngineError, Result, SqlState};
use crate::lexer::lex;
use crate::token::Token;

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Parser { tokens, pos: 0 }
    }

    /// Parse a single complete statement from `sql`.
    pub fn parse_sql(sql: &str) -> Result<Statement> {
        let tokens = lex(sql)?;
        let mut p = Parser::new(tokens);
        let stmt = p.parse_statement()?;
        p.expect_eof()?;
        Ok(stmt)
    }

    fn parse_statement(&mut self) -> Result<Statement> {
        // Dispatch on the leading keyword. Remaining productions land in Phases D–E.
        match self.peek_keyword().as_deref() {
            Some("create") => Ok(Statement::CreateTable(self.parse_create_table()?)),
            Some("drop") => Ok(Statement::DropTable(self.parse_drop_table()?)),
            Some("insert") => Ok(Statement::Insert(self.parse_insert()?)),
            Some("select") => self.parse_query_expr(),
            Some("update") => Ok(Statement::Update(self.parse_update()?)),
            Some("delete") => Ok(Statement::Delete(self.parse_delete()?)),
            Some(other) => Err(syntax(format!("unexpected keyword '{other}'"))),
            None => Err(syntax("expected a SQL statement")),
        }
    }

    /// `CREATE TABLE <name> ( <coldef> [, <coldef>]* )`, where each `<coldef>` is
    /// `<name> <typename> [PRIMARY KEY]`. Type names are kept as written and
    /// resolved during execution (the catalog owns the type lattice).
    fn parse_create_table(&mut self) -> Result<CreateTable> {
        self.expect_keyword("create")?;
        self.expect_keyword("table")?;
        let name = self.expect_identifier()?;
        self.expect(&Token::LParen)?;

        let mut columns = Vec::new();
        loop {
            columns.push(self.parse_column_def()?);
            match self.advance() {
                Token::Comma => continue,
                Token::RParen => break,
                other => return Err(syntax(format!("expected ',' or ')', found {other:?}"))),
            }
        }
        if columns.is_empty() {
            return Err(syntax("a table must have at least one column"));
        }
        Ok(CreateTable { name, columns })
    }

    fn parse_column_def(&mut self) -> Result<ColumnDef> {
        let name = self.expect_identifier()?;
        let type_name = self.expect_identifier()?;
        let type_mod = self.parse_type_mod()?;
        // Zero or more order-free column constraints: `PRIMARY KEY`, `NOT NULL`, and
        // `DEFAULT <literal>`. A boolean constraint may be repeated harmlessly; a repeated
        // `DEFAULT` just keeps the last (the catalog stores one default).
        let mut primary_key = false;
        let mut not_null = false;
        let mut default = None;
        loop {
            match self.peek_keyword().as_deref() {
                Some("primary") => {
                    self.advance();
                    self.expect_keyword("key")?;
                    primary_key = true;
                }
                Some("not") => {
                    self.advance();
                    self.expect_keyword("null")?;
                    not_null = true;
                }
                Some("default") => {
                    self.advance();
                    default = Some(self.parse_literal()?);
                }
                _ => break,
            }
        }
        Ok(ColumnDef {
            name,
            type_name,
            type_mod,
            primary_key,
            not_null,
            default,
        })
    }

    /// Parse an optional parenthesized type modifier `"(" integer ("," integer)? ")"` that
    /// follows a type name (the first parameterized type, decimal — spec/grammar/grammar.ebnf
    /// `type_name`). The shape is accepted for any type name; whether a typmod is *meaningful*
    /// (decimal only) and in range (1..=1000, 0..=p) is decided at resolve. Empty parens or a
    /// non-integer inside is a 42601 syntax error.
    fn parse_type_mod(&mut self) -> Result<Option<TypeMod>> {
        if !matches!(self.peek(), Token::LParen) {
            return Ok(None);
        }
        self.advance(); // '('
        let precision = self.expect_typmod_int()?;
        let scale = if matches!(self.peek(), Token::Comma) {
            self.advance();
            Some(self.expect_typmod_int()?)
        } else {
            None
        };
        self.expect(&Token::RParen)?;
        Ok(Some(TypeMod { precision, scale }))
    }

    fn expect_typmod_int(&mut self) -> Result<u64> {
        match self.advance() {
            Token::Int(m) => Ok(m),
            other => Err(syntax(format!(
                "expected an integer type modifier, found {other:?}"
            ))),
        }
    }

    /// `DROP TABLE <name>`. Removes the named table. A missing table is rejected at
    /// execution time (42P01), not here. Single table; no `IF EXISTS`, no
    /// `CASCADE` / `RESTRICT` this slice (spec/design/grammar.md §13).
    fn parse_drop_table(&mut self) -> Result<DropTable> {
        self.expect_keyword("drop")?;
        self.expect_keyword("table")?;
        let name = self.expect_identifier()?;
        Ok(DropTable { name })
    }

    /// `INSERT INTO <table> [( <col> [, <col>]* )] ( VALUES <row> [, <row>]* | <select> )`. The
    /// source is either a VALUES list (each `<row>` is `( <value> [, <value>]* )`, each `<value>`
    /// a literal or the `DEFAULT` keyword) or a SELECT (INSERT ... SELECT — §24). The optional
    /// column list names the target columns; unlisted columns take their default. The executor
    /// resolves names + type-checks each row and inserts all-or-nothing (spec/design/grammar.md
    /// §12 / §24, constraints.md §2).
    fn parse_insert(&mut self) -> Result<Insert> {
        self.expect_keyword("insert")?;
        self.expect_keyword("into")?;
        let table = self.expect_identifier()?;

        // Optional column list `( col [, col]* )` before VALUES. An empty `()` is rejected
        // (the first `expect_identifier` errors 42601 on `)`).
        let columns = if matches!(self.peek(), Token::LParen) {
            self.advance(); // '('
            let mut names = Vec::new();
            loop {
                names.push(self.expect_identifier()?);
                match self.advance() {
                    Token::Comma => continue,
                    Token::RParen => break,
                    other => return Err(syntax(format!("expected ',' or ')', found {other:?}"))),
                }
            }
            Some(names)
        } else {
            None
        };

        // The source is EITHER a SELECT (INSERT ... SELECT — §24) OR a VALUES list. `VALUES`
        // and `SELECT` are disjoint leading keywords, so a peek decides without lookahead.
        let source = if self.peek_keyword().as_deref() == Some("select") {
            InsertSource::Select(Box::new(self.parse_select()?))
        } else {
            self.expect_keyword("values")?;
            let mut rows = Vec::new();
            loop {
                rows.push(self.parse_insert_row()?);
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                    continue;
                }
                break;
            }
            InsertSource::Values(rows)
        };
        Ok(Insert {
            table,
            columns,
            source,
        })
    }

    /// One parenthesized `( <value> [, <value>]* )` row of an INSERT.
    fn parse_insert_row(&mut self) -> Result<Vec<InsertValue>> {
        self.expect(&Token::LParen)?;
        let mut values = Vec::new();
        loop {
            values.push(self.parse_insert_value()?);
            match self.advance() {
                Token::Comma => continue,
                Token::RParen => break,
                other => return Err(syntax(format!("expected ',' or ')', found {other:?}"))),
            }
        }
        if values.is_empty() {
            return Err(syntax("a VALUES row must have at least one value"));
        }
        Ok(values)
    }

    /// One INSERT value slot: the `DEFAULT` keyword (not reserved — §3), a bind parameter
    /// (`$N`, bound at execute — spec/design/api.md §5), else a literal.
    fn parse_insert_value(&mut self) -> Result<InsertValue> {
        if self.peek_keyword().as_deref() == Some("default") {
            self.advance();
            Ok(InsertValue::Default)
        } else if let Token::Param(n) = self.peek() {
            let n = *n;
            self.advance();
            Ok(InsertValue::Param(n))
        } else {
            Ok(InsertValue::Lit(self.parse_literal()?))
        }
    }

    /// A literal value for INSERT: an integer (with an optional leading unary minus,
    /// folded here), or one of the keywords `NULL` / `TRUE` / `FALSE`. INSERT takes
    /// literals only — not general expressions (spec/grammar/grammar.ebnf `literal`).
    fn parse_literal(&mut self) -> Result<Literal> {
        let negate = if matches!(self.peek(), Token::Minus) {
            self.advance();
            true
        } else {
            false
        };
        match self.advance() {
            Token::Int(m) => {
                let signed = if negate { -(m as i128) } else { m as i128 };
                i64::try_from(signed).map(Literal::Int).map_err(|_| {
                    EngineError::new(
                        SqlState::NumericValueOutOfRange,
                        "value out of range: integer literal exceeds the maximum signed 64-bit value",
                    )
                })
            }
            Token::Decimal(digits, scale) => {
                // A decimal literal carries the unscaled coefficient + scale; the leading
                // unary minus (if any) folds into the sign. Cap checks are at resolve.
                Ok(Literal::Decimal(Decimal::from_digits_scale(
                    negate, &digits, scale,
                )))
            }
            Token::Str(s) if !negate => Ok(Literal::Text(s)),
            Token::Word(w) if !negate && w.eq_ignore_ascii_case("null") => Ok(Literal::Null),
            Token::Word(w) if !negate && w.eq_ignore_ascii_case("true") => Ok(Literal::Bool(true)),
            Token::Word(w) if !negate && w.eq_ignore_ascii_case("false") => {
                Ok(Literal::Bool(false))
            }
            other => Err(syntax(format!("expected a literal value, found {other:?}"))),
        }
    }

    /// `SELECT [DISTINCT] <items> FROM <table> [WHERE <predicate>] [ORDER BY <key> [,
    /// <key>]*] [LIMIT <count>] [OFFSET <count>]`, where `<items>` is `*` or a
    /// comma-separated list of column refs / CASTs. LIMIT/OFFSET may appear in either
    /// order (§9).
    ///
    /// `DISTINCT` is not a reserved word (a column may be named `distinct`), and it is
    /// the only modifier before the select list, so it takes a two-token lookahead: the
    /// leading `DISTINCT` is the modifier iff the next token is neither `FROM` nor
    /// end-of-input — otherwise the word is a column named `distinct`
    /// (spec/design/grammar.md §11). This rule must be byte-identical across cores.
    /// Parse a top-level query expression (spec/design/grammar.md §25): one or more `select_core`s
    /// combined by `UNION`/`INTERSECT`/`EXCEPT`, with an optional trailing `ORDER BY`/`LIMIT`/
    /// `OFFSET` applying to the whole result. A lone query (no set operator) folds the trailing
    /// clauses back onto the single `Select` and is returned as `Statement::Select`, leaving the
    /// plain-query path untouched; otherwise it is a `Statement::SetOp`.
    fn parse_query_expr(&mut self) -> Result<Statement> {
        let node = self.parse_set_expr()?;
        let order_by = self.parse_order_by()?;
        let (limit, offset) = self.parse_limit_offset()?;
        match node {
            QueryExpr::Select(mut sel) => {
                sel.order_by = order_by;
                sel.limit = limit;
                sel.offset = offset;
                Ok(Statement::Select(*sel))
            }
            QueryExpr::SetOp(mut so) => {
                so.order_by = order_by;
                so.limit = limit;
                so.offset = offset;
                Ok(Statement::SetOp(*so))
            }
        }
    }

    /// `set_expr ::= intersect_expr (("UNION" | "EXCEPT") ("ALL"|"DISTINCT")? intersect_expr)*` —
    /// the lower-precedence, left-associative level. `INTERSECT` binds tighter (parsed inside
    /// `parse_intersect_expr`), so `a UNION b INTERSECT c` becomes `a UNION (b INTERSECT c)`.
    fn parse_set_expr(&mut self) -> Result<QueryExpr> {
        let mut left = self.parse_intersect_expr()?;
        loop {
            let op = match self.peek_keyword().as_deref() {
                Some("union") => SetOpKind::Union,
                Some("except") => SetOpKind::Except,
                _ => break,
            };
            self.advance(); // UNION | EXCEPT
            let all = self.parse_setop_quantifier();
            let right = self.parse_intersect_expr()?;
            left = QueryExpr::SetOp(Box::new(SetOp {
                op,
                all,
                lhs: left,
                rhs: right,
                order_by: Vec::new(),
                limit: None,
                offset: None,
            }));
        }
        Ok(left)
    }

    /// `intersect_expr ::= select_core ("INTERSECT" ("ALL"|"DISTINCT")? select_core)*` — the
    /// higher-precedence, left-associative `INTERSECT` level.
    fn parse_intersect_expr(&mut self) -> Result<QueryExpr> {
        let mut left = QueryExpr::Select(Box::new(self.parse_select_core()?));
        while self.peek_keyword().as_deref() == Some("intersect") {
            self.advance(); // INTERSECT
            let all = self.parse_setop_quantifier();
            let right = QueryExpr::Select(Box::new(self.parse_select_core()?));
            left = QueryExpr::SetOp(Box::new(SetOp {
                op: SetOpKind::Intersect,
                all,
                lhs: left,
                rhs: right,
                order_by: Vec::new(),
                limit: None,
                offset: None,
            }));
        }
        Ok(left)
    }

    /// The optional quantifier after a set operator: `ALL` (multiset) or `DISTINCT` (the explicit
    /// spelling of the deduplicating default). Returns whether `ALL` was given.
    fn parse_setop_quantifier(&mut self) -> bool {
        match self.peek_keyword().as_deref() {
            Some("all") => {
                self.advance();
                true
            }
            Some("distinct") => {
                self.advance();
                false
            }
            _ => false,
        }
    }

    /// A complete `SELECT` with its own optional trailing `ORDER BY`/`LIMIT`/`OFFSET` — the form an
    /// `INSERT ... SELECT` source takes (spec/design/grammar.md §24). Behaviorally identical to the
    /// pre-set-operations `select`: a `select_core` plus the trailing clauses.
    fn parse_select(&mut self) -> Result<Select> {
        let mut sel = self.parse_select_core()?;
        sel.order_by = self.parse_order_by()?;
        let (limit, offset) = self.parse_limit_offset()?;
        sel.limit = limit;
        sel.offset = offset;
        Ok(sel)
    }

    /// `select_core ::= "SELECT" "DISTINCT"? select_items "FROM" from_clause where? group_by?
    /// having?` — a `SELECT` WITHOUT a trailing `ORDER BY`/`LIMIT`/`OFFSET` (the operand form of a
    /// set operation). The returned `Select` has empty `order_by` and no `limit`/`offset`.
    fn parse_select_core(&mut self) -> Result<Select> {
        self.expect_keyword("select")?;

        let distinct = if self.peek_keyword().as_deref() == Some("distinct") {
            let modifier = match self.tokens.get(self.pos + 1) {
                Some(Token::Word(w)) => !w.eq_ignore_ascii_case("from"),
                Some(Token::Eof) | None => false,
                Some(_) => true,
            };
            if modifier {
                self.advance();
            }
            modifier
        } else {
            false
        };

        let items = self.parse_select_items()?;
        self.expect_keyword("from")?;
        let (from, joins) = self.parse_from_clause()?;

        let filter = self.parse_optional_where()?;

        let group_by = self.parse_group_by()?;

        let having = self.parse_having()?;

        Ok(Select {
            distinct,
            items,
            from,
            joins,
            filter,
            group_by,
            having,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        })
    }

    /// `having_clause ::= "HAVING" expr` (grammar.md §19), after GROUP BY and before ORDER BY.
    /// `HAVING` is not reserved, so it is a clause only in this position; the predicate is a
    /// general expression (it may reference aggregates) checked for boolean at resolve.
    fn parse_having(&mut self) -> Result<Option<Expr>> {
        if self.peek_keyword().as_deref() != Some("having") {
            return Ok(None);
        }
        self.advance(); // HAVING
        Ok(Some(self.parse_expr()?))
    }

    /// `group_by ::= "GROUP" "BY" column_ref ("," column_ref)*` (grammar.md §18). Parsed after
    /// WHERE, before ORDER BY. Empty when absent. Each key is a bare/qualified column (never an
    /// expression/alias/ordinal — the same narrowing ORDER BY makes). `GROUP` is not reserved,
    /// so it is a clause only when immediately followed by `BY`.
    fn parse_group_by(&mut self) -> Result<Vec<Expr>> {
        if self.peek_keyword().as_deref() != Some("group") {
            return Ok(Vec::new());
        }
        self.advance(); // GROUP
        self.expect_keyword("by")?;
        let mut keys = Vec::new();
        loop {
            let (qualifier, name) = self.parse_column_ref()?;
            keys.push(match qualifier {
                Some(qualifier) => Expr::QualifiedColumn { qualifier, name },
                None => Expr::Column(name),
            });
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(keys)
    }

    /// `from_clause ::= table_ref join_clause*` (spec/grammar/grammar.ebnf, grammar.md §15).
    /// The first table reference followed by a left-deep chain of zero or more joins. The
    /// join keywords are not reserved (§3); the loop recognizes a join only by a leading
    /// join keyword (`JOIN` / `INNER`/`CROSS`/`LEFT`/`RIGHT`/`FULL` ... `JOIN`), so any other
    /// trailing word ends the FROM clause.
    fn parse_from_clause(&mut self) -> Result<(TableRef, Vec<JoinClause>)> {
        let from = self.parse_table_ref()?;
        let mut joins = Vec::new();
        while let Some(j) = self.parse_join_clause()? {
            joins.push(j);
        }
        Ok((from, joins))
    }

    /// `table_ref ::= identifier ("AS"? identifier)?` — a table name with an optional alias.
    /// An explicit `AS` takes the next identifier unconditionally; an implicit alias is taken
    /// only when the next token is a word that is NOT a clause/join keyword (so `FROM t WHERE`
    /// and `FROM t JOIN ...` keep no alias). The stop-keyword set is a §8 cross-core surface
    /// (grammar.md §15).
    fn parse_table_ref(&mut self) -> Result<TableRef> {
        let name = self.expect_identifier()?;
        let alias = if self.peek_keyword().as_deref() == Some("as") {
            self.advance();
            Some(self.expect_identifier()?)
        } else {
            match self.peek() {
                Token::Word(w) if !is_table_ref_stop_keyword(&w.to_ascii_lowercase()) => {
                    let alias = w.clone();
                    self.advance();
                    Some(alias)
                }
                _ => None,
            }
        };
        Ok(TableRef { name, alias })
    }

    /// Parse one `join_clause` if a join keyword begins here, else `None` (ending the FROM
    /// chain). `CROSS JOIN` has no `ON`; the `INNER`/outer kinds require `ON <expr>` (a missing
    /// `ON` is 42601). The outer kinds (`LEFT`/`RIGHT`/`FULL [OUTER]`) parse into the AST but
    /// are rejected at execution (0A000) — spec/design/grammar.md §15.
    fn parse_join_clause(&mut self) -> Result<Option<JoinClause>> {
        let kw = match self.peek_keyword() {
            Some(k) => k,
            None => return Ok(None),
        };
        let (kind, is_cross) = match kw.as_str() {
            // A bare JOIN is INNER.
            "join" => {
                self.advance();
                (JoinKind::Inner, false)
            }
            "inner" => {
                self.advance();
                self.expect_keyword("join")?;
                (JoinKind::Inner, false)
            }
            "cross" => {
                self.advance();
                self.expect_keyword("join")?;
                (JoinKind::Cross, true)
            }
            "left" | "right" | "full" => {
                self.advance();
                // Optional OUTER.
                if self.peek_keyword().as_deref() == Some("outer") {
                    self.advance();
                }
                self.expect_keyword("join")?;
                let kind = match kw.as_str() {
                    "left" => JoinKind::Left,
                    "right" => JoinKind::Right,
                    _ => JoinKind::Full,
                };
                (kind, false)
            }
            // Not a join keyword: the FROM chain ends here.
            _ => return Ok(None),
        };
        let table = self.parse_table_ref()?;
        let on = if is_cross {
            None
        } else {
            self.expect_keyword("on")?;
            Some(self.parse_expr()?)
        };
        Ok(Some(JoinClause { kind, table, on }))
    }

    /// Parse an optional `ORDER BY <key> ("," <key>)*`, where each key is a bare column with
    /// an optional `ASC`/`DESC` and an optional `NULLS FIRST|LAST`. `nulls_first` is resolved
    /// here: explicit if given, else the direction default (ASC → last, DESC → first). A bare
    /// `NULLS` not followed by `FIRST`/`LAST` is a syntax error (42601). Returns an empty vec
    /// when there is no ORDER BY (spec/grammar/grammar.ebnf `order_by`).
    fn parse_order_by(&mut self) -> Result<Vec<OrderKey>> {
        let mut keys = Vec::new();
        if self.peek_keyword().as_deref() != Some("order") {
            return Ok(keys);
        }
        self.advance();
        self.expect_keyword("by")?;
        loop {
            let (qualifier, column) = self.parse_column_ref()?;
            let descending = match self.peek_keyword().as_deref() {
                Some("asc") => {
                    self.advance();
                    false
                }
                Some("desc") => {
                    self.advance();
                    true
                }
                _ => false,
            };
            let nulls_first = if self.peek_keyword().as_deref() == Some("nulls") {
                self.advance();
                match self.peek_keyword().as_deref() {
                    Some("first") => {
                        self.advance();
                        true
                    }
                    Some("last") => {
                        self.advance();
                        false
                    }
                    other => {
                        return Err(syntax(format!(
                            "NULLS must be followed by FIRST or LAST, found {other:?}"
                        )));
                    }
                }
            } else {
                // No explicit clause: default follows direction (grammar.md §10).
                // NULL is the largest value (PostgreSQL model), so ASC → NULLS LAST,
                // DESC → NULLS FIRST.
                descending
            };
            keys.push(OrderKey {
                qualifier,
                column,
                descending,
                nulls_first,
            });
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(keys)
    }

    /// Parse an optional trailing `LIMIT <count>` and/or `OFFSET <count>` in either
    /// order, each at most once (a repeat is a syntax error, 42601). Returns the
    /// resolved non-negative counts (spec/grammar/grammar.ebnf `limit_offset`).
    fn parse_limit_offset(&mut self) -> Result<(Option<i64>, Option<i64>)> {
        let mut limit = None;
        let mut offset = None;
        loop {
            match self.peek_keyword().as_deref() {
                Some("limit") if limit.is_none() => {
                    self.advance();
                    limit = Some(self.parse_count(true)?);
                }
                Some("offset") if offset.is_none() => {
                    self.advance();
                    offset = Some(self.parse_count(false)?);
                }
                Some("limit") => return Err(syntax("duplicate LIMIT clause")),
                Some("offset") => return Err(syntax("duplicate OFFSET clause")),
                _ => break,
            }
        }
        Ok((limit, offset))
    }

    /// A LIMIT/OFFSET count: a non-negative integer literal. The sign is folded as in
    /// `parse_literal`; a negative value is rejected at parse time with 2201W (LIMIT) /
    /// 2201X (OFFSET), and a positive magnitude over i64::MAX traps 22003 (the value -0
    /// folds to 0 and is accepted). `is_limit` selects which structured error to raise.
    fn parse_count(&mut self, is_limit: bool) -> Result<i64> {
        let negate = if matches!(self.peek(), Token::Minus) {
            self.advance();
            true
        } else {
            false
        };
        match self.advance() {
            Token::Int(m) => {
                let signed = if negate { -(m as i128) } else { m as i128 };
                if signed < 0 {
                    let (state, what) = if is_limit {
                        (SqlState::InvalidRowCountInLimitClause, "LIMIT")
                    } else {
                        (SqlState::InvalidRowCountInOffsetClause, "OFFSET")
                    };
                    return Err(EngineError::new(
                        state,
                        format!("{what} must not be negative"),
                    ));
                }
                i64::try_from(signed).map_err(|_| {
                    EngineError::new(
                        SqlState::NumericValueOutOfRange,
                        "value out of range: count exceeds the maximum signed 64-bit value",
                    )
                })
            }
            other => Err(syntax(format!(
                "expected an integer count, found {other:?}"
            ))),
        }
    }

    /// `UPDATE <table> SET <col> = <operand> [, <col> = <operand>]* [WHERE <pred>]`.
    fn parse_update(&mut self) -> Result<Update> {
        self.expect_keyword("update")?;
        let table = self.expect_identifier()?;
        self.expect_keyword("set")?;

        let mut assignments = Vec::new();
        loop {
            let column = self.expect_identifier()?;
            self.expect(&Token::Eq)?;
            let value = self.parse_expr()?;
            assignments.push(Assignment { column, value });
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        if assignments.is_empty() {
            return Err(syntax("UPDATE must set at least one column"));
        }

        let filter = self.parse_optional_where()?;
        Ok(Update {
            table,
            assignments,
            filter,
        })
    }

    /// `DELETE FROM <table> [WHERE <pred>]`. No WHERE deletes every row.
    fn parse_delete(&mut self) -> Result<Delete> {
        self.expect_keyword("delete")?;
        self.expect_keyword("from")?;
        let table = self.expect_identifier()?;
        let filter = self.parse_optional_where()?;
        Ok(Delete { table, filter })
    }

    /// Parse an optional trailing `WHERE <expr>` (shared by SELECT/UPDATE/DELETE). The
    /// expression must resolve to boolean (checked by the executor).
    fn parse_optional_where(&mut self) -> Result<Option<Expr>> {
        if self.peek_keyword().as_deref() == Some("where") {
            self.advance();
            Ok(Some(self.parse_expr()?))
        } else {
            Ok(None)
        }
    }

    fn parse_select_items(&mut self) -> Result<SelectItems> {
        if matches!(self.peek(), Token::Star) {
            self.advance();
            return Ok(SelectItems::All);
        }
        let mut items = Vec::new();
        loop {
            let expr = self.parse_expr()?;
            // Optional `AS alias` output label. `AS` is not reserved, so it is taken as
            // an alias marker only here, after a complete expr (spec/grammar/grammar.ebnf
            // `select_item`). The alias never enters resolution (grammar.md §8).
            let alias = if self.peek_keyword().as_deref() == Some("as") {
                self.advance();
                Some(self.expect_identifier()?)
            } else {
                None
            };
            items.push(SelectItem { expr, alias });
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(SelectItems::Items(items))
    }

    // --- expression precedence ladder (spec/grammar/grammar.ebnf `expr`) ---------
    // Loosest to tightest: OR < AND < NOT < comparison/IS NULL < additive <
    // multiplicative < unary minus < primary. One function per level keeps the
    // grammar legible (CLAUDE.md §10). The precedence is authored data
    // (spec/functions/catalog.toml); this ladder must agree with it.

    /// Parse a general expression (the entry point for WHERE, the SELECT list, and
    /// UPDATE assignment values).
    fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_and()?;
        while self.peek_keyword().as_deref() == Some("or") {
            self.advance();
            let rhs = self.parse_and()?;
            lhs = binary(BinaryOp::Or, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_not()?;
        while self.peek_keyword().as_deref() == Some("and") {
            self.advance();
            let rhs = self.parse_not()?;
            lhs = binary(BinaryOp::And, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_not(&mut self) -> Result<Expr> {
        if self.peek_keyword().as_deref() == Some("not") {
            self.advance();
            // right-associative: NOT NOT x
            let operand = self.parse_not()?;
            return Ok(Expr::Unary {
                op: UnaryOp::Not,
                operand: Box::new(operand),
            });
        }
        self.parse_comparison()
    }

    /// One comparison, a postfix `IS [NOT] NULL`, or `IS [NOT] DISTINCT FROM`, all
    /// non-associative: `a = b = c` is a syntax error, and `a + 1 IS NULL` binds as
    /// `(a + 1) IS NULL`. After the shared `IS` `NOT`? the parser dispatches on the
    /// `NULL` vs `DISTINCT FROM` keyword (spec/grammar/grammar.ebnf `comparison`).
    fn parse_comparison(&mut self) -> Result<Expr> {
        let lhs = self.parse_additive()?;
        if self.peek_keyword().as_deref() == Some("is") {
            self.advance();
            let negated = if self.peek_keyword().as_deref() == Some("not") {
                self.advance();
                true
            } else {
                false
            };
            // IS [NOT] DISTINCT FROM <additive> — NULL-safe equality; else IS [NOT] NULL.
            if self.peek_keyword().as_deref() == Some("distinct") {
                self.advance();
                self.expect_keyword("from")?;
                let rhs = self.parse_additive()?;
                return Ok(Expr::IsDistinctFrom {
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                    negated,
                });
            }
            self.expect_keyword("null")?;
            return Ok(Expr::IsNull {
                operand: Box::new(lhs),
                negated,
            });
        }
        // `NOT`? (`IN` (...) | `BETWEEN` lo `AND` hi) — a `NOT` here is consumed only when
        // followed by one of these postfix-predicate keywords (two-token lookahead; the prefix
        // `NOT` was already taken by parse_not). They bind at the comparison level (35),
        // non-associative (grammar.md §20-§21).
        let negated = self.peek_keyword().as_deref() == Some("not")
            && matches!(
                self.peek_keyword_at(1).as_deref(),
                Some("in") | Some("between") | Some("like")
            );
        if negated {
            self.advance(); // NOT
        }
        if self.peek_keyword().as_deref() == Some("in") {
            self.advance();
            self.expect(&Token::LParen)?;
            // A non-empty value list (`IN ()` is rejected: parse_additive on `)` is 42601).
            let mut list = vec![self.parse_additive()?];
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                list.push(self.parse_additive()?);
            }
            self.expect(&Token::RParen)?;
            return Ok(Expr::In {
                lhs: Box::new(lhs),
                list,
                negated,
            });
        }
        if self.peek_keyword().as_deref() == Some("between") {
            self.advance();
            // Both bounds parse at the ADDITIVE level, which never consumes `AND` (a looser
            // level owned by parse_and). So the BETWEEN's structural `AND` is matched here and
            // `x BETWEEN a AND b AND c` parses as `(x BETWEEN a AND b) AND c` (grammar.md §21).
            let lo = self.parse_additive()?;
            self.expect_keyword("and")?;
            let hi = self.parse_additive()?;
            return Ok(Expr::Between {
                lhs: Box::new(lhs),
                lo: Box::new(lo),
                hi: Box::new(hi),
                negated,
            });
        }
        if self.peek_keyword().as_deref() == Some("like") {
            self.advance();
            let rhs = self.parse_additive()?;
            return Ok(Expr::Like {
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                negated,
            });
        }
        let op = match self.peek() {
            Token::Eq => Some(BinaryOp::Eq),
            Token::Lt => Some(BinaryOp::Lt),
            Token::Gt => Some(BinaryOp::Gt),
            Token::Le => Some(BinaryOp::Le),
            Token::Ge => Some(BinaryOp::Ge),
            _ => None,
        };
        match op {
            Some(op) => {
                self.advance();
                let rhs = self.parse_additive()?;
                Ok(binary(op, lhs, rhs))
            }
            None => Ok(lhs),
        }
    }

    fn parse_additive(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Token::Plus => BinaryOp::Add,
                Token::Minus => BinaryOp::Sub,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_multiplicative()?;
            lhs = binary(op, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Token::Star => BinaryOp::Mul,
                Token::Slash => BinaryOp::Div,
                Token::Percent => BinaryOp::Mod,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_unary()?;
            lhs = binary(op, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if matches!(self.peek(), Token::Minus) {
            self.advance();
            // Fold unary-minus-of-an-integer-literal into one negative literal: this
            // makes int64::MIN representable (`-(2^63)`) and lets the negative value
            // range-check against its context like any literal (spec/design/types.md §6).
            if let Token::Int(m) = self.peek() {
                let m = *m;
                self.advance();
                let folded = -(m as i128); // m <= 2^63 ⇒ folded ∈ [-2^63, 0] ⊆ i64
                return Ok(Expr::Literal(Literal::Int(folded as i64)));
            }
            // Fold unary-minus of a decimal literal into one negative decimal literal (like
            // the integer fold). Decimal negation never overflows.
            if matches!(self.peek(), Token::Decimal(..)) {
                if let Token::Decimal(digits, scale) = self.advance() {
                    return Ok(Expr::Literal(Literal::Decimal(Decimal::from_digits_scale(
                        true, &digits, scale,
                    ))));
                }
            }
            let operand = self.parse_unary()?;
            return Ok(Expr::Unary {
                op: UnaryOp::Neg,
                operand: Box::new(operand),
            });
        }
        self.parse_primary()
    }

    /// A primary: a parenthesized expression, `CAST(...)`, a literal (integer,
    /// `TRUE`/`FALSE`, `NULL`), or a column reference.
    fn parse_primary(&mut self) -> Result<Expr> {
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            let e = self.parse_expr()?;
            self.expect(&Token::RParen)?;
            return Ok(e);
        }
        if self.peek_keyword().as_deref() == Some("cast") {
            self.advance();
            self.expect(&Token::LParen)?;
            let inner = self.parse_expr()?;
            self.expect_keyword("as")?;
            let type_name = self.expect_identifier()?;
            let type_mod = self.parse_type_mod()?;
            self.expect(&Token::RParen)?;
            return Ok(Expr::Cast {
                inner: Box::new(inner),
                type_name,
                type_mod,
            });
        }
        if self.peek_keyword().as_deref() == Some("case") {
            self.advance();
            // Simple form has an operand between CASE and the first WHEN; the searched form
            // starts directly with WHEN (grammar.md §23).
            let operand = if self.peek_keyword().as_deref() == Some("when") {
                None
            } else {
                Some(Box::new(self.parse_expr()?))
            };
            let mut whens = Vec::new();
            while self.peek_keyword().as_deref() == Some("when") {
                self.advance();
                let cond = self.parse_expr()?;
                self.expect_keyword("then")?;
                let res = self.parse_expr()?;
                whens.push((cond, res));
            }
            if whens.is_empty() {
                return Err(syntax("CASE requires at least one WHEN clause"));
            }
            let els = if self.peek_keyword().as_deref() == Some("else") {
                self.advance();
                Some(Box::new(self.parse_expr()?))
            } else {
                None
            };
            self.expect_keyword("end")?;
            return Ok(Expr::Case {
                operand,
                whens,
                els,
            });
        }
        match self.peek() {
            Token::Param(n) => {
                let n = *n;
                self.advance();
                Ok(Expr::Param(n))
            }
            Token::Int(m) => {
                let m = *m;
                self.advance();
                if m <= i64::MAX as u64 {
                    Ok(Expr::Literal(Literal::Int(m as i64)))
                } else {
                    // The only m > i64::MAX the lexer admits is 2^63, which fits no
                    // signed integer type unless negated (handled by the unary fold).
                    Err(EngineError::new(
                        SqlState::NumericValueOutOfRange,
                        "value out of range: integer literal exceeds the maximum signed 64-bit value",
                    ))
                }
            }
            Token::Word(w) if w.eq_ignore_ascii_case("null") => {
                self.advance();
                Ok(Expr::Literal(Literal::Null))
            }
            Token::Word(w) if w.eq_ignore_ascii_case("true") => {
                self.advance();
                Ok(Expr::Literal(Literal::Bool(true)))
            }
            Token::Word(w) if w.eq_ignore_ascii_case("false") => {
                self.advance();
                Ok(Expr::Literal(Literal::Bool(false)))
            }
            Token::Str(_) => {
                if let Token::Str(s) = self.advance() {
                    Ok(Expr::Literal(Literal::Text(s)))
                } else {
                    unreachable!("peeked a string literal")
                }
            }
            Token::Decimal(..) => {
                if let Token::Decimal(digits, scale) = self.advance() {
                    Ok(Expr::Literal(Literal::Decimal(Decimal::from_digits_scale(
                        false, &digits, scale,
                    ))))
                } else {
                    unreachable!("peeked a decimal literal")
                }
            }
            Token::Word(_) => {
                // Function call: a BARE identifier IMMEDIATELY followed by "(" is a call
                // (grammar.md §17). The one-token lookahead keeps function names non-reserved
                // (a column may be named `count`/`abs`); a qualified name (`t.col`) is never a
                // call. Aggregate and scalar names resolve; any other name is 42883.
                if matches!(self.tokens.get(self.pos + 1), Some(Token::LParen)) {
                    return self.parse_function_call();
                }
                let (qualifier, name) = self.parse_column_ref()?;
                Ok(match qualifier {
                    Some(qualifier) => Expr::QualifiedColumn { qualifier, name },
                    None => Expr::Column(name),
                })
            }
            other => Err(syntax(format!("expected an expression, found {other:?}"))),
        }
    }

    /// `function_call ::= identifier "(" ( "*" | expr ("," expr)* ) ")"` — the shared
    /// aggregate/scalar call syntax (grammar.md §17). `COUNT(*)` is the `star` form; every
    /// other call takes a comma-separated argument list (resolution checks the per-function
    /// arity). DISTINCT inside the parens is deferred (rejected 42601). The function name is
    /// resolved (case-insensitively) against the catalog later.
    fn parse_function_call(&mut self) -> Result<Expr> {
        let name = self.expect_identifier()?;
        self.expect(&Token::LParen)?;
        // DISTINCT inside a function call (COUNT(DISTINCT x)) is deferred — reject at parse.
        if self.peek_keyword().as_deref() == Some("distinct") {
            return Err(syntax("DISTINCT inside an aggregate is not supported yet"));
        }
        let (args, star) = if matches!(self.peek(), Token::Star) {
            self.advance();
            (Vec::new(), true)
        } else {
            let mut args = vec![self.parse_expr()?];
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                args.push(self.parse_expr()?);
            }
            (args, false)
        };
        self.expect(&Token::RParen)?;
        Ok(Expr::FuncCall { name, args, star })
    }

    /// `column_ref ::= identifier ("." identifier)?` — a bare column name, or a qualified
    /// `rel.col` (the `.` is the `Dot` token). Returns `(qualifier, name)`; `qualifier` is
    /// `None` for a bare column (spec/grammar/grammar.ebnf `column_ref`, grammar.md §15).
    fn parse_column_ref(&mut self) -> Result<(Option<String>, String)> {
        let first = self.expect_identifier()?;
        if matches!(self.peek(), Token::Dot) {
            self.advance();
            let second = self.expect_identifier()?;
            Ok((Some(first), second))
        } else {
            Ok((None, first))
        }
    }

    // --- cursor helpers (used by the productions added in later phases) -------

    /// Peek the current token without consuming it.
    pub fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    /// The current token lowercased if it is a word, else None.
    pub fn peek_keyword(&self) -> Option<String> {
        match self.peek() {
            Token::Word(w) => Some(w.to_ascii_lowercase()),
            _ => None,
        }
    }

    /// The keyword (lowercased) `offset` tokens ahead of the cursor, if that token is a word.
    /// Used for the two-token `NOT IN`/`NOT BETWEEN`/`NOT LIKE` lookahead (a CLAUDE.md §8
    /// determinism surface — byte-identical across the three parsers).
    pub fn peek_keyword_at(&self, offset: usize) -> Option<String> {
        match self.tokens.get(self.pos + offset) {
            Some(Token::Word(w)) => Some(w.to_ascii_lowercase()),
            _ => None,
        }
    }

    /// Consume and return the current token.
    pub fn advance(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    /// Consume the current token, requiring it to equal `want`.
    pub fn expect(&mut self, want: &Token) -> Result<()> {
        let got = self.advance();
        if &got == want {
            Ok(())
        } else {
            Err(syntax(format!("expected {want:?}, found {got:?}")))
        }
    }

    /// Consume the current token, requiring it to be the given keyword
    /// (case-insensitive).
    pub fn expect_keyword(&mut self, kw: &str) -> Result<()> {
        match self.advance() {
            Token::Word(w) if w.eq_ignore_ascii_case(kw) => Ok(()),
            other => Err(syntax(format!("expected keyword '{kw}', found {other:?}"))),
        }
    }

    /// Consume the current token, requiring it to be a bare word, and return it.
    pub fn expect_identifier(&mut self) -> Result<String> {
        match self.advance() {
            Token::Word(w) => Ok(w),
            other => Err(syntax(format!("expected an identifier, found {other:?}"))),
        }
    }

    /// Require that all input has been consumed.
    pub fn expect_eof(&self) -> Result<()> {
        match self.peek() {
            Token::Eof => Ok(()),
            other => Err(syntax(format!("unexpected trailing input: {other:?}"))),
        }
    }
}

fn syntax(msg: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::SyntaxError, msg.into())
}

/// Whether `kw` (already lower-cased) is a keyword that may legally follow a `table_ref`,
/// and so must NOT be swallowed as an implicit table alias: a trailing clause keyword
/// (`where`/`order`/`limit`/`offset`) or any join-machinery keyword
/// (`join`/`inner`/`cross`/`left`/`right`/`full`/`outer`/`on`). `as` is handled separately
/// (explicit alias). This set is a CLAUDE.md §8 cross-core determinism surface
/// (spec/design/grammar.md §15).
fn is_table_ref_stop_keyword(kw: &str) -> bool {
    matches!(
        kw,
        "where"
            | "group"
            | "having"
            | "order"
            | "limit"
            | "offset"
            | "join"
            | "inner"
            | "cross"
            | "left"
            | "right"
            | "full"
            | "outer"
            | "on"
            | "as"
            // set operators end a SELECT core — they must not be swallowed as an implicit table
            // alias (`FROM a UNION ...` is a UNION, not a table `a` aliased `union`). §25.
            | "union"
            | "intersect"
            | "except"
    )
}

/// Build a binary-operator expression node.
fn binary(op: BinaryOp, lhs: Expr, rhs: Expr) -> Expr {
    Expr::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}
