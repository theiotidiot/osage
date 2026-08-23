//! Database layer.
//!
//! Every connection owns a dedicated OS thread. The ADBC handles never leave
//! that thread, requests arrive over a channel, and results are published to a
//! single application-wide event channel. Nothing here ever blocks the render
//! thread.

pub mod catalog;
pub mod worker;

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use crate::types::{CatalogNode, CatalogPath, ConnectionState, Profile, QueryError, QueryResult};

/// Work sent to a connection thread.
#[derive(Debug, Clone)]
pub enum DbRequest {
    /// Open the driver/database/connection. Idempotent.
    Connect,
    /// Drop the live connection but keep the thread alive.
    Disconnect,
    /// Lazily fetch the children of the node at `path`. An empty path means
    /// "top-level catalogs".
    LoadChildren { path: CatalogPath },
    /// Run a single statement on behalf of an editor tab.
    Execute { tab_id: usize, sql: String },
    /// Best-effort cancel of the in-flight statement.
    Cancel,
    /// Stop the thread.
    Shutdown,
}

/// Results published back to the application event loop.
#[derive(Debug)]
pub enum DbEvent {
    State {
        profile_id: String,
        state: ConnectionState,
    },
    CatalogChildren {
        profile_id: String,
        path: CatalogPath,
        result: Result<Vec<CatalogNode>, String>,
    },
    QueryStarted {
        tab_id: usize,
    },
    QueryDone {
        profile_id: String,
        tab_id: usize,
        result: Box<Result<QueryResult, QueryError>>,
    },
}

/// One live connection thread.
pub struct DbHandle {
    tx: Sender<DbRequest>,
    join: Option<JoinHandle<()>>,
}

impl DbHandle {
    /// Queue a request. Returns `false` if the worker thread is gone.
    pub fn send(&self, req: DbRequest) -> bool {
        self.tx.send(req).is_ok()
    }
}

impl Drop for DbHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(DbRequest::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Owns every connection thread and the shared event channel they publish to.
pub struct ConnectionManager {
    handles: HashMap<String, DbHandle>,
    events_tx: Sender<DbEvent>,
}

impl ConnectionManager {
    /// Build a manager plus the receiver the application loop drains.
    pub fn new() -> (Self, Receiver<DbEvent>) {
        let (events_tx, events_rx) = mpsc::channel();
        (
            Self {
                handles: HashMap::new(),
                events_tx,
            },
            events_rx,
        )
    }

    /// Spawn the worker thread for `profile` if it isn't running yet.
    pub fn ensure_thread(&mut self, profile: &Profile, secret: Option<String>) {
        if self.handles.contains_key(&profile.id) {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let events = self.events_tx.clone();
        let id = profile.id.clone();
        let profile = profile.clone();
        let join = std::thread::Builder::new()
            .name(format!("osage-db-{id}"))
            .spawn(move || worker::run(profile, secret, rx, events))
            .expect("spawn db worker");
        self.handles.insert(
            id,
            DbHandle {
                tx,
                join: Some(join),
            },
        );
    }

    /// Send a request to a profile's worker. No-op if it has no thread.
    pub fn send(&self, profile_id: &str, req: DbRequest) -> bool {
        match self.handles.get(profile_id) {
            Some(handle) => handle.send(req),
            None => false,
        }
    }

    /// Stop and join a profile's worker thread.
    pub fn shutdown(&mut self, profile_id: &str) {
        self.handles.remove(profile_id);
    }

    pub fn shutdown_all(&mut self) {
        self.handles.clear();
    }
}
