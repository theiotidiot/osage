//! Milestone M0: a headless path through the real database layer.
//!
//! `osage --probe <driver> <uri> [sql]` connects, walks the catalog with the
//! same lazy `GetObjects` calls the TUI makes, prints it as JSON, then runs a
//! query and prints the rows. It deliberately drives `ConnectionManager` and
//! `db::worker` rather than a parallel implementation, so a green probe means
//! the integration the UI depends on actually works.

use std::collections::HashMap;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use color_eyre::eyre::{bail, eyre};

use crate::db::{ConnectionManager, DbEvent, DbRequest};
use crate::types::{CatalogNode, CatalogPath, ConnectionState, NodeKind, Profile};

const TIMEOUT: Duration = Duration::from_secs(30);
/// Cap how many tables we descend into for columns, so a wide database does
/// not produce unreadable output.
const MAX_TABLES_WITH_COLUMNS: usize = 5;

pub fn run(driver: &str, uri: &str, sql: Option<&str>) -> color_eyre::Result<()> {
    let profile = Profile {
        id: "probe".into(),
        name: "probe".into(),
        driver: driver.to_string(),
        uri: uri.to_string(),
        username: std::env::var("OSAGE_PROBE_USER").ok(),
        secret_ref: None,
        options: HashMap::new(),
        color: None,
    };
    let secret = std::env::var("OSAGE_PROBE_PASSWORD").ok();

    let (mut manager, events) = ConnectionManager::new();
    manager.ensure_thread(&profile, secret);
    manager.send(&profile.id, DbRequest::Connect);

    // 1. Connect.
    loop {
        match events.recv_timeout(TIMEOUT) {
            Ok(DbEvent::State { state, .. }) => match state {
                ConnectionState::Connected => {
                    eprintln!("connected: {driver} {uri}");
                    break;
                }
                ConnectionState::Errored(e) => bail!("connect failed: {e}"),
                _ => continue,
            },
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => bail!("timed out connecting"),
            Err(RecvTimeoutError::Disconnected) => bail!("worker thread died while connecting"),
        }
    }

    // 2. Walk the catalog one level at a time, exactly like the tree does.
    let mut root = CatalogNode::new(NodeKind::Catalog, "probe");
    root.children = fetch(&manager, &events, CatalogPath::default())?;

    let mut descended = 0usize;
    for catalog in root.children.iter_mut() {
        let catalog_path = CatalogPath {
            catalog: Some(catalog.name.clone()),
            ..Default::default()
        };
        catalog.children = fetch(&manager, &events, catalog_path.clone())?;
        for schema in catalog.children.iter_mut() {
            let schema_path = CatalogPath {
                schema: Some(schema.name.clone()),
                ..catalog_path.clone()
            };
            schema.children = fetch(&manager, &events, schema_path.clone())?;
            for table in schema.children.iter_mut() {
                if descended >= MAX_TABLES_WITH_COLUMNS {
                    break;
                }
                let table_path = CatalogPath {
                    table: Some(table.name.clone()),
                    ..schema_path.clone()
                };
                table.children = fetch(&manager, &events, table_path)?;
                descended += 1;
            }
        }
    }

    println!("{}", serde_json::to_string_pretty(&node_to_json(&root))?);

    // 3. Run a query.
    if let Some(sql) = sql {
        manager.send(
            &profile.id,
            DbRequest::Execute {
                tab_id: 0,
                sql: sql.to_string(),
            },
        );
        loop {
            match events.recv_timeout(TIMEOUT) {
                Ok(DbEvent::QueryDone { result, .. }) => match *result {
                    Ok(result) => {
                        eprintln!(
                            "{} rows · {}ms",
                            result.row_count,
                            result.elapsed.as_millis()
                        );
                        if !result.batches.is_empty() {
                            arrow::util::pretty::print_batches(&result.batches)?;
                        }
                        break;
                    }
                    Err(e) => bail!("query failed: {}", e.message),
                },
                Ok(_) => continue,
                Err(e) => bail!("query wait failed: {e}"),
            }
        }
    }

    manager.shutdown_all();
    Ok(())
}

/// Request one level of the catalog and block for its reply.
fn fetch(
    manager: &ConnectionManager,
    events: &std::sync::mpsc::Receiver<DbEvent>,
    path: CatalogPath,
) -> color_eyre::Result<Vec<CatalogNode>> {
    manager.send("probe", DbRequest::LoadChildren { path: path.clone() });
    loop {
        match events.recv_timeout(TIMEOUT) {
            Ok(DbEvent::CatalogChildren { result, .. }) => {
                return result.map_err(|e| eyre!("catalog {}: {e}", path.qualified()));
            }
            Ok(_) => continue,
            Err(e) => bail!("catalog wait failed for {}: {e}", path.qualified()),
        }
    }
}

