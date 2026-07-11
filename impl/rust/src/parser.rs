//! Hand-written recursive-descent parser (CLAUDE.md §5, §10).
//!
//! Statement productions are filled in feature-by-feature (Phases B–E). Until a
//! production is implemented it returns a structured `0A000` feature-not-supported
//! error rather than panicking, so the harness reports "not yet" cleanly.

use crate::ast::{
    AlterSeqAction, AlterSequence, Assignment, BinaryOp, CheckDef, ColumnDef, ConflictAction,
    ConflictTarget, CreateIndex, CreateSequence, CreateTable, CreateType, Cte, CteBody, DefaultDef,
    Delete, DropIndex, DropSequence, DropTable, DropType, ExcludeDef, Expr, ForeignKeyDef,
    GroupItem, IdentitySpec, IndexKeyElem, IndexPredicate, Insert, InsertSource, InsertValue,
    JoinClause, JoinKind, JsonOnBehavior, JsonPredicateKind, JsonTable, JsonWrapper, JtColumn,
    Literal, OnConflict, OrderKey, Overriding, QueryExpr, RefAction, Select, SelectItem,
    SelectItems, SeqOptions, SetOp, SetOpKind, Statement, SubscriptSpec, TableRef, TypeFieldDef,
    TypeMod, UnaryOp, UniqueDef, Update, WindowDef, WithExpr, WithQuery,
};
use crate::ast::{FrameBound, FrameExclusion, FrameMode, WindowFrame, WindowOrderKey};
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
            // CREATE SEQUENCE — a 2-token lookahead keeps SEQUENCE non-reserved (sequences.md §44).
            Some("create") if self.peek_keyword_at(1).as_deref() == Some("sequence") => {
                Ok(Statement::CreateSequence(self.parse_create_sequence()?))
            }
            Some("create") => Ok(Statement::CreateTable(self.parse_create_table()?)),
            Some("drop") if self.peek_keyword_at(1).as_deref() == Some("index") => {
                Ok(Statement::DropIndex(self.parse_drop_index()?))
            }
            Some("drop") if self.peek_keyword_at(1).as_deref() == Some("type") => {
                Ok(Statement::DropType(self.parse_drop_type()?))
            }
            Some("drop") if self.peek_keyword_at(1).as_deref() == Some("sequence") => {
                Ok(Statement::DropSequence(self.parse_drop_sequence()?))
            }
            Some("drop") => Ok(Statement::DropTable(self.parse_drop_table()?)),
            // ALTER SEQUENCE — the only ALTER statement this slice (sequences.md §4). A 2-token
            // lookahead recognizes it; any other `ALTER …` (TABLE, SYSTEM, …) is not a statement
            // keyword jed knows and falls through to the generic unknown-keyword 42601 below
            // (the no-escape-hatch surface — resource/no_escape_hatch.test).
            Some("alter") if self.peek_keyword_at(1).as_deref() == Some("sequence") => {
                Ok(Statement::AlterSequence(self.parse_alter_sequence()?))
            }
            Some("insert") => Ok(Statement::Insert(self.parse_insert()?)),
            Some("select") => self.parse_query_expr(),
            // `WITH …` at statement start can only begin a query with common table expressions
            // (spec/design/cte.md). `with` is non-reserved but unambiguous here.
            Some("with") => self.parse_with_statement(),
            Some("update") => Ok(Statement::Update(self.parse_update()?)),
            Some("delete") => Ok(Statement::Delete(self.parse_delete()?)),
            Some("explain") => self.parse_explain(),
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

    /// `EXPLAIN [ANALYZE] <statement>` (spec/design/explain.md). EXPLAIN is a positional leading
    /// keyword — non-reserved, no lookahead — followed by an optional ANALYZE modifier and then a
    /// restricted inner statement (a query or DML). ANALYZE is consumed positionally: no inner
    /// statement begins with the word ANALYZE, so there is no ambiguity.
    fn parse_explain(&mut self) -> Result<Statement> {
        self.advance(); // EXPLAIN
        let analyze = self.peek_keyword().as_deref() == Some("analyze");
        if analyze {
            self.advance();
        }
        let inner = self.parse_explain_inner()?;
        Ok(Statement::Explain {
            analyze,
            inner: Box::new(inner),
        })
    }

    /// The statement EXPLAIN wraps — restricted to a query (`SELECT` / `WITH`) or a DML statement
    /// (`INSERT` / `UPDATE` / `DELETE`). DDL, transaction control, and a nested `EXPLAIN` have no
    /// query plan to render and are rejected `42601`.
    fn parse_explain_inner(&mut self) -> Result<Statement> {
        match self.peek_keyword().as_deref() {
            Some("select") => self.parse_query_expr(),
            Some("with") => self.parse_with_statement(),
            Some("insert") => Ok(Statement::Insert(self.parse_insert()?)),
            Some("update") => Ok(Statement::Update(self.parse_update()?)),
            Some("delete") => Ok(Statement::Delete(self.parse_delete()?)),
            Some(other) => Err(syntax(format!("EXPLAIN does not support '{other}'"))),
            None => Err(syntax("expected a statement after EXPLAIN")),
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
        // An optional table_scope between CREATE and TABLE makes the table TEMPORARY
        // (spec/design/temp-tables.md, grammar.ebnf `table_scope`). TEMP / TEMPORARY are NOT reserved
        // (§3): recognized positionally here — the word after TABLE is always the table name, so
        // `CREATE TABLE temp (...)` is an ordinary persistent table named "temp".
        let temp = matches!(
            self.peek_keyword().as_deref(),
            Some("temp") | Some("temporary")
        );
        if temp {
            self.advance();
        }
        self.expect_keyword("table")?;
        // An optional database qualifier `db.table` (attached-databases.md §3, Slice 1b): create the
        // table INTO the named database (`main` / `temp` / a host attachment). A bare name uses the
        // implicit scope. The `.` after the first identifier makes it the qualifier, the next the name.
        let (db, name) = self.parse_qualified_table_name()?;
        self.expect(&Token::LParen)?;

        let mut columns = Vec::new();
        let mut table_pks = Vec::new();
        let mut checks = Vec::new();
        let mut uniques = Vec::new();
        let mut foreign_keys = Vec::new();
        let mut excludes = Vec::new();
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
            } else if self.at_exclusion_table_constraint() {
                excludes.push(self.parse_exclusion_table_constraint()?);
            } else {
                columns.push(self.parse_column_def(
                    &name,
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
            temp,
            columns,
            table_pks,
            checks,
            uniques,
            foreign_keys,
            excludes,
            db,
        })
    }

    /// Whether the cursor sits on a table-level `EXCLUDE` constraint: the keyword `EXCLUDE`, or
    /// `CONSTRAINT <ident> EXCLUDE` (spec/design/gist.md §7). The keyword stays non-reserved — a
    /// column named `exclude` is followed by a type name (an identifier), and `EXCLUDE` here is
    /// followed by `USING` or `(` (never a type name), so the lookahead loses nothing.
    fn at_exclusion_table_constraint(&self) -> bool {
        (self.peek_keyword().as_deref() == Some("exclude")
            && (self.peek_keyword_at(1).as_deref() == Some("using")
                || matches!(self.peek_at(1), Token::LParen)))
            || (self.peek_keyword().as_deref() == Some("constraint")
                && self.peek_keyword_at(2).as_deref() == Some("exclude"))
    }

    /// Parse one `[CONSTRAINT name] EXCLUDE [USING method] ( col WITH op [, col2 WITH op2 ...] )`
    /// (the cursor is verified by `at_exclusion_table_constraint`). Each operand is a bare column
    /// name; the `WITH` operator is captured as its source text (`=` / `&&`) and mapped to a
    /// strategy at execution (spec/design/gist.md §7). The `USING` method (only `gist`) is captured
    /// verbatim and validated at execution.
    fn parse_exclusion_table_constraint(&mut self) -> Result<ExcludeDef> {
        let name = if self.peek_keyword().as_deref() == Some("constraint") {
            self.advance();
            Some(self.expect_identifier()?)
        } else {
            None
        };
        self.expect_keyword("exclude")?;
        let using = if self.peek_keyword().as_deref() == Some("using") {
            self.advance();
            Some(self.expect_identifier()?)
        } else {
            None
        };
        self.expect(&Token::LParen)?;
        let mut elements = Vec::new();
        loop {
            let col = self.expect_identifier()?;
            self.expect_keyword("with")?;
            // The operator is a single token (`=` / `&&`); render it to source text for execution.
            let start = self.pos;
            self.advance();
            let op = render_tokens(&self.tokens[start..self.pos]);
            elements.push((col, op));
            match self.advance() {
                Token::Comma => continue,
                Token::RParen => break,
                other => return Err(syntax(format!("expected ',' or ')', found {other:?}"))),
            }
        }
        Ok(ExcludeDef {
            name,
            using,
            elements,
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
        table_name: &str,
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
        let mut identity: Option<IdentitySpec> = None;
        let mut collation: Option<String> = None;
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
                // `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY [( seq_options )]`
                // (spec/design/sequences.md §13). Two identity specs on one column is 42601
                // ("multiple identity specifications"). The desugaring (owned sequence + nextval
                // default + NOT NULL + the type gate) is at execution.
                Some("generated") => {
                    self.advance();
                    let always = match self.peek_keyword().as_deref() {
                        Some("always") => {
                            self.advance();
                            true
                        }
                        Some("by") => {
                            self.advance();
                            self.expect_keyword("default")?;
                            false
                        }
                        other => {
                            return Err(syntax(format!(
                                "expected ALWAYS or BY DEFAULT after GENERATED, found {other:?}"
                            )));
                        }
                    };
                    self.expect_keyword("as")?;
                    self.expect_keyword("identity")?;
                    let options = if matches!(self.peek(), Token::LParen) {
                        self.parse_sequence_options(true)?
                    } else {
                        SeqOptions::default()
                    };
                    if identity.is_some() {
                        return Err(EngineError::new(
                            SqlState::SyntaxError,
                            format!(
                                "multiple identity specifications for column {name} of table {table_name}"
                            ),
                        ));
                    }
                    identity = Some(IdentitySpec { always, options });
                }
                // `COLLATE "name"` in column position (spec/design/collation.md §1) — a quoted,
                // case-sensitive collation name. Validity (text-only 42804, loaded name 42704) is
                // checked at execution against the catalog. A repeat keeps the last (like DEFAULT).
                Some("collate") => {
                    self.advance();
                    collation = Some(self.expect_collation_name()?);
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
            identity,
            collation,
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

    /// `DROP TABLE [IF EXISTS] <name> [, …] [CASCADE | RESTRICT]`. Removes the named
    /// tables. Existence/dependency are resolved at execution time (42P01 — or a no-op when
    /// `IF EXISTS` is present — and 2BP01), not here. A comma list collects several names;
    /// the trailing `CASCADE`/`RESTRICT` keyword sets the FK-dependency mode (`RESTRICT` is
    /// the default) (spec/design/grammar.md §13). `IF EXISTS` is recognized only when the
    /// next two keywords are exactly `IF EXISTS` (the two-token lookahead the statement
    /// dispatch uses) — a lone `if` is an ordinary non-reserved identifier, so `DROP TABLE if`
    /// drops a table named `if` (PG-faithful, §1).
    fn parse_drop_table(&mut self) -> Result<DropTable> {
        self.expect_keyword("drop")?;
        self.expect_keyword("table")?;
        let if_exists = self.peek_keyword().as_deref() == Some("if")
            && self.peek_keyword_at(1).as_deref() == Some("exists");
        if if_exists {
            self.advance(); // IF
            self.advance(); // EXISTS
        }
        let mut names = vec![self.expect_identifier()?];
        while matches!(self.peek(), Token::Comma) {
            self.advance();
            names.push(self.expect_identifier()?);
        }
        // The trailing dependency mode is optional; `RESTRICT` is the default (and the only
        // mode the bare form ever had). Anything else after the name list is trailing input
        // (the dispatch's end-of-statement check raises 42601).
        let cascade = match self.peek_keyword().as_deref() {
            Some("cascade") => {
                self.advance();
                true
            }
            Some("restrict") => {
                self.advance();
                false
            }
            _ => false,
        };
        Ok(DropTable {
            names,
            if_exists,
            cascade,
        })
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
        // The unnamed form is `INDEX ON <table> [USING <method>] (` — the word after INDEX is the
        // index NAME unless it is `ON` followed by a word and then `(` OR `USING` (the three-token
        // lookahead, extended for the optional USING clause — grammar.md §30, gin.md §3). A §8
        // determinism surface, byte-identical across the parsers.
        let unnamed = self.peek_keyword().as_deref() == Some("on")
            && matches!(self.peek_at(1), Token::Word(_))
            && (matches!(self.peek_at(2), Token::LParen)
                || matches!(self.peek_at(2), Token::Word(w) if w.eq_ignore_ascii_case("using")));
        let name = if unnamed {
            None
        } else {
            Some(self.expect_identifier()?)
        };
        self.expect_keyword("on")?;
        // An optional database qualifier `db.table` on the target table (attached-databases.md §3,
        // Slice 1b): build the index ON a table in the named database (`main` / `temp` / a host
        // attachment).
        let (db, table) = self.parse_qualified_table_name()?;
        // Optional `USING <method>` (PG order: between the table and the column list — gin.md §3,
        // grammar.md §30). Not reserved (recognized positionally); the method name is resolved —
        // and an unknown one rejected 42704 — at execution, not here.
        let using = if self.peek_keyword().as_deref() == Some("using") {
            self.advance();
            Some(self.expect_identifier()?)
        } else {
            None
        };
        self.expect(&Token::LParen)?;
        let mut keys = Vec::new();
        loop {
            keys.push(self.parse_index_element()?);
            match self.advance() {
                Token::Comma => continue,
                Token::RParen => break,
                other => return Err(syntax(format!("expected ',' or ')', found {other:?}"))),
            }
        }
        // An optional trailing `WHERE predicate` makes the index PARTIAL (indexes.md §9). `where`
        // is recognized positionally after the closing `)` (non-reserved — a column may be named
        // `where`); its text is captured for the canonical persisted form (like CHECK/DEFAULT).
        let predicate = if self.peek_keyword().as_deref() == Some("where") {
            self.advance();
            let start = self.pos;
            let expr = self.parse_expr()?;
            let text = render_tokens(&self.tokens[start..self.pos]);
            Some(IndexPredicate { text, expr })
        } else {
            None
        };
        Ok(CreateIndex {
            name,
            table,
            keys,
            unique,
            using,
            db,
            predicate,
        })
    }

    /// One `index_element` (grammar.md §30, indexes.md §1): a bare column, a bare function
    /// call (`lower(email)`), or a parenthesized expression (`(a + b)`). PostgreSQL's
    /// `index_elem`: a general operator expression must be parenthesized (a bare `a + b`
    /// errors — `parse_primary` stops before the operator, so the element loop then sees an
    /// unexpected token); a parenthesized bare column `(a)` normalizes to a column key.
    fn parse_index_element(&mut self) -> Result<IndexKeyElem> {
        if matches!(self.peek(), Token::LParen) {
            // `( expr )` — any parenthesized expression.
            self.advance();
            let start = self.pos;
            let expr = self.parse_expr()?;
            let end = self.pos;
            self.expect(&Token::RParen)?;
            Ok(Self::index_key_from_expr(&self.tokens, expr, start, end))
        } else if matches!(self.peek(), Token::Word(_)) && matches!(self.peek_at(1), Token::LParen)
        {
            // A bare function call `f(args)` — parse ONLY the primary, so a trailing operator
            // (`lower(x) + 1`) leaves `+` for the element loop to reject (PG requires parens).
            let start = self.pos;
            let expr = self.parse_primary()?;
            let end = self.pos;
            Ok(Self::index_key_from_expr(&self.tokens, expr, start, end))
        } else {
            // A bare column name.
            Ok(IndexKeyElem::Column(self.expect_identifier()?))
        }
    }

    /// Classify a parsed index-element expression: a bare column reference (`a`, `(a)`,
    /// `((a))`) becomes a column key (PG-matched), anything else an expression key carrying
    /// its canonical text (rendered from the captured token span, like CHECK/DEFAULT).
    fn index_key_from_expr(tokens: &[Token], expr: Expr, start: usize, end: usize) -> IndexKeyElem {
        if let Expr::Column(name) = &expr {
            return IndexKeyElem::Column(name.clone());
        }
        let text = render_tokens(&tokens[start..end]);
        IndexKeyElem::Expr { text, expr }
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
        let fields = self.parse_field_def_list()?;
        Ok(CreateType { name, fields })
    }

    /// Parse a `( field type [NOT NULL] [, …] )` field-definition list — the body shared by
    /// `CREATE TYPE … AS (…)` (composite.md) and a FROM-clause **column-definition list**
    /// `AS t(col type, …)` (C0, json-table.md §1). The caller has consumed the opening `(`; this
    /// consumes through the matching `)`. Each field is `name type [numeric(p,s)] [[]] [NOT NULL]`.
    /// Skip an optional `FORMAT JSON [ENCODING …]` clause after a SQL/JSON context item.
    fn skip_format_json(&mut self) {
        if self.peek_keyword().as_deref() == Some("format")
            && self.peek_keyword_at(1).as_deref() == Some("json")
        {
            self.advance(); // FORMAT
            self.advance(); // JSON
        }
    }

    /// Parse an optional `RETURNING <type> [FORMAT JSON]` clause → the type name (resolved later).
    fn parse_json_returning(&mut self) -> Result<Option<String>> {
        if self.peek_keyword().as_deref() != Some("returning") {
            return Ok(None);
        }
        self.advance(); // RETURNING
        let ty = self.expect_identifier()?;
        self.skip_format_json();
        Ok(Some(ty))
    }

    /// Parse one constant SQL/JSON behavior word (`ERROR` / `NULL` / `TRUE` / `FALSE` / `UNKNOWN` /
    /// `EMPTY [ARRAY|OBJECT]`). `DEFAULT expr` is the deferred S3 follow-on (0A000).
    fn parse_json_behavior(&mut self) -> Result<JsonOnBehavior> {
        match self.peek_keyword().as_deref() {
            Some("error") => {
                self.advance();
                Ok(JsonOnBehavior::Error)
            }
            Some("null") => {
                self.advance();
                Ok(JsonOnBehavior::Null)
            }
            Some("true") => {
                self.advance();
                Ok(JsonOnBehavior::True)
            }
            Some("false") => {
                self.advance();
                Ok(JsonOnBehavior::False)
            }
            Some("unknown") => {
                self.advance();
                Ok(JsonOnBehavior::Unknown)
            }
            Some("empty") => {
                self.advance();
                match self.peek_keyword().as_deref() {
                    Some("object") => {
                        self.advance();
                        Ok(JsonOnBehavior::EmptyObject)
                    }
                    Some("array") => {
                        self.advance();
                        Ok(JsonOnBehavior::EmptyArray)
                    }
                    // bare `EMPTY` defaults to `EMPTY ARRAY` (PostgreSQL).
                    _ => Ok(JsonOnBehavior::EmptyArray),
                }
            }
            Some("default") => Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "ON ERROR / ON EMPTY DEFAULT expr is not supported yet",
            )),
            _ => Err(syntax("expected a SQL/JSON ON ERROR/EMPTY behavior")),
        }
    }

    /// Parse JSON_EXISTS's single optional `<behavior> ON ERROR` clause.
    fn parse_json_on_error_only(&mut self) -> Result<Option<JsonOnBehavior>> {
        // A behavior word followed by `ON ERROR`.
        if self.is_json_behavior_start() && self.peek_on_clause_is("error") {
            let b = self.parse_json_behavior()?;
            self.advance(); // ON
            self.advance(); // ERROR
            Ok(Some(b))
        } else {
            Ok(None)
        }
    }

    /// Parse the optional `<behavior> ON EMPTY` then `<behavior> ON ERROR` clauses (in that order).
    fn parse_json_on_clauses(
        &mut self,
    ) -> Result<(Option<JsonOnBehavior>, Option<JsonOnBehavior>)> {
        let mut on_empty = None;
        let mut on_error = None;
        if self.is_json_behavior_start() && self.peek_on_clause_is("empty") {
            let b = self.parse_json_behavior()?;
            self.advance(); // ON
            self.advance(); // EMPTY
            on_empty = Some(b);
        }
        if self.is_json_behavior_start() && self.peek_on_clause_is("error") {
            let b = self.parse_json_behavior()?;
            self.advance(); // ON
            self.advance(); // ERROR
            on_error = Some(b);
        }
        Ok((on_empty, on_error))
    }

    /// Parse JSON_QUERY's optional `[WITH [COND|UNCOND] [ARRAY] WRAPPER | WITHOUT [ARRAY] WRAPPER]`
    /// and `[KEEP|OMIT QUOTES [ON SCALAR STRING]]` clauses.
    fn parse_json_wrapper_quotes(&mut self) -> Result<(JsonWrapper, bool)> {
        let mut wrapper = JsonWrapper::Without;
        match self.peek_keyword().as_deref() {
            Some("with") => {
                self.advance(); // WITH
                wrapper = match self.peek_keyword().as_deref() {
                    Some("conditional") => {
                        self.advance();
                        JsonWrapper::Conditional
                    }
                    Some("unconditional") => {
                        self.advance();
                        JsonWrapper::Unconditional
                    }
                    _ => JsonWrapper::Unconditional,
                };
                if self.peek_keyword().as_deref() == Some("array") {
                    self.advance();
                }
                self.expect_keyword("wrapper")?;
            }
            Some("without") => {
                self.advance(); // WITHOUT
                if self.peek_keyword().as_deref() == Some("array") {
                    self.advance();
                }
                self.expect_keyword("wrapper")?;
            }
            _ => {}
        }
        let mut keep_quotes = true;
        match self.peek_keyword().as_deref() {
            Some("keep") => {
                self.advance();
                self.expect_keyword("quotes")?;
                self.skip_on_scalar_string();
            }
            Some("omit") => {
                self.advance();
                self.expect_keyword("quotes")?;
                self.skip_on_scalar_string();
                keep_quotes = false;
            }
            _ => {}
        }
        Ok((wrapper, keep_quotes))
    }

    /// Skip an optional `ON SCALAR STRING` after a QUOTES clause.
    fn skip_on_scalar_string(&mut self) {
        if self.peek_keyword().as_deref() == Some("on")
            && self.peek_keyword_at(1).as_deref() == Some("scalar")
        {
            self.advance(); // ON
            self.advance(); // SCALAR
            if self.peek_keyword().as_deref() == Some("string") {
                self.advance();
            }
        }
    }

    /// True if the cursor is at a SQL/JSON behavior word (ERROR/NULL/TRUE/FALSE/UNKNOWN/EMPTY/DEFAULT).
    fn is_json_behavior_start(&self) -> bool {
        matches!(
            self.peek_keyword().as_deref(),
            Some("error" | "null" | "true" | "false" | "unknown" | "empty" | "default")
        )
    }

    /// True if the upcoming clause is `… ON <which>` (a one-or-two-token lookahead past the behavior).
    fn peek_on_clause_is(&self, which: &str) -> bool {
        // The behavior is 1 token (ERROR/NULL/…) or 2 (EMPTY ARRAY/OBJECT); scan to the `ON`.
        for skip in [1usize, 2] {
            if self.peek_keyword_at(skip).as_deref() == Some("on")
                && self.peek_keyword_at(skip + 1).as_deref() == Some(which)
            {
                return true;
            }
        }
        false
    }

    fn parse_field_def_list(&mut self) -> Result<Vec<TypeFieldDef>> {
        let mut fields = Vec::new();
        loop {
            let fname = self.expect_identifier()?;
            let base_type = self.expect_identifier()?;
            let type_mod = self.parse_type_mod()?;
            // An array-typed field (`xs i32[]`) — the same `[]` suffix a column type takes
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
        Ok(fields)
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

    /// `CREATE SEQUENCE [IF NOT EXISTS] <name> [options]` (spec/design/grammar.md §44). The
    /// options are order-free and each at most once (a repeat is 42601); option values are signed
    /// integer literals. Validation of the resolved option set (22023) and the namespace collision
    /// (42P07) are execution-time.
    fn parse_create_sequence(&mut self) -> Result<CreateSequence> {
        self.expect_keyword("create")?;
        self.expect_keyword("sequence")?;
        let if_not_exists = self.parse_if_not_exists()?;
        let name = self.expect_identifier()?;
        let options = self.parse_sequence_options(false)?;
        Ok(CreateSequence {
            name,
            if_not_exists,
            options,
        })
    }

    /// Parse the order-free sequence-option set (`INCREMENT [BY] n`, `MINVALUE`/`MAXVALUE` and their
    /// `NO` forms, `START [WITH] n`, `CACHE c`, `[NO] CYCLE`) shared by CREATE SEQUENCE and an
    /// IDENTITY column's `( seq_options )` (spec/design/sequences.md §13). When `parenthesized`, the
    /// options are wrapped in `( … )` and the loop stops at `)`; each option appears at most once
    /// (a repeat is 42601 via `dup_check`).
    fn parse_sequence_options(&mut self, parenthesized: bool) -> Result<SeqOptions> {
        let (seq, _restart) = self.parse_seq_options_inner(parenthesized, false)?;
        Ok(seq)
    }

    /// The shared order-free option loop. When `allow_restart` (only on `ALTER SEQUENCE`, never
    /// parenthesized), `RESTART [[WITH] n]` is also accepted as an interleavable pseudo-option and
    /// returned separately — `None` (absent), `Some(None)` (bare `RESTART`), or `Some(Some(n))`
    /// (`RESTART WITH n`); `RESTART` is invalid in `CREATE`/identity, where it falls through to the
    /// `_ => break` arm. Each option (including `RESTART`) appears at most once (42601).
    fn parse_seq_options_inner(
        &mut self,
        parenthesized: bool,
        allow_restart: bool,
    ) -> Result<(SeqOptions, Option<Option<i64>>)> {
        if parenthesized {
            self.expect(&Token::LParen)?;
        }
        let mut seq = SeqOptions::default();
        let mut restart: Option<Option<i64>> = None;
        loop {
            match self.peek_keyword().as_deref() {
                // `RESTART [[WITH] n]` — only on ALTER; resets the counter (sequences.md §15).
                Some("restart") if allow_restart => {
                    self.dup_check(restart.is_some(), "RESTART")?;
                    self.advance();
                    let v = if matches!(self.peek(), Token::Int(_) | Token::Minus)
                        || self.peek_keyword().as_deref() == Some("with")
                    {
                        self.consume_keyword("with");
                        Some(self.parse_signed_int_literal()?)
                    } else {
                        None
                    };
                    restart = Some(v);
                }
                // `AS <type>` — the sequence value type (order-free, S5 — sequences.md §14). The raw
                // type name is stored; it is resolved (and a non-integer type rejected 22023) at
                // execution. Inside an IDENTITY column's `( … )` a set `data_type` is 42601.
                Some("as") => {
                    self.dup_check(seq.data_type.is_some(), "AS")?;
                    self.advance();
                    seq.data_type = Some(self.expect_identifier()?);
                }
                Some("increment") => {
                    self.dup_check(seq.increment.is_some(), "INCREMENT")?;
                    self.advance();
                    self.consume_keyword("by");
                    seq.increment = Some(self.parse_signed_int_literal()?);
                }
                Some("minvalue") => {
                    self.dup_check(seq.min_value.is_some(), "MINVALUE")?;
                    self.advance();
                    seq.min_value = Some(Some(self.parse_signed_int_literal()?));
                }
                Some("maxvalue") => {
                    self.dup_check(seq.max_value.is_some(), "MAXVALUE")?;
                    self.advance();
                    seq.max_value = Some(Some(self.parse_signed_int_literal()?));
                }
                Some("start") => {
                    self.dup_check(seq.start.is_some(), "START")?;
                    self.advance();
                    self.consume_keyword("with");
                    seq.start = Some(self.parse_signed_int_literal()?);
                }
                Some("cache") => {
                    self.dup_check(seq.cache.is_some(), "CACHE")?;
                    self.advance();
                    seq.cache = Some(self.parse_signed_int_literal()?);
                }
                Some("cycle") => {
                    self.dup_check(seq.cycle.is_some(), "CYCLE")?;
                    self.advance();
                    seq.cycle = Some(true);
                }
                // `NO MINVALUE` / `NO MAXVALUE` / `NO CYCLE`.
                Some("no") => {
                    self.advance();
                    match self.peek_keyword().as_deref() {
                        Some("minvalue") => {
                            self.dup_check(seq.min_value.is_some(), "MINVALUE")?;
                            self.advance();
                            seq.min_value = Some(None);
                        }
                        Some("maxvalue") => {
                            self.dup_check(seq.max_value.is_some(), "MAXVALUE")?;
                            self.advance();
                            seq.max_value = Some(None);
                        }
                        Some("cycle") => {
                            self.dup_check(seq.cycle.is_some(), "CYCLE")?;
                            self.advance();
                            seq.cycle = Some(false);
                        }
                        other => {
                            return Err(syntax(format!(
                                "expected MINVALUE, MAXVALUE, or CYCLE after NO, found {other:?}"
                            )));
                        }
                    }
                }
                _ => break,
            }
        }
        if parenthesized {
            self.expect(&Token::RParen)?;
        }
        Ok((seq, restart))
    }

    /// `DROP SEQUENCE [IF EXISTS] <name> [, …] [RESTRICT | CASCADE]` (grammar.md §44). `CASCADE`
    /// is 0A000 at execution; a missing sequence (42P01) is execution-time.
    fn parse_drop_sequence(&mut self) -> Result<DropSequence> {
        self.expect_keyword("drop")?;
        self.expect_keyword("sequence")?;
        let if_exists = self.peek_keyword().as_deref() == Some("if");
        if if_exists {
            self.advance();
            self.expect_keyword("exists")?;
        }
        let mut names = vec![self.expect_identifier()?];
        while matches!(self.peek(), Token::Comma) {
            self.advance();
            names.push(self.expect_identifier()?);
        }
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
                "DROP SEQUENCE ... CASCADE is not supported".to_string(),
            ));
        }
        Ok(DropSequence { names, if_exists })
    }

    /// `ALTER SEQUENCE [IF EXISTS] <name> <action>` (spec/design/sequences.md §15). After the name
    /// the next keyword dispatches: `RENAME` → the rename form; `OWNED`/`OWNER`/`SET` → 0A000;
    /// otherwise the order-free option loop (the `CREATE` options plus an interleavable `RESTART`),
    /// which requires ≥ 1 option (a bare `ALTER SEQUENCE s` is 42601). `AS` is parsed into the option
    /// set and rejected as 0A000 at execution.
    fn parse_alter_sequence(&mut self) -> Result<AlterSequence> {
        self.expect_keyword("alter")?;
        self.expect_keyword("sequence")?;
        let if_exists = self.peek_keyword().as_deref() == Some("if");
        if if_exists {
            self.advance();
            self.expect_keyword("exists")?;
        }
        let name = self.expect_identifier()?;
        let action = match self.peek_keyword().as_deref() {
            Some("rename") => {
                self.advance();
                self.expect_keyword("to")?;
                AlterSeqAction::Rename(self.expect_identifier()?)
            }
            // The remaining ALTER actions jed does not support are 0A000 (not syntax errors), so the
            // parser recognizes their leading keyword and reports the feature gap (sequences.md §15).
            Some("owned") | Some("owner") | Some("set") => {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "this ALTER SEQUENCE action is not supported".to_string(),
                ));
            }
            _ => {
                let (options, restart) = self.parse_seq_options_inner(false, true)?;
                // ≥ 1 action required: a bare `ALTER SEQUENCE s` (no option, no RESTART) is 42601.
                if options == SeqOptions::default() && restart.is_none() {
                    return Err(syntax(
                        "ALTER SEQUENCE requires at least one action".to_string(),
                    ));
                }
                AlterSeqAction::SetOptions { options, restart }
            }
        };
        Ok(AlterSequence {
            name,
            if_exists,
            action,
        })
    }

    /// `IF NOT EXISTS` prefix (optional) — consumed when present.
    fn parse_if_not_exists(&mut self) -> Result<bool> {
        if self.peek_keyword().as_deref() == Some("if") {
            self.advance();
            self.expect_keyword("not")?;
            self.expect_keyword("exists")?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Consume an optional noise keyword (e.g. the `BY` in `INCREMENT BY`, the `WITH` in
    /// `START WITH`) when present.
    fn consume_keyword(&mut self, kw: &str) {
        if self.peek_keyword().as_deref() == Some(kw) {
            self.advance();
        }
    }

    /// 42601 when an option appeared twice.
    fn dup_check(&self, already: bool, opt: &str) -> Result<()> {
        if already {
            Err(syntax(format!("{opt} specified more than once")))
        } else {
            Ok(())
        }
    }

    /// A signed integer literal (`-? INT`) as an i64 — the sequence-option value form. The lexer
    /// caps an `Int` magnitude at 2^63, so the only out-of-range case is a bare positive 2^63
    /// (2200H/22003 — `numeric_value_out_of_range`); a negated 2^63 is i64::MIN (valid).
    fn parse_signed_int_literal(&mut self) -> Result<i64> {
        let neg = matches!(self.peek(), Token::Minus);
        if neg {
            self.advance();
        }
        let m = match self.advance() {
            Token::Int(m) => m,
            other => return Err(syntax(format!("expected an integer, found {other:?}"))),
        };
        let v: i128 = if neg { -(m as i128) } else { m as i128 };
        if v < i64::MIN as i128 || v > i64::MAX as i128 {
            return Err(EngineError::new(
                SqlState::NumericValueOutOfRange,
                "sequence parameter out of i64 range".to_string(),
            ));
        }
        Ok(v as i64)
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
        let (db, table) = self.parse_qualified_table_name()?;

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

        // Optional `OVERRIDING { SYSTEM | USER } VALUE` clause (spec/design/sequences.md §13),
        // after the column list and before the source. `OVERRIDING` / `SYSTEM` / `USER` / `VALUE`
        // are non-reserved; the clause is unambiguous against a `VALUES`/`SELECT` source.
        let overriding = if self.peek_keyword().as_deref() == Some("overriding") {
            self.advance();
            let mode = match self.peek_keyword().as_deref() {
                Some("system") => Overriding::System,
                Some("user") => Overriding::User,
                other => {
                    return Err(syntax(format!(
                        "expected SYSTEM or USER after OVERRIDING, found {other:?}"
                    )));
                }
            };
            self.advance();
            self.expect_keyword("value")?;
            Some(mode)
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
        let on_conflict = self.parse_on_conflict()?;
        let returning = self.parse_returning()?;
        Ok(Insert {
            table,
            db,
            columns,
            overriding,
            source,
            on_conflict,
            returning,
        })
    }

    /// Parse the optional `ON CONFLICT [target] action` clause (UPSERT — spec/design/upsert.md),
    /// after the source and before RETURNING. `ON`/`CONFLICT`/`DO`/`NOTHING`/`CONSTRAINT` are not
    /// reserved (§3); the clause is recognized by the `ON CONFLICT` two-keyword lead.
    fn parse_on_conflict(&mut self) -> Result<Option<OnConflict>> {
        if self.peek_keyword().as_deref() != Some("on")
            || self.peek_keyword_at(1).as_deref() != Some("conflict")
        {
            return Ok(None);
        }
        self.advance(); // ON
        self.advance(); // CONFLICT

        // Optional conflict target: a `( col, … )` column list or `ON CONSTRAINT name`.
        let target = if matches!(self.peek(), Token::LParen) {
            self.advance(); // '('
            let mut cols = Vec::new();
            loop {
                cols.push(self.expect_identifier()?);
                match self.advance() {
                    Token::Comma => continue,
                    Token::RParen => break,
                    other => return Err(syntax(format!("expected ',' or ')', found {other:?}"))),
                }
            }
            Some(ConflictTarget::Columns(cols))
        } else if self.peek_keyword().as_deref() == Some("on") {
            self.advance(); // ON
            self.expect_keyword("constraint")?;
            Some(ConflictTarget::Constraint(self.expect_identifier()?))
        } else {
            None
        };

        // The action: `DO NOTHING` or `DO UPDATE SET assignment [, …] [WHERE …]`.
        self.expect_keyword("do")?;
        let action = match self.peek_keyword().as_deref() {
            Some("nothing") => {
                self.advance();
                ConflictAction::DoNothing
            }
            Some("update") => {
                self.advance();
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
                let filter = self.parse_optional_where()?;
                ConflictAction::DoUpdate {
                    assignments,
                    filter,
                }
            }
            other => {
                return Err(syntax(format!(
                    "expected NOTHING or UPDATE after ON CONFLICT DO, found {other:?}"
                )));
            }
        };
        Ok(Some(OnConflict { target, action }))
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
            // `parse_set_expr` always begins with `parse_select_core`, so a top-level query_expr
            // never yields a nested `WITH` (a leading `WITH` statement routes to
            // `parse_with_statement`); the arm is for match exhaustiveness only.
            QueryExpr::With(_) => unreachable!("a top-level query_expr never begins with WITH"),
        })
    }

    /// Parse a top-level `query_expr` as a `QueryExpr` node — a set expression plus an optional
    /// trailing `ORDER BY` / `LIMIT` / `OFFSET` folded onto it. The shared core of
    /// `parse_query_expr` (which wraps it in a `Statement`) and a `WITH` clause's main body. Unlike
    /// `parse_subquery` it opens no new nesting level — the body is at the statement top level.
    fn parse_query_expr_node(&mut self) -> Result<QueryExpr> {
        let node = self.parse_set_expr()?;
        let order_by = self.parse_order_by(true)?;
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
            // `parse_set_expr` never yields a nested `WITH`; passthrough for exhaustiveness.
            QueryExpr::With(w) => QueryExpr::With(w),
        })
    }

    /// `query_statement ::= with_clause? query_expr` — a top-level query prefixed by a `WITH`
    /// clause defining common table expressions (spec/design/cte.md). `WITH RECURSIVE`
    /// (spec/design/recursive-cte.md) sets the `recursive` flag and lets a CTE reference itself;
    /// the CTE bodies and the main body are WITH-less `query_expr`s (the top-level-only narrowing —
    /// a nested `WITH` surfaces as 42601 because a body must begin with `SELECT`).
    fn parse_with_statement(&mut self) -> Result<Statement> {
        self.expect_keyword("with")?;
        // `WITH RECURSIVE …` enables self-reference (recursive-cte.md). RECURSIVE in this position
        // is the keyword (PG reserves it), so a CTE may not be named `recursive` — a documented
        // narrowing. The flag governs the whole list; whether a given CTE is *actually* recursive
        // is decided at planning by whether its body references its own name.
        let recursive = if self.peek_keyword().as_deref() == Some("recursive") {
            self.advance();
            true
        } else {
            false
        };
        let mut ctes = Vec::new();
        loop {
            ctes.push(self.parse_cte()?);
            if matches!(self.peek(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        // The primary may be a data-modifying statement (spec/design/writable-cte.md): a leading
        // INSERT/UPDATE/DELETE keyword selects it, otherwise a WITH-less query_expr.
        let body = self.parse_cte_body(false)?;
        Ok(Statement::With(WithQuery {
            ctes,
            body,
            recursive,
        }))
    }

    /// Parse a `cte_body` (spec/design/writable-cte.md): a data-modifying `INSERT`/`UPDATE`/`DELETE`
    /// when one leads, otherwise a query. `parenthesized` is true for a CTE body inside `( … )`
    /// (the closing `)` is the caller's), false for the `WITH` primary (it runs to end of
    /// statement). A query body parsed here is the WITH-less `query_expr` (the top-level-only
    /// nested-WITH narrowing — a nested `WITH` surfaces as a leftover `42601`).
    fn parse_cte_body(&mut self, parenthesized: bool) -> Result<CteBody> {
        let kw = self.peek_keyword();
        if matches!(
            kw.as_deref(),
            Some("insert") | Some("update") | Some("delete")
        ) {
            // A parenthesized data-modifying body counts one nesting level, like parse_subquery does
            // for a parenthesized query body (grammar.md §48); the primary (parenthesized = false)
            // runs at the statement top level and does not.
            if parenthesized {
                self.deepen()?;
            }
            let body = match kw.as_deref() {
                Some("insert") => CteBody::Insert(Box::new(self.parse_insert()?)),
                Some("update") => CteBody::Update(Box::new(self.parse_update()?)),
                _ => CteBody::Delete(Box::new(self.parse_delete()?)),
            };
            if parenthesized {
                self.undeepen();
            }
            Ok(body)
        } else if parenthesized {
            Ok(CteBody::Query(self.parse_subquery()?))
        } else {
            Ok(CteBody::Query(self.parse_query_expr_node()?))
        }
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
        let body = self.parse_cte_body(true)?;
        self.expect(&Token::RParen)?;
        Ok(Cte {
            name,
            columns,
            materialized,
            body,
        })
    }

    /// Parse a parenthesized subquery's inner `query_expr` (grammar.md §26): a full set-expression
    /// plus an optional trailing `ORDER BY` / `LIMIT` / `OFFSET` folded onto the node. Mirrors
    /// `parse_query_expr` but yields a `QueryExpr` (the subquery operand) rather than a `Statement`.
    /// The caller has already consumed the opening `(` and consumes the closing `)`. A leading
    /// `WITH` begins a **nested** common-table-expression query (spec/design/cte.md §7), parsed into
    /// a `QueryExpr::With`.
    fn parse_subquery(&mut self) -> Result<QueryExpr> {
        // A nested scalar subquery / EXISTS / IN (SELECT …) is one query-nesting level deeper;
        // the guard also protects the parser's own stack against `(SELECT (SELECT … ))`.
        self.deepen()?;
        let result = if self.at_with_clause() {
            self.parse_with_query_expr()?
        } else {
            self.parse_subquery_inner()?
        };
        self.undeepen();
        Ok(result)
    }

    /// The non-`WITH` body of a subquery: a set-expression plus an optional trailing
    /// `ORDER BY` / `LIMIT` / `OFFSET` folded onto the node. Split out so a nested `WITH`'s main
    /// query (`parse_with_query_expr`) reuses it.
    fn parse_subquery_inner(&mut self) -> Result<QueryExpr> {
        let node = self.parse_set_expr()?;
        let order_by = self.parse_order_by(true)?;
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
            // `parse_set_expr` never yields a nested `WITH`; passthrough for exhaustiveness.
            QueryExpr::With(w) => QueryExpr::With(w),
        })
    }

    /// Whether a `SELECT` keyword sits at token index `idx`.
    fn is_select_at(&self, idx: usize) -> bool {
        matches!(self.tokens.get(idx), Some(Token::Word(w)) if w.eq_ignore_ascii_case("select"))
    }

    /// Whether a `WITH` clause (`WITH RECURSIVE …`, `WITH <name> ( …`, or `WITH <name> AS …`)
    /// begins at token index `idx` (spec/design/cte.md §7), as opposed to an ordinary expression or
    /// a column named `with`. The shape-based lookahead keeps the recognition unambiguous even where
    /// `with` is a legal identifier (e.g. `x IN (with)` is a value list, not a nested WITH).
    fn is_with_clause_at(&self, idx: usize) -> bool {
        if !matches!(self.tokens.get(idx), Some(Token::Word(w)) if w.eq_ignore_ascii_case("with")) {
            return false;
        }
        match self.tokens.get(idx + 1) {
            Some(Token::Word(w)) if w.eq_ignore_ascii_case("recursive") => true,
            Some(Token::Word(_)) => {
                matches!(self.tokens.get(idx + 2), Some(Token::LParen))
                    || matches!(self.tokens.get(idx + 2), Some(Token::Word(w)) if w.eq_ignore_ascii_case("as"))
            }
            _ => false,
        }
    }

    /// Whether a query expression — a `SELECT` or a nested `WITH` clause (cte.md §7) — begins at
    /// token index `idx`. The §26 leading-`SELECT` lookahead, extended with `WITH`.
    fn is_query_start_at(&self, idx: usize) -> bool {
        self.is_select_at(idx) || self.is_with_clause_at(idx)
    }

    /// Whether the NEXT token begins a query expression (a `SELECT` or nested `WITH`) — the
    /// disambiguator at every subquery position (a scalar subquery `( … )`, `IN ( … )`,
    /// `op ANY/ALL ( … )`).
    fn at_subquery_start(&self) -> bool {
        self.is_query_start_at(self.pos)
    }

    /// Whether the NEXT token begins a nested `WITH` clause (cte.md §7).
    fn at_with_clause(&self) -> bool {
        self.is_with_clause_at(self.pos)
    }

    /// Parse a nested `WITH [RECURSIVE] cte (, cte)* query_expr` into a `QueryExpr::With`
    /// (spec/design/cte.md §7). The CTE bodies reuse `parse_cte` (so a CTE body may itself nest a
    /// `WITH`); the main query is a WITH-less `query_expr` (a bare second `WITH` after the list is
    /// not valid — a nested `WITH` reaches the body only through parentheses). A data-modifying CTE
    /// body parses here but is rejected at planning (`0A000`, top-level-only — matching PostgreSQL).
    fn parse_with_query_expr(&mut self) -> Result<QueryExpr> {
        self.expect_keyword("with")?;
        let recursive = if self.peek_keyword().as_deref() == Some("recursive") {
            self.advance();
            true
        } else {
            false
        };
        let mut ctes = Vec::new();
        loop {
            ctes.push(self.parse_cte()?);
            if matches!(self.peek(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        let body = self.parse_subquery_inner()?;
        Ok(QueryExpr::With(Box::new(WithExpr {
            ctes,
            recursive,
            body: Box::new(body),
        })))
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
        sel.order_by = self.parse_order_by(true)?;
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

        // WINDOW name AS ( definition ) (, …) — named windows referenced by OVER name (window.md §5).
        let windows = self.parse_window_clause()?;

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
            windows,
        })
    }

    /// `window_clause ::= "WINDOW" identifier "AS" "(" window_definition ")" ("," …)*` (window.md
    /// §5). Each entry is a full inline window definition (a base-window reference inside the
    /// definition is deferred). Empty when no WINDOW keyword is present. WINDOW is non-reserved.
    fn parse_window_clause(&mut self) -> Result<Vec<(String, WindowDef)>> {
        if self.peek_keyword().as_deref() != Some("window") {
            return Ok(Vec::new());
        }
        self.advance();
        let mut windows = Vec::new();
        loop {
            let name = self.expect_identifier()?;
            self.expect_keyword("as")?;
            self.expect(&Token::LParen)?;
            let def = self.parse_window_definition()?;
            self.expect(&Token::RParen)?;
            windows.push((name, def));
            if matches!(self.peek(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        Ok(windows)
    }

    /// Parse a window definition body `[base] [PARTITION BY …] [ORDER BY …] [frame]` between the
    /// already-consumed `(` and the closing `)` (spec/design/window.md §3, §5). The optional leading
    /// **base-window name** (a bareword that is not a clause-introducing keyword) marks a definition
    /// that extends a named window — the resolver merges it in (§5). Used by both the inline
    /// `OVER ( … )` and the `WINDOW name AS ( … )` clause so the two spellings parse identically.
    fn parse_window_definition(&mut self) -> Result<WindowDef> {
        let base = self.parse_opt_base_window_name();
        let mut partition = Vec::new();
        if self.peek_keyword().as_deref() == Some("partition") {
            self.advance();
            self.expect_keyword("by")?;
            // A PARTITION BY key is a general expression (`PARTITION BY a + b`), not just a column
            // (spec/design/window.md §5.1). A bare column resolves to its slot directly; a compound
            // expression is materialized into a synthetic window-key column before the window stage.
            loop {
                partition.push(self.parse_expr()?);
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        let order = self.parse_window_order_by()?;
        let frame = self.parse_window_frame()?;
        Ok(WindowDef {
            base,
            partition,
            order,
            frame,
        })
    }

    /// The optional leading base-window name of a window definition (spec/design/window.md §5).
    /// Present when the next token is a bareword that is **not** a clause-introducing keyword
    /// (`PARTITION`/`ORDER`/`ROWS`/`RANGE`/`GROUPS`) — those start the definition's own clauses, so
    /// an unquoted occurrence is the keyword, never a base name (matching PostgreSQL; a window named
    /// like a keyword would need quoting, which jed's window names do not support).
    fn parse_opt_base_window_name(&mut self) -> Option<String> {
        let is_base = match self.peek() {
            Token::Word(w) => !matches!(
                w.to_ascii_lowercase().as_str(),
                "partition" | "order" | "rows" | "range" | "groups"
            ),
            _ => false,
        };
        if is_base {
            if let Token::Word(w) = self.advance() {
                return Some(w);
            }
        }
        None
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

    /// `group_by ::= "GROUP" "BY" group_item ("," group_item)*` (grammar.md §18). Parsed after
    /// WHERE, before ORDER BY. Empty when absent. Each term is an ordinary column, a parenthesized
    /// column group, or `ROLLUP`/`CUBE`/`GROUPING SETS` (spec/design/aggregates.md §12); every
    /// grouping column is a bare/qualified column (the same narrowing ORDER BY makes — never an
    /// expression/alias/ordinal). `GROUP` is not reserved, so it is a clause only when immediately
    /// followed by `BY`.
    fn parse_group_by(&mut self) -> Result<Vec<GroupItem>> {
        if self.peek_keyword().as_deref() != Some("group") {
            return Ok(Vec::new());
        }
        self.advance(); // GROUP
        self.expect_keyword("by")?;
        let mut items = Vec::new();
        loop {
            items.push(self.parse_group_item()?);
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(items)
    }

    /// One GROUP BY grouping term — a `ROLLUP`/`CUBE`/`GROUPING SETS` construct, or an ordinary
    /// column group (a bare column, a parenthesized `(a, b)`, or the empty set `()`). Also used for
    /// the elements of a `GROUPING SETS (...)` list (which may themselves nest these forms).
    /// `ROLLUP`/`CUBE`/`GROUPING`/`SETS` are unreserved, recognized by lookahead only (a column
    /// literally named one of them must be quoted).
    fn parse_group_item(&mut self) -> Result<GroupItem> {
        match self.peek_keyword().as_deref() {
            Some("rollup") => {
                self.advance();
                Ok(GroupItem::Rollup(self.parse_group_set_list()?))
            }
            Some("cube") => {
                self.advance();
                Ok(GroupItem::Cube(self.parse_group_set_list()?))
            }
            Some("grouping") if self.peek_keyword_at(1).as_deref() == Some("sets") => {
                self.advance(); // GROUPING
                self.advance(); // SETS
                self.expect(&Token::LParen)?;
                let mut elems = Vec::new();
                loop {
                    elems.push(self.parse_group_item()?);
                    if matches!(self.peek(), Token::Comma) {
                        self.advance();
                        continue;
                    }
                    break;
                }
                self.expect(&Token::RParen)?;
                Ok(GroupItem::GroupingSets(elems))
            }
            _ => Ok(GroupItem::Set(self.parse_group_set()?)),
        }
    }

    /// The parenthesized `( group_set ("," group_set)* )` argument list of `ROLLUP` / `CUBE`,
    /// where each element is a grouping expression group (`spec/design/aggregates.md` §12/§15).
    fn parse_group_set_list(&mut self) -> Result<Vec<Vec<Expr>>> {
        self.expect(&Token::LParen)?;
        let mut sets = Vec::new();
        loop {
            sets.push(self.parse_group_set()?);
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        self.expect(&Token::RParen)?;
        Ok(sets)
    }

    /// A single grouping "expression group": a parenthesized `( e, ... )` / empty `()`, or a bare
    /// grouping term. Each member is a general expression — a bare/qualified column, a select-list
    /// ordinal (a bare integer literal), an output alias, or any expression (aggregates.md §15). A
    /// parenthesized list of two-or-more is a column group `(a, b)`; a single parenthesized
    /// expression `(a + b)` is one term — both fall out of parsing a comma-list of expressions.
    fn parse_group_set(&mut self) -> Result<Vec<Expr>> {
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            let mut cols = Vec::new();
            if !matches!(self.peek(), Token::RParen) {
                loop {
                    cols.push(self.parse_expr()?);
                    if matches!(self.peek(), Token::Comma) {
                        self.advance();
                        continue;
                    }
                    break;
                }
            }
            self.expect(&Token::RParen)?;
            Ok(cols)
        } else {
            Ok(vec![self.parse_expr()?])
        }
    }

    /// `from_clause ::= table_ref join_clause*` (spec/grammar/grammar.ebnf, grammar.md §15).
    /// The first table reference followed by a left-deep chain of zero or more joins. The
    /// join keywords are not reserved (§3); the loop recognizes a join only by a leading
    /// join keyword (`JOIN` / `INNER`/`CROSS`/`LEFT`/`RIGHT`/`FULL` ... `JOIN`), so any other
    /// trailing word ends the FROM clause.
    fn parse_from_clause(&mut self) -> Result<(TableRef, Vec<JoinClause>)> {
        let from = self.parse_table_ref()?;
        let mut joins = Vec::new();
        loop {
            while let Some(j) = self.parse_join_clause()? {
                joins.push(j);
            }
            // Comma-FROM (grammar.md §15): `FROM a, b` is an implicit `CROSS JOIN`. The comma
            // separates top-level FROM items, each of which is its own join sub-chain; it binds
            // LOOSER than `JOIN`, so the new item begins a fresh ON-resolution segment (recorded
            // by `comma: true`). After the comma's table_ref, the inner loop picks up any joins of
            // the new item (`a, b JOIN c ON …`) before the next comma is considered.
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                let table = self.parse_table_ref()?;
                joins.push(JoinClause {
                    kind: JoinKind::Cross,
                    table,
                    on: None,
                    using: None,
                    natural: false,
                    comma: true,
                });
                continue;
            }
            break;
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
        // An optional leading `LATERAL` (grammar.md §44) marks a derived table / table function as
        // correlated to the EARLIER FROM relations. `LATERAL` is non-reserved (§3), so it is the
        // keyword only when a derived table `(` or a function call `name(` follows (a two-token
        // lookahead) — otherwise it is an ordinary identifier (e.g. a table named `lateral`). A
        // table function is implicitly lateral regardless, so the keyword is redundant (but
        // accepted) there.
        let lateral = self.peek_keyword().as_deref() == Some("lateral")
            && (matches!(self.peek_at(1), Token::LParen)
                || (matches!(self.peek_at(1), Token::Word(_))
                    && matches!(self.peek_at(2), Token::LParen)));
        if lateral {
            self.advance();
        }
        if matches!(self.peek(), Token::LParen) {
            let mut tr = self.parse_derived_table()?;
            tr.lateral = lateral;
            return Ok(tr);
        }
        // `JSON_TABLE(ctx, path [AS n] COLUMNS (…))` — a table source (json-table.md §3, T1),
        // recognized by the keyword followed by `(`.
        if self.peek_keyword().as_deref() == Some("json_table")
            && matches!(self.peek_at(1), Token::LParen)
        {
            return self.parse_json_table();
        }
        let mut name = self.expect_identifier()?;
        // An optional DATABASE qualifier `db "." table` (attached-databases.md §3): a `.` after the
        // first identifier makes it the database qualifier and the next identifier the table name. A
        // qualified name is a BASE TABLE only — never a set-returning function (no cross-database SRF)
        // — so the function `(` branch below is guarded off when a qualifier is present. Mirrors
        // `parse_column_ref`'s one-`.`-lookahead, the only dotted-name precedent.
        let db = if matches!(self.peek(), Token::Dot) {
            self.advance(); // .
            let table = self.expect_identifier()?;
            let qualifier = name;
            name = table;
            Some(qualifier)
        } else {
            None
        };
        // A `(` right after the name = a set-returning function call (no `*`/`DISTINCT` — those
        // are aggregate/star forms, not an SRF argument list).
        let args = if matches!(self.peek(), Token::LParen) {
            if db.is_some() {
                return Err(syntax(
                    "a database-qualified name cannot be a function call",
                ));
            }
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
        // A `(` after the alias is a FROM-clause list on a table function (a base table never has
        // one there). The TYPED column-definition list `AS t(col type, …)` (C0, json-table.md §1) —
        // for the record-returning functions — is parsed here; the rename-only form `AS g(col)` (no
        // type) stays a deferred narrowing (grammar.md §35).
        let column_defs = if alias.is_some() && matches!(self.peek(), Token::LParen) {
            self.advance(); // (
            // Disambiguate: a col-def list has `name type`; a rename list has `name ,`/`name )`.
            // After the opening `(`, the current token is the first column name, so a `Word` in the
            // NEXT slot means a type follows (col-def list).
            let typed = matches!(self.peek_at(1), Token::Word(_));
            if !typed {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "column alias list on a table function is not supported yet",
                ));
            }
            Some(self.parse_field_def_list()?)
        } else {
            None
        };
        Ok(TableRef {
            name,
            db,
            alias,
            args,
            subquery: None,
            values: None,
            column_aliases: None,
            column_defs,
            json_table: None,
            // An SRF is implicitly lateral; `lateral` records only whether the keyword was written.
            lateral,
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
        } else if self.at_subquery_start() {
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
            // A derived table has no database qualifier (only a base table in table position does).
            db: None,
            alias,
            args: None,
            subquery,
            values,
            column_aliases,
            column_defs: None,
            json_table: None,
            // The caller (`parse_table_ref`) sets `lateral` from a leading `LATERAL` keyword.
            lateral: false,
        })
    }

    /// Parse `JSON_TABLE(ctx, path [AS n] COLUMNS (col, …)) [AS alias]` (json-table.md §3, T1). The
    /// caller has verified the `JSON_TABLE` keyword + `(`.
    fn parse_json_table(&mut self) -> Result<TableRef> {
        self.advance(); // JSON_TABLE
        self.advance(); // (
        let ctx = self.parse_expr()?;
        self.skip_format_json();
        self.expect(&Token::Comma)?;
        let path = self.parse_expr()?;
        // An optional `AS name` for the root path (the path-name) is accepted and ignored (it only
        // matters with an explicit PLAN clause, the deferred T2).
        if self.peek_keyword().as_deref() == Some("as") {
            self.advance();
            self.expect_identifier()?;
        }
        if self.peek_keyword().as_deref() == Some("passing") {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "JSON_TABLE PASSING clause is not supported yet",
            ));
        }
        self.expect_keyword("columns")?;
        let columns = self.parse_jt_columns()?;
        // An explicit PLAN clause is the deferred T2 slice.
        if self.peek_keyword().as_deref() == Some("plan") {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "JSON_TABLE explicit PLAN clause is not supported yet",
            ));
        }
        self.expect(&Token::RParen)?;
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
        Ok(TableRef {
            name: alias.clone().unwrap_or_else(|| "json_table".to_string()),
            // A JSON_TABLE source has no database qualifier.
            db: None,
            alias,
            args: None,
            subquery: None,
            values: None,
            column_aliases: None,
            column_defs: None,
            json_table: Some(Box::new(JsonTable {
                ctx: Box::new(ctx),
                path: Box::new(path),
                columns,
            })),
            lateral: false,
        })
    }

    /// Parse a parenthesized `JSON_TABLE` `COLUMNS` list — `"(" jt_column ("," jt_column)* ")"`.
    fn parse_jt_columns(&mut self) -> Result<Vec<JtColumn>> {
        self.expect(&Token::LParen)?;
        let mut cols = vec![self.parse_jt_column()?];
        while matches!(self.peek(), Token::Comma) {
            self.advance();
            cols.push(self.parse_jt_column()?);
        }
        self.expect(&Token::RParen)?;
        Ok(cols)
    }

    /// Parse one `JSON_TABLE` column: `NESTED [PATH] p [AS n] COLUMNS (…)`, `name FOR ORDINALITY`,
    /// `name type EXISTS [PATH p] [ON ERROR]`, or a regular `name type [PATH p] [wrapper] [quotes]
    /// [ON …]` column (json-table.md §3.3).
    fn parse_jt_column(&mut self) -> Result<JtColumn> {
        if self.peek_keyword().as_deref() == Some("nested") {
            self.advance(); // NESTED
            if self.peek_keyword().as_deref() == Some("path") {
                self.advance();
            }
            let path = match self.advance() {
                Token::Str(s) => s,
                _ => return Err(syntax("expected a string path after NESTED PATH")),
            };
            if self.peek_keyword().as_deref() == Some("as") {
                self.advance();
                self.expect_identifier()?;
            }
            self.expect_keyword("columns")?;
            let columns = self.parse_jt_columns()?;
            return Ok(JtColumn::Nested { path, columns });
        }
        let name = self.expect_identifier()?;
        // `name FOR ORDINALITY`.
        if self.peek_keyword().as_deref() == Some("for") {
            self.advance();
            self.expect_keyword("ordinality")?;
            return Ok(JtColumn::Ordinality { name });
        }
        // `name type …` — parse the type name + optional `[]`.
        let type_name = self.expect_identifier()?;
        let array = if matches!(self.peek(), Token::LBracket) {
            self.advance();
            self.expect(&Token::RBracket)?;
            true
        } else {
            false
        };
        // `EXISTS` column.
        if self.peek_keyword().as_deref() == Some("exists") {
            self.advance();
            let path = self.parse_jt_path_clause()?;
            let on_error = self.parse_json_on_error_only()?;
            return Ok(JtColumn::Exists {
                name,
                type_name,
                path,
                on_error,
            });
        }
        // A regular column.
        self.skip_format_json();
        let path = self.parse_jt_path_clause()?;
        let (wrapper, keep_quotes) = self.parse_json_wrapper_quotes()?;
        let (on_empty, on_error) = self.parse_json_on_clauses()?;
        Ok(JtColumn::Regular {
            name,
            type_name,
            array,
            path,
            wrapper,
            keep_quotes,
            on_empty,
            on_error,
        })
    }

    /// Parse an optional `PATH '<string>'` clause on a JSON_TABLE column.
    fn parse_jt_path_clause(&mut self) -> Result<Option<String>> {
        if self.peek_keyword().as_deref() == Some("path") {
            self.advance();
            match self.advance() {
                Token::Str(s) => Ok(Some(s)),
                _ => Err(syntax("expected a string after PATH")),
            }
        } else {
            Ok(None)
        }
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
        // An optional leading `NATURAL` (grammar.md §15) makes the join derive its USING column list
        // from the common column names. It is non-reserved (`natural` is in the table-ref stop set so
        // it is not swallowed as the prior relation's alias); once consumed it MUST be followed by a
        // join (a `NATURAL CROSS JOIN` / bare `NATURAL <non-join>` is 42601), and takes no ON/USING.
        let natural = self.peek_keyword().as_deref() == Some("natural");
        if natural {
            self.advance();
        }
        let kw = match self.peek_keyword() {
            Some(k) => k,
            None if natural => {
                return Err(syntax("NATURAL must be followed by a join".to_string()));
            }
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
            // `NATURAL CROSS JOIN` is invalid (PostgreSQL): NATURAL implies a derived condition.
            "cross" => {
                if natural {
                    return Err(syntax("NATURAL CROSS JOIN is not allowed".to_string()));
                }
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
            // After NATURAL a join keyword is required; otherwise the FROM chain just ends here.
            _ if natural => return Err(syntax("NATURAL must be followed by a join".to_string())),
            _ => return Ok(None),
        };
        let table = self.parse_table_ref()?;
        // A non-CROSS, non-NATURAL join takes either `ON <expr>` or `USING (col, …)` (grammar.md §15).
        // A NATURAL join derives its condition (no ON/USING), and CROSS takes none. USING is not
        // reserved (§3): it is the join condition only as the keyword immediately following the right
        // table_ref. The column list has one or more names; an empty list is a 42601.
        let (on, using) = if is_cross || natural {
            (None, None)
        } else if self.peek_keyword().as_deref() == Some("using") {
            self.advance();
            self.expect(&Token::LParen)?;
            let mut cols = vec![self.expect_identifier()?];
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                cols.push(self.expect_identifier()?);
            }
            self.expect(&Token::RParen)?;
            (None, Some(cols))
        } else {
            self.expect_keyword("on")?;
            (Some(self.parse_expr()?), None)
        };
        Ok(Some(JoinClause {
            kind,
            table,
            on,
            using,
            natural,
            comma: false,
        }))
    }

    /// Parse an optional `ORDER BY <key> ("," <key>)*` (spec/grammar/grammar.ebnf `order_by`).
    /// `nulls_first` is resolved here: explicit if given, else the direction default (ASC → last,
    /// DESC → first). A bare `NULLS` not followed by `FIRST`/`LAST` is a syntax error (42601). Returns
    /// an empty vec when there is no ORDER BY.
    ///
    /// Each key is parsed as a **general expression** and classified into one of the three `OrderKey`
    /// modes (grammar.md §10): a bare (optionally `COLLATE`-wrapped) column reference is a **column key**
    /// (kept on the fast path so PK-scan elision + the column's collation still apply); anything else is
    /// a general **expression key**. `allow_ordinal` governs the bare-integer case: when set (the query
    /// and set-operation ORDER BY) a bare integer literal (the unary-minus fold makes `-1` one negative
    /// `Int`) is an **ordinal**; when clear (WITHIN GROUP) a bare integer is just a constant expression
    /// key, matching PostgreSQL where a WITHIN GROUP integer is a constant, not an ordinal.
    fn parse_order_by(&mut self, allow_ordinal: bool) -> Result<Vec<OrderKey>> {
        let mut keys = Vec::new();
        if self.peek_keyword().as_deref() != Some("order") {
            return Ok(keys);
        }
        self.advance();
        self.expect_keyword("by")?;
        loop {
            let expr = self.parse_expr()?;
            let (collation, descending, nulls_first) = self.parse_sort_suffix()?;
            let key =
                Self::classify_order_key(expr, collation, descending, nulls_first, allow_ordinal);
            keys.push(key);
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(keys)
    }

    /// Classify a parsed ORDER BY key expression into one of the three `OrderKey` modes (grammar.md §10).
    /// `allow_ordinal` matches PostgreSQL's rule that only a **bare integer constant** is an ordinal — and
    /// only in a query/set-operation ORDER BY: when set, a `Literal::Int` (positive, or negative via the
    /// parser's unary-minus-on-literal fold) is an **ordinal**; when clear (WITHIN GROUP), the same bare
    /// integer falls through to a constant **expression key**. A bare column reference — directly, or
    /// wrapped in a `COLLATE` that `parse_expr`
    /// absorbed (`ORDER BY name COLLATE "x"`) — is a **column key** carrying that collation, so it stays
    /// on the fast path (PK-scan elision, per-column collation); every other shape (`a + 1`, `1 + 1`,
    /// `abs(b)`, a COLLATE over a compound) is a general **expression key**.
    fn classify_order_key(
        expr: Expr,
        collation: Option<String>,
        descending: bool,
        nulls_first: bool,
        allow_ordinal: bool,
    ) -> OrderKey {
        let mk = |ordinal, expr, qualifier, column, collation| OrderKey {
            ordinal,
            expr,
            qualifier,
            column,
            collation,
            descending,
            nulls_first,
        };
        match expr {
            Expr::Literal(Literal::Int(n)) if allow_ordinal => {
                mk(Some(n), None, None, String::new(), collation)
            }
            Expr::Column(name) => mk(None, None, None, name, collation),
            Expr::QualifiedColumn { qualifier, name } => {
                mk(None, None, Some(qualifier), name, collation)
            }
            // `parse_expr` folds a trailing `COLLATE "x"` into the key (collation.md §1). When it wraps a
            // bare column, unwrap back to a column key carrying that explicit collation — exactly the
            // OrderKey the old column-only parser built, so the column fast path is byte-identical.
            Expr::Collate {
                inner,
                collation: c,
            } => match *inner {
                Expr::Column(name) => mk(None, None, None, name, Some(c)),
                Expr::QualifiedColumn { qualifier, name } => {
                    mk(None, None, Some(qualifier), name, Some(c))
                }
                other => mk(
                    None,
                    Some(Expr::Collate {
                        inner: Box::new(other),
                        collation: c,
                    }),
                    None,
                    String::new(),
                    collation,
                ),
            },
            other => mk(None, Some(other), None, String::new(), collation),
        }
    }

    /// The trailing modifiers shared by every sort key: an optional `COLLATE "name"`, an optional
    /// `ASC`/`DESC` direction, and an optional `NULLS FIRST|LAST`. Returns `(collation, descending,
    /// nulls_first)`; `nulls_first` is resolved here — explicit if given, else the direction default
    /// (ASC → NULLS LAST, DESC → NULLS FIRST: NULL is the largest value, the PostgreSQL model,
    /// grammar.md §10). A bare `NULLS` not followed by `FIRST`/`LAST` is 42601. Used by both the query
    /// `ORDER BY` (after a column ref) and the window `ORDER BY` (after a general expression).
    fn parse_sort_suffix(&mut self) -> Result<(Option<String>, bool, bool)> {
        // Optional `COLLATE "name"` on the sort key (spec/design/collation.md §1), between the key
        // and the ASC/DESC direction (PG order).
        let collation = if self.peek_keyword().as_deref() == Some("collate") {
            self.advance();
            Some(self.expect_collation_name()?)
        } else {
            None
        };
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
            descending
        };
        Ok((collation, descending, nulls_first))
    }

    /// Parse a window `ORDER BY` (spec/design/window.md §5.1). Unlike the query `parse_order_by`
    /// (column references only), each key is a **general expression** (`ORDER BY a + b`,
    /// `ORDER BY sum(x)`) followed by the shared sort suffix. Returns an empty vec when absent. A
    /// `COLLATE` clause binds tighter than the comparison/arithmetic that could appear in a key, so
    /// `parse_expr` already absorbs an inline `expr COLLATE "x"`; the trailing `COLLATE` here is the
    /// sort-key collation (the same two-level reading the query ORDER BY uses on a bare column).
    fn parse_window_order_by(&mut self) -> Result<Vec<WindowOrderKey>> {
        let mut keys = Vec::new();
        if self.peek_keyword().as_deref() != Some("order") {
            return Ok(keys);
        }
        self.advance();
        self.expect_keyword("by")?;
        loop {
            let expr = self.parse_expr()?;
            let (collation, descending, nulls_first) = self.parse_sort_suffix()?;
            keys.push(WindowOrderKey {
                expr,
                collation,
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
        let (db, table) = self.parse_qualified_table_name()?;
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
            db,
            assignments,
            filter,
            returning,
        })
    }

    /// `DELETE FROM <table> [WHERE <pred>]`. No WHERE deletes every row.
    fn parse_delete(&mut self) -> Result<Delete> {
        self.expect_keyword("delete")?;
        self.expect_keyword("from")?;
        let (db, table) = self.parse_qualified_table_name()?;
        let filter = self.parse_optional_where()?;
        let returning = self.parse_returning()?;
        Ok(Delete {
            table,
            db,
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
            // `t.*` — a qualified star (all columns of the relation labeled `t`), a select-list /
            // RETURNING item MIXABLE with other items (grammar.md §15). Recognized by the
            // three-token shape `identifier "." "*"` before the general expr parser, so `t.col`
            // (Dot then a word) and `a * b` (no Dot) are untouched, and a bare `*` was already
            // handled above. It takes no `AS` alias (PG rejects one).
            if matches!(self.peek(), Token::Word(_))
                && matches!(self.peek_at(1), Token::Dot)
                && matches!(self.peek_at(2), Token::Star)
            {
                let qualifier = self.expect_identifier()?;
                self.advance(); // .
                self.advance(); // *
                items.push(SelectItem {
                    expr: Expr::QualifiedStar { qualifier },
                    alias: None,
                });
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                    continue;
                }
                break;
            }
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
            // IS [NOT] JSON [VALUE|SCALAR|ARRAY|OBJECT] [(WITH|WITHOUT) UNIQUE [KEYS]] — the SQL/JSON
            // well-formedness predicate (json-sql-functions.md §5).
            if self.peek_keyword().as_deref() == Some("json") {
                self.advance();
                let kind = match self.peek_keyword().as_deref() {
                    Some("value") => {
                        self.advance();
                        JsonPredicateKind::Value
                    }
                    Some("scalar") => {
                        self.advance();
                        JsonPredicateKind::Scalar
                    }
                    Some("array") => {
                        self.advance();
                        JsonPredicateKind::Array
                    }
                    Some("object") => {
                        self.advance();
                        JsonPredicateKind::Object
                    }
                    _ => JsonPredicateKind::Value,
                };
                // The unique-keys clause: `(WITH|WITHOUT) UNIQUE [KEYS]`. Consume `WITH`/`WITHOUT`
                // only when `UNIQUE` follows (a two-token lookahead — `WITH` otherwise starts no
                // expression-level clause here). `KEYS` is optional.
                let unique_keys = match self.peek_keyword().as_deref() {
                    Some(w @ ("with" | "without"))
                        if self.peek_keyword_at(1).as_deref() == Some("unique") =>
                    {
                        self.advance(); // WITH / WITHOUT
                        self.advance(); // UNIQUE
                        if self.peek_keyword().as_deref() == Some("keys") {
                            self.advance();
                        }
                        w == "with"
                    }
                    _ => false,
                };
                return Ok(Expr::IsJson {
                    operand: Box::new(lhs),
                    negated,
                    kind,
                    unique_keys,
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
                Some("in") | Some("between") | Some("like") | Some("ilike")
            );
        if negated {
            self.advance(); // NOT
        }
        if self.peek_keyword().as_deref() == Some("in") {
            self.advance();
            self.expect(&Token::LParen)?;
            // `IN (SELECT ...)` is the uncorrelated IN-subquery (grammar.md §26), disambiguated by
            // a leading `SELECT` (or a nested `WITH` — cte.md §7); otherwise a non-empty value list
            // (`IN ()` is rejected: parse_concat on `)` is 42601).
            if self.at_subquery_start() {
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
        // LIKE / ILIKE (case-insensitive) — grammar.md §22. Both bind at the comparison level; the
        // pattern is one CONCAT expression. `ilike` is just another peeked keyword (no lexer change).
        if matches!(self.peek_keyword().as_deref(), Some("like") | Some("ilike")) {
            let insensitive = self.peek_keyword().as_deref() == Some("ilike");
            self.advance();
            let rhs = self.parse_concat()?;
            return Ok(Expr::Like {
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                negated,
                insensitive,
            });
        }
        // `~` / `~*` / `!~` / `!~*` — regular-expression match (grammar.md §22b, regex.md). These are
        // PUNCTUATION operators (not keywords), so `negated`/`insensitive` come from the token itself;
        // there is no `NOT ~` keyword form (`NOT x ~ p` is the prefix-NOT over the whole match, taken
        // a level up by parse_not). The pattern is one CONCAT expression, like LIKE's.
        let regex = match self.peek() {
            Token::Tilde => Some((false, false)),
            Token::TildeStar => Some((false, true)),
            Token::BangTilde => Some((true, false)),
            Token::BangTildeStar => Some((true, true)),
            _ => None,
        };
        if let Some((rx_negated, rx_insensitive)) = regex {
            self.advance();
            let rhs = self.parse_concat()?;
            return Ok(Expr::Regex {
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                negated: rx_negated,
                insensitive: rx_insensitive,
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
                    // A leading `SELECT` (or a nested `WITH` — cte.md §7) is the SUBQUERY form
                    // `op ANY/ALL(SELECT …)` — the subquery spelling of IN (array-functions.md
                    // §11.6), the §26 leading-`SELECT` lookahead; anything else is the array operand
                    // (§11.1).
                    if self.at_subquery_start() {
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
                Token::JsonPathExists => BinaryOp::JsonPathExists,
                Token::JsonPathMatch => BinaryOp::JsonPathMatch,
                Token::ContainedBy => BinaryOp::ContainedBy,
                Token::Overlaps => BinaryOp::Overlaps,
                Token::StrictlyLeft => BinaryOp::StrictlyLeft,
                Token::StrictlyRight => BinaryOp::StrictlyRight,
                Token::NotExtendRight => BinaryOp::NotExtendRight,
                Token::NotExtendLeft => BinaryOp::NotExtendLeft,
                Token::Adjacent => BinaryOp::Adjacent,
                // The jsonb accessor operators (json-sql-functions.md §1) — "any other operator"
                // precedence, same level as `@>`/`||`, left-associative (`doc -> 'a' -> 'b'`).
                Token::Arrow => BinaryOp::JsonGet,
                Token::ArrowText => BinaryOp::JsonGetText,
                Token::HashArrow => BinaryOp::JsonGetPath,
                Token::HashArrowText => BinaryOp::JsonGetPathText,
                Token::Question => BinaryOp::JsonHasKey,
                Token::QuestionPipe => BinaryOp::JsonHasAnyKey,
                Token::QuestionAmp => BinaryOp::JsonHasAllKeys,
                Token::HashMinus => BinaryOp::JsonDeletePath,
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
        let mut lhs = self.parse_at_time_zone()?;
        loop {
            let op = match self.peek() {
                Token::Star => BinaryOp::Mul,
                Token::Slash => BinaryOp::Div,
                Token::Percent => BinaryOp::Mod,
                _ => break,
            };
            self.deepen()?; // each chained * / % is one more AST level
            self.advance();
            let rhs = self.parse_at_time_zone()?;
            lhs = binary(op, lhs, rhs);
        }
        self.depth = base;
        Ok(lhs)
    }

    /// The `AT TIME ZONE` rung (grammar.md §49, [timezones.md](../design/timezones.md) §6): a
    /// left-associative infix operator binding **tighter than `* / %`, additive, and the comparisons,
    /// looser than `COLLATE` / `::` / unary minus** (PostgreSQL's `%left AT`). `value AT TIME ZONE
    /// zone` desugars at parse time to the function call `timezone(zone, value)` — PostgreSQL's own
    /// implementation — so the resolver/evaluator/cost have one path for the operator and the bare
    /// `timezone(...)` call. `AT`/`TIME`/`ZONE` are non-reserved words (matched as a three-token
    /// sequence, never reserved), so a bare column named `at`/`time`/`zone` is unaffected.
    fn parse_at_time_zone(&mut self) -> Result<Expr> {
        let base = self.depth;
        let mut lhs = self.parse_unary()?;
        while self.peek_at_time_zone() {
            self.deepen()?; // each chained AT TIME ZONE is one more AST level
            self.advance(); // AT
            self.advance(); // TIME
            self.advance(); // ZONE
            let zone = self.parse_unary()?;
            lhs = Expr::FuncCall {
                name: "timezone".to_string(),
                args: vec![zone, lhs],
                arg_names: None,
                star: false,
                distinct: false,
                filter: None,
                variadic: false,
                over: None,
                over_name: None,
                within_group: None,
            };
        }
        self.depth = base;
        Ok(lhs)
    }

    /// Whether the next three tokens are the words `AT TIME ZONE` (case-insensitive).
    fn peek_at_time_zone(&self) -> bool {
        matches!(self.tokens.get(self.pos), Some(Token::Word(a)) if a.eq_ignore_ascii_case("at"))
            && matches!(self.tokens.get(self.pos + 1), Some(Token::Word(b)) if b.eq_ignore_ascii_case("time"))
            && matches!(self.tokens.get(self.pos + 2), Some(Token::Word(c)) if c.eq_ignore_ascii_case("zone"))
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if matches!(self.peek(), Token::Minus) {
            self.advance();
            // Fold unary-minus-of-an-integer-literal into one negative literal: this
            // makes i64::MIN representable (`-(2^63)`) and lets the negative value
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
            // `COLLATE "name"` is a postfix operator (spec/design/collation.md §1) sharing this
            // rung with `::`/`[]`/`.field` — so it binds tighter than `||` and the comparisons
            // (PG precedence) and chains left-to-right (`a::text COLLATE "C"`, `t.c COLLATE "x"`).
            let is_collate = matches!(self.peek(), Token::Word(_))
                && self.peek_keyword().as_deref() == Some("collate");
            let is_postfix = match self.peek() {
                Token::DoubleColon | Token::LBracket => true,
                Token::Dot => field_accessible,
                _ => is_collate,
            };
            if !is_postfix {
                break;
            }
            self.deepen()?;
            match self.peek() {
                _ if is_collate => {
                    self.advance(); // COLLATE
                    let collation = self.expect_collation_name()?;
                    expr = Expr::Collate {
                        inner: Box::new(expr),
                        collation,
                    };
                    field_accessible = false;
                }
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
            // `SELECT` (or a nested `WITH` — cte.md §7) after the `(`; otherwise this is a
            // parenthesized expression.
            if self.at_subquery_start() {
                let q = self.parse_subquery()?;
                self.expect(&Token::RParen)?;
                return Ok(Expr::ScalarSubquery(Box::new(q)));
            }
            let e = self.parse_expr()?;
            self.expect(&Token::RParen)?;
            return Ok(e);
        }
        // `EXISTS ( SELECT ... )` — the existence predicate (grammar.md §26). Recognized only when
        // an open-paren + a query start (`SELECT`, or a nested `WITH` — cte.md §7) follows, so
        // `exists` stays usable as a column / function name.
        if self.peek_keyword().as_deref() == Some("exists")
            && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
            && self.is_query_start_at(self.pos + 2)
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
        // `EXTRACT(field FROM source)` (grammar.md §50, timezones.md §9.2). Recognized only when
        // `extract` is immediately followed by `(`, so `extract` stays usable as a column / function
        // name otherwise (the one-token lookahead, §8). The field is an identifier or a string
        // literal (lowercased); the source is any expression.
        if self.peek_keyword().as_deref() == Some("extract")
            && matches!(self.peek_at(1), Token::LParen)
        {
            self.advance(); // EXTRACT
            self.expect(&Token::LParen)?;
            let field = match self.peek() {
                Token::Str(_) => {
                    if let Token::Str(s) = self.advance() {
                        s
                    } else {
                        unreachable!()
                    }
                }
                _ => self.expect_identifier()?,
            };
            self.expect_keyword("from")?;
            let source = self.parse_expr()?;
            self.expect(&Token::RParen)?;
            return Ok(Expr::Extract {
                field: field.to_ascii_lowercase(),
                source: Box::new(source),
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
        // `JSON(expr [(WITH|WITHOUT) UNIQUE [KEYS]])` — the SQL/JSON JSON() constructor
        // (json-sql-functions.md §5). Distinguished from the `json '...'` typed literal (handled
        // above, a string follows) and a generic call by being the JSON keyword followed by `(`.
        if self.peek_keyword().as_deref() == Some("json")
            && matches!(self.peek_at(1), Token::LParen)
        {
            self.advance(); // JSON
            self.advance(); // (
            let operand = self.parse_expr()?;
            let unique_keys = match self.peek_keyword().as_deref() {
                Some(w @ ("with" | "without"))
                    if self.peek_keyword_at(1).as_deref() == Some("unique") =>
                {
                    self.advance(); // WITH / WITHOUT
                    self.advance(); // UNIQUE
                    if self.peek_keyword().as_deref() == Some("keys") {
                        self.advance();
                    }
                    w == "with"
                }
                _ => false,
            };
            self.expect(&Token::RParen)?;
            return Ok(Expr::JsonCtor {
                operand: Box::new(operand),
                unique_keys,
            });
        }
        // The SQL/JSON query functions `JSON_EXISTS` / `JSON_VALUE` / `JSON_QUERY`
        // (json-sql-functions.md §5, S2) — keyword-led primaries with sub-clauses.
        if let Some(kw @ ("json_exists" | "json_value" | "json_query")) =
            self.peek_keyword().as_deref()
            && matches!(self.peek_at(1), Token::LParen)
        {
            let kw = kw.to_string();
            self.advance(); // the function keyword
            self.advance(); // (
            let ctx = self.parse_expr()?;
            // `FORMAT JSON` after the context item is accepted (and ignored — a text/json/jsonb
            // context is coerced to jsonb regardless).
            self.skip_format_json();
            self.expect(&Token::Comma)?;
            let path = self.parse_expr()?;
            // `PASSING arg AS name, …` (the path-variable surface) is the deferred S2 follow-on.
            if self.peek_keyword().as_deref() == Some("passing") {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "JSON query function PASSING clause is not supported yet",
                ));
            }
            let expr = match kw.as_str() {
                "json_exists" => {
                    let on_error = self.parse_json_on_error_only()?;
                    Expr::JsonExists {
                        ctx: Box::new(ctx),
                        path: Box::new(path),
                        on_error,
                    }
                }
                "json_value" => {
                    let returning = self.parse_json_returning()?;
                    let (on_empty, on_error) = self.parse_json_on_clauses()?;
                    Expr::JsonValue {
                        ctx: Box::new(ctx),
                        path: Box::new(path),
                        returning,
                        on_empty,
                        on_error,
                    }
                }
                _ => {
                    let returning = self.parse_json_returning()?;
                    let (wrapper, keep_quotes) = self.parse_json_wrapper_quotes()?;
                    let (on_empty, on_error) = self.parse_json_on_clauses()?;
                    Expr::JsonQuery {
                        ctx: Box::new(ctx),
                        path: Box::new(path),
                        returning,
                        wrapper,
                        keep_quotes,
                        on_empty,
                        on_error,
                    }
                }
            };
            self.expect(&Token::RParen)?;
            return Ok(expr);
        }
        // `COALESCE(a, b, …)` — the first-non-NULL conditional (grammar.md §51). Recognized only
        // when COALESCE is immediately followed by `(` (the JSON(/EXTRACT( one-token lookahead),
        // so the word stays usable as a column name. At least one argument (an empty list is
        // 42601 — PostgreSQL's grammar has no empty form).
        if self.peek_keyword().as_deref() == Some("coalesce")
            && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
        {
            self.advance(); // COALESCE
            self.advance(); // (
            if matches!(self.peek(), Token::RParen) {
                return Err(syntax("COALESCE requires at least one argument"));
            }
            let mut args = Vec::new();
            loop {
                args.push(self.parse_expr()?);
                if !matches!(self.peek(), Token::Comma) {
                    break;
                }
                self.advance(); // ,
            }
            self.expect(&Token::RParen)?;
            return Ok(Expr::Coalesce(args));
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
                    distinct: false,
                    filter: None,
                    variadic: false,
                    over: None,
                    over_name: None,
                    within_group: None,
                })
            }
            // `current_date` — the SQL-standard bare keyword, desugared to the current_date()
            // catalog function (functions.md §12, date.md §6). Unlike current_timestamp there is
            // no typmod form; a following `(` is the explicit call spelling, which jed also
            // resolves (PG rejects it as a syntax error — a documented jed-lenient divergence).
            Token::Word(w)
                if w.eq_ignore_ascii_case("current_date")
                    && !matches!(self.tokens.get(self.pos + 1), Some(Token::LParen)) =>
            {
                self.advance();
                Ok(Expr::FuncCall {
                    name: "current_date".to_string(),
                    args: Vec::new(),
                    arg_names: None,
                    star: false,
                    distinct: false,
                    filter: None,
                    variadic: false,
                    over: None,
                    over_name: None,
                    within_group: None,
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
        // A leading DISTINCT (`COUNT(DISTINCT x)`, aggregates.md §5) folds only the distinct
        // argument values. It is not reserved (a column may be named `distinct`), but here — right
        // after `(` — it is always the modifier. `DISTINCT *` and `DISTINCT )` (no argument) are
        // both 42601 syntax errors (PG); the resolver rejects DISTINCT on a non-aggregate (42809)
        // or a window function (0A000).
        let distinct = if self.peek_keyword().as_deref() == Some("distinct") {
            self.advance();
            if matches!(self.peek(), Token::Star) {
                return Err(syntax("DISTINCT cannot be used with *"));
            }
            if matches!(self.peek(), Token::RParen) {
                return Err(syntax("DISTINCT requires an aggregate argument"));
            }
            true
        } else {
            false
        };
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
        // A trailing `WITHIN GROUP (ORDER BY <key>)` marks an ordered-set aggregate (mode /
        // percentile_cont / percentile_disc — aggregates.md §13). It comes between the argument list
        // and any FILTER / OVER (PG order). `WITHIN`/`GROUP` are not reserved; right after the call's
        // `)` they are always the clause. The order key reuses `parse_order_by` with `allow_ordinal`
        // OFF — a general expression (`ORDER BY a + b`) but with a bare integer treated as a constant
        // (not an ordinal), matching PostgreSQL; the resolver enforces exactly one key (42883) and the
        // per-name rules.
        let within_group = if self.peek_keyword().as_deref() == Some("within") {
            self.advance();
            self.expect_keyword("group")?;
            self.expect(&Token::LParen)?;
            if self.peek_keyword().as_deref() != Some("order") {
                return Err(syntax("WITHIN GROUP requires an ORDER BY clause"));
            }
            let keys = self.parse_order_by(false)?;
            self.expect(&Token::RParen)?;
            Some(Box::new(keys))
        } else {
            None
        };
        // A trailing `FILTER (WHERE cond)` restricts which input rows feed THIS aggregate
        // (aggregates.md §11). PG syntax: `agg(args) FILTER (WHERE cond) [OVER (...)]` — FILTER binds
        // to the aggregate and precedes any OVER. FILTER is not a reserved word, but right after the
        // call's `)` it is always the modifier (PG: `count(*) filter` with no `(` is a syntax error,
        // not an alias). The condition is an ordinary boolean expression; the resolver rejects FILTER
        // on a non-aggregate (42809) or a window function (0A000), an aggregate inside cond (42803),
        // and a non-boolean cond (42804).
        let filter = if self.peek_keyword().as_deref() == Some("filter") {
            self.advance();
            self.expect(&Token::LParen)?;
            if self.peek_keyword().as_deref() != Some("where") {
                return Err(syntax("FILTER requires a WHERE clause"));
            }
            self.advance();
            let cond = self.parse_expr()?;
            self.expect(&Token::RParen)?;
            Some(Box::new(cond))
        } else {
            None
        };
        // A trailing `OVER (...)` turns the call into a window-function call (spec/design/window.md,
        // grammar.ebnf `over_clause`). S0 parses only the inline `OVER ( [PARTITION BY cols]
        // [ORDER BY ...] )` form; a named window `OVER name` (the WINDOW clause) is deferred to S5.
        let mut over_name: Option<String> = None;
        let over = if self.peek_keyword().as_deref() == Some("over") {
            self.advance();
            // `OVER name` references a named window (the WINDOW clause — window.md §5); `OVER (...)`
            // is an inline definition. A named reference is desugared to its definition at resolve.
            if !matches!(self.peek(), Token::LParen) {
                over_name = Some(self.expect_identifier()?);
                return Ok(Expr::FuncCall {
                    name,
                    args,
                    arg_names,
                    star,
                    distinct,
                    filter,
                    variadic,
                    over: None,
                    over_name,
                    within_group,
                });
            }
            self.expect(&Token::LParen)?;
            // `[base] [PARTITION BY cols] [ORDER BY …] [frame]` — the shared definition body. A
            // leading base-window name (window.md §5) extends a named window; merged at resolve.
            let def = self.parse_window_definition()?;
            self.expect(&Token::RParen)?;
            Some(Box::new(def))
        } else {
            None
        };
        Ok(Expr::FuncCall {
            name,
            args,
            arg_names,
            star,
            distinct,
            filter,
            variadic,
            over,
            over_name,
            within_group,
        })
    }

    /// Parse an optional window frame clause `{ROWS|RANGE|GROUPS} frame_extent [EXCLUDE …]`
    /// (spec/design/window.md §6, grammar.ebnf `frame_clause`). A single bound is the START
    /// (END = CURRENT ROW). `EXCLUDE` is rejected `0A000` in S4. Returns `None` when no frame
    /// keyword is present (the default frame).
    fn parse_window_frame(&mut self) -> Result<Option<WindowFrame>> {
        let mode = match self.peek_keyword().as_deref() {
            Some("rows") => FrameMode::Rows,
            Some("range") => FrameMode::Range,
            Some("groups") => FrameMode::Groups,
            _ => return Ok(None),
        };
        self.advance();
        let (start, end) = if self.peek_keyword().as_deref() == Some("between") {
            self.advance();
            let s = self.parse_frame_bound()?;
            self.expect_keyword("and")?;
            let e = self.parse_frame_bound()?;
            (s, e)
        } else {
            // A single bound is the frame START; the END defaults to CURRENT ROW.
            (self.parse_frame_bound()?, FrameBound::CurrentRow)
        };
        let exclude = self.parse_frame_exclusion()?;
        Ok(Some(WindowFrame {
            mode,
            start,
            end,
            exclude,
        }))
    }

    /// Parse an optional `EXCLUDE { CURRENT ROW | GROUP | TIES | NO OTHERS }` clause
    /// (spec/design/window.md §6); absent → `NoOthers` (drop nothing).
    fn parse_frame_exclusion(&mut self) -> Result<FrameExclusion> {
        if self.peek_keyword().as_deref() != Some("exclude") {
            return Ok(FrameExclusion::NoOthers);
        }
        self.advance();
        match self.peek_keyword().as_deref() {
            Some("current") => {
                self.advance();
                self.expect_keyword("row")?;
                Ok(FrameExclusion::CurrentRow)
            }
            Some("group") => {
                self.advance();
                Ok(FrameExclusion::Group)
            }
            Some("ties") => {
                self.advance();
                Ok(FrameExclusion::Ties)
            }
            Some("no") => {
                self.advance();
                self.expect_keyword("others")?;
                Ok(FrameExclusion::NoOthers)
            }
            _ => Err(syntax(
                "expected CURRENT ROW, GROUP, TIES, or NO OTHERS after EXCLUDE",
            )),
        }
    }

    /// Parse one frame bound: `UNBOUNDED PRECEDING|FOLLOWING`, `CURRENT ROW`, or `expr
    /// PRECEDING|FOLLOWING` (spec/design/window.md §6).
    fn parse_frame_bound(&mut self) -> Result<FrameBound> {
        match self.peek_keyword().as_deref() {
            Some("unbounded") => {
                self.advance();
                match self.peek_keyword().as_deref() {
                    Some("preceding") => {
                        self.advance();
                        Ok(FrameBound::UnboundedPreceding)
                    }
                    Some("following") => {
                        self.advance();
                        Ok(FrameBound::UnboundedFollowing)
                    }
                    _ => Err(syntax("expected PRECEDING or FOLLOWING after UNBOUNDED")),
                }
            }
            Some("current") => {
                self.advance();
                self.expect_keyword("row")?;
                Ok(FrameBound::CurrentRow)
            }
            _ => {
                let e = self.parse_expr()?;
                match self.peek_keyword().as_deref() {
                    Some("preceding") => {
                        self.advance();
                        Ok(FrameBound::Preceding(Box::new(e)))
                    }
                    Some("following") => {
                        self.advance();
                        Ok(FrameBound::Following(Box::new(e)))
                    }
                    _ => Err(syntax("expected PRECEDING or FOLLOWING in frame bound")),
                }
            }
        }
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

    /// Parse `qualified_table ::= (identifier ".")? identifier` in DML-target position
    /// (attached-databases.md §3): an optional database qualifier followed by the table name.
    /// Returns `(db, name)` where `db` is `None` for a bare (implicit-scope) name. The FROM-position
    /// analogue is inlined in `parse_table_ref` (which must also disambiguate the function /
    /// derived-table forms). Mirrors `parse_column_ref`'s one-`.`-lookahead — the only dotted-name
    /// precedent.
    fn parse_qualified_table_name(&mut self) -> Result<(Option<String>, String)> {
        let name = self.expect_identifier()?;
        if matches!(self.peek(), Token::Dot) {
            self.advance(); // .
            let table = self.expect_identifier()?;
            Ok((Some(name), table))
        } else {
            Ok((None, name))
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

    /// Consume a quoted collation name after `COLLATE` (spec/design/collation.md §1). The name is a
    /// double-quoted identifier — case-sensitive and kept verbatim (`"C"`, `"en-US"`), so a bare
    /// word is not accepted (it would case-fold). An empty name (`""`) is a 42601 syntax error.
    fn expect_collation_name(&mut self) -> Result<String> {
        match self.advance() {
            Token::QuotedIdent(s) if !s.is_empty() => Ok(s),
            Token::QuotedIdent(_) => Err(syntax("collation name may not be empty".to_string())),
            other => Err(syntax(format!(
                "expected a quoted collation name after COLLATE, found {other:?}"
            ))),
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
        // A double-quoted identifier round-trips verbatim with `"` doubled (collation names in a
        // persisted COLLATE expression, spec/design/collation.md §1).
        Token::QuotedIdent(s) => format!("\"{}\"", s.replace('"', "\"\"")),
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
        Token::JsonPathExists => "@?".into(),
        Token::JsonPathMatch => "@@".into(),
        Token::ContainedBy => "<@".into(),
        Token::Overlaps => "&&".into(),
        Token::StrictlyLeft => "<<".into(),
        Token::StrictlyRight => ">>".into(),
        Token::NotExtendRight => "&<".into(),
        Token::NotExtendLeft => "&>".into(),
        Token::Adjacent => "-|-".into(),
        Token::Arrow => "->".into(),
        Token::ArrowText => "->>".into(),
        Token::HashArrow => "#>".into(),
        Token::HashArrowText => "#>>".into(),
        Token::Question => "?".into(),
        Token::QuestionPipe => "?|".into(),
        Token::QuestionAmp => "?&".into(),
        Token::HashMinus => "#-".into(),
        Token::Tilde => "~".into(),
        Token::TildeStar => "~*".into(),
        Token::BangTilde => "!~".into(),
        Token::BangTildeStar => "!~*".into(),
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
            // USING introduces a join condition after the right table_ref (`JOIN b USING (k)`), so
            // it must not be swallowed as `b`'s implicit alias (grammar.md §15).
            | "using"
            // NATURAL prefixes a join (`a NATURAL JOIN b`), so it must not be swallowed as the prior
            // relation's alias (grammar.md §15).
            | "natural"
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
            // WINDOW ends a SELECT core's FROM — it introduces the named-window clause and must
            // not be swallowed as an implicit table alias (`FROM t WINDOW w AS …`). window.md §5.
            | "window"
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
