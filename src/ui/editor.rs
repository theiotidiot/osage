//! Editor pane: tab bar, syntax-highlighted buffer, and the completion popup.
//!
//! The buffer is rendered by hand rather than through `TextArea::widget()` so
//! that tree-sitter highlight spans, the error span from the driver and the
//! cursor cell can all be composed into the same `Line`.

use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, List, ListItem, ListState, Paragraph};

use crate::app::App;
use crate::types::{HighlightClass, Pane};
use crate::ui::{ACCENT, CHROME, pane_block, spinner_frame, truncate_line};

/// Most rows the completion popup ever shows at once.
pub const COMPLETION_ROWS: usize = 10;

/// Colour for each highlight class.
pub fn class_style(class: HighlightClass) -> Style {
    match class {
        HighlightClass::Keyword => Style::default()
            .fg(Color::LightBlue)
            .add_modifier(Modifier::BOLD),
        HighlightClass::String => Style::default().fg(Color::Green),
        HighlightClass::Number => Style::default().fg(Color::Magenta),
        HighlightClass::Comment => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
        HighlightClass::Function => Style::default().fg(Color::Yellow),
        HighlightClass::Operator => Style::default().fg(Color::Cyan),
        HighlightClass::Identifier => Style::default(),
    }
}

/// Scroll offset that keeps `cursor` inside a `viewport`-sized window while
/// preferring to show the start of the buffer. Pure and deterministic, so no
/// scroll state has to be stored.
pub fn scroll_offset(cursor: usize, viewport: usize) -> usize {
    if viewport == 0 || cursor < viewport {
        0
    } else {
        cursor + 1 - viewport
    }
}

/// Byte offset at which each line starts, assuming `\n` separators.
pub fn line_starts(lines: &[String]) -> Vec<usize> {
    let mut out = Vec::with_capacity(lines.len());
    let mut acc = 0usize;
    for line in lines {
        out.push(acc);
        acc += line.len() + 1;
    }
    out
}

/// Intersect a buffer-global byte span with one line, returning line-local byte
/// bounds. Always clamped — a lagging reparse must never panic the renderer.
pub fn local_error_span(
    span: (usize, usize),
    line_start: usize,
    line_len: usize,
) -> Option<(usize, usize)> {
    let (lo, hi) = (span.0.min(span.1), span.0.max(span.1));
    let line_end = line_start + line_len;
    if hi <= line_start || lo >= line_end {
        return None;
    }
    let s = lo.saturating_sub(line_start).min(line_len);
    let e = hi.saturating_sub(line_start).min(line_len);
    if s >= e { None } else { Some((s, e)) }
}

/// Compose one buffer line into styled spans.
///
/// `highlights` carry byte offsets relative to the line start; anything out of
/// bounds is clamped. `cursor` is a *character* index, matching what
/// `TextArea::cursor()` reports.
pub fn styled_line(
    text: &str,
    highlights: &[(Range<usize>, HighlightClass)],
    error: Option<(usize, usize)>,
    cursor: Option<usize>,
    col_off: usize,
    width: usize,
) -> Line<'static> {
    if width == 0 {
        return Line::from(Vec::<Span<'static>>::new());
    }

    // One cell per character; tabs are flattened so the buffer stays aligned.
    let chars: Vec<(usize, char)> = text
        .char_indices()
        .map(|(b, c)| (b, if c == '\t' { ' ' } else { c }))
        .collect();
    let mut styles = vec![Style::default(); chars.len()];

    let len = text.len();
    for (range, class) in highlights {
        let s = range.start.min(len);
        let e = range.end.min(len);
        if s >= e {
            continue;
        }
        let style = class_style(*class);
        for (i, (byte, _)) in chars.iter().enumerate() {
            if *byte >= s && *byte < e {
                styles[i] = style;
            }
        }
    }

    if let Some((s, e)) = error {
        let (s, e) = (s.min(len), e.min(len));
        for (i, (byte, _)) in chars.iter().enumerate() {
            if *byte >= s && *byte < e {
                styles[i] = styles[i]
                    .fg(Color::Red)
                    .add_modifier(Modifier::UNDERLINED | Modifier::BOLD);
            }
        }
    }

    // Cells actually visible after horizontal scroll, plus a virtual trailing
    // cell so the cursor can sit at end-of-line.
    let mut cells: Vec<(char, Style)> = chars
        .iter()
        .zip(styles.iter())
        .map(|((_, c), s)| (*c, *s))
        .collect();
    if let Some(col) = cursor {
        while cells.len() <= col {
            cells.push((' ', Style::default()));
        }
        cells[col].1 = cells[col].1.add_modifier(Modifier::REVERSED);
    }

    let visible: Vec<(char, Style)> = cells.into_iter().skip(col_off).take(width).collect();

    // Coalesce runs of equal style into spans.
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut current: Option<Style> = None;
    for (ch, style) in visible {
        if current != Some(style) {
            if let Some(prev) = current {
                spans.push(Span::styled(std::mem::take(&mut buf), prev));
            }
            current = Some(style);
        }
        buf.push(ch);
    }
    if let Some(style) = current {
        spans.push(Span::styled(buf, style));
    }
    Line::from(spans)
}

