//! Application state and the main loop.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tui_textarea::TextArea;

use crate::db::{ConnectionManager, DbEvent, DbRequest};
use crate::event::{self, AppEvent};
use crate::sql::highlight::Highlighter;
use crate::types::*;

const TICK: Duration = Duration::from_millis(120);
const COMPLETION_LIMIT: usize = 40;

/// One editor tab: a buffer, the connection it runs against, and its last
/// result.
pub struct EditorTab {
    pub id: usize,
    pub profile_id: Option<String>,
    pub textarea: TextArea<'static>,
    pub result: Option<QueryResult>,
    pub error: Option<QueryError>,
    /// A statement is in flight.
    pub running: bool,
    /// Per-line highlight spans, recomputed on every edit.
    pub highlights: Vec<HighlightedLine>,
    highlighter: Option<Highlighter>,
    /// Results grid scroll offsets.
    pub row_offset: usize,
    pub col_offset: usize,
}

impl EditorTab {
    pub fn new(id: usize, profile_id: Option<String>) -> Self {
        let mut textarea = TextArea::default();
        textarea.set_tab_length(2);
        let mut tab = Self {
            id,
            profile_id,
            textarea,
            result: None,
            error: None,
            running: false,
            highlights: Vec::new(),
            highlighter: Highlighter::new().ok(),
            row_offset: 0,
            col_offset: 0,
        };
        tab.reparse();
        tab
    }

    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Byte offset of the cursor within the buffer.
    pub fn cursor_offset(&self) -> usize {
        let (row, col) = self.textarea.cursor();
        crate::sql::statement::byte_offset(&self.text(), row, col)
    }

    /// Re-run syntax highlighting over the whole buffer.
    pub fn reparse(&mut self) {
        let text = self.text();
        self.highlights = match self.highlighter.as_mut() {
            Some(h) => h.highlight(&text),
            None => Vec::new(),
        };
    }

    /// Title shown in the tab bar.
    pub fn title(&self) -> String {
        let first = self
            .textarea
            .lines()
            .iter()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.trim())
            .unwrap_or("");
        if first.is_empty() {
            format!("tab {}", self.id)
        } else {
            let truncated: String = first.chars().take(18).collect();
            truncated
        }
    }

    pub fn total_rows(&self) -> usize {
        self.result.as_ref().map(|r| r.row_count).unwrap_or(0)
    }
}

/// Which modal, if any, is capturing input.
pub enum Modal {
    None,
    Profile(ProfileForm),
    Export(ExportDialog),
    Palette(CommandPalette),
    Confirm(Confirm),
}

impl Modal {
    pub fn is_open(&self) -> bool {
        !matches!(self, Modal::None)
    }
}

/// Add/edit form for a connection profile.
pub struct ProfileForm {
    /// `None` when adding, `Some(id)` when editing an existing profile.
    pub editing: Option<String>,
    pub fields: Vec<(&'static str, String)>,
    pub selected: usize,
    pub error: Option<String>,
}

impl ProfileForm {
    pub const FIELDS: [&'static str; 7] = [
        "id", "name", "driver", "uri", "username", "password", "color",
    ];

    pub fn blank() -> Self {
        Self {
            editing: None,
            fields: Self::FIELDS.iter().map(|f| (*f, String::new())).collect(),
            selected: 0,
            error: None,
        }
    }

    pub fn from_profile(profile: &Profile) -> Self {
        let color = profile
            .color
            .map(|c| format!("{c:?}").to_lowercase())
            .unwrap_or_default();
        let values = [
            profile.id.clone(),
            profile.name.clone(),
            profile.driver.clone(),
            profile.uri.clone(),
            profile.username.clone().unwrap_or_default(),
            String::new(),
            color,
        ];
        Self {
            editing: Some(profile.id.clone()),
            fields: Self::FIELDS
                .iter()
                .zip(values)
                .map(|(f, v)| (*f, v))
                .collect(),
            selected: 0,
            error: None,
        }
    }

    pub fn value(&self, name: &str) -> &str {
        self.fields
            .iter()
            .find(|(f, _)| *f == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }

    /// Password fields render masked.
    pub fn is_secret(&self, index: usize) -> bool {
        self.fields.get(index).map(|(f, _)| *f) == Some("password")
    }
}

/// Format picker plus destination path for exporting the current result set.
pub struct ExportDialog {
    pub format: ExportFormat,
    pub path: String,
    pub error: Option<String>,
    /// `false` while picking the format, `true` while editing the path.
    pub editing_path: bool,
}

/// `:`-driven command palette.
pub struct CommandPalette {
    pub query: String,
    pub selected: usize,
    pub matches: Vec<Command>,
}

/// Actions reachable from the command palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Connect,
    Disconnect,
    AddProfile,
    EditProfile,
    DeleteProfile,
    RefreshCatalog,
    NewTab,
    CloseTab,
    Export,
    Quit,
}

impl Command {
    pub const ALL: [Command; 10] = [
        Command::Connect,
        Command::Disconnect,
        Command::AddProfile,
        Command::EditProfile,
        Command::DeleteProfile,
        Command::RefreshCatalog,
        Command::NewTab,
        Command::CloseTab,
        Command::Export,
        Command::Quit,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Command::Connect => "connect",
            Command::Disconnect => "disconnect",
            Command::AddProfile => "profile: add",
            Command::EditProfile => "profile: edit",
            Command::DeleteProfile => "profile: delete",
            Command::RefreshCatalog => "catalog: refresh",
            Command::NewTab => "tab: new",
            Command::CloseTab => "tab: close",
            Command::Export => "results: export",
            Command::Quit => "quit",
        }
    }
}

