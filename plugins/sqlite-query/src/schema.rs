//! Reading the schema, which is where most of the value of this plugin is.
//!
//! A model that can see column types, the primary key, the foreign keys and
//! the indexes writes SQL that joins correctly and filters on indexed columns.
//! A model that can only see table names guesses. So `describe_table` returns
//! the whole picture, including the original `CREATE` statement, rather than a
//! column list.
//!
//! Every statement in here is fixed text owned by the plugin. The one place a
//! caller's string reaches SQL is a table name, and it goes in either as a bind
//! parameter or as a quoted identifier — never by concatenation.

use rusqlite::Connection;
use serde::Serialize;

use crate::query::{SqlError, bounded};
use crate::settings::Limits;

/// SQLite's own limit on an identifier is generous; this is a sanity bound so a
/// megabyte-long "table name" never reaches the parser.
const MAX_OBJECT_NAME_LEN: usize = 255;

#[derive(Clone, Debug, Serialize)]
pub struct ObjectSummary {
    pub name: String,
    /// `table`, `view`, or `virtual table`.
    pub kind: String,
    /// Column names, so one call is usually enough to plan a query. Omitted
    /// when the caller asks for names only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ObjectListing {
    pub database: String,
    pub objects: Vec<ObjectSummary>,
    pub object_count: usize,
    /// True when the schema has more objects than `max_rows` allows.
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ColumnDescription {
    pub name: String,
    /// The declared type from `CREATE TABLE`. SQLite does not require one, so
    /// this is empty for a column declared without a type.
    #[serde(rename = "type")]
    pub declared_type: String,
    pub not_null: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// 1-based position within the primary key, absent for non-key columns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_key_position: Option<i64>,
}

/// One row of `PRAGMA foreign_key_list`, which reports composite keys as one
/// row per column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKeyRow {
    pub id: i64,
    pub seq: i64,
    pub table: String,
    pub from: String,
    pub to: Option<String>,
    pub on_update: String,
    pub on_delete: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ForeignKey {
    pub columns: Vec<String>,
    pub references_table: String,
    /// Empty when the constraint targets the referenced table's primary key
    /// without naming its columns.
    pub references_columns: Vec<String>,
    pub on_update: String,
    pub on_delete: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct IndexDescription {
    pub name: String,
    pub unique: bool,
    pub partial: bool,
    /// `c` for `CREATE INDEX`, `u` for a `UNIQUE` constraint, `pk` for the
    /// primary key.
    pub origin: String,
    pub columns: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TableDescription {
    pub database: String,
    pub name: String,
    pub kind: String,
    /// The statement that created it. Generated columns, CHECK constraints and
    /// collations only show up here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sql: Option<String>,
    pub columns: Vec<ColumnDescription>,
    pub primary_key: Vec<String>,
    pub foreign_keys: Vec<ForeignKey>,
    pub indexes: Vec<IndexDescription>,
    /// True when the column or index list hit `max_rows`.
    pub truncated: bool,
}

/// Wrap a name as a SQL identifier.
///
/// `PRAGMA` takes its argument as an identifier, not as a bind parameter, so
/// this is the one place a caller's string is concatenated into SQL. Doubling
/// the embedded quote is the escape SQLite's own tokenizer defines, so the
/// result is always exactly one identifier no matter what the name contains.
pub fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Reject names that cannot be a SQLite object name.
///
/// The NUL check matters: SQL is handed to SQLite as a C string, so an embedded
/// NUL would silently cut the statement short.
pub fn validate_object_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("table name must not be empty".to_string());
    }
    if name.len() > MAX_OBJECT_NAME_LEN {
        return Err(format!(
            "table name is longer than {MAX_OBJECT_NAME_LEN} bytes"
        ));
    }
    if name.contains('\0') {
        return Err("table name must not contain a NUL byte".to_string());
    }
    Ok(())
}

/// Collapse `PRAGMA foreign_key_list` rows into one entry per constraint.
///
/// The pragma emits one row per referencing column, so a composite key arrives
/// as several rows sharing an `id`. Sorting first makes the output stable
/// regardless of the order SQLite happens to return.
pub fn group_foreign_keys(mut rows: Vec<ForeignKeyRow>) -> Vec<ForeignKey> {
    rows.sort_by_key(|row| (row.id, row.seq));
    let mut grouped: Vec<ForeignKey> = Vec::new();
    let mut current: Option<i64> = None;
    for row in rows {
        if current != Some(row.id) {
            current = Some(row.id);
            grouped.push(ForeignKey {
                columns: Vec::new(),
                references_table: row.table,
                references_columns: Vec::new(),
                on_update: row.on_update,
                on_delete: row.on_delete,
            });
        }
        let entry = grouped.last_mut().expect("a group was just pushed");
        entry.columns.push(row.from);
        if let Some(to) = row.to {
            entry.references_columns.push(to);
        }
    }
    grouped
}

/// Classify a schema object from its `sqlite_schema` type and DDL.
///
/// A virtual table has type `table`, which hides the fact that its data lives
/// somewhere else entirely — worth telling the caller about.
fn object_kind(schema_type: &str, sql: Option<&str>) -> String {
    let is_virtual = sql
        .map(|sql| {
            let head: String = sql.trim_start().chars().take(14).collect();
            head.eq_ignore_ascii_case("CREATE VIRTUAL")
        })
        .unwrap_or(false);
    if schema_type == "table" && is_virtual {
        "virtual table".to_string()
    } else {
        schema_type.to_string()
    }
}

/// List the tables and views in a database.
pub fn list_objects(
    connection: &Connection,
    database: &str,
    include_internal: bool,
    limits: &Limits,
) -> Result<ObjectListing, SqlError> {
    bounded(connection, limits, || {
        let mut statement = connection
            .prepare(
                "SELECT name, type, sql FROM sqlite_schema \
                 WHERE type IN ('table', 'view') ORDER BY type, name",
            )
            .map_err(|error| SqlError::Backend(format!("could not read sqlite_schema: {error}")))?;

        let mut names: Vec<(String, String)> = Vec::new();
        let mut rows = statement
            .query([])
            .map_err(|error| SqlError::Backend(format!("could not read sqlite_schema: {error}")))?;
        while let Some(row) = rows
            .next()
            .map_err(|error| SqlError::Backend(format!("could not read sqlite_schema: {error}")))?
        {
            let name: String = row.get(0).map_err(backend)?;
            let schema_type: String = row.get(1).map_err(backend)?;
            let sql: Option<String> = row.get(2).map_err(backend)?;
            // `sqlite_` is reserved for SQLite's own bookkeeping tables. They
            // are rarely what a question is about, so they are opt-in.
            if !include_internal && name.starts_with("sqlite_") {
                continue;
            }
            names.push((name, object_kind(&schema_type, sql.as_deref())));
        }
        drop(rows);
        drop(statement);

        let truncated = names.len() > limits.max_rows;
        names.truncate(limits.max_rows);

        let mut objects = Vec::with_capacity(names.len());
        for (name, kind) in names {
            let columns = column_names(connection, &name)?;
            objects.push(ObjectSummary {
                name,
                kind,
                columns: Some(columns),
            });
        }

        Ok(ObjectListing {
            database: database.to_string(),
            object_count: objects.len(),
            objects,
            truncated,
        })
    })
}

/// Describe one table or view in full.
pub fn describe_object(
    connection: &Connection,
    database: &str,
    requested_name: &str,
    limits: &Limits,
) -> Result<TableDescription, SqlError> {
    validate_object_name(requested_name).map_err(SqlError::Rejected)?;

    bounded(connection, limits, || {
        // `COLLATE NOCASE` because SQLite object names are case-insensitive, so
        // asking for `Orders` should find `orders`. The name that comes back is
        // then the canonical one used for every PRAGMA below.
        let found: Option<(String, String, Option<String>)> = connection
            .query_row(
                "SELECT name, type, sql FROM sqlite_schema \
                 WHERE name = ?1 COLLATE NOCASE AND type IN ('table', 'view') LIMIT 1",
                [requested_name],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(SqlError::Backend(format!(
                    "could not read sqlite_schema: {other}"
                ))),
            })?;

        let Some((name, schema_type, sql)) = found else {
            return Err(SqlError::Rejected(format!(
                "no table or view named {requested_name:?} in database {database:?}. \
                 Call list_tables to see what exists."
            )));
        };

        let mut columns = Vec::new();
        let mut primary_key: Vec<(i64, String)> = Vec::new();
        let mut truncated = false;

        let mut statement = connection
            .prepare(&format!(
                "PRAGMA main.table_info({})",
                quote_identifier(&name)
            ))
            .map_err(backend)?;
        let mut rows = statement.query([]).map_err(backend)?;
        while let Some(row) = rows.next().map_err(backend)? {
            if columns.len() >= limits.max_rows {
                truncated = true;
                break;
            }
            let column_name: String = row.get(1).map_err(backend)?;
            let declared_type: String = row.get(2).map_err(backend)?;
            let not_null: i64 = row.get(3).map_err(backend)?;
            let default: Option<String> = row.get(4).map_err(backend)?;
            let primary_key_position: i64 = row.get(5).map_err(backend)?;
            if primary_key_position > 0 {
                primary_key.push((primary_key_position, column_name.clone()));
            }
            columns.push(ColumnDescription {
                name: column_name,
                declared_type,
                not_null: not_null != 0,
                default,
                primary_key_position: (primary_key_position > 0).then_some(primary_key_position),
            });
        }
        drop(rows);
        drop(statement);

        primary_key.sort_by_key(|(position, _)| *position);

        Ok(TableDescription {
            database: database.to_string(),
            kind: object_kind(&schema_type, sql.as_deref()),
            sql,
            columns,
            primary_key: primary_key.into_iter().map(|(_, name)| name).collect(),
            foreign_keys: foreign_keys(connection, &name)?,
            indexes: indexes(connection, &name, limits)?,
            truncated,
            name,
        })
    })
}