/// The tab bar: `title [profile]` per open tab.
pub fn tab_bar(app: &App) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (index, tab) in app.tabs.iter().enumerate() {
        let active = index == app.active_tab.min(app.tabs.len().saturating_sub(1));
        let style = if active {
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(CHROME)
        };
        let profile = tab.profile_id.clone().unwrap_or_else(|| "—".into());
        spans.push(Span::styled(
            format!(" {} · {} ", tab.title(), profile),
            style,
        ));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

/// Draw the editor pane. Returns the on-screen cursor cell, if it is visible —
/// the completion popup anchors to it.
pub fn draw(frame: &mut Frame, app: &App, area: Rect) -> Option<(u16, u16)> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let focused = app.focus == Pane::Editor;
    let tab = app.tab();

    let title = if tab.running {
        format!("EDITOR {}", spinner_frame(app.spinner))
    } else {
        "EDITOR".to_string()
    };
    let block = pane_block(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
    let (bar, body) = (chunks[0], chunks[1]);

    if bar.height > 0 {
        frame.render_widget(
            Paragraph::new(truncate_line(tab_bar(app), bar.width as usize)),
            bar,
        );
    }
    if body.height == 0 || body.width == 0 {
        return None;
    }

    let lines = tab.textarea.lines();
    let (cur_row, cur_col) = tab.textarea.cursor();
    let viewport_h = body.height as usize;
    let viewport_w = body.width as usize;
    let row_off = scroll_offset(cur_row, viewport_h);
    let col_off = scroll_offset(cur_col, viewport_w);

    let starts = line_starts(lines);
    let error_span = tab.error.as_ref().and_then(|e| e.span);

    let mut rendered: Vec<Line<'static>> = Vec::with_capacity(viewport_h);
    for index in row_off..(row_off + viewport_h).min(lines.len()) {
        let text = &lines[index];
        let highlights: &[(Range<usize>, HighlightClass)] = tab
            .highlights
            .get(index)
            .map(|h| h.as_slice())
            .unwrap_or(&[]);
        let error = error_span.and_then(|span| local_error_span(span, starts[index], text.len()));
        let cursor = (focused && index == cur_row).then_some(cur_col);
        rendered.push(styled_line(
            text, highlights, error, cursor, col_off, viewport_w,
        ));
    }
    frame.render_widget(Paragraph::new(rendered), body);

    let screen_row = cur_row.checked_sub(row_off)?;
    let screen_col = cur_col.checked_sub(col_off)?;
    if screen_row >= viewport_h || screen_col >= viewport_w {
        return None;
    }
    Some((body.x + screen_col as u16, body.y + screen_row as u16))
}

/// Geometry for the completion popup: below the cursor, flipped above when
/// there is no room. Split out for testing.
pub fn popup_rect(anchor: (u16, u16), items: usize, widest: usize, full: Rect) -> Rect {
    let width = (widest as u16 + 4).clamp(16, 48).min(full.width.max(1));
    let height = (items.min(COMPLETION_ROWS) as u16 + 2).min(full.height.max(1));

    let max_x = full.x + full.width.saturating_sub(width);
    let x = anchor.0.min(max_x).max(full.x);

    let below = anchor.1.saturating_add(1);
    let y = if below.saturating_add(height) <= full.y + full.height {
        below
    } else if anchor.1 >= full.y + height {
        anchor.1 - height
    } else {
        // Neither fits cleanly; pin to whatever room the screen has.
        (full.y + full.height).saturating_sub(height).max(full.y)
    };
    Rect::new(x, y, width, height)
}

/// Render the autocomplete popup on top of everything else.
pub fn draw_completion(
    frame: &mut Frame,
    app: &App,
    cursor: Option<(u16, u16)>,
    editor_area: Rect,
    full: Rect,
) {
    let Some(popup) = app.completion.as_ref() else {
        return;
    };
    if popup.items.is_empty() || full.width < 4 || full.height < 3 {
        return;
    }
    let anchor = cursor.unwrap_or((editor_area.x, editor_area.y));
    let widest = popup
        .items
        .iter()
        .map(|c| c.text.chars().count() + c.detail.chars().count() + 2)
        .max()
        .unwrap_or(16);
    let rect = popup_rect(anchor, popup.items.len(), widest, full);
    if rect.width < 3 || rect.height < 3 {
        return;
    }

    let block = ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT));
    let inner_width = block.inner(rect).width as usize;

    let items: Vec<ListItem<'static>> = popup
        .items
        .iter()
        .map(|c| {
            let left = c.text.clone();
            let right = c.detail.clone();
            let used = left.chars().count() + right.chars().count();
            let pad = inner_width.saturating_sub(used).max(1);
            ListItem::new(truncate_line(
                Line::from(vec![
                    Span::raw(left),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(
                        right,
                        Style::default().fg(CHROME).add_modifier(Modifier::DIM),
                    ),
                ]),
                inner_width,
            ))
        })
        .collect();

    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(ACCENT)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
    );
    let mut state =
        ListState::default().with_selected(Some(popup.selected.min(popup.items.len() - 1)));

    frame.render_widget(Clear, rect);
    frame.render_stateful_widget(list, rect, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::CompletionPopup;
    use crate::types::{Completion, CompletionKind, QueryError};
    use crate::ui::test_support;

    fn text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn scroll_offset_keeps_cursor_visible() {
        assert_eq!(scroll_offset(0, 10), 0);
        assert_eq!(scroll_offset(9, 10), 0);
        assert_eq!(scroll_offset(10, 10), 1);
        assert_eq!(scroll_offset(100, 10), 91);
        assert_eq!(scroll_offset(5, 0), 0);
    }

    #[test]
    fn line_starts_accounts_for_newlines() {
        let lines = vec!["abc".to_string(), "de".to_string(), "".to_string()];
        assert_eq!(line_starts(&lines), vec![0, 4, 7]);
    }

    #[test]
    fn local_error_span_clamps_and_rejects_misses() {
        assert_eq!(local_error_span((2, 5), 0, 10), Some((2, 5)));
        assert_eq!(local_error_span((0, 100), 4, 3), Some((0, 3)));
        assert_eq!(local_error_span((0, 2), 4, 3), None);
        assert_eq!(local_error_span((9, 9), 0, 10), None);
        assert_eq!(local_error_span((5, 2), 0, 10), Some((2, 5)));
    }

    #[test]
    fn styled_line_clamps_out_of_bounds_highlights() {
        let hl = vec![
            (0..4, HighlightClass::Keyword),
            (900..1000, HighlightClass::String),
        ];
        let line = styled_line("SELECT 1", &hl, None, None, 0, 40);
        assert_eq!(text(&line), "SELECT 1");
    }

    #[test]
    fn styled_line_handles_multibyte_without_slicing_panics() {
        // A single-byte-indexed span landing mid-character must not panic.
        let hl = vec![(0..2, HighlightClass::String)];
        let line = styled_line("'é☃'", &hl, None, Some(1), 0, 40);
        assert_eq!(text(&line), "'é☃'");
    }

    #[test]
    fn styled_line_scrolls_horizontally_and_marks_the_cursor() {
        let line = styled_line("abcdefghij", &[], None, Some(7), 4, 4);
        assert_eq!(text(&line), "efgh");
        // Cursor at char 7 → visible column 3 of the window.
        let line = styled_line("abcdefghij", &[], None, Some(7), 4, 4);
        let joined: Vec<char> = text(&line).chars().collect();
        assert_eq!(joined[3], 'h');
        assert!(
            line.spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::REVERSED))
        );
    }

    #[test]
    fn cursor_past_end_of_line_gets_a_virtual_cell() {
        let line = styled_line("ab", &[], None, Some(2), 0, 10);
        assert_eq!(text(&line), "ab ");
    }

    #[test]
    fn error_span_is_underlined_red() {
        let line = styled_line("SELECT frm", &[], Some((7, 10)), None, 0, 40);
        let bad: Vec<_> = line
            .spans
            .iter()
            .filter(|s| s.style.fg == Some(Color::Red))
            .collect();
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].content.as_ref(), "frm");
        assert!(bad[0].style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn popup_flips_above_when_there_is_no_room_below() {
        let full = Rect::new(0, 0, 80, 24);
        let below = popup_rect((10, 2), 5, 20, full);
        assert_eq!(below.y, 3);
        let above = popup_rect((10, 22), 5, 20, full);
        assert!(above.y + above.height <= 24);
        assert!(above.y < 22);
    }

    #[test]
    fn popup_stays_inside_the_screen_horizontally() {
        let full = Rect::new(0, 0, 40, 24);
        let rect = popup_rect((38, 4), 3, 60, full);
        assert!(rect.x + rect.width <= 40, "{rect:?}");
    }

    #[test]
    fn draws_editor_with_highlights_error_and_popup() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (mut app, _rx) = test_support::app();
        app.focus = Pane::Editor;
        app.tab_mut()
            .textarea
            .insert_str("SELECT * FROM orders\nWHERE 1 = 1");
        app.tab_mut().reparse();
        app.tab_mut().error = Some(QueryError {
            message: "syntax error".into(),
            span: Some((7, 8)),
        });
        app.completion = Some(CompletionPopup {
            items: vec![Completion {
                text: "orders".into(),
                detail: "table".into(),
                kind: CompletionKind::Table,
            }],
            selected: 0,
        });

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(dump.contains("SELECT"), "{dump}");
        assert!(dump.contains("orders"), "{dump}");
    }
}
