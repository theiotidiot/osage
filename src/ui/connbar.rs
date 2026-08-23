//! Connection bar: one chip per profile, across the top of the screen.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::types::ConnectionState;
use crate::ui::{CHROME, truncate_line};

/// Build the chip line. Split out from `draw` so it can be unit-tested.
pub fn chips(app: &App) -> Line<'static> {
    if app.profiles.is_empty() {
        return Line::from(Span::styled(
            " no connections — `:` then `connect` ",
            Style::default().fg(CHROME).add_modifier(Modifier::DIM),
        ));
    }

    let active = app.active_profile_id();
    let mut spans: Vec<Span<'static>> = Vec::new();
    for profile in &app.profiles {
        let state = app.state_of(&profile.id);
        let is_active = active.as_deref() == Some(profile.id.as_str());

        let mut style = Style::default().fg(profile.tag_color());
        match state {
            ConnectionState::Disconnected => {
                style = style.fg(CHROME).add_modifier(Modifier::DIM);
            }
            ConnectionState::Connecting => {
                style = style.add_modifier(Modifier::ITALIC);
            }
            ConnectionState::Connected => {
                style = style.add_modifier(Modifier::BOLD);
            }
            ConnectionState::Errored(_) => {
                style = style.fg(Color::Red).add_modifier(Modifier::BOLD);
            }
        }
        if is_active {
            style = style.add_modifier(Modifier::UNDERLINED);
        }

        spans.push(Span::styled(
            if is_active { " ▸" } else { "  " },
            Style::default().fg(CHROME),
        ));
        spans.push(Span::styled(
            format!("[{} {}]", state.indicator(), profile.id),
            style,
        ));
    }
    Line::from(spans)
}

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let line = truncate_line(chips(app), area.width as usize);
    frame.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Profile;
    use crate::ui::test_support;
    use std::collections::HashMap;

    fn profile(id: &str) -> Profile {
        Profile {
            id: id.into(),
            name: format!("{id} name"),
            driver: "duckdb".into(),
            uri: ":memory:".into(),
            username: None,
            secret_ref: None,
            options: HashMap::new(),
            color: None,
        }
    }

    #[test]
    fn empty_state_hints_at_connect() {
        let (app, _rx) = test_support::app();
        let text: String = chips(&app)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("connect"), "{text}");
    }

    #[test]
    fn marks_the_active_profile_and_shows_state_glyphs() {
        let (mut app, _rx) = test_support::app();
        app.profiles = vec![profile("prod-pg"), profile("local-duckdb")];
        app.connections
            .insert("prod-pg".into(), ConnectionState::Connected);
        app.connections
            .insert("local-duckdb".into(), ConnectionState::Disconnected);
        app.tabs[0].profile_id = Some("prod-pg".into());

        let line = chips(&app);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("▸[● prod-pg]"), "{text}");
        assert!(text.contains("[○ local-duckdb]"), "{text}");
    }

    #[test]
    fn truncates_when_narrow() {
        let (mut app, _rx) = test_support::app();
        app.profiles = (0..20).map(|i| profile(&format!("profile-{i}"))).collect();
        let line = truncate_line(chips(&app), 30);
        assert!(line.width() <= 30, "{}", line.width());
    }
}