fn node_to_json(node: &CatalogNode) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("name".into(), node.name.clone().into());
    let kind = match &node.kind {
        NodeKind::Catalog => "catalog".to_string(),
        NodeKind::Schema => "schema".to_string(),
        NodeKind::Table => "table".to_string(),
        NodeKind::View => "view".to_string(),
        NodeKind::Column {
            data_type,
            nullable,
        } => {
            map.insert("data_type".into(), data_type.clone().into());
            map.insert("nullable".into(), (*nullable).into());
            "column".to_string()
        }
    };
    map.insert("kind".into(), kind.into());
    if !node.children.is_empty() {
        map.insert(
            "children".into(),
            node.children.iter().map(node_to_json).collect(),
        );
    }
    serde_json::Value::Object(map)
}

#[cfg(test)]
mod tests {
    //! End-to-end tests against real ADBC drivers (milestone M0's exit
    //! criteria). They skip themselves when the driver or fixture is absent so
    //! a bare checkout still runs `cargo test` green.

    use super::*;
    use std::sync::mpsc::Receiver;

    const DUCKDB_FIXTURE: &str = "/tmp/osage-test.duckdb";

    fn driver_available(name: &str) -> bool {
        let Some(home) = std::env::var_os("HOME") else {
            return false;
        };
        std::path::Path::new(&home)
            .join("Library/Application Support/ADBC/Drivers")
            .join(format!("{name}.toml"))
            .exists()
    }

    /// Connect and return the live manager plus its event stream.
    fn connect(driver: &str, uri: &str) -> Option<(ConnectionManager, Receiver<DbEvent>)> {
        let profile = Profile {
            id: "probe".into(),
            name: "probe".into(),
            driver: driver.into(),
            uri: uri.into(),
            username: None,
            secret_ref: None,
            options: HashMap::new(),
            color: None,
        };
        let (mut manager, events) = ConnectionManager::new();
        manager.ensure_thread(&profile, None);
        manager.send("probe", DbRequest::Connect);
        loop {
            match events.recv_timeout(TIMEOUT) {
                Ok(DbEvent::State { state, .. }) => match state {
                    ConnectionState::Connected => return Some((manager, events)),
                    ConnectionState::Errored(e) => {
                        eprintln!("skipping: connect failed: {e}");
                        return None;
                    }
                    _ => continue,
                },
                Ok(_) => continue,
                Err(e) => {
                    eprintln!("skipping: {e}");
                    return None;
                }
            }
        }
    }

    fn query(
        manager: &ConnectionManager,
        events: &Receiver<DbEvent>,
        sql: &str,
    ) -> crate::types::QueryResult {
        manager.send(
            "probe",
            DbRequest::Execute {
                tab_id: 0,
                sql: sql.into(),
            },
        );
        loop {
            match events.recv_timeout(TIMEOUT).expect("query reply") {
                DbEvent::QueryDone { result, .. } => match *result {
                    Ok(r) => return r,
                    Err(e) => panic!("query failed: {}", e.message),
                },
                _ => continue,
            }
        }
    }

    /// Walk down to the schema that holds the fixture tables.
    ///
    /// A driver can expose the same schema name in several catalogs — DuckDB
    /// has `main` under `system`, `temp` and the attached database — so keep
    /// looking until one actually contains tables, and report which catalog
    /// won so the column fetch can be scoped to it.
    fn tables_of(
        manager: &ConnectionManager,
        events: &Receiver<DbEvent>,
        wanted_schema: &str,
    ) -> (CatalogPath, Vec<CatalogNode>) {
        let catalogs = fetch(manager, events, CatalogPath::default()).expect("catalogs");
        assert!(!catalogs.is_empty(), "driver reported no catalogs");
        let mut seen = Vec::new();
        for catalog in &catalogs {
            assert_eq!(catalog.kind, NodeKind::Catalog);
            let catalog_path = CatalogPath {
                catalog: Some(catalog.name.clone()),
                ..Default::default()
            };
            let schemas = fetch(manager, events, catalog_path.clone()).expect("schemas");
            for schema in &schemas {
                seen.push(format!("{}.{}", catalog.name, schema.name));
                if schema.name != wanted_schema {
                    continue;
                }
                assert_eq!(schema.kind, NodeKind::Schema);
                let schema_path = CatalogPath {
                    schema: Some(schema.name.clone()),
                    ..catalog_path.clone()
                };
                let tables = fetch(manager, events, schema_path.clone()).expect("tables");
                if !tables.is_empty() {
                    return (schema_path, tables);
                }
            }
        }
        panic!("no non-empty schema named {wanted_schema}; saw {seen:?}");
    }

