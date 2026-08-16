//! What SQLite is allowed to compile, decided by SQLite itself.
//!
//! This is the half of the safety story that a read-only file handle does not
//! cover. Opening read-only stops writes to the *main* database, but it does
//! not stop two other things:
//!
//! * `ATTACH DATABASE '/etc/secrets.db' AS leak` — a plain `SELECT` can then
//!   read any file the plugin process can reach, which would defeat the whole
//!   point of configuring an explicit list of databases.
//! * `CREATE TEMP TABLE …` — the temp database is writable even when the main
//!   one is not, so a query could still burn disk in the temp directory.
//!
//! Both are refused here, at `sqlite3_prepare` time, through the authorizer
//! callback. That is a decision made by SQLite's own parser about the compiled
//! statement, not a guess made by looking at the SQL text: scanning for the
//! word "DROP" is defeated by a comment, a view, a trigger, or a different
//! keyword, whereas the authorizer sees the real action list.

use rusqlite::hooks::{AuthAction, Authorization};

/// Which of the three connection kinds a statement is being compiled for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqlPolicy {
    /// Arbitrary SQL supplied by a caller against a read-only connection.
    ModelQuery,
    /// The plugin's own fixed `PRAGMA` and `sqlite_schema` statements, still on
    /// a read-only connection.
    Introspection,
    /// A caller's SQL against a database the operator explicitly opened for
    /// writing with `--db-rw`.
    Write,
}

/// SQL functions that read or write files, or load native code, regardless of
/// which database is open. None of these ships in a stock SQLite build, but a
/// contributor's SQLite could have been built with them, and an allowlist of
/// every safe scalar function would be unmaintainable.
const DENIED_FUNCTIONS: &[&str] = &[
    "load_extension",
    "readfile",
    "writefile",
    "edit",
    "fsdir",
    "zipfile",
    "fts3_tokenizer",
];

fn is_denied_function(name: &str) -> bool {
    DENIED_FUNCTIONS
        .iter()
        .any(|denied| denied.eq_ignore_ascii_case(name))
}

/// Decide one authorizer action.
///
/// Written as a free function over borrowed data so the policy can be tested
/// exhaustively without opening a database.
pub fn authorize(policy: SqlPolicy, action: &AuthAction<'_>) -> Authorization {
    match action {
        // Denied in every mode, including `--db-rw`: attaching a second file is
        // how a query escapes the configured set of databases.
        AuthAction::Attach { .. } | AuthAction::Detach { .. } => Authorization::Deny,

        AuthAction::Function { function_name } if is_denied_function(function_name) => {
            Authorization::Deny
        }

        // `PRAGMA` belongs to the plugin's own introspection statements and to
        // nobody else. A caller has `list_tables` and `describe_table` for
        // schema questions, and `execute` exists to change data — not to change
        // how the database is stored. `PRAGMA writable_schema=ON` on a `--db-rw`
        // database would be a very short path to a corrupt file.
        AuthAction::Pragma { .. } => match policy {
            SqlPolicy::Introspection => Authorization::Allow,
            SqlPolicy::ModelQuery | SqlPolicy::Write => Authorization::Deny,
        },

        // A writable database was an explicit operator decision, so past the
        // rules above the caller gets ordinary SQL.
        _ if policy == SqlPolicy::Write => Authorization::Allow,

        AuthAction::Select
        | AuthAction::Read { .. }
        | AuthAction::Function { .. }
        | AuthAction::Recursive => Authorization::Allow,

        // Fail closed. `AuthAction` is `#[non_exhaustive]`, so a newer SQLite
        // action this code has never heard of is refused rather than allowed.
        _ => Authorization::Deny,
    }
}

