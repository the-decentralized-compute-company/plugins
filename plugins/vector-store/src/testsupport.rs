//! Test-only scaffolding: a throwaway directory tree and a directory-link
//! helper.
//!
//! Hand-rolled rather than pulled from a crate so the plugin's release
//! dependency set stays as small as the thing it does — nothing here is
//! compiled into the shipped binary.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A directory under the system temp dir that deletes itself on drop.
pub struct TempTree {
    path: PathBuf,
}

impl TempTree {
    pub fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vector-store-{tag}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp tree");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The tree root as the plugin would hold it: canonical, so containment
    /// checks compare like with like.
    pub fn canonical_root(&self) -> PathBuf {
        std::fs::canonicalize(&self.path).expect("canonicalize temp tree")
    }

    /// Write a file at a `/`-separated relative path, creating parents.
    pub fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let mut target = self.path.clone();
        for segment in relative.split('/') {
            target.push(segment);
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("create parent directory");
        }
        std::fs::write(&target, contents).expect("write temp file");
        target
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        // Best effort: a leaked temp directory is a nuisance, a panicking
        // destructor masking a real test failure is worse.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Create a directory link at `link` pointing at `target`.
///
/// Unix gets a symlink. Windows tries a real symlink first — which needs
/// Developer Mode or `SeCreateSymbolicLinkPrivilege` — and falls back to a
/// directory junction, which an unprivileged user can create. Returns `Err(())`
/// when the platform allows neither, so a test can skip rather than fail on a
/// locked-down machine.
pub fn link_directory(target: &Path, link: &Path) -> Result<(), ()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).map_err(|_| ())
    }

    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_dir(target, link).is_ok() {
            return Ok(());
        }
        let status = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|_| ())?;
        if status.success() && link.exists() {
            Ok(())
        } else {
            Err(())
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        Err(())
    }
}