fn column_names(connection: &Connection, table: &str) -> Result<Vec<String>, SqlError> {
    let mut statement = connection
        .prepare(&format!(
            "PRAGMA main.table_info({})",
            quote_identifier(table)
        ))
        .map_err(backend)?;
    let mut rows = statement.query([]).map_err(backend)?;
    let mut names = Vec::new();
    while let Some(row) = rows.next().map_err(backend)? {
        names.push(row.get::<_, String>(1).map_err(backend)?);
    }
    Ok(names)
}

fn foreign_keys(connection: &Connection, table: &str) -> Result<Vec<ForeignKey>, SqlError> {
    let mut statement = connection
        .prepare(&format!(
            "PRAGMA main.foreign_key_list({})",
            quote_identifier(table)
        ))
        .map_err(backend)?;
    let mut rows = statement.query([]).map_err(backend)?;
    let mut collected = Vec::new();
    while let Some(row) = rows.next().map_err(backend)? {
        collected.push(ForeignKeyRow {
            id: row.get(0).map_err(backend)?,
            seq: row.get(1).map_err(backend)?,
            table: row.get(2).map_err(backend)?,
            from: row.get(3).map_err(backend)?,
            to: row.get(4).map_err(backend)?,
            on_update: row.get(5).map_err(backend)?,
            on_delete: row.get(6).map_err(backend)?,
        });
    }
    Ok(group_foreign_keys(collected))
}

