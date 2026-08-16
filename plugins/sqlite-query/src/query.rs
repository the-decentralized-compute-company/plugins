//! Running one statement inside a row cap, a byte cap and a wall-clock cap.
//!
//! Every bound is enforced while the result is being read, not after: the point
//! is that `SELECT * FROM events` on a billion-row table returns a small answer
//! quickly, rather than returning a large answer slowly or filling memory
//! first and trimming afterwards.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::Duration;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, ErrorCode, params_from_iter};
use serde::Serialize;
use serde_json::Value;

use crate::policy::{SqlPolicy, denial_hint};
use crate::settings::Limits;
use crate::value::cell_from;

/// Why a result set stopped early.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Truncation {
    /// More rows matched than `max_rows` allows.
    RowLimit,
    /// The rows returned so far were already near `max_bytes`.
    ByteLimit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ColumnInfo {
    pub name: String,
    /// The type from the `CREATE TABLE` statement, when the column came
    /// straight from a table. Expressions have none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declared_type: Option<String>,
}

/// A bounded result set. `rows` is column-positional so the column names are
/// paid for once rather than once per row.
#[derive(Clone, Debug, Serialize)]
pub struct ResultSet {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Vec<Value>>,
    pub row_count: usize,
    /// `None` means the result is complete. Anything else must be surfaced to
    /// the caller verbatim — a silently shortened answer is a wrong answer.
    pub truncated: Option<Truncation>,
    /// How many individual cells were shortened by `max_cell_bytes`. Those
    /// cells also say so in their own value.
    pub truncated_cells: usize,
    pub estimated_bytes: usize,
}

/// What went wrong, split by who can fix it.
#[derive(Clone, Debug)]
pub enum SqlError {
    /// The statement ran out of its wall-clock budget and was interrupted.
    Timeout { after_ms: u64 },
    /// The caller's SQL or parameters were the problem.
    Rejected(String),
    /// The database or the environment was the problem.
    Backend(String),
}

impl std::fmt::Display for SqlError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout { after_ms } => write!(
                formatter,
                "statement exceeded the {after_ms}ms time limit and was cancelled. \
                 Narrow it with a WHERE clause or a LIMIT, or ask the operator to raise \
                 --timeout-ms."
            ),
            Self::Rejected(message) | Self::Backend(message) => write!(formatter, "{message}"),
        }
    }
}

/// Which phase an error came from, used to decide whose fault it is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Prepare,
    Step,
}

/// Run a read-only statement and collect a bounded result set.
pub fn run_query(
    connection: &Connection,
    sql: &str,
    params: &[SqlValue],
    limits: &Limits,
    policy: SqlPolicy,
) -> Result<ResultSet, SqlError> {
    execute(connection, sql, params, limits, policy, false)
}

#[derive(Clone, Debug)]
pub struct WriteOutcome {
    pub result: ResultSet,
    /// Rows inserted, updated or deleted, as counted by SQLite.
    pub rows_affected: u64,
    pub last_insert_rowid: i64,
}

/// Run a statement on a writable connection.
///
/// Unlike [`run_query`] this always steps the statement to completion, even
/// past the row cap: stopping halfway through `INSERT … RETURNING` would leave
/// part of the work undone with no way for the caller to tell.
pub fn run_write(
    connection: &Connection,
    sql: &str,
    params: &[SqlValue],
    limits: &Limits,
) -> Result<WriteOutcome, SqlError> {
    let result = execute(connection, sql, params, limits, SqlPolicy::Write, true)?;
    Ok(WriteOutcome {
        result,
        rows_affected: connection.changes(),
        last_insert_rowid: connection.last_insert_rowid(),
    })
}

fn execute(
    connection: &Connection,
    sql: &str,
    params: &[SqlValue],
    limits: &Limits,
    policy: SqlPolicy,
    drain_remaining: bool,
) -> Result<ResultSet, SqlError> {
    if sql.trim().is_empty() {
        return Err(SqlError::Rejected("sql must not be empty".to_string()));
    }
    bounded(connection, limits, || {
        step_statement(connection, sql, params, limits, policy, drain_remaining)
    })
}

