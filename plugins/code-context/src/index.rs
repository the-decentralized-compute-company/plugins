//! The file inventory.
//!
//! The index is a *manifest plus a symbol table*, not a content cache. It
//! records which files exist, how big they are, when they changed, how many
//! lines they have, and what they declare. It deliberately does not hold their
//! text: this process runs on hardware somebody contributed, and a plugin that
//! quietly parks a repository in RAM is a bad guest.
//!
//! Reindexing is incremental. Every refresh re-walks the directory tree — that
//! part is unavoidable, it is how you learn a file was deleted — but a file
//! whose size and mtime are unchanged is never reopened, reread, or reparsed.
//! On a large repository that is the difference between a stat-bound walk and a
//! read-bound one.
//!
//! The change signal is `(size, mtime)`. A rewrite that preserves both is
//! invisible until someone calls `reindex` with `force: true`; that is stated
//! in the README rather than papered over with a hash of every file.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ignore::WalkBuilder;
use serde::Serialize;

use crate::filters;
use crate::options::Options;
use crate::paths::relative_display;
use crate::symbols::{self, Symbol};

/// Why files were left out of the index, so `status` can explain itself
/// instead of silently returning less than the caller expected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SkipCounts {
    /// Name or content matched a credential heuristic.
    pub secret: u64,
    /// Minified bundle, source map, or an over-long single line.
    pub generated: u64,
    /// Larger than `max_file_bytes`.
    pub too_large: u64,
    /// NUL bytes or invalid UTF-8.
    pub binary: u64,
    /// Present but unreadable — permissions, or a race with a writer.
    pub unreadable: u64,
}

#[derive(Debug, Clone)]
pub struct FileRecord {
    /// Root-relative, forward slashes, on every platform.
    pub relative: String,
    pub size: u64,
    /// Nanoseconds since the Unix epoch, or 0 when the platform would not say.
    pub modified_nanos: u128,
    pub lines: u32,
    pub symbols: Vec<Symbol>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RefreshReport {
    pub added: u64,
    pub updated: u64,
    pub removed: u64,
    pub unchanged: u64,
    pub skipped: SkipCounts,
    pub duration_ms: u64,
}

#[derive(Debug, Default)]
pub struct Index {
    files: BTreeMap<String, FileRecord>,
    skipped: SkipCounts,
    last_refresh: Option<Instant>,
    last_refresh_unix: Option<u64>,
}

impl Index {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn files(&self) -> impl Iterator<Item = &FileRecord> {
        self.files.values()
    }

    pub fn get(&self, relative: &str) -> Option<&FileRecord> {
        self.files.get(relative)
    }

    pub fn file_count(&self) -> u64 {
        self.files.len() as u64
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.values().map(|record| record.size).sum()
    }

    pub fn total_lines(&self) -> u64 {
        self.files
            .values()
            .map(|record| u64::from(record.lines))
            .sum()
    }

    pub fn symbol_count(&self) -> u64 {
        self.files
            .values()
            .map(|record| record.symbols.len() as u64)
            .sum()
    }

    pub fn skipped(&self) -> SkipCounts {
        self.skipped
    }

    pub fn last_refresh_unix(&self) -> Option<u64> {
        self.last_refresh_unix
    }

    /// True when the index has never been built, or is older than
    /// `refresh_secs`.
    pub fn is_stale(&self, refresh_secs: u64) -> bool {
        match self.last_refresh {
            None => true,
            Some(instant) => instant.elapsed().as_secs() >= refresh_secs,
        }
    }

