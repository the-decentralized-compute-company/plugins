//! Test-only scaffolding: a throwaway directory and a repository builder.
//!
//! Hand-rolled rather than pulled from a crate so the plugin's release
//! dependency set stays as small as the thing it does — nothing here is
//! compiled into the shipped binary.
//!
//! The fixtures build *real* repositories with libgit2's write APIs, so every
//! test in this crate reads real objects rather than a mock. That is the point:
//! a stub could not tell you that a blame hunk's `orig_start_line` means what
//! this code assumes it means. The plugin itself never calls a write API; the
//! writes live here, behind `#[cfg(test)]`.
//!
//! Times are deterministic — a fixed base epoch, one hour per commit — so an
//! assertion about a rendered date is stable on every machine and in every
//! time zone.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use git2::{Repository, Signature, Time};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 2024-03-15T08:21:07Z, the instant every fixture's first commit carries.
pub const BASE_EPOCH: i64 = 1_710_490_867;
/// Seconds between one fixture commit and the next.
pub const COMMIT_INTERVAL: i64 = 3_600;

pub const DEFAULT_AUTHOR_NAME: &str = "Ada Lovelace";
pub const DEFAULT_AUTHOR_EMAIL: &str = "ada@example.org";

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
            "git-tools-{tag}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp tree");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Initialise a repository in a subdirectory of this tree.
    pub fn repository(&self, name: &str) -> RepoFixture {
        RepoFixture::init(self.path.join(name))
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        // Best effort: a leaked temp directory is a nuisance, a panicking
        // destructor masking a real test failure is worse.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub struct RepoFixture {
    root: PathBuf,
    repository: Repository,
    clock: Cell<i64>,
}

impl RepoFixture {
    fn init(root: PathBuf) -> Self {
        std::fs::create_dir_all(&root).expect("create repository directory");
        let repository = Repository::init(&root).expect("git init");

        // Pin the line-ending filters off. Without this a contributor whose
        // global gitconfig sets core.autocrlf=true gets different blob bytes
        // than the assertions below expect, and the failure looks like a bug in
        // the plugin rather than in the fixture.
        {
            let mut config = repository.config().expect("repository config");
            config.set_bool("core.autocrlf", false).expect("autocrlf");
            config.set_bool("core.safecrlf", false).expect("safecrlf");
        }

        Self {
            root,
            repository,
            clock: Cell::new(BASE_EPOCH),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn repository(&self) -> &Repository {
        &self.repository
    }

    /// Set one local config key, as `.git/config` would carry it.
    pub fn set_config(&self, key: &str, value: &str) {
        let mut config = self.repository.config().expect("repository config");
        config.set_str(key, value).expect("set config value");
    }

    /// Write a file at a `/`-separated relative path, creating parents.
    pub fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let mut target = self.root.clone();
        for segment in relative.split('/') {
            target.push(segment);
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("create parent directory");
        }
        std::fs::write(&target, contents).expect("write file");
        target
    }

    pub fn remove(&self, relative: &str) {
        let mut target = self.root.clone();
        for segment in relative.split('/') {
            target.push(segment);
        }
        std::fs::remove_file(&target).expect("remove file");
    }

    /// Stage everything and commit it as the default author.
    pub fn commit(&self, message: &str) -> git2::Oid {
        self.commit_as(DEFAULT_AUTHOR_NAME, DEFAULT_AUTHOR_EMAIL, message)
    }

    /// Stage everything and commit it as a named author, on the next tick of
    /// the fixture clock.
    pub fn commit_as(&self, name: &str, email: &str, message: &str) -> git2::Oid {
        let when = self.tick();
        let signature =
            Signature::new(name, email, &Time::new(when, 0)).expect("build a signature");

        let mut index = self.repository.index().expect("index");
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .expect("stage additions");
        // add_all does not notice deletions; update_all does.
        index
            .update_all(["*"].iter(), None)
            .expect("stage removals");
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = self.repository.find_tree(tree_id).expect("find tree");

        let parent = self
            .repository
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok());
        let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();

        self.repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &parents,
            )
            .expect("commit")
    }

    /// Create a branch pointing at the current HEAD commit.
    pub fn branch(&self, name: &str) {
        let head = self
            .repository
            .head()
            .expect("HEAD")
            .peel_to_commit()
            .expect("HEAD commit");
        self.repository
            .branch(name, &head, false)
            .expect("create branch");
    }

    /// Create a lightweight tag pointing at the current HEAD commit.
    pub fn tag_light(&self, name: &str) {
        let head = self
            .repository
            .head()
            .expect("HEAD")
            .peel_to_commit()
            .expect("HEAD commit");
        self.repository
            .tag_lightweight(name, head.as_object(), false)
            .expect("create lightweight tag");
    }

    /// Create an annotated tag pointing at the current HEAD commit.
    pub fn tag_annotated(&self, name: &str, message: &str) {
        let when = self.tick();
        let signature = Signature::new(
            DEFAULT_AUTHOR_NAME,
            DEFAULT_AUTHOR_EMAIL,
            &Time::new(when, 0),
        )
        .expect("build a signature");
        let head = self
            .repository
            .head()
            .expect("HEAD")
            .peel_to_commit()
            .expect("HEAD commit");
        self.repository
            .tag(name, head.as_object(), &signature, message, false)
            .expect("create annotated tag");
    }

    /// Copy this repository's git directory into `destination` and mark it
    /// bare, producing a genuine bare repository with the same objects.
    ///
    /// Cheaper and more faithful than rebuilding history through the low-level
    /// object API, and it keeps the bare-repository tests reading the same
    /// commits as everything else.
    pub fn clone_bare(&self, destination: &Path) {
        copy_dir_all(&self.root.join(".git"), destination);
        // A stale index and worktree-shaped config would make libgit2 disagree
        // with itself about whether this is bare.
        let _ = std::fs::remove_file(destination.join("index"));
        let bare = Repository::open_bare(destination).expect("open the copy as bare");
        let mut config = bare.config().expect("config");
        config.set_bool("core.bare", true).expect("core.bare");
        // Absent on a fresh init; removing it is belt and braces for a
        // fixture that may have set it.
        let _ = config.remove("core.worktree");
    }

    fn tick(&self) -> i64 {
        let now = self.clock.get();
        self.clock.set(now + COMMIT_INTERVAL);
        now
    }
}

