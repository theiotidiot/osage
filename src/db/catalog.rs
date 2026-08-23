//! Decoding of ADBC `GetObjects` record batches into `CatalogNode` values.
//!
//! `GetObjects` returns one row per catalog with everything below it nested
//! inside two levels of `list<struct<..>>`:
//!
//! ```text
//! catalog_name        : utf8
//! catalog_db_schemas  : list<struct{
//!     db_schema_name  : utf8,
//!     db_schema_tables: list<struct{
//!         table_name       : utf8,
//!         table_type       : utf8,
//!         table_columns    : list<struct{ column_name, xdbc_type_name, xdbc_nullable, .. }>,
//!         table_constraints: list<struct{ .. }>,
//!     }>,
//! }>
//! ```
//!
//! (see `adbc_core::schemas::GET_OBJECTS_SCHEMA` and friends). We only ever ask
//! the driver for the depth we need, and this module only ever hands back the
//! single level directly below `parent`.

use std::collections::HashSet;

use arrow::array::{
    Array, ArrayRef, Int16Array, Int32Array, LargeListArray, LargeStringArray, ListArray,
    StringArray, StringViewArray, StructArray,
};
use arrow::record_batch::RecordBatch;

use crate::types::{CatalogNode, CatalogPath, LoadState, NodeKind};

/// Display name used for a catalog whose name the driver reports as NULL or as
/// the empty string (SQLite and Flight SQL do this, among others).
///
/// The placeholder is what the UI shows *and* what ends up in a
/// [`CatalogPath::catalog`], so matching keeps working end to end: decoding
/// normalises a NULL/empty catalog name to this placeholder before comparing,
/// and [`server_catalog_filter`] maps it back to `""` — which is precisely the
/// ADBC spelling for "objects with no catalog" — before it is sent to the
/// driver as a `GetObjects` filter. The only thing this breaks is a real
/// catalog literally named `(default)`, which we accept.
pub const DEFAULT_CATALOG: &str = "(default)";

/// Normalise a driver-reported catalog name into the name we display and key on.
pub fn display_catalog(name: Option<&str>) -> String {
    match name {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => DEFAULT_CATALOG.to_string(),
    }
}

/// Map a catalog name from a [`CatalogPath`] back to the filter string the
/// driver expects. See [`DEFAULT_CATALOG`].
pub fn server_catalog_filter(name: &str) -> &str {
    if name == DEFAULT_CATALOG { "" } else { name }
}

/// Decode a `GetObjects` result into the children of the node at `parent`.
///
/// CONTRACT: returns exactly one level below `parent` (catalogs for an empty
/// path, schemas under a catalog, tables/views under a schema, columns under a
/// table). Never returns deeper levels.
pub fn decode_children(
    batches: &[RecordBatch],
    parent: &CatalogPath,
) -> Result<Vec<CatalogNode>, String> {
    match (
        parent.catalog.as_deref(),
        parent.schema.as_deref(),
        parent.table.as_deref(),
    ) {
        (None, _, _) => decode_catalogs(batches),
        (Some(catalog), None, _) => decode_schemas(batches, catalog),
        (Some(catalog), Some(schema), None) => decode_tables(batches, catalog, schema),
        (Some(catalog), Some(schema), Some(table)) => {
            decode_columns(batches, catalog, schema, table)
        }
    }
}

// ---------------------------------------------------------------------------
// levels
// ---------------------------------------------------------------------------

fn decode_catalogs(batches: &[RecordBatch]) -> Result<Vec<CatalogNode>, String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for batch in batches {
        let names = required_column(batch, "catalog_name")?;
        for row in 0..batch.num_rows() {
            let name = display_catalog(string_at(names.as_ref(), row, "catalog_name")?.as_deref());
            if seen.insert(name.clone()) {
                out.push(CatalogNode::new(NodeKind::Catalog, name));
            }
        }
    }
    Ok(out)
}

fn decode_schemas(batches: &[RecordBatch], catalog: &str) -> Result<Vec<CatalogNode>, String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for schemas in catalog_rows(batches, catalog)?.into_iter().flatten() {
        let names = required_field(schemas.as_ref(), "db_schema_name", "catalog_db_schemas")?;
        for i in 0..schemas.len() {
            if schemas.is_null(i) {
                continue;
            }
            let name = string_at(names.as_ref(), i, "db_schema_name")?.unwrap_or_default();
            if seen.insert(name.clone()) {
                out.push(CatalogNode::new(NodeKind::Schema, name));
            }
        }
    }
    Ok(out)
}

