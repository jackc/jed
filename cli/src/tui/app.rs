//! TUI application state + key dispatch (spec/design/cli.md §6).

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tui_textarea::TextArea;

use super::grid::Grid;
use super::history::History;
use super::schema::SchemaPane;
use crate::session::{ExecOutput, Session, TxState};
use crate::splitter;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Editor,
    Results,
    Schema,
}

/// One message-log line (the area between editor and grid).
pub struct Message {
    pub text: String,
    pub is_error: bool,
}

pub struct App {
    pub session: Session,
    pub editor: TextArea<'static>,
    pub focus: Focus,
    pub grid: Grid,
    pub schema: SchemaPane,
    pub message: Option<Message>,
    pub history: History,
    pub history_open: bool,
    pub history_sel: usize,
    pub help_open: bool,
    pub show_schema: bool,
    pub quit: bool,
}

impl App {
    pub fn new(session: Session) -> App {
        let mut editor = TextArea::default();
        editor.set_cursor_line_style(ratatui::style::Style::default());
        let mut schema = SchemaPane::default();
        schema.refresh(&session.db);
        App {
            session,
            editor,
            focus: Focus::Editor,
            grid: Grid::default(),
            schema,
            message: None,
            history: History::load(),
            history_open: false,
            history_sel: 0,
            help_open: false,
            show_schema: true,
            quit: false,
        }
    }

    pub fn tx_state(&self) -> TxState {
        self.session.tx_state()
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Overlays swallow keys first.
        if self.help_open {
            if matches!(key.code, KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('q')) {
                self.help_open = false;
            }
            return;
        }
        if self.history_open {
            self.on_history_key(key);
            return;
        }

        // Global keys.
        match key.code {
            KeyCode::Char('q') if ctrl => {
                self.quit = true;
                return;
            }
            KeyCode::F(1) => {
                self.help_open = true;
                return;
            }
            KeyCode::F(5) => {
                self.run_buffer();
                return;
            }
            KeyCode::Enter if ctrl => {
                self.run_buffer();
                return;
            }
            KeyCode::Char('r') if ctrl => {
                if !self.history.entries().is_empty() {
                    self.history_open = true;
                    self.history_sel = 0;
                }
                return;
            }
            KeyCode::Char('s') if ctrl => {
                self.show_schema = !self.show_schema;
                return;
            }
            _ => {}
        }

        match self.focus {
            Focus::Editor => match key.code {
                KeyCode::Esc => self.focus = Focus::Results,
                _ => {
                    self.editor.input(key);
                }
            },
            Focus::Results => match key.code {
                KeyCode::Tab => {
                    self.focus = if self.show_schema {
                        Focus::Schema
                    } else {
                        Focus::Editor
                    }
                }
                KeyCode::BackTab => self.focus = Focus::Editor,
                KeyCode::Char('q') => self.quit = true,
                KeyCode::Char('?') => self.help_open = true,
                KeyCode::Enter => self.focus = Focus::Editor,
                _ => self.grid.on_key(key.code),
            },
            Focus::Schema => match key.code {
                KeyCode::Tab => self.focus = Focus::Editor,
                KeyCode::BackTab => self.focus = Focus::Results,
                KeyCode::Char('q') => self.quit = true,
                KeyCode::Char('?') => self.help_open = true,
                KeyCode::Up => self.schema.move_sel(-1),
                KeyCode::Down => self.schema.move_sel(1),
                KeyCode::Enter => {
                    if let Some(name) = self.schema.selected_table() {
                        self.editor.insert_str(&name);
                        self.focus = Focus::Editor;
                    }
                }
                _ => {}
            },
        }
    }

    fn on_history_key(&mut self, key: KeyEvent) {
        let len = self.history.entries().len();
        match key.code {
            KeyCode::Esc => self.history_open = false,
            KeyCode::Up => self.history_sel = self.history_sel.saturating_sub(1),
            KeyCode::Down => {
                if self.history_sel + 1 < len {
                    self.history_sel += 1;
                }
            }
            KeyCode::Enter => {
                // Entries display most-recent-first; sel indexes that view.
                if let Some(entry) = self.history.entries().iter().rev().nth(self.history_sel) {
                    self.editor =
                        TextArea::from(entry.lines().map(String::from).collect::<Vec<_>>());
                    self.editor
                        .set_cursor_line_style(ratatui::style::Style::default());
                }
                self.history_open = false;
                self.focus = Focus::Editor;
            }
            _ => {}
        }
    }

    /// Run the editor buffer: split, execute sequentially, stop at the first error
    /// (cli.md §6). The grid shows the LAST query result; the message line carries the
    /// final statement tag or the error.
    fn run_buffer(&mut self) {
        let text = self.editor.lines().join("\n");
        let stmts = match splitter::split(&text) {
            Ok(stmts) => stmts,
            Err(e) => {
                self.set_message(
                    format!("ERROR 42601: {} (line {})", e.message, e.line),
                    true,
                );
                return;
            }
        };
        if stmts.is_empty() {
            return;
        }
        for stmt in stmts {
            self.history.add(&stmt.sql);
            match self.session.run(&stmt.sql) {
                Ok(ExecOutput::Statement { tag, cost }) => {
                    let text = if tag == "OK" {
                        format!("OK (cost {cost})")
                    } else {
                        tag.to_string()
                    };
                    self.set_message(text, false);
                }
                Ok(ExecOutput::Query {
                    columns,
                    rows,
                    cost,
                }) => {
                    let n = rows.len();
                    let noun = if n == 1 { "row" } else { "rows" };
                    self.set_message(format!("{n} {noun} (cost {cost})"), false);
                    self.grid.set(&columns, &rows, cost);
                }
                Err(e) => {
                    let hint = match e.code() {
                        "54P01" => "  (raise the ceiling: --max-cost)",
                        "25P02" => "  (ROLLBACK to end the failed transaction)",
                        _ => "",
                    };
                    self.set_message(format!("ERROR {}: {}{hint}", e.code(), e.message), true);
                    break;
                }
            }
        }
        self.schema.refresh(&self.session.db);
    }

    fn set_message(&mut self, text: String, is_error: bool) {
        self.message = Some(Message { text, is_error });
    }
}
