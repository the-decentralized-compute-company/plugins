//! Database fixtures for the unit tests.
//!
//! Two shapes are needed. An in-memory database is enough to test the caps, the
//! authorizer policy and the schema readers. A real file on disk is the only
//! way to test the thing that matters most — that `SQLITE_OPEN_READ_ONLY`
//! refuses a write even when nothing else does.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{Connection, OpenFlags};

use crate::policy::{SqlPolicy, authorize};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// Install the authorizer for `policy` on an already-populated connection.
///
/// Seeding has to happen first: a `ModelQuery` authorizer refuses `CREATE
/// TABLE`, which is exactly the behaviour under test.
pub fn arm(connection: &Connection, policy: SqlPolicy) {
    connection.authorizer(Some(move |context: rusqlite::hooks::AuthContext<'_>| {
        authorize(policy, &context.action)
    }));
}

/// An unarmed in-memory database with `setup` already applied.
pub fn in_memory(setup: &str) -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory database opens");
    if !setup.trim().is_empty() {
        connection.execute_batch(setup).expect("setup runs");
    }
    connection
}

/// A SQLite file in the system temp directory, removed when dropped.
pub struct TempDatabase {
    path: PathBuf,
}

impl TempDatabase {
    pub fn create(setup: &str) -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tdcc-sqlite-query-test-{}-{id}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let connection = Connection::open(&path).expect("temporary database is created");
        if !setup.trim().is_empty() {
            connection.execute_batch(setup).expect("setup runs");
        }
        drop(connection);
        Self { path }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn open_read_only(&self, policy: SqlPolicy) -> Connection {
        let connection = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .expect("read-only open");
        arm(&connection, policy);
        connection
    }

    pub fn open_writable(&self) -> Connection {
        let connection = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .expect("writable open");
        arm(&connection, SqlPolicy::Write);
        connection
    }
}

impl Drop for TempDatabase {
    fn drop(&mut self) {
        // Best effort: a still-open connection on Windows keeps the file, and a
        // leftover test file in the temp directory is not worth a panic.
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_extension("db-wal"));
        let _ = std::fs::remove_file(self.path.with_extension("db-shm"));
    }
}
