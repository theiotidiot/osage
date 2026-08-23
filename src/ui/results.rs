//! Results pane: the Arrow-backed grid, or the driver's error message.
//!
//! Only the visible page of rows is ever formatted — result sets are
//! materialized in full but can be very large.

use arrow::util::display::{ArrayFormatter, FormatOptions};
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, Wrap};

use crate::app::App;
use crate::types::{Pane, QueryResult};
use crate::ui::{ACCENT, CHROME, pane_block};

/// Widest a single column is allowed to get (spec F6: auto-sized but capped).
pub const MAX_COL_WIDTH: usize = 40;
/// Narrowest a column may be, so headers stay clickable-looking.
pub const MIN_COL_WIDTH: usize = 3;
/// Glyph used for SQL NULL.
pub const NULL_GLYPH: &str = "∅";

/// Clamp a measured content width into the pane's budget.
pub fn clamp_width(content: usize) -> u16 {
    content.clamp(MIN_COL_WIDTH, MAX_COL_WIDTH) as u16
}

/// Map an absolute row index onto `(batch index, offset within batch)`.
///
/// `lens` is the per-batch row count. Returns `None` past the end.
pub fn locate_row(lens: &[usize], absolute: usize) -> Option<(usize, usize)> {
    let mut remaining = absolute;
    for (index, len) in lens.iter().enumerate() {
        if remaining < *len {
            return Some((index, remaining));
        }
        remaining -= *len;
    }
    None
}

/// Which columns fit, starting at `col_offset`.
///
/// Returns the column indices plus the width chosen for each.
fn column_window(
    result: &QueryResult,
    page: &[Vec<String>],
    col_offset: usize,
    budget: usize,
) -> Vec<(usize, u16)> {
    let fields = result.schema.fields();
    let mut out = Vec::new();
    let mut used = 0usize;
    for column in col_offset..fields.len() {
        let field = &fields[column];
        let header =
            field.name().chars().count() + field.data_type().to_string().chars().count() + 1;
        let widest = page
            .iter()
            .filter_map(|row| row.get(column - col_offset))
            .map(|cell| cell.chars().count())
            .max()
            .unwrap_or(0)
            .max(header);
        let width = clamp_width(widest);
        // +1 for the inter-column gap.
        if !out.is_empty() && used + width as usize + 1 > budget {
            break;
        }
        used += width as usize + 1;
        out.push((column, width));
    }
    out
}

