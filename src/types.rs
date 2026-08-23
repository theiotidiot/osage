//! Shared core types. Every module in the crate agrees on these; treat this file
//! as the contract between the DB layer, the UI layer and the SQL tooling.

use std::collections::HashMap;
use std::time::Duration;

use arrow::datatypes::SchemaRef;
use arrow::record_batch::RecordBatch;
use ratatui::style::Color;
use serde::{Deserialize, Serialize};

/// A saved connection profile. Secrets are never stored here — only a
/// `secret_ref` pointing at an OS keychain entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Stable slug, unique across profiles. Used as the key everywhere.
    pub id: String,
    pub name: String,
    /// ADBC driver path or name (e.g. `duckdb`, `postgresql`, `/path/to/lib.dylib`).
    pub driver: String,
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Keychain key, never the secret itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub options: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Color>,
}

impl Profile {
    /// Color to tag this profile with in the UI, falling back to a neutral.
    pub fn tag_color(&self) -> Color {
        self.color.unwrap_or(Color::Gray)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Errored(String),
}

impl ConnectionState {
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnectionState::Connected)
    }

    /// Glyph shown in the connection bar.
    pub fn indicator(&self) -> &'static str {
        match self {
            ConnectionState::Disconnected => "○",
            ConnectionState::Connecting => "◐",
            ConnectionState::Connected => "●",
            ConnectionState::Errored(_) => "✕",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    Catalog,
    Schema,
    Table,
    View,
    Column { data_type: String, nullable: bool },
}

impl NodeKind {
    pub fn glyph(&self) -> &'static str {
        match self {
            NodeKind::Catalog => "▤",
            NodeKind::Schema => "◇",
            NodeKind::Table => "▦",
            NodeKind::View => "◫",
            NodeKind::Column { .. } => "·",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState {
    NotLoaded,
    Loading,
    Loaded,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogNode {
    pub kind: NodeKind,
    pub name: String,
    pub children: Vec<CatalogNode>,
    pub load_state: LoadState,
    pub expanded: bool,
}

impl CatalogNode {
    pub fn new(kind: NodeKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
            children: Vec::new(),
            load_state: LoadState::NotLoaded,
            expanded: false,
        }
    }

    /// Leaf nodes (columns) can never be expanded.
    pub fn expandable(&self) -> bool {
        !matches!(self.kind, NodeKind::Column { .. })
    }

    /// Follow a path of child indices, if it resolves.
    pub fn node_at_mut(&mut self, path: &[usize]) -> Option<&mut CatalogNode> {
        match path.split_first() {
            None => Some(self),
            Some((head, rest)) => self.children.get_mut(*head)?.node_at_mut(rest),
        }
    }
}

/// Where a catalog node lives, independent of tree indices — used as the cache
/// key and to address lazy-load requests.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct CatalogPath {
    pub catalog: Option<String>,
    pub schema: Option<String>,
    pub table: Option<String>,
}

impl CatalogPath {
    /// `catalog.schema.table`, skipping absent levels.
    pub fn qualified(&self) -> String {
        [
            self.catalog.as_deref(),
            self.schema.as_deref(),
            self.table.as_deref(),
        ]
        .iter()
        .flatten()
        .copied()
        .collect::<Vec<_>>()
        .join(".")
    }
}

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub schema: SchemaRef,
    /// MVP materializes the whole result set; streaming comes later.
    pub batches: Vec<RecordBatch>,
    pub elapsed: Duration,
    pub row_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    pub message: String,
    /// Byte offsets into the executed statement, when the driver reports a
    /// position.
    pub span: Option<(usize, usize)>,
}

impl QueryError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            span: None,
        }
    }
}

/// Which pane currently owns the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Catalog,
    Editor,
    Results,
}

/// A completion candidate offered in the popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Text inserted when accepted.
    pub text: String,
    /// Short right-hand annotation (`table`, `column · int4`, `keyword`).
    pub detail: String,
    pub kind: CompletionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Keyword,
    Table,
    Column,
    Schema,
}

/// What the cursor position means syntactically, driving which candidates we
/// offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionContext {
    TableName,
    /// Tables (resolved through aliases) visible from the current statement.
    ColumnName {
        scoped_tables: Vec<String>,
    },
    Keyword,
    /// Fall back to a blended keyword + table + column list.
    Unknown,
}

/// Export formats offered for the current result set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Json,
    ArrowIpc,
}

impl ExportFormat {
    pub fn extension(&self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
            ExportFormat::ArrowIpc => "arrow",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ExportFormat::Csv => "CSV",
            ExportFormat::Json => "JSON",
            ExportFormat::ArrowIpc => "Arrow IPC",
        }
    }

    pub const ALL: [ExportFormat; 3] = [
        ExportFormat::Csv,
        ExportFormat::Json,
        ExportFormat::ArrowIpc,
    ];
}

/// A shareable snapshot of a syntax-highlighted line.
pub type HighlightedLine = Vec<(std::ops::Range<usize>, HighlightClass)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HighlightClass {
    Keyword,
    String,
    Number,
    Comment,
    Identifier,
    Function,
    Operator,
}
