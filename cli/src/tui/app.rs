//! TUI application state + key dispatch (spec/design/cli.md §6).

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tui_textarea::TextArea;

use super::complete;
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

/// The open autocomplete popup (cli.md §6): the filtered candidates, the highlighted one,
/// and how many characters of the word were already typed (replaced on accept, so a
/// candidate completes in its canonical spelling).
pub struct Completion {
    pub items: Vec<String>,
    pub sel: usize,
    pub prefix_chars: usize,
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
    pub completion: Option<Completion>,
    /// The editor viewport's (row, col) scroll offsets — view-side bookkeeping kept here
    /// so draw.rs can keep the cursor visible (the editor is custom-rendered for syntax
    /// highlighting, so tui-textarea's internal scrolling is unused).
    pub editor_scroll: (usize, usize),
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
            completion: None,
            editor_scroll: (0, 0),
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
            Focus::Editor => {
                // The completion popup (cli.md §6) swallows its navigation keys; any
                // other key closes it and is processed normally below.
                if self.completion.is_some() {
                    match key.code {
                        KeyCode::Up => {
                            let c = self.completion.as_mut().expect("popup open");
                            c.sel = c.sel.saturating_sub(1);
                            return;
                        }
                        KeyCode::Down => {
                            let c = self.completion.as_mut().expect("popup open");
                            if c.sel + 1 < c.items.len() {
                                c.sel += 1;
                            }
                            return;
                        }
                        KeyCode::Enter | KeyCode::Tab => {
                            self.accept_completion();
                            return;
                        }
                        KeyCode::Esc => {
                            self.completion = None;
                            return;
                        }
                        _ => self.completion = None,
                    }
                }
                match key.code {
                    KeyCode::Esc => self.focus = Focus::Results,
                    KeyCode::Tab if !ctrl => self.trigger_completion(key),
                    _ => {
                        self.editor.input(key);
                    }
                }
            }
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

    /// Tab in the editor (cli.md §6): at a partial word, complete it from the catalog +
    /// grammar words — directly when one candidate matches, via the popup when several
    /// do. At a non-word position the key stays an ordinary Tab.
    fn trigger_completion(&mut self, key: KeyEvent) {
        let (row, col) = self.editor.cursor();
        let (_, prefix) = complete::current_word(&self.editor.lines()[row], col);
        let items = complete::candidates(&self.session.db, &prefix);
        match items.len() {
            0 => {
                self.editor.input(key);
            }
            1 => self.replace_word(prefix.chars().count(), &items[0].clone()),
            _ => {
                self.completion = Some(Completion {
                    items,
                    sel: 0,
                    prefix_chars: prefix.chars().count(),
                });
            }
        }
    }

    /// Accept the highlighted candidate: replace the typed prefix with the candidate's
    /// canonical spelling (so `use<Tab>` becomes `Users`, not `users`).
    fn accept_completion(&mut self) {
        let Some(c) = self.completion.take() else {
            return;
        };
        let candidate = c.items[c.sel].clone();
        self.replace_word(c.prefix_chars, &candidate);
    }

    fn replace_word(&mut self, prefix_chars: usize, candidate: &str) {
        for _ in 0..prefix_chars {
            self.editor.delete_char();
        }
        self.editor.insert_str(candidate);
    }

    /// Run the editor buffer: split, execute sequentially, stop at the first error
    /// (cli.md §6). The grid shows the LAST query result; the message line carries the
    /// final statement tag or the error.
    fn run_buffer(&mut self) {
        self.completion = None;
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
                Ok(ExecOutput::Statement { tag, cost, rows }) => {
                    self.set_message(crate::session::statement_line(tag, cost, rows), false);
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

#[cfg(test)]
mod tests {
    use super::*;
    use jed::{Engine, execute};

    fn app() -> App {
        let mut db = Engine::new();
        execute(
            &mut db,
            "CREATE TABLE Users (id i32 PRIMARY KEY, score i32)",
        )
        .unwrap();
        execute(&mut db, "CREATE TABLE selections (sel i32 PRIMARY KEY)").unwrap();
        App::new(Session::new(db, "memory".to_string()))
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn type_str(app: &mut App, s: &str) {
        for ch in s.chars() {
            press(app, KeyCode::Char(ch));
        }
    }

    fn editor_text(app: &App) -> String {
        app.editor.lines().join("\n")
    }

    #[test]
    fn tab_completes_a_unique_word_in_canonical_spelling() {
        let mut a = app();
        type_str(&mut a, "SELECT * FROM use");
        press(&mut a, KeyCode::Tab);
        assert_eq!(editor_text(&a), "SELECT * FROM Users");
        assert!(a.completion.is_none());
    }

    #[test]
    fn tab_opens_a_popup_for_multiple_candidates() {
        let mut a = app();
        type_str(&mut a, "sel");
        press(&mut a, KeyCode::Tab);
        let c = a.completion.as_ref().expect("popup open");
        assert_eq!(c.items, vec!["selections", "select"]);
        // Down selects the keyword; Enter accepts it, replacing the typed prefix.
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Enter);
        assert_eq!(editor_text(&a), "select");
        assert!(a.completion.is_none());
    }

    #[test]
    fn esc_closes_the_popup_and_typing_falls_through() {
        let mut a = app();
        type_str(&mut a, "sel");
        press(&mut a, KeyCode::Tab);
        press(&mut a, KeyCode::Esc);
        assert!(a.completion.is_none());
        assert_eq!(editor_text(&a), "sel"); // Esc closed the popup, not the editor focus
        assert_eq!(a.focus as usize, Focus::Editor as usize);

        // A non-navigation key closes the popup and is processed as input.
        press(&mut a, KeyCode::Tab);
        assert!(a.completion.is_some());
        press(&mut a, KeyCode::Char('x'));
        assert!(a.completion.is_none());
        assert_eq!(editor_text(&a), "selx");
    }

    #[test]
    fn tab_at_a_non_word_position_stays_a_tab() {
        let mut a = app();
        type_str(&mut a, "SELECT ");
        press(&mut a, KeyCode::Tab);
        assert!(a.completion.is_none());
        assert!(
            editor_text(&a).len() > "SELECT ".len(),
            "Tab should insert whitespace: {:?}",
            editor_text(&a)
        );
    }
}
