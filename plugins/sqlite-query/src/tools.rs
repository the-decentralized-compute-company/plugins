//! The tool handlers, and the shapes they hand back to a model.
//!
//! Two rules shape every response here:
//!
//! * A bounded answer always says it is bounded. `truncated` is never omitted
//!   when it happened, and a human-readable `note` repeats it, because a model
//!   that silently receives 200 of 4,000,000 rows will confidently report the
//!   wrong total.
//! * The limits that produced the answer travel with it, so the caller can tell
//!   the difference between "there are only 12 rows" and "you were given 12".
//!
//! Every handler does its SQLite work inside `spawn_blocking`. `rusqlite` is a
//! blocking C library, and the plugin's control connection has to stay
//! responsive to host health checks while a query runs.

use std::sync::Arc;
use std::time::Instant;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tdcc_plugin::{PluginError, PluginResult};

use crate::db::Catalog;
use crate::policy::SqlPolicy;
use crate::query::{ResultSet, SqlError, Truncation, run_query, run_write, sql_value_from_json};
use crate::schema::{ObjectListing, TableDescription, describe_object, list_objects};
use crate::settings::Limits;

// ---------------------------------------------------------------------------
// Tool arguments
//
// The doc comment on each field becomes its description in the JSON Schema the
// host publishes, so these are written for the model that will read them.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTablesArgs {
    /// Name of a configured database, as reported by `list_databases`. May be
    /// omitted when only one database is configured. This is an alias, never a
    /// file path — file paths are chosen by the operator and are not accepted
    /// here.
    #[serde(default)]
    pub database: Option<String>,

    /// Include SQLite's own bookkeeping tables, whose names start with
    /// `sqlite_`. Off by default because they almost never answer a question
    /// about the data.
    #[serde(default)]
    pub include_internal: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DescribeTableArgs {
    /// Name of a configured database. May be omitted when only one is
    /// configured.
    #[serde(default)]
    pub database: Option<String>,

    /// Table or view to describe. Matching is case-insensitive, the same way
    /// SQLite treats object names.
    pub table: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueryArgs {
    /// Name of a configured database. May be omitted when only one is
    /// configured.
    #[serde(default)]
    pub database: Option<String>,

    /// One read-only SQL statement. Multiple statements separated by semicolons
    /// are refused. The connection is opened read-only, so anything that writes
    /// fails regardless of how it is phrased.
    pub sql: String,

    /// Values bound to the `?1`, `?2` … placeholders in `sql`, in order. Use
    /// these instead of writing literals into the statement text. Only null,
    /// booleans, numbers and strings can be bound.
    #[serde(default)]
    pub params: Option<Vec<Value>>,

    /// Stop after this many rows. It can only lower the operator's row cap,
    /// never raise it.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecuteArgs {
    /// Name of a database the operator registered as writable with
    /// `--db-rw`. Any other database refuses this tool.
    #[serde(default)]
    pub database: Option<String>,

    /// One SQL statement that may modify data. Multiple statements separated by
    /// semicolons are refused.
    pub sql: String,

    /// Values bound to the `?1`, `?2` … placeholders in `sql`, in order.
    #[serde(default)]
    pub params: Option<Vec<Value>>,
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// The caps a result was produced under, echoed so a caller can tell a small
/// answer from a trimmed one.
#[derive(Debug, Serialize)]
pub struct LimitsReport {
    pub max_rows: usize,
    pub max_bytes: usize,
    pub max_cell_bytes: usize,
    pub timeout_ms: u64,
}

impl LimitsReport {
    fn new(limits: &Limits) -> Self {
        Self {
            max_rows: limits.max_rows,
            max_bytes: limits.max_bytes,
            max_cell_bytes: limits.max_cell_bytes,
            timeout_ms: limits.timeout.as_millis() as u64,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DatabaseStatus {
    pub alias: String,
    /// The path the operator configured. Shown so an operator can see which
    /// file an answer came from.
    pub path: String,
    /// `read-only` or `read-write`.
    pub mode: &'static str,
    /// False when the configured file is missing or is not a regular file.
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DatabaseListing {
    pub databases: Vec<DatabaseStatus>,
    pub limits: LimitsReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TableListingResponse {
    #[serde(flatten)]
    pub listing: ObjectListing,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DescribeResponse {
    #[serde(flatten)]
    pub table: TableDescription,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct QueryResponse {
    pub database: String,
    #[serde(flatten)]
    pub result: ResultSet,
    pub elapsed_ms: u64,
    pub limits: LimitsReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ExecuteResponse {
    pub database: String,
    pub rows_affected: u64,
    pub last_insert_rowid: i64,
    #[serde(flatten)]
    pub result: ResultSet,
    pub elapsed_ms: u64,
    pub limits: LimitsReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn list_databases(catalog: Arc<Catalog>) -> PluginResult<DatabaseListing> {
    blocking(move || {
        let databases: Vec<DatabaseStatus> = catalog
            .databases()
            .map(|database| {
                let metadata = std::fs::metadata(&database.path);
                let (available, size_bytes, problem) = match metadata {
                    Ok(metadata) if metadata.is_file() => (true, Some(metadata.len()), None),
                    Ok(_) => (
                        false,
                        None,
                        Some("configured path is not a regular file".to_string()),
                    ),
                    Err(error) => (false, None, Some(error.to_string())),
                };
                DatabaseStatus {
                    alias: database.alias.clone(),
                    path: database.configured_path.display().to_string(),
                    mode: database.mode(),
                    available,
                    size_bytes,
                    problem,
                }
            })
            .collect();

        let note = databases.is_empty().then(|| {
            "No databases are configured. The operator adds them with \
             --db <alias>=<path> in this plugin's [[plugin]].args."
                .to_string()
        });

        Ok(DatabaseListing {
            databases,
            limits: LimitsReport::new(&catalog.limits()),
            note,
        })
    })
    .await
}

pub async fn list_tables(
    catalog: Arc<Catalog>,
    args: ListTablesArgs,
) -> PluginResult<TableListingResponse> {
    blocking(move || {
        let limits = catalog.limits();
        let database = catalog
            .resolve(args.database.as_deref())
            .map_err(PluginError::invalid_params)?;
        let connection = catalog
            .open_read_only(database, SqlPolicy::Introspection)
            .map_err(PluginError::internal)?;

        let started = Instant::now();
        let listing = list_objects(
            &connection,
            &database.alias,
            args.include_internal.unwrap_or(false),
            &limits,
        )
        .map_err(to_plugin_error)?;

        let note = listing.truncated.then(|| {
            format!(
                "Only the first {} objects are shown; this database has more. \
                 Ask the operator to raise --max-rows to see the rest.",
                listing.object_count
            )
        });
        Ok(TableListingResponse {
            listing,
            elapsed_ms: started.elapsed().as_millis() as u64,
            note,
        })
    })
    .await
}

pub async fn describe_table(
    catalog: Arc<Catalog>,
    args: DescribeTableArgs,
) -> PluginResult<DescribeResponse> {
    blocking(move || {
        let limits = catalog.limits();
        let database = catalog
            .resolve(args.database.as_deref())
            .map_err(PluginError::invalid_params)?;
        let connection = catalog
            .open_read_only(database, SqlPolicy::Introspection)
            .map_err(PluginError::internal)?;

        let started = Instant::now();
        let table = describe_object(&connection, &database.alias, &args.table, &limits)
            .map_err(to_plugin_error)?;

        let note = table.truncated.then(|| {
            "This table has more columns or indexes than max_rows allows, so the lists \
             above are incomplete. The `sql` field still shows the full definition."
                .to_string()
        });
        Ok(DescribeResponse {
            table,
            elapsed_ms: started.elapsed().as_millis() as u64,
            note,
        })
    })
    .await
}

pub async fn query(catalog: Arc<Catalog>, args: QueryArgs) -> PluginResult<QueryResponse> {
    blocking(move || {
        let mut limits = catalog.limits();
        // A caller may ask for fewer rows. It may not ask for more: the cap is
        // the operator's, not the caller's.
        if let Some(requested) = args.limit {
            limits.max_rows = limits.max_rows.min(requested.max(1));
        }
        let params = bind_parameters(args.params.as_deref())?;

        let database = catalog
            .resolve(args.database.as_deref())
            .map_err(PluginError::invalid_params)?;
        let connection = catalog
            .open_read_only(database, SqlPolicy::ModelQuery)
            .map_err(PluginError::internal)?;

        let started = Instant::now();
        let result = run_query(
            &connection,
            &args.sql,
            &params,
            &limits,
            SqlPolicy::ModelQuery,
        )
        .map_err(to_plugin_error)?;

        Ok(QueryResponse {
            database: database.alias.clone(),
            note: truncation_note(&result),
            elapsed_ms: started.elapsed().as_millis() as u64,
            limits: LimitsReport::new(&limits),
            result,
        })
    })
    .await
}

pub async fn execute(catalog: Arc<Catalog>, args: ExecuteArgs) -> PluginResult<ExecuteResponse> {
    blocking(move || {
        let limits = catalog.limits();
        let params = bind_parameters(args.params.as_deref())?;

        let database = catalog
            .resolve(args.database.as_deref())
            .map_err(PluginError::invalid_params)?;
        // Refuses unless the operator registered this database with --db-rw.
        let connection = catalog
            .open_writable(database)
            .map_err(PluginError::invalid_params)?;

        let started = Instant::now();
        let outcome =
            run_write(&connection, &args.sql, &params, &limits).map_err(to_plugin_error)?;

        Ok(ExecuteResponse {
            database: database.alias.clone(),
            rows_affected: outcome.rows_affected,
            last_insert_rowid: outcome.last_insert_rowid,
            note: truncation_note(&outcome.result),
            elapsed_ms: started.elapsed().as_millis() as u64,
            limits: LimitsReport::new(&limits),
            result: outcome.result,
        })
    })
    .await
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Say in words what the `truncated` field says in a token, because that is the
/// difference between a model reporting "4 orders" and "at least 4 orders".
pub fn truncation_note(result: &ResultSet) -> Option<String> {
    let mut notes = Vec::new();
    match result.truncated {
        Some(Truncation::RowLimit) => notes.push(format!(
            "INCOMPLETE: stopped at the {}-row limit, more rows matched. Add a LIMIT, \
             an ORDER BY, or an aggregate such as count(*) to get a complete answer.",
            result.row_count
        )),
        Some(Truncation::ByteLimit) => notes.push(format!(
            "INCOMPLETE: stopped after {} rows because the response size limit was \
             reached, more rows matched. Select fewer columns or narrow the query.",
            result.row_count
        )),
        None => {}
    }
    if result.truncated_cells > 0 {
        notes.push(format!(
            "{} cell(s) were too long and were shortened; each says so in its own value.",
            result.truncated_cells
        ));
    }
    (!notes.is_empty()).then(|| notes.join(" "))
}

fn bind_parameters(params: Option<&[Value]>) -> PluginResult<Vec<rusqlite::types::Value>> {
    params
        .unwrap_or(&[])
        .iter()
        .enumerate()
        .map(|(index, value)| {
            sql_value_from_json(value).map_err(|error| {
                PluginError::invalid_params(format!("parameter {}: {error}", index + 1))
            })
        })
        .collect()
}

/// Route an error to the party that can fix it. A rejected statement or an
/// exhausted time budget is something the caller can rewrite; anything else is
/// the database's or the operator's problem.
fn to_plugin_error(error: SqlError) -> PluginError {
    let message = error.to_string();
    match error {
        SqlError::Rejected(_) | SqlError::Timeout { .. } => PluginError::invalid_params(message),
        SqlError::Backend(_) => PluginError::internal(message),
    }
}

/// Move blocking SQLite work off the async runtime so host health checks keep
/// answering while a query runs.
async fn blocking<T, F>(work: F) -> PluginResult<T>
where
    F: FnOnce() -> PluginResult<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| PluginError::internal(format!("sqlite worker did not finish: {error}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ColumnInfo;
    use crate::settings::{DatabaseSpec, Settings};
    use crate::testutil::TempDatabase;
    use std::path::PathBuf;

    const SHOP: &str = "
        CREATE TABLE customers (
            id     INTEGER PRIMARY KEY,
            email  TEXT NOT NULL UNIQUE,
            region TEXT DEFAULT 'unknown'
        );
        CREATE TABLE orders (
            id          INTEGER PRIMARY KEY,
            customer_id INTEGER NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
            total       REAL NOT NULL
        );
        INSERT INTO customers (id, email, region) VALUES (1, 'ada@example.com', 'eu');
        INSERT INTO orders (id, customer_id, total) VALUES (1, 1, 42.5), (2, 1, 17.0);
    ";

    /// A catalog wired to a real file on disk, so these tests exercise the same
    /// open path the host does rather than an in-memory shortcut.
    fn catalog_for(temporary: &TempDatabase, writable: bool) -> Arc<Catalog> {
        Arc::new(Catalog::new(Settings {
            databases: vec![DatabaseSpec {
                alias: "shop".to_string(),
                path: temporary.path().to_path_buf(),
                writable,
            }],
            limits: Limits::default(),
        }))
    }

    #[tokio::test]
    async fn a_query_reads_a_real_file_and_reports_the_caps_it_ran_under() {
        let temporary = TempDatabase::create(SHOP);
        let response = query(
            catalog_for(&temporary, false),
            QueryArgs {
                database: Some("shop".to_string()),
                sql: "SELECT email, (SELECT sum(total) FROM orders WHERE customer_id = c.id) \
                      AS spend FROM customers c WHERE region = ?1"
                    .to_string(),
                params: Some(vec![serde_json::json!("eu")]),
                limit: None,
            },
        )
        .await
        .expect("query runs");

        assert_eq!(response.database, "shop");
        assert_eq!(response.result.row_count, 1);
        assert_eq!(
            response.result.rows[0][0],
            serde_json::json!("ada@example.com")
        );
        assert_eq!(response.result.rows[0][1], serde_json::json!(59.5));
        assert_eq!(response.result.truncated, None);
        assert_eq!(response.note, None);
        assert_eq!(response.limits.max_rows, Limits::default().max_rows);
    }

    #[tokio::test]
    async fn a_caller_limit_lowers_the_row_cap_but_cannot_raise_it() {
        let temporary = TempDatabase::create(SHOP);
        let catalog = catalog_for(&temporary, false);

        let lowered = query(
            Arc::clone(&catalog),
            QueryArgs {
                database: None,
                sql: "SELECT id FROM orders".to_string(),
                params: None,
                limit: Some(1),
            },
        )
        .await
        .expect("query runs");
        assert_eq!(lowered.result.row_count, 1);
        assert_eq!(lowered.limits.max_rows, 1);
        assert!(
            lowered
                .note
                .expect("truncation note")
                .contains("INCOMPLETE")
        );

        let raised = query(
            catalog,
            QueryArgs {
                database: None,
                sql: "SELECT id FROM orders".to_string(),
                params: None,
                limit: Some(1_000_000),
            },
        )
        .await
        .expect("query runs");
        assert_eq!(raised.limits.max_rows, Limits::default().max_rows);
    }

    #[tokio::test]
    async fn a_write_through_the_query_tool_is_refused_on_a_read_only_database() {
        let temporary = TempDatabase::create(SHOP);
        let error = query(
            catalog_for(&temporary, false),
            QueryArgs {
                database: None,
                sql: "DELETE FROM orders".to_string(),
                params: None,
                limit: None,
            },
        )
        .await
        .expect_err("writes are refused");
        assert!(error.message.contains("read-only"), "{}", error.message);

        // And the rows are still there.
        let remaining = query(
            catalog_for(&temporary, false),
            QueryArgs {
                database: None,
                sql: "SELECT count(*) FROM orders".to_string(),
                params: None,
                limit: None,
            },
        )
        .await
        .expect("query runs");
        assert_eq!(remaining.result.rows[0][0], serde_json::json!(2));
    }

    #[tokio::test]
    async fn execute_is_refused_unless_the_operator_opted_the_database_in() {
        let temporary = TempDatabase::create(SHOP);
        let error = execute(
            catalog_for(&temporary, false),
            ExecuteArgs {
                database: None,
                sql: "DELETE FROM orders".to_string(),
                params: None,
            },
        )
        .await
        .expect_err("execute needs --db-rw");
        assert!(error.message.contains("--db-rw"), "{}", error.message);
    }

    #[tokio::test]
    async fn execute_writes_when_the_database_was_registered_with_db_rw() {
        let temporary = TempDatabase::create(SHOP);
        let catalog = catalog_for(&temporary, true);

        let outcome = execute(
            Arc::clone(&catalog),
            ExecuteArgs {
                database: None,
                sql: "DELETE FROM orders WHERE id = ?1".to_string(),
                params: Some(vec![serde_json::json!(1)]),
            },
        )
        .await
        .expect("execute runs");
        assert_eq!(outcome.rows_affected, 1);

        let remaining = query(
            catalog,
            QueryArgs {
                database: None,
                sql: "SELECT count(*) FROM orders".to_string(),
                params: None,
                limit: None,
            },
        )
        .await
        .expect("query runs");
        assert_eq!(remaining.result.rows[0][0], serde_json::json!(1));
    }

    #[tokio::test]
    async fn the_schema_tools_answer_from_a_real_file() {
        let temporary = TempDatabase::create(SHOP);
        let catalog = catalog_for(&temporary, false);

        let listing = list_tables(
            Arc::clone(&catalog),
            ListTablesArgs {
                database: None,
                include_internal: None,
            },
        )
        .await
        .expect("list runs");
        let names: Vec<_> = listing
            .listing
            .objects
            .iter()
            .map(|object| object.name.as_str())
            .collect();
        assert_eq!(names, ["customers", "orders"]);

        let described = describe_table(
            catalog,
            DescribeTableArgs {
                database: None,
                table: "orders".to_string(),
            },
        )
        .await
        .expect("describe runs");
        assert_eq!(described.table.primary_key, ["id"]);
        assert_eq!(
            described.table.foreign_keys[0].references_table,
            "customers"
        );
        assert_eq!(described.note, None);
    }

    #[tokio::test]
    async fn an_unknown_alias_is_a_caller_error_that_names_the_real_ones() {
        let temporary = TempDatabase::create(SHOP);
        let error = query(
            catalog_for(&temporary, false),
            QueryArgs {
                database: Some("payroll".to_string()),
                sql: "SELECT 1".to_string(),
                params: None,
                limit: None,
            },
        )
        .await
        .expect_err("unknown alias");
        assert_eq!(error.code, PluginError::invalid_params("").code);
        assert!(error.message.contains("shop"), "{}", error.message);
    }

    #[tokio::test]
    async fn list_databases_reports_a_missing_file_instead_of_hiding_it() {
        let catalog = Arc::new(Catalog::new(Settings {
            databases: vec![DatabaseSpec {
                alias: "gone".to_string(),
                path: PathBuf::from("/nowhere/gone.db"),
                writable: false,
            }],
            limits: Limits::default(),
        }));

        let listing = list_databases(catalog).await.expect("listing runs");
        assert_eq!(listing.databases.len(), 1);
        assert!(!listing.databases[0].available);
        assert!(listing.databases[0].problem.is_some());
        assert_eq!(listing.databases[0].mode, "read-only");
        assert_eq!(listing.note, None);
    }

    #[tokio::test]
    async fn an_empty_catalog_says_how_to_fix_itself() {
        let catalog = Arc::new(Catalog::new(Settings {
            databases: Vec::new(),
            limits: Limits::default(),
        }));
        let listing = list_databases(catalog).await.expect("listing runs");
        assert!(listing.databases.is_empty());
        assert!(listing.note.expect("a note").contains("--db"));
    }

    fn result_set(
        row_count: usize,
        truncated: Option<Truncation>,
        truncated_cells: usize,
    ) -> ResultSet {
        ResultSet {
            columns: vec![ColumnInfo {
                name: "id".into(),
                declared_type: None,
            }],
            rows: Vec::new(),
            row_count,
            truncated,
            truncated_cells,
            estimated_bytes: 0,
        }
    }

    #[test]
    fn a_complete_result_carries_no_note() {
        assert_eq!(truncation_note(&result_set(3, None, 0)), None);
    }

    #[test]
    fn each_kind_of_truncation_is_stated_in_words() {
        let rows = truncation_note(&result_set(200, Some(Truncation::RowLimit), 0))
            .expect("row truncation is announced");
        assert!(rows.contains("INCOMPLETE"), "{rows}");
        assert!(rows.contains("200-row limit"), "{rows}");

        let bytes = truncation_note(&result_set(17, Some(Truncation::ByteLimit), 0))
            .expect("byte truncation is announced");
        assert!(bytes.contains("INCOMPLETE"), "{bytes}");
        assert!(bytes.contains("size limit"), "{bytes}");
    }

    #[test]
    fn shortened_cells_are_announced_even_when_every_row_was_returned() {
        let note = truncation_note(&result_set(2, None, 5)).expect("cells are announced");
        assert!(note.contains("5 cell(s)"), "{note}");
        assert!(!note.contains("INCOMPLETE"), "{note}");
    }

    #[test]
    fn bind_parameters_reports_the_position_of_a_bad_value() {
        let error = bind_parameters(Some(&[
            serde_json::json!("ok"),
            serde_json::json!({"nested": true}),
        ]))
        .expect_err("objects cannot be bound");
        assert!(error.message.contains("parameter 2"), "{}", error.message);
    }

    #[test]
    fn no_parameters_binds_nothing() {
        assert!(bind_parameters(None).unwrap().is_empty());
        assert!(bind_parameters(Some(&[])).unwrap().is_empty());
    }

    #[test]
    fn errors_are_routed_to_whoever_can_fix_them() {
        assert_eq!(
            to_plugin_error(SqlError::Rejected("bad sql".into())).code,
            PluginError::invalid_params("").code
        );
        assert_eq!(
            to_plugin_error(SqlError::Timeout { after_ms: 5 }).code,
            PluginError::invalid_params("").code
        );
        assert_eq!(
            to_plugin_error(SqlError::Backend("disk".into())).code,
            PluginError::internal("").code
        );
    }
}