    /// Re-walk the root and bring the inventory back in line with the disk.
    ///
    /// With `force = false` an unchanged file costs one `stat`. With
    /// `force = true` every surviving file is reread and reparsed, which is the
    /// escape hatch for an editor that preserved size and mtime.
    pub fn refresh(&mut self, root: &Path, options: &Options, force: bool) -> RefreshReport {
        let started = Instant::now();
        let mut report = RefreshReport::default();
        let mut next: BTreeMap<String, FileRecord> = BTreeMap::new();

        for entry in walker(root, options).build() {
            let Ok(entry) = entry else {
                // A directory that vanished mid-walk, or one we may not read.
                report.skipped.unreadable += 1;
                continue;
            };
            // `follow_links(false)` means a symlink reports as a symlink, not
            // as its target, so this also excludes every link — the indexer
            // never leaves the root even before path resolution gets a say.
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }

            let Some(relative) = relative_display(root, entry.path()) else {
                continue;
            };
            let name = relative.rsplit('/').next().unwrap_or(relative.as_str());

            if filters::is_secret_path(&relative) {
                report.skipped.secret += 1;
                continue;
            }
            if filters::is_generated_file_name(name) {
                report.skipped.generated += 1;
                continue;
            }

            let Ok(metadata) = entry.metadata() else {
                report.skipped.unreadable += 1;
                continue;
            };
            if metadata.len() > options.max_file_bytes {
                report.skipped.too_large += 1;
                continue;
            }
            let modified_nanos = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);

            // The incremental fast path: same size, same mtime, keep the
            // parsed record and never touch the file's contents.
            if !force
                && modified_nanos != 0
                && let Some(existing) = self.files.get(&relative)
                && existing.size == metadata.len()
                && existing.modified_nanos == modified_nanos
            {
                report.unchanged += 1;
                next.insert(relative.clone(), existing.clone());
                continue;
            }

            match read_indexable(entry.path()) {
                Ok(Some(text)) => {
                    let record = FileRecord {
                        symbols: symbols::extract(&relative, &text),
                        lines: text.lines().count() as u32,
                        relative: relative.clone(),
                        size: metadata.len(),
                        modified_nanos,
                    };
                    if self.files.contains_key(&relative) {
                        report.updated += 1;
                    } else {
                        report.added += 1;
                    }
                    next.insert(relative, record);
                }
                Ok(None) => report.skipped.binary += 1,
                Err(SkipReason::Secret) => report.skipped.secret += 1,
                Err(SkipReason::Generated) => report.skipped.generated += 1,
                Err(SkipReason::Unreadable) => report.skipped.unreadable += 1,
            }
        }

        report.removed = self
            .files
            .keys()
            .filter(|relative| !next.contains_key(*relative))
            .count() as u64;

        self.files = next;
        self.skipped = report.skipped;
        self.last_refresh = Some(Instant::now());
        self.last_refresh_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|elapsed| elapsed.as_secs());
        report.duration_ms = started.elapsed().as_millis() as u64;
        report
    }
}

enum SkipReason {
    Secret,
    Generated,
    Unreadable,
}

/// Read a candidate file, returning `Ok(None)` when it is binary.
///
/// The content-level secret check happens here rather than in the walker
/// because it needs the bytes: a file called `deploy.tf` holding a PEM block
/// passes every name heuristic there is.
fn read_indexable(path: &Path) -> Result<Option<String>, SkipReason> {
    let bytes = std::fs::read(path).map_err(|_| SkipReason::Unreadable)?;
    if filters::looks_binary(&bytes) {
        return Ok(None);
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(None);
    };
    if filters::contains_private_key_block(&text) {
        return Err(SkipReason::Secret);
    }
    if filters::looks_minified(&text) {
        return Err(SkipReason::Generated);
    }
    Ok(Some(text))
}

