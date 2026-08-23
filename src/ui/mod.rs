//! Rendering. Pure functions of `App` state — no I/O, no blocking.
//!
//! Layout, top to bottom:
//!
//! ```text
//! ┌──────────────────────────────────────────┐
//! │ connection bar                           │  1 line
//! ├────────────────┬─────────────────────────┤
//! │ CATALOG (30%)  │ EDITOR                  │
//! │                ├─────────────────────────┤
//! │                │ RESULTS                 │
//! ├────────────────┴─────────────────────────┤
//! │ status                                   │  1 line
//! └──────────────────────────────────────────┘
//! ```

pub mod catalog;
pub mod connbar;
pub mod editor;
pub mod modal;
pub mod results;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::app::App;
use crate::types::Pane;

/// Accent used for the focused pane's border and for selections.
pub const ACCENT: Color = Color::Cyan;
/// Everything unfocused.
pub const CHROME: Color = Color::DarkGray;

/// Braille spinner frames, advanced by `App::spinner`.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The spinner glyph for a given tick.
pub fn spinner_frame(tick: usize) -> &'static str {
    SPINNER[tick % SPINNER.len()]
}

/// A pane frame. The focused pane gets a thick accent border so the keyboard
/// location is never ambiguous.
pub fn pane_block(title: impl Into<String>, focused: bool) -> Block<'static> {
    let (border_type, border_style, title_style) = if focused {
        (
            BorderType::Thick,
            Style::default().fg(ACCENT),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    } else {
        (
            BorderType::Plain,
            Style::default().fg(CHROME),
            Style::default().fg(CHROME),
        )
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(border_type)
        .border_style(border_style)
        .title(Span::styled(format!(" {} ", title.into()), title_style))
}

/// Human name of a pane, for the status bar.
pub fn pane_name(pane: Pane) -> &'static str {
    match pane {
        Pane::Catalog => "catalog",
        Pane::Editor => "editor",
        Pane::Results => "results",
    }
}

/// Clip a line to `max` display columns, appending `…` when it overflows.
pub fn truncate_line(line: Line<'static>, max: usize) -> Line<'static> {
    if max == 0 {
        return Line::from(Vec::<Span<'static>>::new());
    }
    if line.width() <= max {
        return line;
    }
    let budget = max.saturating_sub(1);
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in line.spans {
        if used >= budget {
            break;
        }
        let w = span.width();
        if used + w <= budget {
            used += w;
            out.push(span);
        } else {
            let room = budget - used;
            let mut taken = String::new();
            let mut taken_w = 0usize;
            for ch in span.content.chars() {
                let cw = Span::raw(ch.to_string()).width();
                if taken_w + cw > room {
                    break;
                }
                taken_w += cw;
                taken.push(ch);
            }
            out.push(Span::styled(taken, span.style));
            break;
        }
    }
    out.push(Span::styled("…", Style::default().fg(CHROME)));
    Line::from(out)
}

/// Draw the whole frame: connection bar, catalog pane, editor, results, status.
///
/// Everything below is defensive about tiny areas — a resize must never panic.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }

    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);
    let (bar, body, status) = (chunks[0], chunks[1], chunks[2]);

    if bar.height > 0 {
        connbar::draw(frame, app, bar);
    }

    let mut editor_area = Rect::new(area.x, area.y, 0, 0);
    let mut cursor: Option<(u16, u16)> = None;

    if body.height > 0 && body.width > 0 {
        let cols = Layout::horizontal([Constraint::Percentage(30), Constraint::Min(0)]).split(body);
        catalog::draw(frame, app, cols[0]);

        let right = cols[1];
        if right.width > 0 && right.height > 0 {
            let rows = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(right);
            editor_area = rows[0];
            cursor = editor::draw(frame, app, rows[0]);
            results::draw(frame, app, rows[1]);
        }
    }

    if status.height > 0 {
        draw_status(frame, app, status);
    }

    // Overlays last, so they sit on top of everything.
    editor::draw_completion(frame, app, cursor, editor_area, area);
    modal::draw(frame, app, area);
}

