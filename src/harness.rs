//! Milestone M1's exit criterion, as a test: the whole loop — connect, browse
//! the catalog, type, run, render — driven through the real `App` and a real
//! ADBC connection, with a `TestBackend` standing in for the terminal.

#![cfg(test)]

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::app::App;
use crate::db::{ConnectionManager, DbEvent};
use crate::types::{ConnectionState, LoadState, NodeKind, Pane, Profile};

const FIXTURE: &str = "/tmp/osage-test.duckdb";
const DEADLINE: Duration = Duration::from_secs(20);

fn fixture_available() -> bool {
    let Some(home) = std::env::var_os("HOME") else {
        return false;
    };
    std::path::Path::new(&home)
        .join("Library/Application Support/ADBC/Drivers/duckdb.toml")
        .exists()
        && std::path::Path::new(FIXTURE).exists()
}

fn app_with_fixture() -> (App, Receiver<DbEvent>) {
    let profile = Profile {
        id: "duck".into(),
        name: "Local DuckDB".into(),
        driver: "duckdb".into(),
        uri: FIXTURE.into(),
        username: None,
        secret_ref: None,
        options: HashMap::new(),
        color: Some(ratatui::style::Color::Yellow),
    };
    let (manager, events) = ConnectionManager::new();
    (App::new(vec![profile], manager), events)
}

/// Pump database events into the app until `done` is satisfied.
fn pump(app: &mut App, events: &Receiver<DbEvent>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while !done(app) {
        assert!(start.elapsed() < DEADLINE, "timed out waiting for {what}");
        match events.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => app.handle_db_event(ev),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(e) => panic!("event channel closed while waiting for {what}: {e}"),
        }
    }
}

fn type_text(app: &mut App, text: &str) {
    app.focus = Pane::Editor;
    for ch in text.chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
}

fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| crate::ui::draw(frame, app))
        .expect("draw");
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>()
        .chunks(width as usize)
        .map(|row| row.concat())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn full_loop_connect_browse_run_render() {
    if !fixture_available() {
        eprintln!("skipping: duckdb driver or fixture missing");
        return;
    }
    let (mut app, events) = app_with_fixture();

    // 1. Connect. The catalog request is issued automatically on success.
    app.connect("duck");
    pump(&mut app, &events, "connection", |app| {
        app.state_of("duck").is_connected()
    });
    pump(&mut app, &events, "root catalogs", |app| {
        !app.catalog_rows().is_empty()
    });

    // 2. Expanding a catalog lazily loads exactly one level below it.
    let rows = app.catalog_rows();
    let fixture_catalog = rows
        .iter()
        .position(|r| r.name.contains("osage-test"))
        .unwrap_or_else(|| panic!("fixture catalog missing from {:?}", names(&rows)));
    app.focus = Pane::Catalog;
    app.catalog_selected = fixture_catalog;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    pump(&mut app, &events, "schemas", |app| {
        app.catalog_rows()
            .iter()
            .any(|r| r.kind == NodeKind::Schema)
    });

    let rows = app.catalog_rows();
    let main_schema = rows
        .iter()
        .position(|r| r.kind == NodeKind::Schema && r.name == "main")
        .unwrap_or_else(|| panic!("`main` schema missing from {:?}", names(&rows)));
    app.catalog_selected = main_schema;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    pump(&mut app, &events, "tables", |app| {
        app.catalog_rows()
            .iter()
            .any(|r| r.name == "customers" && r.kind == NodeKind::Table)
    });

    // 3. Ctrl-i inserts the fully-qualified name at the editor cursor.
    let rows = app.catalog_rows();
    app.catalog_selected = rows.iter().position(|r| r.name == "customers").unwrap();
    let qualified = rows[app.catalog_selected].qualified.clone();
    assert!(
        qualified.ends_with("main.customers"),
        "qualified name was {qualified}"
    );
    type_text(&mut app, "SELECT id, name FROM ");
    app.focus = Pane::Editor;
    app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::CONTROL));
    assert!(
        app.tab().text().contains("main.customers"),
        "buffer was {:?}",
        app.tab().text()
    );

    // 4. Highlighting keeps up with the buffer, one entry per line.
    assert_eq!(app.tab().highlights.len(), app.tab().textarea.lines().len());
    assert!(
        app.tab().highlights.iter().any(|line| line
            .iter()
            .any(|(_, class)| *class == crate::types::HighlightClass::Keyword)),
        "no keyword was highlighted"
    );

    // 5. Run it.
    type_text(&mut app, " ORDER BY id");
    app.run_current_statement();
    assert!(app.tab().running, "the tab should show as running");
    pump(&mut app, &events, "query result", |app| !app.tab().running);

    let result = app.tab().result.as_ref().unwrap_or_else(|| {
        panic!(
            "no result set; buffer={:?} error={:?}",
            app.tab().text(),
            app.tab().error
        )
    });
    assert_eq!(result.row_count, 3);
    assert_eq!(result.schema.fields().len(), 2);
    assert!(app.tab().error.is_none());
    assert!(app.status.contains("3 rows"), "status was {:?}", app.status);

    // 6. Render the finished state and check the panes actually show it.
    let screen = render(&mut app, 120, 32);
    assert!(screen.contains("customers"), "catalog pane:\n{screen}");
    assert!(screen.contains("SELECT"), "editor pane:\n{screen}");
    assert!(screen.contains("name"), "results header:\n{screen}");
    assert!(screen.contains("ada"), "results body:\n{screen}");
    assert!(screen.contains("3 rows"), "status bar:\n{screen}");

    // 7. And it survives a cramped terminal.
    let _ = render(&mut app, 20, 8);
}