/// The directory walk.
///
/// `.gitignore`, `.ignore`, `.git/info/exclude` and the user's global gitignore
/// are all honoured by the `ignore` crate — the same implementation ripgrep
/// uses — rather than reimplemented here. `require_git(false)` makes a plain
/// directory with a `.gitignore` behave like a checkout, which is what an
/// operator pointing this at an exported tree expects.
fn walker(root: &Path, options: &Options) -> WalkBuilder {
    let include_vendored = options.include_vendored;
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!options.include_hidden)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .require_git(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            // depth 0 is the root itself; a root that happens to be called
            // `target` is still the root.
            if entry.depth() == 0 || !entry.file_type().is_some_and(|kind| kind.is_dir()) {
                return true;
            }
            let Some(name) = entry.file_name().to_str() else {
                return false;
            };
            if filters::is_version_control_directory(name) || filters::is_secret_directory(name) {
                return false;
            }
            include_vendored || !filters::is_vendored_directory(name)
        });
    builder
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::TempTree;
    use std::path::PathBuf;

    fn options_for(root: &Path) -> Options {
        Options {
            root: root.to_path_buf(),
            max_file_bytes: 64 * 1024,
            refresh_secs: 0,
            include_hidden: false,
            include_vendored: false,
        }
    }

    fn indexed_paths(index: &Index) -> Vec<String> {
        index.files().map(|file| file.relative.clone()).collect()
    }

    fn build(tree: &TempTree) -> (PathBuf, Options, Index, RefreshReport) {
        let root = tree.canonical_root();
        let options = options_for(&root);
        let mut index = Index::new();
        let report = index.refresh(&root, &options, false);
        (root, options, index, report)
    }

    #[test]
    fn ordinary_source_files_are_indexed_with_their_symbols() {
        let tree = TempTree::new("index-basic");
        tree.write("src/main.rs", "pub fn main() {}\nstruct Config;\n");
        tree.write("README.md", "# Title\n");

        let (_root, _options, index, report) = build(&tree);

        assert_eq!(indexed_paths(&index), vec!["README.md", "src/main.rs"]);
        assert_eq!(report.added, 2);
        assert_eq!(report.unchanged, 0);
        let main = index.get("src/main.rs").expect("indexed");
        assert_eq!(main.lines, 2);
        assert_eq!(
            main.symbols
                .iter()
                .map(|symbol| symbol.name.as_str())
                .collect::<Vec<_>>(),
            vec!["main", "Config"]
        );
    }

    #[test]
    fn gitignored_and_vendored_trees_stay_out() {
        let tree = TempTree::new("index-ignore");
        tree.write(".gitignore", "ignored.rs\nbuilt/\n");
        tree.write("src/kept.rs", "fn kept() {}\n");
        tree.write("src/ignored.rs", "fn ignored() {}\n");
        tree.write("built/artifact.rs", "fn artifact() {}\n");
        tree.write("node_modules/left-pad/index.js", "module.exports = 1\n");
        tree.write("target/debug/build.rs", "fn build() {}\n");

        let (_root, _options, index, _report) = build(&tree);

        assert_eq!(indexed_paths(&index), vec!["src/kept.rs"]);
    }

    #[test]
    fn credential_shaped_files_never_enter_the_index() {
        let tree = TempTree::new("index-secrets");
        tree.write("src/main.rs", "fn main() {}\n");
        tree.write("config/.env", "API_TOKEN=hunter2\n");
        tree.write("config/service.pem", "not really a key\n");
        tree.write(
            "config/deploy.tf",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEow==\n-----END RSA PRIVATE KEY-----\n",
        );

        // Hidden files are walked here on purpose: skipping `.env` because it
        // starts with a dot would prove nothing about the secret filter.
        let root = tree.canonical_root();
        let options = Options {
            include_hidden: true,
            ..options_for(&root)
        };
        let mut index = Index::new();
        let report = index.refresh(&root, &options, false);

        assert_eq!(indexed_paths(&index), vec!["src/main.rs"]);
        // `.env` and `service.pem` by name, `deploy.tf` by its contents.
        assert_eq!(report.skipped.secret, 3);
        assert!(index.get("config/.env").is_none());
    }

    #[test]
    fn binary_oversized_and_minified_files_are_counted_not_indexed() {
        let tree = TempTree::new("index-shape");
        tree.write("src/main.rs", "fn main() {}\n");
        tree.write_bytes("assets/logo.png", b"\x89PNG\r\n\x1a\n\x00\x00");
        tree.write("assets/huge.txt", &"x".repeat(70 * 1024));
        tree.write("assets/app.js", &format!("var a={};\n", "1".repeat(3000)));
        tree.write("assets/app.min.js", "var a=1\n");

        let (_root, _options, index, report) = build(&tree);

        assert_eq!(indexed_paths(&index), vec!["src/main.rs"]);
        assert_eq!(report.skipped.binary, 1);
        assert_eq!(report.skipped.too_large, 1);
        // The over-long line and the `.min.js` name.
        assert_eq!(report.skipped.generated, 2);
    }

    #[test]
    fn a_second_refresh_reuses_unchanged_records_and_notices_the_rest() {
        let tree = TempTree::new("index-incremental");
        tree.write("src/a.rs", "fn a() {}\n");
        tree.write("src/b.rs", "fn b() {}\n");

        let (root, options, mut index, first) = build(&tree);
        assert_eq!(first.added, 2);

        // Nothing touched: both files take the stat-only path.
        let second = index.refresh(&root, &options, false);
        assert_eq!(second.unchanged, 2);
        assert_eq!(second.added, 0);
        assert_eq!(second.updated, 0);
        assert_eq!(second.removed, 0);

        // One file rewritten with different content and size, one added, one
        // deleted.
        tree.write("src/a.rs", "fn a() {}\nfn a2() {}\nfn a3() {}\n");
        tree.write("src/c.rs", "fn c() {}\n");
        std::fs::remove_file(root.join("src").join("b.rs")).expect("remove b.rs");

        let third = index.refresh(&root, &options, false);
        assert_eq!(third.updated, 1, "a.rs changed");
        assert_eq!(third.added, 1, "c.rs is new");
        assert_eq!(third.removed, 1, "b.rs is gone");
        assert_eq!(indexed_paths(&index), vec!["src/a.rs", "src/c.rs"]);
        assert_eq!(index.get("src/a.rs").expect("a.rs").symbols.len(), 3);
    }

    #[test]
    fn a_forced_refresh_reparses_everything() {
        let tree = TempTree::new("index-force");
        tree.write("src/a.rs", "fn a() {}\n");

        let (root, options, mut index, _first) = build(&tree);
        let forced = index.refresh(&root, &options, true);

        assert_eq!(forced.unchanged, 0);
        assert_eq!(forced.updated, 1);
    }

    /// The one test that runs against a real, messy tree rather than a
    /// fixture: this crate's own directory, `target/` and all.
    #[test]
    fn indexing_this_crate_finds_its_source_and_skips_its_build_output() {
        let root = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).expect("crate root");
        let options = Options {
            max_file_bytes: 1024 * 1024,
            ..options_for(&root)
        };
        let mut index = Index::new();
        index.refresh(&root, &options, false);

        assert!(
            index.get("src/main.rs").is_some(),
            "the plugin cannot index itself"
        );
        assert!(
            index
                .files()
                .all(|record| !record.relative.starts_with("target/")),
            "build output leaked into the index"
        );
        // Proof that the private-key heuristic anchors on the line start: this
        // module holds the PEM marker as a string literal.
        assert!(
            index.get("src/filters.rs").is_some(),
            "a source file that merely mentions a PEM header must stay indexable"
        );
    }

    #[test]
    fn hidden_files_are_skipped_unless_asked_for() {
        let tree = TempTree::new("index-hidden");
        tree.write("src/main.rs", "fn main() {}\n");
        tree.write(".github/workflows/ci.yml", "name: ci\n");

        let (root, mut options, mut index, _report) = build(&tree);
        assert_eq!(indexed_paths(&index), vec!["src/main.rs"]);

        options.include_hidden = true;
        index.refresh(&root, &options, false);
        assert_eq!(
            indexed_paths(&index),
            vec![".github/workflows/ci.yml", "src/main.rs"]
        );
    }
}
