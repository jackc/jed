//! Hand-written recursive-descent parser (CLAUDE.md §5, §10).
//!
//! Statement productions are filled in feature-by-feature (Phases B–E). Until a
//! production is implemented it returns a structured `0A000` feature-not-supported
//! error rather than panicking, so the harness reports "not yet" cleanly.

use crate::ast::{
    Assignment, ColumnDef, CompareOp, CreateTable, Delete, Insert, Literal, Operand, OrderBy,
    Predicate, Select, SelectExpr, SelectItems, Statement, Update,
};
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
            Some("insert") => Ok(Statement::Insert(self.parse_insert()?)),
            Some("select") => Ok(Statement::Select(self.parse_select()?)),
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
        // Optional `PRIMARY KEY`.
        let primary_key = if self.peek_keyword().as_deref() == Some("primary") {
            self.advance();
            self.expect_keyword("key")?;
            true
        } else {
            false
        };
        Ok(ColumnDef {
            name,
            type_name,
            primary_key,
        })
    }

    /// `INSERT INTO <table> VALUES ( <literal> [, <literal>]* )`. Values map
    /// positionally to columns; the executor type-checks against the catalog.
    fn parse_insert(&mut self) -> Result<Insert> {
        self.expect_keyword("insert")?;
        self.expect_keyword("into")?;
        let table = self.expect_identifier()?;
        self.expect_keyword("values")?;
        self.expect(&Token::LParen)?;

        let mut values = Vec::new();
        loop {
            values.push(self.parse_literal()?);
            match self.advance() {
                Token::Comma => continue,
                Token::RParen => break,
                other => return Err(syntax(format!("expected ',' or ')', found {other:?}"))),
            }
        }
        if values.is_empty() {
            return Err(syntax("VALUES must have at least one value"));
        }
        Ok(Insert { table, values })
    }

    /// A literal: an integer (already lexed) or the keyword `NULL`.
    fn parse_literal(&mut self) -> Result<Literal> {
        match self.advance() {
            Token::Int(n) => Ok(Literal::Int(n)),
            Token::Word(w) if w.eq_ignore_ascii_case("null") => Ok(Literal::Null),
            other => Err(syntax(format!("expected a literal value, found {other:?}"))),
        }
    }

    /// `SELECT <items> FROM <table> [WHERE <predicate>] [ORDER BY <col> [ASC|DESC]]`,
    /// where `<items>` is `*` or a comma-separated list of column refs / CASTs.
    fn parse_select(&mut self) -> Result<Select> {
        self.expect_keyword("select")?;
        let items = self.parse_select_items()?;
        self.expect_keyword("from")?;
        let from = self.expect_identifier()?;

        let filter = self.parse_optional_where()?;

        let order_by = if self.peek_keyword().as_deref() == Some("order") {
            self.advance();
            self.expect_keyword("by")?;
            let column = self.expect_identifier()?;
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
            Some(OrderBy { column, descending })
        } else {
            None
        };

        Ok(Select {
            items,
            from,
            filter,
            order_by,
        })
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
            let value = self.parse_operand()?;
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

    /// Parse an optional trailing `WHERE <predicate>` (shared by SELECT/UPDATE/DELETE).
    fn parse_optional_where(&mut self) -> Result<Option<Predicate>> {
        if self.peek_keyword().as_deref() == Some("where") {
            self.advance();
            Ok(Some(self.parse_predicate()?))
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
            items.push(self.parse_select_expr()?);
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(SelectItems::Items(items))
    }

    /// A projected expression: `CAST ( <expr> AS <type> )` or a bare column name.
    fn parse_select_expr(&mut self) -> Result<SelectExpr> {
        if self.peek_keyword().as_deref() == Some("cast") {
            self.advance();
            self.expect(&Token::LParen)?;
            let inner = self.parse_select_expr()?;
            self.expect_keyword("as")?;
            let type_name = self.expect_identifier()?;
            self.expect(&Token::RParen)?;
            return Ok(SelectExpr::Cast {
                inner: Box::new(inner),
                type_name,
            });
        }
        // A bare integer literal is allowed as a projected value (used by CAST
        // tests like `SELECT CAST(1000 AS int16)`).
        if let Token::Int(n) = self.peek() {
            let n = *n;
            self.advance();
            return Ok(SelectExpr::Literal(Literal::Int(n)));
        }
        Ok(SelectExpr::Column(self.expect_identifier()?))
    }

    /// A WHERE predicate: `<col> IS [NOT] NULL` or `<col> <cmp> <operand>`, where
    /// `<operand>` is another column or a literal.
    fn parse_predicate(&mut self) -> Result<Predicate> {
        let column = self.expect_identifier()?;
        if self.peek_keyword().as_deref() == Some("is") {
            self.advance();
            let negated = if self.peek_keyword().as_deref() == Some("not") {
                self.advance();
                true
            } else {
                false
            };
            self.expect_keyword("null")?;
            return Ok(Predicate::IsNull { column, negated });
        }
        let op = match self.advance() {
            Token::Eq => CompareOp::Eq,
            Token::Lt => CompareOp::Lt,
            Token::Gt => CompareOp::Gt,
            Token::Le => CompareOp::Le,
            Token::Ge => CompareOp::Ge,
            other => {
                return Err(syntax(format!(
                    "expected a comparison operator, found {other:?}"
                )));
            }
        };
        let rhs = self.parse_operand()?;
        Ok(Predicate::Compare { column, op, rhs })
    }

    /// A comparison operand: a literal (integer or NULL) or a column reference.
    fn parse_operand(&mut self) -> Result<Operand> {
        match self.peek() {
            Token::Int(n) => {
                let n = *n;
                self.advance();
                Ok(Operand::Literal(Literal::Int(n)))
            }
            Token::Word(w) if w.eq_ignore_ascii_case("null") => {
                self.advance();
                Ok(Operand::Literal(Literal::Null))
            }
            Token::Word(_) => Ok(Operand::Column(self.expect_identifier()?)),
            other => Err(syntax(format!("expected an operand, found {other:?}"))),
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