fn indexes(
    connection: &Connection,
    table: &str,
    limits: &Limits,
) -> Result<Vec<IndexDescription>, SqlError> {
    let mut statement = connection
        .prepare(&format!(
            "PRAGMA main.index_list({})",
            quote_identifier(table)
        ))
        .map_err(backend)?;
    let mut rows = statement.query([]).map_err(backend)?;
    let mut listed = Vec::new();
    while let Some(row) = rows.next().map_err(backend)? {
        if listed.len() >= limits.max_rows {
            break;
        }
        let unique: i64 = row.get(2).map_err(backend)?;
        let partial: i64 = row.get(4).map_err(backend)?;
        listed.push((
            row.get::<_, String>(1).map_err(backend)?,
            unique != 0,
            row.get::<_, String>(3).map_err(backend)?,
            partial != 0,
        ));
    }
    drop(rows);
    drop(statement);

    let mut described = Vec::with_capacity(listed.len());
    for (name, unique, origin, partial) in listed {
        described.push(IndexDescription {
            columns: index_columns(connection, &name)?,
            name,
            unique,
            partial,
            origin,
        });
    }
    Ok(described)
}

fn index_columns(connection: &Connection, index: &str) -> Result<Vec<String>, SqlError> {
    let mut statement = connection
        .prepare(&format!(
            "PRAGMA main.index_info({})",
            quote_identifier(index)
        ))
        .map_err(backend)?;
    let mut rows = statement.query([]).map_err(backend)?;
    let mut columns = Vec::new();
    while let Some(row) = rows.next().map_err(backend)? {
        // A NULL name means the index is on an expression rather than a column.
        let name: Option<String> = row.get(2).map_err(backend)?;
        columns.push(name.unwrap_or_else(|| "<expression>".to_string()));
    }
    Ok(columns)
}

