//! Per-connection worker thread. Owns the ADBC handles for one profile.
//!
//! CONTRACT (implement, do not change the signature):
//!   `run` blocks until it receives `DbRequest::Shutdown` or its request
//!   channel closes, servicing requests one at a time and publishing every
//!   outcome on `events`.

use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

use adbc_core::LOAD_FLAG_DEFAULT;
use adbc_core::error::Error as AdbcError;
use adbc_core::options::{AdbcVersion, ObjectDepth, OptionDatabase, OptionValue};
use adbc_core::{Connection as _, Database as _, Driver as _, Statement as _};
use adbc_driver_manager::{ManagedConnection, ManagedDatabase, ManagedDriver};

use super::catalog;
use super::{DbEvent, DbRequest};
use crate::types::{CatalogPath, ConnectionState, Profile, QueryError, QueryResult};

/// The live ADBC handles. Never leaves this thread.
///
/// `ManagedConnection` internally holds an `Arc` to the database, which holds
/// one to the driver library, so release order is correct however these fields
/// are dropped. We keep the driver around so a reconnect does not have to
/// re-resolve the manifest.
struct Live {
    conn: ManagedConnection,
    _database: ManagedDatabase,
    _driver: ManagedDriver,
}

pub fn run(
    profile: Profile,
    secret: Option<String>,
    requests: Receiver<DbRequest>,
    events: Sender<DbEvent>,
) {
    let mut live: Option<Live> = None;

    // `recv` returning Err means every `DbHandle` was dropped: the app is gone.
    while let Ok(request) = requests.recv() {
        let alive = match request {
            DbRequest::Connect => handle_connect(&profile, secret.as_deref(), &mut live, &events),
            DbRequest::Disconnect => {
                live = None;
                publish(&events, state(&profile, ConnectionState::Disconnected))
            }
            DbRequest::LoadChildren { path } => {
                handle_load_children(&profile, live.as_ref(), path, &events)
            }
            DbRequest::Execute { tab_id, sql } => {
                handle_execute(&profile, live.as_mut(), tab_id, &sql, &events)
            }
            DbRequest::Cancel => {
                // Requests are serviced serially on this thread, so a Cancel
                // can only ever be dequeued *between* statements — never while
                // one is in flight. Honouring it is therefore a no-op in
                // practice for the MVP; cancelling a running query needs the
                // statement handle to be shared with a second thread, which is
                // deliberately out of scope. Wired up anyway so the request
                // path is complete.
                if let Some(l) = live.as_mut() {
                    let _ = l.conn.cancel();
                }
                true
            }
            DbRequest::Shutdown => break,
        };
        if !alive {
            // The event channel is closed: nobody is listening any more.
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// request handlers
// ---------------------------------------------------------------------------

fn handle_connect(
    profile: &Profile,
    secret: Option<&str>,
    live: &mut Option<Live>,
    events: &Sender<DbEvent>,
) -> bool {
    // Idempotent: a second Connect on a live connection just republishes.
    if live.is_some() {
        return publish(events, state(profile, ConnectionState::Connected));
    }

    if !publish(events, state(profile, ConnectionState::Connecting)) {
        return false;
    }

    match open(profile, secret) {
        Ok(opened) => {
            *live = Some(opened);
            publish(events, state(profile, ConnectionState::Connected))
        }
        Err(message) => publish(events, state(profile, ConnectionState::Errored(message))),
    }
}

fn handle_load_children(
    profile: &Profile,
    live: Option<&Live>,
    path: CatalogPath,
    events: &Sender<DbEvent>,
) -> bool {
    let result = match live {
        Some(l) => fetch_children(&l.conn, &path),
        None => Err(format!("{} is not connected", profile.name)),
    };
    publish(
        events,
        DbEvent::CatalogChildren {
            profile_id: profile.id.clone(),
            path,
            result,
        },
    )
}

fn handle_execute(
    profile: &Profile,
    live: Option<&mut Live>,
    tab_id: usize,
    sql: &str,
    events: &Sender<DbEvent>,
) -> bool {
    if !publish(events, DbEvent::QueryStarted { tab_id }) {
        return false;
    }

    let result = match live {
        Some(l) => execute(&mut l.conn, sql),
        None => Err(QueryError::new(format!(
            "{} is not connected",
            profile.name
        ))),
    };

    publish(
        events,
        DbEvent::QueryDone {
            profile_id: profile.id.clone(),
            tab_id,
            result: Box::new(result),
        },
    )
}

// ---------------------------------------------------------------------------
// ADBC work
// ---------------------------------------------------------------------------

fn open(profile: &Profile, secret: Option<&str>) -> Result<Live, String> {
    let mut driver = load_driver(&profile.driver)?;

    let mut opts: Vec<(OptionDatabase, OptionValue)> = Vec::new();
    opts.push((
        OptionDatabase::Uri,
        OptionValue::String(profile.uri.clone()),
    ));
    if let Some(username) = &profile.username {
        opts.push((
            OptionDatabase::Username,
            OptionValue::String(username.clone()),
        ));
    }
    if let Some(password) = secret {
        opts.push((
            OptionDatabase::Password,
            OptionValue::String(password.to_string()),
        ));
    }
    // Driver-specific keys straight from profiles.toml. `OptionDatabase::from`
    // folds the three canonical keys ("uri"/"username"/"password") back onto
    // their dedicated variants, so a profile may override them here too.
    for (key, value) in &profile.options {
        opts.push((
            OptionDatabase::from(key.as_str()),
            OptionValue::String(value.clone()),
        ));
    }

    let database = driver
        .new_database_with_opts(opts)
        .map_err(|e| format!("opening database: {}", describe(&e)))?;
    let conn = database
        .new_connection()
        .map_err(|e| format!("opening connection: {}", describe(&e)))?;

    Ok(Live {
        conn,
        _database: database,
        _driver: driver,
    })
}

/// True when `driver` should be treated as a shared-library path rather than a
/// driver name to resolve through manifests / the loader search path.
fn looks_like_path(driver: &str) -> bool {
    driver.contains('/')
        || driver.contains('\\')
        || driver.ends_with(".so")
        || driver.ends_with(".dylib")
        || driver.ends_with(".dll")
}

fn load_driver(spec: &str) -> Result<ManagedDriver, String> {
    // Prefer ADBC 1.1.0; some drivers only implement 1.0.0 and fail to init.
    let mut first_error = None;
    for version in [AdbcVersion::V110, AdbcVersion::V100] {
        match load_driver_at(spec, version) {
            Ok(driver) => return Ok(driver),
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }
    Err(format!(
        "loading driver `{spec}`: {}",
        first_error.unwrap_or_else(|| "unknown error".to_string())
    ))
}

fn load_driver_at(spec: &str, version: AdbcVersion) -> Result<ManagedDriver, String> {
    if looks_like_path(spec) {
        return ManagedDriver::load_dynamic_from_filename(spec, None, version)
            .map_err(|e| describe(&e));
    }
    // `load_from_name` resolves driver manifests plus the well-known driver
    // names; only if that finds nothing do we try a raw dynamic library.
    match ManagedDriver::load_from_name(spec, None, version, LOAD_FLAG_DEFAULT, None) {
        Ok(driver) => Ok(driver),
        Err(manifest_err) => {
            ManagedDriver::load_dynamic_from_name(spec, None, version).map_err(|e| {
                format!(
                    "{} (manifest lookup: {})",
                    describe(&e),
                    describe(&manifest_err)
                )
            })
        }
    }
}

fn fetch_children(
    conn: &ManagedConnection,
    path: &CatalogPath,
) -> Result<Vec<crate::types::CatalogNode>, String> {
    let (depth, catalog_filter, schema_filter, table_filter) = plan(path);

    let mut reader = conn
        .get_objects(
            depth,
            catalog_filter.as_deref(),
            schema_filter.as_deref(),
            table_filter.as_deref(),
            None,
            None,
        )
        .map_err(|e| format!("GetObjects failed: {}", describe(&e)))?;

    let mut batches = Vec::new();
    for batch in reader.by_ref() {
        batches.push(batch.map_err(|e| format!("reading GetObjects result: {e}"))?);
    }

    catalog::decode_children(&batches, path)
}

/// The narrowest `GetObjects` call that answers exactly one level below `path`.
///
/// Never `ObjectDepth::All` from the root: spec F2 forbids fetching the whole
/// catalog eagerly. Note the catalog/schema/table filters are *search patterns*
/// as far as ADBC is concerned; we pass names through literally, which is right
/// for every name that does not itself contain `%` or `_`, and harmlessly wide
/// (we re-match by name while decoding) for the ones that do.
fn plan(path: &CatalogPath) -> (ObjectDepth, Option<String>, Option<String>, Option<String>) {
    match (
        path.catalog.as_deref(),
        path.schema.as_deref(),
        path.table.as_deref(),
    ) {
        (None, _, _) => (ObjectDepth::Catalogs, None, None, None),
        (Some(catalog), None, _) => (
            ObjectDepth::Schemas,
            Some(catalog::server_catalog_filter(catalog).to_string()),
            None,
            None,
        ),
        (Some(catalog), Some(schema), None) => (
            ObjectDepth::Tables,
            Some(catalog::server_catalog_filter(catalog).to_string()),
            Some(schema.to_string()),
            None,
        ),
        (Some(catalog), Some(schema), Some(table)) => (
            ObjectDepth::Columns,
            Some(catalog::server_catalog_filter(catalog).to_string()),
            Some(schema.to_string()),
            Some(table.to_string()),
        ),
    }
}

fn execute(conn: &mut ManagedConnection, sql: &str) -> Result<QueryResult, QueryError> {
    let started = Instant::now();

    let mut statement = conn
        .new_statement()
        .map_err(|e| QueryError::new(describe(&e)))?;
    statement
        .set_sql_query(sql)
        .map_err(|e| QueryError::new(describe(&e)))?;
    let mut reader = statement
        .execute()
        .map_err(|e| QueryError::new(describe(&e)))?;

    let schema = reader.schema();
    let mut batches = Vec::new();
    let mut row_count = 0usize;
    for batch in reader.by_ref() {
        let batch = batch.map_err(|e| QueryError::new(e.to_string()))?;
        row_count += batch.num_rows();
        batches.push(batch);
    }

    Ok(QueryResult {
        schema,
        batches,
        elapsed: started.elapsed(),
        row_count,
    })
}

// ---------------------------------------------------------------------------
// plumbing
// ---------------------------------------------------------------------------

fn state(profile: &Profile, state: ConnectionState) -> DbEvent {
    DbEvent::State {
        profile_id: profile.id.clone(),
        state,
    }
}

/// Returns `false` once the event channel is closed — the app is gone and this
/// thread should wind down.
fn publish(events: &Sender<DbEvent>, event: DbEvent) -> bool {
    events.send(event).is_ok()
}

/// Human-readable rendering of an ADBC error. `Display` on `adbc_core`'s error
/// includes the raw sqlstate byte array, which is noise in a status bar.
fn describe(err: &AdbcError) -> String {
    let message = err.message.trim();
    if message.is_empty() {
        format!("{:?}", err.status)
    } else {
        format!("{message} ({:?})", err.status)
    }
}
