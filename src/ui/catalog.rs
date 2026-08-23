//! Catalog pane: the flattened lazy-loaded tree, one `CatalogRow` per line.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, CatalogRow};
use crate::types::{LoadState, NodeKind, Pane};
use crate::ui::{ACCENT, CHROME, pane_block, spinner_frame, truncate_line};

/// How much of a load error we inline before cutting it off.
const ERROR_SNIPPET: usize = 48;

/// The disclosure marker in front of a row: `▼` open, `▶` closed, blank for
/// leaves (columns).
fn marker(row: &CatalogRow) -> &'static str {
    if matches!(row.kind, NodeKind::Column { .. }) {
        "  "
    } else if row.expanded {
        "▼ "
    } else {
        "▶ "
    }
}

/// Render one tree row. Pure, so it is easy to test.
pub fn row_line(row: &CatalogRow, spinner: &str) -> Line<'static> {
    let errored = matches!(row.load_state, LoadState::Error(_));
    let base = if errored {
        Style::default().fg(Color::Red)
    } else {
        Style::default()
    };

    let mut spans: Vec<Span<'static>> = vec![
        Span::raw(" ".repeat(row.depth * 2)),
        Span::styled(marker(row), Style::default().fg(CHROME)),
        Span::styled(format!("{} ", row.kind.glyph()), base.fg(CHROME)),
    ];

    match &row.kind {
        NodeKind::Column {
            data_type,
            nullable,
        } => {
            spans.push(Span::styled(row.name.clone(), base));
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                data_type.clone(),
                Style::default().fg(CHROME).add_modifier(Modifier::DIM),
            ));
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                if *nullable { "[NULL]" } else { "[NOT NULL]" },
                Style::default()
                    .fg(if *nullable { Color::Yellow } else { CHROME })
                    .add_modifier(Modifier::DIM),
            ));
        }
        _ => spans.push(Span::styled(
            row.name.clone(),
            if errored {
                base
            } else {
                base.add_modifier(Modifier::BOLD)
            },
        )),
    }

    match &row.load_state {
        LoadState::Loading => spans.push(Span::styled(
            format!(" {spinner}"),
            Style::default().fg(ACCENT),
        )),
        LoadState::Error(msg) => {
            let snippet: String = msg
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(ERROR_SNIPPET)
                .collect();
            spans.push(Span::styled(
                format!("  ! {snippet}"),
                Style::default().fg(Color::Red),
            ));
        }
        LoadState::NotLoaded | LoadState::Loaded => {}
    }

    Line::from(spans)
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let focused = app.focus == Pane::Catalog;
    let rows = app.catalog_rows();

    // Keep the selection inside the tree; the tree shrinks when nodes collapse.
    if !rows.is_empty() && app.catalog_selected >= rows.len() {
        app.catalog_selected = rows.len() - 1;
    }

    let block = pane_block("CATALOG", focused);
    let inner = block.inner(area);

    if rows.is_empty() {
        let hint = Paragraph::new(vec![
            Line::from(Span::styled(
                "no connections",
                Style::default().fg(CHROME).add_modifier(Modifier::DIM),
            )),
            Line::from(Span::styled(
                "`:` then `connect`",
                Style::default().fg(CHROME).add_modifier(Modifier::DIM),
            )),
        ])
        .wrap(Wrap { trim: true })
        .block(block);
        frame.render_widget(hint, area);
        return;
    }

    let width = inner.width as usize;
    let spinner = spinner_frame(app.spinner);
    let items: Vec<ListItem<'static>> = rows
        .iter()
        .map(|row| ListItem::new(truncate_line(row_line(row, spinner), width)))
        .collect();

    let highlight = if focused {
        Style::default()
            .bg(ACCENT)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::REVERSED)
    };

    let list = List::new(items)
        .block(block)
        .highlight_style(highlight)
        .highlight_symbol("");

    // A fresh state each frame: ratatui scrolls the offset so the selected row
    // is visible, which is exactly the bookkeeping we want.
    let mut state =
        ListState::default().with_selected(Some(app.catalog_selected.min(rows.len() - 1)));
    frame.render_stateful_widget(list, area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CatalogNode;
    use crate::ui::test_support;

    fn row(
        kind: NodeKind,
        name: &str,
        depth: usize,
        load_state: LoadState,
        expanded: bool,
    ) -> CatalogRow {
        CatalogRow {
            profile_id: "p".into(),
            path: vec![0],
            depth,
            name: name.into(),
            kind,
            load_state,
            expanded,
            qualified: name.into(),
        }
    }

    fn text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn expanded_and_collapsed_markers() {
        let open = row(NodeKind::Schema, "public", 1, LoadState::Loaded, true);
        assert!(text(&row_line(&open, "⠋")).contains("▼"));
        let shut = row(NodeKind::Schema, "public", 1, LoadState::Loaded, false);
        assert!(text(&row_line(&shut, "⠋")).contains("▶"));
    }

    #[test]
    fn columns_render_type_and_nullability_and_no_marker() {
        let col = row(
            NodeKind::Column {
                data_type: "int4".into(),
                nullable: false,
            },
            "id",
            3,
            LoadState::Loaded,
            false,
        );
        let t = text(&row_line(&col, "⠋"));
        assert!(t.contains("id"), "{t}");
        assert!(t.contains("int4"), "{t}");
        assert!(t.contains("[NOT NULL]"), "{t}");
        assert!(!t.contains('▶'), "{t}");
        // 3 levels of indent = 6 leading spaces.
        assert!(t.starts_with("        "), "{t:?}");
    }

    #[test]
    fn loading_shows_the_spinner_and_errors_show_the_message() {
        let loading = row(NodeKind::Catalog, "main", 0, LoadState::Loading, true);
        assert!(text(&row_line(&loading, "⠹")).contains('⠹'));

        let errored = row(
            NodeKind::Catalog,
            "main",
            0,
            LoadState::Error("permission denied".into()),
            false,
        );
        let line = row_line(&errored, "⠋");
        assert!(text(&line).contains("permission denied"));
        assert!(line.spans.iter().any(|s| s.style.fg == Some(Color::Red)));
    }

    #[test]
    fn long_error_is_truncated() {
        let msg = "x".repeat(500);
        let errored = row(NodeKind::Table, "t", 2, LoadState::Error(msg), false);
        let t = text(&row_line(&errored, "⠋"));
        assert!(t.len() < 200, "{}", t.len());
    }

    #[test]
    fn renders_a_populated_tree_and_clamps_the_selection() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (mut app, _rx) = test_support::app();
        app.profiles = vec![crate::types::Profile {
            id: "p".into(),
            name: "p".into(),
            driver: "duckdb".into(),
            uri: ":memory:".into(),
            username: None,
            secret_ref: None,
            options: Default::default(),
            color: None,
        }];
        let mut root = CatalogNode::new(NodeKind::Catalog, "p");
        let mut cat = CatalogNode::new(NodeKind::Catalog, "memory");
        cat.expanded = true;
        cat.load_state = LoadState::Loaded;
        let mut schema = CatalogNode::new(NodeKind::Schema, "public");
        schema.expanded = true;
        schema
            .children
            .push(CatalogNode::new(NodeKind::Table, "orders"));
        cat.children.push(schema);
        root.children.push(cat);
        app.catalogs.insert("p".into(), root);
        app.catalog_selected = 99;

        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|frame| draw(frame, &mut app, frame.area()))
            .unwrap();
        assert_eq!(app.catalog_selected, 2);

        let buf = terminal.backend().buffer().clone();
        let dump: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(dump.contains("memory"), "{dump}");
        assert!(dump.contains("orders"), "{dump}");
    }
}