/// Every statement in this module is the plugin's own, so a failure is the
/// database's problem rather than the caller's.
fn backend(error: rusqlite::Error) -> SqlError {
    SqlError::Backend(format!("schema read failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::SqlPolicy;
    use crate::testutil::{arm, in_memory};

    const SCHEMA: &str = "
        CREATE TABLE customers (
            id INTEGER PRIMARY KEY,
            email TEXT NOT NULL UNIQUE,
            region TEXT DEFAULT 'unknown'
        );
        CREATE TABLE orders (
            tenant TEXT NOT NULL,
            number INTEGER NOT NULL,
            customer_id INTEGER REFERENCES customers(id) ON DELETE CASCADE,
            total REAL,
            PRIMARY KEY (tenant, number)
        );
        CREATE INDEX orders_by_customer ON orders(customer_id);
        CREATE VIEW big_orders AS SELECT * FROM orders WHERE total > 100;
    ";

    fn schema_connection() -> Connection {
        let connection = in_memory(SCHEMA);
        arm(&connection, SqlPolicy::Introspection);
        connection
    }

    #[test]
    fn identifiers_are_quoted_so_a_hostile_name_stays_one_identifier() {
        assert_eq!(quote_identifier("orders"), "\"orders\"");
        assert_eq!(quote_identifier("my\"table"), "\"my\"\"table\"");
        assert_eq!(
            quote_identifier("x\"); DROP TABLE t; --"),
            "\"x\"\"); DROP TABLE t; --\""
        );
    }

    #[test]
    fn impossible_object_names_are_refused() {
        assert!(validate_object_name("orders").is_ok());
        assert!(validate_object_name("").is_err());
        assert!(validate_object_name("   ").is_err());
        assert!(validate_object_name("a\0b").is_err());
        assert!(validate_object_name(&"n".repeat(MAX_OBJECT_NAME_LEN + 1)).is_err());
    }

    #[test]
    fn a_composite_foreign_key_becomes_one_entry() {
        let grouped = group_foreign_keys(vec![
            ForeignKeyRow {
                id: 0,
                seq: 1,
                table: "parent".into(),
                from: "b".into(),
                to: Some("y".into()),
                on_update: "NO ACTION".into(),
                on_delete: "CASCADE".into(),
            },
            ForeignKeyRow {
                id: 0,
                seq: 0,
                table: "parent".into(),
                from: "a".into(),
                to: Some("x".into()),
                on_update: "NO ACTION".into(),
                on_delete: "CASCADE".into(),
            },
        ]);

        assert_eq!(
            grouped,
            vec![ForeignKey {
                columns: vec!["a".into(), "b".into()],
                references_table: "parent".into(),
                references_columns: vec!["x".into(), "y".into()],
                on_update: "NO ACTION".into(),
                on_delete: "CASCADE".into(),
            }]
        );
    }

    #[test]
    fn separate_constraints_stay_separate_and_ordered_by_id() {
        let grouped = group_foreign_keys(vec![
            ForeignKeyRow {
                id: 1,
                seq: 0,
                table: "second".into(),
                from: "b".into(),
                to: None,
                on_update: "NO ACTION".into(),
                on_delete: "NO ACTION".into(),
            },
            ForeignKeyRow {
                id: 0,
                seq: 0,
                table: "first".into(),
                from: "a".into(),
                to: Some("id".into()),
                on_update: "NO ACTION".into(),
                on_delete: "NO ACTION".into(),
            },
        ]);

        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].references_table, "first");
        assert_eq!(grouped[1].references_table, "second");
        // An implicit target is reported as no columns rather than as a guess.
        assert!(grouped[1].references_columns.is_empty());
    }

    #[test]
    fn a_virtual_table_is_labelled_as_one() {
        assert_eq!(object_kind("table", Some("CREATE TABLE t (a)")), "table");
        assert_eq!(
            object_kind("table", Some("  create virtual table t USING fts5(body)")),
            "virtual table"
        );
        assert_eq!(
            object_kind("view", Some("CREATE VIEW v AS SELECT 1")),
            "view"
        );
        assert_eq!(object_kind("table", None), "table");
    }

    #[test]
    fn listing_shows_tables_views_and_their_columns() {
        let connection = schema_connection();
        let listing = list_objects(&connection, "app", false, &Limits::default()).expect("listed");

        let names: Vec<_> = listing
            .objects
            .iter()
            .map(|object| object.name.as_str())
            .collect();
        assert_eq!(names, ["customers", "orders", "big_orders"]);
        assert_eq!(listing.objects[2].kind, "view");
        assert!(!listing.truncated);

        let order_columns = listing.objects[1].columns.as_ref().expect("columns");
        assert_eq!(order_columns, &["tenant", "number", "customer_id", "total"]);
    }

    #[test]
    fn listing_respects_the_row_cap_and_says_so() {
        let connection = schema_connection();
        let listing = list_objects(
            &connection,
            "app",
            false,
            &Limits {
                max_rows: 1,
                ..Limits::default()
            },
        )
        .expect("listed");
        assert_eq!(listing.object_count, 1);
        assert!(listing.truncated);
    }

    #[test]
    fn describe_reports_types_keys_foreign_keys_and_indexes() {
        let connection = schema_connection();
        let described =
            describe_object(&connection, "app", "orders", &Limits::default()).expect("described");

        assert_eq!(described.kind, "table");
        assert_eq!(described.primary_key, ["tenant", "number"]);
        assert!(described.sql.as_deref().unwrap().contains("CREATE TABLE"));

        let tenant = &described.columns[0];
        assert_eq!(tenant.name, "tenant");
        assert_eq!(tenant.declared_type, "TEXT");
        assert!(tenant.not_null);
        assert_eq!(tenant.primary_key_position, Some(1));

        let total = described
            .columns
            .iter()
            .find(|column| column.name == "total")
            .expect("total column");
        assert!(!total.not_null);
        assert_eq!(total.primary_key_position, None);

        assert_eq!(described.foreign_keys.len(), 1);
        assert_eq!(described.foreign_keys[0].columns, ["customer_id"]);
        assert_eq!(described.foreign_keys[0].references_table, "customers");
        assert_eq!(described.foreign_keys[0].on_delete, "CASCADE");

        let index = described
            .indexes
            .iter()
            .find(|index| index.name == "orders_by_customer")
            .expect("declared index");
        assert_eq!(index.columns, ["customer_id"]);
        assert!(!index.unique);
    }

    #[test]
    fn describe_reports_a_column_default_and_a_unique_constraint_index() {
        let connection = schema_connection();
        let described = describe_object(&connection, "app", "customers", &Limits::default())
            .expect("described");

        let region = described
            .columns
            .iter()
            .find(|column| column.name == "region")
            .expect("region column");
        assert_eq!(region.default.as_deref(), Some("'unknown'"));

        assert!(
            described
                .indexes
                .iter()
                .any(|index| index.unique && index.origin == "u" && index.columns == ["email"]),
            "the UNIQUE constraint should surface as a unique index: {:?}",
            described.indexes
        );
    }

    #[test]
    fn describe_is_case_insensitive_like_sqlite_itself() {
        let connection = schema_connection();
        let described =
            describe_object(&connection, "app", "ORDERS", &Limits::default()).expect("described");
        assert_eq!(described.name, "orders");
    }

    #[test]
    fn describing_something_that_does_not_exist_is_a_clear_caller_error() {
        let connection = schema_connection();
        let error = describe_object(&connection, "app", "nope", &Limits::default())
            .expect_err("no such table");
        assert!(matches!(error, SqlError::Rejected(_)), "{error}");
        assert!(error.to_string().contains("list_tables"), "{error}");
    }

    #[test]
    fn a_table_name_carrying_sql_is_treated_as_a_name_and_nothing_else() {
        let connection = schema_connection();
        let error = describe_object(
            &connection,
            "app",
            "orders\"); DROP TABLE orders; --",
            &Limits::default(),
        )
        .expect_err("not a table");
        assert!(matches!(error, SqlError::Rejected(_)), "{error}");

        // The table is still there.
        describe_object(&connection, "app", "orders", &Limits::default()).expect("orders survived");
    }

    #[test]
    fn internal_sqlite_tables_are_opt_in() {
        let connection = in_memory(
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT); INSERT INTO t DEFAULT VALUES;",
        );
        arm(&connection, SqlPolicy::Introspection);

        let hidden = list_objects(&connection, "app", false, &Limits::default()).expect("listed");
        assert!(
            hidden
                .objects
                .iter()
                .all(|object| object.name != "sqlite_sequence")
        );

        let shown = list_objects(&connection, "app", true, &Limits::default()).expect("listed");
        assert!(
            shown
                .objects
                .iter()
                .any(|object| object.name == "sqlite_sequence")
        );
    }
}
