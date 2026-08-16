//! The set of databases this plugin may open, and how it opens them.
//!
//! Confinement here rests on one structural decision rather than on a check:
//! **no tool takes a path**. A caller selects a database by alias, and an alias
//! can only resolve to a `PathBuf` that came from `[[plugin]].args` or the
//! environment at launch. There is no code path from caller input to a
//! filesystem path, so there is nothing for `../` to escape through.
//!
//! The second half of confinement is [`crate::policy`], which stops a compiled
//! statement from reaching a different file via `ATTACH`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags};

use crate::policy::{SqlPolicy, authorize};
use crate::settings::{Limits, Settings, validate_alias};

/// One configured database, after launch-time path resolution.
#[derive(Clone, Debug)]
pub struct Database {
    pub alias: String,
    /// Exactly what the operator wrote, kept for error messages.
    pub configured_path: PathBuf,
    /// The only path this plugin will ever hand to SQLite for this alias.
    pub path: PathBuf,
    /// Set by `--db-rw`. Read-only databases can never be opened for writing.
    pub writable: bool,
}

impl Database {
    /// Mode as reported to callers. Kept as a word rather than a bool so the
    /// tool output reads the same way the configuration does.
    pub fn mode(&self) -> &'static str {
        if self.writable {
            "read-write"
        } else {
            "read-only"
        }
    }
}

/// Every database the plugin knows about, plus the shared statement bounds.
#[derive(Clone, Debug)]
pub struct Catalog {
    databases: BTreeMap<String, Database>,
    limits: Limits,
}