#[test]
fn a_failing_statement_shows_in_the_results_pane() {
    if !fixture_available() {
        return;
    }
    let (mut app, events) = app_with_fixture();
    app.connect("duck");
    pump(&mut app, &events, "connection", |app| {
        app.state_of("duck").is_connected()
    });

    type_text(&mut app, "SELECT * FROM nope");
    app.run_current_statement();
    pump(&mut app, &events, "query error", |app| !app.tab().running);

    let error = app.tab().error.as_ref().expect("an error");
    assert!(
        error.message.to_lowercase().contains("nope"),
        "error was {:?}",
        error.message
    );
    assert!(
        app.tab().result.is_none(),
        "a failed query cleared no result"
    );

    app.focus = Pane::Results;
    let screen = render(&mut app, 120, 32);
    assert!(screen.to_lowercase().contains("nope"), "screen:\n{screen}");
}

/// The catalog pane must be able to connect when it is empty — that is exactly
/// the state its own empty-state hint sends you to.
#[test]
fn c_connects_from_an_empty_catalog_pane() {
    if !fixture_available() {
        return;
    }
    let (mut app, events) = app_with_fixture();
    assert!(app.catalog_rows().is_empty());
    app.focus = Pane::Catalog;
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
    pump(&mut app, &events, "connection", |app| {
        app.state_of("duck").is_connected()
    });
    pump(&mut app, &events, "root catalogs", |app| {
        !app.catalog_rows().is_empty()
    });

    // And `c` again on a populated tree disconnects.
    app.catalog_selected = 0;
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
    assert_eq!(app.state_of("duck"), ConnectionState::Disconnected);
}

#[test]
fn disconnecting_drops_the_catalog_and_keeps_the_ui_alive() {
    if !fixture_available() {
        return;
    }
    let (mut app, events) = app_with_fixture();
    app.connect("duck");
    pump(&mut app, &events, "root catalogs", |app| {
        !app.catalog_rows().is_empty()
    });

    app.disconnect("duck");
    assert_eq!(app.state_of("duck"), ConnectionState::Disconnected);
    assert!(app.catalog_rows().is_empty());
    let screen = render(&mut app, 100, 24);
    assert!(screen.contains("Local DuckDB") || screen.contains("duck"));
}

#[test]
fn refresh_reloads_a_node_in_place() {
    if !fixture_available() {
        return;
    }
    let (mut app, events) = app_with_fixture();
    app.connect("duck");
    pump(&mut app, &events, "root catalogs", |app| {
        !app.catalog_rows().is_empty()
    });

    let rows = app.catalog_rows();
    let index = rows
        .iter()
        .position(|r| r.name.contains("osage-test"))
        .expect("fixture catalog");
    app.focus = Pane::Catalog;
    app.catalog_selected = index;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    pump(&mut app, &events, "schemas", |app| {
        app.catalog_rows()
            .iter()
            .any(|r| r.kind == NodeKind::Schema)
    });
    let before = app.catalog_rows().len();

    app.catalog_selected = index;
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
    pump(&mut app, &events, "refreshed schemas", |app| {
        app.catalog_rows()
            .get(index)
            .map(|r| r.load_state == LoadState::Loaded)
            .unwrap_or(false)
            && app.catalog_rows().len() == before
    });
}

fn names(rows: &[crate::app::CatalogRow]) -> Vec<String> {
    rows.iter().map(|r| r.name.clone()).collect()
}

/// The probe reports three DuckDB catalogs (`system`, `temp`, and the attached
/// database); the tree must show all of them, not just one.
#[test]
fn every_catalog_the_driver_reports_is_listed() {
    if !fixture_available() {
        return;
    }
    let (mut app, events) = app_with_fixture();
    app.connect("duck");
    pump(&mut app, &events, "root catalogs", |app| {
        !app.catalog_rows().is_empty()
    });
    // Give any straggling batches a moment to arrive.
    std::thread::sleep(Duration::from_millis(300));
    while let Ok(ev) = events.try_recv() {
        app.handle_db_event(ev);
    }

    let rows = app.catalog_rows();
    let listed = names(&rows);
    assert!(
        listed.iter().any(|n| n.contains("osage-test")),
        "listed {listed:?}"
    );
    assert!(listed.iter().any(|n| n == "system"), "listed {listed:?}");
    assert!(listed.iter().any(|n| n == "temp"), "listed {listed:?}");

    let screen = render(&mut app, 100, 24);
    for name in &listed {
        assert!(screen.contains(name.as_str()), "{name} missing:\n{screen}");
    }
}