/// A yes/no prompt.
pub struct Confirm {
    pub message: String,
    pub action: Command,
}

/// Autocomplete popup state.
pub struct CompletionPopup {
    pub items: Vec<Completion>,
    pub selected: usize,
}

/// One rendered line of the catalog tree, flattened across every connection.
#[derive(Debug, Clone)]
pub struct CatalogRow {
    pub profile_id: String,
    /// Indices from the profile's root node down to this one.
    pub path: Vec<usize>,
    pub depth: usize,
    pub name: String,
    pub kind: NodeKind,
    pub load_state: LoadState,
    pub expanded: bool,
    /// Fully-qualified `catalog.schema.table` for this row's location.
    pub qualified: String,
}

pub struct App {
    pub profiles: Vec<Profile>,
    pub connections: HashMap<String, ConnectionState>,
    /// Synthetic root per profile; its children are the catalogs.
    pub catalogs: HashMap<String, CatalogNode>,
    pub tabs: Vec<EditorTab>,
    pub active_tab: usize,
    pub focus: Pane,
    pub status: String,
    pub modal: Modal,
    pub completion: Option<CompletionPopup>,
    pub catalog_selected: usize,
    pub connections_manager: ConnectionManager,
    pub should_quit: bool,
    /// Animation frame for the running-query spinner.
    pub spinner: usize,
    next_tab_id: usize,
}

impl App {
    pub fn new(profiles: Vec<Profile>, manager: ConnectionManager) -> Self {
        let connections = profiles
            .iter()
            .map(|p| (p.id.clone(), ConnectionState::Disconnected))
            .collect();
        let first_profile = profiles.first().map(|p| p.id.clone());
        Self {
            profiles,
            connections,
            catalogs: HashMap::new(),
            tabs: vec![EditorTab::new(1, first_profile)],
            active_tab: 0,
            focus: Pane::Editor,
            status: "ready".into(),
            modal: Modal::None,
            completion: None,
            catalog_selected: 0,
            connections_manager: manager,
            should_quit: false,
            spinner: 0,
            next_tab_id: 2,
        }
    }

    pub fn tab(&self) -> &EditorTab {
        &self.tabs[self.active_tab.min(self.tabs.len() - 1)]
    }

    pub fn tab_mut(&mut self) -> &mut EditorTab {
        let idx = self.active_tab.min(self.tabs.len() - 1);
        &mut self.tabs[idx]
    }

    pub fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn state_of(&self, id: &str) -> ConnectionState {
        self.connections
            .get(id)
            .cloned()
            .unwrap_or(ConnectionState::Disconnected)
    }

    /// The profile the active tab runs against, falling back to the first
    /// connected one.
    pub fn active_profile_id(&self) -> Option<String> {
        self.tab()
            .profile_id
            .clone()
            .or_else(|| self.profiles.first().map(|p| p.id.clone()))
    }

    // ---- catalog tree ---------------------------------------------------

    /// Flatten every profile's expanded catalog tree into render rows.
    pub fn catalog_rows(&self) -> Vec<CatalogRow> {
        let mut rows = Vec::new();
        for profile in &self.profiles {
            let Some(root) = self.catalogs.get(&profile.id) else {
                continue;
            };
            for (index, child) in root.children.iter().enumerate() {
                flatten(
                    &profile.id,
                    child,
                    vec![index],
                    0,
                    &mut Vec::new(),
                    &mut rows,
                );
            }
        }
        rows
    }

    pub fn selected_row(&self) -> Option<CatalogRow> {
        self.catalog_rows().get(self.catalog_selected).cloned()
    }

    /// Address of a row within its profile's tree, for lazy loading.
    pub fn path_of(&self, row: &CatalogRow) -> CatalogPath {
        let mut path = CatalogPath::default();
        let Some(root) = self.catalogs.get(&row.profile_id) else {
            return path;
        };
        let mut node = root;
        for index in &row.path {
            let Some(child) = node.children.get(*index) else {
                break;
            };
            match child.kind {
                NodeKind::Catalog => path.catalog = Some(child.name.clone()),
                NodeKind::Schema => path.schema = Some(child.name.clone()),
                NodeKind::Table | NodeKind::View => path.table = Some(child.name.clone()),
                NodeKind::Column { .. } => {}
            }
            node = child;
        }
        path
    }