impl Catalog {
    /// Resolve every configured path once, at launch.
    ///
    /// Canonicalization is best-effort: a database that does not exist yet is
    /// still registered, so `list_databases` can report *why* it is unusable
    /// instead of the alias silently not existing.
    pub fn new(settings: Settings) -> Self {
        let mut databases = BTreeMap::new();
        for spec in settings.databases {
            let path = std::fs::canonicalize(&spec.path).unwrap_or_else(|_| spec.path.clone());
            databases.insert(
                spec.alias.clone(),
                Database {
                    alias: spec.alias,
                    configured_path: spec.path,
                    path,
                    writable: spec.writable,
                },
            );
        }
        Self {
            databases,
            limits: settings.limits,
        }
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub fn databases(&self) -> impl Iterator<Item = &Database> {
        self.databases.values()
    }

    pub fn aliases(&self) -> Vec<&str> {
        self.databases.keys().map(String::as_str).collect()
    }

    /// Map an optional caller-supplied alias onto a configured database.
    ///
    /// Omitting the alias is allowed only when there is exactly one database,
    /// because guessing between several is how a model ends up reading the
    /// wrong system's data.
    pub fn resolve(&self, requested: Option<&str>) -> Result<&Database, String> {
        if self.databases.is_empty() {
            return Err(
                "no databases are configured. The operator must launch this plugin with \
                 --db <alias>=<path> in [[plugin]].args (or TDCC_SQLITE_QUERY_DB)."
                    .to_string(),
            );
        }
        match requested {
            Some(alias) => {
                // Validate before lookup so a caller passing a path gets told
                // that aliases are not paths, rather than "unknown database".
                validate_alias(alias)?;
                self.databases.get(alias).ok_or_else(|| {
                    format!(
                        "unknown database {alias:?}. Configured databases: {}.",
                        self.aliases().join(", ")
                    )
                })
            }
            None if self.databases.len() == 1 => Ok(self
                .databases
                .values()
                .next()
                .expect("checked len() == 1 above")),
            None => Err(format!(
                "several databases are configured, so 'database' is required. Choose one of: {}.",
                self.aliases().join(", ")
            )),
        }
    }

    /// Open a read-only connection.
    ///
    /// `SQLITE_OPEN_READ_ONLY` is what actually makes this read-only: the file
    /// handle cannot write, so it does not matter what the SQL says. The
    /// authorizer on top refuses `ATTACH` and temp-object creation, which a
    /// read-only handle does not cover.
    ///
    /// `SQLITE_OPEN_URI` is deliberately absent, so a configured path is always
    /// a plain filename and never a `file:` URI with query parameters.
    pub fn open_read_only(
        &self,
        database: &Database,
        policy: SqlPolicy,
    ) -> Result<Connection, String> {
        debug_assert!(policy != SqlPolicy::Write);
        self.open(
            database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            policy,
        )
    }

    /// Open a writable connection, only for a database registered `--db-rw`.
    ///
    /// `SQLITE_OPEN_CREATE` is absent: a typo in the configured path must fail,
    /// not quietly create an empty database somewhere on the contributor's
    /// disk.
    pub fn open_writable(&self, database: &Database) -> Result<Connection, String> {
        if !database.writable {
            return Err(format!(
                "database {:?} is read-only. Writing requires the operator to register it \
                 with --db-rw <alias>=<path>, which is off by default.",
                database.alias
            ));
        }
        self.open(
            database,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            SqlPolicy::Write,
        )
    }

    fn open(
        &self,
        database: &Database,
        flags: OpenFlags,
        policy: SqlPolicy,
    ) -> Result<Connection, String> {
        if !database.path.is_file() {
            return Err(format!(
                "database {:?} is configured as {} but that is not a readable file",
                database.alias,
                database.configured_path.display()
            ));
        }
        let connection = Connection::open_with_flags(&database.path, flags).map_err(|error| {
            format!(
                "could not open database {:?} ({}): {error}",
                database.alias,
                database.configured_path.display()
            )
        })?;
        // Bound the wait for a lock held by another process. Without this a
        // busy database blocks until the statement timeout fires, which turns
        // a fast, clear "database is locked" into a slow, vague timeout.
        connection
            .busy_timeout(self.limits.timeout)
            .map_err(|error| format!("could not set busy timeout: {error}"))?;
        connection.authorizer(Some(move |context: rusqlite::hooks::AuthContext<'_>| {
            authorize(policy, &context.action)
        }));
        Ok(connection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::DatabaseSpec;

    fn catalog(aliases: &[&str]) -> Catalog {
        Catalog::new(Settings {
            databases: aliases
                .iter()
                .map(|alias| DatabaseSpec {
                    alias: (*alias).to_string(),
                    path: PathBuf::from(format!("/nowhere/{alias}.db")),
                    writable: false,
                })
                .collect(),
            limits: Limits::default(),
        })
    }

    #[test]
    fn a_single_database_may_be_selected_by_omission() {
        let catalog = catalog(&["app"]);
        assert_eq!(catalog.resolve(None).unwrap().alias, "app");
        assert_eq!(catalog.resolve(Some("app")).unwrap().alias, "app");
    }

    #[test]
    fn omitting_the_alias_with_several_databases_lists_the_choices() {
        let catalog = catalog(&["app", "analytics"]);
        let error = catalog.resolve(None).expect_err("ambiguous");
        assert!(error.contains("analytics"), "{error}");
        assert!(error.contains("app"), "{error}");
    }

    #[test]
    fn an_unknown_alias_lists_the_configured_ones() {
        let error = catalog(&["app"])
            .resolve(Some("other"))
            .expect_err("unknown");
        assert!(error.contains("unknown database"), "{error}");
        assert!(error.contains("app"), "{error}");
    }

    #[test]
    fn a_path_shaped_alias_is_refused_before_any_lookup_happens() {
        let catalog = catalog(&["app"]);
        for attempt in [
            "../../etc/passwd",
            "/etc/shadow",
            r"C:\Windows\x.db",
            "app/../other",
        ] {
            let error = catalog
                .resolve(Some(attempt))
                .expect_err("paths are not aliases");
            assert!(
                error.contains("may only contain") || error.contains("must start with"),
                "{attempt} produced {error}"
            );
        }
    }

    #[test]
    fn an_empty_catalog_explains_how_to_configure_one() {
        let error = catalog(&[]).resolve(None).expect_err("nothing configured");
        assert!(error.contains("--db"), "{error}");
    }

    #[test]
    fn a_read_only_database_refuses_a_writable_connection_before_touching_the_disk() {
        let catalog = catalog(&["app"]);
        let database = catalog.resolve(Some("app")).unwrap();
        let error = catalog.open_writable(database).expect_err("read-only");
        assert!(error.contains("--db-rw"), "{error}");
    }

    #[test]
    fn a_missing_file_is_reported_with_the_configured_path() {
        let catalog = catalog(&["app"]);
        let database = catalog.resolve(Some("app")).unwrap();
        let error = catalog
            .open_read_only(database, SqlPolicy::ModelQuery)
            .expect_err("missing file");
        assert!(error.contains("not a readable file"), "{error}");
        assert!(error.contains("app.db"), "{error}");
    }

    #[test]
    fn mode_reads_the_way_the_configuration_does() {
        let catalog = Catalog::new(Settings {
            databases: vec![
                DatabaseSpec {
                    alias: "ro".into(),
                    path: PathBuf::from("/nowhere/ro.db"),
                    writable: false,
                },
                DatabaseSpec {
                    alias: "rw".into(),
                    path: PathBuf::from("/nowhere/rw.db"),
                    writable: true,
                },
            ],
            limits: Limits::default(),
        });
        assert_eq!(catalog.resolve(Some("ro")).unwrap().mode(), "read-only");
        assert_eq!(catalog.resolve(Some("rw")).unwrap().mode(), "read-write");
    }
}
