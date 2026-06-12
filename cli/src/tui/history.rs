//! Statement history (spec/design/cli.md §6): a session list persisted one entry per
//! line at `~/.jed_history` (`JED_HISTFILE` overrides; no HOME → in-session only).
//! Multi-line statements are flattened to one line at save — the v1 trade.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

pub struct History {
    entries: Vec<String>,
    path: Option<PathBuf>,
}

impl History {
    pub fn load() -> History {
        let path = std::env::var_os("JED_HISTFILE")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".jed_history")));
        let entries = match &path {
            Some(p) => std::fs::read_to_string(p)
                .map(|text| {
                    text.lines()
                        .filter(|l| !l.trim().is_empty())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default(),
            None => Vec::new(),
        };
        History { entries, path }
    }

    /// Oldest-first; the history modal shows them reversed (most recent first).
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn add(&mut self, sql: &str) {
        let flat = sql.split_whitespace().collect::<Vec<_>>().join(" ");
        if flat.is_empty() || self.entries.last().is_some_and(|last| *last == flat) {
            return;
        }
        if let Some(p) = &self.path {
            // Appending per statement keeps history across a crash; failure to write
            // (read-only HOME, etc.) silently degrades to in-session history.
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "{flat}");
            }
        }
        self.entries.push(flat);
    }
}
