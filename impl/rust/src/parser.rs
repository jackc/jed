//! Hand-written recursive-descent parser (CLAUDE.md §5, §10).
//!
//! Statement productions are filled in feature-by-feature (Phases B–E). Until a
//! production is implemented it returns a structured `0A000` feature-not-supported
//! error rather than panicking, so the harness reports "not yet" cleanly.

use crate::ast::{
    Assignment, BinaryOp, CheckDef, ColumnDef, CreateIndex, CreateTable, CreateType, Cte,
    DefaultDef, Delete, DropIndex, DropTable, DropType, Expr, ForeignKeyDef, Insert, InsertSource,
    InsertValue, JoinClause, JoinKind, Literal, OrderKey, QueryExpr, RefAction, Select, SelectItem,
    SelectItems, SetOp, SetOpKind, Statement, SubscriptSpec, TableRef, TypeFieldDef, TypeMod,
    UnaryOp, UniqueDef, Update, WithQuery,
};
use crate::decimal::Decimal;
use crate::error::{EngineError, Result, SqlState};
use crate::lexer::lex;
use crate::token::Token;

/// Maximum expression / subquery / set-operation **nesting depth** a statement may reach
/// (spec/design/cost.md §7; CLAUDE.md §13). The §13 native-stack-safety gate for untrusted
/// input: the recursive-descent parser and the resolve/eval walks recurse to a statement's
/// nesting depth, so deeply-nested SQL would overflow the call stack BEFORE the cost meter runs
/// (`54P01` cannot catch it). Counting logical depth against this **fixed** bound — rather than
/// PG's runtime stack-pointer probe — is deterministic and **cross-core identical** (§8): the
/// constant is the SAME in every core (Rust / Go / TS). `256` sits with a >2× margin under the
/// weakest core's native ceiling (the TS/Node default stack: ~547 nested subqueries) yet far
/// above any realistic query. Exceeding it aborts with `54001 statement_too_complex`.
pub const MAX_EXPR_DEPTH: usize = 256;

