//! Result-set export to CSV, JSON and Arrow IPC.
//!
//! Every export is written to a temporary sibling file first and only then
//! renamed onto `path`, so an export that fails halfway (unsupported column
//! type, disk full, ...) can never leave a truncated file where the user asked
//! for their data.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::types::{ExportFormat, QueryResult};

/// Disambiguates concurrent exports that target the same directory.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `result` to `path` in `format`. Returns the number of rows written.
///
/// A leading `~/` in `path` is expanded to the user's home directory, missing
/// parent directories are created, and the write goes to a temporary sibling
/// that is renamed into place only on success. An empty result set is legal and
/// produces a valid (header-only / empty) file plus a row count of `0`.
pub fn export(result: &QueryResult, format: ExportFormat, path: &Path) -> Result<usize, String> {
    let target = expand_tilde(path);

    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| format!("could not create directory {}: {e}", parent.display()))?;
    }

    let temp = temp_sibling(&target);
    let rows = match write_temp(result, format, &temp) {
        Ok(rows) => rows,
        Err(e) => {
            let _ = fs::remove_file(&temp);
            return Err(e);
        }
    };

    fs::rename(&temp, &target).map_err(|e| {
        let _ = fs::remove_file(&temp);
        format!("could not write {}: {e}", target.display())
    })?;

    Ok(rows)
}

/// Default filename suggested in the export dialog.
///
/// Deliberately a *bare* filename with no directory and no timestamp: the app
/// regenerates it on every format change, so a stable name keeps the dialog
/// from thrashing under the user's cursor, and a relative name lands the export
/// in the process's current working directory — which is where someone who just
/// hits Enter expects to find it.
pub fn default_filename(format: ExportFormat) -> String {
    format!("osage-export.{}", format.extension())
}

/// Serialize into `temp`, flushing everything down to the file before returning.
fn write_temp(result: &QueryResult, format: ExportFormat, temp: &Path) -> Result<usize, String> {
    let file =
        File::create(temp).map_err(|e| format!("could not create {}: {e}", temp.display()))?;
    let mut writer = BufWriter::new(file);

    let rows = match format {
        ExportFormat::Csv => write_csv(result, &mut writer, temp)?,
        ExportFormat::Json => write_json(result, &mut writer, temp)?,
        ExportFormat::ArrowIpc => write_ipc(result, &mut writer, temp)?,
    };

    writer
        .flush()
        .map_err(|e| format!("could not write {}: {e}", temp.display()))?;
    writer
        .into_inner()
        .map_err(|e| format!("could not write {}: {e}", temp.display()))?
        .sync_all()
        .map_err(|e| format!("could not flush {}: {e}", temp.display()))?;

    Ok(rows)
}

/// CSV with a single header row, followed by every batch's rows. Nulls render
/// as empty fields (arrow's default).
fn write_csv(
    result: &QueryResult,
    sink: &mut BufWriter<File>,
    temp: &Path,
) -> Result<usize, String> {
    let mut writer = arrow::csv::WriterBuilder::new()
        .with_header(true)
        .build(sink);

    if result.batches.is_empty() {
        // Still emit the header line so the file describes the (empty) shape.
        let empty = arrow::record_batch::RecordBatch::new_empty(result.schema.clone());
        writer
            .write(&empty)
            .map_err(|e| format!("could not write CSV to {}: {e}", temp.display()))?;
        return Ok(0);
    }

    let mut rows = 0usize;
    for batch in &result.batches {
        writer
            .write(batch)
            .map_err(|e| format!("could not write CSV to {}: {e}", temp.display()))?;
        rows += batch.num_rows();
    }
    Ok(rows)
}

