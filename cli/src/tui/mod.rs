//! TUI mode (spec/design/cli.md §6): terminal setup/teardown + the event loop. All
//! state and key dispatch live in `App` (app.rs); all rendering in draw.rs. The TUI
//! layer holds no execution logic — script mode and the TUI share `Session`.

mod app;
mod draw;
mod grid;
mod history;
mod schema;

use std::io;

use crossterm::event::{self, Event};

use crate::session::Session;
use app::App;

/// Run the full-screen TUI until quit. `ratatui::init` installs the panic hook that
/// restores the terminal, so a panic never leaves the shell in raw mode.
pub fn run(session: Session) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let mut app = App::new(session);
    let result = loop {
        if let Err(e) = terminal.draw(|frame| draw::draw(frame, &mut app)) {
            break Err(e);
        }
        match event::read() {
            Ok(Event::Key(key)) => app.on_key(key),
            Ok(_) => {} // resize redraws on the next loop pass; mouse/paste are unused
            Err(e) => break Err(e),
        }
        if app.quit {
            break Ok(());
        }
    };
    ratatui::restore();
    result
}