fn decode_tables(
    batches: &[RecordBatch],
    catalog: &str,
    schema: &str,
) -> Result<Vec<CatalogNode>, String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for tables in schema_rows(batches, catalog, schema)?.into_iter().flatten() {
        let names = required_field(tables.as_ref(), "table_name", "db_schema_tables")?;
        let types = optional_field(tables.as_ref(), "table_type", "db_schema_tables")?;
        for i in 0..tables.len() {
            if tables.is_null(i) {
                continue;
            }
            let name = string_at(names.as_ref(), i, "table_name")?.unwrap_or_default();
            let table_type = match &types {
                Some(col) => string_at(col.as_ref(), i, "table_type")?.unwrap_or_default(),
                None => String::new(),
            };
            // ADBC leaves the exact spelling to the driver: "VIEW",
            // "LOCAL TEMPORARY VIEW", "MATERIALIZED VIEW", ...
            let kind = if table_type.to_ascii_uppercase().contains("VIEW") {
                NodeKind::View
            } else {
                NodeKind::Table
            };
            if seen.insert(name.clone()) {
                out.push(CatalogNode::new(kind, name));
            }
        }
    }
    Ok(out)
}

fn decode_columns(
    batches: &[RecordBatch],
    catalog: &str,
    schema: &str,
    table: &str,
) -> Result<Vec<CatalogNode>, String> {
    let mut out = Vec::new();
    for columns in table_rows(batches, catalog, schema, table)?
        .into_iter()
        .flatten()
    {
        let names = required_field(columns.as_ref(), "column_name", "table_columns")?;
        let type_names = optional_field(columns.as_ref(), "xdbc_type_name", "table_columns")?;
        let type_codes = optional_field(columns.as_ref(), "xdbc_data_type", "table_columns")?;
        let nullables = optional_field(columns.as_ref(), "xdbc_nullable", "table_columns")?;
        for i in 0..columns.len() {
            if columns.is_null(i) {
                continue;
            }
            let name = string_at(names.as_ref(), i, "column_name")?.unwrap_or_default();

            let mut data_type = match &type_names {
                Some(col) => string_at(col.as_ref(), i, "xdbc_type_name")?.unwrap_or_default(),
                None => String::new(),
            };
            if data_type.is_empty()
                && let Some(col) = &type_codes
                && let Some(code) = i16_at(col.as_ref(), i, "xdbc_data_type")?
            {
                data_type = xdbc_type_label(code);
            }

            // ODBC semantics: 0 = NOT NULL, 1 = nullable, 2 = unknown. A NULL
            // (driver does not report it) is treated as nullable.
            let nullable = match &nullables {
                Some(col) => i16_at(col.as_ref(), i, "xdbc_nullable")?.is_none_or(|n| n != 0),
                None => true,
            };

            // Columns are leaves: nothing below them can ever be loaded.
            let mut node = CatalogNode::new(
                NodeKind::Column {
                    data_type,
                    nullable,
                },
                name,
            );
            node.load_state = LoadState::Loaded;
            out.push(node);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// row selection
// ---------------------------------------------------------------------------
//
// Each selector narrows one level and returns the *child list* of every entry
// that matched, so the next level can just iterate.
//
// If nothing matched by name we fall back to every entry at that level: the
// request was already scoped server-side via `GetObjects` filters, and some
// drivers normalise or ignore the filter (case folding, an empty catalog name,
// pattern semantics). Returning the server's own answer beats returning
// nothing. When a name *does* match we never fall back, so an empty-but-real
// level still decodes as empty.

/// `catalog_db_schemas` values for every catalog row matching `catalog`.
fn catalog_rows(batches: &[RecordBatch], catalog: &str) -> Result<Vec<Option<ArrayRef>>, String> {
    let mut matched = Vec::new();
    let mut all = Vec::new();
    for batch in batches {
        let names = required_column(batch, "catalog_name")?;
        let schemas = batch.column_by_name("catalog_db_schemas").cloned();
        for row in 0..batch.num_rows() {
            let name = display_catalog(string_at(names.as_ref(), row, "catalog_name")?.as_deref());
            let entry = match &schemas {
                Some(col) => list_value(col.as_ref(), row, "catalog_db_schemas")?,
                None => None,
            };
            if name == catalog {
                matched.push(entry.clone());
            }
            all.push(entry);
        }
    }
    Ok(if matched.is_empty() { all } else { matched })
}

/// `db_schema_tables` values for every schema entry matching `schema`.
fn schema_rows(
    batches: &[RecordBatch],
    catalog: &str,
    schema: &str,
) -> Result<Vec<Option<ArrayRef>>, String> {
    let mut matched = Vec::new();
    let mut all = Vec::new();
    for schemas in catalog_rows(batches, catalog)?.into_iter().flatten() {
        let names = required_field(schemas.as_ref(), "db_schema_name", "catalog_db_schemas")?;
        let tables = optional_field(schemas.as_ref(), "db_schema_tables", "catalog_db_schemas")?;
        for i in 0..schemas.len() {
            if schemas.is_null(i) {
                continue;
            }
            let name = string_at(names.as_ref(), i, "db_schema_name")?.unwrap_or_default();
            let entry = match &tables {
                Some(col) => list_value(col.as_ref(), i, "db_schema_tables")?,
                None => None,
            };
            if name == schema {
                matched.push(entry.clone());
            }
            all.push(entry);
        }
    }
    Ok(if matched.is_empty() { all } else { matched })
}

/// `table_columns` values for every table entry matching `table`.
fn table_rows(
    batches: &[RecordBatch],
    catalog: &str,
    schema: &str,
    table: &str,
) -> Result<Vec<Option<ArrayRef>>, String> {
    let mut matched = Vec::new();
    let mut all = Vec::new();
    for tables in schema_rows(batches, catalog, schema)?.into_iter().flatten() {
        let names = required_field(tables.as_ref(), "table_name", "db_schema_tables")?;
        let columns = optional_field(tables.as_ref(), "table_columns", "db_schema_tables")?;
        for i in 0..tables.len() {
            if tables.is_null(i) {
                continue;
            }
            let name = string_at(names.as_ref(), i, "table_name")?.unwrap_or_default();
            let entry = match &columns {
                Some(col) => list_value(col.as_ref(), i, "table_columns")?,
                None => None,
            };
            if name == table {
                matched.push(entry.clone());
            }
            all.push(entry);
        }
    }
    Ok(if matched.is_empty() { all } else { matched })
}

// ---------------------------------------------------------------------------
// arrow access helpers — every one of these reports a descriptive error rather
// than panicking, because drivers disagree about the fine print.
// ---------------------------------------------------------------------------

fn required_column(batch: &RecordBatch, name: &str) -> Result<ArrayRef, String> {
    batch.column_by_name(name).cloned().ok_or_else(|| {
        format!(
            "GetObjects result has no `{name}` column (found: {})",
            field_names(batch)
        )
    })
}

fn field_names(batch: &RecordBatch) -> String {
    batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn as_struct<'a>(array: &'a dyn Array, ctx: &str) -> Result<&'a StructArray, String> {
    array.as_any().downcast_ref::<StructArray>().ok_or_else(|| {
        format!(
            "GetObjects: expected `{ctx}` to hold structs, found {:?}",
            array.data_type()
        )
    })
}

/// Field of a struct array, or `None` when the driver omitted it entirely.
fn optional_field(array: &dyn Array, name: &str, ctx: &str) -> Result<Option<ArrayRef>, String> {
    Ok(as_struct(array, ctx)?.column_by_name(name).cloned())
}

fn required_field(array: &dyn Array, name: &str, ctx: &str) -> Result<ArrayRef, String> {
    let structs = as_struct(array, ctx)?;
    structs.column_by_name(name).cloned().ok_or_else(|| {
        format!(
            "GetObjects: `{ctx}` struct has no `{name}` field (found: {})",
            structs
                .fields()
                .iter()
                .map(|f| f.name().as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

fn string_at(array: &dyn Array, row: usize, ctx: &str) -> Result<Option<String>, String> {
    if row >= array.len() || array.is_null(row) {
        return Ok(None);
    }
    if let Some(a) = array.as_any().downcast_ref::<StringArray>() {
        return Ok(Some(a.value(row).to_string()));
    }
    if let Some(a) = array.as_any().downcast_ref::<LargeStringArray>() {
        return Ok(Some(a.value(row).to_string()));
    }
    if let Some(a) = array.as_any().downcast_ref::<StringViewArray>() {
        return Ok(Some(a.value(row).to_string()));
    }
    Err(format!(
        "GetObjects: expected `{ctx}` to be a string column, found {:?}",
        array.data_type()
    ))
}

fn i16_at(array: &dyn Array, row: usize, ctx: &str) -> Result<Option<i16>, String> {
    if row >= array.len() || array.is_null(row) {
        return Ok(None);
    }
    if let Some(a) = array.as_any().downcast_ref::<Int16Array>() {
        return Ok(Some(a.value(row)));
    }
    // Some drivers widen the xdbc_* metadata columns.
    if let Some(a) = array.as_any().downcast_ref::<Int32Array>() {
        return Ok(Some(a.value(row) as i16));
    }
    Err(format!(
        "GetObjects: expected `{ctx}` to be an int16 column, found {:?}",
        array.data_type()
    ))
}

/// Values of the list at `row`, or `None` when that entry is null.
fn list_value(array: &dyn Array, row: usize, ctx: &str) -> Result<Option<ArrayRef>, String> {
    if row >= array.len() || array.is_null(row) {
        return Ok(None);
    }
    if let Some(a) = array.as_any().downcast_ref::<ListArray>() {
        return Ok(Some(a.value(row)));
    }
    if let Some(a) = array.as_any().downcast_ref::<LargeListArray>() {
        return Ok(Some(a.value(row)));
    }
    Err(format!(
        "GetObjects: expected `{ctx}` to be a list column, found {:?}",
        array.data_type()
    ))
}

/// Last-resort rendering of `xdbc_data_type` when `xdbc_type_name` is absent.
/// Codes are the ODBC/JDBC SQL type codes ADBC borrows.
fn xdbc_type_label(code: i16) -> String {
    let name = match code {
        -7 => "BIT",
        -6 => "TINYINT",
        -5 => "BIGINT",
        -4 => "LONGVARBINARY",
        -3 => "VARBINARY",
        -2 => "BINARY",
        -1 => "LONGVARCHAR",
        0 => "NULL",
        1 => "CHAR",
        2 => "NUMERIC",
        3 => "DECIMAL",
        4 => "INTEGER",
        5 => "SMALLINT",
        6 => "FLOAT",
        7 => "REAL",
        8 => "DOUBLE",
        12 => "VARCHAR",
        16 => "BOOLEAN",
        91 => "DATE",
        92 => "TIME",
        93 => "TIMESTAMP",
        _ => return format!("sql_type({code})"),
    };
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use adbc_core::schemas::{
        COLUMN_SCHEMA, GET_OBJECTS_SCHEMA, OBJECTS_DB_SCHEMA_SCHEMA, TABLE_SCHEMA,
    };
    use arrow::array::new_null_array;
    use arrow::buffer::OffsetBuffer;
    use arrow::datatypes::{DataType, Field, Schema};

    /// Build a struct array of `len` rows matching `dt` (one of the adbc_core
    /// schema statics), filling every field we did not supply with nulls.
    fn structs(dt: &DataType, len: usize, provided: &[(&str, ArrayRef)]) -> ArrayRef {
        let DataType::Struct(fields) = dt else {
            panic!("not a struct: {dt:?}");
        };
        let arrays: Vec<ArrayRef> = fields
            .iter()
            .map(|f| {
                provided
                    .iter()
                    .find(|(name, _)| name == f.name())
                    .map(|(_, a)| a.clone())
                    .unwrap_or_else(|| new_null_array(f.data_type(), len))
            })
            .collect();
        Arc::new(StructArray::try_new(fields.clone(), arrays, None).unwrap())
    }

    fn list_of(values: ArrayRef, offsets: &[i32]) -> ArrayRef {
        let field = Arc::new(Field::new_list_field(values.data_type().clone(), true));
        Arc::new(ListArray::new(
            field,
            OffsetBuffer::new(offsets.to_vec().into()),
            values,
            None,
        ))
    }

    fn s(values: &[Option<&str>]) -> ArrayRef {
        Arc::new(StringArray::from(values.to_vec()))
    }

    /// Two catalogs:
    ///   "main"  -> schema "public" -> table "orders" (3 columns), view "order_view"
    ///           -> schema "other"  -> no tables
    ///   NULL    -> no schemas                (decodes as the `(default)` catalog)
    fn sample_batch() -> RecordBatch {
        // ---- columns of `orders` -------------------------------------------
        let columns = structs(
            &COLUMN_SCHEMA,
            3,
            &[
                ("column_name", s(&[Some("id"), Some("name"), Some("extra")])),
                (
                    "xdbc_type_name",
                    s(&[Some("INTEGER"), Some("VARCHAR"), None]),
                ),
                (
                    "xdbc_nullable",
                    Arc::new(Int16Array::from(vec![Some(0), Some(1), None])),
                ),
            ],
        );
        // orders owns all three columns; order_view reports none.
        let table_columns = list_of(columns, &[0, 3, 3]);

        // ---- tables of `public` --------------------------------------------
        let tables = structs(
            &TABLE_SCHEMA,
            2,
            &[
                ("table_name", s(&[Some("orders"), Some("order_view")])),
                ("table_type", s(&[Some("TABLE"), Some("VIEW")])),
                ("table_columns", table_columns),
            ],
        );
        // "public" owns both tables; "other" owns none.
        let db_schema_tables = list_of(tables, &[0, 2, 2]);

        // ---- schemas of `main` ---------------------------------------------
        let schemas = structs(
            &OBJECTS_DB_SCHEMA_SCHEMA,
            2,
            &[
                ("db_schema_name", s(&[Some("public"), Some("other")])),
                ("db_schema_tables", db_schema_tables),
            ],
        );
        // "main" owns both schemas; the NULL-named catalog owns none.
        let catalog_db_schemas = list_of(schemas, &[0, 2, 2]);

        RecordBatch::try_new(
            GET_OBJECTS_SCHEMA.clone(),
            vec![s(&[Some("main"), None]), catalog_db_schemas],
        )
        .unwrap()
    }

    fn path(catalog: Option<&str>, schema: Option<&str>, table: Option<&str>) -> CatalogPath {
        CatalogPath {
            catalog: catalog.map(String::from),
            schema: schema.map(String::from),
            table: table.map(String::from),
        }
    }

    #[test]
    fn decodes_catalogs_at_the_root() {
        let batch = sample_batch();
        let nodes = decode_children(&[batch], &CatalogPath::default()).unwrap();

        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].name, "main");
        assert_eq!(nodes[0].kind, NodeKind::Catalog);
        assert_eq!(nodes[0].load_state, LoadState::NotLoaded);
        assert!(!nodes[0].expanded);
        assert!(nodes[0].children.is_empty());
        // NULL catalog name surfaces under the documented placeholder.
        assert_eq!(nodes[1].name, DEFAULT_CATALOG);
        assert_eq!(nodes[1].kind, NodeKind::Catalog);
    }

    #[test]
    fn decodes_schemas_under_a_catalog() {
        let batch = sample_batch();
        let nodes = decode_children(&[batch], &path(Some("main"), None, None)).unwrap();

        let names: Vec<_> = nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["public", "other"]);
        assert!(nodes.iter().all(|n| n.kind == NodeKind::Schema));
        assert!(nodes.iter().all(|n| n.load_state == LoadState::NotLoaded));
    }

    #[test]
    fn placeholder_catalog_matches_the_null_named_row() {
        let batch = sample_batch();
        // The `(default)` catalog genuinely has no schemas — and because it
        // matched by name we must not fall back to `main`'s schemas.
        let nodes = decode_children(&[batch], &path(Some(DEFAULT_CATALOG), None, None)).unwrap();
        assert!(nodes.is_empty(), "got {nodes:?}");
    }

    #[test]
    fn decodes_tables_and_views_under_a_schema() {
        let batch = sample_batch();
        let nodes = decode_children(&[batch], &path(Some("main"), Some("public"), None)).unwrap();

        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].name, "orders");
        assert_eq!(nodes[0].kind, NodeKind::Table);
        assert_eq!(nodes[1].name, "order_view");
        assert_eq!(nodes[1].kind, NodeKind::View);
        assert!(nodes.iter().all(|n| n.load_state == LoadState::NotLoaded));
    }

    #[test]
    fn empty_schema_yields_no_tables() {
        let batch = sample_batch();
        let nodes = decode_children(&[batch], &path(Some("main"), Some("other"), None)).unwrap();
        assert!(nodes.is_empty(), "got {nodes:?}");
    }

    #[test]
    fn decodes_columns_under_a_table() {
        let batch = sample_batch();
        let nodes = decode_children(
            &[batch],
            &path(Some("main"), Some("public"), Some("orders")),
        )
        .unwrap();

        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[0].name, "id");
        assert_eq!(
            nodes[0].kind,
            NodeKind::Column {
                data_type: "INTEGER".into(),
                nullable: false, // xdbc_nullable = 0 → NOT NULL
            }
        );
        assert_eq!(
            nodes[1].kind,
            NodeKind::Column {
                data_type: "VARCHAR".into(),
                nullable: true, // xdbc_nullable = 1 → nullable
            }
        );
        assert_eq!(
            nodes[2].kind,
            NodeKind::Column {
                data_type: String::new(), // no type name, no type code
                nullable: true,           // xdbc_nullable = NULL → nullable
            }
        );
        // Columns are leaves.
        assert!(nodes.iter().all(|n| !n.expandable()));
        assert!(nodes.iter().all(|n| n.children.is_empty()));
    }

    #[test]
    fn view_detection_is_case_insensitive_and_substring_based() {
        let tables = structs(
            &TABLE_SCHEMA,
            3,
            &[
                ("table_name", s(&[Some("a"), Some("b"), Some("c")])),
                (
                    "table_type",
                    s(&[
                        Some("materialized view"),
                        Some("LOCAL TEMPORARY"),
                        Some("BASE TABLE"),
                    ]),
                ),
            ],
        );
        let schemas = structs(
            &OBJECTS_DB_SCHEMA_SCHEMA,
            1,
            &[
                ("db_schema_name", s(&[Some("public")])),
                ("db_schema_tables", list_of(tables, &[0, 3])),
            ],
        );
        let batch = RecordBatch::try_new(
            GET_OBJECTS_SCHEMA.clone(),
            vec![s(&[Some("main")]), list_of(schemas, &[0, 1])],
        )
        .unwrap();

        let nodes = decode_children(&[batch], &path(Some("main"), Some("public"), None)).unwrap();
        let kinds: Vec<_> = nodes.iter().map(|n| n.kind.clone()).collect();
        assert_eq!(kinds, [NodeKind::View, NodeKind::Table, NodeKind::Table]);
    }

    #[test]
    fn falls_back_to_the_xdbc_type_code_when_the_name_is_missing() {
        let columns = structs(
            &COLUMN_SCHEMA,
            2,
            &[
                ("column_name", s(&[Some("a"), Some("b")])),
                (
                    "xdbc_data_type",
                    Arc::new(Int16Array::from(vec![Some(4), Some(999)])),
                ),
            ],
        );
        let tables = structs(
            &TABLE_SCHEMA,
            1,
            &[
                ("table_name", s(&[Some("t")])),
                ("table_type", s(&[Some("TABLE")])),
                ("table_columns", list_of(columns, &[0, 2])),
            ],
        );
        let schemas = structs(
            &OBJECTS_DB_SCHEMA_SCHEMA,
            1,
            &[
                ("db_schema_name", s(&[Some("public")])),
                ("db_schema_tables", list_of(tables, &[0, 1])),
            ],
        );
        let batch = RecordBatch::try_new(
            GET_OBJECTS_SCHEMA.clone(),
            vec![s(&[Some("main")]), list_of(schemas, &[0, 1])],
        )
        .unwrap();

        let nodes =
            decode_children(&[batch], &path(Some("main"), Some("public"), Some("t"))).unwrap();
        assert_eq!(
            nodes[0].kind,
            NodeKind::Column {
                data_type: "INTEGER".into(),
                nullable: true
            }
        );
        assert_eq!(
            nodes[1].kind,
            NodeKind::Column {
                data_type: "sql_type(999)".into(),
                nullable: true
            }
        );
    }

    #[test]
    fn schema_mismatch_is_an_error_not_a_panic() {
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new(
                "catalog_name",
                DataType::Int32,
                true,
            )])),
            vec![Arc::new(Int32Array::from(vec![Some(1)])) as ArrayRef],
        )
        .unwrap();
        let err = decode_children(&[batch], &CatalogPath::default()).unwrap_err();
        assert!(err.contains("catalog_name"), "{err}");
    }

    #[test]
    fn missing_column_is_an_error_not_a_panic() {
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("nope", DataType::Utf8, true)])),
            vec![s(&[Some("x")])],
        )
        .unwrap();
        let err = decode_children(&[batch], &CatalogPath::default()).unwrap_err();
        assert!(err.contains("no `catalog_name` column"), "{err}");
    }

    #[test]
    fn server_filter_round_trips_the_default_placeholder() {
        assert_eq!(server_catalog_filter(DEFAULT_CATALOG), "");
        assert_eq!(server_catalog_filter("main"), "main");
        assert_eq!(display_catalog(None), DEFAULT_CATALOG);
        assert_eq!(display_catalog(Some("")), DEFAULT_CATALOG);
        assert_eq!(display_catalog(Some("main")), "main");
    }
}