/// JSON as an *array of objects* (`arrow::json::ArrayWriter`) rather than
/// newline-delimited JSON: the overwhelmingly common thing to do with an
/// exported result set is paste it into a tool — `jq`, a REST client, a
/// scratch script — and those all want one parseable document. An empty result
/// therefore still yields a valid `[]`.
fn write_json(
    result: &QueryResult,
    sink: &mut BufWriter<File>,
    temp: &Path,
) -> Result<usize, String> {
    let mut writer = arrow::json::ArrayWriter::new(sink);

    let mut rows = 0usize;
    for batch in &result.batches {
        writer
            .write(batch)
            .map_err(|e| format!("could not write JSON to {}: {e}", temp.display()))?;
        rows += batch.num_rows();
    }

    writer
        .finish()
        .map_err(|e| format!("could not finish JSON in {}: {e}", temp.display()))?;
    Ok(rows)
}

/// Arrow IPC file format: schema up front, one message per batch, then the
/// footer written by `finish()` (without which the file is unreadable).
fn write_ipc(
    result: &QueryResult,
    sink: &mut BufWriter<File>,
    temp: &Path,
) -> Result<usize, String> {
    let mut writer = arrow::ipc::writer::FileWriter::try_new(sink, result.schema.as_ref())
        .map_err(|e| format!("could not write Arrow IPC to {}: {e}", temp.display()))?;

    let mut rows = 0usize;
    for batch in &result.batches {
        writer
            .write(batch)
            .map_err(|e| format!("could not write Arrow IPC to {}: {e}", temp.display()))?;
        rows += batch.num_rows();
    }

    writer
        .finish()
        .map_err(|e| format!("could not finish Arrow IPC in {}: {e}", temp.display()))?;
    Ok(rows)
}

/// Expand a leading `~` / `~/` to the home directory. Anything else (including
/// `~user`, which we cannot resolve) is returned untouched.
fn expand_tilde(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    let rest = if text == "~" {
        ""
    } else if let Some(rest) = text.strip_prefix("~/") {
        rest
    } else {
        return path.to_path_buf();
    };

    match dirs::home_dir() {
        Some(home) if rest.is_empty() => home,
        Some(home) => home.join(rest),
        None => path.to_path_buf(),
    }
}