    // ---- lifecycle ------------------------------------------------------

    pub fn connect(&mut self, profile_id: &str) {
        let Some(profile) = self.profile(profile_id).cloned() else {
            self.status = format!("no such profile: {profile_id}");
            return;
        };
        let secret = profile
            .secret_ref
            .as_deref()
            .and_then(|r| crate::config::get_secret(r).ok().flatten());
        self.connections
            .insert(profile.id.clone(), ConnectionState::Connecting);
        self.connections_manager.ensure_thread(&profile, secret);
        self.connections_manager
            .send(&profile.id, DbRequest::Connect);
        self.status = format!("connecting to {}…", profile.name);
    }

    pub fn disconnect(&mut self, profile_id: &str) {
        self.connections_manager
            .send(profile_id, DbRequest::Disconnect);
        self.connections_manager.shutdown(profile_id);
        self.connections
            .insert(profile_id.to_string(), ConnectionState::Disconnected);
        self.catalogs.remove(profile_id);
        self.catalog_selected = 0;
        self.status = format!("disconnected {profile_id}");
    }

    /// Ask the worker for the children of `path`, marking the node loading.
    pub fn request_children(&mut self, profile_id: &str, path: CatalogPath, tree_path: &[usize]) {
        if let Some(root) = self.catalogs.get_mut(profile_id)
            && let Some(node) = root.node_at_mut(tree_path)
        {
            node.load_state = LoadState::Loading;
        }
        self.connections_manager
            .send(profile_id, DbRequest::LoadChildren { path });
    }

    /// Run the statement under the cursor in the active tab.
    pub fn run_current_statement(&mut self) {
        let Some(profile_id) = self.active_profile_id() else {
            self.status = "no connection selected".into();
            return;
        };
        if !self.state_of(&profile_id).is_connected() {
            self.status = format!("{profile_id} is not connected");
            return;
        }
        let text = self.tab().text();
        let cursor = self.tab().cursor_offset();
        let Some(statement) = crate::sql::statement::statement_at(&text, cursor) else {
            self.status = "no statement under cursor".into();
            return;
        };
        let tab_id = self.tab().id;
        {
            let tab = self.tab_mut();
            tab.running = true;
            tab.error = None;
            tab.row_offset = 0;
            tab.col_offset = 0;
        }
        self.connections_manager.send(
            &profile_id,
            DbRequest::Execute {
                tab_id,
                sql: statement.sql,
            },
        );
        self.status = "running…".into();
    }

    /// Best-effort abort of the statement running in the active tab.
    pub fn cancel_running_statement(&mut self) {
        let Some(profile_id) = self.active_profile_id() else {
            return;
        };
        if !self.tab().running {
            return;
        }
        self.connections_manager
            .send(&profile_id, DbRequest::Cancel);
        self.status = "cancelling…".into();
    }

