//! Centred, `Clear`ed popups for every `Modal` variant.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};

use crate::app::{App, CommandPalette, Confirm, ExportDialog, Modal, ProfileForm};
use crate::types::ExportFormat;
use crate::ui::{ACCENT, CHROME, truncate_line};

/// A `percent_x` × `percent_y` rectangle centred inside `area`.
pub fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let width = area.width.saturating_mul(percent_x) / 100;
    let height = area.height.saturating_mul(percent_y) / 100;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

/// Centre a box of an explicit size, shrinking to fit when the screen is small.
fn sized_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(w) / 2,
        area.y + area.height.saturating_sub(h) / 2,
        w,
        h,
    )
}

fn modal_block(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}

fn hint(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(CHROME).add_modifier(Modifier::DIM),
    ))
}

fn error_line(message: Option<&String>) -> Option<Line<'static>> {
    message.map(|m| {
        Line::from(Span::styled(
            m.clone(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))
    })
}

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    if area.width < 4 || area.height < 3 {
        return;
    }
    match &app.modal {
        Modal::None => {}
        Modal::Profile(form) => draw_profile(frame, form, area),
        Modal::Export(dialog) => draw_export(frame, dialog, area),
        Modal::Palette(palette) => draw_palette(frame, palette, area),
        Modal::Confirm(confirm) => draw_confirm(frame, confirm, area),
    }
}

// ---- profile form -------------------------------------------------------

/// One labelled field row. `▸` marks the selected field, secrets are masked.
pub fn field_line(
    label: &str,
    value: &str,
    selected: bool,
    secret: bool,
    label_width: usize,
) -> Line<'static> {
    let shown = if secret {
        "•".repeat(value.chars().count())
    } else {
        value.to_string()
    };
    let caret = if selected { "▸ " } else { "  " };
    let label_style = if selected {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(CHROME)
    };
    let value_style = if selected {
        Style::default()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    Line::from(vec![
        Span::styled(caret.to_string(), label_style),
        Span::styled(format!("{label:<label_width$} "), label_style),
        Span::styled(
            if selected {
                format!("{shown}▏")
            } else {
                shown
            },
            value_style,
        ),
    ])
}

fn draw_profile(frame: &mut Frame, form: &ProfileForm, area: Rect) {
    let title = match &form.editing {
        Some(id) => format!("edit profile · {id}"),
        None => "new profile".to_string(),
    };
    let height = form.fields.len() as u16 + 5;
    let rect = sized_rect(64, height, area);
    let block = modal_block(&title);
    let inner = block.inner(rect);

    let label_width = form
        .fields
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(8);

    let mut lines: Vec<Line<'static>> = form
        .fields
        .iter()
        .enumerate()
        .map(|(index, (label, value))| {
            field_line(
                label,
                value,
                index == form.selected,
                form.is_secret(index),
                label_width,
            )
        })
        .collect();
    lines.push(Line::raw(""));
    if let Some(err) = error_line(form.error.as_ref()) {
        lines.push(err);
    }
    lines.push(hint("Tab/↓ next · Enter save · Esc cancel"));

    let width = inner.width as usize;
    let lines: Vec<Line<'static>> = lines.into_iter().map(|l| truncate_line(l, width)).collect();

    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(block), rect);
}

// ---- export -------------------------------------------------------------

fn draw_export(frame: &mut Frame, dialog: &ExportDialog, area: Rect) {
    let rect = sized_rect(60, 9, area);
    let block = modal_block("export results");
    let inner = block.inner(rect);

    let mut format_spans: Vec<Span<'static>> =
        vec![Span::styled("  format  ", Style::default().fg(CHROME))];
    for format in ExportFormat::ALL {
        let selected = format == dialog.format;
        let style = if selected && !dialog.editing_path {
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else if selected {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(CHROME)
        };
        format_spans.push(Span::styled(format!(" {} ", format.label()), style));
        format_spans.push(Span::raw(" "));
    }

    let path_style = if dialog.editing_path {
        Style::default()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };

    let mut lines = vec![
        Line::from(format_spans),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  path    ", Style::default().fg(CHROME)),
            Span::styled(
                if dialog.editing_path {
                    format!("{}▏", dialog.path)
                } else {
                    dialog.path.clone()
                },
                path_style,
            ),
        ]),
        Line::raw(""),
    ];
    if let Some(err) = error_line(dialog.error.as_ref()) {
        lines.push(err);
    }
    lines.push(hint(
        "←/→ format · Tab edit path · Enter export · Esc cancel",
    ));

    let width = inner.width as usize;
    let lines: Vec<Line<'static>> = lines.into_iter().map(|l| truncate_line(l, width)).collect();

    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(block), rect);
}