fn copy_dir_all(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("create destination");
    for entry in std::fs::read_dir(source).expect("read source directory") {
        let entry = entry.expect("directory entry");
        let kind = entry.file_type().expect("file type");
        let target = destination.join(entry.file_name());
        if kind.is_dir() {
            copy_dir_all(&entry.path(), &target);
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixture_builds_a_repository_with_deterministic_times() {
        let tree = TempTree::new("fixture-self");
        let fixture = tree.repository("repo");
        fixture.write("a.txt", "one\n");
        let first = fixture.commit("first");
        fixture.write("b.txt", "two\n");
        let second = fixture.commit("second");

        assert_ne!(first, second);

        let repository = fixture.repository();
        let head = repository
            .head()
            .expect("HEAD")
            .peel_to_commit()
            .expect("commit");
        assert_eq!(head.id(), second);
        assert_eq!(head.summary(), Some("second"));
        assert_eq!(head.author().when().seconds(), BASE_EPOCH + COMMIT_INTERVAL);
        assert_eq!(head.parent_count(), 1);
        assert_eq!(
            head.parent(0).expect("parent").author().when().seconds(),
            BASE_EPOCH
        );
    }

    #[test]
    fn a_deletion_is_staged_by_the_next_commit() {
        let tree = TempTree::new("fixture-delete");
        let fixture = tree.repository("repo");
        fixture.write("keep.txt", "keep\n");
        fixture.write("drop.txt", "drop\n");
        fixture.commit("both");
        fixture.remove("drop.txt");
        fixture.commit("one gone");

        let head_tree = fixture
            .repository()
            .head()
            .expect("HEAD")
            .peel_to_tree()
            .expect("tree");
        assert!(head_tree.get_name("keep.txt").is_some());
        assert!(head_tree.get_name("drop.txt").is_none());
    }

    #[test]
    fn a_bare_copy_carries_the_same_objects() {
        let tree = TempTree::new("fixture-bare");
        let fixture = tree.repository("repo");
        fixture.write("a.txt", "one\n");
        let head = fixture.commit("only");

        let bare_path = tree.path().join("bare.git");
        fixture.clone_bare(&bare_path);

        let bare = Repository::open(&bare_path).expect("open bare");
        assert!(bare.is_bare());
        assert!(bare.workdir().is_none());
        assert_eq!(
            bare.head()
                .expect("HEAD")
                .peel_to_commit()
                .expect("commit")
                .id(),
            head
        );
    }
}