/// Format the page of rows `[start, start + count)` for `columns`.
fn format_page(
    result: &QueryResult,
    start: usize,
    count: usize,
    columns: &[usize],
) -> Vec<Vec<(String, bool)>> {
    let options = FormatOptions::new().with_null(NULL_GLYPH);
    let lens: Vec<usize> = result.batches.iter().map(|b| b.num_rows()).collect();

    let mut out = Vec::with_capacity(count);
    let mut current: Option<usize> = None;
    let mut formatters: Vec<Option<ArrayFormatter>> = Vec::new();

    for absolute in start..start.saturating_add(count) {
        let Some((batch_index, offset)) = locate_row(&lens, absolute) else {
            break;
        };
        if current != Some(batch_index) {
            let batch = &result.batches[batch_index];
            formatters = columns
                .iter()
                .map(|c| {
                    batch
                        .columns()
                        .get(*c)
                        .and_then(|array| ArrayFormatter::try_new(array.as_ref(), &options).ok())
                })
                .collect();
            current = Some(batch_index);
        }
        let batch = &result.batches[batch_index];
        let row = columns
            .iter()
            .enumerate()
            .map(|(slot, c)| {
                let is_null = batch
                    .columns()
                    .get(*c)
                    .map(|a| a.is_null(offset))
                    .unwrap_or(true);
                let text = match formatters.get(slot).and_then(|f| f.as_ref()) {
                    Some(f) => f
                        .value(offset)
                        .try_to_string()
                        .unwrap_or_else(|_| "?".into()),
                    None => "?".to_string(),
                };
                (text, is_null)
            })
            .collect();
        out.push(row);
    }
    out
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let focused = app.focus == Pane::Results;

    // ---- error takes over the whole pane (spec F5) ----------------------
    if let Some(error) = app.tab().error.as_ref() {
        let block = pane_block("RESULTS · error", focused).border_style(
            Style::default().fg(Color::Red).add_modifier(if focused {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        );
        let body = Paragraph::new(vec![Line::from(Span::styled(
            error.message.clone(),
            Style::default().fg(Color::Red),
        ))])
        .wrap(Wrap { trim: true })
        .block(block);
        frame.render_widget(body, area);
        return;
    }

    // ---- empty state ----------------------------------------------------
    if app.tab().result.is_none() {
        let block = pane_block("RESULTS", focused);
        let hint = Paragraph::new(Line::from(Span::styled(
            "no results — Ctrl-Enter (or F5) to run",
            Style::default().fg(CHROME).add_modifier(Modifier::DIM),
        )))
        .wrap(Wrap { trim: true })
        .block(block);
        frame.render_widget(hint, area);
        return;
    }

    // ---- clamp scroll bookkeeping before borrowing immutably ------------
    let total_rows = app.tab().total_rows();
    let total_cols = app
        .tab()
        .result
        .as_ref()
        .map(|r| r.schema.fields().len())
        .unwrap_or(0);
    {
        let tab = app.tab_mut();
        if tab.row_offset >= total_rows {
            tab.row_offset = total_rows.saturating_sub(1);
        }
        if tab.col_offset >= total_cols {
            tab.col_offset = total_cols.saturating_sub(1);
        }
    }

    let tab = app.tab();
    let result = tab.result.as_ref().expect("checked above");
    let row_offset = tab.row_offset;
    let col_offset = tab.col_offset;

    // Reserve: 2 border rows + 1 header row.
    let visible_rows = (area.height as usize).saturating_sub(3);
    let budget = (area.width as usize).saturating_sub(2);

    // Format once with every remaining column so widths can be measured, then
    // decide the window. Cheap: at most `visible_rows` rows.
    // No more columns can fit than `budget / (MIN_COL_WIDTH + 1)`, so never
    // format past that — wide schemas would otherwise cost a lot per frame.
    let max_measured = budget / (MIN_COL_WIDTH + 1) + 1;
    let all_columns: Vec<usize> = (col_offset..total_cols).take(max_measured).collect();
    let measured = format_page(result, row_offset, visible_rows, &all_columns);
    let measured_text: Vec<Vec<String>> = measured
        .iter()
        .map(|row| row.iter().map(|(t, _)| t.clone()).collect())
        .collect();
    let window = column_window(result, &measured_text, col_offset, budget);

    let fields = result.schema.fields();
    let header = Row::new(
        window
            .iter()
            .map(|(c, _)| {
                let field = &fields[*c];
                Cell::from(Line::from(vec![
                    Span::styled(
                        field.name().clone(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" {}", field.data_type()),
                        Style::default().fg(CHROME).add_modifier(Modifier::DIM),
                    ),
                ]))
            })
            .collect::<Vec<_>>(),
    )
    .style(Style::default())
    .height(1);

    let rows: Vec<Row<'static>> = measured
        .iter()
        .map(|row| {
            Row::new(
                window
                    .iter()
                    .map(|(c, _)| {
                        let slot = c - col_offset;
                        match row.get(slot) {
                            Some((text, true)) => Cell::from(Span::styled(
                                text.clone(),
                                Style::default().fg(CHROME).add_modifier(Modifier::DIM),
                            )),
                            Some((text, false)) => Cell::from(text.clone()),
                            None => Cell::from(""),
                        }
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect();

    let shown = measured.len();
    let first = if shown == 0 { 0 } else { row_offset + 1 };
    let last = row_offset + shown;
    let col_last = window.last().map(|(c, _)| c + 1).unwrap_or(col_offset);
    let title = format!(
        "RESULTS · rows {first}–{last} of {total_rows} · cols {}–{col_last} of {total_cols} · {}ms",
        (col_offset + 1).min(total_cols.max(1)),
        result.elapsed.as_millis()
    );

    let widths: Vec<Constraint> = window.iter().map(|(_, w)| Constraint::Length(*w)).collect();

    let block = pane_block(title, focused);
    if widths.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no columns",
                Style::default().fg(CHROME),
            )))
            .block(block),
            area,
        );
        return;
    }

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .row_highlight_style(Style::default().bg(ACCENT).fg(Color::Black))
        .block(block);
    frame.render_widget(table, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use arrow::array::{Int32Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::ui::test_support;

    fn sample(rows: usize) -> QueryResult {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let ids: Vec<i32> = (0..rows as i32).collect();
        let names: Vec<Option<String>> = (0..rows)
            .map(|i| {
                if i % 3 == 0 {
                    None
                } else {
                    Some(format!("row-{i}"))
                }
            })
            .collect();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from(ids)),
                Arc::new(StringArray::from(names)),
            ],
        )
        .unwrap();
        QueryResult {
            schema,
            batches: vec![batch],
            elapsed: Duration::from_millis(38),
            row_count: rows,
        }
    }

    #[test]
    fn clamp_width_caps_and_floors() {
        assert_eq!(clamp_width(0), MIN_COL_WIDTH as u16);
        assert_eq!(clamp_width(1), MIN_COL_WIDTH as u16);
        assert_eq!(clamp_width(12), 12);
        assert_eq!(clamp_width(MAX_COL_WIDTH), MAX_COL_WIDTH as u16);
        assert_eq!(clamp_width(10_000), MAX_COL_WIDTH as u16);
    }

    #[test]
    fn locate_row_maps_across_batches() {
        let lens = [3usize, 0, 2, 4];
        assert_eq!(locate_row(&lens, 0), Some((0, 0)));
        assert_eq!(locate_row(&lens, 2), Some((0, 2)));
        // The empty batch is skipped entirely.
        assert_eq!(locate_row(&lens, 3), Some((2, 0)));
        assert_eq!(locate_row(&lens, 4), Some((2, 1)));
        assert_eq!(locate_row(&lens, 5), Some((3, 0)));
        assert_eq!(locate_row(&lens, 8), Some((3, 3)));
        assert_eq!(locate_row(&lens, 9), None);
        assert_eq!(locate_row(&[], 0), None);
    }

    #[test]
    fn only_the_visible_page_is_formatted() {
        let result = sample(10_000);
        let page = format_page(&result, 9_995, 20, &[0, 1]);
        assert_eq!(page.len(), 5);
        assert_eq!(page[0][0].0, "9995");
    }

    #[test]
    fn nulls_are_flagged() {
        let result = sample(4);
        let page = format_page(&result, 0, 4, &[0, 1]);
        assert!(page[0][1].1, "row 0 name should be null");
        assert!(!page[1][1].1);
        assert_eq!(page[0][1].0, NULL_GLYPH);
    }

    #[test]
    fn column_window_respects_the_budget() {
        let result = sample(3);
        let page: Vec<Vec<String>> = vec![vec!["1".into(), "row-1".into()]];
        let wide = column_window(&result, &page, 0, 200);
        assert_eq!(wide.len(), 2);
        let narrow = column_window(&result, &page, 0, 4);
        assert_eq!(narrow.len(), 1, "at least one column always renders");
    }

    fn dump(app: &mut crate::app::App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_header_names_from_the_arrow_schema() {
        let (mut app, _rx) = test_support::app();
        app.tab_mut().result = Some(sample(50));
        let text = dump(&mut app, 100, 30);
        assert!(text.contains("id"), "{text}");
        assert!(text.contains("name"), "{text}");
        assert!(text.contains("row-1"), "{text}");
        assert!(text.contains("rows 1–"), "{text}");
    }

    #[test]
    fn renders_the_error_instead_of_a_grid() {
        let (mut app, _rx) = test_support::app();
        app.tab_mut().result = Some(sample(5));
        app.tab_mut().error = Some(crate::types::QueryError::new("relation does not exist"));
        let text = dump(&mut app, 90, 24);
        assert!(text.contains("relation does not exist"), "{text}");
        assert!(!text.contains("row-1"), "{text}");
    }

    #[test]
    fn empty_state_names_the_run_binding() {
        let (mut app, _rx) = test_support::app();
        let text = dump(&mut app, 90, 24);
        assert!(text.contains("Ctrl-Enter"), "{text}");
    }

    #[test]
    fn scrolled_offsets_are_clamped_not_panicked() {
        let (mut app, _rx) = test_support::app();
        app.tab_mut().result = Some(sample(5));
        app.tab_mut().row_offset = 900;
        app.tab_mut().col_offset = 900;
        let _ = dump(&mut app, 60, 20);
        assert_eq!(app.tab().row_offset, 4);
        assert_eq!(app.tab().col_offset, 1);
    }

    #[test]
    fn grid_survives_a_tiny_pane() {
        let (mut app, _rx) = test_support::app();
        app.tab_mut().result = Some(sample(5));
        for (w, h) in [(10u16, 4u16), (4, 4), (1, 1), (20, 5)] {
            let _ = dump(&mut app, w, h);
        }
    }
}