/// The message a caller sees when the authorizer refuses a statement.
///
/// SQLite reports `not authorized` with no detail, which is useless to a model
/// trying to fix its own query.
pub fn denial_hint(policy: SqlPolicy) -> &'static str {
    match policy {
        SqlPolicy::Write => {
            "SQLite refused to compile this statement. ATTACH, DETACH, PRAGMA and \
             file-access functions are blocked even on a writable database."
        }
        _ => {
            "SQLite refused to compile this statement. This connection is read-only: \
             only SELECT and other read actions are allowed, and ATTACH, PRAGMA, temp \
             table creation and file-access functions are blocked. Use describe_table \
             for schema details."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::hooks::TransactionOperation;

    const READ_POLICIES: [SqlPolicy; 2] = [SqlPolicy::ModelQuery, SqlPolicy::Introspection];

    #[test]
    fn attach_and_detach_are_denied_in_every_mode() {
        for policy in [
            SqlPolicy::ModelQuery,
            SqlPolicy::Introspection,
            SqlPolicy::Write,
        ] {
            assert_eq!(
                authorize(
                    policy,
                    &AuthAction::Attach {
                        filename: "/etc/other.db"
                    }
                ),
                Authorization::Deny,
                "{policy:?} must not allow ATTACH"
            );
            assert_eq!(
                authorize(
                    policy,
                    &AuthAction::Detach {
                        database_name: "leak"
                    }
                ),
                Authorization::Deny,
                "{policy:?} must not allow DETACH"
            );
        }
    }

    #[test]
    fn file_and_extension_functions_are_denied_in_every_mode_case_insensitively() {
        for policy in [
            SqlPolicy::ModelQuery,
            SqlPolicy::Introspection,
            SqlPolicy::Write,
        ] {
            for name in ["load_extension", "LOAD_EXTENSION", "readfile", "WriteFile"] {
                assert_eq!(
                    authorize(
                        policy,
                        &AuthAction::Function {
                            function_name: name
                        }
                    ),
                    Authorization::Deny,
                    "{policy:?} must not allow {name}"
                );
            }
        }
    }

    #[test]
    fn ordinary_functions_stay_available() {
        assert_eq!(
            authorize(
                SqlPolicy::ModelQuery,
                &AuthAction::Function {
                    function_name: "json_extract"
                }
            ),
            Authorization::Allow
        );
    }

    #[test]
    fn reads_are_allowed_on_a_read_only_connection() {
        for policy in READ_POLICIES {
            assert_eq!(authorize(policy, &AuthAction::Select), Authorization::Allow);
            assert_eq!(
                authorize(
                    policy,
                    &AuthAction::Read {
                        table_name: "orders",
                        column_name: "total"
                    }
                ),
                Authorization::Allow
            );
            assert_eq!(
                authorize(policy, &AuthAction::Recursive),
                Authorization::Allow
            );
        }
    }

    #[test]
    fn every_mutating_action_is_denied_on_a_read_only_connection() {
        let mutations = [
            AuthAction::Insert {
                table_name: "orders",
            },
            AuthAction::Update {
                table_name: "orders",
                column_name: "total",
            },
            AuthAction::Delete {
                table_name: "orders",
            },
            AuthAction::DropTable {
                table_name: "orders",
            },
            AuthAction::DropView {
                view_name: "recent",
            },
            AuthAction::CreateTable {
                table_name: "scratch",
            },
            AuthAction::CreateView {
                view_name: "scratch",
            },
            AuthAction::AlterTable {
                database_name: "main",
                table_name: "orders",
            },
            AuthAction::Reindex { index_name: "idx" },
            AuthAction::Analyze {
                table_name: "orders",
            },
            AuthAction::CreateVtable {
                table_name: "t",
                module_name: "fts5",
            },
            AuthAction::DropVtable {
                table_name: "t",
                module_name: "fts5",
            },
            AuthAction::Transaction {
                operation: TransactionOperation::Begin,
            },
            AuthAction::Savepoint {
                operation: TransactionOperation::Begin,
                savepoint_name: "sp",
            },
        ];
        for policy in READ_POLICIES {
            for action in &mutations {
                assert_eq!(
                    authorize(policy, action),
                    Authorization::Deny,
                    "{policy:?} must deny {action:?}"
                );
            }
        }
    }

    #[test]
    fn temp_objects_are_denied_even_though_the_temp_database_is_writable() {
        for policy in READ_POLICIES {
            for action in [
                AuthAction::CreateTempTable {
                    table_name: "scratch",
                },
                AuthAction::CreateTempView {
                    view_name: "scratch",
                },
                AuthAction::CreateTempIndex {
                    index_name: "i",
                    table_name: "scratch",
                },
                AuthAction::CreateTempTrigger {
                    trigger_name: "t",
                    table_name: "scratch",
                },
            ] {
                assert_eq!(authorize(policy, &action), Authorization::Deny);
            }
        }
    }

    #[test]
    fn pragma_is_reserved_for_the_plugins_own_introspection() {
        let pragma = AuthAction::Pragma {
            pragma_name: "table_info",
            pragma_value: Some("orders"),
        };
        assert_eq!(
            authorize(SqlPolicy::Introspection, &pragma),
            Authorization::Allow
        );
        assert_eq!(
            authorize(SqlPolicy::ModelQuery, &pragma),
            Authorization::Deny
        );
        // Including on a writable database: `execute` is there to change data,
        // not to reach for `PRAGMA writable_schema=ON`.
        assert_eq!(authorize(SqlPolicy::Write, &pragma), Authorization::Deny);
    }

    #[test]
    fn a_writable_database_allows_writes_but_still_not_escape_hatches() {
        for allowed in [
            AuthAction::Insert {
                table_name: "orders",
            },
            AuthAction::Update {
                table_name: "orders",
                column_name: "total",
            },
            AuthAction::Delete {
                table_name: "orders",
            },
            AuthAction::Transaction {
                operation: TransactionOperation::Begin,
            },
        ] {
            assert_eq!(
                authorize(SqlPolicy::Write, &allowed),
                Authorization::Allow,
                "{allowed:?} should be allowed on a --db-rw database"
            );
        }

        for denied in [
            AuthAction::Attach { filename: "x.db" },
            AuthAction::Detach {
                database_name: "leak",
            },
            AuthAction::Pragma {
                pragma_name: "writable_schema",
                pragma_value: Some("ON"),
            },
            AuthAction::Function {
                function_name: "load_extension",
            },
        ] {
            assert_eq!(
                authorize(SqlPolicy::Write, &denied),
                Authorization::Deny,
                "{denied:?} should stay blocked even on a --db-rw database"
            );
        }
    }

    #[test]
    fn an_unrecognized_action_code_fails_closed() {
        let unknown = AuthAction::Unknown {
            code: 9_999,
            arg1: None,
            arg2: None,
        };
        for policy in READ_POLICIES {
            assert_eq!(authorize(policy, &unknown), Authorization::Deny);
        }
    }
}