/// Maximum length, in **bytes**, of a single identifier — table / column / type / alias /
/// function name (spec/design/cost.md §7; CLAUDE.md §13). The §13 identifier-hardening gate for
/// untrusted input: an unbounded identifier would otherwise consume O(input) memory and land
/// verbatim in the on-disk catalog and keys. Checked in the lexer when an identifier token is
/// built (the *producer*, so every identifier on every parse path is bounded), aborting with
/// `42622 name_too_long`. Identifiers are ASCII-only (spec/design/grammar.md §3), so the byte
/// length is the character count. `63` matches PostgreSQL's `NAMEDATALEN − 1` boundary — but jed
/// **errors** where PG silently truncates (a documented PG divergence: jed has no notices, and a
/// silent truncation could collide two distinct names — CLAUDE.md §1). A fixed constant, so it is
/// deterministic and cross-core identical (§8): the same name is accepted or rejected in Rust /
/// Go / TS alike.
pub const MAX_IDENTIFIER_LENGTH: usize = 63;

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// Current expression/query nesting depth (see `MAX_EXPR_DEPTH`). Incremented once per AST
    /// level descended (`deepen`), restored on the way back up; left stale on the error path
    /// because a depth error aborts the whole parse.
    depth: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Parser {
            tokens,
            pos: 0,
            depth: 0,
        }
    }

    /// Descend one nesting level, enforcing `MAX_EXPR_DEPTH` (spec/design/cost.md §7). Call at
    /// every point the AST gains a level — a binary-chain step, a unary, a postfix, a re-entry
    /// into a fresh sub-expression, a nested subquery, a set-op branch. The caller restores the
    /// depth with `undeepen` on the success path (`?` short-circuits leave it stale, which is
    /// harmless: the parse is aborting).
    fn deepen(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_EXPR_DEPTH {
            return Err(EngineError::new(
                SqlState::StatementTooComplex,
                format!(
                    "statement too complex: nesting depth exceeds the maximum of {MAX_EXPR_DEPTH}"
                ),
            ));
        }
        Ok(())
    }

    /// Restore one nesting level taken by `deepen` (success path only).
    fn undeepen(&mut self) {
        self.depth -= 1;
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
            // CREATE / DROP dispatch on the object keyword (TABLE vs [UNIQUE] INDEX —
            // grammar.md §30; UNIQUE needs no lookahead of its own — after CREATE the next
            // word being UNIQUE can only be CREATE UNIQUE INDEX).
            Some("create")
                if self.peek_keyword_at(1).as_deref() == Some("index")
                    || self.peek_keyword_at(1).as_deref() == Some("unique") =>
            {
                Ok(Statement::CreateIndex(self.parse_create_index()?))
            }
            // CREATE TYPE — a 2-token lookahead keeps TYPE non-reserved (the CREATE UNIQUE INDEX
            // precedent — composite.md §1).
            Some("create") if self.peek_keyword_at(1).as_deref() == Some("type") => {
                Ok(Statement::CreateType(self.parse_create_type()?))
            }
            Some("create") => Ok(Statement::CreateTable(self.parse_create_table()?)),
            Some("drop") if self.peek_keyword_at(1).as_deref() == Some("index") => {
                Ok(Statement::DropIndex(self.parse_drop_index()?))
            }
            Some("drop") if self.peek_keyword_at(1).as_deref() == Some("type") => {
                Ok(Statement::DropType(self.parse_drop_type()?))
            }
            Some("drop") => Ok(Statement::DropTable(self.parse_drop_table()?)),
            Some("insert") => Ok(Statement::Insert(self.parse_insert()?)),
            Some("select") => self.parse_query_expr(),
            // `WITH …` at statement start can only begin a query with common table expressions
            // (spec/design/cte.md). `with` is non-reserved but unambiguous here.
            Some("with") => self.parse_with_statement(),
            Some("update") => Ok(Statement::Update(self.parse_update()?)),
            Some("delete") => Ok(Statement::Delete(self.parse_delete()?)),
            Some("begin") | Some("start") => self.parse_begin(),
            Some("commit") | Some("end") => self.parse_commit(),
            Some("rollback") => self.parse_rollback(),
            Some(other) => Err(syntax(format!("unexpected keyword '{other}'"))),
            None => Err(syntax("expected a SQL statement")),
        }
    }

    /// `BEGIN [TRANSACTION|WORK] [READ ONLY|READ WRITE]` or `START TRANSACTION [READ ONLY|READ
    /// WRITE]` — open an explicit transaction (spec/design/grammar.md §27). The access mode
    /// defaults to READ WRITE.
    fn parse_begin(&mut self) -> Result<Statement> {
        match self.peek_keyword().as_deref() {
            Some("start") => {
                self.advance();
                self.expect_keyword("transaction")?;
            }
            // plain BEGIN, with the optional noise word TRANSACTION | WORK
            _ => {
                self.advance(); // BEGIN
                if matches!(
                    self.peek_keyword().as_deref(),
                    Some("transaction") | Some("work")
                ) {
                    self.advance();
                }
            }
        }
        Ok(Statement::Begin {
            writable: self.parse_access_mode()?,
        })
    }

    /// The optional access mode after a transaction opener: `READ ONLY` → `Some(false)`,
    /// `READ WRITE` → `Some(true)`, absent → `None` (unspecified — the executor applies the
    /// handle's default: READ WRITE, or READ ONLY on a read-only handle; transactions.md §4.3,
    /// api.md §2.1).
    fn parse_access_mode(&mut self) -> Result<Option<bool>> {
        if self.peek_keyword().as_deref() != Some("read") {
            return Ok(None);
        }
        self.advance(); // READ
        match self.peek_keyword().as_deref() {
            Some("only") => {
                self.advance();
                Ok(Some(false))
            }
            Some("write") => {
                self.advance();
                Ok(Some(true))
            }
            other => Err(syntax(format!(
                "expected ONLY or WRITE after READ, found {other:?}"
            ))),
        }
    }

    /// `COMMIT [TRANSACTION|WORK]` / `END [TRANSACTION|WORK]` (spec/design/grammar.md §27).
    fn parse_commit(&mut self) -> Result<Statement> {
        self.advance(); // COMMIT or END
        self.consume_transaction_or_work();
        Ok(Statement::Commit)
    }

    /// `ROLLBACK [TRANSACTION|WORK]` (spec/design/grammar.md §27).
    fn parse_rollback(&mut self) -> Result<Statement> {
        self.expect_keyword("rollback")?;
        self.consume_transaction_or_work();
        Ok(Statement::Rollback)
    }

    /// Consume the optional trailing `TRANSACTION` / `WORK` noise word on a COMMIT/ROLLBACK.
    fn consume_transaction_or_work(&mut self) {
        if matches!(
            self.peek_keyword().as_deref(),
            Some("transaction") | Some("work")
        ) {
            self.advance();
        }
    }

    /// `CREATE TABLE <name> ( <element> [, <element>]* )`, where each `<element>` is a
    /// column definition or the table-level `PRIMARY KEY ( <col> [, <col>]* )` constraint
    /// (spec/design/grammar.md §28). An element starting with the two keywords `PRIMARY KEY`
    /// is the table constraint — nothing is lost, since a column named `primary` would need
    /// a type named `key`, which does not exist. Type names are kept as written and
    /// resolved during execution (the catalog owns the type lattice); the constraint's
    /// member names are likewise resolved there (42703/42701/42P16).
    fn parse_create_table(&mut self) -> Result<CreateTable> {
        self.expect_keyword("create")?;
        self.expect_keyword("table")?;
        let name = self.expect_identifier()?;
        self.expect(&Token::LParen)?;

        let mut columns = Vec::new();
        let mut table_pks = Vec::new();
        let mut checks = Vec::new();
        let mut uniques = Vec::new();
        let mut foreign_keys = Vec::new();
        loop {
            if self.peek_keyword().as_deref() == Some("primary")
                && self.peek_keyword_at(1).as_deref() == Some("key")
            {
                self.advance();
                self.advance();
                table_pks.push(self.parse_pk_column_list()?);
            } else if self.at_check_constraint() {
                checks.push(self.parse_check_constraint()?);
            } else if self.at_unique_table_constraint() {
                uniques.push(self.parse_unique_table_constraint()?);
            } else if self.at_foreign_key_table_constraint() {
                foreign_keys.push(self.parse_foreign_key_table_constraint()?);
            } else {
                columns.push(self.parse_column_def(
                    &mut checks,
                    &mut uniques,
                    &mut foreign_keys,
                )?);
            }
            match self.advance() {
                Token::Comma => continue,
                Token::RParen => break,
                other => return Err(syntax(format!("expected ',' or ')', found {other:?}"))),
            }
        }
        if columns.is_empty() {
            return Err(syntax("a table must have at least one column"));
        }
        Ok(CreateTable {
            name,
            columns,
            table_pks,
            checks,
            uniques,
            foreign_keys,
        })
    }

    /// Whether the cursor sits on a table-level `FOREIGN KEY` constraint: the two keywords
    /// `FOREIGN KEY`, or `CONSTRAINT <ident> FOREIGN KEY` (spec/design/grammar.md §43). The
    /// keywords stay non-reserved — a column named `foreign` would need a type named `key`
    /// (none exists), so the lookahead loses nothing (the `PRIMARY KEY` precedent).
    fn at_foreign_key_table_constraint(&self) -> bool {
        (self.peek_keyword().as_deref() == Some("foreign")
            && self.peek_keyword_at(1).as_deref() == Some("key"))
            || (self.peek_keyword().as_deref() == Some("constraint")
                && self.peek_keyword_at(2).as_deref() == Some("foreign")
                && self.peek_keyword_at(3).as_deref() == Some("key"))
    }

    /// Parse one table-level `[CONSTRAINT name] FOREIGN KEY ( col [, col]* ) references_clause`
    /// (the cursor is verified by `at_foreign_key_table_constraint`). The local-column list
    /// reuses the PRIMARY KEY list shape (spec/design/grammar.md §43).
    fn parse_foreign_key_table_constraint(&mut self) -> Result<ForeignKeyDef> {
        let name = if self.peek_keyword().as_deref() == Some("constraint") {
            self.advance();
            Some(self.expect_identifier()?)
        } else {
            None
        };
        self.expect_keyword("foreign")?;
        self.expect_keyword("key")?;
        let columns = self.parse_pk_column_list()?;
        let (ref_table, ref_columns, on_delete, on_update) = self.parse_references_clause()?;
        Ok(ForeignKeyDef {
            name,
            columns,
            ref_table,
            ref_columns,
            on_delete,
            on_update,
        })
    }

    /// Parse a `references_clause` from the `REFERENCES` keyword onward (shared by the
    /// column-level and table-level forms — spec/design/grammar.md §43): the referenced table,
    /// an optional referenced-column list (`None` defaults to the parent's primary key), and the
    /// `ON DELETE` / `ON UPDATE` actions (each at most once, either order; a repeat is 42601).
    fn parse_references_clause(
        &mut self,
    ) -> Result<(String, Option<Vec<String>>, RefAction, RefAction)> {
        self.expect_keyword("references")?;
        let ref_table = self.expect_identifier()?;
        let ref_columns = if matches!(self.peek(), Token::LParen) {
            Some(self.parse_pk_column_list()?)
        } else {
            None
        };
        let mut on_delete = RefAction::NoAction;
        let mut on_update = RefAction::NoAction;
        let mut seen_delete = false;
        let mut seen_update = false;
        while self.peek_keyword().as_deref() == Some("on") {
            self.advance();
            match self.peek_keyword().as_deref() {
                Some("delete") => {
                    self.advance();
                    if seen_delete {
                        return Err(syntax("ON DELETE specified more than once"));
                    }
                    seen_delete = true;
                    on_delete = self.parse_referential_action()?;
                }
                Some("update") => {
                    self.advance();
                    if seen_update {
                        return Err(syntax("ON UPDATE specified more than once"));
                    }
                    seen_update = true;
                    on_update = self.parse_referential_action()?;
                }
                _ => return Err(syntax("expected DELETE or UPDATE after ON")),
            }
        }
        Ok((ref_table, ref_columns, on_delete, on_update))
    }

    /// Parse one `referential_action` (spec/design/grammar.md §43). All five PG actions parse;
    /// CASCADE / SET NULL / SET DEFAULT are rejected later at CREATE TABLE (0A000).
    fn parse_referential_action(&mut self) -> Result<RefAction> {
        match self.peek_keyword().as_deref() {
            Some("no") => {
                self.advance();
                self.expect_keyword("action")?;
                Ok(RefAction::NoAction)
            }
            Some("restrict") => {
                self.advance();
                Ok(RefAction::Restrict)
            }
            Some("cascade") => {
                self.advance();
                Ok(RefAction::Cascade)
            }
            Some("set") => {
                self.advance();
                match self.peek_keyword().as_deref() {
                    Some("null") => {
                        self.advance();
                        Ok(RefAction::SetNull)
                    }
                    Some("default") => {
                        self.advance();
                        Ok(RefAction::SetDefault)
                    }
                    _ => Err(syntax("expected NULL or DEFAULT after SET")),
                }
            }
            _ => Err(syntax(
                "expected a referential action: NO ACTION / RESTRICT / CASCADE / SET NULL / SET DEFAULT",
            )),
        }
    }

    /// Whether the cursor sits on a `CHECK` constraint: the keyword `CHECK` followed by `(`,
    /// or `CONSTRAINT <ident> CHECK (` (spec/design/grammar.md §29). The keywords stay
    /// non-reserved — a column named `check`/`constraint` is followed by a type name (an
    /// identifier, never `(`), so the lookahead loses nothing.
    fn at_check_constraint(&self) -> bool {
        (self.peek_keyword().as_deref() == Some("check")
            && matches!(self.peek_at(1), Token::LParen))
            || (self.peek_keyword().as_deref() == Some("constraint")
                && self.peek_keyword_at(2).as_deref() == Some("check")
                && matches!(self.peek_at(3), Token::LParen))
    }

    /// Parse one `[CONSTRAINT name] CHECK ( expr )` (the cursor is verified by
    /// `at_check_constraint`). The token span between the parentheses is re-rendered as the
    /// constraint's persisted text (spec/fileformat/format.md "Check-expression text").
    fn parse_check_constraint(&mut self) -> Result<CheckDef> {
        let name = if self.peek_keyword().as_deref() == Some("constraint") {
            self.advance();
            Some(self.expect_identifier()?)
        } else {
            None
        };
        self.expect_keyword("check")?;
        self.expect(&Token::LParen)?;
        let start = self.pos;
        let expr = self.parse_expr()?;
        let text = render_tokens(&self.tokens[start..self.pos]);
        self.expect(&Token::RParen)?;
        Ok(CheckDef { name, expr, text })
    }

    /// Whether the cursor sits on a table-level `UNIQUE` constraint: the keyword `UNIQUE`
    /// followed by `(`, or `CONSTRAINT <ident> UNIQUE` (spec/design/grammar.md §31). The
    /// keywords stay non-reserved — a column named `unique` is followed by a type name (an
    /// identifier, never `(`), so the lookahead loses nothing.
    fn at_unique_table_constraint(&self) -> bool {
        (self.peek_keyword().as_deref() == Some("unique")
            && matches!(self.peek_at(1), Token::LParen))
            || (self.peek_keyword().as_deref() == Some("constraint")
                && self.peek_keyword_at(2).as_deref() == Some("unique"))
    }

    /// Parse one table-level `[CONSTRAINT name] UNIQUE ( col [, col]* )` (the cursor is
    /// verified by `at_unique_table_constraint`). The member list reuses the PRIMARY KEY
    /// list shape (spec/design/grammar.md §31).
    fn parse_unique_table_constraint(&mut self) -> Result<UniqueDef> {
        let name = if self.peek_keyword().as_deref() == Some("constraint") {
            self.advance();
            Some(self.expect_identifier()?)
        } else {
            None
        };
        self.expect_keyword("unique")?;
        let columns = self.parse_pk_column_list()?;
        Ok(UniqueDef { name, columns })
    }

    /// The parenthesized member list of a table-level `PRIMARY KEY` constraint:
    /// `( <col> [, <col>]* )`. Must be non-empty — `PRIMARY KEY ()` is 42601 (the first
    /// `expect_identifier` rejects `)`).
    fn parse_pk_column_list(&mut self) -> Result<Vec<String>> {
        self.expect(&Token::LParen)?;
        let mut cols = vec![self.expect_identifier()?];
        loop {
            match self.advance() {
                Token::Comma => cols.push(self.expect_identifier()?),
                Token::RParen => break,
                other => return Err(syntax(format!("expected ',' or ')', found {other:?}"))),
            }
        }
        Ok(cols)
    }

    fn parse_column_def(
        &mut self,
        checks: &mut Vec<CheckDef>,
        uniques: &mut Vec<UniqueDef>,
        foreign_keys: &mut Vec<ForeignKeyDef>,
    ) -> Result<ColumnDef> {
        let name = self.expect_identifier()?;
        let base_type = self.expect_identifier()?;
        let type_mod = self.parse_type_mod()?;
        let is_array = self.consume_array_brackets()?;
        let type_name = if is_array {
            format!("{base_type}[]")
        } else {
            base_type
        };
        // Zero or more order-free column constraints: `PRIMARY KEY`, `NOT NULL`,
        // `DEFAULT <literal>`, `[CONSTRAINT name] CHECK ( expr )`, and
        // `[CONSTRAINT name] UNIQUE`. A boolean constraint may be repeated harmlessly; a
        // repeated `DEFAULT` just keeps the last (the catalog stores one default); each
        // `CHECK` is a distinct constraint, collected into the statement-wide list in
        // textual order (a column-level check is semantically identical to a table-level
        // one — spec/design/constraints.md §4). A column-level `UNIQUE` collects the same
        // way as the one-member form (a repeat folds at execution —
        // spec/design/constraints.md §5).
        let mut primary_key = false;
        let mut not_null = false;
        let mut default = None;
        loop {
            if self.at_check_constraint() {
                checks.push(self.parse_check_constraint()?);
                continue;
            }
            // `CONSTRAINT <name> UNIQUE` in column position (the named one-member form;
            // `CONSTRAINT <name> CHECK (` was caught above).
            if self.peek_keyword().as_deref() == Some("constraint")
                && self.peek_keyword_at(2).as_deref() == Some("unique")
            {
                self.advance();
                let cname = self.expect_identifier()?;
                self.expect_keyword("unique")?;
                uniques.push(UniqueDef {
                    name: Some(cname),
                    columns: vec![name.clone()],
                });
                continue;
            }
            // `CONSTRAINT <name> REFERENCES …` in column position (the named one-member FK).
            if self.peek_keyword().as_deref() == Some("constraint")
                && self.peek_keyword_at(2).as_deref() == Some("references")
            {
                self.advance();
                let cname = self.expect_identifier()?;
                let (ref_table, ref_columns, on_delete, on_update) =
                    self.parse_references_clause()?;
                foreign_keys.push(ForeignKeyDef {
                    name: Some(cname),
                    columns: vec![name.clone()],
                    ref_table,
                    ref_columns,
                    on_delete,
                    on_update,
                });
                continue;
            }
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
                    // A `DEFAULT` takes any scalar expression (constraints.md §2). Capture the
                    // re-rendered token span as the persisted text (format.md
                    // "Check-expression text"), as a `CHECK` does — the executor classifies a
                    // bare literal (constant fast-path) vs an expression (text-persisted).
                    let start = self.pos;
                    let expr = self.parse_expr()?;
                    let text = render_tokens(&self.tokens[start..self.pos]);
                    default = Some(DefaultDef { expr, text });
                }
                Some("unique") => {
                    self.advance();
                    uniques.push(UniqueDef {
                        name: None,
                        columns: vec![name.clone()],
                    });
                }
                Some("references") => {
                    // The column-level one-member FK: `REFERENCES parent [(col)] [actions]`.
                    // `parse_references_clause` consumes the `REFERENCES` keyword itself.
                    let (ref_table, ref_columns, on_delete, on_update) =
                        self.parse_references_clause()?;
                    foreign_keys.push(ForeignKeyDef {
                        name: None,
                        columns: vec![name.clone()],
                        ref_table,
                        ref_columns,
                        on_delete,
                        on_update,
                    });
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
    /// Consume a trailing array type suffix `[]` (spec/design/array.md §1) after a type name (and
    /// its optional typmod). Returns whether the type is an array. Multiple `[][]` collapse to one
    /// array level — multidimensionality is a value property, not array-of-array (§2), so the type
    /// is dimension-agnostic. Only the empty-bracket form `[]` is accepted this slice (a size like
    /// `[3]` is deferred).
    fn consume_array_brackets(&mut self) -> Result<bool> {
        let mut is_array = false;
        while matches!(self.peek(), Token::LBracket) {
            self.advance(); // [
            self.expect(&Token::RBracket)?;
            is_array = true;
        }
        Ok(is_array)
    }

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

    /// `CREATE [UNIQUE] INDEX [name] ON <table> ( col [, col]* )` (spec/design/grammar.md
    /// §30). The optional name needs one disambiguation because no word is reserved: the
    /// word after INDEX is the index name UNLESS it is `ON` followed by a word and then `(`
    /// — that exact three-token shape can only be the unnamed form's `ON table (`. Key
    /// columns are bare identifiers (no expression / ordered / partial keys this slice — a
    /// `(`/`ASC`/`DESC` after a key is the natural 42601).
    fn parse_create_index(&mut self) -> Result<CreateIndex> {
        self.expect_keyword("create")?;
        let unique = self.peek_keyword().as_deref() == Some("unique");
        if unique {
            self.advance();
        }
        self.expect_keyword("index")?;
        let unnamed = self.peek_keyword().as_deref() == Some("on")
            && matches!(self.peek_at(1), Token::Word(_))
            && matches!(self.peek_at(2), Token::LParen);
        let name = if unnamed {
            None
        } else {
            Some(self.expect_identifier()?)
        };
        self.expect_keyword("on")?;
        let table = self.expect_identifier()?;
        self.expect(&Token::LParen)?;
        let mut columns = Vec::new();
        loop {
            columns.push(self.expect_identifier()?);
            match self.advance() {
                Token::Comma => continue,
                Token::RParen => break,
                other => return Err(syntax(format!("expected ',' or ')', found {other:?}"))),
            }
        }
        Ok(CreateIndex {
            name,
            table,
            columns,
            unique,
        })
    }

    /// `DROP INDEX <name>` (spec/design/grammar.md §30). A missing index (42704) or a
    /// table's name (42809) is rejected at execution time, not here.
    fn parse_drop_index(&mut self) -> Result<DropIndex> {
        self.expect_keyword("drop")?;
        self.expect_keyword("index")?;
        let name = self.expect_identifier()?;
        Ok(DropIndex { name })
    }

    /// `CREATE TYPE <name> AS ( <field> <type> [NOT NULL] [, …] )` — a composite (row) type
    /// (spec/design/composite.md, grammar.md). At least one field (an empty list is a syntax
    /// error); each field's type is a bare type name (built-in or a composite), optionally with a
    /// trailing `[]` for an array-typed field (spec/design/array.md §12), resolved at execution
    /// (42704 if unknown).
    fn parse_create_type(&mut self) -> Result<CreateType> {
        self.expect_keyword("create")?;
        self.expect_keyword("type")?;
        let name = self.expect_identifier()?;
        self.expect_keyword("as")?;
        self.expect(&Token::LParen)?;
        let mut fields = Vec::new();
        loop {
            let fname = self.expect_identifier()?;
            let base_type = self.expect_identifier()?;
            let type_mod = self.parse_type_mod()?;
            // An array-typed field (`xs int32[]`) — the same `[]` suffix a column type takes
            // (spec/design/array.md §1); the canonical spelling carries the brackets.
            let type_name = if self.consume_array_brackets()? {
                format!("{base_type}[]")
            } else {
                base_type
            };
            let mut not_null = false;
            if self.peek_keyword().as_deref() == Some("not") {
                self.advance();
                self.expect_keyword("null")?;
                not_null = true;
            }
            fields.push(TypeFieldDef {
                name: fname,
                type_name,
                type_mod,
                not_null,
            });
            match self.advance() {
                Token::Comma => continue,
                Token::RParen => break,
                other => return Err(syntax(format!("expected ',' or ')', found {other:?}"))),
            }
        }
        Ok(CreateType { name, fields })
    }

    /// `DROP TYPE [IF EXISTS] <name> [RESTRICT | CASCADE]` (spec/design/composite.md §7).
    /// `RESTRICT` is the default and the only behavior this slice; `CASCADE` is rejected
    /// (0A000) at execution. A missing type (42704) and dependents (2BP01) are execution-time.
    fn parse_drop_type(&mut self) -> Result<DropType> {
        self.expect_keyword("drop")?;
        self.expect_keyword("type")?;
        let if_exists = self.peek_keyword().as_deref() == Some("if");
        if if_exists {
            self.advance();
            self.expect_keyword("exists")?;
        }
        let name = self.expect_identifier()?;
        // Optional trailing RESTRICT / CASCADE (a keyword, consumed here; CASCADE is 0A000 at exec).
        let cascade = match self.peek_keyword().as_deref() {
            Some("restrict") => {
                self.advance();
                false
            }
            Some("cascade") => {
                self.advance();
                true
            }
            _ => false,
        };
        if cascade {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "DROP TYPE ... CASCADE is not supported".to_string(),
            ));
        }
        Ok(DropType { name, if_exists })
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
        let returning = self.parse_returning()?;
        Ok(Insert {
            table,
            columns,
            source,
            returning,
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

    /// One INSERT value slot: the `DEFAULT` keyword (not reserved — §3), a `ROW(…)` composite
    /// constructor (spec/design/composite.md §1), a bind parameter (`$N`, bound at execute —
    /// spec/design/api.md §5), else a literal.
    fn parse_insert_value(&mut self) -> Result<InsertValue> {
        if self.peek_keyword().as_deref() == Some("default") {
            self.advance();
            Ok(InsertValue::Default)
        } else if self.peek_keyword().as_deref() == Some("row")
            && matches!(self.peek_at(1), Token::LParen)
        {
            // ROW(field, field, …) — recurse on each field (a literal, a `$N`, or a nested ROW).
            self.advance(); // ROW
            self.expect(&Token::LParen)?;
            let mut fields = Vec::new();
            if !matches!(self.peek(), Token::RParen) {
                loop {
                    fields.push(self.parse_insert_value()?);
                    match self.advance() {
                        Token::Comma => continue,
                        Token::RParen => break,
                        other => {
                            return Err(syntax(format!("expected ',' or ')', found {other:?}")));
                        }
                    }
                }
            } else {
                self.advance(); // the empty ROW() — consume ')'
            }
            Ok(InsertValue::Row(fields))
        } else if self.peek_keyword().as_deref() == Some("array")
            && matches!(self.peek_at(1), Token::LBracket)
        {
            // ARRAY[elem, …] — recurse on each element (a literal or a `$N`).
            self.advance(); // ARRAY
            self.expect(&Token::LBracket)?;
            let mut elems = Vec::new();
            if !matches!(self.peek(), Token::RBracket) {
                loop {
                    elems.push(self.parse_insert_value()?);
                    match self.advance() {
                        Token::Comma => continue,
                        Token::RBracket => break,
                        other => {
                            return Err(syntax(format!("expected ',' or ']', found {other:?}")));
                        }
                    }
                }
            } else {
                self.advance(); // the empty ARRAY[] — consume ']'
            }
            Ok(InsertValue::Array(elems))
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
        Ok(match self.parse_query_expr_node()? {
            QueryExpr::Select(sel) => Statement::Select(*sel),
            QueryExpr::SetOp(so) => Statement::SetOp(*so),
        })
    }

    /// Parse a top-level `query_expr` as a `QueryExpr` node — a set expression plus an optional
    /// trailing `ORDER BY` / `LIMIT` / `OFFSET` folded onto it. The shared core of
    /// `parse_query_expr` (which wraps it in a `Statement`) and a `WITH` clause's main body. Unlike
    /// `parse_subquery` it opens no new nesting level — the body is at the statement top level.
    fn parse_query_expr_node(&mut self) -> Result<QueryExpr> {
        let node = self.parse_set_expr()?;
        let order_by = self.parse_order_by()?;
        let (limit, offset) = self.parse_limit_offset()?;
        Ok(match node {
            QueryExpr::Select(mut sel) => {
                sel.order_by = order_by;
                sel.limit = limit;
                sel.offset = offset;
                QueryExpr::Select(sel)
            }
            QueryExpr::SetOp(mut so) => {
                so.order_by = order_by;
                so.limit = limit;
                so.offset = offset;
                QueryExpr::SetOp(so)
            }
        })
    }

    /// `query_statement ::= with_clause? query_expr` — a top-level query prefixed by a `WITH`
    /// clause defining common table expressions (spec/design/cte.md). `WITH RECURSIVE` is deferred
    /// (0A000); the CTE bodies and the main body are WITH-less `query_expr`s (the top-level-only
    /// narrowing — a nested `WITH` surfaces as 42601 because a body must begin with `SELECT`).
    fn parse_with_statement(&mut self) -> Result<Statement> {
        self.expect_keyword("with")?;
        // `WITH RECURSIVE …` is deferred this slice. RECURSIVE in this position is the keyword (PG
        // reserves it), so a CTE may not be named `recursive` — a documented narrowing (cte.md §6).
        if self.peek_keyword().as_deref() == Some("recursive") {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "WITH RECURSIVE is not supported yet",
            ));
        }
        let mut ctes = Vec::new();
        loop {
            ctes.push(self.parse_cte()?);
            if matches!(self.peek(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        let body = self.parse_query_expr_node()?;
        Ok(Statement::With(WithQuery { ctes, body }))
    }

    /// `cte ::= identifier ("(" ident ("," ident)* ")")? "AS" ("NOT"? "MATERIALIZED")? "("
    /// query_expr ")"` (spec/design/cte.md). The optional column list renames the body's output
    /// columns; `[NOT] MATERIALIZED` is the explicit evaluation hint. The body reuses
    /// `parse_subquery` (one nesting level, trailing clauses allowed) between its parens.
    fn parse_cte(&mut self) -> Result<Cte> {
        let name = self.expect_identifier()?;
        let columns = if matches!(self.peek(), Token::LParen) {
            self.advance();
            let mut cols = vec![self.expect_identifier()?];
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                cols.push(self.expect_identifier()?);
            }
            self.expect(&Token::RParen)?;
            Some(cols)
        } else {
            None
        };
        self.expect_keyword("as")?;
        let materialized = match self.peek_keyword().as_deref() {
            Some("materialized") => {
                self.advance();
                Some(true)
            }
            Some("not") if self.peek_keyword_at(1).as_deref() == Some("materialized") => {
                self.advance();
                self.advance();
                Some(false)
            }
            _ => None,
        };
        self.expect(&Token::LParen)?;
        let query = self.parse_subquery()?;
        self.expect(&Token::RParen)?;
        Ok(Cte {
            name,
            columns,
            materialized,
            query,
        })
    }

    /// Parse a parenthesized subquery's inner `query_expr` (grammar.md §26): a full set-expression
    /// plus an optional trailing `ORDER BY` / `LIMIT` / `OFFSET` folded onto the node. Mirrors
    /// `parse_query_expr` but yields a `QueryExpr` (the subquery operand) rather than a `Statement`.
    /// The caller has already consumed the opening `(` and consumes the closing `)`.
    fn parse_subquery(&mut self) -> Result<QueryExpr> {
        // A nested scalar subquery / EXISTS / IN (SELECT …) is one query-nesting level deeper;
        // the guard also protects the parser's own stack against `(SELECT (SELECT … ))`.
        self.deepen()?;
        let node = self.parse_set_expr()?;
        let order_by = self.parse_order_by()?;
        let (limit, offset) = self.parse_limit_offset()?;
        let result = match node {
            QueryExpr::Select(mut sel) => {
                sel.order_by = order_by;
                sel.limit = limit;
                sel.offset = offset;
                QueryExpr::Select(sel)
            }
            QueryExpr::SetOp(mut so) => {
                so.order_by = order_by;
                so.limit = limit;
                so.offset = offset;
                QueryExpr::SetOp(so)
            }
        };
        self.undeepen();
        Ok(result)
    }

    /// `set_expr ::= intersect_expr (("UNION" | "EXCEPT") ("ALL"|"DISTINCT")? intersect_expr)*` —
    /// the lower-precedence, left-associative level. `INTERSECT` binds tighter (parsed inside
    /// `parse_intersect_expr`), so `a UNION b INTERSECT c` becomes `a UNION (b INTERSECT c)`.
    fn parse_set_expr(&mut self) -> Result<QueryExpr> {
        let base = self.depth;
        let mut left = self.parse_intersect_expr()?;
        loop {
            let op = match self.peek_keyword().as_deref() {
                Some("union") => SetOpKind::Union,
                Some("except") => SetOpKind::Except,
                _ => break,
            };
            self.deepen()?; // each chained UNION/EXCEPT is one more set-op nesting level
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
        self.depth = base;
        Ok(left)
    }

    /// `intersect_expr ::= select_core ("INTERSECT" ("ALL"|"DISTINCT")? select_core)*` — the
    /// higher-precedence, left-associative `INTERSECT` level.
    fn parse_intersect_expr(&mut self) -> Result<QueryExpr> {
        let base = self.depth;
        let mut left = QueryExpr::Select(Box::new(self.parse_select_core()?));
        while self.peek_keyword().as_deref() == Some("intersect") {
            self.deepen()?; // each chained INTERSECT is one more set-op nesting level
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
        self.depth = base;
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

    /// `select_core ::= "SELECT" "DISTINCT"? select_items ("FROM" from_clause)? where?
    /// group_by? having?` — a `SELECT` WITHOUT a trailing `ORDER BY`/`LIMIT`/`OFFSET` (the
    /// operand form of a set operation). The returned `Select` has empty `order_by` and no
    /// `limit`/`offset`. The FROM clause is optional: with no `from` keyword the SELECT is
    /// FROM-less — one virtual zero-column row (spec/design/grammar.md §34).
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
        let (from, joins) = if self.peek_keyword().as_deref() == Some("from") {
            self.advance(); // FROM
            let (f, j) = self.parse_from_clause()?;
            (Some(f), j)
        } else {
            (None, Vec::new())
        };

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

    /// `table_ref ::= derived_table derived_alias | (identifier | table_function) ("AS"?
    /// identifier)?` (grammar.md §15/§35/§42). A `(` at the START of a table_ref, when the next
    /// token is `SELECT`, begins a DERIVED TABLE — a parenthesized subquery used as a relation,
    /// `FROM (SELECT …) AS t` (§42); any other leading `(` is a 42601 this slice (no
    /// parenthesized-join FROM). Otherwise it is a base table name OR a set-returning function call
    /// (`generate_series(1, 5)`), a `(` immediately after the leading identifier marking the
    /// function form. The alias logic is shared: an explicit `AS` takes the next identifier
    /// unconditionally; an implicit alias is taken only when the next token is a word that is NOT
    /// a clause/join keyword (so `FROM t WHERE` and `FROM t JOIN ...` keep no alias). The
    /// stop-keyword set, and the leading-`SELECT` lookahead, are §8 cross-core surfaces.
    fn parse_table_ref(&mut self) -> Result<TableRef> {
        if matches!(self.peek(), Token::LParen) {
            return self.parse_derived_table();
        }
        let name = self.expect_identifier()?;
        // A `(` right after the name = a set-returning function call (no `*`/`DISTINCT` — those
        // are aggregate/star forms, not an SRF argument list).
        let args = if matches!(self.peek(), Token::LParen) {
            self.advance();
            let mut a = vec![self.parse_expr()?];
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                a.push(self.parse_expr()?);
            }
            self.expect(&Token::RParen)?;
            Some(a)
        } else {
            None
        };
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
        // The column-alias-list form `... AS g(n)` is a deferred narrowing (grammar.md §35): a
        // `(` after the alias is unambiguous (a base table never has one there) and rejected.
        if alias.is_some() && matches!(self.peek(), Token::LParen) {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "column alias list on a table function is not supported yet",
            ));
        }
        Ok(TableRef {
            name,
            alias,
            args,
            subquery: None,
            values: None,
            column_aliases: None,
        })
    }

    /// Parse a DERIVED TABLE — `"(" (query_expr | values_body) ")" derived_alias?` (grammar.md §42).
    /// The caller has verified the next token is `(`. A derived table is recognized when a `SELECT`
    /// (a `query_expr` body) OR `VALUES` (a VALUES-body relation) follows the `(` — the §26
    /// leading-`SELECT` lookahead, extended with `VALUES`, a §8 cross-core surface; any other
    /// leading `(` is a 42601 (no parenthesized-join FROM this slice). The alias is OPTIONAL
    /// (PostgreSQL 18 relaxed the old mandatory-alias rule): present, it is the relation's label and
    /// may carry a column-rename list `(c1, c2, …)`; absent, the relation has no qualifier (its bare
    /// columns still resolve and can be ambiguous). `alias`/`name` carry the alias (empty when none).
    fn parse_derived_table(&mut self) -> Result<TableRef> {
        // Consume the opening `(`. A leading `SELECT` is a query-expr body; a leading `VALUES` is a
        // VALUES-body relation; any other leading `(` is rejected (a parenthesized-join FROM
        // `(a JOIN b ON …)` is a deferred narrowing).
        self.advance();
        // The body is EITHER a query_expr (a leading `SELECT`) OR a VALUES list (a leading
        // `VALUES`) — `(VALUES (e…),(e…))`, a computed relation of literal rows (grammar.md §42).
        // The VALUES body's values are GENERAL expressions (resolved as constants at plan time,
        // parent = None) and it takes NO trailing ORDER BY / LIMIT (a deferred narrowing — that
        // surfaces as a 42601 leftover token at the expected `)`).
        let (subquery, values) = if self.peek_keyword().as_deref() == Some("values") {
            (None, Some(self.parse_values_body()?))
        } else if self.peek_keyword().as_deref() == Some("select") {
            (Some(Box::new(self.parse_subquery()?)), None)
        } else {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "subquery in FROM must begin with SELECT or VALUES (a parenthesized join is not supported)",
            ));
        };
        self.expect(&Token::RParen)?;
        // The alias is optional, parsed exactly like a base table's: an explicit `AS` takes the
        // next identifier; an implicit alias is a word that is not a clause/join stop keyword.
        let alias = if self.peek_keyword().as_deref() == Some("as") {
            self.advance();
            Some(self.expect_identifier()?)
        } else {
            match self.peek() {
                Token::Word(w) if !is_table_ref_stop_keyword(&w.to_ascii_lowercase()) => {
                    let a = w.clone();
                    self.advance();
                    Some(a)
                }
                _ => None,
            }
        };
        // Optional column-rename list `(c1, c2, …)` — only when a table alias was given (PG: a
        // column list with no preceding alias name, `(SELECT …) (a)`, is a syntax error: the bare
        // `(` falls through here and a later token check rejects it).
        let column_aliases = if alias.is_some() && matches!(self.peek(), Token::LParen) {
            self.advance();
            let mut cols = vec![self.expect_identifier()?];
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                cols.push(self.expect_identifier()?);
            }
            self.expect(&Token::RParen)?;
            Some(cols)
        } else {
            None
        };
        Ok(TableRef {
            name: alias.clone().unwrap_or_default(),
            alias,
            args: None,
            subquery,
            values,
            column_aliases,
        })
    }

    /// Parse a VALUES-body's rows — `VALUES "(" expr ("," expr)* ")" ("," …)*` (grammar.md §42),
    /// the body of a `FROM (VALUES …)` derived table. The caller has verified the next keyword is
    /// `VALUES` (here consumed). Each row is a parenthesized list of GENERAL expressions (unlike the
    /// `INSERT … VALUES` slot, which is a literal/`$N`/`DEFAULT`); arity equality across rows and
    /// per-column type unification are resolve-time concerns (the executor's `plan_values`). At
    /// least one row, each with at least one value (an empty `()` is a 42601). NO trailing
    /// ORDER BY / LIMIT is consumed — the caller's `)` follows the last row.
    fn parse_values_body(&mut self) -> Result<Vec<Vec<Expr>>> {
        self.expect_keyword("values")?;
        let mut rows = Vec::new();
        loop {
            self.expect(&Token::LParen)?;
            let mut row = vec![self.parse_expr()?];
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                row.push(self.parse_expr()?);
            }
            self.expect(&Token::RParen)?;
            rows.push(row);
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(rows)
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
        let returning = self.parse_returning()?;
        Ok(Update {
            table,
            assignments,
            filter,
            returning,
        })
    }

    /// `DELETE FROM <table> [WHERE <pred>]`. No WHERE deletes every row.
    fn parse_delete(&mut self) -> Result<Delete> {
        self.expect_keyword("delete")?;
        self.expect_keyword("from")?;
        let table = self.expect_identifier()?;
        let filter = self.parse_optional_where()?;
        let returning = self.parse_returning()?;
        Ok(Delete {
            table,
            filter,
            returning,
        })
    }

    /// Parse an optional terminal `RETURNING <select_items>` clause (shared by
    /// INSERT/UPDATE/DELETE — spec/design/grammar.md §32). `RETURNING` is not reserved (§3):
    /// it is a clause only in this trailing position (and it joins the table_ref
    /// implicit-alias stop set, so an `INSERT ... SELECT` source never swallows it — §15).
    /// The item list is the ordinary select-items production (`*` or expressions with
    /// optional `AS` labels); an empty list fails in `parse_expr` (42601).
    fn parse_returning(&mut self) -> Result<Option<SelectItems>> {
        if self.peek_keyword().as_deref() != Some("returning") {
            return Ok(None);
        }
        self.advance(); // RETURNING
        Ok(Some(self.parse_select_items()?))
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
        // A fresh sub-expression is one nesting level deeper (parens, ARRAY/ROW/CASE/function
        // operands, subscript indices all re-enter here). Bounds the recursive descent itself.
        self.deepen()?;
        let e = self.parse_or()?;
        self.undeepen();
        Ok(e)
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let base = self.depth;
        let mut lhs = self.parse_and()?;
        while self.peek_keyword().as_deref() == Some("or") {
            self.deepen()?; // each chained OR is one more AST level
            self.advance();
            let rhs = self.parse_and()?;
            lhs = binary(BinaryOp::Or, lhs, rhs);
        }
        self.depth = base;
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let base = self.depth;
        let mut lhs = self.parse_not()?;
        while self.peek_keyword().as_deref() == Some("and") {
            self.deepen()?; // each chained AND is one more AST level
            self.advance();
            let rhs = self.parse_not()?;
            lhs = binary(BinaryOp::And, lhs, rhs);
        }
        self.depth = base;
        Ok(lhs)
    }

    fn parse_not(&mut self) -> Result<Expr> {
        if self.peek_keyword().as_deref() == Some("not") {
            self.advance();
            // right-associative: NOT NOT x — each NOT is one more AST level (recursion here, so
            // the depth guard also protects the parser's own stack).
            self.deepen()?;
            let operand = self.parse_not()?;
            self.undeepen();
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
        let lhs = self.parse_concat()?;
        if self.peek_keyword().as_deref() == Some("is") {
            self.advance();
            let negated = if self.peek_keyword().as_deref() == Some("not") {
                self.advance();
                true
            } else {
                false
            };
            // IS [NOT] DISTINCT FROM <concat> — NULL-safe equality; else IS [NOT] NULL.
            if self.peek_keyword().as_deref() == Some("distinct") {
                self.advance();
                self.expect_keyword("from")?;
                let rhs = self.parse_concat()?;
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
            // `IN (SELECT ...)` is the uncorrelated IN-subquery (grammar.md §26), disambiguated by
            // a leading `SELECT`; otherwise a non-empty value list (`IN ()` is rejected:
            // parse_concat on `)` is 42601).
            if self.peek_keyword().as_deref() == Some("select") {
                let q = self.parse_subquery()?;
                self.expect(&Token::RParen)?;
                return Ok(Expr::InSubquery {
                    lhs: Box::new(lhs),
                    query: Box::new(q),
                    negated,
                });
            }
            let mut list = vec![self.parse_concat()?];
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                list.push(self.parse_concat()?);
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
            // Both bounds parse at the CONCAT level (one tighter than comparison), which never
            // consumes `AND` (a looser level owned by parse_and). So the BETWEEN's structural `AND`
            // is matched here and `x BETWEEN a AND b AND c` parses as `(x BETWEEN a AND b) AND c`
            // (grammar.md §21); a `||` bound (`x BETWEEN a || b AND c`) still works.
            let lo = self.parse_concat()?;
            self.expect_keyword("and")?;
            let hi = self.parse_concat()?;
            return Ok(Expr::Between {
                lhs: Box::new(lhs),
                lo: Box::new(lo),
                hi: Box::new(hi),
                negated,
            });
        }
        if self.peek_keyword().as_deref() == Some("like") {
            self.advance();
            let rhs = self.parse_concat()?;
            return Ok(Expr::Like {
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                negated,
            });
        }
        let op = match self.peek() {
            Token::Eq => Some(BinaryOp::Eq),
            Token::Ne => Some(BinaryOp::Ne),
            Token::Lt => Some(BinaryOp::Lt),
            Token::Gt => Some(BinaryOp::Gt),
            Token::Le => Some(BinaryOp::Le),
            Token::Ge => Some(BinaryOp::Ge),
            _ => None,
        };
        match op {
            Some(op) => {
                self.advance();
                // `op ANY/SOME/ALL ( array )` — a quantified array comparison (grammar.md §41):
                // a quantifier may stand in for the ordinary right operand. SOME folds to ANY.
                let quant = match self.peek_keyword().as_deref() {
                    Some("all") => Some(true),
                    Some("any") | Some("some") => Some(false),
                    _ => None,
                };
                if let Some(all) = quant {
                    self.advance(); // ANY / SOME / ALL
                    self.expect(&Token::LParen)?;
                    // A leading `SELECT` is the SUBQUERY form `op ANY/ALL(SELECT …)` — the subquery
                    // spelling of IN (array-functions.md §11.6), the §26 leading-`SELECT` lookahead;
                    // anything else is the array operand (§11.1).
                    if self.peek_keyword().as_deref() == Some("select") {
                        let query = self.parse_subquery()?;
                        self.expect(&Token::RParen)?;
                        return Ok(Expr::QuantifiedSubquery {
                            op,
                            all,
                            lhs: Box::new(lhs),
                            query: Box::new(query),
                        });
                    }
                    let array = self.parse_expr()?; // a full expression resolving to an array
                    self.expect(&Token::RParen)?;
                    return Ok(Expr::Quantified {
                        op,
                        all,
                        lhs: Box::new(lhs),
                        array: Box::new(array),
                    });
                }
                let rhs = self.parse_concat()?;
                Ok(binary(op, lhs, rhs))
            }
            None => Ok(lhs),
        }
    }

    /// The "any other operator" level (grammar.md §39/§40, array-functions.md §8/§10): one rung
    /// tighter than the comparisons, looser than additive, left-associative. It hosts `||` array
    /// concatenation plus the `@>`/`<@`/`&&` array containment/overlap operators — all the same
    /// precedence in PostgreSQL. Each operand is an additive expression, so `a + b || c` is
    /// `(a + b) || c`; chaining mixes freely (`a || b @> c` is `(a || b) @> c`).
    fn parse_concat(&mut self) -> Result<Expr> {
        let base = self.depth;
        let mut lhs = self.parse_additive()?;
        loop {
            let op = match self.peek() {
                Token::Concat => BinaryOp::Concat,
                Token::Contains => BinaryOp::Contains,
                Token::ContainedBy => BinaryOp::ContainedBy,
                Token::Overlaps => BinaryOp::Overlaps,
                _ => break,
            };
            self.deepen()?; // each chained operator is one more AST level
            self.advance();
            let rhs = self.parse_additive()?;
            lhs = binary(op, lhs, rhs);
        }
        self.depth = base;
        Ok(lhs)
    }

    fn parse_additive(&mut self) -> Result<Expr> {
        let base = self.depth;
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Token::Plus => BinaryOp::Add,
                Token::Minus => BinaryOp::Sub,
                _ => break,
            };
            self.deepen()?; // each chained +/- is one more AST level (the `1+1+…` vector)
            self.advance();
            let rhs = self.parse_multiplicative()?;
            lhs = binary(op, lhs, rhs);
        }
        self.depth = base;
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr> {
        let base = self.depth;
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Token::Star => BinaryOp::Mul,
                Token::Slash => BinaryOp::Div,
                Token::Percent => BinaryOp::Mod,
                _ => break,
            };
            self.deepen()?; // each chained * / % is one more AST level
            self.advance();
            let rhs = self.parse_unary()?;
            lhs = binary(op, lhs, rhs);
        }
        self.depth = base;
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if matches!(self.peek(), Token::Minus) {
            self.advance();
            // Fold unary-minus-of-an-integer-literal into one negative literal: this
            // makes int64::MIN representable (`-(2^63)`) and lets the negative value
            // range-check against its context like any literal (spec/design/types.md §6).
            // SUPPRESSED when a `::` immediately follows the literal: `::` binds tighter than
            // unary minus (PostgreSQL), so `-N::T` is `-(N::T)` — the cast applies to the
            // unsigned magnitude first (grammar.md §37). A one-token lookahead on the token
            // AFTER the literal, a §8 cross-core determinism surface.
            if let Token::Int(m) = self.peek()
                && self.tokens.get(self.pos + 1) != Some(&Token::DoubleColon)
            {
                let m = *m;
                self.advance();
                let folded = -(m as i128); // m <= 2^63 ⇒ folded ∈ [-2^63, 0] ⊆ i64
                return Ok(Expr::Literal(Literal::Int(folded as i64)));
            }
            // Fold unary-minus of a decimal literal into one negative decimal literal (like
            // the integer fold). Decimal negation never overflows. Same `::` suppression.
            if matches!(self.peek(), Token::Decimal(..))
                && self.tokens.get(self.pos + 1) != Some(&Token::DoubleColon)
            {
                if let Token::Decimal(digits, scale) = self.advance() {
                    return Ok(Expr::Literal(Literal::Decimal(Decimal::from_digits_scale(
                        true, &digits, scale,
                    ))));
                }
            }
            // each chained unary `-` is one more AST level (recursion here, so the depth guard
            // also protects the parser's own stack against `- - - … x`).
            self.deepen()?;
            let operand = self.parse_unary()?;
            self.undeepen();
            return Ok(Expr::Unary {
                op: UnaryOp::Neg,
                operand: Box::new(operand),
            });
        }
        self.parse_postfix()
    }

    /// A primary optionally followed by one or more postfix operators, applied left-to-right in
    /// token order: a `::type` PostgreSQL typecast (grammar.md §37) or a `.field` / `.*` composite
    /// field selection (spec/design/composite.md §S4). `expr :: type` desugars to
    /// `CAST(expr AS type)` here at parse time — one resolver / evaluator / cost path for both
    /// spellings — and casts chain left-associatively (`x::int8::int2` = `(x::int8)::int2`). A
    /// typmod rides on the type name exactly as in `CAST` (`x::numeric(10,2)`).
    ///
    /// Field selection follows PostgreSQL's **parens-required** rule: `.field` / `.*` applies ONLY
    /// to a **parenthesized** base — `(home).zip`, `(t.home).zip`, `(ROW(1,2)).f1` — and chains on a
    /// prior field access (`(c).a.b`). A bare `home.zip` / `t.home.zip` is a (multi-part) column
    /// reference, never field access (PG raises `42P01` for the unparenthesized form). So `.field`
    /// fires only when the primary started with `(` or after a previous `.field`; otherwise the `.`
    /// is left for the caller (a trailing `.field` on a bare name is then a syntax error, like PG).
    fn parse_postfix(&mut self) -> Result<Expr> {
        // Only a PARENTHESIZED primary is field-accessible (PG requires `(expr).field`). A
        // subsequent `.field` keeps the chain field-accessible (`(c).a.b`); a `::` cast does not.
        let base = self.depth;
        let mut field_accessible = matches!(self.peek(), Token::LParen);
        let mut expr = self.parse_primary()?;
        loop {
            // each postfix `::`/`[…]`/`.field` wraps the base in one more AST level; deepen only
            // when a postfix actually follows (not on the terminating non-postfix token).
            let is_postfix = match self.peek() {
                Token::DoubleColon | Token::LBracket => true,
                Token::Dot => field_accessible,
                _ => false,
            };
            if !is_postfix {
                break;
            }
            self.deepen()?;
            match self.peek() {
                Token::DoubleColon => {
                    self.advance();
                    let base_type = self.expect_identifier()?;
                    let type_mod = self.parse_type_mod()?;
                    let is_array = self.consume_array_brackets()?;
                    let type_name = if is_array {
                        format!("{base_type}[]")
                    } else {
                        base_type
                    };
                    expr = Expr::Cast {
                        inner: Box::new(expr),
                        type_name,
                        type_mod,
                    };
                    field_accessible = false;
                }
                // `base[..][..]` — array subscript (spec/design/array.md §6). Applies to ANY base
                // (no parens rule, unlike `.field`). Consecutive `[…]` brackets collect into ONE
                // access (so `a[1][2]` is a single multidim element read, not nested subscripting).
                // Each spec is an index `[i]` or a slice `[m:n]` (bounds optionally omitted). After a
                // subscript a `.field` still needs parens (PG), so it is not field-accessible.
                Token::LBracket => {
                    let mut subscripts = Vec::new();
                    while matches!(self.peek(), Token::LBracket) {
                        self.advance(); // [
                        // The lower bound / index is absent only before a `:` or `]` (`[:n]`, `[]`).
                        let lower = if matches!(self.peek(), Token::Colon | Token::RBracket) {
                            None
                        } else {
                            Some(self.parse_expr()?)
                        };
                        if matches!(self.peek(), Token::Colon) {
                            self.advance(); // :
                            let upper = if matches!(self.peek(), Token::RBracket) {
                                None
                            } else {
                                Some(self.parse_expr()?)
                            };
                            self.expect(&Token::RBracket)?;
                            subscripts.push(SubscriptSpec::Slice(lower, upper));
                        } else {
                            // Index form: a bare `[]` (no index, no colon) is a syntax error.
                            let idx = lower.ok_or_else(|| {
                                syntax("array subscript requires an index".to_string())
                            })?;
                            self.expect(&Token::RBracket)?;
                            subscripts.push(SubscriptSpec::Index(idx));
                        }
                    }
                    expr = Expr::Subscript {
                        base: Box::new(expr),
                        subscripts,
                    };
                    field_accessible = false;
                }
                // `.field` / `.*` — composite field selection (spec/design/composite.md §S4),
                // parens-required: only on a parenthesized / chained-field base.
                Token::Dot if field_accessible => {
                    self.advance();
                    if matches!(self.peek(), Token::Star) {
                        self.advance();
                        expr = Expr::FieldStar {
                            base: Box::new(expr),
                        };
                        field_accessible = false; // `.*` is terminal
                    } else {
                        let field = self.expect_identifier()?;
                        expr = Expr::FieldAccess {
                            base: Box::new(expr),
                            field,
                        };
                        // a field value may itself be composite → `(c).a.b` chains
                    }
                }
                _ => break,
            }
        }
        self.depth = base;
        Ok(expr)
    }

    /// A primary: a parenthesized expression, `CAST(...)`, a literal (integer,
    /// `TRUE`/`FALSE`, `NULL`), or a column reference.
    fn parse_primary(&mut self) -> Result<Expr> {
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            // `(SELECT ...)` is a scalar subquery (grammar.md §26), disambiguated by a leading
            // `SELECT` after the `(`; otherwise this is a parenthesized expression.
            if self.peek_keyword().as_deref() == Some("select") {
                let q = self.parse_subquery()?;
                self.expect(&Token::RParen)?;
                return Ok(Expr::ScalarSubquery(Box::new(q)));
            }
            let e = self.parse_expr()?;
            self.expect(&Token::RParen)?;
            return Ok(e);
        }
        // `EXISTS ( SELECT ... )` — the existence predicate (grammar.md §26). Recognized only when
        // an open-paren + `SELECT` follows, so `exists` stays usable as a column / function name.
        if self.peek_keyword().as_deref() == Some("exists")
            && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
            && matches!(self.tokens.get(self.pos + 2), Some(Token::Word(w)) if w.eq_ignore_ascii_case("select"))
        {
            self.advance(); // EXISTS
            self.expect(&Token::LParen)?;
            let q = self.parse_subquery()?;
            self.expect(&Token::RParen)?;
            return Ok(Expr::Exists(Box::new(q)));
        }
        if self.peek_keyword().as_deref() == Some("cast") {
            self.advance();
            self.expect(&Token::LParen)?;
            let inner = self.parse_expr()?;
            self.expect_keyword("as")?;
            let base_type = self.expect_identifier()?;
            let type_mod = self.parse_type_mod()?;
            let is_array = self.consume_array_brackets()?;
            let type_name = if is_array {
                format!("{base_type}[]")
            } else {
                base_type
            };
            self.expect(&Token::RParen)?;
            return Ok(Expr::Cast {
                inner: Box::new(inner),
                type_name,
                type_mod,
            });
        }
        // `ROW(e1, e2, …)` composite constructor (spec/design/composite.md §1). Recognized when
        // `ROW` is immediately followed by `(`, so `row` stays usable as a column / function name
        // otherwise. The bare `(a, b)` form is deferred (`0A000`); only the keyword form parses.
        if self.peek_keyword().as_deref() == Some("row") && matches!(self.peek_at(1), Token::LParen)
        {
            self.advance(); // ROW
            self.expect(&Token::LParen)?;
            let mut fields = Vec::new();
            if !matches!(self.peek(), Token::RParen) {
                loop {
                    fields.push(self.parse_expr()?);
                    match self.advance() {
                        Token::Comma => continue,
                        Token::RParen => break,
                        other => {
                            return Err(syntax(format!("expected ',' or ')', found {other:?}")));
                        }
                    }
                }
            } else {
                self.advance(); // the empty ROW() — consume ')'
            }
            return Ok(Expr::Row(fields));
        }
        // `ARRAY[e1, e2, …]` array constructor (spec/design/array.md §1). Recognized only when
        // `ARRAY` is immediately followed by `[`, so `array` stays usable as an identifier
        // otherwise. `ARRAY[]` is the empty array.
        if self.peek_keyword().as_deref() == Some("array")
            && matches!(self.peek_at(1), Token::LBracket)
        {
            self.advance(); // ARRAY
            self.expect(&Token::LBracket)?;
            let mut elems = Vec::new();
            if !matches!(self.peek(), Token::RBracket) {
                loop {
                    elems.push(self.parse_expr()?);
                    match self.advance() {
                        Token::Comma => continue,
                        Token::RBracket => break,
                        other => {
                            return Err(syntax(format!("expected ',' or ']', found {other:?}")));
                        }
                    }
                }
            } else {
                self.advance(); // the empty ARRAY[] — consume ']'
            }
            return Ok(Expr::Array(elems));
        }
        // A typed string literal `type '...'` (grammar.md §36) — PostgreSQL's `type 'string'`,
        // equal to `CAST('string' AS type)` over a string-literal operand: ANY type-naming word
        // immediately followed by a string (`INTERVAL '1 day'`, `TIMESTAMP '...'`, `INTEGER '42'`,
        // `NUMERIC '1.5'`, `BOOLEAN 'true'`, `BYTEA '\xDE'`, …). Recognized only when the next token
        // is a string — a one-token lookahead — so the word stays usable as a column / function
        // name otherwise (`SELECT interval FROM t`). `true`/`false`/`null` are excluded: they are
        // their own value literals (handled below), not type names. The type name is resolved (and
        // the string coerced to it) at resolve time; an unknown type is 42704 there.
        if let Token::Word(w) = self.peek()
            && matches!(self.tokens.get(self.pos + 1), Some(Token::Str(_)))
            && !matches!(w.to_ascii_lowercase().as_str(), "null" | "true" | "false")
        {
            let type_name = self.expect_identifier()?;
            if let Token::Str(text) = self.advance() {
                return Ok(Expr::TypedLiteral { type_name, text });
            }
            unreachable!("peeked a string literal after the type name");
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
            // `current_timestamp` — the SQL-standard bare keyword (no parens), reserved like the
            // value literals above. Pure sugar: desugar to a `now()` call so resolution / execution
            // / cost / volatility are entirely shared (spec/design/functions.md §12). Not fired when
            // followed by `(` (a precision typmod, deferred) so that form resolves normally (42883).
            Token::Word(w)
                if w.eq_ignore_ascii_case("current_timestamp")
                    && !matches!(self.tokens.get(self.pos + 1), Some(Token::LParen)) =>
            {
                self.advance();
                Ok(Expr::FuncCall {
                    name: "now".to_string(),
                    args: Vec::new(),
                    arg_names: None,
                    star: false,
                    variadic: false,
                })
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

    /// `function_call ::= identifier "(" ( "*" | function_arg ("," function_arg)* )? ")"` and
    /// `function_arg ::= ( identifier "=>" )? expr` — the shared aggregate/scalar call syntax
    /// (grammar.md §17). `COUNT(*)` is the `star` form; the argument list may be empty (a
    /// function whose parameters all DEFAULT, e.g. `make_interval()`); otherwise it is a
    /// comma-separated list of positional and/or NAMED (`name => value`) arguments. A positional
    /// argument may not follow a named one (42601). `arg_names` stays empty when every argument
    /// is positional (byte-identical to a pre-named call); resolution checks per-function arity,
    /// rejects named notation on a function with no parameter names, and fills defaults. The
    /// function name is resolved (case-insensitively) against the catalog later.
    fn parse_function_call(&mut self) -> Result<Expr> {
        let name = self.expect_identifier()?;
        self.expect(&Token::LParen)?;
        // DISTINCT inside a function call (COUNT(DISTINCT x)) is deferred — reject at parse.
        if self.peek_keyword().as_deref() == Some("distinct") {
            return Err(syntax("DISTINCT inside an aggregate is not supported yet"));
        }
        let mut args = Vec::new();
        let mut arg_names: Vec<Option<String>> = Vec::new();
        let mut any_named = false;
        let mut variadic = false;
        let star = if matches!(self.peek(), Token::Star) {
            self.advance();
            true
        } else if matches!(self.peek(), Token::RParen) {
            // Empty argument list (make_interval()) — leave args/arg_names empty.
            false
        } else {
            loop {
                // The final argument may be `VARIADIC expr` (grammar.md §17, array-functions.md
                // §12): the array is passed directly to a variadic parameter. VARIADIC is a plain
                // keyword (not reserved) recognized only at the start of an argument; once seen, no
                // further argument may follow (42601) and it does not combine with a name.
                if self.peek_keyword().as_deref() == Some("variadic") {
                    self.advance();
                    variadic = true;
                    args.push(self.parse_expr()?);
                    arg_names.push(None);
                    // A VARIADIC argument must be the last (PostgreSQL, 42601).
                    if matches!(self.peek(), Token::Comma) {
                        return Err(syntax("VARIADIC argument must be the last argument"));
                    }
                    break;
                }
                // A named argument is `identifier "=>" expr` (grammar.md §17); a two-token
                // lookahead (Word then `=>`) distinguishes it from a bare expr that starts with
                // an identifier (a column reference).
                let argname = if matches!(self.peek(), Token::Word(_))
                    && matches!(self.peek_at(1), Token::FatArrow)
                {
                    let nm = self.expect_identifier()?;
                    self.expect(&Token::FatArrow)?;
                    any_named = true;
                    Some(nm)
                } else if any_named {
                    // A positional argument may not follow a named one (PostgreSQL, 42601).
                    return Err(syntax("positional argument cannot follow named argument"));
                } else {
                    None
                };
                args.push(self.parse_expr()?);
                arg_names.push(argname);
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
            false
        };
        self.expect(&Token::RParen)?;
        // None unless a name appeared (the all-positional sentinel — §8 — keeping `Expr` small).
        let arg_names = if any_named {
            Some(Box::new(arg_names))
        } else {
            None
        };
        Ok(Expr::FuncCall {
            name,
            args,
            arg_names,
            star,
            variadic,
        })
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

    /// The token `offset` positions ahead of the cursor (`Eof` past the end). Used with
    /// `peek_keyword_at` for the CHECK-constraint lookahead (spec/design/grammar.md §29).
    pub fn peek_at(&self, offset: usize) -> &Token {
        self.tokens.get(self.pos + offset).unwrap_or(&Token::Eof)
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

/// Parse a bare expression — the catalog-load path for a persisted CHECK expression
/// (spec/design/constraints.md §4.5). The text was written by `render_tokens`, so it
/// re-lexes to a value-identical token sequence; the caller maps a failure to XX001
/// (the file claimed to be well-formed).
pub fn parse_expression(text: &str) -> Result<Expr> {
    let tokens = lex(text)?;
    let mut p = Parser::new(tokens);
    let expr = p.parse_expr()?;
    p.expect_eof()?;
    Ok(expr)
}

/// Re-render a token slice as the persisted check-expression text: each token rendered by
/// the closed table in spec/fileformat/format.md "Check-expression text", joined with
/// single spaces. A byte contract — identical across every core (CLAUDE.md §8).
pub fn render_tokens(tokens: &[Token]) -> String {
    let parts: Vec<String> = tokens.iter().map(render_token).collect();
    parts.join(" ")
}

fn render_token(t: &Token) -> String {
    match t {
        Token::Word(w) => w.clone(),
        Token::Int(m) => m.to_string(),
        // The digit string with `.` inserted `scale` digits from the right. The lexer
        // guarantees scale <= coeff.len() (every fractional digit is in the coefficient),
        // so the insertion point is in range; scale == len renders a leading-dot form
        // (".5") and scale == 0 a trailing-dot form ("1."), both of which re-lex as the
        // same decimal value (spec/fileformat/format.md "Check-expression text").
        Token::Decimal(coeff, scale) => {
            let split = coeff.len() - *scale as usize;
            format!("{}.{}", &coeff[..split], &coeff[split..])
        }
        Token::Str(s) => format!("'{}'", s.replace('\'', "''")),
        Token::Param(n) => format!("${n}"),
        Token::Comma => ",".into(),
        Token::Dot => ".".into(),
        Token::LParen => "(".into(),
        Token::RParen => ")".into(),
        Token::LBracket => "[".into(),
        Token::RBracket => "]".into(),
        Token::Star => "*".into(),
        Token::Plus => "+".into(),
        Token::Minus => "-".into(),
        Token::Slash => "/".into(),
        Token::Percent => "%".into(),
        Token::Eq => "=".into(),
        Token::Ne => "<>".into(),
        Token::Lt => "<".into(),
        Token::Gt => ">".into(),
        Token::Le => "<=".into(),
        Token::Ge => ">=".into(),
        Token::DoubleColon => "::".into(),
        Token::Colon => ":".into(),
        Token::FatArrow => "=>".into(),
        Token::Concat => "||".into(),
        Token::Contains => "@>".into(),
        Token::ContainedBy => "<@".into(),
        Token::Overlaps => "&&".into(),
        Token::Eof => String::new(),
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
            // RETURNING ends an INSERT ... SELECT source — it must not be swallowed as the
            // source's implicit table alias (`... SELECT v FROM t RETURNING v` is the INSERT's
            // clause). §32; PostgreSQL fully reserves the word.
            | "returning"
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
