//! Frame rendering (spec/design/cli.md §6): schema sidebar · query editor · message
//! line · results grid · status bar, plus the help and history overlays. Pure view —
//! the only state it writes back is viewport bookkeeping (scroll offsets, page size).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table};

use super::app::{App, Focus};
use crate::session::TxState;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [main, status] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());

    let (schema_area, right) = if app.show_schema {
        let [s, r] = Layout::horizontal([Constraint::Length(30), Constraint::Min(20)]).areas(main);
        (Some(s), r)
    } else {
        (None, main)
    };
    let [editor_area, message_area, grid_area] = Layout::vertical([
        Constraint::Percentage(40),
        Constraint::Length(1),
        Constraint::Min(3),
    ])
    .areas(right);

    if let Some(area) = schema_area {
        draw_schema(frame, app, area);
    }
    draw_editor(frame, app, editor_area);
    draw_message(frame, app, message_area);
    draw_grid(frame, app, grid_area);
    draw_status(frame, app, status);

    if app.help_open {
        draw_help(frame);
    }
    if app.history_open {
        draw_history(frame, app);
    }
}

fn border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn draw_schema(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Schema;
    let block = Block::bordered()
        .title(" Schema ")
        .border_style(border_style(focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let height = inner.height as usize;
    // Keep the selection visible.
    if app.schema.selected < app.schema.off {
        app.schema.off = app.schema.selected;
    } else if height > 0 && app.schema.selected >= app.schema.off + height {
        app.schema.off = app.schema.selected - height + 1;
    }

    let lines: Vec<Line> = app
        .schema
        .lines
        .iter()
        .enumerate()
        .skip(app.schema.off)
        .take(height)
        .map(|(i, l)| {
            let mut style = if l.table.is_some() {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            if focused && i == app.schema.selected {
                style = style.bg(Color::DarkGray);
            }
            Line::styled(l.text.clone(), style)
        })
        .collect();
    let body = if app.schema.lines.is_empty() {
        Paragraph::new(Line::styled(
            "(no tables)",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Paragraph::new(lines)
    };
    frame.render_widget(body, inner);
}

fn draw_editor(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Editor;
    app.editor.set_block(
        Block::bordered()
            .title(" Query — Ctrl+Enter / F5 runs ")
            .border_style(border_style(focused)),
    );
    frame.render_widget(&app.editor, area);
}

fn draw_message(frame: &mut Frame, app: &App, area: Rect) {
    let Some(msg) = &app.message else { return };
    let style = if msg.is_error {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Green)
    };
    frame.render_widget(
        Paragraph::new(Line::styled(format!(" {}", msg.text), style)),
        area,
    );
}

fn draw_grid(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Results;
    let grid = &mut app.grid;
    let mut block = Block::bordered()
        .title(" Results ")
        .border_style(border_style(focused));
    if grid.present {
        let n = grid.cells.len();
        let noun = if n == 1 { "row" } else { "rows" };
        block = block.title_bottom(format!(" {n} {noun} · cost {} ", grid.cost));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if !grid.present {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "No results yet — write SQL above and press Ctrl+Enter (F1: help)",
                Style::default().fg(Color::DarkGray),
            )),
            inner,
        );
        return;
    }

    // The header consumes one line; the rest is the row viewport (drives PgUp/PgDn).
    let body_height = (inner.height as usize).saturating_sub(1);
    grid.page = body_height.max(1);
    let col_off = grid.col_off.min(grid.columns.len().saturating_sub(1));

    let header = Row::new(
        grid.columns[col_off..]
            .iter()
            .map(|c| Cell::from(c.clone()))
            .collect::<Vec<_>>(),
    )
    .style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));

    let rows: Vec<Row> = grid
        .cells
        .iter()
        .skip(grid.row_off)
        .take(body_height)
        .map(|r| {
            Row::new(
                r[col_off..]
                    .iter()
                    .map(|cell| {
                        if cell == "NULL" {
                            Cell::from(Span::styled("NULL", Style::default().fg(Color::DarkGray)))
                        } else {
                            Cell::from(cell.clone())
                        }
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect();

    let widths: Vec<Constraint> = grid.widths[col_off..]
        .iter()
        .zip(&grid.columns[col_off..])
        .map(|(w, name)| Constraint::Length((*w).max(name.chars().count()) as u16))
        .collect();
    frame.render_widget(
        Table::new(rows, widths).header(header).column_spacing(2),
        inner,
    );
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let tx = match app.tx_state() {
        TxState::Autocommit => Span::raw("autocommit"),
        TxState::Open => Span::styled("TX OPEN", Style::default().fg(Color::Yellow)),
        TxState::Failed => Span::styled(
            "TX FAILED — ROLLBACK to recover",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    };
    let mut spans = vec![Span::raw(format!(" {} · ", app.session.source)), tx];
    let ceiling = app.session.db.max_cost();
    if ceiling > 0 {
        spans.push(Span::raw(format!(" · max_cost {ceiling}")));
    }
    spans.push(Span::styled(
        " · F1 help · Ctrl+Q quit",
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Black)),
        area,
    );
}

const HELP: &[&str] = &[
    "Ctrl+Enter / F5   run the editor buffer (statements end with ;)",
    "Esc               leave the editor (then Tab cycles panes)",
    "Tab / Shift+Tab   cycle focus: results → schema → editor",
    "Ctrl+R            statement history (Enter loads into the editor)",
    "Ctrl+S            toggle the schema sidebar",
    "arrows / PgUp/Dn  scroll the results grid (when focused)",
    "Enter (schema)    insert the selected table name into the editor",
    "F1 / ?            this help · Esc closes",
    "Ctrl+Q            quit (an open transaction is rolled back)",
    "",
    "jed cannot cancel a running statement; bound runaway queries",
    "with --max-cost N (deterministic 54P01 abort).",
];

fn draw_help(frame: &mut Frame) {
    let area = centered(frame.area(), 64, HELP.len() as u16 + 2);
    frame.render_widget(Clear, area);
    let lines: Vec<Line> = HELP.iter().map(|l| Line::raw(*l)).collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Keys ")),
        area,
    );
}

fn draw_history(frame: &mut Frame, app: &App) {
    let area = centered(frame.area(), 70, 14);
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(" History — Enter loads, Esc closes ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let height = inner.height as usize;
    let off = app.history_sel.saturating_sub(height.saturating_sub(1));
    let lines: Vec<Line> = app
        .history
        .entries()
        .iter()
        .rev()
        .enumerate()
        .skip(off)
        .take(height)
        .map(|(i, e)| {
            let style = if i == app.history_sel {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };
            Line::styled(e.clone(), style)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}