/// Run `work` under the statement time limit.
///
/// Shared with the schema readers in [`crate::schema`], which issue several
/// `PRAGMA`s in a row and should be bounded as one unit rather than each
/// separately.
pub fn bounded<T>(
    connection: &Connection,
    limits: &Limits,
    work: impl FnOnce() -> Result<T, SqlError>,
) -> Result<T, SqlError> {
    let (result, interrupted) = with_interrupt_timeout(connection, limits.timeout, work);

    // An interrupted statement surfaces as an ordinary SQLite error, so the
    // watchdog's own signal is what turns it into an honest "timed out".
    match result {
        Err(SqlError::Backend(_) | SqlError::Rejected(_)) if interrupted => {
            Err(SqlError::Timeout {
                after_ms: limits.timeout.as_millis() as u64,
            })
        }
        other => other,
    }
}

fn step_statement(
    connection: &Connection,
    sql: &str,
    params: &[SqlValue],
    limits: &Limits,
    policy: SqlPolicy,
    drain_remaining: bool,
) -> Result<ResultSet, SqlError> {
    // `prepare` compiles exactly one statement and rejects a trailing second
    // one, so "one statement per call" is enforced by the SQL parser rather
    // than by looking for semicolons.
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| classify(error, Phase::Prepare, policy))?;

    // Belt and braces on top of the read-only file handle: this asks SQLite
    // whether the *compiled* statement writes, so a write attempt fails before
    // it starts with a message that names the cause.
    if policy != SqlPolicy::Write && !statement.readonly() {
        return Err(SqlError::Rejected(
            "this statement writes to the database, but this connection is read-only. \
             A writable database has to be registered by the operator with --db-rw and \
             used through the execute tool."
                .to_string(),
        ));
    }

    let expected = statement.parameter_count();
    if expected != params.len() {
        return Err(SqlError::Rejected(format!(
            "the statement has {expected} bind parameter(s) but {} were supplied",
            params.len()
        )));
    }

    let columns: Vec<ColumnInfo> = statement
        .columns()
        .iter()
        .map(|column| ColumnInfo {
            name: column.name().to_string(),
            declared_type: column.decl_type().map(str::to_string),
        })
        .collect();
    let column_count = statement.column_count();

    let mut rows = statement
        .query(params_from_iter(params.iter()))
        .map_err(|error| classify(error, Phase::Step, policy))?;

    let mut collected: Vec<Vec<Value>> = Vec::new();
    let mut estimated_bytes = 0usize;
    let mut truncated_cells = 0usize;
    let mut truncated = None;

    while let Some(row) = rows
        .next()
        .map_err(|error| classify(error, Phase::Step, policy))?
    {
        // Checked after fetching, so the cap is only reported when a further
        // row really existed.
        if collected.len() >= limits.max_rows {
            truncated = Some(Truncation::RowLimit);
            break;
        }

        let mut values = Vec::with_capacity(column_count);
        let mut row_cost = 0usize;
        for index in 0..column_count {
            let cell = cell_from(
                row.get_ref(index)
                    .map_err(|error| classify(error, Phase::Step, policy))?,
                limits.max_cell_bytes,
            );
            row_cost += cell.cost;
            truncated_cells += usize::from(cell.truncated);
            values.push(cell.value);
        }

        // The first row is always kept even if it alone blows the budget:
        // returning nothing at all would hide the shape of the answer.
        if !collected.is_empty() && estimated_bytes + row_cost > limits.max_bytes {
            truncated = Some(Truncation::ByteLimit);
            break;
        }

        estimated_bytes += row_cost;
        collected.push(values);
    }

    if drain_remaining && truncated.is_some() {
        while rows
            .next()
            .map_err(|error| classify(error, Phase::Step, policy))?
            .is_some()
        {}
    }

    Ok(ResultSet {
        row_count: collected.len(),
        columns,
        rows: collected,
        truncated,
        truncated_cells,
        estimated_bytes,
    })
}