    /// `nullability` is only asserted for drivers that actually populate
    /// ADBC's optional `xdbc_nullable` field. The PostgreSQL driver leaves it
    /// NULL for every column — even a primary key — and decoding NULL as
    /// "nullable" is the correct conservative reading. Papering over that would
    /// mean per-database special-casing, which the spec rules out.
    fn assert_fixture_catalog(
        manager: &ConnectionManager,
        events: &Receiver<DbEvent>,
        schema: &str,
        nullability: bool,
    ) {
        let (schema_path, tables) = tables_of(manager, events, schema);
        let named = |n: &str| tables.iter().find(|t| t.name == n).cloned();

        let customers = named("customers")
            .unwrap_or_else(|| panic!("customers not among {:?}", table_names(&tables)));
        assert_eq!(customers.kind, NodeKind::Table);
        let orders = named("orders")
            .unwrap_or_else(|| panic!("orders not among {:?}", table_names(&tables)));
        assert_eq!(orders.kind, NodeKind::Table);
        // Views must come back as views, not tables.
        let view = named("big_orders")
            .unwrap_or_else(|| panic!("big_orders not among {:?}", table_names(&tables)));
        assert_eq!(
            view.kind,
            NodeKind::View,
            "view was not classified as a view"
        );

        // Columns are only fetched when the table node is expanded, and must
        // stay scoped to the catalog the table was found in.
        let columns = fetch(
            manager,
            events,
            CatalogPath {
                table: Some("customers".into()),
                ..schema_path
            },
        )
        .expect("columns");
        let names = table_names(&columns);
        for wanted in ["id", "name", "email"] {
            assert!(names.iter().any(|n| n == wanted), "columns were {names:?}");
        }

        let column = |n: &str| columns.iter().find(|c| c.name == n).unwrap().kind.clone();
        for name in ["id", "name", "email"] {
            match column(name) {
                NodeKind::Column { data_type, .. } => {
                    assert!(!data_type.is_empty(), "`{name}` has no data type");
                }
                other => panic!("expected a column, got {other:?}"),
            }
        }
        if !nullability {
            return;
        }
        match column("name") {
            NodeKind::Column { nullable, .. } => {
                assert!(!nullable, "`name` is NOT NULL but was reported nullable")
            }
            other => panic!("expected a column, got {other:?}"),
        }
        match column("email") {
            NodeKind::Column { nullable, .. } => {
                assert!(nullable, "`email` is nullable but was reported NOT NULL")
            }
            other => panic!("expected a column, got {other:?}"),
        }
    }

    fn table_names(nodes: &[CatalogNode]) -> Vec<String> {
        nodes.iter().map(|n| n.name.clone()).collect()
    }

    #[test]
    fn duckdb_end_to_end() {
        if !driver_available("duckdb") || !std::path::Path::new(DUCKDB_FIXTURE).exists() {
            eprintln!("skipping: duckdb driver or fixture missing");
            return;
        }
        let Some((manager, events)) = connect("duckdb", DUCKDB_FIXTURE) else {
            return;
        };
        assert_fixture_catalog(&manager, &events, "main", true);

        let result = query(
            &manager,
            &events,
            "SELECT id, name FROM customers ORDER BY id",
        );
        assert_eq!(result.row_count, 3);
        assert_eq!(result.schema.fields().len(), 2);
        assert_eq!(result.schema.field(1).name(), "name");
    }

    #[test]
    fn postgres_end_to_end() {
        if !driver_available("postgresql") {
            eprintln!("skipping: postgresql driver missing");
            return;
        }
        let uri = std::env::var("OSAGE_TEST_PG")
            .unwrap_or_else(|_| "postgresql://osage@127.0.0.1:55432/osage_test".to_string());
        let Some((manager, events)) = connect("postgresql", &uri) else {
            return;
        };
        // The postgres driver does not populate `xdbc_nullable`.
        assert_fixture_catalog(&manager, &events, "public", false);

        let result = query(
            &manager,
            &events,
            "SELECT id, name FROM customers ORDER BY id",
        );
        assert_eq!(result.row_count, 3);
        assert_eq!(result.schema.fields().len(), 2);
    }

    #[test]
    fn query_error_is_reported_not_panicked() {
        if !driver_available("duckdb") || !std::path::Path::new(DUCKDB_FIXTURE).exists() {
            return;
        }
        let Some((manager, events)) = connect("duckdb", DUCKDB_FIXTURE) else {
            return;
        };
        manager.send(
            "probe",
            DbRequest::Execute {
                tab_id: 0,
                sql: "SELECT * FROM does_not_exist".into(),
            },
        );
        loop {
            match events.recv_timeout(TIMEOUT).expect("reply") {
                DbEvent::QueryDone { result, .. } => {
                    assert!(result.is_err(), "bad SQL should produce an error");
                    break;
                }
                _ => continue,
            }
        }
    }
}