/// A hidden sibling of `target` in the same directory, so the final `rename` is
/// an atomic same-filesystem operation.
fn temp_sibling(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export".to_string());
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_name = format!(".{name}.osage-{}-{n}.tmp", std::process::id());
    match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(temp_name),
        _ => PathBuf::from(temp_name),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use arrow::array::{Int64Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;

    use super::*;
    use crate::types::QueryResult;

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Unique scratch directory; no temp-dir crate is available here, so
    /// uniqueness comes from pid + a process-local counter.
    fn scratch(tag: &str) -> PathBuf {
        let n = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("osage-export-{}-{n}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("name", DataType::Utf8, false),
            Field::new("id", DataType::Int64, false),
            // Nullable, and actually null in the data below.
            Field::new("score", DataType::Int64, true),
        ]))
    }

    fn batch(names: &[&str], ids: &[i64], scores: &[Option<i64>]) -> RecordBatch {
        RecordBatch::try_new(
            schema(),
            vec![
                Arc::new(StringArray::from(names.to_vec())),
                Arc::new(Int64Array::from(ids.to_vec())),
                Arc::new(Int64Array::from(scores.to_vec())),
            ],
        )
        .expect("batch")
    }

    /// Two batches, five rows, one null in `score` (row "beta").
    fn fixture() -> QueryResult {
        let batches = vec![
            batch(
                &["alpha", "beta", "gamma"],
                &[1, 2, 3],
                &[Some(10), None, Some(30)],
            ),
            batch(&["delta", "epsilon"], &[4, 5], &[Some(40), Some(50)]),
        ];
        QueryResult {
            schema: schema(),
            batches,
            elapsed: Duration::from_millis(7),
            row_count: 5,
        }
    }

    fn empty() -> QueryResult {
        QueryResult {
            schema: schema(),
            batches: Vec::new(),
            elapsed: Duration::from_millis(1),
            row_count: 0,
        }
    }

    #[test]
    fn csv_has_one_header_and_all_rows_with_empty_null() {
        let dir = scratch("csv");
        let path = dir.join("out.csv");

        let rows = export(&fixture(), ExportFormat::Csv, &path).expect("export csv");
        assert_eq!(rows, 5, "returned row count");

        let text = fs::read_to_string(&path).expect("read csv");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 6, "one header + five data lines: {lines:?}");
        assert_eq!(lines[0], "name,id,score");
        // Header appears exactly once, not once per batch.
        assert_eq!(lines.iter().filter(|l| **l == "name,id,score").count(), 1);

        let beta: Vec<&str> = lines[2].split(',').collect();
        assert_eq!(
            beta,
            vec!["beta", "2", ""],
            "null renders as an empty field"
        );
        assert_eq!(lines[5], "epsilon,5,50", "second batch made it in");

        cleanup(&dir);
    }

    #[test]
    fn json_is_an_array_of_objects() {
        let dir = scratch("json");
        let path = dir.join("out.json");

        let rows = export(&fixture(), ExportFormat::Json, &path).expect("export json");
        assert_eq!(rows, 5, "returned row count");

        let text = fs::read_to_string(&path).expect("read json");
        let value: serde_json::Value = serde_json::from_str(&text).expect("parse json");
        let array = value.as_array().expect("top level array");
        assert_eq!(array.len(), 5);

        assert_eq!(array[0]["name"], serde_json::json!("alpha"));
        assert_eq!(array[0]["id"], serde_json::json!(1));
        assert_eq!(array[0]["score"], serde_json::json!(10));

        // The null column is either omitted or explicitly null — never a value.
        let beta = array[1].as_object().expect("row object");
        assert!(
            beta.get("score").map(|v| v.is_null()).unwrap_or(true),
            "null score must be absent or null, got {beta:?}"
        );
        assert_eq!(array[4]["name"], serde_json::json!("epsilon"));

        cleanup(&dir);
    }

    #[test]
    fn arrow_ipc_round_trips() {
        let dir = scratch("ipc");
        let path = dir.join("out.arrow");
        let result = fixture();

        let rows = export(&result, ExportFormat::ArrowIpc, &path).expect("export ipc");
        assert_eq!(rows, 5, "returned row count");

        let file = File::open(&path).expect("open ipc");
        let reader = arrow::ipc::reader::FileReader::try_new(file, None).expect("ipc reader");
        assert_eq!(
            reader.schema().fields(),
            result.schema.fields(),
            "schema round-trips identically"
        );

        let read_back: Vec<RecordBatch> = reader.collect::<Result<_, _>>().expect("read batches");
        let read_rows: usize = read_back.iter().map(|b| b.num_rows()).sum();
        assert_eq!(read_rows, 5);
        assert_eq!(read_back.iter().map(|b| b.num_columns()).max(), Some(3));

        cleanup(&dir);
    }

    #[test]
    fn creates_missing_nested_directories() {
        let dir = scratch("nested");
        let path = dir.join("a").join("b").join("c").join("out.csv");
        assert!(!path.parent().unwrap().exists());

        let rows = export(&fixture(), ExportFormat::Csv, &path).expect("export into new dirs");
        assert_eq!(rows, 5);
        assert!(path.exists(), "file landed at {}", path.display());

        cleanup(&dir);
    }

    #[test]
    fn empty_result_writes_valid_files_and_returns_zero() {
        let dir = scratch("empty");
        let result = empty();

        for format in ExportFormat::ALL {
            let path = dir.join(format!("empty.{}", format.extension()));
            let rows = export(&result, format, &path)
                .unwrap_or_else(|e| panic!("export {}: {e}", format.label()));
            assert_eq!(rows, 0, "{} row count", format.label());
            assert!(path.exists(), "{} file exists", format.label());
        }

        // CSV keeps the header so the shape survives.
        let csv = fs::read_to_string(dir.join("empty.csv")).expect("read csv");
        assert_eq!(csv.lines().collect::<Vec<_>>(), vec!["name,id,score"]);

        // JSON is still a parseable, empty array.
        let json = fs::read_to_string(dir.join("empty.json")).expect("read json");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse json");
        assert_eq!(value.as_array().map(|a| a.len()), Some(0));

        // IPC is still a readable file with the right schema.
        let file = File::open(dir.join("empty.arrow")).expect("open ipc");
        let reader = arrow::ipc::reader::FileReader::try_new(file, None).expect("ipc reader");
        assert_eq!(reader.schema().fields(), result.schema.fields());
        let batches: Vec<RecordBatch> = reader.collect::<Result<_, _>>().expect("read batches");
        assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 0);

        cleanup(&dir);
    }

    #[test]
    fn row_count_agrees_across_formats() {
        let dir = scratch("counts");
        let result = fixture();
        for format in ExportFormat::ALL {
            let path = dir.join(format!("all.{}", format.extension()));
            let rows = export(&result, format, &path)
                .unwrap_or_else(|e| panic!("export {}: {e}", format.label()));
            assert_eq!(rows, result.row_count, "{} row count", format.label());
        }
        cleanup(&dir);
    }

    #[test]
    fn leaves_no_temporary_files_behind() {
        let dir = scratch("tidy");
        let path = dir.join("out.json");
        export(&fixture(), ExportFormat::Json, &path).expect("export");

        let entries: Vec<String> = fs::read_dir(&dir)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["out.json".to_string()]);

        cleanup(&dir);
    }

    #[test]
    fn failed_export_does_not_clobber_an_existing_file() {
        let dir = scratch("atomic");
        let path = dir.join("out.csv");
        fs::write(&path, "PRECIOUS").expect("seed file");

        // A struct column cannot be rendered as CSV, so this must fail.
        let nested_schema = Arc::new(Schema::new(vec![Field::new_struct(
            "s",
            vec![Field::new("inner", DataType::Int64, true)],
            false,
        )]));
        let inner: arrow::array::ArrayRef = Arc::new(Int64Array::from(vec![Some(1)]));
        let struct_array = arrow::array::StructArray::from(vec![(
            Arc::new(Field::new("inner", DataType::Int64, true)),
            inner,
        )]);
        let bad = QueryResult {
            schema: nested_schema.clone(),
            batches: vec![
                RecordBatch::try_new(nested_schema, vec![Arc::new(struct_array)]).expect("batch"),
            ],
            elapsed: Duration::from_millis(1),
            row_count: 1,
        };

        let err = export(&bad, ExportFormat::Csv, &path).expect_err("nested CSV must fail");
        assert!(
            err.contains(&path.file_name().unwrap().to_string_lossy().to_string())
                || err.contains("CSV"),
            "error mentions the failure: {err}"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "PRECIOUS",
            "existing file untouched"
        );
        // And the temporary sibling is gone.
        assert_eq!(fs::read_dir(&dir).expect("read dir").count(), 1);

        cleanup(&dir);
    }

    #[test]
    fn expands_leading_tilde() {
        let home = dirs::home_dir().expect("home dir");
        assert_eq!(expand_tilde(Path::new("~/out.csv")), home.join("out.csv"));
        assert_eq!(expand_tilde(Path::new("~")), home);
        // Only a *leading* `~/` is special.
        assert_eq!(
            expand_tilde(Path::new("/tmp/~/out.csv")),
            PathBuf::from("/tmp/~/out.csv")
        );
        assert_eq!(expand_tilde(Path::new("out.csv")), PathBuf::from("out.csv"));
    }

    #[test]
    fn default_filenames_are_bare_and_stable() {
        assert_eq!(default_filename(ExportFormat::Csv), "osage-export.csv");
        assert_eq!(default_filename(ExportFormat::Json), "osage-export.json");
        assert_eq!(
            default_filename(ExportFormat::ArrowIpc),
            "osage-export.arrow"
        );
        // Stable across calls — the app regenerates it on every format change.
        assert_eq!(
            default_filename(ExportFormat::Csv),
            default_filename(ExportFormat::Csv)
        );
        for format in ExportFormat::ALL {
            let name = default_filename(format);
            assert!(
                !name.contains('/'),
                "bare filename so it lands in the cwd: {name}"
            );
        }
    }
}