// ---- command palette ----------------------------------------------------

fn draw_palette(frame: &mut Frame, palette: &CommandPalette, area: Rect) {
    let rows = palette.matches.len().clamp(1, 12) as u16;
    // The palette scales with the screen but never gets uselessly narrow.
    let wanted = centered_rect(60, 60, area);
    let rect = sized_rect(wanted.width.max(44), rows + 4, area);
    let block = modal_block("command");
    let inner = block.inner(rect);
    if inner.width == 0 || inner.height == 0 {
        frame.render_widget(Clear, rect);
        frame.render_widget(block, rect);
        return;
    }

    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);

    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(inner);

    frame.render_widget(
        Paragraph::new(truncate_line(
            Line::from(vec![
                Span::styled(": ", Style::default().fg(ACCENT)),
                Span::styled(
                    format!("{}▏", palette.query),
                    Style::default().fg(Color::White),
                ),
            ]),
            chunks[0].width as usize,
        )),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(hint("↑/↓ select · Enter run · Esc cancel")),
        chunks[1],
    );

    if chunks[2].height == 0 {
        return;
    }
    if palette.matches.is_empty() {
        frame.render_widget(
            Paragraph::new(hint("no matching command")).wrap(Wrap { trim: true }),
            chunks[2],
        );
        return;
    }
    let items: Vec<ListItem<'static>> = palette
        .matches
        .iter()
        .map(|c| ListItem::new(Line::from(format!("  {}", c.label()))))
        .collect();
    let list = List::new(items).highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD),
    );
    let mut state =
        ListState::default().with_selected(Some(palette.selected.min(palette.matches.len() - 1)));
    frame.render_stateful_widget(list, chunks[2], &mut state);
}

// ---- confirm ------------------------------------------------------------

fn draw_confirm(frame: &mut Frame, confirm: &Confirm, area: Rect) {
    let rect = sized_rect(52, 7, area);
    let block = modal_block("confirm");
    let lines = vec![
        Line::from(Span::styled(
            confirm.message.clone(),
            Style::default().fg(Color::White),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                " y ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" yes    "),
            Span::styled(
                " n ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" no"),
        ]),
    ];
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(block),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_rect_halves_the_area() {
        let out = centered_rect(50, 50, Rect::new(0, 0, 100, 100));
        assert_eq!(out, Rect::new(25, 25, 50, 50));
    }

    #[test]
    fn centered_rect_respects_a_non_zero_origin() {
        let out = centered_rect(50, 50, Rect::new(10, 4, 80, 20));
        assert_eq!(out, Rect::new(30, 9, 40, 10));
    }

    #[test]
    fn centered_rect_is_full_size_at_100_percent() {
        let area = Rect::new(3, 7, 40, 12);
        assert_eq!(centered_rect(100, 100, area), area);
    }

    #[test]
    fn centered_rect_degrades_to_empty_on_a_tiny_area() {
        let out = centered_rect(50, 50, Rect::new(0, 0, 1, 1));
        assert_eq!(out.width, 0);
        assert_eq!(out.height, 0);
        // Still inside the parent.
        assert!(out.x <= 1 && out.y <= 1);
    }

    #[test]
    fn sized_rect_shrinks_rather_than_overflowing() {
        let out = sized_rect(64, 12, Rect::new(0, 0, 20, 6));
        assert_eq!(out, Rect::new(0, 0, 20, 6));
    }

    #[test]
    fn secret_fields_are_masked() {
        let line = field_line("password", "hunter2", false, true, 8);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("•••••••"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn selected_field_shows_a_caret() {
        let line = field_line("uri", "duckdb://", true, false, 8);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with("▸ "), "{text}");
        assert!(text.contains("duckdb://"), "{text}");
    }
}