/// Bottom line: `status · focus · rows/elapsed/profile`.
fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let running = app.tab().running;
    let mut left: Vec<Span<'static>> = Vec::new();
    if running {
        left.push(Span::styled(
            format!("{} ", spinner_frame(app.spinner)),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    }
    left.push(Span::styled(
        app.status.clone(),
        Style::default().fg(Color::White),
    ));

    let mut right_bits: Vec<String> = Vec::new();
    if let Some(result) = app.tab().result.as_ref() {
        right_bits.push(format!("{} rows", result.row_count));
        right_bits.push(format!("{}ms", result.elapsed.as_millis()));
    }
    right_bits.push(pane_name(app.focus).to_string());
    if let Some(id) = app.active_profile_id() {
        right_bits.push(id);
    }
    let right = right_bits.join(" · ");

    let halves = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length((right.chars().count() as u16 + 1).min(area.width)),
    ])
    .split(area);

    frame.render_widget(
        Paragraph::new(truncate_line(Line::from(left), halves[0].width as usize))
            .style(Style::default().bg(Color::Reset)),
        halves[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(right, Style::default().fg(CHROME))))
            .right_aligned(),
        halves[1],
    );
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::mpsc::Receiver;

    use crate::app::App;
    use crate::db::{ConnectionManager, DbEvent};

    /// An `App` with no profiles and no connections. The receiver must be kept
    /// alive or the manager's sender half dangles.
    pub fn app() -> (App, Receiver<DbEvent>) {
        let (manager, rx) = ConnectionManager::new();
        (App::new(Vec::new(), manager), rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(width: u16, height: u16, app: &mut App) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let area = buf.area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn draws_empty_app_at_normal_size() {
        let (mut app, _rx) = test_support::app();
        let buf = render(80, 24, &mut app);
        let text = buffer_text(&buf);
        assert!(text.contains("CATALOG"), "{text}");
        assert!(text.contains("EDITOR"), "{text}");
        assert!(text.contains("RESULTS"), "{text}");
        assert!(text.contains("ready"), "{text}");
    }

    #[test]
    fn draws_at_degenerate_sizes_without_panicking() {
        let (mut app, _rx) = test_support::app();
        for (w, h) in [(10u16, 4u16), (1, 1), (2, 2), (5, 3), (80, 1), (1, 40)] {
            let _ = render(w, h, &mut app);
        }
    }

    #[test]
    fn draws_every_modal_without_panicking() {
        use crate::app::{CommandPalette, Confirm, ExportDialog, Modal, ProfileForm};
        use crate::types::ExportFormat;

        let (mut app, _rx) = test_support::app();
        let modals = [
            Modal::Profile(ProfileForm::blank()),
            Modal::Export(ExportDialog {
                format: ExportFormat::Csv,
                path: "/tmp/out.csv".into(),
                error: Some("nope".into()),
                editing_path: true,
            }),
            Modal::Palette(CommandPalette {
                query: "con".into(),
                selected: 0,
                matches: crate::app::Command::ALL.to_vec(),
            }),
            Modal::Confirm(Confirm {
                message: "delete profile?".into(),
                action: crate::app::Command::DeleteProfile,
            }),
        ];
        for modal in modals {
            app.modal = modal;
            let _ = render(80, 24, &mut app);
            let _ = render(12, 5, &mut app);
        }
    }

    #[test]
    fn truncate_line_appends_ellipsis() {
        let line = Line::from(vec![Span::raw("hello"), Span::raw(" world")]);
        let out = truncate_line(line, 6);
        assert_eq!(out.width(), 6);
        let joined: String = out.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "hello…");
    }

    #[test]
    fn truncate_line_is_identity_when_it_fits() {
        let line = Line::from("abc");
        assert_eq!(truncate_line(line, 10).width(), 3);
        assert_eq!(truncate_line(Line::from("abc"), 0).width(), 0);
    }

    #[test]
    fn spinner_cycles() {
        assert_eq!(spinner_frame(0), SPINNER[0]);
        assert_eq!(spinner_frame(SPINNER.len()), SPINNER[0]);
        assert_eq!(spinner_frame(SPINNER.len() * 3 + 4), SPINNER[4]);
    }
}
