//! Hand-written recursive-descent parser (CLAUDE.md §5, §10).
//!
//! Statement productions are filled in feature-by-feature (Phases B–E). Until a
//! production is implemented it returns a structured `0A000` feature-not-supported
//! error rather than panicking, so the harness reports "not yet" cleanly.

use crate::ast::{
    AlterColumnAction, AlterColumnKind, AlterConstraintDef, AlterSeqAction, AlterSequence,
    AlterTable, AlterTableAction, AlterTableEdit, AlterView, AlterViewAction, Analyze, Assignment,
    BinaryOp, CheckDef, ColumnDef, ConflictAction, ConflictTarget, CreateIndex, CreateSequence,
    CreateTable, CreateTableAs, CreateType, CreateView, Cte, CteBody, DefaultDef, Delete,
    DropIndex, DropSequence, DropTable, DropType, DropView, ExcludeDef, Expr, ForeignKeyDef,
    GroupItem, IdentitySpec, IndexKeyElem, IndexPredicate, Insert, InsertSource, InsertValue,
    JoinClause, JoinKind, JsonBehavior, JsonOnBehavior, JsonPassing, JsonPredicateKind, JsonTable,
    JsonWrapper, JtColumn, Literal, OnConflict, OrderKey, Overriding, QueryExpr, RefAction,
    Reindex, ReindexKind, ReturningClause, SavepointOp, Select, SelectItem, SelectItems,
    SeqOptions, SetOp, SetOpKind, SrcPos, StarPos, Statement, SubscriptSpec, TableRef, TableRefPos,
    TypeFieldDef, TypeMod, UnaryOp, UniqueDef, Update, WindowDef, WithExpr, WithQuery,
};
use crate::ast::{CheckOption, FrameBound, FrameExclusion, FrameMode, WindowFrame, WindowOrderKey};
use crate::ast::{CycleClause, SearchClause};
use crate::decimal::Decimal;
use crate::error::{EngineError, Result, SqlState};
use crate::lexer::{lex, lex_with_positions};
use crate::stack::outline;
use crate::token::Token;

/// Maximum expression / subquery / set-operation **nesting depth** a statement may reach
/// (spec/design/cost.md §7; CLAUDE.md §13). The §13 native-stack-safety gate for untrusted
/// input: the recursive-descent parser and the resolve/eval walks recurse to a statement's
/// nesting depth, so deeply-nested SQL would overflow the call stack BEFORE the cost meter runs
/// (`54P01` cannot catch it). Counting logical depth against this **fixed** bound — rather than
/// PG's runtime stack-pointer probe — is deterministic and **cross-core identical** (§8): the
/// constant is the SAME in every core (Rust / Go / TS), far above any realistic query. Exceeding it
/// aborts with `54001 statement_too_complex`. In this core a statement at the limit stays within a
/// 1 MiB native stack in an optimized build (spec/design/cost.md §7.1).
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
    /// The deepest `depth` the parse has reached. A stored view body's is its nesting depth
    /// (spec/design/views.md §4.1), which a reference adds to its own.
    max_depth: usize,
    /// The token position just past the `)` of the most recent bare `(a, b, …)` row constructor
    /// (0 = none). A bare row is not a parenthesized base, so `parse_postfix` refuses `.field` on it
    /// (PostgreSQL: `(1,2).f1` is a syntax error, `((1,2)).f1` is field access).
    bare_row_end: usize,
    /// For each token index, whether a PARENTHESIZED QUERY begins there (`is_query_start_at`,
    /// grammar.md §25). Precomputed once in one backward pass so every probe is O(1) — a long run
    /// of `(` costs linear time, not time per nesting level.
    paren_query: Vec<bool>,
    /// Each token's 1-based character position in the statement text (spec/design/error-fields.md
    /// §10.1), index-aligned with `tokens`; empty for a parse of stored text, which records none.
    positions: Vec<u32>,
    /// The furthest token index the parse has examined through `peek` / `advance` — the token a
    /// parse error is positioned at (spec/design/error-fields.md §10.3).
    furthest: std::cell::Cell<usize>,
    /// `SELECT … INTO` (spec/design/create-table-as.md §6): whether the next `select_core` is the
    /// leftmost SELECT of the statement's own query, the one place an `INTO` clause may appear. Set
    /// for a query statement (and a `CREATE VIEW` body, which refuses it); the first `select_core`
    /// takes it, so every later or nested one sees `false`.
    into_ok: bool,
    /// Whether the statement's own query is being parsed (a query statement or a `CREATE VIEW`
    /// body), so an `INTO` on a later set-operation arm is *only allowed on first SELECT* rather than
    /// *not allowed here*.
    into_top: bool,
    /// Whether the query is a `CREATE VIEW` body: its leftmost `INTO` is 0A000.
    into_view: bool,
    /// How many query constructs (a SELECT's clauses, a CTE body) enclose the cursor within the
    /// statement's own query: an `INTO` at depth 0 is on a set-operation arm.
    into_nest: usize,
    /// The `INTO` target the leftmost SELECT named, taken by the statement-level caller.
    select_into: Option<SelectInto>,
}

/// The target of a `SELECT … INTO [TEMP | TEMPORARY] [TABLE] [db.]name` (create-table-as.md §6).
struct SelectInto {
    temp: bool,
    db: Option<String>,
    name: String,
}

impl Parser {
    /// A parser over stored text: no source positions are recorded (error-fields.md §10.1).
    pub fn new(tokens: Vec<Token>) -> Self {
        Self::with_positions(tokens, Vec::new())
    }

    /// A parser over host-written text, recording each token's character position.
    pub fn with_positions(tokens: Vec<Token>, positions: Vec<u32>) -> Self {
        let mut p = Parser {
            tokens,
            positions,
            furthest: std::cell::Cell::new(0),
            pos: 0,
            depth: 0,
            max_depth: 0,
            bare_row_end: 0,
            paren_query: Vec::new(),
            into_ok: false,
            into_top: false,
            into_view: false,
            into_nest: 0,
            select_into: None,
        };
        p.paren_query = p.compute_paren_query();
        p
    }

    /// Build the `paren_query` table: the `(` at `i` begins a parenthesized query when what follows
    /// it is a query start (a `SELECT`, a `VALUES` list, a `WITH` clause, or — recursively — another
    /// parenthesized query) AND its matching `)` is followed by a token that can only continue a
    /// query (`is_query_follow_at`). Computed back to front, so the recursion is a table lookup.
    fn compute_paren_query(&self) -> Vec<bool> {
        let n = self.tokens.len();
        let mut close = vec![n; n];
        let mut open: Vec<usize> = Vec::new();
        for (i, t) in self.tokens.iter().enumerate() {
            match t {
                Token::LParen => open.push(i),
                Token::RParen => {
                    if let Some(o) = open.pop() {
                        close[o] = i;
                    }
                }
                _ => {}
            }
        }
        let mut q = vec![false; n];
        for i in (0..n).rev() {
            if !matches!(self.tokens[i], Token::LParen) {
                continue;
            }
            let inner = self.is_select_at(i + 1)
                || self.is_values_at(i + 1)
                || self.is_with_clause_at(i + 1)
                || q.get(i + 1).copied().unwrap_or(false);
            q[i] = inner && self.is_query_follow_at(close[i] + 1);
        }
        q
    }

    /// Descend one nesting level, enforcing `MAX_EXPR_DEPTH` (spec/design/cost.md §7). Call at
    /// every point the AST gains a level — a binary-chain step, a unary, a postfix, a re-entry
    /// into a fresh sub-expression, a nested subquery, a set-op branch. The caller restores the
    /// depth with `undeepen` on the success path (`?` short-circuits leave it stale, which is
    /// harmless: the parse is aborting).
    fn deepen(&mut self) -> Result<()> {
        self.depth += 1;
        self.max_depth = self.max_depth.max(self.depth);
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

    /// Parse a single complete statement from `sql`. An error carries the character position of
    /// the token the parse stopped at (spec/design/error-fields.md §10.2/§10.3), except a class-54
    /// program limit.
    pub fn parse_sql(sql: &str) -> Result<Statement> {
        let (tokens, positions) = lex_with_positions(sql)?;
        let mut p = Parser::with_positions(tokens, positions);
        let parsed = p.parse_statement().and_then(|stmt| {
            p.expect_eof()?;
            Ok(stmt)
        });
        parsed.map_err(|e| {
            if e.code().starts_with("54") {
                e
            } else {
                let at = p.src_pos(p.furthest.get()).0;
                e.with_position(at)
            }
        })
    }

    /// The character position of token `i` (`SrcPos::NONE` for stored text).
    fn src_pos(&self, i: usize) -> SrcPos {
        SrcPos(self.positions.get(i).copied().unwrap_or(0))
    }

    /// The character position of the current token.
    fn here(&self) -> SrcPos {
        self.src_pos(self.pos)
    }

    /// Record that the parse examined token `i` (error-fields.md §10.3).
    fn note(&self, i: usize) {
        if i > self.furthest.get() {
            self.furthest.set(i);
        }
    }

    fn parse_statement(&mut self) -> Result<Statement> {
        // Dispatch on the leading keyword. Remaining productions land in Phases D–E.
        match self.peek_keyword().as_deref() {
            Some("analyze") => Ok(Statement::Analyze(self.parse_analyze()?)),
            Some("reindex") => Ok(Statement::Reindex(self.parse_reindex()?)),
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
            // CREATE VIEW (views.md §1, grammar.md §54) — VIEW is recognized positionally, so
            // `CREATE TABLE view (…)` still names a table. The deferred forms are recognized by their
            // keyword sequence and refused with 0A000 before the body is parsed (§12).
            Some("create") if self.at_deferred_create_view() => Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "this form of CREATE VIEW is not supported",
            )),
            Some("create") if self.at_create_view() => {
                Ok(Statement::CreateView(self.parse_create_view()?))
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
            Some("drop") if self.peek_keyword_at(1).as_deref() == Some("view") => {
                Ok(Statement::DropView(self.parse_drop_view()?))
            }
            Some("drop") => Ok(Statement::DropTable(self.parse_drop_table()?)),
            // ALTER SEQUENCE — the only ALTER statement this slice (sequences.md §4). A 2-token
            // lookahead recognizes it; any other `ALTER …` (TABLE, SYSTEM, …) is not a statement
            // keyword jed knows and falls through to the generic unknown-keyword 42601 below
            // (the no-escape-hatch surface — resource/no_escape_hatch.test).
            Some("alter") if self.peek_keyword_at(1).as_deref() == Some("sequence") => {
                Ok(Statement::AlterSequence(self.parse_alter_sequence()?))
            }
            // ALTER VIEW … RENAME (views.md §7.1); its other actions are recognized and refused.
            Some("alter") if self.peek_keyword_at(1).as_deref() == Some("view") => {
                Ok(Statement::AlterView(self.parse_alter_view()?))
            }
            Some("alter") if self.peek_keyword_at(1).as_deref() == Some("table") => {
                Ok(Statement::AlterTable(self.parse_alter_table()?))
            }
            Some("insert") => Ok(Statement::Insert(self.parse_insert()?)),
            Some("select") => self.parse_into_query(Self::parse_query_expr),
            // A top-level `VALUES` query (grammar.md §25): `VALUES (…), (…) [ORDER BY …]`.
            Some("values") => self.parse_into_query(Self::parse_query_expr),
            // `WITH …` at statement start can only begin a query with common table expressions
            // (spec/design/cte.md). `with` is non-reserved but unambiguous here.
            Some("with") => self.parse_into_query(Self::parse_with_statement),
            Some("update") => Ok(Statement::Update(self.parse_update()?)),
            Some("delete") => Ok(Statement::Delete(self.parse_delete()?)),
            Some("explain") => self.parse_explain(),
            Some("begin") | Some("start") => self.parse_begin(),
            Some("commit") | Some("end") => self.parse_commit(),
            Some("rollback") => self.parse_rollback(),
            Some("savepoint") => {
                self.advance(); // SAVEPOINT
                Ok(Statement::Savepoint {
                    op: SavepointOp::Define,
                    name: self.expect_identifier()?,
                })
            }
            Some("release") => {
                self.advance(); // RELEASE
                Ok(Statement::Savepoint {
                    op: SavepointOp::Release,
                    name: self.parse_savepoint_name()?,
                })
            }
            Some(other) => Err(syntax(format!("unexpected keyword '{other}'"))),
            // A leading `(` can only begin a parenthesized query (grammar.md §25).
            None if matches!(self.peek(), Token::LParen) => {
                self.parse_into_query(Self::parse_query_expr)
            }
            None => Err(syntax("expected a SQL statement")),
        }
    }

    /// Parse a query statement whose leftmost SELECT may carry `INTO` (spec/design/create-table-as.md
    /// §6). A query with an `INTO` target is the statement `CREATE [TEMP] TABLE target AS query`.
    fn parse_into_query(&mut self, parse: fn(&mut Self) -> Result<Statement>) -> Result<Statement> {
        self.into_ok = true;
        self.into_top = true;
        let stmt = parse(self)?;
        self.into_ok = false;
        self.into_top = false;
        let Some(into) = self.select_into.take() else {
            return Ok(stmt);
        };
        let query = match stmt {
            Statement::Select(sel) => QueryExpr::Select(Box::new(sel)),
            Statement::SetOp(so) => QueryExpr::SetOp(Box::new(so)),
            Statement::With(WithQuery {
                ctes,
                body: CteBody::Query(body),
                recursive,
            }) => QueryExpr::With(Box::new(WithExpr {
                ctes,
                recursive,
                body: Box::new(body),
            })),
            _ => return Err(syntax("SELECT ... INTO is not allowed here")),
        };
        Ok(Statement::CreateTable(CreateTable {
            name: into.name,
            temp: into.temp,
            if_not_exists: false,
            columns: Vec::new(),
            table_pks: Vec::new(),
            checks: Vec::new(),
            uniques: Vec::new(),
            foreign_keys: Vec::new(),
            excludes: Vec::new(),
            db: into.db,
            on_commit: None,
            as_query: Some(Box::new(CreateTableAs {
                column_names: None,
                query,
                with_data: true,
            })),
        }))
    }