    pub fn new_tab(&mut self) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let profile = self.active_profile_id();
        self.tabs.push(EditorTab::new(id, profile));
        self.active_tab = self.tabs.len() - 1;
    }

    pub fn close_tab(&mut self) {
        if self.tabs.len() <= 1 {
            self.status = "last tab stays open".into();
            return;
        }
        let idx = self.active_tab.min(self.tabs.len() - 1);
        self.tabs.remove(idx);
        self.active_tab = idx.saturating_sub(1);
    }

    // ---- event handling -------------------------------------------------

    pub(crate) fn handle_db_event(&mut self, ev: DbEvent) {
        match ev {
            DbEvent::State { profile_id, state } => {
                if state.is_connected() && !self.catalogs.contains_key(&profile_id) {
                    let mut root = CatalogNode::new(NodeKind::Catalog, profile_id.clone());
                    root.expanded = true;
                    root.load_state = LoadState::Loading;
                    self.catalogs.insert(profile_id.clone(), root);
                    self.connections_manager.send(
                        &profile_id,
                        DbRequest::LoadChildren {
                            path: CatalogPath::default(),
                        },
                    );
                }
                self.status = match &state {
                    ConnectionState::Connected => format!("connected {profile_id}"),
                    ConnectionState::Errored(e) => format!("{profile_id}: {e}"),
                    _ => self.status.clone(),
                };
                self.connections.insert(profile_id, state);
            }
            DbEvent::CatalogChildren {
                profile_id,
                path,
                result,
            } => self.apply_children(&profile_id, &path, result),
            DbEvent::QueryStarted { tab_id, .. } => {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.running = true;
                }
            }
            DbEvent::QueryDone {
                profile_id,
                tab_id,
                result,
            } => {
                let summary = match result.as_ref() {
                    Ok(r) => format!(
                        "{} rows · {}ms · {}",
                        r.row_count,
                        r.elapsed.as_millis(),
                        profile_id
                    ),
                    Err(e) => format!("error · {profile_id}: {}", e.message),
                };
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.running = false;
                    match *result {
                        Ok(r) => {
                            tab.result = Some(r);
                            tab.error = None;
                        }
                        Err(e) => {
                            tab.error = Some(e);
                            tab.result = None;
                        }
                    }
                    tab.row_offset = 0;
                }
                self.status = summary;
            }
        }
    }

    /// Graft freshly loaded children onto the node they belong to.
    fn apply_children(
        &mut self,
        profile_id: &str,
        path: &CatalogPath,
        result: Result<Vec<CatalogNode>, String>,
    ) {
        let Some(root) = self.catalogs.get_mut(profile_id) else {
            return;
        };
        let Some(node) = find_by_path(root, path) else {
            return;
        };
        match result {
            Ok(children) => {
                node.children = children;
                node.load_state = LoadState::Loaded;
                node.expanded = true;
            }
            Err(e) => {
                node.load_state = LoadState::Error(e.clone());
                self.status = format!("catalog: {e}");
            }
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if self.modal.is_open() {
            self.handle_modal_key(key);
            return;
        }
        if self.completion.is_some() && self.handle_completion_key(key) {
            return;
        }
        // With no popup or modal open, Esc is free — use it to abort a
        // statement rather than leaving the user to wait one out.
        if key.code == KeyCode::Esc && self.tab().running {
            self.cancel_running_statement();
            return;
        }
        if self.handle_global_key(key) {
            return;
        }
        match self.focus {
            Pane::Catalog => self.handle_catalog_key(key),
            Pane::Editor => self.handle_editor_key(key),
            Pane::Results => self.handle_results_key(key),
        }
    }

    /// Keys that work regardless of focus. Returns `true` if consumed.
    fn handle_global_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if !ctrl {
            // `:` opens the palette everywhere except inside the editor, where
            // it is a literal character.
            if key.code == KeyCode::Char(':') && self.focus != Pane::Editor {
                self.open_palette();
                return true;
            }
            return false;
        }
        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
                true
            }
            KeyCode::Char('h') => {
                self.focus = match self.focus {
                    Pane::Catalog => Pane::Catalog,
                    Pane::Editor | Pane::Results => Pane::Catalog,
                };
                true
            }
            KeyCode::Char('l') => {
                self.focus = match self.focus {
                    Pane::Catalog => Pane::Editor,
                    other => other,
                };
                true
            }
            KeyCode::Char('j') => {
                self.focus = match self.focus {
                    Pane::Editor => Pane::Results,
                    other => other,
                };
                true
            }
            KeyCode::Char('k') => {
                self.focus = match self.focus {
                    Pane::Results => Pane::Editor,
                    other => other,
                };
                true
            }
            KeyCode::Char('t') => {
                self.new_tab();
                true
            }
            KeyCode::Char('w') => {
                self.close_tab();
                true
            }
            KeyCode::Char('e') => {
                self.open_export();
                true
            }
            KeyCode::Enter => {
                self.run_current_statement();
                true
            }
            _ => false,
        }
    }

    fn handle_catalog_key(&mut self, key: KeyEvent) {
        let rows = self.catalog_rows();
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if !rows.is_empty() {
                    self.catalog_selected = (self.catalog_selected + 1).min(rows.len() - 1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.catalog_selected = self.catalog_selected.saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.toggle_selected_node(),
            KeyCode::Char('r') => self.refresh_selected_node(),
            KeyCode::Char('c') => {
                // With nothing connected the tree is empty, so fall back to the
                // active tab's profile — otherwise the pane the empty-state
                // hint points at would have no way to connect.
                let id = rows
                    .get(self.catalog_selected)
                    .map(|row| row.profile_id.clone())
                    .or_else(|| self.active_profile_id());
                if let Some(id) = id {
                    if self.state_of(&id).is_connected() {
                        self.disconnect(&id);
                    } else {
                        self.connect(&id);
                    }
                }
            }
            _ => {}
        }
    }

    fn toggle_selected_node(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if matches!(row.kind, NodeKind::Column { .. }) {
            return;
        }
        let path = self.path_of(&row);
        let needs_load = {
            let root = self.catalogs.get_mut(&row.profile_id);
            match root.and_then(|r| r.node_at_mut(&row.path)) {
                Some(node) => {
                    node.expanded = !node.expanded;
                    node.expanded && node.load_state == LoadState::NotLoaded
                }
                None => false,
            }
        };
        if needs_load {
            self.request_children(&row.profile_id.clone(), path, &row.path.clone());
        }
    }

    fn refresh_selected_node(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let path = self.path_of(&row);
        if let Some(root) = self.catalogs.get_mut(&row.profile_id)
            && let Some(node) = root.node_at_mut(&row.path)
        {
            node.children.clear();
            node.load_state = LoadState::NotLoaded;
        }
        self.request_children(&row.profile_id.clone(), path, &row.path.clone());
        self.status = format!("refreshing {}", row.qualified);
    }

    fn handle_editor_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char(' ') {
            self.open_completion(true);
            return;
        }
        if ctrl && key.code == KeyCode::Char('i') {
            self.insert_selected_table();
            return;
        }
        if key.code == KeyCode::F(5) {
            self.run_current_statement();
            return;
        }
        let input = tui_textarea::Input::from(TermEvent::Key(key));
        let changed = self.tab_mut().textarea.input(input);
        if changed {
            self.tab_mut().reparse();
            self.open_completion(false);
        }
    }

    fn handle_results_key(&mut self, key: KeyEvent) {
        let page = 20;
        let total = self.tab().total_rows();
        let tab = self.tab_mut();
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                tab.row_offset = (tab.row_offset + 1).min(total.saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => tab.row_offset = tab.row_offset.saturating_sub(1),
            KeyCode::PageDown => {
                tab.row_offset = (tab.row_offset + page).min(total.saturating_sub(1))
            }
            KeyCode::PageUp => tab.row_offset = tab.row_offset.saturating_sub(page),
            KeyCode::Home => tab.row_offset = 0,
            KeyCode::End => tab.row_offset = total.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') => tab.col_offset += 1,
            KeyCode::Left | KeyCode::Char('h') => tab.col_offset = tab.col_offset.saturating_sub(1),
            _ => {}
        }
    }

    /// Insert the fully-qualified name of the selected catalog row at the
    /// editor cursor.
    fn insert_selected_table(&mut self) {
        let Some(row) = self.selected_row() else {
            self.status = "nothing selected in the catalog".into();
            return;
        };
        let qualified = row.qualified.clone();
        self.tab_mut().textarea.insert_str(&qualified);
        self.tab_mut().reparse();
        self.status = format!("inserted {qualified}");
    }

    // ---- completion -----------------------------------------------------

    /// Recompute the completion popup. `forced` bypasses the minimum prefix
    /// length.
    fn open_completion(&mut self, forced: bool) {
        let text = self.tab().text();
        let cursor = self.tab().cursor_offset();
        let prefix = crate::sql::complete::prefix_at(&text, cursor).to_string();
        if !forced && prefix.is_empty() {
            self.completion = None;
            return;
        }
        let context = crate::sql::complete::context_at(&text, cursor);
        let roots: Vec<&CatalogNode> = self.catalogs.values().collect();
        let items = crate::sql::complete::candidates(&roots, &context, &prefix, COMPLETION_LIMIT);
        self.completion = if items.is_empty() {
            None
        } else {
            Some(CompletionPopup { items, selected: 0 })
        };
    }

    /// Returns `true` when the popup consumed the key.
    fn handle_completion_key(&mut self, key: KeyEvent) -> bool {
        let Some(popup) = self.completion.as_mut() else {
            return false;
        };
        match key.code {
            KeyCode::Esc => {
                self.completion = None;
                true
            }
            KeyCode::Down => {
                popup.selected = (popup.selected + 1) % popup.items.len();
                true
            }
            KeyCode::Up => {
                popup.selected = popup
                    .selected
                    .checked_sub(1)
                    .unwrap_or(popup.items.len() - 1);
                true
            }
            KeyCode::Tab | KeyCode::Enter => {
                self.accept_completion();
                true
            }
            _ => false,
        }
    }

    fn accept_completion(&mut self) {
        let Some(popup) = self.completion.take() else {
            return;
        };
        let Some(item) = popup.items.get(popup.selected).cloned() else {
            return;
        };
        let prefix_chars = self.tab().text()[..self.tab().cursor_offset()]
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .count();
        {
            let textarea = &mut self.tab_mut().textarea;
            for _ in 0..prefix_chars {
                textarea.delete_char();
            }
            textarea.insert_str(&item.text);
        }
        self.tab_mut().reparse();
    }

    // ---- modals ---------------------------------------------------------

    fn open_palette(&mut self) {
        self.modal = Modal::Palette(CommandPalette {
            query: String::new(),
            selected: 0,
            matches: Command::ALL.to_vec(),
        });
    }

    fn open_export(&mut self) {
        if self.tab().result.is_none() {
            self.status = "no result set to export".into();
            return;
        }
        self.modal = Modal::Export(ExportDialog {
            format: ExportFormat::Csv,
            path: crate::export::default_filename(ExportFormat::Csv),
            error: None,
            editing_path: false,
        });
    }

    fn handle_modal_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.modal = Modal::None;
            return;
        }
        match &mut self.modal {
            Modal::Palette(_) => self.handle_palette_key(key),
            Modal::Profile(_) => self.handle_profile_key(key),
            Modal::Export(_) => self.handle_export_key(key),
            Modal::Confirm(_) => self.handle_confirm_key(key),
            Modal::None => {}
        }
    }

    fn handle_palette_key(&mut self, key: KeyEvent) {
        let Modal::Palette(palette) = &mut self.modal else {
            return;
        };
        match key.code {
            KeyCode::Char(c) => {
                palette.query.push(c);
                palette.matches = filter_commands(&palette.query);
                palette.selected = 0;
            }
            KeyCode::Backspace => {
                palette.query.pop();
                palette.matches = filter_commands(&palette.query);
                palette.selected = 0;
            }
            KeyCode::Down => {
                if !palette.matches.is_empty() {
                    palette.selected = (palette.selected + 1) % palette.matches.len();
                }
            }
            KeyCode::Up => {
                if !palette.matches.is_empty() {
                    palette.selected = palette
                        .selected
                        .checked_sub(1)
                        .unwrap_or(palette.matches.len() - 1);
                }
            }
            KeyCode::Enter => {
                let command = palette.matches.get(palette.selected).copied();
                self.modal = Modal::None;
                if let Some(command) = command {
                    self.run_command(command);
                }
            }
            _ => {}
        }
    }

    pub fn run_command(&mut self, command: Command) {
        match command {
            Command::Connect => {
                if let Some(id) = self.selected_profile_id() {
                    self.connect(&id);
                }
            }
            Command::Disconnect => {
                if let Some(id) = self.selected_profile_id() {
                    self.disconnect(&id);
                }
            }
            Command::AddProfile => self.modal = Modal::Profile(ProfileForm::blank()),
            Command::EditProfile => {
                if let Some(profile) = self
                    .selected_profile_id()
                    .and_then(|id| self.profile(&id).cloned())
                {
                    self.modal = Modal::Profile(ProfileForm::from_profile(&profile));
                }
            }
            Command::DeleteProfile => {
                if let Some(id) = self.selected_profile_id() {
                    self.modal = Modal::Confirm(Confirm {
                        message: format!("delete profile {id}?"),
                        action: Command::DeleteProfile,
                    });
                }
            }
            Command::RefreshCatalog => self.refresh_selected_node(),
            Command::NewTab => self.new_tab(),
            Command::CloseTab => self.close_tab(),
            Command::Export => self.open_export(),
            Command::Quit => self.should_quit = true,
        }
    }

    /// Profile targeted by profile-scoped commands: the catalog selection if
    /// there is one, else the active tab's connection.
    fn selected_profile_id(&self) -> Option<String> {
        self.selected_row()
            .map(|r| r.profile_id)
            .or_else(|| self.active_profile_id())
    }

    fn handle_profile_key(&mut self, key: KeyEvent) {
        let Modal::Profile(form) = &mut self.modal else {
            return;
        };
        match key.code {
            KeyCode::Tab | KeyCode::Down => form.selected = (form.selected + 1) % form.fields.len(),
            KeyCode::BackTab | KeyCode::Up => {
                form.selected = form
                    .selected
                    .checked_sub(1)
                    .unwrap_or(form.fields.len() - 1)
            }
            KeyCode::Char(c) => form.fields[form.selected].1.push(c),
            KeyCode::Backspace => {
                form.fields[form.selected].1.pop();
            }
            KeyCode::Enter => self.commit_profile_form(),
            _ => {}
        }
    }

    fn commit_profile_form(&mut self) {
        let Modal::Profile(form) = &mut self.modal else {
            return;
        };
        let id = form.value("id").trim().to_string();
        if id.is_empty() {
            form.error = Some("id is required".into());
            return;
        }
        let uri = form.value("uri").trim().to_string();
        let driver = form.value("driver").trim().to_string();
        if driver.is_empty() {
            form.error = Some("driver is required".into());
            return;
        }
        let name = {
            let n = form.value("name").trim();
            if n.is_empty() {
                id.clone()
            } else {
                n.to_string()
            }
        };
        let username = non_empty(form.value("username"));
        let password = non_empty(form.value("password"));
        let color = parse_color(form.value("color"));
        let editing = form.editing.clone();

        let secret_ref = password.as_ref().map(|_| format!("osage/{id}"));
        if let (Some(reference), Some(secret)) = (secret_ref.as_ref(), password.as_ref())
            && let Err(e) = crate::config::set_secret(reference, secret)
        {
            if let Modal::Profile(form) = &mut self.modal {
                form.error = Some(format!("keychain: {e}"));
            }
            return;
        }

        // Keep an existing secret_ref when the password field was left blank.
        let existing_secret_ref = editing
            .as_ref()
            .and_then(|id| self.profile(id))
            .and_then(|p| p.secret_ref.clone());
        let options = editing
            .as_ref()
            .and_then(|id| self.profile(id))
            .map(|p| p.options.clone())
            .unwrap_or_default();

        let profile = Profile {
            id: id.clone(),
            name,
            driver,
            uri,
            username,
            secret_ref: secret_ref.or(existing_secret_ref),
            options,
            color,
        };

        match editing {
            Some(old_id) => {
                if let Some(slot) = self.profiles.iter_mut().find(|p| p.id == old_id) {
                    *slot = profile;
                }
                if old_id != id
                    && let Some(state) = self.connections.remove(&old_id)
                {
                    self.connections.insert(id.clone(), state);
                }
            }
            None => {
                self.profiles.push(profile);
                self.connections
                    .insert(id.clone(), ConnectionState::Disconnected);
            }
        }

        match crate::config::save_profiles(&self.profiles) {
            Ok(()) => {
                self.modal = Modal::None;
                self.status = format!("saved profile {id}");
            }
            Err(e) => {
                if let Modal::Profile(form) = &mut self.modal {
                    form.error = Some(e);
                }
            }
        }
    }

    fn handle_export_key(&mut self, key: KeyEvent) {
        let Modal::Export(dialog) = &mut self.modal else {
            return;
        };
        if dialog.editing_path {
            match key.code {
                KeyCode::Char(c) => dialog.path.push(c),
                KeyCode::Backspace => {
                    dialog.path.pop();
                }
                KeyCode::Enter => self.commit_export(),
                KeyCode::Tab => dialog.editing_path = false,
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Left | KeyCode::Up => {
                let index = ExportFormat::ALL
                    .iter()
                    .position(|f| *f == dialog.format)
                    .unwrap_or(0);
                dialog.format =
                    ExportFormat::ALL[index.checked_sub(1).unwrap_or(ExportFormat::ALL.len() - 1)];
                dialog.path = crate::export::default_filename(dialog.format);
            }
            KeyCode::Right | KeyCode::Down => {
                let index = ExportFormat::ALL
                    .iter()
                    .position(|f| *f == dialog.format)
                    .unwrap_or(0);
                dialog.format = ExportFormat::ALL[(index + 1) % ExportFormat::ALL.len()];
                dialog.path = crate::export::default_filename(dialog.format);
            }
            KeyCode::Tab => dialog.editing_path = true,
            KeyCode::Enter => self.commit_export(),
            _ => {}
        }
    }

    fn commit_export(&mut self) {
        let Modal::Export(dialog) = &self.modal else {
            return;
        };
        let (format, path) = (dialog.format, dialog.path.clone());
        let Some(result) = self.tab().result.as_ref() else {
            self.modal = Modal::None;
            return;
        };
        match crate::export::export(result, format, std::path::Path::new(&path)) {
            Ok(rows) => {
                self.modal = Modal::None;
                self.status = format!("exported {rows} rows → {path}");
            }
            Err(e) => {
                if let Modal::Export(dialog) = &mut self.modal {
                    dialog.error = Some(e);
                }
            }
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) {
        let Modal::Confirm(confirm) = &self.modal else {
            return;
        };
        let action = confirm.action;
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                self.modal = Modal::None;
                if action == Command::DeleteProfile {
                    self.delete_selected_profile();
                }
            }
            KeyCode::Char('n') => self.modal = Modal::None,
            _ => {}
        }
    }

    fn delete_selected_profile(&mut self) {
        let Some(id) = self.selected_profile_id() else {
            return;
        };
        if let Some(secret_ref) = self.profile(&id).and_then(|p| p.secret_ref.clone()) {
            let _ = crate::config::delete_secret(&secret_ref);
        }
        self.disconnect(&id);
        self.profiles.retain(|p| p.id != id);
        self.connections.remove(&id);
        match crate::config::save_profiles(&self.profiles) {
            Ok(()) => self.status = format!("deleted profile {id}"),
            Err(e) => self.status = format!("delete failed: {e}"),
        }
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn parse_color(value: &str) -> Option<ratatui::style::Color> {
    use ratatui::style::Color;
    match value.trim().to_lowercase().as_str() {
        "" => None,
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "white" => Some(Color::White),
        other => other.parse().ok(),
    }
}

fn filter_commands(query: &str) -> Vec<Command> {
    let query = query.to_lowercase();
    Command::ALL
        .iter()
        .copied()
        .filter(|c| c.label().contains(&query))
        .collect()
}

/// Identifiers that need it are wrapped in SQL's standard double quotes, so a
/// catalog like `osage-test` or a table like `Order` pastes into the editor as
/// valid SQL. Bare lowercase identifiers are left alone — quoting everything
/// would be correct but unpleasant to read.
fn quote_ident(name: &str) -> String {
    let plain = !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !RESERVED.contains(&name);
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// Words that cannot appear unquoted where a table name is expected. Kept
/// deliberately short — it only needs the names people actually give objects.
const RESERVED: [&str; 24] = [
    "all", "and", "as", "by", "case", "column", "default", "desc", "end", "from", "group", "in",
    "index", "is", "join", "like", "limit", "not", "null", "or", "order", "select", "table",
    "where",
];

/// Walk a subtree, emitting a row per visible node.
fn flatten(
    profile_id: &str,
    node: &CatalogNode,
    path: Vec<usize>,
    depth: usize,
    ancestors: &mut Vec<String>,
    out: &mut Vec<CatalogRow>,
) {
    let qualified = {
        let mut parts = ancestors.clone();
        if node.expandable() {
            parts.push(node.name.clone());
        }
        parts
            .iter()
            .map(|part| quote_ident(part))
            .collect::<Vec<_>>()
            .join(".")
    };
    out.push(CatalogRow {
        profile_id: profile_id.to_string(),
        path: path.clone(),
        depth,
        name: node.name.clone(),
        kind: node.kind.clone(),
        load_state: node.load_state.clone(),
        expanded: node.expanded,
        qualified,
    });
    if !node.expanded {
        return;
    }
    if node.expandable() {
        ancestors.push(node.name.clone());
    }
    for (index, child) in node.children.iter().enumerate() {
        let mut child_path = path.clone();
        child_path.push(index);
        flatten(profile_id, child, child_path, depth + 1, ancestors, out);
    }
    if node.expandable() {
        ancestors.pop();
    }
}

/// Resolve a `CatalogPath` to the node it names within a profile's tree.
fn find_by_path<'a>(root: &'a mut CatalogNode, path: &CatalogPath) -> Option<&'a mut CatalogNode> {
    let mut node = root;
    for name in [
        path.catalog.as_deref(),
        path.schema.as_deref(),
        path.table.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let index = node.children.iter().position(|c| c.name == name)?;
        node = &mut node.children[index];
    }
    Some(node)
}

