//! The results grid: the last query result, pre-rendered, with two-axis scrolling
//! (spec/design/cli.md §6). Cell text comes from the shared render helpers so the grid
//! shows exactly what script mode prints.

use crossterm::event::KeyCode;
use jed::Value;

use crate::render;

#[derive(Default)]
pub struct Grid {
    pub columns: Vec<String>,
    /// Rendered cells, rows only (the header is `columns`).
    pub cells: Vec<Vec<String>>,
    pub widths: Vec<usize>,
    pub numeric: Vec<bool>,
    pub cost: i64,
    pub row_off: usize,
    pub col_off: usize,
    /// Whether any result has been set yet (an empty result is still a result).
    pub present: bool,
    /// The viewport height draw.rs last used — lets paging keys size their jump.
    pub page: usize,
}

impl Grid {
    pub fn set(&mut self, columns: &[String], rows: &[Vec<Value>], cost: i64) {
        let grid = render::rendered_grid(columns, rows);
        self.numeric = render::numeric_columns(columns, rows);
        self.widths = (0..columns.len())
            .map(|c| grid.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
            .collect();
        self.columns = columns.to_vec();
        self.cells = grid.into_iter().skip(1).collect();
        self.cost = cost;
        self.row_off = 0;
        self.col_off = 0;
        self.present = true;
    }

    pub fn on_key(&mut self, code: KeyCode) {
        let max_row = self.cells.len().saturating_sub(1);
        let max_col = self.columns.len().saturating_sub(1);
        let page = self.page.max(1);
        match code {
            KeyCode::Up => self.row_off = self.row_off.saturating_sub(1),
            KeyCode::Down => self.row_off = (self.row_off + 1).min(max_row),
            KeyCode::Left => self.col_off = self.col_off.saturating_sub(1),
            KeyCode::Right => self.col_off = (self.col_off + 1).min(max_col),
            KeyCode::PageUp => self.row_off = self.row_off.saturating_sub(page),
            KeyCode::PageDown => self.row_off = (self.row_off + page).min(max_row),
            KeyCode::Home => {
                self.row_off = 0;
                self.col_off = 0;
            }
            KeyCode::End => self.row_off = max_row,
            _ => {}
        }
    }
}