    /// The `INTO [TEMPORARY | TEMP] [TABLE] [db.]name` of a `SELECT … INTO`, after its select list
    /// (spec/design/create-table-as.md §6). `owner` is whether this SELECT is the statement's
    /// leftmost one. Anywhere else the clause is 42601 — on a later set-operation arm *INTO is only
    /// allowed on first SELECT*, in a subquery, CTE body, or another statement's query *not allowed
    /// here* — and in a `CREATE VIEW` body 0A000, as in PostgreSQL. `TEMP`, `TEMPORARY`, `UNLOGGED`,
    /// and `TABLE` are words of the clause unless a `.` follows (a database qualifier).
    fn parse_select_into(&mut self, owner: bool) -> Result<()> {
        self.advance(); // INTO
        if !owner {
            let msg = if self.into_top && self.into_nest == 1 {
                "INTO is only allowed on first SELECT of UNION/INTERSECT/EXCEPT"
            } else {
                "SELECT ... INTO is not allowed here"
            };
            return Err(syntax(msg));
        }
        if self.into_view {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "views must not contain SELECT INTO",
            ));
        }
        let word = |p: &Self, w: &str| {
            p.peek_keyword().as_deref() == Some(w) && !matches!(p.peek_at(1), Token::Dot)
        };
        let temp = word(self, "temp") || word(self, "temporary");
        if temp {
            self.advance();
        } else if word(self, "unlogged") {
            return Err(unlogged_not_supported());
        }
        if word(self, "table") {
            self.advance();
        }
        let (db, name) = self.parse_qualified_table_name()?;
        self.select_into = Some(SelectInto { temp, db, name });
        Ok(())
    }

    /// Whether the statement is one of the deferred `CREATE … VIEW` forms (views.md §12): `CREATE
    /// [OR REPLACE] TEMP | TEMPORARY VIEW`, `CREATE [OR REPLACE] RECURSIVE VIEW`, `CREATE
    /// MATERIALIZED VIEW`. Recognized by keyword sequence so each fails 0A000, not 42601.
    fn at_deferred_create_view(&self) -> bool {
        let kw = |i: usize| self.peek_keyword_at(i);
        let at = if kw(1).as_deref() == Some("or") && kw(2).as_deref() == Some("replace") {
            3
        } else {
            1
        };
        matches!(
            kw(at).as_deref(),
            Some("temp") | Some("temporary") | Some("recursive") | Some("materialized")
        ) && kw(at + 1).as_deref() == Some("view")
    }

    /// Whether the statement is `CREATE [OR REPLACE] VIEW` (views.md §1).
    fn at_create_view(&self) -> bool {
        let kw = |i: usize| self.peek_keyword_at(i);
        kw(1).as_deref() == Some("view")
            || (kw(1).as_deref() == Some("or")
                && kw(2).as_deref() == Some("replace")
                && kw(3).as_deref() == Some("view"))
    }

    /// `CREATE [OR REPLACE] VIEW [IF NOT EXISTS] [db.]name [(col, …)] AS subquery_expr` (views.md
    /// §1, grammar.md §54). The body runs from the token after `AS` to the end of the statement and
    /// is captured as its canonical token text. A trailing `WITH [CASCADED | LOCAL] CHECK OPTION`
    /// is cut off before the body and gives the view a check option (§15.6). A `WITH ( … )` option
    /// list after the name is a deferred form (0A000). `OR REPLACE` with `IF NOT EXISTS` is 42601.
    fn parse_create_view(&mut self) -> Result<CreateView> {
        self.expect_keyword("create")?;
        let or_replace = self.peek_keyword().as_deref() == Some("or");
        if or_replace {
            self.advance(); // OR
            self.expect_keyword("replace")?;
        }
        self.expect_keyword("view")?;
        // A two-token lookahead keeps a lone `if` an ordinary view name (`CREATE VIEW if AS …`).
        let if_not_exists = if self.peek_keyword().as_deref() == Some("if")
            && self.peek_keyword_at(1).as_deref() == Some("not")
        {
            self.parse_if_not_exists()?
        } else {
            false
        };
        if or_replace && if_not_exists {
            return Err(syntax(
                "CREATE OR REPLACE VIEW cannot be combined with IF NOT EXISTS",
            ));
        }
        let (db, name) = self.parse_qualified_table_name()?;
        if self.peek_keyword().as_deref() == Some("with")
            && matches!(self.peek_at(1), Token::LParen)
        {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "view options (WITH ( … )) are not supported",
            ));
        }
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
        // A trailing `WITH [CASCADED | LOCAL] CHECK OPTION` is cut off before the body is parsed
        // (an unreserved `with` would otherwise be read as a table alias).
        let trailing = self.trailing_check_option();
        let check_option = trailing.map(|(at, option)| {
            self.tokens[at] = Token::Eof;
            option
        });
        let start = self.pos;
        // The body's leftmost SELECT may not take `INTO` (0A000, create-table-as.md §6).
        (self.into_ok, self.into_top, self.into_view) = (true, true, true);
        self.parse_subquery()?;
        (self.into_ok, self.into_top, self.into_view) = (false, false, false);
        let end = self.pos;
        if let Some((at, _)) = trailing {
            if end != at {
                return Err(syntax(format!(
                    "unexpected trailing input: {:?}",
                    self.peek()
                )));
            }
        }
        let body = render_tokens(&self.tokens[start..end]);
        // `render_tokens` re-lexes to the same token sequence, so the body's tokens keep the
        // statement's positions (and its `Eof` the position of the token after the body).
        let body_positions = self
            .positions
            .get(start..=end)
            .map_or_else(Vec::new, <[u32]>::to_vec);
        Ok(CreateView {
            name,
            db,
            or_replace,
            if_not_exists,
            columns,
            body,
            body_positions,
            check_option,
        })
    }

    /// The index and option of a trailing `WITH [CASCADED | LOCAL] CHECK OPTION` at the end of the
    /// token stream (views.md §15.6), if the statement ends with one. Bare `WITH CHECK OPTION` is
    /// `CASCADED`.
    fn trailing_check_option(&self) -> Option<(usize, CheckOption)> {
        let mut end = self.tokens.len();
        if matches!(self.tokens.last(), Some(Token::Eof)) {
            end -= 1;
        }
        let word = |i: usize, w: &str| matches!(self.tokens.get(i), Some(Token::Word(x)) if x == w);
        if end < self.pos + 3 || !word(end - 1, "option") || !word(end - 2, "check") {
            return None;
        }
        if word(end - 3, "with") {
            return Some((end - 3, CheckOption::Cascaded));
        }
        if end >= self.pos + 4 && word(end - 4, "with") {
            if word(end - 3, "cascaded") {
                return Some((end - 4, CheckOption::Cascaded));
            }
            if word(end - 3, "local") {
                return Some((end - 4, CheckOption::Local));
            }
        }
        None
    }

    /// `ALTER VIEW [IF EXISTS] [db.]name RENAME { TO new | [COLUMN] old TO new }` (views.md §7.1).
    /// The other `ALTER VIEW` actions (`ALTER [COLUMN]`, `OWNER TO`, `SET …`, `RESET …`) are
    /// recognized by their first keyword and refused with 0A000.
    fn parse_alter_view(&mut self) -> Result<AlterView> {
        self.expect_keyword("alter")?;
        self.expect_keyword("view")?;
        let if_exists = self.peek_keyword().as_deref() == Some("if");
        if if_exists {
            self.advance();
            self.expect_keyword("exists")?;
        }
        let (db, name) = self.parse_qualified_table_name()?;
        match self.peek_keyword().as_deref() {
            Some("rename") => {}
            Some("alter") | Some("owner") | Some("set") | Some("reset") => {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "this form of ALTER VIEW is not supported",
                ));
            }
            _ => return Err(syntax("expected RENAME after ALTER VIEW name")),
        }
        self.advance(); // RENAME
        let action = if self.peek_keyword().as_deref() == Some("to") {
            self.advance();
            AlterViewAction::RenameTo(self.expect_identifier()?)
        } else {
            if self.peek_keyword().as_deref() == Some("column") {
                self.advance();
            }
            let old = self.expect_identifier()?;
            self.expect_keyword("to")?;
            let new = self.expect_identifier()?;
            AlterViewAction::RenameColumn { old, new }
        };
        Ok(AlterView {
            name,
            db,
            if_exists,
            action,
        })
    }

    /// `DROP VIEW [IF EXISTS] [db.]name [, …] [CASCADE | RESTRICT]` (views.md §1). `IF EXISTS` is a
    /// two-token lookahead, so `DROP VIEW if` drops a view named `if`.
    fn parse_drop_view(&mut self) -> Result<DropView> {
        self.expect_keyword("drop")?;
        self.expect_keyword("view")?;
        let if_exists = self.peek_keyword().as_deref() == Some("if")
            && self.peek_keyword_at(1).as_deref() == Some("exists");
        if if_exists {
            self.advance(); // IF
            self.advance(); // EXISTS
        }
        let mut names = vec![self.parse_qualified_table_name()?];
        while matches!(self.peek(), Token::Comma) {
            self.advance();
            names.push(self.parse_qualified_table_name()?);
        }
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
        Ok(DropView {
            names,
            if_exists,
            cascade,
        })
    }

    /// Parse `ANALYZE qualified_table [(identifier [, identifier ...])]`.
    fn parse_analyze(&mut self) -> Result<Analyze> {
        self.expect_keyword("analyze")?;
        let (db, name) = self.parse_qualified_table_name()?;
        let mut columns = Vec::new();
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            loop {
                columns.push(self.expect_identifier()?);
                if !matches!(self.peek(), Token::Comma) {
                    break;
                }
                self.advance();
            }
            self.expect(&Token::RParen)?;
        }
        Ok(Analyze { name, db, columns })
    }

    /// `REINDEX [(option, …)] {INDEX | TABLE} [CONCURRENTLY] [db.]name` or `REINDEX [(option, …)]
    /// {DATABASE | SYSTEM} [CONCURRENTLY] [name]` (spec/design/indexes.md §10, grammar.md §59).
    /// Options are `VERBOSE [boolean]` and `CONCURRENTLY [boolean]`, repeatable (the last value
    /// wins, as PostgreSQL); an unknown option is 42601, `TABLESPACE` and `REINDEX SCHEMA` are
    /// 0A000 (jed has neither tablespaces nor schemas). The positional `CONCURRENTLY` is recognized
    /// only before a name for INDEX/TABLE, so a relation named `concurrently` stays nameable.
    fn parse_reindex(&mut self) -> Result<Reindex> {
        self.expect_keyword("reindex")?;
        let (mut concurrently, mut verbose) = (false, false);
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            loop {
                let name = self
                    .peek_keyword()
                    .ok_or_else(|| syntax("expected a REINDEX option"))?;
                self.advance();
                match name.as_str() {
                    "verbose" => verbose = self.parse_reindex_boolean(&name)?,
                    "concurrently" => concurrently = self.parse_reindex_boolean(&name)?,
                    "tablespace" => {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            "REINDEX TABLESPACE is not supported: jed has no tablespaces",
                        ));
                    }
                    _ => return Err(syntax(format!("unrecognized REINDEX option \"{name}\""))),
                }
                if matches!(self.peek(), Token::RParen) {
                    self.advance();
                    break;
                }
                if !matches!(self.peek(), Token::Comma) {
                    return Err(syntax("expected ',' or ')' in REINDEX option list"));
                }
                self.advance();
            }
        }
        let kind = match self.peek_keyword().as_deref() {
            Some("index") => ReindexKind::Index,
            Some("table") => ReindexKind::Table,
            Some("database") => ReindexKind::Database,
            Some("system") => ReindexKind::System,
            Some("schema") => {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "REINDEX SCHEMA is not supported: jed has no schemas",
                ));
            }
            _ => {
                return Err(syntax(
                    "expected INDEX, TABLE, SCHEMA, DATABASE, or SYSTEM after REINDEX",
                ));
            }
        };
        self.advance();
        let relation = matches!(kind, ReindexKind::Index | ReindexKind::Table);
        if self.peek_keyword().as_deref() == Some("concurrently")
            && (!relation || is_ident(self.peek_at(1)))
        {
            self.advance();
            concurrently = true;
        }
        let (db, name) = if relation {
            let (db, name) = self.parse_qualified_table_name()?;
            (db, Some(name))
        } else if is_ident(self.peek()) {
            (None, Some(self.expect_identifier()?))
        } else {
            (None, None)
        };
        Ok(Reindex {
            kind,
            db,
            name,
            concurrently,
            verbose,
        })
    }

    /// A REINDEX option's optional boolean value (PostgreSQL's `defGetBoolean`): omitted means
    /// TRUE; `TRUE`/`FALSE`/`ON`/`OFF` (bare or quoted) and the integers `1`/`0` are accepted;
    /// anything else is 42601.
    fn parse_reindex_boolean(&mut self, option: &str) -> Result<bool> {
        if matches!(self.peek(), Token::Comma | Token::RParen) {
            return Ok(true);
        }
        let value = match self.peek().clone() {
            Token::Word(w) | Token::Str(w) => match w.to_ascii_lowercase().as_str() {
                "true" | "on" => Some(true),
                "false" | "off" => Some(false),
                _ => None,
            },
            Token::Int(1) => Some(true),
            Token::Int(0) => Some(false),
            _ => None,
        };
        match value {
            Some(v) => {
                self.advance();
                Ok(v)
            }
            None => Err(syntax(format!("{option} requires a Boolean value"))),
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
        // `ROLLBACK [TRANSACTION|WORK] TO [SAVEPOINT] name` (transactions.md §11).
        if self.peek_keyword().as_deref() == Some("to") {
            self.advance(); // TO
            return Ok(Statement::Savepoint {
                op: SavepointOp::RollbackTo,
                name: self.parse_savepoint_name()?,
            });
        }
        Ok(Statement::Rollback)
    }

    /// The `[SAVEPOINT] name` tail of `RELEASE` / `ROLLBACK TO` (grammar.md §27). The noise word
    /// `SAVEPOINT` is consumed only when a name follows it, so `RELEASE savepoint` names a savepoint
    /// called `savepoint` (PostgreSQL's grammar).
    fn parse_savepoint_name(&mut self) -> Result<String> {
        if self.peek_keyword().as_deref() == Some("savepoint")
            && matches!(self.peek_at(1), Token::Word(_) | Token::QuotedIdent(_))
        {
            self.advance(); // SAVEPOINT
        }
        self.expect_identifier()
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
        let (mut analyze, mut verbose, mut costs, mut lane) = (false, false, true, false);
        let mut seen = std::collections::HashSet::new();
        let mut option_list = false;
        // A `(` opens the option list unless a parenthesized query follows (`EXPLAIN (SELECT 1)`,
        // grammar.md §25): an option is a plain word, never `(`, `SELECT`, `VALUES (`, or a `WITH`
        // clause.
        let paren_query = matches!(self.peek_at(1), Token::LParen)
            || self.is_select_at(self.pos + 1)
            || self.is_values_at(self.pos + 1)
            || self.is_with_clause_at(self.pos + 1);
        if matches!(self.peek(), Token::LParen) && !paren_query {
            option_list = true;
            self.advance();
            loop {
                let name = self
                    .peek_keyword()
                    .ok_or_else(|| syntax("expected an EXPLAIN option"))?;
                if !matches!(name.as_str(), "analyze" | "verbose" | "costs" | "lane") {
                    return Err(syntax(format!("unrecognized EXPLAIN option: {name}")));
                }
                self.advance();
                if !seen.insert(name.clone()) {
                    return Err(syntax(format!(
                        "EXPLAIN option specified more than once: {name}"
                    )));
                }
                let value = match self.peek_keyword().as_deref() {
                    Some("true") | Some("on") => {
                        self.advance();
                        true
                    }
                    Some("false") | Some("off") => {
                        self.advance();
                        false
                    }
                    _ => true,
                };
                match name.as_str() {
                    "analyze" => analyze = value,
                    "verbose" => verbose = value,
                    "costs" => costs = value,
                    "lane" => lane = value,
                    _ => unreachable!(),
                }
                if matches!(self.peek(), Token::RParen) {
                    self.advance();
                    break;
                }
                if !matches!(self.peek(), Token::Comma) {
                    return Err(syntax("expected ',' or ')' in EXPLAIN option list"));
                }
                self.advance();
            }
        }
        if self.peek_keyword().as_deref() == Some("analyze") {
            if option_list {
                return Err(syntax(
                    "cannot mix EXPLAIN option list with positional ANALYZE",
                ));
            }
            self.advance();
            analyze = true;
        }
        let inner = self.parse_explain_inner()?;
        Ok(Statement::Explain {
            analyze,
            verbose,
            costs,
            lane,
            inner: Box::new(inner),
        })
    }

    /// The statement EXPLAIN wraps — restricted to a query (`SELECT` / `VALUES` / `WITH` / a
    /// parenthesized query) or a DML statement (`INSERT` / `UPDATE` / `DELETE`). DDL, transaction
    /// control, and a nested `EXPLAIN` have no query plan to render and are rejected `42601`.
    fn parse_explain_inner(&mut self) -> Result<Statement> {
        match self.peek_keyword().as_deref() {
            Some("select") | Some("values") => self.parse_into_query(Self::parse_query_expr),
            Some("with") => self.parse_into_query(Self::parse_with_statement),
            Some("insert") => Ok(Statement::Insert(self.parse_insert()?)),
            Some("update") => Ok(Statement::Update(self.parse_update()?)),
            Some("delete") => Ok(Statement::Delete(self.parse_delete()?)),
            // `EXPLAIN CREATE TABLE … AS` (create-table-as.md §7); the column-definition form has no
            // plan to render.
            Some("create") if !self.at_create_view() => {
                let ct = self.parse_create_table()?;
                if ct.as_query.is_none() {
                    return Err(syntax("EXPLAIN does not support CREATE TABLE without AS"));
                }
                Ok(Statement::CreateTable(ct))
            }
            Some(other) => Err(syntax(format!("EXPLAIN does not support '{other}'"))),
            None if matches!(self.peek(), Token::LParen) => {
                self.parse_into_query(Self::parse_query_expr)
            }
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
        } else if self.peek_keyword().as_deref() == Some("unlogged") {
            return Err(unlogged_not_supported());
        }
        self.expect_keyword("table")?;
        // `IF NOT EXISTS` (grammar.md §13) is recognized by a two-token lookahead, so a lone `if`
        // stays an ordinary non-reserved table name: `CREATE TABLE if (...)` creates a table named
        // `if` (the DROP TABLE IF EXISTS precedent).
        let if_not_exists = if self.peek_keyword().as_deref() == Some("if")
            && self.peek_keyword_at(1).as_deref() == Some("not")
        {
            self.parse_if_not_exists()?
        } else {
            false
        };
        // An optional database qualifier `db.table` (attached-databases.md §3, Slice 1b): create the
        // table INTO the named database (`main` / `temp` / a host attachment). A bare name uses the
        // implicit scope. The `.` after the first identifier makes it the qualifier, the next the name.
        let (db, name) = self.parse_qualified_table_name()?;
        if self.at_create_table_as() {
            return self.parse_create_table_as(name, db, temp, if_not_exists);
        }
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
        let on_commit = self.parse_on_commit()?;
        Ok(CreateTable {
            name,
            temp,
            if_not_exists,
            columns,
            table_pks,
            checks,
            uniques,
            foreign_keys,
            excludes,
            db,
            on_commit,
            as_query: None,
        })
    }

    /// The rest of a `JSON_EXISTS` / `JSON_VALUE` / `JSON_QUERY` call after its keyword was peeked
    /// (json-sql-functions.md §5). Kept out of `parse_primary` (never inlined) so its several `Expr`
    /// temporaries do not enlarge the frame every parenthesis level of an expression recurses through
    /// (the MAX_EXPR_DEPTH native-stack bound, cost.md §7).
    #[inline(never)]
    fn parse_json_query_function(&mut self, kw: &str) -> Result<Expr> {
        self.advance(); // the function keyword
        self.advance(); // (
        let ctx = self.parse_expr()?;
        // `FORMAT JSON` after the context item is accepted (and ignored — a text/json/jsonb
        // context is coerced to jsonb regardless).
        self.skip_format_json();
        self.expect(&Token::Comma)?;
        let path = self.parse_expr()?;
        let passing = self.parse_json_passing()?;
        let expr = match kw {
            "json_exists" => {
                let on_error = self.parse_json_on_error_only()?;
                Expr::JsonExists {
                    ctx: Box::new(ctx),
                    path: Box::new(path),
                    passing,
                    on_error,
                }
            }
            "json_value" => {
                let returning = self.parse_json_returning()?;
                let (on_empty, on_error) = self.parse_json_on_clauses()?;
                Expr::JsonValue {
                    ctx: Box::new(ctx),
                    path: Box::new(path),
                    passing,
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
                    passing,
                    returning,
                    wrapper,
                    keep_quotes,
                    on_empty,
                    on_error,
                }
            }
        };
        self.expect(&Token::RParen)?;
        Ok(expr)
    }

    /// Whether the cursor, just past a CREATE TABLE's name, begins the `CREATE TABLE … AS` form
    /// (spec/design/create-table-as.md §1): the keyword `AS`, the keyword `ON` (an `ON COMMIT` clause
    /// precedes `AS`; the column-definition form needs `(` here), or a parenthesized list of bare
    /// column names followed by `AS` or `ON`. A column definition always has a type after its name,
    /// so `( name , … )` of single identifiers is never the column-definition form.
    fn at_create_table_as(&self) -> bool {
        let kw = |i: usize| matches!(self.peek_at(i), Token::Word(w) if w == "as" || w == "on");
        if kw(0) {
            return true;
        }
        if !matches!(self.peek_at(0), Token::LParen) {
            return false;
        }
        let mut i = 1;
        loop {
            if !matches!(self.peek_at(i), Token::Word(_) | Token::QuotedIdent(_)) {
                return false;
            }
            match self.peek_at(i + 1) {
                Token::Comma => i += 2,
                Token::RParen => return kw(i + 2),
                _ => return false,
            }
        }
    }

    /// The rest of `CREATE [TEMP] TABLE [IF NOT EXISTS] name [( col [, …] )] [ON COMMIT …] AS query
    /// [WITH [NO] DATA]` (spec/design/create-table-as.md §1, grammar.md §13). The query is a
    /// `subquery_expr`. A trailing `WITH [NO] DATA` is cut off before the query is parsed, as CREATE
    /// VIEW cuts off its check option: an unreserved `with` would otherwise read as a table alias.
    fn parse_create_table_as(
        &mut self,
        name: String,
        db: Option<String>,
        temp: bool,
        if_not_exists: bool,
    ) -> Result<CreateTable> {
        let column_names = if matches!(self.peek(), Token::LParen) {
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
        let on_commit = self.parse_on_commit()?;
        self.expect_keyword("as")?;
        let trailing = self.trailing_with_data();
        if let Some((at, _)) = trailing {
            self.tokens[at] = Token::Eof;
        }
        let query = self.parse_subquery()?;
        if let Some((at, _)) = trailing {
            if self.pos != at {
                return Err(syntax(format!(
                    "unexpected trailing input: {:?}",
                    self.peek()
                )));
            }
        }
        Ok(CreateTable {
            name,
            temp,
            if_not_exists,
            columns: Vec::new(),
            table_pks: Vec::new(),
            checks: Vec::new(),
            uniques: Vec::new(),
            foreign_keys: Vec::new(),
            excludes: Vec::new(),
            db,
            on_commit,
            as_query: Some(Box::new(CreateTableAs {
                column_names,
                query,
                with_data: trailing.is_none_or(|(_, with_data)| with_data),
            })),
        })
    }

    /// A trailing `WITH DATA` / `WITH NO DATA` ending the statement (spec/design/create-table-as.md
    /// §1): the token index of its `WITH` and whether the table is filled. `None` when absent.
    fn trailing_with_data(&self) -> Option<(usize, bool)> {
        let mut end = self.tokens.len();
        if matches!(self.tokens.last(), Some(Token::Eof)) {
            end -= 1;
        }
        let word = |i: usize, w: &str| matches!(self.tokens.get(i), Some(Token::Word(x)) if x == w);
        if end < self.pos + 2 || !word(end - 1, "data") {
            return None;
        }
        if word(end - 2, "with") {
            return Some((end - 2, true));
        }
        if end >= self.pos + 3 && word(end - 2, "no") && word(end - 3, "with") {
            return Some((end - 3, false));
        }
        None
    }

    /// Parse the optional trailing `ON COMMIT { PRESERVE ROWS | DELETE ROWS | DROP }` of a CREATE TABLE
    /// (grammar.ebnf `on_commit`, spec/design/temp-tables.md §5.1), `None` when absent. The words are
    /// recognized positionally after the closing parenthesis, so none is reserved.
    fn parse_on_commit(&mut self) -> Result<Option<crate::catalog::OnCommit>> {
        use crate::catalog::OnCommit;
        if self.peek_keyword().as_deref() != Some("on") {
            return Ok(None);
        }
        self.advance();
        self.expect_keyword("commit")?;
        let action = match self.peek_keyword().as_deref() {
            Some("preserve") => OnCommit::PreserveRows,
            Some("delete") => OnCommit::DeleteRows,
            Some("drop") => {
                self.advance();
                return Ok(Some(OnCommit::Drop));
            }
            _ => {
                return Err(syntax(
                    "expected PRESERVE ROWS, DELETE ROWS, or DROP after ON COMMIT",
                ));
            }
        };
        self.advance();
        self.expect_keyword("rows")?;
        Ok(Some(action))
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
        Ok(ForeignKeyDef {
            name,
            columns,
            ..self.parse_references_clause()?
        })
    }

    /// Parse a `references_clause` from the `REFERENCES` keyword onward (shared by the
    /// column-level and table-level forms — spec/design/grammar.md §43): the referenced table,
    /// an optional referenced-column list (`None` defaults to the parent's primary key), an
    /// optional `MATCH FULL | SIMPLE` (`MATCH PARTIAL` is 0A000, as in PostgreSQL), and the
    /// `ON DELETE` / `ON UPDATE` actions (each at most once, either order; a repeat is 42601).
    /// The returned definition carries no name and no local columns; the caller fills them in.
    fn parse_references_clause(&mut self) -> Result<ForeignKeyDef> {
        self.expect_keyword("references")?;
        let ref_table = self.expect_identifier()?;
        let ref_columns = if matches!(self.peek(), Token::LParen) {
            Some(self.parse_pk_column_list()?)
        } else {
            None
        };
        let mut match_full = false;
        if self.peek_keyword().as_deref() == Some("match") {
            self.advance();
            match self.peek_keyword().as_deref() {
                Some("full") => match_full = true,
                Some("simple") => {}
                Some("partial") => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "MATCH PARTIAL not yet implemented",
                    ));
                }
                _ => return Err(syntax("expected FULL, PARTIAL, or SIMPLE after MATCH")),
            }
            self.advance();
        }
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
        Ok(ForeignKeyDef {
            name: None,
            columns: Vec::new(),
            ref_table,
            ref_columns,
            match_full,
            on_delete,
            on_update,
        })
    }

    /// Parse one `referential_action` (spec/design/grammar.md §43). All five PG actions parse;
    /// All five PostgreSQL actions are resolved and persisted by CREATE/ALTER TABLE.
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

    /// Whether the cursor sits on a table-level PRIMARY KEY constraint, including the named form.
    /// ALTER TABLE uses this lookahead to keep the authoritative-but-deferred ADD PRIMARY KEY
    /// grammar on its 0A000 path instead of misclassifying `primary key` as a column name/type.
    fn at_primary_key_table_constraint(&self) -> bool {
        (self.peek_keyword().as_deref() == Some("primary")
            && self.peek_keyword_at(1).as_deref() == Some("key"))
            || (self.peek_keyword().as_deref() == Some("constraint")
                && self.peek_keyword_at(2).as_deref() == Some("primary")
                && self.peek_keyword_at(3).as_deref() == Some("key"))
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
        let base_type = self.expect_type_name()?;
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
                foreign_keys.push(ForeignKeyDef {
                    name: Some(cname),
                    columns: vec![name.clone()],
                    ..self.parse_references_clause()?
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
                    foreign_keys.push(ForeignKeyDef {
                        columns: vec![name.clone()],
                        ..self.parse_references_clause()?
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

    /// `CREATE [UNIQUE] INDEX [IF NOT EXISTS name | name] ON <table> ( col [, col]* )`
    /// (spec/design/grammar.md §30). The optional name needs one disambiguation because no word is reserved: the
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
        // `IF NOT EXISTS` (grammar.md §30) is recognized by a two-token lookahead, so a lone `if`
        // stays an ordinary index name (`CREATE INDEX if ON t (a)`). PostgreSQL requires a name
        // after it, so the unnamed lookahead below does not apply: `IF NOT EXISTS ON t (a)` is 42601.
        let if_not_exists = if self.peek_keyword().as_deref() == Some("if")
            && self.peek_keyword_at(1).as_deref() == Some("not")
        {
            self.parse_if_not_exists()?
        } else {
            false
        };
        // The unnamed form is `INDEX ON <table> [USING <method>] (` — the word after INDEX is the
        // index NAME unless it is `ON` followed by a word and then `(` OR `USING` (the three-token
        // lookahead, extended for the optional USING clause — grammar.md §30, gin.md §3). A §8
        // determinism surface, byte-identical across the parsers.
        let unnamed = !if_not_exists
            && self.peek_keyword().as_deref() == Some("on")
            && is_ident(self.peek_at(1))
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
            if_not_exists,
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
        } else if is_ident(self.peek()) && matches!(self.peek_at(1), Token::LParen) {
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
        if let Expr::Column(name, _) = &expr {
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
    fn skip_format_json(&mut self) -> bool {
        if self.peek_keyword().as_deref() == Some("format")
            && self.peek_keyword_at(1).as_deref() == Some("json")
        {
            self.advance(); // FORMAT
            self.advance(); // JSON
            return true;
        }
        false
    }

    /// Parse an optional `PASSING value [FORMAT JSON [ENCODING name]] AS name [, …]` clause of a SQL/JSON query
    /// function or `JSON_TABLE` (json-sql-functions.md §5.4) → its arguments (empty when absent).
    /// The name is any identifier, folded like one (`AS X` binds `$x`; `AS "X"` binds `$X`).
    fn parse_json_passing(&mut self) -> Result<Vec<JsonPassing>> {
        let mut passing = Vec::new();
        if self.peek_keyword().as_deref() != Some("passing") {
            return Ok(passing);
        }
        self.advance(); // PASSING
        loop {
            let expr = self.parse_expr()?;
            let format_json = self.skip_format_json();
            let encoding = if format_json && self.peek_keyword().as_deref() == Some("encoding") {
                self.advance(); // ENCODING
                Some(json_encoding_name(&self.expect_identifier()?)?)
            } else {
                None
            };
            self.expect_keyword("as")?;
            let name = self.expect_identifier()?;
            passing.push(JsonPassing {
                expr,
                format_json,
                encoding,
                name,
            });
            if !matches!(self.peek(), Token::Comma) {
                return Ok(passing);
            }
            self.advance();
        }
    }

    /// Parse an optional `RETURNING <type> [FORMAT JSON]` clause → the type name (resolved later).
    fn parse_json_returning(&mut self) -> Result<Option<String>> {
        if self.peek_keyword().as_deref() != Some("returning") {
            return Ok(None);
        }
        self.advance(); // RETURNING
        let ty = self.expect_type_name()?;
        self.skip_format_json();
        Ok(Some(ty))
    }

    /// Parse one constant SQL/JSON behavior word (`ERROR` / `NULL` / `TRUE` / `FALSE` / `UNKNOWN` /
    /// `EMPTY [ARRAY|OBJECT]`). `DEFAULT expr` is parsed by [`Self::parse_json_on_clauses`].
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
            Some("default") => Err(syntax(
                "invalid ON ERROR behavior: only ERROR, TRUE, FALSE, or UNKNOWN is allowed in ON ERROR for JSON_EXISTS()",
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
    /// A behavior is a constant word or `DEFAULT expr` (json-sql-functions.md §5.3); the expression
    /// ends at the `ON`.
    fn parse_json_on_clauses(&mut self) -> Result<(Option<JsonBehavior>, Option<JsonBehavior>)> {
        let mut on_empty = None;
        let mut on_error = None;
        if self.peek_keyword().as_deref() == Some("default") {
            let b = self.parse_json_default_behavior()?;
            if self.expect_on_clause()? == "empty" {
                on_empty = Some(b);
            } else {
                return Ok((None, Some(b)));
            }
        } else if self.is_json_behavior_start() && self.peek_on_clause_is("empty") {
            let kind = self.parse_json_behavior()?;
            self.advance(); // ON
            self.advance(); // EMPTY
            on_empty = Some(JsonBehavior {
                kind,
                default: None,
            });
        }
        if self.peek_keyword().as_deref() == Some("default") {
            let b = self.parse_json_default_behavior()?;
            if self.expect_on_clause()? == "empty" {
                return Err(syntax("ON EMPTY must come before ON ERROR"));
            }
            on_error = Some(b);
        } else if self.is_json_behavior_start() && self.peek_on_clause_is("error") {
            let kind = self.parse_json_behavior()?;
            self.advance(); // ON
            self.advance(); // ERROR
            on_error = Some(JsonBehavior {
                kind,
                default: None,
            });
        }
        Ok((on_empty, on_error))
    }

    /// Parse `DEFAULT expr` of an `ON EMPTY` / `ON ERROR` clause (the cursor at `DEFAULT`).
    fn parse_json_default_behavior(&mut self) -> Result<JsonBehavior> {
        self.advance(); // DEFAULT
        let expr = self.parse_expr()?;
        Ok(JsonBehavior {
            kind: JsonOnBehavior::Default,
            default: Some(Box::new(expr)),
        })
    }

    /// Consume `ON EMPTY` or `ON ERROR` after a `DEFAULT expr` behavior → `"empty"` / `"error"`.
    fn expect_on_clause(&mut self) -> Result<&'static str> {
        self.expect_keyword("on")?;
        match self.peek_keyword().as_deref() {
            Some("empty") => {
                self.advance();
                Ok("empty")
            }
            Some("error") => {
                self.advance();
                Ok("error")
            }
            _ => Err(syntax("expected EMPTY or ERROR after ON")),
        }
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
            let base_type = self.expect_type_name()?;
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
                _ => {
                    if !self.parse_one_seq_option(&mut seq)? {
                        break;
                    }
                }
            }
        }
        if parenthesized {
            self.expect(&Token::RParen)?;
        }
        Ok((seq, restart))
    }

    /// One order-free sequence option (`sequence_option` in grammar.ebnf) into `seq`, or `false`
    /// when the next token starts none. Shared by the CREATE/ALTER SEQUENCE and identity `( … )`
    /// option loops and by `ALTER TABLE … ALTER COLUMN … SET <sequence_option>` (alter.md §2.8).
    fn parse_one_seq_option(&mut self, seq: &mut SeqOptions) -> Result<bool> {
        match self.peek_keyword().as_deref() {
            // `AS <type>` — the sequence value type (order-free, S5 — sequences.md §14). The raw
            // type name is stored; it is resolved (and a non-integer type rejected 22023) at
            // execution. Inside an IDENTITY column's `( … )` a set `data_type` is 42601.
            Some("as") => {
                self.dup_check(seq.data_type.is_some(), "AS")?;
                self.advance();
                seq.data_type = Some(self.expect_type_name()?);
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
            // `SEQUENCE NAME name` — identity options only; the executor rejects it elsewhere
            // (42601, sequences.md §13.1).
            Some("sequence") if self.peek_keyword_at(1).as_deref() == Some("name") => {
                self.dup_check(seq.sequence_name.is_some(), "SEQUENCE NAME")?;
                self.advance();
                self.advance();
                seq.sequence_name = Some(self.expect_identifier()?);
            }
            _ => return Ok(false),
        }
        Ok(true)
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

    /// Parse ALTER TABLE's authoritative grammar frame (alter.md §1). Slices 1-5 execute the
    /// complete planned surface other than identity management.
    fn parse_alter_table(&mut self) -> Result<AlterTable> {
        self.expect_keyword("alter")?;
        self.expect_keyword("table")?;
        let if_exists = self.peek_keyword().as_deref() == Some("if");
        if if_exists {
            self.advance();
            self.expect_keyword("exists")?;
        }
        let (db, name) = self.parse_qualified_table_name()?;
        let action = if self.peek_keyword().as_deref() == Some("rename") {
            self.advance();
            match self.peek_keyword().as_deref() {
                Some("to") => {
                    self.advance();
                    AlterTableAction::RenameTable(self.expect_identifier()?)
                }
                Some("constraint") => {
                    self.advance();
                    let old = self.expect_identifier()?;
                    self.expect_keyword("to")?;
                    let new = self.expect_identifier()?;
                    AlterTableAction::RenameConstraint { old, new }
                }
                _ => {
                    if self.peek_keyword().as_deref() == Some("column") {
                        self.advance();
                    }
                    let old = self.expect_identifier()?;
                    self.expect_keyword("to")?;
                    let new = self.expect_identifier()?;
                    AlterTableAction::RenameColumn { old, new }
                }
            }
        } else {
            let mut actions = Vec::new();
            loop {
                match self.peek_keyword().as_deref() {
                    Some("add") => {
                        self.advance();
                        let column_noise = self.peek_keyword().as_deref() == Some("column");
                        if column_noise {
                            self.advance();
                        }
                        let if_not_exists = if self.peek_keyword().as_deref() == Some("if") {
                            self.advance();
                            self.expect_keyword("not")?;
                            self.expect_keyword("exists")?;
                            true
                        } else {
                            false
                        };
                        if column_noise
                            || if_not_exists
                            || !(self.at_check_constraint()
                                || self.at_unique_table_constraint()
                                || self.at_foreign_key_table_constraint()
                                || self.at_exclusion_table_constraint()
                                || self.at_primary_key_table_constraint())
                        {
                            let mut checks = Vec::new();
                            let mut uniques = Vec::new();
                            let mut foreign_keys = Vec::new();
                            let column = self.parse_column_def(
                                &name,
                                &mut checks,
                                &mut uniques,
                                &mut foreign_keys,
                            )?;
                            actions.push(AlterTableEdit::AddColumn {
                                column,
                                checks,
                                uniques,
                                foreign_keys,
                                if_not_exists,
                            });
                            if !matches!(self.peek(), Token::Comma) {
                                break;
                            }
                            self.advance();
                            continue;
                        }
                        if self.at_primary_key_table_constraint() {
                            if self.peek_keyword().as_deref() == Some("constraint") {
                                self.advance();
                                self.expect_identifier()?; // jed derives the PK handle.
                            }
                            self.expect_keyword("primary")?;
                            self.expect_keyword("key")?;
                            actions
                                .push(AlterTableEdit::AddPrimaryKey(self.parse_pk_column_list()?));
                            if !matches!(self.peek(), Token::Comma) {
                                break;
                            }
                            self.advance();
                            continue;
                        }
                        let def = if self.at_check_constraint() {
                            AlterConstraintDef::Check(self.parse_check_constraint()?)
                        } else if self.at_unique_table_constraint() {
                            AlterConstraintDef::Unique(self.parse_unique_table_constraint()?)
                        } else if self.at_foreign_key_table_constraint() {
                            AlterConstraintDef::ForeignKey(
                                self.parse_foreign_key_table_constraint()?,
                            )
                        } else if self.at_exclusion_table_constraint() {
                            AlterConstraintDef::Exclude(self.parse_exclusion_table_constraint()?)
                        } else {
                            unreachable!("constraint lookahead")
                        };
                        actions.push(AlterTableEdit::AddConstraint(def));
                    }
                    Some("drop") => {
                        self.advance();
                        if self.peek_keyword().as_deref() == Some("primary") {
                            self.advance();
                            self.expect_keyword("key")?;
                            let cascade = if self.peek_keyword().as_deref() == Some("cascade") {
                                self.advance();
                                true
                            } else {
                                if self.peek_keyword().as_deref() == Some("restrict") {
                                    self.advance();
                                }
                                false
                            };
                            actions.push(AlterTableEdit::DropPrimaryKey { cascade });
                            if !matches!(self.peek(), Token::Comma) {
                                break;
                            }
                            self.advance();
                            continue;
                        }
                        let constraint = self.peek_keyword().as_deref() == Some("constraint");
                        if constraint || self.peek_keyword().as_deref() == Some("column") {
                            self.advance();
                        }
                        let if_exists = if self.peek_keyword().as_deref() == Some("if") {
                            self.advance();
                            self.expect_keyword("exists")?;
                            true
                        } else {
                            false
                        };
                        let name = self.expect_identifier()?;
                        let cascade = if self.peek_keyword().as_deref() == Some("cascade") {
                            self.advance();
                            true
                        } else {
                            if self.peek_keyword().as_deref() == Some("restrict") {
                                self.advance();
                            }
                            false
                        };
                        actions.push(if constraint {
                            AlterTableEdit::DropConstraint {
                                name,
                                if_exists,
                                cascade,
                            }
                        } else {
                            AlterTableEdit::DropColumn {
                                name,
                                if_exists,
                                cascade,
                            }
                        });
                    }
                    _ => {
                        self.expect_keyword("alter")?;
                        if self.peek_keyword().as_deref() == Some("column") {
                            self.advance();
                        }
                        let column = self.expect_identifier()?;
                        let set_target = self.peek_keyword_at(1);
                        let kind = match self.peek_keyword().as_deref() {
                            // The identity option list (alter.md §2.8): `SET GENERATED …`,
                            // `SET <sequence_option>`, `RESTART …`, in any order.
                            Some("restart") => self.parse_alter_identity_options()?,
                            Some("set")
                                if !matches!(
                                    set_target.as_deref(),
                                    Some("default" | "data" | "not")
                                ) =>
                            {
                                self.parse_alter_identity_options()?
                            }
                            Some("add") => {
                                self.advance();
                                self.expect_keyword("generated")?;
                                let always = self.parse_generated_when()?;
                                self.expect_keyword("as")?;
                                self.expect_keyword("identity")?;
                                let options = if matches!(self.peek(), Token::LParen) {
                                    self.parse_sequence_options(true)?
                                } else {
                                    SeqOptions::default()
                                };
                                AlterColumnKind::AddIdentity(IdentitySpec { always, options })
                            }
                            Some("set") => {
                                self.advance();
                                if self.peek_keyword().as_deref() == Some("default") {
                                    self.advance();
                                    let start = self.pos;
                                    let expr = self.parse_expr()?;
                                    let text = render_tokens(&self.tokens[start..self.pos]);
                                    AlterColumnKind::SetDefault(DefaultDef { expr, text })
                                } else if self.peek_keyword().as_deref() == Some("data") {
                                    self.advance();
                                    self.expect_keyword("type")?;
                                    self.parse_alter_column_type()?
                                } else {
                                    self.expect_keyword("not")?;
                                    self.expect_keyword("null")?;
                                    AlterColumnKind::SetNotNull
                                }
                            }
                            Some("drop") => {
                                self.advance();
                                if self.peek_keyword().as_deref() == Some("default") {
                                    self.advance();
                                    AlterColumnKind::DropDefault
                                } else if self.peek_keyword().as_deref() == Some("identity") {
                                    self.advance();
                                    let if_exists = if self.peek_keyword().as_deref() == Some("if")
                                    {
                                        self.advance();
                                        self.expect_keyword("exists")?;
                                        true
                                    } else {
                                        false
                                    };
                                    AlterColumnKind::DropIdentity { if_exists }
                                } else {
                                    self.expect_keyword("not")?;
                                    self.expect_keyword("null")?;
                                    AlterColumnKind::DropNotNull
                                }
                            }
                            Some("type") => {
                                self.advance();
                                self.parse_alter_column_type()?
                            }
                            other => {
                                return Err(syntax(format!(
                                    "expected SET or DROP, found {other:?}"
                                )));
                            }
                        };
                        actions.push(AlterTableEdit::AlterColumn(AlterColumnAction {
                            column,
                            action: kind,
                        }));
                    }
                }
                if !matches!(self.peek(), Token::Comma) {
                    break;
                }
                self.advance();
            }
            AlterTableAction::Actions(actions)
        };
        Ok(AlterTable {
            name,
            db,
            if_exists,
            action,
        })
    }

    /// `ALWAYS` (true) or `BY DEFAULT` (false) after `GENERATED`.
    fn parse_generated_when(&mut self) -> Result<bool> {
        match self.peek_keyword().as_deref() {
            Some("always") => {
                self.advance();
                Ok(true)
            }
            Some("by") => {
                self.advance();
                self.expect_keyword("default")?;
                Ok(false)
            }
            other => Err(syntax(format!(
                "expected ALWAYS or BY DEFAULT after GENERATED, found {other:?}"
            ))),
        }
    }

    /// The identity alteration list of `ALTER [COLUMN] c` (alter.md §2.8, PostgreSQL's
    /// `alter_identity_column_option_list`): one or more of `SET GENERATED { ALWAYS | BY DEFAULT }`,
    /// `SET <sequence_option>`, and `RESTART [[WITH] n]`, order-free, each at most once (42601).
    /// `SET AS`, `SET RESTART`, and `SET SEQUENCE NAME` are 42601 ("not supported here").
    fn parse_alter_identity_options(&mut self) -> Result<AlterColumnKind> {
        let mut generated: Option<bool> = None;
        let mut options = SeqOptions::default();
        let mut restart: Option<Option<i64>> = None;
        loop {
            match self.peek_keyword().as_deref() {
                Some("restart") => {
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
                Some("set")
                    if !matches!(
                        self.peek_keyword_at(1).as_deref(),
                        Some("default" | "data" | "not")
                    ) =>
                {
                    self.advance();
                    match self.peek_keyword().as_deref() {
                        Some("generated") => {
                            self.dup_check(generated.is_some(), "SET GENERATED")?;
                            self.advance();
                            generated = Some(self.parse_generated_when()?);
                        }
                        Some(opt @ ("as" | "restart")) => {
                            return Err(syntax(format!(
                                "sequence option \"{opt}\" not supported here"
                            )));
                        }
                        Some("sequence") => {
                            return Err(syntax("invalid sequence option SEQUENCE NAME"));
                        }
                        other => {
                            if !self.parse_one_seq_option(&mut options)? {
                                return Err(syntax(format!(
                                    "expected an identity option after SET, found {other:?}"
                                )));
                            }
                        }
                    }
                }
                _ => break,
            }
        }
        Ok(AlterColumnKind::SetIdentity {
            generated,
            options,
            restart,
        })
    }

    fn parse_alter_column_type(&mut self) -> Result<AlterColumnKind> {
        let base = self.expect_type_name()?;
        let type_mod = self.parse_type_mod()?;
        let type_name = if self.consume_array_brackets()? {
            format!("{base}[]")
        } else {
            base
        };
        let using = if self.peek_keyword().as_deref() == Some("using") {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(AlterColumnKind::SetType {
            type_name,
            type_mod,
            using,
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
        let table_pos = self.here();
        let (db, table) = self.parse_qualified_table_name()?;
        // Optional target alias `AS alias` (spec/design/upsert.md §5). PostgreSQL requires the `AS`
        // here (no implicit alias), which also keeps a column list / source keyword unambiguous.
        let alias = if self.peek_keyword().as_deref() == Some("as") {
            self.advance();
            Some(self.expect_identifier()?)
        } else {
            None
        };

        // Optional column list `( col [, col]* )` before VALUES. An empty `()` is rejected
        // (the first `expect_identifier` errors 42601 on `)`). A `(` that begins a parenthesized
        // query source (`INSERT INTO t (SELECT …) UNION …` — grammar.md §24) is not a column list.
        let mut column_pos = Vec::new();
        let mut columns = if matches!(self.peek(), Token::LParen) && !self.at_insert_paren_query() {
            self.advance(); // '('
            let mut names = Vec::new();
            loop {
                column_pos.push(self.here());
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

        // The source is DEFAULT VALUES, a query (§24), or a VALUES list. DEFAULT VALUES is the
        // all-columns-omitted form, represented through the existing column-list machinery as one
        // empty row mapped by an empty synthetic list. PostgreSQL does not combine it with an
        // explicit column list or OVERRIDING.
        let source = if self.peek_keyword().as_deref() == Some("default") {
            if columns.is_some() || overriding.is_some() {
                return Err(syntax(
                    "DEFAULT VALUES cannot be combined with a column list or OVERRIDING",
                ));
            }
            self.advance();
            self.expect_keyword("values")?;
            columns = Some(Vec::new());
            InsertSource::Values(vec![Vec::new()])
        } else if let Some(rows) = self.parse_insert_values_source()? {
            InsertSource::Values(rows)
        } else if self.is_select_at(self.pos)
            || self.is_values_at(self.pos)
            || self.at_with_clause()
            || matches!(self.peek(), Token::LParen)
        {
            // A query source (§24): a `SELECT`, a `VALUES` query, a set operation, a parenthesized
            // query, or a `WITH`-prefixed query. After the column list a `(` can only begin a query.
            InsertSource::Query(Box::new(self.parse_insert_query()?))
        } else {
            // Neither a VALUES list nor a query: the error names the expected VALUES keyword (a
            // `VALUES` here was already parsed above, so this always fails).
            self.expect_keyword("values")?;
            return Err(syntax("expected VALUES or a query"));
        };
        let on_conflict = self.parse_on_conflict()?;
        let returning = self.parse_returning()?;
        Ok(Insert {
            table,
            table_pos,
            db,
            alias,
            columns,
            column_pos,
            overriding,
            source,
            on_conflict,
            returning,
        })
    }

    /// The plain `VALUES` source of an INSERT (grammar.md §12/§24): `VALUES row [, row]*`, optionally
    /// wrapped in any number of parentheses (`INSERT INTO t (VALUES …)` is the same source —
    /// PostgreSQL), whose slots take `DEFAULT` and adapt to their target columns. It is that source
    /// only when the list stands alone: a set operator or a trailing `ORDER BY` / `LIMIT` / `OFFSET`
    /// after it (inside or outside the parentheses) makes the whole source a query whose operand is
    /// a `VALUES` query, so this rewinds and returns `None` for `parse_insert_query`. `None` too when
    /// no `VALUES` list begins here. Each parenthesis is one nesting level, as for a query.
    fn parse_insert_values_source(&mut self) -> Result<Option<Vec<Vec<InsertValue>>>> {
        let (start, depth, furthest) = (self.pos, self.depth, self.furthest.get());
        let mut parens = 0;
        while matches!(self.tokens.get(self.pos + parens), Some(Token::LParen)) {
            parens += 1;
        }
        if self.peek_keyword_at(parens).as_deref() != Some("values") {
            return Ok(None);
        }
        for _ in 0..parens {
            self.advance(); // '('
            self.deepen()?;
        }
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
        // A set operator or ORDER/LIMIT/OFFSET (a query-follow token other than `)`) after the list
        // or after one of its closing parentheses makes it a query operand.
        let continues_query =
            |p: &Self| p.is_query_follow_at(p.pos) && !matches!(p.peek(), Token::RParen);
        let mut plain = true;
        for _ in 0..parens {
            if continues_query(self) {
                plain = false;
                break;
            }
            self.expect(&Token::RParen)?;
        }
        if plain && !continues_query(self) {
            self.depth = depth;
            return Ok(Some(rows));
        }
        // Rewind, forgetting what the abandoned parse examined so a later error is positioned by
        // the re-parse (error-fields.md §10.3).
        self.pos = start;
        self.depth = depth;
        self.furthest.set(furthest);
        Ok(None)
    }

    /// Whether the `(` at the current position begins a parenthesized query source rather than an
    /// INSERT column list (grammar.md §24). A column list holds only identifiers, so the `(` begins
    /// a query when it is followed by another `(`, by a `WITH` clause, or by a bare `SELECT` that is
    /// not itself a listed column name (`(select)` / `(select, b)` stay column lists — keywords are
    /// not reserved, §3). A §8 cross-core surface.
    fn at_insert_paren_query(&self) -> bool {
        let next = self.pos + 1;
        match self.tokens.get(next) {
            Some(Token::LParen) => true,
            _ if self.is_with_clause_at(next) || self.is_values_at(next) => true,
            _ if self.is_select_at(next) => !matches!(
                self.tokens.get(next + 1),
                Some(Token::Comma) | Some(Token::RParen)
            ),
            _ => false,
        }
    }

    /// An INSERT's query source (grammar.md §24): a `subquery_expr` at the statement top level — a
    /// `WITH`-prefixed query, or a set expression (whose operands may be parenthesized queries)
    /// with an optional trailing `ORDER BY` / `LIMIT` / `OFFSET` folded onto it. A lone `SELECT`
    /// folds back to `QueryExpr::Select`, exactly the pre-set-operations source.
    fn parse_insert_query(&mut self) -> Result<QueryExpr> {
        if self.at_with_clause() {
            self.parse_with_query_expr()
        } else {
            self.parse_query_expr_node()
        }
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

        // Optional conflict target: a `( col, … )` column list (with an optional `WHERE
        // index_predicate`) or `ON CONSTRAINT name`.
        let mut index_predicate = None;
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
            index_predicate = self.parse_optional_where()?;
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
                    let pos = self.here();
                    let column = self.expect_identifier()?;
                    self.expect(&Token::Eq)?;
                    let (is_default, value) = self.parse_assignment_rhs()?;
                    assignments.push(Assignment {
                        column,
                        pos,
                        is_default,
                        value,
                    });
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
        Ok(Some(OnConflict {
            target,
            index_predicate,
            action,
        }))
    }

    /// One parenthesized `( <value> [, <value>]* )` row of an INSERT.
    fn parse_insert_row(&mut self) -> Result<Vec<InsertValue>> {
        self.expect(&Token::LParen)?;
        let mut values = Vec::new();
        loop {
            values.push(self.parse_insert_slot()?);
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

    /// One top-level INSERT value slot (spec/design/grammar.md §16). The literal-shaped forms
    /// (`DEFAULT`, a literal, `$N`, a literal-field `ROW(…)` / `ARRAY[…]`) keep their contextual
    /// adaptation to the target column when the slot is exactly that form — the next token closes
    /// the slot. Anything else (`1 + 1`, `now()`, `CASE …`, `(SELECT …)`, a constructor with an
    /// expression field) re-parses from the slot's start as a general expression. A non-syntax
    /// error from the literal form (an out-of-range integer, 22003) is the slot's own error.
    fn parse_insert_slot(&mut self) -> Result<InsertValue> {
        let (pos, depth) = (self.pos, self.depth);
        match self.parse_insert_value() {
            Ok(v) if matches!(self.peek(), Token::Comma | Token::RParen) => return Ok(v),
            Err(e) if e.state != SqlState::SyntaxError => return Err(e),
            _ => {}
        }
        self.pos = pos;
        self.depth = depth;
        Ok(InsertValue::Expr(Box::new(self.parse_expr()?)))
    }

    /// One literal-shaped INSERT value: the `DEFAULT` keyword (not reserved — §3), a `ROW(…)` composite
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
            // Each constructor level is one nesting level (MAX_EXPR_DEPTH, cost.md §7).
            self.deepen()?;
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
            self.undeepen();
            Ok(InsertValue::Row(fields))
        } else if self.peek_keyword().as_deref() == Some("array")
            && matches!(self.peek_at(1), Token::LBracket)
        {
            // ARRAY[elem, …] — recurse on each element (a literal or a `$N`).
            self.deepen()?;
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
            self.undeepen();
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
    /// folded here), or one of the keywords `NULL` / `TRUE` / `FALSE`
    /// (spec/grammar/grammar.ebnf `literal`). A slot that is not exactly a literal is a
    /// general expression (`parse_insert_slot`).
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
            // A parenthesized `(WITH … query)` statement is the top-level WITH query itself
            // (PostgreSQL: the parentheses add nothing — grammar.md §25).
            QueryExpr::With(we) => Statement::With(WithQuery {
                ctes: we.ctes,
                body: CteBody::Query(*we.body),
                recursive: we.recursive,
            }),
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
        fold_trailing(node, order_by, limit, offset)
    }

    /// `query_statement ::= with_clause? query_expr` — a top-level query prefixed by a `WITH`
    /// clause defining common table expressions (spec/design/cte.md). `WITH RECURSIVE`
    /// (spec/design/recursive-cte.md) sets the `recursive` flag and lets a CTE reference itself.
    /// Parenthesized CTE query bodies are `subquery_expr`s and may carry a nested `WITH` (cte.md §7);
    /// the top-level primary is unprefixed, so a bare second `WITH` remains invalid.
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
        // INSERT/UPDATE/DELETE keyword selects it, otherwise an unprefixed query_expr. A
        // data-modifying primary's SELECT is another statement's query, where `INTO` is not allowed.
        if matches!(
            self.peek_keyword().as_deref(),
            Some("insert") | Some("update") | Some("delete")
        ) {
            self.into_ok = false;
            self.into_top = false;
        }
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
    /// statement). A parenthesized query body uses `subquery_expr` and may carry a nested `WITH`;
    /// the top-level primary uses an unprefixed `query_expr`.
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
            let body = self.parse_query_expr_node()?;
            reject_second_with(&body)?;
            Ok(CteBody::Query(body))
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
        // A CTE body's SELECT never takes the statement's `INTO` (create-table-as.md §6).
        let into_ok = std::mem::replace(&mut self.into_ok, false);
        self.into_nest += 1;
        let body = self.parse_cte_body(true)?;
        self.into_nest -= 1;
        self.into_ok = into_ok;
        self.expect(&Token::RParen)?;
        let search = if self.peek_keyword().as_deref() == Some("search") {
            Some(self.parse_search_clause()?)
        } else {
            None
        };
        let cycle = if self.peek_keyword().as_deref() == Some("cycle") {
            Some(Box::new(self.parse_cycle_clause()?))
        } else {
            None
        };
        Ok(Cte {
            name,
            columns,
            materialized,
            body,
            search,
            cycle,
        })
    }

    /// `SEARCH { DEPTH | BREADTH } FIRST BY col, … SET seq_col` (spec/design/recursive-cte.md §9).
    fn parse_search_clause(&mut self) -> Result<SearchClause> {
        let pos = self.here();
        self.expect_keyword("search")?;
        let breadth_first = match self.peek_keyword().as_deref() {
            Some("depth") => false,
            Some("breadth") => true,
            _ => return Err(syntax("expected DEPTH or BREADTH after SEARCH".to_string())),
        };
        self.advance();
        self.expect_keyword("first")?;
        self.expect_keyword("by")?;
        let columns = self.parse_identifier_list()?;
        self.expect_keyword("set")?;
        let set_column = self.expect_identifier()?;
        Ok(SearchClause {
            breadth_first,
            columns,
            set_column,
            pos,
        })
    }

    /// `CYCLE col, … SET mark_col [TO value DEFAULT default] USING path_col`
    /// (spec/design/recursive-cte.md §9).
    fn parse_cycle_clause(&mut self) -> Result<CycleClause> {
        let pos = self.here();
        self.expect_keyword("cycle")?;
        let columns = self.parse_identifier_list()?;
        self.expect_keyword("set")?;
        let mark_column = self.expect_identifier()?;
        let (mark_value, mark_default) = if self.peek_keyword().as_deref() == Some("to") {
            self.advance();
            let value = self.parse_cycle_mark_constant()?;
            self.expect_keyword("default")?;
            let default = self.parse_cycle_mark_constant()?;
            (Some(value), Some(default))
        } else {
            (None, None)
        };
        self.expect_keyword("using")?;
        let path_column = self.expect_identifier()?;
        Ok(CycleClause {
            columns,
            mark_column,
            mark_value,
            mark_default,
            path_column,
            pos,
        })
    }

    /// A cycle mark constant (PostgreSQL's `AexprConst`): a number, string, `TRUE` / `FALSE` /
    /// `NULL`, or a typed string literal (`date '…'`). A sign is not part of a constant, so `-1` is
    /// a syntax error, as in PostgreSQL.
    fn parse_cycle_mark_constant(&mut self) -> Result<Expr> {
        if matches!(self.peek(), Token::Minus | Token::LParen) {
            return Err(syntax(format!(
                "expected a constant in a CYCLE clause, found {:?}",
                self.peek()
            )));
        }
        let e = self.parse_primary()?;
        match e {
            Expr::Literal(..) | Expr::TypedLiteral { .. } => Ok(e),
            _ => Err(syntax("expected a constant in a CYCLE clause".to_string())),
        }
    }

    /// A comma-separated list of one or more identifiers.
    fn parse_identifier_list(&mut self) -> Result<Vec<String>> {
        let mut names = vec![self.expect_identifier()?];
        while matches!(self.peek(), Token::Comma) {
            self.advance();
            names.push(self.expect_identifier()?);
        }
        Ok(names)
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
        fold_trailing(node, order_by, limit, offset)
    }

    /// Whether a `SELECT` keyword sits at token index `idx`.
    fn is_select_at(&self, idx: usize) -> bool {
        matches!(self.tokens.get(idx), Some(Token::Word(w)) if w.eq_ignore_ascii_case("select"))
    }

    /// Whether a `VALUES` list (`VALUES (`) begins at token index `idx` (grammar.md §25) — the
    /// `VALUES` keyword followed by a row's `(`, so a column named `values` stays an expression.
    fn is_values_at(&self, idx: usize) -> bool {
        matches!(self.tokens.get(idx), Some(Token::Word(w)) if w == "values")
            && matches!(self.tokens.get(idx + 1), Some(Token::LParen))
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
            Some(Token::Word(w)) if w == "recursive" => true,
            Some(t) if is_ident(t) => {
                matches!(self.tokens.get(idx + 2), Some(Token::LParen))
                    || matches!(self.tokens.get(idx + 2), Some(Token::Word(w)) if w.eq_ignore_ascii_case("as"))
            }
            _ => false,
        }
    }

    /// Whether a query expression — a `SELECT`, a `VALUES` list, a nested `WITH` clause (cte.md §7),
    /// or a parenthesized query (grammar.md §25) — begins at token index `idx`. The §26
    /// leading-`SELECT` lookahead, extended with `VALUES (` and `WITH`, and with a run of `(`s that
    /// ends at a `SELECT`/`VALUES (`/`WITH` where EVERY `(` in the run is closed by a `)` followed by
    /// a token that can only continue a query — `UNION`/`INTERSECT`/`EXCEPT`/`ORDER`/`LIMIT`/`OFFSET`
    /// or a further `)`. Thus
    /// `((SELECT 1) UNION SELECT 2)` and `((SELECT 1))` are subqueries but `((SELECT 1) + 1)` and
    /// `((SELECT 1), 2)` are expressions. Precomputed (`compute_paren_query`), so a long run of `(`
    /// can neither exhaust the stack nor cost time per probe; a §8 cross-core surface.
    fn is_query_start_at(&self, idx: usize) -> bool {
        self.is_select_at(idx)
            || self.is_values_at(idx)
            || self.is_with_clause_at(idx)
            || self.paren_query.get(idx).copied().unwrap_or(false)
    }

    /// Whether the token at `idx` may follow a parenthesized query but never a parenthesized
    /// expression: a set operator, a trailing `ORDER`/`LIMIT`/`OFFSET`, or a closing `)`.
    fn is_query_follow_at(&self, idx: usize) -> bool {
        match self.tokens.get(idx) {
            Some(Token::RParen) => true,
            Some(Token::Word(w)) => matches!(
                w.to_ascii_lowercase().as_str(),
                "union" | "intersect" | "except" | "order" | "limit" | "offset"
            ),
            _ => false,
        }
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
        reject_second_with(&body)?;
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
        let mut left = self.parse_set_operand()?;
        while self.peek_keyword().as_deref() == Some("intersect") {
            self.deepen()?; // each chained INTERSECT is one more set-op nesting level
            self.advance(); // INTERSECT
            let all = self.parse_setop_quantifier();
            let right = self.parse_set_operand()?;
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

    /// A set-operation operand (grammar.md §25): a `select_core`, a `VALUES` list, or a
    /// parenthesized query — a full `subquery_expr` (its own `WITH`, set operations, and trailing
    /// `ORDER BY`/`LIMIT`/`OFFSET`) between `(` and `)`, one nesting level deeper. In operand
    /// position a `(` can only begin a query, so no lookahead is needed. A `VALUES` operand is the
    /// query `SELECT * FROM (VALUES …)` over a `*VALUES*` relation (`values_query`), so its
    /// columns are `column1`, `column2`, … and a trailing `ORDER BY` folds onto it like a `SELECT`'s.
    fn parse_set_operand(&mut self) -> Result<QueryExpr> {
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            let q = self.parse_subquery()?;
            self.expect(&Token::RParen)?;
            return Ok(q);
        }
        if self.peek_keyword().as_deref() == Some("values") {
            // A VALUES operand is no SELECT, so a later arm cannot take the statement's `INTO`.
            self.into_ok = false;
            let rows = self.parse_values_body()?;
            return Ok(values_query(rows, Vec::new(), None, None));
        }
        Ok(QueryExpr::Select(self.parse_select_core()?))
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

    /// `select_core ::= "SELECT" "DISTINCT"? select_items ("FROM" from_clause)? where?
    /// group_by? having?` — a `SELECT` WITHOUT a trailing `ORDER BY`/`LIMIT`/`OFFSET` (the
    /// operand form of a set operation). The returned `Select` has empty `order_by` and no
    /// `limit`/`offset`. The FROM clause is optional: with no `from` keyword the SELECT is
    /// FROM-less — one virtual zero-column row (spec/design/grammar.md §34).
    #[inline(never)]
    fn parse_select_core(&mut self) -> Result<Box<Select>> {
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

        // The SELECT is filled in place on the heap, so the clauses below — each of which may
        // recurse into a subquery — share a small frame (spec/design/cost.md §7.1). Only the
        // statement's leftmost SELECT may take an `INTO` (create-table-as.md §6), so this one owns
        // it and every SELECT nested in its clauses does not.
        let owner = std::mem::replace(&mut self.into_ok, false);
        self.into_nest += 1;
        let mut sel = empty_select(distinct);
        outline(|| -> Result<()> {
            sel.items = self.parse_select_items()?;
            Ok(())
        })?;
        if self.peek_keyword().as_deref() == Some("into") {
            self.parse_select_into(owner)?;
        }
        if self.peek_keyword().as_deref() == Some("from") {
            self.advance(); // FROM
            let (f, j) = self.parse_from_clause()?;
            sel.from = Some(f);
            sel.joins = j;
        }
        outline(|| -> Result<()> {
            sel.filter = self.parse_optional_where()?;

            sel.group_by = self.parse_group_by()?;

            sel.having = self.parse_having()?;

            // WINDOW name AS ( definition ) (, …) — named windows referenced by OVER name (window.md §5).
            sel.windows = self.parse_window_clause()?;
            Ok(())
        })?;
        self.into_nest -= 1;
        Ok(sel)
    }

    /// `window_clause ::= "WINDOW" identifier "AS" "(" window_definition ")" ("," …)*` (window.md
    /// §5). Each entry is a full inline window definition (a base-window reference inside the
    /// definition is deferred). Empty when no WINDOW keyword is present. WINDOW is non-reserved.
    #[inline(never)]
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
                w.as_str(),
                "partition" | "order" | "rows" | "range" | "groups"
            ),
            // A quoted name is never a keyword (grammar.md §3).
            Token::QuotedIdent(_) => true,
            _ => false,
        };
        if is_base {
            if let Token::Word(w) | Token::QuotedIdent(w) = self.advance() {
                return Some(w);
            }
        }
        None
    }

    /// `having_clause ::= "HAVING" expr` (grammar.md §19), after GROUP BY and before ORDER BY.
    /// `HAVING` is not reserved, so it is a clause only in this position; the predicate is a
    /// general expression (it may reference aggregates) checked for boolean at resolve.
    #[inline(never)]
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
    #[inline(never)]
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
    #[inline(never)]
    fn parse_from_clause(&mut self) -> Result<(TableRef, Vec<JoinClause>)> {
        // The first item in a small frame; the join/comma loop in its own (cost.md §7.1).
        let first = self.parse_table_ref();
        self.parse_from_clause_tail(first)
    }

    #[inline(never)]
    fn parse_from_clause_tail(
        &mut self,
        first: Result<TableRef>,
    ) -> Result<(TableRef, Vec<JoinClause>)> {
        let first = first?;
        // A leading unaliased parenthesized join is the same left-deep chain without its parentheses.
        let (from, mut joins) = splice_leading_group(first, Vec::new());
        // Where the current comma item's `comma` join sits in `joins` (none for the first item).
        let mut segment: Option<usize> = None;
        loop {
            while let Some(j) = self.parse_join_clause()? {
                joins.push(j);
            }
            if let Some(seg) = segment {
                group_outer_join_item(&mut joins, seg);
            }
            // Comma-FROM (grammar.md §15): `FROM a, b` is an implicit `CROSS JOIN`. The comma
            // separates top-level FROM items, each of which is its own join sub-chain; it binds
            // LOOSER than `JOIN`, so the new item begins a fresh ON-resolution segment (recorded
            // by `comma: true`). After the comma's table_ref, the inner loop picks up any joins of
            // the new item (`a, b JOIN c ON …`) before the next comma is considered.
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                let item = self.parse_table_ref()?;
                // An unaliased parenthesized join after a comma starts the new segment the same way.
                let (table, group_joins) = splice_leading_group(item, Vec::new());
                segment = Some(joins.len());
                joins.push(JoinClause {
                    kind: JoinKind::Cross,
                    table,
                    on: None,
                    using: None,
                    natural: false,
                    comma: true,
                });
                joins.extend(group_joins);
                continue;
            }
            break;
        }
        Ok((from, joins))
    }

    /// Parse the inside of a parenthesized join `"(" table_ref join_clause+ ")"`
    /// (spec/design/grammar.md §15) after its `(`, stopping before the `)`. The group may not contain
    /// a comma, and must contain at least one join (`FROM (a)` is 42601, as in PostgreSQL). Its body
    /// is a `SELECT *` over the chain; the caller marks it `join_group` when no alias follows.
    #[inline(never)]
    fn parse_join_group(&mut self) -> Result<QueryExpr> {
        self.deepen()?;
        let first = self.parse_table_ref()?;
        let (from, mut joins) = splice_leading_group(first, Vec::new());
        while let Some(j) = self.parse_join_clause()? {
            joins.push(j);
        }
        // `((a JOIN b ON p))` is a join (the inner group's chain); `(a)` is not.
        if joins.is_empty() {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "a parenthesized FROM item must be a subquery, a VALUES list, or a join",
            ));
        }
        self.undeepen();
        Ok(join_group_body(from, joins))
    }

    /// `table_ref ::= derived_table derived_alias? | joined_group derived_alias? | table_function
    /// table_function_alias? | qualified_table ("AS"? identifier)?` (grammar.md §15/§35/§42). A `(`
    /// at the START of a table_ref, when `SELECT`/`WITH`/`VALUES` follows, begins a DERIVED TABLE — a
    /// parenthesized subquery used as a relation, `FROM (SELECT …) AS t` (§42); any other leading `(`
    /// begins a PARENTHESIZED JOIN `(a JOIN b ON …)` (§15). Otherwise it is a base table name OR a
    /// set-returning function call
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
                || (is_ident(self.peek_at(1)) && matches!(self.peek_at(2), Token::LParen)));
        if lateral {
            self.advance();
        }
        if matches!(self.peek(), Token::LParen) {
            let mut tr = self.parse_derived_table(lateral);
            if let Ok(tr) = &mut tr {
                tr.lateral = lateral;
            }
            return tr;
        }
        // `JSON_TABLE(ctx, path [AS n] COLUMNS (…))` — a table source (json-table.md §3, T1),
        // recognized by the keyword followed by `(`.
        if self.peek_keyword().as_deref() == Some("json_table")
            && matches!(self.peek_at(1), Token::LParen)
        {
            return self.parse_json_table();
        }
        self.parse_named_table_ref(lateral)
    }

    /// A base table, view, or table function reference — the non-derived `table_ref` forms — in its
    /// own frame, off the derived-table recursion (spec/design/cost.md §7.1).
    #[inline(never)]
    fn parse_named_table_ref(&mut self, lateral: bool) -> Result<TableRef> {
        let mut name_pos = self.pos;
        let src = self.here();
        let mut name = self.expect_identifier()?;
        // An optional DATABASE qualifier `db "." table` (attached-databases.md §3): a `.` after the
        // first identifier makes it the database qualifier and the next identifier the table name. A
        // qualified name is a BASE TABLE only — never a set-returning function (no cross-database SRF)
        // — so the function `(` branch below is guarded off when a qualifier is present. Mirrors
        // `parse_column_ref`'s one-`.`-lookahead, the only dotted-name precedent.
        let db = if matches!(self.peek(), Token::Dot) {
            self.advance(); // .
            name_pos = self.pos;
            let table = self.expect_identifier()?;
            let qualifier = name;
            name = table;
            Some(qualifier)
        } else {
            None
        };
        // A `(` right after the name = a set-returning function call (no `*`/`DISTINCT` — those
        // are aggregate/star forms, not an SRF argument list).
        // An argument may be named (`name => value`, grammar.md §17); `arg_names` is `None` when
        // every argument is positional.
        let mut arg_names = None;
        let args = if matches!(self.peek(), Token::LParen) {
            if db.is_some() {
                return Err(syntax(
                    "a database-qualified name cannot be a function call",
                ));
            }
            self.advance();
            let mut a = Vec::new();
            let mut names: Vec<Option<String>> = Vec::new();
            loop {
                let argname = if is_ident(self.peek()) && matches!(self.peek_at(1), Token::FatArrow)
                {
                    let nm = self.expect_identifier()?;
                    self.expect(&Token::FatArrow)?;
                    Some(nm)
                } else if names.iter().any(Option::is_some) {
                    // A positional argument may not follow a named one (PostgreSQL, 42601).
                    return Err(syntax("positional argument cannot follow named argument"));
                } else {
                    None
                };
                a.push(self.parse_expr()?);
                names.push(argname);
                if !matches!(self.peek(), Token::Comma) {
                    break;
                }
                self.advance();
            }
            self.expect(&Token::RParen)?;
            if names.iter().any(Option::is_some) {
                arg_names = Some(names);
            }
            Some(a)
        } else {
            None
        };
        let mut alias_pos = None;
        // `AS (c type, …)` — a table function's column-definition list without a relation alias
        // (PostgreSQL's `func_alias_clause`); the relation keeps the function's name.
        let mut bare_defs = None;
        let alias = if self.peek_keyword().as_deref() == Some("as") {
            self.advance();
            if args.is_some() && matches!(self.peek(), Token::LParen) {
                self.advance(); // (
                bare_defs = Some(self.parse_field_def_list()?);
                None
            } else {
                alias_pos = Some(self.pos);
                Some(self.expect_identifier()?)
            }
        } else {
            match self.peek() {
                Token::QuotedIdent(w) => {
                    let alias = w.clone();
                    alias_pos = Some(self.pos);
                    self.advance();
                    Some(alias)
                }
                Token::Word(w) if !is_table_ref_stop_keyword(w) => {
                    let alias = w.clone();
                    alias_pos = Some(self.pos);
                    self.advance();
                    Some(alias)
                }
                _ => None,
            }
        };
        // A `(` after an alias is a column alias list. On a base table (or view) it is the
        // rename-only list `AS x(p, q)` (grammar.md §15). On a table function it is either the
        // rename-only list `AS g(c1, …)` (§35) or the typed column-definition list
        // `AS g(c type, …)` used by record-returning functions (C0, json-table.md §1).
        let mut list_pos = None;
        let (column_aliases, column_defs) =
            if alias.is_some() && matches!(self.peek(), Token::LParen) && args.is_none() {
                let open = self.pos;
                self.advance(); // (
                let mut aliases = vec![self.expect_identifier()?];
                while matches!(self.peek(), Token::Comma) {
                    self.advance();
                    aliases.push(self.expect_identifier()?);
                }
                list_pos = Some((open, self.pos));
                self.expect(&Token::RParen)?;
                (Some(aliases), None)
            } else if alias.is_some() && matches!(self.peek(), Token::LParen) {
                self.advance(); // (
                // Disambiguate: a col-def list has `name type`; a rename list has `name ,`/`name )`.
                // After the opening `(`, the current token is the first column name, so a `Word` in the
                // NEXT slot means a type follows (col-def list).
                let typed = is_ident(self.peek_at(1));
                if typed {
                    (None, Some(self.parse_field_def_list()?))
                } else {
                    let mut aliases = vec![self.expect_identifier()?];
                    while matches!(self.peek(), Token::Comma) {
                        self.advance();
                        aliases.push(self.expect_identifier()?);
                    }
                    self.expect(&Token::RParen)?;
                    (Some(aliases), None)
                }
            } else {
                (None, bare_defs)
            };
        Ok(TableRef {
            name,
            db,
            alias,
            args,
            arg_names,
            subquery: None,
            values: None,
            column_aliases,
            column_defs,
            json_table: None,
            // An SRF is implicitly lateral; `lateral` records only whether the keyword was written.
            lateral,
            pos: TableRefPos {
                name: name_pos,
                alias: alias_pos,
                list: list_pos,
                depth: self.depth,
                src,
            },
        })
    }

    /// Parse a DERIVED TABLE — `"(" (query_expr | values_body) ")" derived_alias?` (grammar.md §42)
    /// — or a PARENTHESIZED JOIN `"(" table_ref join_clause+ ")" derived_alias?` (§15). The caller
    /// has verified the next token is `(`. A derived table is recognized when a `SELECT`/`WITH` (a
    /// `query_expr` body) OR `VALUES` (a VALUES-body relation) follows the `(` — the §26
    /// leading-`SELECT` lookahead, extended with `VALUES`, a §8 cross-core surface; any other
    /// leading `(` is a parenthesized join, which may not follow `LATERAL` (42601). The alias is
    /// OPTIONAL (PostgreSQL 18 relaxed the old mandatory-alias rule): present, it is the relation's
    /// label and may carry a column-rename list `(c1, c2, …)`; absent, the relation has no qualifier
    /// (its bare columns still resolve and can be ambiguous). An aliased join is the derived table
    /// `(SELECT * FROM <join>) AS alias`; an unaliased one keeps its inner labels visible
    /// (`join_group`). `alias`/`name` carry the alias (empty when none).
    fn parse_derived_table(&mut self, lateral: bool) -> Result<TableRef> {
        // Consume the opening `(`. A leading `SELECT`/`WITH` is a query-expr body; a leading `VALUES`
        // is a VALUES-body relation; any other leading `(` is a parenthesized join. The body, the
        // nesting recursion, and the alias tail each take their own frame (cost.md §7.1).
        self.advance();
        let body = self.parse_derived_body(lateral);
        self.finish_derived_table(body)
    }

    /// The body of a derived table after its `(`: the subquery (or a VALUES list, or a
    /// parenthesized join), and whether it is a join group.
    #[inline(never)]
    #[allow(clippy::type_complexity)]
    fn parse_derived_body(
        &mut self,
        lateral: bool,
    ) -> Result<(Option<Box<QueryExpr>>, Option<Vec<Vec<Expr>>>, bool)> {
        // The body is EITHER a query_expr (a leading `SELECT`) OR a VALUES list (a leading
        // `VALUES`) — `(VALUES (e…),(e…))`, a computed relation of literal rows (grammar.md §42).
        // The VALUES body's values are GENERAL expressions (resolved as constants at plan time,
        // parent = None). A trailing ORDER BY / LIMIT / OFFSET makes the body the query
        // `SELECT * FROM (VALUES …) ORDER BY … LIMIT …` (`values_query`).
        let mut join_group = false;
        let (start, depth, furthest) = (self.pos, self.depth, self.furthest.get());
        let mut values_rows = None;
        if self.peek_keyword().as_deref() == Some("values") {
            let rows = self.parse_values_body()?;
            // A set operator after the list makes the body a query with a VALUES operand
            // (grammar.md §25): re-parse it as one.
            if matches!(
                self.peek_keyword().as_deref(),
                Some("union") | Some("intersect") | Some("except")
            ) {
                self.pos = start;
                self.depth = depth;
                self.furthest.set(furthest);
            } else {
                values_rows = Some(rows);
            }
        }
        let (subquery, values) = if let Some(rows) = values_rows {
            let order_by = self.parse_order_by(true)?;
            let (limit, offset) = self.parse_limit_offset()?;
            if order_by.is_empty() && limit.is_none() && offset.is_none() {
                (None, Some(rows))
            } else {
                (
                    Some(Box::new(values_query(rows, order_by, limit, offset))),
                    None,
                )
            }
        } else if self.at_subquery_start() {
            (Some(Box::new(self.parse_subquery()?)), None)
        } else if lateral {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "LATERAL must be followed by a subquery or a function call",
            ));
        } else {
            join_group = true;
            (Some(Box::new(self.parse_join_group()?)), None)
        };
        Ok((subquery, values, join_group))
    }

    /// The `)` and optional alias / column list after a derived table's body.
    #[inline(never)]
    #[allow(clippy::type_complexity)]
    fn finish_derived_table(
        &mut self,
        body: Result<(Option<Box<QueryExpr>>, Option<Vec<Vec<Expr>>>, bool)>,
    ) -> Result<TableRef> {
        let (mut subquery, values, join_group) = body?;
        self.expect(&Token::RParen)?;
        // The alias is optional, parsed exactly like a base table's: an explicit `AS` takes the
        // next identifier; an implicit alias is a word that is not a clause/join stop keyword.
        let alias = if self.peek_keyword().as_deref() == Some("as") {
            self.advance();
            Some(self.expect_identifier()?)
        } else {
            match self.peek() {
                Token::QuotedIdent(w) => {
                    let a = w.clone();
                    self.advance();
                    Some(a)
                }
                Token::Word(w) if !is_table_ref_stop_keyword(w) => {
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
        // An unaliased join keeps its inner relation labels visible to the enclosing query.
        if join_group && alias.is_none() {
            if let Some(QueryExpr::Select(sel)) = subquery.as_deref_mut() {
                sel.join_group = true;
            }
        }
        Ok(TableRef {
            name: alias.clone().unwrap_or_default(),
            // A derived table has no database qualifier (only a base table in table position does).
            db: None,
            alias,
            args: None,
            arg_names: None,
            subquery,
            values,
            column_aliases,
            column_defs: None,
            json_table: None,
            // The caller (`parse_table_ref`) sets `lateral` from a leading `LATERAL` keyword.
            lateral: false,
            pos: TableRefPos::default(),
        })
    }

    /// Parse `JSON_TABLE(ctx, path [AS n] COLUMNS (col, …)) [AS alias]` (json-table.md §3, T1). The
    /// caller has verified the `JSON_TABLE` keyword + `(`.
    #[inline(never)]
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
        let passing = self.parse_json_passing()?;
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
                Token::QuotedIdent(w) => {
                    let a = w.clone();
                    self.advance();
                    Some(a)
                }
                Token::Word(w) if !is_table_ref_stop_keyword(w) => {
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
            arg_names: None,
            subquery: None,
            values: None,
            column_aliases: None,
            column_defs: None,
            json_table: Some(Box::new(JsonTable {
                ctx: Box::new(ctx),
                path: Box::new(path),
                passing,
                columns,
            })),
            lateral: false,
            pos: TableRefPos::default(),
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
            // Each NESTED level is one more nesting level of the statement (cost.md §7,
            // json.md §6.4): a deep NESTED chain recurses in the parser and the planner.
            self.deepen()?;
            let columns = self.parse_jt_columns()?;
            self.undeepen();
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
        let type_name = self.expect_type_name()?;
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
    /// least one row, each with at least one value (an empty `()` is a 42601). A trailing
    /// ORDER BY / LIMIT / OFFSET is the caller's (`parse_derived_table`).
    #[inline(never)]
    fn parse_values_body(&mut self) -> Result<Vec<Vec<Expr>>> {
        self.expect_keyword("values")?;
        let mut rows = Vec::new();
        loop {
            self.expect(&Token::LParen)?;
            let mut row = vec![self.parse_values_value()?];
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                row.push(self.parse_values_value()?);
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

    /// One value of a VALUES-body row: a general expression. A value that is exactly the bare word
    /// `DEFAULT` is the keyword, as in an INSERT slot, and is allowed only in the plain `INSERT …
    /// VALUES` source: here it is 42601 (PostgreSQL: "DEFAULT is not allowed in this context").
    fn parse_values_value(&mut self) -> Result<Expr> {
        if self.peek_keyword().as_deref() == Some("default")
            && matches!(self.peek_at(1), Token::Comma | Token::RParen)
        {
            return Err(syntax("DEFAULT is not allowed in this context"));
        }
        self.parse_expr()
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
    #[inline(never)]
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
        let mk = |ordinal, expr, qualifier, column, pos, collation| OrderKey {
            ordinal,
            expr,
            qualifier,
            column,
            pos,
            collation,
            descending,
            nulls_first,
        };
        match expr {
            Expr::Literal(Literal::Int(n), _) if allow_ordinal => {
                mk(Some(n), None, None, String::new(), SrcPos::NONE, collation)
            }
            Expr::Column(name, pos) => mk(None, None, None, name, pos, collation),
            Expr::QualifiedColumn {
                qualifier,
                name,
                pos,
            } => mk(None, None, Some(qualifier), name, pos, collation),
            // `parse_expr` folds a trailing `COLLATE "x"` into the key (collation.md §1). When it wraps a
            // bare column, unwrap back to a column key carrying that explicit collation — exactly the
            // OrderKey the old column-only parser built, so the column fast path is byte-identical.
            Expr::Collate {
                inner,
                collation: c,
            } => match *inner {
                Expr::Column(name, pos) => mk(None, None, None, name, pos, Some(c)),
                Expr::QualifiedColumn {
                    qualifier,
                    name,
                    pos,
                } => mk(None, None, Some(qualifier), name, pos, Some(c)),
                other => mk(
                    None,
                    Some(Expr::Collate {
                        inner: Box::new(other),
                        collation: c,
                    }),
                    None,
                    String::new(),
                    SrcPos::NONE,
                    collation,
                ),
            },
            other => mk(
                None,
                Some(other),
                None,
                String::new(),
                SrcPos::NONE,
                collation,
            ),
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
    #[inline(never)]
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

    /// The RHS of one `SET col = …` assignment — shared by standalone UPDATE and ON CONFLICT DO
    /// UPDATE. Returns `(true, placeholder)` for the `DEFAULT` form, which is special only when it is
    /// the complete RHS: keywords are legal identifiers, so `SET x = default + 1` parses `default`
    /// as a column reference (grammar.md §16). The placeholder is never resolved.
    fn parse_assignment_rhs(&mut self) -> Result<(bool, Expr)> {
        let next_is_terminator =
            matches!(self.peek_at(1), Token::Comma | Token::RParen | Token::Eof)
                || matches!(
                    self.peek_keyword_at(1).as_deref(),
                    Some("where" | "returning")
                );
        if self.peek_keyword().as_deref() == Some("default") && next_is_terminator {
            self.advance();
            return Ok((true, Expr::Literal(Literal::Null, SrcPos::NONE)));
        }
        Ok((false, self.parse_expr()?))
    }

    /// `UPDATE <table> SET <col> = <operand> [, <col> = <operand>]* [WHERE <pred>]`.
    fn parse_update(&mut self) -> Result<Update> {
        self.expect_keyword("update")?;
        let table_pos = self.here();
        let (db, table) = self.parse_qualified_table_name()?;
        self.expect_keyword("set")?;

        let mut assignments = Vec::new();
        loop {
            let pos = self.here();
            let column = self.expect_identifier()?;
            self.expect(&Token::Eq)?;
            let (is_default, value) = self.parse_assignment_rhs()?;
            assignments.push(Assignment {
                column,
                pos,
                is_default,
                value,
            });
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
            table_pos,
            db,
            assignments,
            filter,
            returning,
            fk_target: None,
        })
    }

    /// `DELETE FROM <table> [WHERE <pred>]`. No WHERE deletes every row.
    fn parse_delete(&mut self) -> Result<Delete> {
        self.expect_keyword("delete")?;
        self.expect_keyword("from")?;
        let table_pos = self.here();
        let (db, table) = self.parse_qualified_table_name()?;
        let filter = self.parse_optional_where()?;
        let returning = self.parse_returning()?;
        Ok(Delete {
            table,
            table_pos,
            db,
            filter,
            returning,
            fk_target: None,
        })
    }

    /// Parse an optional terminal `RETURNING <select_items>` clause (shared by
    /// INSERT/UPDATE/DELETE — spec/design/grammar.md §32). `RETURNING` is not reserved (§3):
    /// it is a clause only in this trailing position (and it joins the table_ref
    /// implicit-alias stop set, so an `INSERT ... SELECT` source never swallows it — §15).
    /// The optional `WITH (OLD AS o, NEW AS n)` prefix aliases either row-version
    /// pseudo-relation; `AS` is mandatory and each version may occur once. The item list is
    /// the ordinary select-items production (`*` or expressions with optional `AS` labels);
    /// an empty list fails in `parse_expr` (42601).
    fn parse_returning(&mut self) -> Result<Option<ReturningClause>> {
        if self.peek_keyword().as_deref() != Some("returning") {
            return Ok(None);
        }
        self.advance(); // RETURNING
        let mut old_alias = None;
        let mut new_alias = None;
        if self.peek_keyword().as_deref() == Some("with")
            && matches!(self.peek_at(1), Token::LParen)
            && matches!(
                self.peek_keyword_at(2).as_deref(),
                Some("old") | Some("new")
            )
            && self.peek_keyword_at(3).as_deref() == Some("as")
        {
            self.advance(); // WITH
            self.expect(&Token::LParen)?;
            loop {
                let version = self.peek_keyword().ok_or_else(|| {
                    EngineError::new(
                        SqlState::SyntaxError,
                        "expected OLD or NEW in RETURNING WITH",
                    )
                })?;
                if version != "old" && version != "new" {
                    return Err(EngineError::new(
                        SqlState::SyntaxError,
                        "expected OLD or NEW in RETURNING WITH",
                    ));
                }
                self.advance();
                self.expect_keyword("as")?;
                let alias = self.expect_identifier()?;
                let slot = if version == "old" {
                    &mut old_alias
                } else {
                    &mut new_alias
                };
                if slot.is_some() {
                    return Err(EngineError::new(
                        SqlState::SyntaxError,
                        format!(
                            "{} cannot be specified multiple times",
                            version.to_ascii_uppercase()
                        ),
                    ));
                }
                *slot = Some(alias);
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                    continue;
                }
                self.expect(&Token::RParen)?;
                break;
            }
        }
        Ok(Some(ReturningClause {
            old_alias,
            new_alias,
            items: self.parse_select_items()?,
        }))
    }

    /// Parse an optional trailing `WHERE <expr>` (shared by SELECT/UPDATE/DELETE). The
    /// expression must resolve to boolean (checked by the executor).
    #[inline(never)]
    fn parse_optional_where(&mut self) -> Result<Option<Expr>> {
        if self.peek_keyword().as_deref() == Some("where") {
            self.advance();
            Ok(Some(self.parse_expr()?))
        } else {
            Ok(None)
        }
    }

    #[inline(never)]
    fn parse_select_items(&mut self) -> Result<SelectItems> {
        if matches!(self.peek(), Token::Star) {
            let pos = self.pos;
            self.advance();
            return Ok(SelectItems::All(StarPos(Some(pos))));
        }
        let mut items = Vec::new();
        loop {
            // `t.*` — a qualified star (all columns of the relation labeled `t`), a select-list /
            // RETURNING item MIXABLE with other items (grammar.md §15). Recognized by the
            // three-token shape `identifier "." "*"` before the general expr parser, so `t.col`
            // (Dot then a word) and `a * b` (no Dot) are untouched, and a bare `*` was already
            // handled above. It takes no `AS` alias (PG rejects one).
            if is_ident(self.peek())
                && matches!(self.peek_at(1), Token::Dot)
                && matches!(self.peek_at(2), Token::Star)
            {
                let pos = self.pos;
                let qualifier = self.expect_identifier()?;
                self.advance(); // .
                self.advance(); // *
                items.push(SelectItem {
                    expr: Expr::QualifiedStar {
                        qualifier,
                        pos: StarPos(Some(pos)),
                    },
                    alias: None,
                });
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                    continue;
                }
                break;
            }
            let expr = self.parse_expr()?;
            // Optional output label, `AS alias` or a bare label (spec/grammar/grammar.ebnf
            // `select_item`). `AS` is not reserved, so it is taken as an alias marker only here,
            // after a complete expr. Without `AS`, a quoted identifier, or a bare word outside the
            // no-bare-label set, is the label (grammar.md §8). The alias never enters resolution.
            let alias = match self.peek() {
                Token::Word(w) if w == "as" => {
                    self.advance();
                    Some(self.expect_identifier()?)
                }
                Token::QuotedIdent(_) => Some(self.expect_identifier()?),
                Token::Word(w) if !is_bare_label_stop_keyword(w) => Some(self.expect_identifier()?),
                _ => None,
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
        // The operand, and an early return when no operator of this rung follows, take a small
        // frame; the chain loop runs in its own (spec/design/cost.md §7.1).
        let lhs = self.parse_and();
        if lhs.is_ok() && self.peek_keyword().as_deref() == Some("or") {
            return self.parse_or_tail(lhs);
        }
        lhs
    }

    #[inline(never)]
    fn parse_or_tail(&mut self, lhs: Result<Expr>) -> Result<Expr> {
        let mut lhs = lhs?;
        let base = self.depth;
        while self.peek_keyword().as_deref() == Some("or") {
            self.deepen()?; // each chained OR is one more AST level
            let pos = self.here();
            self.advance();
            let rhs = self.parse_and()?;
            lhs = binary(BinaryOp::Or, lhs, rhs, pos);
        }
        self.depth = base;
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        // The operand, and an early return when no operator of this rung follows, take a small
        // frame; the chain loop runs in its own (spec/design/cost.md §7.1).
        let lhs = self.parse_not();
        if lhs.is_ok() && self.peek_keyword().as_deref() == Some("and") {
            return self.parse_and_tail(lhs);
        }
        lhs
    }

    #[inline(never)]
    fn parse_and_tail(&mut self, lhs: Result<Expr>) -> Result<Expr> {
        let mut lhs = lhs?;
        let base = self.depth;
        while self.peek_keyword().as_deref() == Some("and") {
            self.deepen()?; // each chained AND is one more AST level
            let pos = self.here();
            self.advance();
            let rhs = self.parse_not()?;
            lhs = binary(BinaryOp::And, lhs, rhs, pos);
        }
        self.depth = base;
        Ok(lhs)
    }

    fn parse_not(&mut self) -> Result<Expr> {
        // A qualified column has structural priority over keyword-led expressions (§3): `not.v`
        // starts with the relation label `not`, not the unary operator. The primary parser applies
        // the same rule before literals / CAST / CASE / current-date spellings.
        if self.peek_keyword().as_deref() == Some("not") && !self.at_qualified_column_ref() {
            return self.parse_not_operand();
        }
        self.parse_comparison()
    }

    /// A prefix `NOT` and its operand, in its own frame (spec/design/cost.md §7.1).
    #[inline(never)]
    fn parse_not_operand(&mut self) -> Result<Expr> {
        let not = self.here();
        self.advance();
        // right-associative: NOT NOT x — each NOT is one more AST level (recursion here, so
        // the depth guard also protects the parser's own stack).
        self.deepen()?;
        let operand = self.parse_not()?;
        self.undeepen();
        Ok(Expr::Unary {
            op: UnaryOp::Not,
            operand: Box::new(operand),
            pos: not,
        })
    }

    /// One comparison, a postfix `IS [NOT] NULL`, or `IS [NOT] DISTINCT FROM`, all
    /// non-associative: `a = b = c` is a syntax error, and `a + 1 IS NULL` binds as
    /// `(a + 1) IS NULL`. After the shared `IS` `NOT`? the parser dispatches on the
    /// `NULL` vs `DISTINCT FROM` keyword (spec/grammar/grammar.ebnf `comparison`).
    fn parse_comparison(&mut self) -> Result<Expr> {
        // The operand, and an early return when no comparison follows, take a small frame; the
        // comparison itself is parsed in its own (spec/design/cost.md §7.1).
        let lhs = self.parse_concat();
        if lhs.is_ok() && self.at_comparison() {
            return self.parse_comparison_tail(lhs);
        }
        lhs
    }

    /// Whether a comparison-level operator follows an operand: `IS`, `[NOT] IN | BETWEEN | LIKE |
    /// ILIKE`, a regex match, or a comparison operator. Exactly the forms `parse_comparison_tail`
    /// consumes, so returning the bare operand otherwise is the same parse.
    fn at_comparison(&self) -> bool {
        match self.peek() {
            Token::Eq
            | Token::Ne
            | Token::Lt
            | Token::Gt
            | Token::Le
            | Token::Ge
            | Token::Tilde
            | Token::TildeStar
            | Token::BangTilde
            | Token::BangTildeStar => true,
            Token::Word(_) => match self.peek_keyword().as_deref() {
                Some("is" | "in" | "between" | "like" | "ilike") => true,
                Some("not") => matches!(
                    self.peek_keyword_at(1).as_deref(),
                    Some("in" | "between" | "like" | "ilike")
                ),
                _ => false,
            },
            _ => false,
        }
    }

    #[inline(never)]
    fn parse_comparison_tail(&mut self, lhs: Result<Expr>) -> Result<Expr> {
        let lhs = lhs?;
        if self.peek_keyword().as_deref() == Some("is") {
            return outline(|| {
                let is_pos = self.here();
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
                        pos: is_pos,
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
        // The operator's position: the `NOT` of a negated form, else the keyword (PostgreSQL's).
        let op_pos = self.here();
        if negated {
            self.advance(); // NOT
        }
        if self.peek_keyword().as_deref() == Some("in") {
            return outline(|| {
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
                        pos: op_pos,
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
                    pos: op_pos,
                });
            });
        }
        if self.peek_keyword().as_deref() == Some("between") {
            return outline(|| {
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
                    pos: op_pos,
                });
            });
        }
        // LIKE / ILIKE (case-insensitive) — grammar.md §22. Both bind at the comparison level; the
        // pattern is one CONCAT expression. `ilike` is just another peeked keyword (no lexer change).
        // An `ESCAPE e` clause desugars, as in PostgreSQL, to the pattern `like_escape(p, e)`; a bare
        // NULL or `$N` pattern/escape is typed text by the function's signature (grammar.md §22).
        if matches!(self.peek_keyword().as_deref(), Some("like") | Some("ilike")) {
            return outline(|| {
                let insensitive = self.peek_keyword().as_deref() == Some("ilike");
                let like_pos = self.here();
                self.advance();
                let mut rhs = self.parse_concat()?;
                if self.peek_keyword().as_deref() == Some("escape") {
                    self.advance();
                    let escape = self.parse_concat()?;
                    rhs = Expr::FuncCall {
                        name: "like_escape".to_string(),
                        args: vec![rhs, escape],
                        arg_names: None,
                        star: false,
                        distinct: false,
                        filter: None,
                        variadic: false,
                        over: None,
                        over_name: None,
                        within_group: None,
                        agg_order: None,
                        pos: like_pos,
                    };
                }
                return Ok(Expr::Like {
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                    negated,
                    insensitive,
                    pos: op_pos,
                });
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
            return outline(|| {
                self.advance();
                let rhs = self.parse_concat()?;
                return Ok(Expr::Regex {
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                    negated: rx_negated,
                    insensitive: rx_insensitive,
                    pos: op_pos,
                });
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
            Some(op) => outline(|| {
                self.advance();
                let pos = op_pos;
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
                            pos,
                        });
                    }
                    let array = self.parse_expr()?; // a full expression resolving to an array
                    self.expect(&Token::RParen)?;
                    return Ok(Expr::Quantified {
                        op,
                        all,
                        lhs: Box::new(lhs),
                        array: Box::new(array),
                        pos,
                    });
                }
                let rhs = self.parse_concat()?;
                Ok(binary(op, lhs, rhs, pos))
            }),
            None => Ok(lhs),
        }
    }

    /// The "any other operator" level (grammar.md §39/§40, array-functions.md §8/§10): one rung
    /// tighter than the comparisons, looser than additive, left-associative. It hosts `||` array
    /// concatenation plus the `@>`/`<@`/`&&` array containment/overlap operators — all the same
    /// precedence in PostgreSQL. Each operand is an additive expression, so `a + b || c` is
    /// `(a + b) || c`; chaining mixes freely (`a || b @> c` is `(a || b) @> c`).
    fn parse_concat(&mut self) -> Result<Expr> {
        // The operand, and an early return when no operator of this rung follows, take a small
        // frame; the chain loop runs in its own (spec/design/cost.md §7.1).
        let lhs = self.parse_additive();
        if lhs.is_ok() && self.at_concat_op() {
            return self.parse_concat_tail(lhs);
        }
        lhs
    }

    /// Whether the next token is an operator of the "any other operator" rung (parse_concat).
    fn at_concat_op(&self) -> bool {
        Self::concat_op(self.peek()).is_some()
    }

    /// The binary operator a token denotes at the "any other operator" rung.
    fn concat_op(t: &Token) -> Option<BinaryOp> {
        Some(match t {
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
            _ => return None,
        })
    }

    #[inline(never)]
    fn parse_concat_tail(&mut self, lhs: Result<Expr>) -> Result<Expr> {
        let mut lhs = lhs?;
        let base = self.depth;
        while let Some(op) = Self::concat_op(self.peek()) {
            self.deepen()?; // each chained operator is one more AST level
            let pos = self.here();
            self.advance();
            let rhs = self.parse_additive()?;
            lhs = binary(op, lhs, rhs, pos);
        }
        self.depth = base;
        Ok(lhs)
    }

    fn parse_additive(&mut self) -> Result<Expr> {
        // See parse_or: a small frame for the operand, the chain loop in its own.
        let lhs = self.parse_multiplicative();
        if lhs.is_ok() && matches!(self.peek(), Token::Plus | Token::Minus) {
            return self.parse_additive_tail(lhs);
        }
        lhs
    }

    #[inline(never)]
    fn parse_additive_tail(&mut self, lhs: Result<Expr>) -> Result<Expr> {
        let mut lhs = lhs?;
        let base = self.depth;
        loop {
            let op = match self.peek() {
                Token::Plus => BinaryOp::Add,
                Token::Minus => BinaryOp::Sub,
                _ => break,
            };
            self.deepen()?; // each chained +/- is one more AST level (the `1+1+…` vector)
            let pos = self.here();
            self.advance();
            let rhs = self.parse_multiplicative()?;
            lhs = binary(op, lhs, rhs, pos);
        }
        self.depth = base;
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr> {
        // The operand, and an early return when no operator of this rung follows, take a small
        // frame; the chain loop runs in its own (spec/design/cost.md §7.1).
        let lhs = self.parse_at_time_zone();
        if lhs.is_ok() && matches!(self.peek(), Token::Star | Token::Slash | Token::Percent) {
            return self.parse_multiplicative_tail(lhs);
        }
        lhs
    }

    #[inline(never)]
    fn parse_multiplicative_tail(&mut self, lhs: Result<Expr>) -> Result<Expr> {
        let mut lhs = lhs?;
        let base = self.depth;
        loop {
            let op = match self.peek() {
                Token::Star => BinaryOp::Mul,
                Token::Slash => BinaryOp::Div,
                Token::Percent => BinaryOp::Mod,
                _ => break,
            };
            self.deepen()?; // each chained * / % is one more AST level
            let pos = self.here();
            self.advance();
            let rhs = self.parse_at_time_zone()?;
            lhs = binary(op, lhs, rhs, pos);
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
        // The operand, and an early return when no operator of this rung follows, take a small
        // frame; the chain loop runs in its own (spec/design/cost.md §7.1).
        let lhs = self.parse_unary();
        if lhs.is_ok() && self.peek_at_time_zone() {
            return self.parse_at_time_zone_tail(lhs);
        }
        lhs
    }

    #[inline(never)]
    fn parse_at_time_zone_tail(&mut self, lhs: Result<Expr>) -> Result<Expr> {
        let mut lhs = lhs?;
        let base = self.depth;
        while self.peek_at_time_zone() {
            self.deepen()?; // each chained AT TIME ZONE is one more AST level
            let at = self.here();
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
                agg_order: None,
                pos: at,
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
            return self.parse_negation();
        }
        self.parse_postfix()
    }

    /// A unary `-` and its operand, in its own frame (spec/design/cost.md §7.1).
    #[inline(never)]
    fn parse_negation(&mut self) -> Result<Expr> {
        let minus = self.here();
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
            return Ok(Expr::Literal(Literal::Int(folded as i64), minus));
        }
        // Fold unary-minus of a decimal literal into one negative decimal literal (like
        // the integer fold). Decimal negation never overflows. Same `::` suppression.
        if matches!(self.peek(), Token::Decimal(..))
            && self.tokens.get(self.pos + 1) != Some(&Token::DoubleColon)
        {
            if let Token::Decimal(digits, scale) = self.advance() {
                return Ok(Expr::Literal(
                    Literal::Decimal(Decimal::from_digits_scale(true, &digits, scale)),
                    minus,
                ));
            }
        }
        // each chained unary `-` is one more AST level (recursion here, so the depth guard
        // also protects the parser's own stack against `- - - … x`).
        self.deepen()?;
        let operand = self.parse_unary()?;
        self.undeepen();
        Ok(Expr::Unary {
            op: UnaryOp::Neg,
            operand: Box::new(operand),
            pos: minus,
        })
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
        // The primary, and an early return when no postfix follows, take a small frame; the
        // postfix loop runs in its own (spec/design/cost.md §7.1).
        let paren = matches!(self.peek(), Token::LParen);
        let expr = self.parse_primary();
        if expr.is_ok() && self.at_postfix(paren && self.pos != self.bare_row_end) {
            return self.parse_postfix_tail(expr, paren);
        }
        expr
    }

    /// Whether a postfix operator follows; `.field` only when `field_accessible`.
    fn at_postfix(&self, field_accessible: bool) -> bool {
        match self.peek() {
            Token::DoubleColon | Token::LBracket => true,
            Token::Dot => field_accessible,
            Token::Word(_) => self.peek_keyword().as_deref() == Some("collate"),
            _ => false,
        }
    }

    #[inline(never)]
    fn parse_postfix_tail(&mut self, expr: Result<Expr>, paren: bool) -> Result<Expr> {
        let mut expr = expr?;
        let base = self.depth;
        let mut field_accessible = paren && self.pos != self.bare_row_end;
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
                    let base_type = self.expect_type_name()?;
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
                        pos: SrcPos::NONE,
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
        // Every word is legal as an identifier (§3), including spellings otherwise dispatched
        // below as literals or keyword-led expressions. Recognize the unambiguous
        // `identifier.identifier` shape first so aliases such as `null`, `cast`, or `case` work.
        if self.at_qualified_column_ref() {
            return outline(|| {
                let pos = self.here();
                let qualifier = self.expect_identifier()?;
                self.expect(&Token::Dot)?;
                let name = self.expect_identifier()?;
                return Ok(Expr::QualifiedColumn {
                    qualifier,
                    name,
                    pos,
                });
            });
        }
        if matches!(self.peek(), Token::LParen) {
            return outline(|| {
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
                // `(a, b, …)` — the bare row constructor (composite.md §1), PostgreSQL's implicit `ROW`
                // spelling: two or more comma-separated expressions in parentheses. Identical to
                // `ROW(a, b, …)`; a single parenthesized expression stays a grouping.
                if matches!(self.peek(), Token::Comma) {
                    let mut fields = vec![e];
                    while matches!(self.peek(), Token::Comma) {
                        self.advance();
                        fields.push(self.parse_expr()?);
                    }
                    self.expect(&Token::RParen)?;
                    self.bare_row_end = self.pos;
                    return Ok(Expr::Row(fields));
                }
                self.expect(&Token::RParen)?;
                return Ok(e);
            });
        }
        // `EXISTS ( SELECT ... )` — the existence predicate (grammar.md §26). Recognized only when
        // an open-paren + a query start (`SELECT`, or a nested `WITH` — cte.md §7) follows, so
        // `exists` stays usable as a column / function name.
        if self.peek_keyword().as_deref() == Some("exists")
            && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
            && self.is_query_start_at(self.pos + 2)
        {
            return outline(|| {
                self.advance(); // EXISTS
                self.expect(&Token::LParen)?;
                let q = self.parse_subquery()?;
                self.expect(&Token::RParen)?;
                return Ok(Expr::Exists(Box::new(q)));
            });
        }
        if self.peek_keyword().as_deref() == Some("cast") {
            return outline(|| {
                let cast_pos = self.here();
                self.advance();
                self.expect(&Token::LParen)?;
                let inner = self.parse_expr()?;
                self.expect_keyword("as")?;
                let base_type = self.expect_type_name()?;
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
                    pos: cast_pos,
                });
            });
        }
        // `EXTRACT(field FROM source)` (grammar.md §50, timezones.md §9.2). Recognized only when
        // `extract` is immediately followed by `(`, so `extract` stays usable as a column / function
        // name otherwise (the one-token lookahead, §8). The field is an identifier (bare or quoted)
        // or a string literal — PostgreSQL's `extract_arg` — ASCII-lowercased, as the unit lookup
        // folds it (timezones.md §9.6); the source is any expression.
        if self.peek_keyword().as_deref() == Some("extract")
            && matches!(self.peek_at(1), Token::LParen)
        {
            return outline(|| {
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
                    Token::Word(_) | Token::QuotedIdent(_) => self.expect_identifier()?,
                    other => {
                        return Err(syntax(format!(
                            "expected an EXTRACT field, found {other:?}"
                        )));
                    }
                };
                self.expect_keyword("from")?;
                let source = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                return Ok(Expr::Extract {
                    field: field.to_ascii_lowercase(),
                    source: Box::new(source),
                });
            });
        }
        // `ROW(e1, e2, …)` composite constructor (spec/design/composite.md §1). Recognized when
        // `ROW` is immediately followed by `(`, so `row` stays usable as a column / function name
        // otherwise. The bare `(a, b)` form is parsed with the parenthesized expression above.
        if self.peek_keyword().as_deref() == Some("row") && matches!(self.peek_at(1), Token::LParen)
        {
            return outline(|| {
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
                                return Err(syntax(format!(
                                    "expected ',' or ')', found {other:?}"
                                )));
                            }
                        }
                    }
                } else {
                    self.advance(); // the empty ROW() — consume ')'
                }
                return Ok(Expr::Row(fields));
            });
        }
        // `ARRAY[e1, e2, …]` array constructor (spec/design/array.md §1). Recognized only when
        // `ARRAY` is immediately followed by `[`, so `array` stays usable as an identifier
        // otherwise. `ARRAY[]` is the empty array.
        if self.peek_keyword().as_deref() == Some("array")
            && matches!(self.peek_at(1), Token::LBracket)
        {
            return outline(|| {
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
                                return Err(syntax(format!(
                                    "expected ',' or ']', found {other:?}"
                                )));
                            }
                        }
                    }
                } else {
                    self.advance(); // the empty ARRAY[] — consume ']'
                }
                return Ok(Expr::Array(elems));
            });
        }
        // A typed string literal `type '...'` (grammar.md §36) — PostgreSQL's `type 'string'`,
        // equal to `CAST('string' AS type)` over a string-literal operand: ANY type-naming word
        // immediately followed by a string (`INTERVAL '1 day'`, `TIMESTAMP '...'`, `INTEGER '42'`,
        // `NUMERIC '1.5'`, `BOOLEAN 'true'`, `BYTEA '\xDE'`, …). Recognized only when the next token
        // is a string — a one-token lookahead — so the word stays usable as a column / function
        // name otherwise (`SELECT interval FROM t`). `true`/`false`/`null` are excluded: they are
        // their own value literals (handled below), not type names. The type name is resolved (and
        // the string coerced to it) at resolve time; an unknown type is 42704 there.
        if is_ident(self.peek())
            && matches!(self.tokens.get(self.pos + 1), Some(Token::Str(_)))
            && !matches!(self.peek(), Token::Word(w) if matches!(w.as_str(), "null" | "true" | "false"))
        {
            return outline(|| {
                let type_name = self.expect_identifier()?;
                let pos = self.here();
                if let Token::Str(text) = self.advance() {
                    return Ok(Expr::TypedLiteral {
                        type_name,
                        text,
                        pos,
                    });
                }
                unreachable!("peeked a string literal after the type name");
            });
        }
        // The two-word `character varying '...'` typed literal (types.md §15).
        if self.peek_keyword().as_deref() == Some("character")
            && self.peek_keyword_at(1).as_deref() == Some("varying")
            && matches!(self.tokens.get(self.pos + 2), Some(Token::Str(_)))
        {
            return outline(|| {
                let type_name = self.expect_type_name()?;
                let pos = self.here();
                if let Token::Str(text) = self.advance() {
                    return Ok(Expr::TypedLiteral {
                        type_name,
                        text,
                        pos,
                    });
                }
                unreachable!("peeked a string literal after the type name");
            });
        }
        // `JSON(expr [(WITH|WITHOUT) UNIQUE [KEYS]])` — the SQL/JSON JSON() constructor
        // (json-sql-functions.md §5). Distinguished from the `json '...'` typed literal (handled
        // above, a string follows) and a generic call by being the JSON keyword followed by `(`.
        if self.peek_keyword().as_deref() == Some("json")
            && matches!(self.peek_at(1), Token::LParen)
        {
            return outline(|| {
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
            });
        }
        // The SQL/JSON query functions `JSON_EXISTS` / `JSON_VALUE` / `JSON_QUERY`
        // (json-sql-functions.md §5, S2) — keyword-led primaries with sub-clauses.
        if let Some(kw @ ("json_exists" | "json_value" | "json_query")) =
            self.peek_keyword().as_deref()
            && matches!(self.peek_at(1), Token::LParen)
        {
            let kw = kw.to_string();
            return self.parse_json_query_function(&kw);
        }
        // `COALESCE(a, b, …)` — the first-non-NULL conditional (grammar.md §51). Recognized only
        // when COALESCE is immediately followed by `(` (the JSON(/EXTRACT( one-token lookahead),
        // so the word stays usable as a column name. At least one argument (an empty list is
        // 42601 — PostgreSQL's grammar has no empty form).
        if self.peek_keyword().as_deref() == Some("coalesce")
            && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
        {
            return outline(|| {
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
            });
        }
        // `GREATEST(a, b, …)` / `LEAST(a, b, …)` — the variadic max/min (grammar.md §52).
        // Recognized only when the keyword is immediately followed by `(` (the same one-token
        // lookahead), so the words stay usable as column names. At least one argument (an empty
        // list is 42601 — PostgreSQL's grammar has no empty form).
        let gl_kw = self.peek_keyword();
        if matches!(gl_kw.as_deref(), Some("greatest") | Some("least"))
            && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
        {
            return outline(|| {
                let greatest = gl_kw.as_deref() == Some("greatest");
                self.advance(); // GREATEST / LEAST
                self.advance(); // (
                if matches!(self.peek(), Token::RParen) {
                    return Err(syntax(if greatest {
                        "GREATEST requires at least one argument"
                    } else {
                        "LEAST requires at least one argument"
                    }));
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
                return Ok(Expr::GreatestLeast { args, greatest });
            });
        }
        // `NULLIF(a, b)` — NULL when `a = b`, else `a` (grammar.md §58). Recognized only when
        // NULLIF is immediately followed by `(` (the same one-token lookahead), so the word stays
        // usable as a column name. Exactly two arguments: any other count is 42601, as in
        // PostgreSQL's grammar.
        if self.peek_keyword().as_deref() == Some("nullif")
            && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
        {
            return outline(|| {
                let pos = self.here();
                self.advance(); // NULLIF
                self.advance(); // (
                let a = self.parse_expr()?;
                self.expect(&Token::Comma)?;
                let b = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                Ok(Expr::NullIf {
                    args: vec![a, b],
                    pos,
                })
            });
        }
        if self.peek_keyword().as_deref() == Some("case") {
            return outline(|| {
                let case_pos = self.here();
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
                    pos: case_pos,
                });
            });
        }
        // The primary token's character position (spec/design/error-fields.md §10.2).
        let here = self.here();
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
                    Ok(Expr::Literal(Literal::Int(m as i64), here))
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
                Ok(Expr::Literal(Literal::Null, here))
            }
            Token::Word(w) if w.eq_ignore_ascii_case("true") => {
                self.advance();
                Ok(Expr::Literal(Literal::Bool(true), here))
            }
            Token::Word(w) if w.eq_ignore_ascii_case("false") => {
                self.advance();
                Ok(Expr::Literal(Literal::Bool(false), here))
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
                    agg_order: None,
                    pos: here,
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
                    agg_order: None,
                    pos: here,
                })
            }
            Token::Str(_) => {
                if let Token::Str(s) = self.advance() {
                    Ok(Expr::Literal(Literal::Text(s), here))
                } else {
                    unreachable!("peeked a string literal")
                }
            }
            Token::Decimal(..) => {
                if let Token::Decimal(digits, scale) = self.advance() {
                    Ok(Expr::Literal(
                        Literal::Decimal(Decimal::from_digits_scale(false, &digits, scale)),
                        here,
                    ))
                } else {
                    unreachable!("peeked a decimal literal")
                }
            }
            Token::Word(_) | Token::QuotedIdent(_) => {
                // Function call: an identifier IMMEDIATELY followed by "(" is a call
                // (grammar.md §17). The one-token lookahead keeps function names non-reserved
                // (a column may be named `count`/`abs`); a qualified name (`t.col`) is never a
                // call. Aggregate and scalar names resolve; any other name is 42883.
                if matches!(self.tokens.get(self.pos + 1), Some(Token::LParen)) {
                    return self.parse_function_call();
                }
                let pos = self.here();
                let (qualifier, name) = self.parse_column_ref()?;
                Ok(match qualifier {
                    Some(qualifier) => Expr::QualifiedColumn {
                        qualifier,
                        name,
                        pos,
                    },
                    None => Expr::Column(name, pos),
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
    /// function name is resolved (byte-exactly) against the catalog later.
    fn parse_function_call(&mut self) -> Result<Expr> {
        // An UNQUOTED `position` is only the SQL special form `POSITION(sub IN s)`.
        if matches!(self.peek(), Token::Word(w) if w == "position") {
            return self.parse_position_form();
        }
        // An UNQUOTED `substring` / `overlay` admits its SQL special form after the first argument
        // (below).
        let substring_form = matches!(self.peek(), Token::Word(w) if w == "substring");
        let overlay_form = matches!(self.peek(), Token::Word(w) if w == "overlay");
        let pos = self.here();
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
        // A trailing in-aggregate `ORDER BY` after the last argument (`json_agg(x ORDER BY y)` —
        // aggregates.md §22): parsed with `allow_ordinal` OFF, so a bare integer is a constant key
        // (PostgreSQL), and it ends the argument list. The resolver rejects it on a non-aggregate
        // (42809) or a window call (0A000).
        let mut agg_order: Option<Box<Vec<OrderKey>>> = None;
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
                    if self.peek_keyword().as_deref() == Some("order") {
                        agg_order = Some(Box::new(self.parse_order_by(false)?));
                        break;
                    }
                    // A VARIADIC argument must be the last (PostgreSQL, 42601).
                    if matches!(self.peek(), Token::Comma) {
                        return Err(syntax("VARIADIC argument must be the last argument"));
                    }
                    break;
                }
                // A named argument is `identifier "=>" expr` (grammar.md §17); a two-token
                // lookahead (Word then `=>`) distinguishes it from a bare expr that starts with
                // an identifier (a column reference).
                let argname = if is_ident(self.peek()) && matches!(self.peek_at(1), Token::FatArrow)
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
                let positional = argname.is_none();
                args.push(self.parse_expr()?);
                arg_names.push(argname);
                // `SUBSTRING(x FROM s [FOR l])` / `SUBSTRING(x FOR l [FROM s])` (grammar.ebnf
                // `substring_expr`, string-functions.md §4): a FROM/FOR keyword after the first
                // positional argument selects the special form.
                if substring_form
                    && positional
                    && !distinct
                    && args.len() == 1
                    && matches!(self.peek_keyword().as_deref(), Some("from" | "for"))
                {
                    let source = args.pop().expect("one argument parsed");
                    return self.parse_substring_tail(name, source, pos);
                }
                // `OVERLAY(s PLACING r FROM start [FOR count])` (grammar.ebnf `overlay_expr`,
                // string-functions.md §3): a PLACING keyword after the first positional argument
                // selects the special form.
                if overlay_form
                    && positional
                    && !distinct
                    && args.len() == 1
                    && self.peek_keyword().as_deref() == Some("placing")
                {
                    let source = args.pop().expect("one argument parsed");
                    return self.parse_overlay_tail(name, source, pos);
                }
                if self.peek_keyword().as_deref() == Some("order") {
                    agg_order = Some(Box::new(self.parse_order_by(false)?));
                    break;
                }
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
            // The in-aggregate ORDER BY and WITHIN GROUP both order the aggregated input; PostgreSQL
            // rejects the pair in its grammar (42601).
            if agg_order.is_some() {
                return Err(syntax(
                    "cannot use multiple ORDER BY clauses with WITHIN GROUP",
                ));
            }
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
                    agg_order,
                    pos,
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
            agg_order,
            pos,
        })
    }

    /// The tail of the SQL special form `SUBSTRING(x FROM s [FOR l])` / `SUBSTRING(x FOR l [FROM s])`
    /// (grammar.ebnf `substring_expr`), the cursor on FROM or FOR. Desugars to the ordinary call
    /// `substring(x, s[, l])` — PostgreSQL's rewrite — with a FOR-only form starting at 1. No
    /// FILTER / OVER may follow (the form is not a function application).
    fn parse_substring_tail(&mut self, name: String, source: Expr, pos: SrcPos) -> Result<Expr> {
        let mut start = None;
        let mut count = None;
        if self.peek_keyword().as_deref() == Some("from") {
            self.advance();
            start = Some(self.parse_expr()?);
            if self.peek_keyword().as_deref() == Some("for") {
                self.advance();
                count = Some(self.parse_expr()?);
            }
        } else {
            self.expect_keyword("for")?;
            count = Some(self.parse_expr()?);
            if self.peek_keyword().as_deref() == Some("from") {
                self.advance();
                start = Some(self.parse_expr()?);
            }
        }
        self.expect(&Token::RParen)?;
        let mut args = vec![
            source,
            start.unwrap_or(Expr::Literal(Literal::Int(1), SrcPos::NONE)),
        ];
        args.extend(count);
        Ok(Expr::FuncCall {
            name,
            args,
            arg_names: None,
            star: false,
            distinct: false,
            filter: None,
            variadic: false,
            over: None,
            over_name: None,
            within_group: None,
            agg_order: None,
            pos,
        })
    }

    /// The SQL special form `POSITION(substring IN string)` (grammar.ebnf `position_expr`,
    /// string-functions.md §3), the cursor on the unquoted `position`. Desugars to the ordinary call
    /// `position(string, substring)` — PostgreSQL's rewrite, arguments swapped. Each operand is an
    /// "any other operator" (`parse_concat`) expression, so the operand's own `IN` is never taken as
    /// an IN predicate; anything else after the first operand (a comma) is 42601, as in PostgreSQL.
    fn parse_position_form(&mut self) -> Result<Expr> {
        let pos = self.here();
        let name = self.expect_identifier()?;
        self.expect(&Token::LParen)?;
        self.deepen()?;
        let sub = self.parse_concat()?;
        self.expect_keyword("in")?;
        let string = self.parse_concat()?;
        self.undeepen();
        self.expect(&Token::RParen)?;
        Ok(Expr::FuncCall {
            name,
            args: vec![string, sub],
            arg_names: None,
            star: false,
            distinct: false,
            filter: None,
            variadic: false,
            over: None,
            over_name: None,
            within_group: None,
            agg_order: None,
            pos,
        })
    }

    /// The tail of the SQL special form `OVERLAY(s PLACING r FROM start [FOR count])` (grammar.ebnf
    /// `overlay_expr`), the cursor on PLACING. Desugars to the ordinary call
    /// `overlay(s, r, start[, count])` — PostgreSQL's rewrite. FROM is required. No FILTER / OVER may
    /// follow (the form is not a function application).
    fn parse_overlay_tail(&mut self, name: String, source: Expr, pos: SrcPos) -> Result<Expr> {
        self.expect_keyword("placing")?;
        let replacement = self.parse_expr()?;
        self.expect_keyword("from")?;
        let start = self.parse_expr()?;
        let mut args = vec![source, replacement, start];
        if self.peek_keyword().as_deref() == Some("for") {
            self.advance();
            args.push(self.parse_expr()?);
        }
        self.expect(&Token::RParen)?;
        Ok(Expr::FuncCall {
            name,
            args,
            arg_names: None,
            star: false,
            distinct: false,
            filter: None,
            variadic: false,
            over: None,
            over_name: None,
            within_group: None,
            agg_order: None,
            pos,
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

    /// Whether the cursor has the exact `identifier "." identifier` shape. Expression
    /// disambiguation uses it so a keyword-spelled relation label wins over the keyword's literal,
    /// unary, or keyword-led primary meaning (spec/design/grammar.md §3).
    fn at_qualified_column_ref(&self) -> bool {
        is_ident(self.peek_at(0))
            && matches!(self.peek_at(1), Token::Dot)
            && is_ident(self.peek_at(2))
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
        self.note(self.pos);
        &self.tokens[self.pos]
    }

    /// The token `offset` positions ahead of the cursor (`Eof` past the end). Used with
    /// `peek_keyword_at` for the CHECK-constraint lookahead (spec/design/grammar.md §29).
    pub fn peek_at(&self, offset: usize) -> &Token {
        self.tokens.get(self.pos + offset).unwrap_or(&Token::Eof)
    }

    /// The current token if it is a bare word (already folded to lowercase by the lexer), else
    /// None. A quoted identifier is never a keyword (grammar.md §3).
    pub fn peek_keyword(&self) -> Option<String> {
        match self.peek() {
            Token::Word(w) => Some(w.clone()),
            _ => None,
        }
    }

    /// The keyword (lowercased) `offset` tokens ahead of the cursor, if that token is a word.
    /// Used for the two-token `NOT IN`/`NOT BETWEEN`/`NOT LIKE` lookahead (a CLAUDE.md §8
    /// determinism surface — byte-identical across the three parsers).
    pub fn peek_keyword_at(&self, offset: usize) -> Option<String> {
        match self.tokens.get(self.pos + offset) {
            Some(Token::Word(w)) => Some(w.clone()),
            _ => None,
        }
    }

    /// Consume and return the current token.
    pub fn advance(&mut self) -> Token {
        self.note(self.pos);
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

    /// Consume the current token, requiring it to be the given keyword: a bare word (already
    /// folded by the lexer; `kw` is lowercase) — never a quoted identifier.
    pub fn expect_keyword(&mut self, kw: &str) -> Result<()> {
        match self.advance() {
            Token::Word(w) if w.eq_ignore_ascii_case(kw) => Ok(()),
            other => Err(syntax(format!("expected keyword '{kw}', found {other:?}"))),
        }
    }

    /// Consume a type name in any type position (column/field definition, `ALTER … TYPE`, `CAST`,
    /// `::`, a typed literal, `RETURNING`, a `JSON_TABLE` column, `CREATE SEQUENCE … AS`): an
    /// identifier, or PostgreSQL's two-word `character varying` (types.md §15). The two-word form
    /// is recognized only as two bare words — `character` then `varying` (a quoted word is an
    /// ordinary identifier) — and returns the alias `"character varying"`, which the catalog
    /// resolves to `text` exactly like `varchar`. Any other `character` stays a one-word name
    /// (unknown type → 42704 at resolve). A CLAUDE.md §8 cross-core determinism surface
    /// (spec/design/grammar.md §14).
    fn expect_type_name(&mut self) -> Result<String> {
        if self.peek_keyword().as_deref() == Some("character")
            && self.peek_keyword_at(1).as_deref() == Some("varying")
        {
            self.advance(); // CHARACTER
            self.advance(); // VARYING
            return Ok("character varying".to_string());
        }
        self.expect_identifier()
    }

    /// Consume the current token, requiring it to be an identifier — a bare word (folded) or a
    /// quoted identifier (exact) — and return the name (grammar.md §3).
    pub fn expect_identifier(&mut self) -> Result<String> {
        match self.advance() {
            Token::Word(w) | Token::QuotedIdent(w) => Ok(w),
            other => Err(syntax(format!("expected an identifier, found {other:?}"))),
        }
    }

    /// Consume a quoted collation name after `COLLATE` (spec/design/collation.md §1). The name is a
    /// double-quoted identifier — case-sensitive and kept verbatim (`"C"`, `"en-US"`), so a bare
    /// word is not accepted (it would case-fold). An empty name (`""`) is a 42601 syntax error.
    fn expect_collation_name(&mut self) -> Result<String> {
        match self.advance() {
            // The lexer rejects a zero-length `""` (42601), so a quoted name is never empty.
            Token::QuotedIdent(s) => Ok(s),
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

/// Parse a stored view definition (spec/design/views.md §3): the canonical body text as a
/// `subquery_expr`, with the body's nesting depth (§4.1) — the deepest level its parse reached,
/// the body's own `subquery_expr` level included. Every token position the AST records
/// (`TableRefPos`, `StarPos`) is an index into `lex(text)`, which is what the rename rewrites and
/// `*` freezing edit.
pub fn parse_view_definition(text: &str) -> Result<(QueryExpr, usize)> {
    parse_view_definition_at(text, &[])
}

/// [`parse_view_definition`] for a body just written by the host, recording `positions` (one per
/// token of `lex(text)`, as [`CreateView::body_positions`](crate::ast::CreateView) holds) on the
/// AST so a resolution error points into the statement (spec/design/error-fields.md §10.1). A
/// position list that does not line up with the tokens is ignored.
pub fn parse_view_definition_at(text: &str, positions: &[u32]) -> Result<(QueryExpr, usize)> {
    let tokens = lex(text)?;
    let positions = if positions.len() == tokens.len() {
        positions.to_vec()
    } else {
        Vec::new()
    };
    let mut p = Parser::with_positions(tokens, positions);
    let body = p.parse_subquery()?;
    p.expect_eof()?;
    Ok((body, p.max_depth))
}

/// Re-render a token slice as the persisted check-expression text: each token rendered by
/// the closed table in spec/fileformat/format.md "Check-expression text", joined with
/// single spaces. A byte contract — identical across every core (CLAUDE.md §8).
pub fn render_tokens(tokens: &[Token]) -> String {
    let parts: Vec<String> = tokens.iter().map(render_token).collect();
    parts.join(" ")
}

/// Rewrite references to one table column in persisted expression text, then reparse the result.
/// Stored expressions have no source positions, so this walks the canonical token stream: bare
/// identifiers and `table.column` names are columns; function/type/named-argument identifiers and
/// composite-field names are not (alter.md §2.2).
pub(crate) fn rewrite_column_identifier(
    text: &str,
    table: &str,
    old: &str,
    new: &str,
) -> Result<(String, Expr)> {
    let mut tokens = lex(text)?;
    for i in 0..tokens.len() {
        if !token_names(&tokens[i], old) {
            continue;
        }
        let prev = i.checked_sub(1).and_then(|j| tokens.get(j));
        let prev2 = i.checked_sub(2).and_then(|j| tokens.get(j));
        let next = tokens.get(i + 1);
        let skip = matches!(next, Some(Token::LParen | Token::FatArrow | Token::Str(_)))
            || matches!(prev, Some(Token::DoubleColon))
            || matches!(prev, Some(Token::Word(w)) if w == "as")
            || (matches!(prev, Some(Token::Dot)) && !prev2.is_some_and(|t| token_names(t, table)))
            || (matches!(next, Some(Token::Dot)) && old == table);
        if !skip {
            tokens[i] = name_token(new);
        }
    }
    if matches!(tokens.last(), Some(Token::Eof)) {
        tokens.pop();
    }
    let rewritten = render_tokens(&tokens);
    let expr = parse_expression(&rewritten)?;
    Ok((rewritten, expr))
}

/// Rewrite a persisted qualified reference's table label during `RENAME TO`.
pub(crate) fn rewrite_table_qualifier(text: &str, old: &str, new: &str) -> Result<(String, Expr)> {
    let mut tokens = lex(text)?;
    for i in 0..tokens.len().saturating_sub(1) {
        if token_names(&tokens[i], old) && matches!(tokens.get(i + 1), Some(Token::Dot)) {
            tokens[i] = name_token(new);
        }
    }
    if matches!(tokens.last(), Some(Token::Eof)) {
        tokens.pop();
    }
    let rewritten = render_tokens(&tokens);
    let expr = parse_expression(&rewritten)?;
    Ok((rewritten, expr))
}

/// Parse a name-valued TEXT argument as exactly one identifier — the sequence functions'
/// `nextval('s')` regclass text form (spec/design/grammar.md §3), PostgreSQL's
/// `SplitIdentifierString` rule: surrounding whitespace (space, tab, LF, CR, FF) is ignored; a
/// double-quoted name is exact (`""` an embedded quote; `'""'` is the empty name, which never
/// exists); otherwise the run of characters up to whitespace or `.` is a bare name, folded to
/// lowercase (ASCII). Empty / all-whitespace text, an unterminated quote, anything after the name
/// (internal whitespace, trailing characters), or a dotted name is `42602 invalid_name` — jed has no
/// schemas and does not resolve a qualified name from text. Not length-checked (cost.md §7).
pub fn parse_name_text(text: &str) -> Result<String> {
    let invalid = || EngineError::new(SqlState::InvalidName, "invalid name syntax");
    let is_ws = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0c}');
    let s = text.trim_start_matches(is_ws);
    let (name, rest) = if let Some(body) = s.strip_prefix('"') {
        let mut name = String::new();
        let mut chars = body.char_indices();
        let rest = loop {
            match chars.next() {
                None => return Err(invalid()),
                Some((i, '"')) => {
                    if body[i + 1..].starts_with('"') {
                        name.push('"');
                        chars.next();
                    } else {
                        break &body[i + 1..];
                    }
                }
                Some((_, c)) => name.push(c),
            }
        };
        (name, rest)
    } else {
        let end = s.find(|c: char| is_ws(c) || c == '.').unwrap_or(s.len());
        if end == 0 {
            return Err(invalid());
        }
        (s[..end].to_ascii_lowercase(), &s[end..])
    };
    if !rest.trim_start_matches(is_ws).is_empty() {
        return Err(invalid());
    }
    Ok(name)
}

/// Whether an identifier token (bare word or quoted) spells exactly `name` (byte-exact after the
/// lexer's fold, grammar.md §3).
fn token_names(t: &Token, name: &str) -> bool {
    matches!(t, Token::Word(w) | Token::QuotedIdent(w) if w == name)
}

/// Whether `name` survives the lexer written bare: `[a-z_][a-z0-9_]*` (grammar.md §3's quoting
/// rule). Any other name must be rendered double-quoted.
pub(crate) fn is_plain_identifier(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_lowercase() || b[0] == b'_')
        && b.iter()
            .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}

/// The token a RENAME rewrite substitutes into persisted expression text: ALWAYS a quoted
/// identifier, even for a plain lowercase name — jed has no reserved keywords, so a bare `null` /
/// `true` would re-lex as the literal (grammar.md §3, alter.md §2.2). Untouched tokens keep their
/// stored spelling.
fn name_token(name: &str) -> Token {
    Token::QuotedIdent(name.to_string())
}

/// Render `name` as SQL identifier text by grammar.md §3's quoting rule: bare when it matches
/// `[a-z_][a-z0-9_]*`, else double-quoted with each `"` doubled.
pub fn quote_identifier(name: &str) -> String {
    if is_plain_identifier(name) {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
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
        // A double-quoted identifier round-trips verbatim with `"` doubled (format v37; a bare
        // word above renders as its folded lowercase spelling).
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

/// Whether a token can be an identifier: a bare word or a quoted identifier (grammar.md §3).
fn is_ident(t: &Token) -> bool {
    matches!(t, Token::Word(_) | Token::QuotedIdent(_))
}

fn syntax(msg: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::SyntaxError, msg.into())
}

/// Remove the parentheses from an unaliased parenthesized join that BEGINS a left-deep chain
/// (spec/design/grammar.md §15): `(a JOIN b ON p) JOIN c ON q` is exactly `a JOIN b ON p JOIN c ON
/// q`, the same join tree with the same ON scopes, so the group's chain becomes the chain's prefix
/// and `joins` follow it. Any other item is returned unchanged.
fn splice_leading_group(item: TableRef, joins: Vec<JoinClause>) -> (TableRef, Vec<JoinClause>) {
    let is_group = item.alias.is_none()
        && matches!(item.subquery.as_deref(), Some(QueryExpr::Select(sel)) if sel.join_group);
    if !is_group {
        return (item, joins);
    }
    let Some(QueryExpr::Select(group)) = item.subquery.map(|b| *b) else {
        unreachable!("a join group's body is a SELECT")
    };
    let group = *group;
    let mut out = group.joins;
    out.extend(joins);
    (group.from.expect("a join group has a FROM item"), out)
}

/// The body of a parenthesized join (grammar.md §15): a `SELECT *` over its chain. Unaliased, the
/// caller marks it `join_group`.
fn join_group_body(from: TableRef, joins: Vec<JoinClause>) -> QueryExpr {
    QueryExpr::Select(Box::new(Select {
        distinct: false,
        items: SelectItems::All(StarPos(None)),
        from: Some(from),
        joins,
        filter: None,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        windows: Vec::new(),
        join_group: false,
        paren_join: true,
        tail: Vec::new(),
    }))
}

/// Make a later comma item whose chain holds a `RIGHT` or `FULL` join the unaliased parenthesized
/// join it is (grammar.md §15): `FROM t, a RIGHT JOIN b ON p` is `FROM t, (a RIGHT JOIN b ON p)`.
/// Flattened into the left-deep chain, `(t × a) RIGHT JOIN b` would keep an unmatched `b` row once
/// rather than once per `t` row. `seg` indexes the item's `comma` join; the joins after it are its
/// chain. An item of only inner and `LEFT` joins stays flattened — the same rows either way.
fn group_outer_join_item(joins: &mut Vec<JoinClause>, seg: usize) {
    let outer = joins[seg + 1..]
        .iter()
        .any(|j| matches!(j.kind, JoinKind::Right | JoinKind::Full));
    if !outer {
        return;
    }
    let chain = joins.split_off(seg + 1);
    let head = joins.pop().expect("the item's comma join");
    let mut body = join_group_body(head.table, chain);
    if let QueryExpr::Select(sel) = &mut body {
        sel.join_group = true;
    }
    joins.push(JoinClause {
        table: TableRef {
            name: String::new(),
            db: None,
            alias: None,
            args: None,
            arg_names: None,
            subquery: Some(Box::new(body)),
            values: None,
            column_aliases: None,
            column_defs: None,
            json_table: None,
            lateral: false,
            pos: TableRefPos::default(),
        },
        ..head
    });
}

/// The label of a `VALUES` query's relation (PostgreSQL's `"*VALUES*"`).
const VALUES_LABEL: &str = "*VALUES*";

/// A heap-allocated SELECT with no clauses, built in its own frame so `parse_select_core` does not
/// hold a `Select`-sized temporary on the recursive path (spec/design/cost.md §7.1).
#[inline(never)]
fn empty_select(distinct: bool) -> Box<Select> {
    Box::new(Select {
        distinct,
        items: SelectItems::Items(Vec::new()),
        from: None,
        joins: Vec::new(),
        filter: None,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        windows: Vec::new(),
        join_group: false,
        paren_join: false,
        tail: Vec::new(),
    })
}

/// A `VALUES` query (grammar.md §25) — a statement, a set-operation operand, or a VALUES body with
/// trailing `ORDER BY` / `LIMIT` / `OFFSET` (§42) — as the query `SELECT * FROM (VALUES …) ORDER BY …
/// LIMIT … OFFSET …` over an inner VALUES relation labeled `*VALUES*` (PostgreSQL's name for it):
/// the keys resolve against `column1`, `column2`, … by name, as `"*VALUES*".column1`, or by
/// ordinal, and an enclosing derived table's alias and column-rename list apply to the query's
/// output unchanged (`v.column1` in the keys is 42P01, as in PostgreSQL).
fn values_query(
    rows: Vec<Vec<Expr>>,
    order_by: Vec<OrderKey>,
    limit: Option<i64>,
    offset: Option<i64>,
) -> QueryExpr {
    let inner = TableRef {
        name: VALUES_LABEL.to_string(),
        db: None,
        alias: Some(VALUES_LABEL.to_string()),
        args: None,
        arg_names: None,
        subquery: None,
        values: Some(rows),
        column_aliases: None,
        column_defs: None,
        json_table: None,
        lateral: false,
        pos: TableRefPos::default(),
    };
    QueryExpr::Select(Box::new(Select {
        distinct: false,
        items: SelectItems::All(StarPos(None)),
        from: Some(inner),
        joins: Vec::new(),
        filter: None,
        group_by: Vec::new(),
        having: None,
        order_by,
        limit,
        offset,
        windows: Vec::new(),
        join_group: false,
        paren_join: false,
        tail: Vec::new(),
    }))
}

/// Fold a query expression's trailing `ORDER BY` / `LIMIT` / `OFFSET` onto its node (grammar.md
/// §25). A parenthesized query is the SAME query as its contents (PostgreSQL), so its own trailing
/// clauses are already set: giving one of them a second time — `(SELECT … ORDER BY a) ORDER BY b`,
/// `(SELECT … LIMIT 2) LIMIT 1` — is 42601, while a clause the contents lack is simply added. A
/// parenthesized nested `WITH` folds onto its main query.
fn fold_trailing(
    node: QueryExpr,
    order_by: Vec<OrderKey>,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<QueryExpr> {
    fn fold(
        cur_order: &mut Vec<OrderKey>,
        cur_limit: &mut Option<i64>,
        cur_offset: &mut Option<i64>,
        order_by: Vec<OrderKey>,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<()> {
        if !order_by.is_empty() {
            if !cur_order.is_empty() {
                return Err(syntax("multiple ORDER BY clauses not allowed"));
            }
            *cur_order = order_by;
        }
        if offset.is_some() {
            if cur_offset.is_some() {
                return Err(syntax("multiple OFFSET clauses not allowed"));
            }
            *cur_offset = offset;
        }
        if limit.is_some() {
            if cur_limit.is_some() {
                return Err(syntax("multiple LIMIT clauses not allowed"));
            }
            *cur_limit = limit;
        }
        Ok(())
    }
    Ok(match node {
        QueryExpr::Select(mut sel) => {
            fold(
                &mut sel.order_by,
                &mut sel.limit,
                &mut sel.offset,
                order_by,
                limit,
                offset,
            )?;
            QueryExpr::Select(sel)
        }
        QueryExpr::SetOp(mut so) => {
            fold(
                &mut so.order_by,
                &mut so.limit,
                &mut so.offset,
                order_by,
                limit,
                offset,
            )?;
            QueryExpr::SetOp(so)
        }
        QueryExpr::With(we) => {
            let WithExpr {
                ctes,
                recursive,
                body,
            } = *we;
            let body = Box::new(fold_trailing(*body, order_by, limit, offset)?);
            QueryExpr::With(Box::new(WithExpr {
                ctes,
                recursive,
                body,
            }))
        }
    })
}

/// A `WITH` clause whose main query is itself a parenthesized `WITH` query — `WITH a AS (…)
/// (WITH b AS (…) SELECT …)` — is 42601 (PostgreSQL: both clauses would attach to one query). A
/// parenthesized `WITH` query as a set-operation operand is a separate query and is fine.
fn reject_second_with(body: &QueryExpr) -> Result<()> {
    if matches!(body, QueryExpr::With(_)) {
        return Err(syntax("multiple WITH clauses not allowed"));
    }
    Ok(())
}

/// Whether `kw` (already folded) may NOT be a select item's bare output label — after a complete
/// expression it needs `AS`. Exactly PostgreSQL's `AS_LABEL` keywords (kwlist.h): words that begin
/// a clause after a select item, continue a type name, or introduce an interval field or a
/// window/aggregate postfix. Any other bare word after an expression is the item's output label.
/// This set is a CLAUDE.md §8 cross-core determinism surface (spec/design/grammar.md §8).
fn is_bare_label_stop_keyword(kw: &str) -> bool {
    matches!(
        kw,
        "array"
            | "as"
            | "char"
            | "character"
            | "create"
            | "day"
            | "except"
            | "fetch"
            | "filter"
            | "for"
            | "from"
            | "grant"
            | "group"
            | "having"
            | "hour"
            | "intersect"
            | "into"
            | "isnull"
            | "limit"
            | "minute"
            | "month"
            | "notnull"
            | "offset"
            | "on"
            | "order"
            | "over"
            | "overlaps"
            | "precision"
            | "returning"
            | "second"
            | "to"
            | "union"
            | "varying"
            | "where"
            | "window"
            | "with"
            | "within"
            | "without"
            | "year"
    )
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
fn binary(op: BinaryOp, lhs: Expr, rhs: Expr, pos: SrcPos) -> Expr {
    Expr::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
        pos,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Keywords are legal identifiers in jed (a deliberate PostgreSQL divergence). UPDATE's
    // DEFAULT special form must yield to an ordinary expression when the RHS continues.
    #[test]
    fn update_default_keyword_with_continuing_rhs_is_column() {
        let Statement::Update(upd) =
            Parser::parse_sql("UPDATE t SET result = default + 1").unwrap()
        else {
            panic!("expected UPDATE");
        };
        let assignment = &upd.assignments[0];
        assert!(!assignment.is_default);
        let Expr::Binary { lhs, .. } = &assignment.value else {
            panic!("expected binary RHS");
        };
        assert!(matches!(lhs.as_ref(), Expr::Column(name, _) if name == "default"));
    }
}

/// The canonical name of a SQL/JSON `ENCODING` clause's encoding (json-sql-functions.md §5.4):
/// `UTF8`, `UTF16`, or `UTF32`, matched case-insensitively as PostgreSQL's `makeJsonEncoding` does.
/// Any other name is `22023`.
fn json_encoding_name(name: &str) -> Result<String> {
    match name.to_ascii_lowercase().as_str() {
        "utf8" => Ok("UTF8".to_string()),
        "utf16" => Ok("UTF16".to_string()),
        "utf32" => Ok("UTF32".to_string()),
        _ => Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("unrecognized JSON encoding: {name}"),
        )),
    }
}

/// `CREATE UNLOGGED TABLE` and `SELECT … INTO UNLOGGED` (create-table-as.md §6): jed has no unlogged
/// tables — every table is durable (CLAUDE.md §9) — so the form is recognized and refused.
fn unlogged_not_supported() -> EngineError {
    EngineError::new(
        SqlState::FeatureNotSupported,
        "unlogged tables are not supported",
    )
}