/// Set up the terminal, run the event loop, and restore on the way out.
pub fn run() -> color_eyre::Result<()> {
    let profiles = crate::config::load_profiles().unwrap_or_default();
    let (manager, db_events) = ConnectionManager::new();
    let mut app = App::new(profiles, manager);
    let events = event::spawn(db_events, TICK);

    let mut terminal = ratatui::init();
    let outcome = event_loop(&mut terminal, &mut app, events);
    ratatui::restore();
    app.connections_manager.shutdown_all();
    outcome
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    events: Receiver<AppEvent>,
) -> color_eyre::Result<()> {
    terminal.draw(|frame| ui::draw(frame, app))?;
    while let Ok(ev) = events.recv() {
        match ev {
            AppEvent::Input(TermEvent::Key(key)) => app.handle_key(key),
            AppEvent::Input(_) => {}
            AppEvent::Db(db) => app.handle_db_event(db),
            AppEvent::Tick => app.spinner = app.spinner.wrapping_add(1),
        }
        if app.should_quit {
            break;
        }
        terminal.draw(|frame| ui::draw(frame, app))?;
    }
    Ok(())
}

use crate::ui;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_identifiers_are_not_quoted() {
        assert_eq!(quote_ident("orders"), "orders");
        assert_eq!(quote_ident("order_items_2"), "order_items_2");
    }

    #[test]
    fn identifiers_needing_quotes_get_them() {
        assert_eq!(quote_ident("osage-test"), "\"osage-test\"");
        assert_eq!(quote_ident("Orders"), "\"Orders\"");
        assert_eq!(quote_ident("two words"), "\"two words\"");
        assert_eq!(quote_ident("2fast"), "\"2fast\"");
        assert_eq!(quote_ident(""), "\"\"");
    }

    #[test]
    fn reserved_words_are_quoted() {
        assert_eq!(quote_ident("order"), "\"order\"");
        assert_eq!(quote_ident("select"), "\"select\"");
    }

    #[test]
    fn embedded_quotes_are_doubled() {
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }

    /// The flattened tree is what `Ctrl-i` pastes, so its qualified names must
    /// already be valid SQL.
    #[test]
    fn qualified_names_quote_every_level_that_needs_it() {
        let mut column = CatalogNode::new(
            NodeKind::Column {
                data_type: "int".into(),
                nullable: true,
            },
            "id",
        );
        column.expanded = false;
        let mut table = CatalogNode::new(NodeKind::Table, "customers");
        table.children = vec![column];
        table.expanded = true;
        let mut schema = CatalogNode::new(NodeKind::Schema, "main");
        schema.children = vec![table];
        schema.expanded = true;
        let mut catalog = CatalogNode::new(NodeKind::Catalog, "osage-test");
        catalog.children = vec![schema];
        catalog.expanded = true;

        let mut rows = Vec::new();
        flatten("p", &catalog, vec![0], 0, &mut Vec::new(), &mut rows);

        let table_row = rows.iter().find(|r| r.name == "customers").unwrap();
        assert_eq!(table_row.qualified, "\"osage-test\".main.customers");
        // Column rows name their table, not themselves.
        let column_row = rows.iter().find(|r| r.name == "id").unwrap();
        assert_eq!(column_row.qualified, "\"osage-test\".main.customers");
    }
}