/// Run `work`, cancelling it with `sqlite3_interrupt` if it outlasts `timeout`.
///
/// A watchdog thread is used rather than a progress handler because it bounds
/// wall-clock time, including time spent blocked on I/O or on a lock, which a
/// VM-step counter does not. Returns whether the interrupt actually fired, so
/// the caller can tell a cancellation apart from an ordinary error.
fn with_interrupt_timeout<T>(
    connection: &Connection,
    timeout: Duration,
    work: impl FnOnce() -> T,
) -> (T, bool) {
    let handle = connection.get_interrupt_handle();
    let fired = Arc::new(AtomicBool::new(false));
    let fired_in_watchdog = Arc::clone(&fired);
    let (finished_tx, finished_rx) = channel::<()>();

    let watchdog = std::thread::spawn(move || {
        // `Disconnected` means `work` finished (or panicked) and dropped the
        // sender, which is not a timeout.
        if finished_rx.recv_timeout(timeout) == Err(RecvTimeoutError::Timeout) {
            fired_in_watchdog.store(true, Ordering::SeqCst);
            // Safe after the connection closes: the handle nulls its pointer
            // under a mutex on close.
            handle.interrupt();
        }
    });

    let result = work();
    let _ = finished_tx.send(());
    let _ = watchdog.join();
    (result, fired.load(Ordering::SeqCst))
}

fn classify(error: rusqlite::Error, phase: Phase, policy: SqlPolicy) -> SqlError {
    match &error {
        rusqlite::Error::MultipleStatement => SqlError::Rejected(
            "only one statement may be run per call; remove everything after the first \
             semicolon"
                .to_string(),
        ),
        rusqlite::Error::SqliteFailure(failure, message) => {
            let detail = message.clone().unwrap_or_else(|| error.to_string());
            match failure.code {
                ErrorCode::AuthorizationForStatementDenied => {
                    SqlError::Rejected(format!("{} ({detail})", denial_hint(policy)))
                }
                ErrorCode::OperationInterrupted => {
                    SqlError::Backend("statement was cancelled".to_string())
                }
                ErrorCode::ReadOnly => SqlError::Rejected(format!(
                    "the database is open read-only, so this statement cannot run ({detail})"
                )),
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => SqlError::Backend(format!(
                    "the database is locked by another process ({detail})"
                )),
                ErrorCode::NotADatabase => SqlError::Backend(format!(
                    "the configured file is not a SQLite database ({detail})"
                )),
                ErrorCode::DatabaseCorrupt => {
                    SqlError::Backend(format!("the database file is corrupt ({detail})"))
                }
                // A failure while compiling is a syntax error, an unknown
                // table, or a bad parameter — all of which the caller can fix.
                // A failure while stepping is the database's problem.
                _ if phase == Phase::Prepare => SqlError::Rejected(detail),
                _ => SqlError::Backend(detail),
            }
        }
        rusqlite::Error::InvalidParameterCount(supplied, expected) => SqlError::Rejected(format!(
            "the statement has {expected} bind parameter(s) but {supplied} were supplied"
        )),
        _ if phase == Phase::Prepare => SqlError::Rejected(error.to_string()),
        _ => SqlError::Backend(error.to_string()),
    }
}

