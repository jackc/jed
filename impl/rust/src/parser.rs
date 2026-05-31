//! Hand-written recursive-descent parser (CLAUDE.md §5, §10).
//!
//! SCAFFOLD (step-5 Phase A): the token cursor and entry point exist; the
//! statement productions are filled in feature-by-feature (Phases B–E). Until a
//! production is implemented it returns a structured `0A000` feature-not-supported
//! error rather than panicking, so the harness reports "not yet" cleanly.

use crate::ast::Statement;
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
        // Dispatch on the leading keyword. Productions land in Phases B–E.
        match self.peek_keyword().as_deref() {
            Some("create") | Some("insert") | Some("select") => Err(not_supported(
                "SQL statement parsing is not implemented yet (step-5 Phase A scaffold)",
            )),
            Some(other) => Err(syntax(format!("unexpected keyword '{other}'"))),
            None => Err(syntax("expected a SQL statement")),
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

fn not_supported(msg: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::FeatureNotSupported, msg.into())
}