/// Convert one JSON bind parameter into a SQLite value.
///
/// Parameters exist so a caller never has to paste a literal into the SQL text.
/// Structured JSON is refused rather than stringified, because silently binding
/// `{"a":1}` as text would make a mismatched query look like it simply found no
/// rows.
pub fn sql_value_from_json(value: &Value) -> Result<SqlValue, String> {
    match value {
        Value::Null => Ok(SqlValue::Null),
        Value::Bool(flag) => Ok(SqlValue::Integer(i64::from(*flag))),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                Ok(SqlValue::Integer(integer))
            } else if let Some(real) = number.as_f64() {
                Ok(SqlValue::Real(real))
            } else {
                Err(format!("{number} is outside the range SQLite can bind"))
            }
        }
        Value::String(text) => Ok(SqlValue::Text(text.clone())),
        Value::Array(_) | Value::Object(_) => Err(
            "bind parameters must be null, a boolean, a number or a string. Serialize \
             structured values to a JSON string first if the column stores JSON."
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TempDatabase, arm, in_memory};
    use serde_json::json;

    fn limits(max_rows: usize, max_bytes: usize) -> Limits {
        Limits {
            max_rows,
            max_bytes,
            ..Limits::default()
        }
    }

    /// A table with `rows` rows, on a connection that only gets the read-only
    /// authorizer once seeding is finished.
    fn seeded(rows: usize) -> Connection {
        let connection =
            in_memory("CREATE TABLE t (id INTEGER PRIMARY KEY, label TEXT NOT NULL, score REAL);");
        for index in 1..=rows {
            connection
                .execute(
                    "INSERT INTO t (id, label, score) VALUES (?1, ?2, ?3)",
                    (index as i64, format!("row-{index}"), index as f64 / 2.0),
                )
                .expect("seed");
        }
        arm(&connection, SqlPolicy::ModelQuery);
        connection
    }

    #[test]
    fn a_complete_result_reports_no_truncation_and_keeps_declared_types() {
        let connection = seeded(3);
        let result = run_query(
            &connection,
            "SELECT id, label FROM t ORDER BY id",
            &[],
            &Limits::default(),
            SqlPolicy::ModelQuery,
        )
        .expect("query runs");

        assert_eq!(result.truncated, None);
        assert_eq!(result.row_count, 3);
        assert_eq!(result.rows[0], vec![json!(1), json!("row-1")]);
        assert_eq!(result.columns[0].declared_type.as_deref(), Some("INTEGER"));
        assert_eq!(result.columns[1].declared_type.as_deref(), Some("TEXT"));
    }

    #[test]
    fn an_expression_column_has_no_declared_type() {
        let connection = seeded(1);
        let result = run_query(
            &connection,
            "SELECT id * 2 AS doubled FROM t",
            &[],
            &Limits::default(),
            SqlPolicy::ModelQuery,
        )
        .expect("query runs");
        assert_eq!(result.columns[0].name, "doubled");
        assert_eq!(result.columns[0].declared_type, None);
    }

    #[test]
    fn the_row_cap_stops_the_scan_and_is_reported() {
        let connection = seeded(50);
        let result = run_query(
            &connection,
            "SELECT id FROM t ORDER BY id",
            &[],
            &limits(5, 1 << 20),
            SqlPolicy::ModelQuery,
        )
        .expect("query runs");

        assert_eq!(result.row_count, 5);
        assert_eq!(result.truncated, Some(Truncation::RowLimit));
    }

    #[test]
    fn a_result_that_exactly_fills_the_row_cap_is_not_called_truncated() {
        let connection = seeded(5);
        let result = run_query(
            &connection,
            "SELECT id FROM t",
            &[],
            &limits(5, 1 << 20),
            SqlPolicy::ModelQuery,
        )
        .expect("query runs");

        assert_eq!(result.row_count, 5);
        assert_eq!(result.truncated, None);
    }

    #[test]
    fn the_byte_cap_stops_the_scan_and_is_reported() {
        let connection = seeded(200);
        let result = run_query(
            &connection,
            "SELECT id, label FROM t ORDER BY id",
            &[],
            &limits(1_000, 1_024),
            SqlPolicy::ModelQuery,
        )
        .expect("query runs");

        assert_eq!(result.truncated, Some(Truncation::ByteLimit));
        assert!(result.row_count > 0, "the caller still sees the shape");
        assert!(result.row_count < 200, "and not the whole table");
    }

    #[test]
    fn one_oversized_row_is_still_returned_rather_than_an_empty_success() {
        let connection = seeded(3);
        let result = run_query(
            &connection,
            "SELECT id FROM t",
            &[],
            &limits(1_000, 1_024),
            SqlPolicy::ModelQuery,
        )
        .expect("query runs");
        assert!(result.row_count >= 1);

        let tiny = run_query(
            &connection,
            "SELECT id FROM t",
            &[],
            &Limits {
                max_rows: 1_000,
                max_bytes: 1,
                ..Limits::default()
            },
            SqlPolicy::ModelQuery,
        )
        .expect("query runs");
        assert_eq!(tiny.row_count, 1);
        assert_eq!(tiny.truncated, Some(Truncation::ByteLimit));
    }

    #[test]
    fn a_long_cell_is_shortened_and_counted() {
        let connection = in_memory("");
        arm(&connection, SqlPolicy::ModelQuery);
        let result = run_query(
            &connection,
            "SELECT ?1 AS blob_of_text",
            &[SqlValue::Text("y".repeat(10_000))],
            &Limits {
                max_cell_bytes: 32,
                ..Limits::default()
            },
            SqlPolicy::ModelQuery,
        )
        .expect("query runs");

        assert_eq!(result.truncated_cells, 1);
        let text = result.rows[0][0].as_str().expect("text");
        assert!(text.contains("bytes truncated"), "{text}");
    }

    #[test]
    fn parameters_are_bound_rather_than_interpolated() {
        let connection = seeded(3);
        let result = run_query(
            &connection,
            "SELECT count(*) FROM t WHERE label = ?1",
            &[SqlValue::Text("row-1'; DROP TABLE t; --".to_string())],
            &Limits::default(),
            SqlPolicy::ModelQuery,
        )
        .expect("query runs");

        assert_eq!(result.rows[0][0], json!(0));
        // The table is still there, which it would not be if the parameter had
        // been pasted into the SQL text.
        run_query(
            &connection,
            "SELECT count(*) FROM t",
            &[],
            &Limits::default(),
            SqlPolicy::ModelQuery,
        )
        .expect("t still exists");
    }

    #[test]
    fn a_parameter_count_mismatch_is_the_callers_error() {
        let connection = seeded(1);
        let error = run_query(
            &connection,
            "SELECT * FROM t WHERE id = ?1 AND label = ?2",
            &[SqlValue::Integer(1)],
            &Limits::default(),
            SqlPolicy::ModelQuery,
        )
        .expect_err("mismatch");
        assert!(matches!(error, SqlError::Rejected(_)), "{error}");
        assert!(error.to_string().contains("2 bind parameter"), "{error}");
    }

    #[test]
    fn a_second_statement_is_refused() {
        let connection = seeded(1);
        let error = run_query(
            &connection,
            "SELECT 1; SELECT 2",
            &[],
            &Limits::default(),
            SqlPolicy::ModelQuery,
        )
        .expect_err("two statements");
        assert!(error.to_string().contains("one statement"), "{error}");
    }

    #[test]
    fn an_empty_statement_is_refused_before_sqlite_sees_it() {
        let connection = seeded(1);
        let error = run_query(
            &connection,
            "   ",
            &[],
            &Limits::default(),
            SqlPolicy::ModelQuery,
        )
        .expect_err("empty");
        assert!(error.to_string().contains("must not be empty"), "{error}");
    }

    #[test]
    fn a_write_is_refused_on_a_read_policy_even_when_the_handle_could_write() {
        // The connection here is an ordinary writable in-memory database, so
        // the refusal comes from the policy and the readonly() check, not from
        // the file handle.
        let connection = seeded(1);
        let error = run_query(
            &connection,
            "DELETE FROM t",
            &[],
            &Limits::default(),
            SqlPolicy::ModelQuery,
        )
        .expect_err("write refused");
        assert!(matches!(error, SqlError::Rejected(_)), "{error}");
    }

    #[test]
    fn attach_is_refused_so_a_query_cannot_reach_another_file() {
        let connection = seeded(1);
        let error = run_query(
            &connection,
            "ATTACH DATABASE 'other.db' AS other",
            &[],
            &Limits::default(),
            SqlPolicy::ModelQuery,
        )
        .expect_err("attach refused");
        assert!(error.to_string().contains("read-only"), "{error}");
    }

    #[test]
    fn pragma_is_refused_for_caller_supplied_sql() {
        let connection = seeded(1);
        run_query(
            &connection,
            "PRAGMA table_info(t)",
            &[],
            &Limits::default(),
            SqlPolicy::ModelQuery,
        )
        .expect_err("pragma refused");
    }

    #[test]
    fn a_runaway_statement_is_cancelled_and_reported_as_a_timeout() {
        let connection = seeded(1);
        let error = run_query(
            &connection,
            "WITH RECURSIVE forever(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM forever) \
             SELECT count(*) FROM forever",
            &[],
            &Limits {
                timeout: Duration::from_millis(150),
                max_rows: 10,
                ..Limits::default()
            },
            SqlPolicy::ModelQuery,
        )
        .expect_err("runaway query");

        assert!(matches!(error, SqlError::Timeout { .. }), "{error}");
        assert!(error.to_string().contains("150ms"), "{error}");
    }

    #[test]
    fn a_fast_statement_is_not_reported_as_a_timeout() {
        let connection = seeded(1);
        let result = run_query(
            &connection,
            "SELECT 1",
            &[],
            &Limits {
                timeout: Duration::from_millis(50),
                ..Limits::default()
            },
            SqlPolicy::ModelQuery,
        );
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn json_parameters_map_onto_sqlite_types_and_reject_structures() {
        assert_eq!(sql_value_from_json(&json!(null)).unwrap(), SqlValue::Null);
        assert_eq!(
            sql_value_from_json(&json!(true)).unwrap(),
            SqlValue::Integer(1)
        );
        assert_eq!(
            sql_value_from_json(&json!(false)).unwrap(),
            SqlValue::Integer(0)
        );
        assert_eq!(
            sql_value_from_json(&json!(7)).unwrap(),
            SqlValue::Integer(7)
        );
        assert_eq!(
            sql_value_from_json(&json!(1.5)).unwrap(),
            SqlValue::Real(1.5)
        );
        assert_eq!(
            sql_value_from_json(&json!("x")).unwrap(),
            SqlValue::Text("x".to_string())
        );
        assert!(sql_value_from_json(&json!([1, 2])).is_err());
        assert!(sql_value_from_json(&json!({"a": 1})).is_err());
    }

    #[test]
    fn a_read_only_file_handle_refuses_a_write_that_the_policy_would_have_allowed() {
        // Proves the file handle itself is read-only: the policy here is the
        // permissive Write policy, so the only thing left to say no is SQLite.
        let temporary =
            TempDatabase::create("CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1);");
        let connection = temporary.open_read_only(SqlPolicy::Write);
        let error = run_query(
            &connection,
            "INSERT INTO t VALUES (2)",
            &[],
            &Limits::default(),
            SqlPolicy::Write,
        )
        .expect_err("read-only handle");
        assert!(
            error.to_string().to_lowercase().contains("read-only")
                || error.to_string().to_lowercase().contains("readonly"),
            "{error}"
        );
    }

    #[test]
    fn a_writable_connection_reports_what_it_changed() {
        let temporary = TempDatabase::create("CREATE TABLE t (id INTEGER PRIMARY KEY);");
        let connection = temporary.open_writable();
        let outcome = run_write(
            &connection,
            "INSERT INTO t (id) VALUES (?1)",
            &[SqlValue::Integer(42)],
            &Limits::default(),
        )
        .expect("insert runs");

        assert_eq!(outcome.rows_affected, 1);
        assert_eq!(outcome.last_insert_rowid, 42);
        assert_eq!(outcome.result.row_count, 0);
    }
}
