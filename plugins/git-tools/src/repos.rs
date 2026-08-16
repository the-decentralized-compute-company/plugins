//! Repository confinement: the security core of this plugin.
//!
//! An operator lists repositories as `--repo <alias>=<path>`. A caller never
//! supplies a path — only an alias — so the set of repositories reachable
//! through this plugin is fixed at launch and cannot be widened by anything a
//! model types.
//!
//! Each configured path is resolved through [`open_confined`], which enforces
//! three separate rules. All three are checked at startup *and* again on every
//! call, because a repository can be edited between the two:
//!
//! 1. **The configured path must itself be the repository.** libgit2 is asked
//!    to open with `NO_SEARCH`, so pointing at `/srv/repo/src` fails rather
//!    than silently opening `/srv/repo`. An operator who meant the parent
//!    should have to say so.
//! 2. **The working tree must be the configured path.** A repository's
//!    `.git/config` may set `core.worktree` to an arbitrary directory, which
//!    would make `repo_status` read files the operator never listed. The
//!    canonical working tree is compared against the canonical configured root
//!    and a mismatch is refused.
//! 3. **The git directory must live inside the configured path.** A `.git`
//!    *file* containing `gitdir: …` — what `git worktree add` and submodules
//!    produce — points the object store somewhere else entirely. Refused for
//!    the same reason.
//!
//! Containment is compared component-wise on canonical paths, so a sibling
//! named `/srv/repo-backup` does not count as being inside `/srv/repo` the way
//! a textual prefix check would say it does. Canonicalization is what makes a
//! symlink or a Windows junction unable to help: the resolved path is what gets
//! compared, not the path the caller or the config file wrote.

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};

use git2::{Repository, RepositoryOpenFlags};
use tdcc_plugin::{PluginError, PluginResult};

use crate::settings::{Disclosure, Limits, RepoSpec, validate_alias};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfinementError {
    /// The configured path could not be canonicalized: it does not exist, or
    /// the process cannot traverse to it.
    Unresolvable(String),
    /// The configured path exists but is not a directory.
    NotADirectory,
    /// libgit2 declined to open it. `raw` carries libgit2's own message, which
    /// may contain absolute paths and is therefore only ever logged at startup,
    /// never returned in a tool response.
    NotARepository { code: String, raw: String },
    /// `core.worktree` (or an equivalent redirection) points the working tree
    /// somewhere other than the configured root.
    WorktreeRedirected,
    /// The git directory resolves outside the configured root — the linked
    /// worktree and submodule case.
    GitdirOutside,
}

impl fmt::Display for ConfinementError {
    /// Deliberately path-free.
    ///
    /// These strings reach a model through tool responses, and telling it where
    /// a contributor's repositories live on disk is a disclosure this plugin
    /// has no reason to make. The operator gets the path from their own
    /// `config.toml` and from the one startup line on stderr.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unresolvable(reason) => write!(
                formatter,
                "the configured path could not be resolved ({reason}); it may have been moved or \
                 deleted since the plugin started"
            ),
            Self::NotADirectory => {
                write!(formatter, "the configured path is not a directory")
            }
            Self::NotARepository { code, .. } => write!(
                formatter,
                "the configured path is not a git repository libgit2 will open ({code}). Point \
                 --repo at the repository root itself, not at a subdirectory of it"
            ),
            Self::WorktreeRedirected => write!(
                formatter,
                "this repository redirects its working tree outside the configured root \
                 (core.worktree), and was refused"
            ),
            Self::GitdirOutside => write!(
                formatter,
                "this repository's git directory resolves outside the configured root, which is \
                 what a linked worktree or a submodule looks like. Configure the main repository \
                 instead"
            ),
        }
    }
}

impl std::error::Error for ConfinementError {}

impl ConfinementError {
    /// libgit2's own message, for the operator's log only.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::NotARepository { raw, .. } => Some(raw),
            _ => None,
        }
    }
}

/// Component-wise containment test.
///
/// Component-wise, not string-prefix: `/srv/repo-backup` must not count as
/// being inside `/srv/repo`, and a textual `starts_with` would say it is. Both
/// arguments must already be canonical for this to mean anything.
pub fn is_within(root: &Path, candidate: &Path) -> bool {
    candidate.starts_with(root)
}

/// Render a canonical path for a human to read.
///
/// Windows canonicalization returns verbatim paths (`\\?\C:\src\repo`). The
/// prefix is meaningful to the OS and noise in a startup log, so it is stripped
/// here and nowhere else — every comparison keeps using the real canonical
/// path.
pub fn display_path(path: &Path) -> String {
    let rendered = path.display().to_string();
    rendered
        .strip_prefix(r"\\?\")
        .unwrap_or(rendered.as_str())
        .to_string()
}

/// Open the repository at `configured`, proving it is the repository the
/// operator named and that nothing about it reaches outside that directory.
///
/// Returns the opened repository together with its canonical root, so callers
/// do not canonicalize twice.
pub fn open_confined(configured: &Path) -> Result<(Repository, PathBuf), ConfinementError> {
    let root = std::fs::canonicalize(configured)
        .map_err(|error| ConfinementError::Unresolvable(error.kind().to_string()))?;
    if !root.is_dir() {
        return Err(ConfinementError::NotADirectory);
    }

    // NO_SEARCH: opening `/srv/repo/src` must fail rather than quietly walking
    // up to `/srv/repo`. No ceiling directories are needed because no search
    // happens at all.
    let repository = Repository::open_ext(
        &root,
        RepositoryOpenFlags::NO_SEARCH,
        std::iter::empty::<&OsStr>(),
    )
    .map_err(|error| ConfinementError::NotARepository {
        code: format!("{:?}", error.code()),
        raw: error.message().to_string(),
    })?;

    if let Some(workdir) = repository.workdir() {
        let workdir = std::fs::canonicalize(workdir)
            .map_err(|error| ConfinementError::Unresolvable(error.kind().to_string()))?;
        if workdir != root {
            return Err(ConfinementError::WorktreeRedirected);
        }
    }

    let gitdir = std::fs::canonicalize(repository.path())
        .map_err(|error| ConfinementError::Unresolvable(error.kind().to_string()))?;
    if !is_within(&root, &gitdir) {
        return Err(ConfinementError::GitdirOutside);
    }

    Ok((repository, root))
}

/// A repository that passed [`open_confined`] at startup.
#[derive(Debug, Clone)]
pub struct ResolvedRepo {
    pub alias: String,
    /// Canonical root. Never included in a tool response.
    pub root: PathBuf,
    pub bare: bool,
}

/// Everything the tool handlers need, and nothing they do not.
///
/// Holds paths and limits only — no open `git2::Repository`, which is not
/// `Sync` and whose object cache would otherwise be shared across concurrent
/// calls. Every handler opens the repository itself, inside `spawn_blocking`,
/// and re-runs the confinement checks while it does.
#[derive(Debug)]
pub struct Registry {
    repositories: Vec<ResolvedRepo>,
    problems: Vec<StartupProblem>,
    limits: Limits,
    disclosure: Disclosure,
}

/// One repository that failed to resolve at startup, kept so `status` can
/// report it instead of the plugin silently pretending it does not exist.
#[derive(Debug, Clone)]
pub struct StartupProblem {
    pub alias: String,
    pub configured: PathBuf,
    pub error: ConfinementError,
}

impl Registry {
    /// Resolve every configured repository.
    ///
    /// A repository that fails resolution is *not* fatal: the plugin starts
    /// with the ones that worked and keeps the rest as [`StartupProblem`]s so
    /// `status` can report them. An operator with four repositories should not
    /// lose all four because one disk is unmounted. Whether *no* repository
    /// resolving is fatal is decided in `main.rs`, which is where the process
    /// can still exit with a message an operator will read.
    pub fn resolve(specs: &[RepoSpec], limits: Limits, disclosure: Disclosure) -> Self {
        let mut repositories = Vec::new();
        let mut problems = Vec::new();

        for spec in specs {
            match open_confined(&spec.path) {
                Ok((repository, root)) => repositories.push(ResolvedRepo {
                    alias: spec.alias.clone(),
                    bare: repository.is_bare(),
                    root,
                }),
                Err(error) => problems.push(StartupProblem {
                    alias: spec.alias.clone(),
                    configured: spec.path.clone(),
                    error,
                }),
            }
        }

        Self {
            repositories,
            problems,
            limits,
            disclosure,
        }
    }

    /// An empty registry, used only by `--print-package-manifest`, which must
    /// work on a machine that has no repositories configured at all.
    pub fn for_manifest_only() -> Self {
        Self {
            repositories: Vec::new(),
            problems: Vec::new(),
            limits: Limits::default(),
            disclosure: Disclosure::default(),
        }
    }

    /// Repositories the operator configured that could not be opened.
    pub fn problems(&self) -> &[StartupProblem] {
        &self.problems
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub fn disclosure(&self) -> Disclosure {
        self.disclosure
    }

    pub fn repositories(&self) -> &[ResolvedRepo] {
        &self.repositories
    }

    pub fn aliases(&self) -> Vec<&str> {
        self.repositories
            .iter()
            .map(|repository| repository.alias.as_str())
            .collect()
    }

    /// Pick the repository a call refers to.
    ///
    /// Omitting `repo` is allowed only when exactly one repository is
    /// configured. With several, the ambiguity is an error listing the choices
    /// rather than a guess — picking the first one silently is how a model
    /// ends up confidently answering about the wrong codebase.
    pub fn select(&self, alias: Option<&str>) -> PluginResult<&ResolvedRepo> {
        if self.repositories.is_empty() {
            return Err(PluginError::invalid_request(
                "no repository is available: every configured --repo failed to resolve at \
                 startup. Call status for the reason.",
            ));
        }

        let Some(alias) = alias else {
            if self.repositories.len() == 1 {
                return Ok(&self.repositories[0]);
            }
            return Err(PluginError::invalid_params(format!(
                "several repositories are configured, so 'repo' is required. Choose one of: {}",
                self.aliases().join(", ")
            )));
        };

        // Validate before echoing: an alias is caller-supplied text and ends up
        // in this error message.
        validate_alias(alias).map_err(PluginError::invalid_params)?;

        self.repositories
            .iter()
            .find(|repository| repository.alias == alias)
            .ok_or_else(|| {
                PluginError::invalid_params(format!(
                    "unknown repository {alias:?}. Configured repositories: {}",
                    self.aliases().join(", ")
                ))
            })
    }

    /// Open a repository for one call, re-running every confinement check.
    ///
    /// Startup validation is not enough on its own: a repository can be
    /// replaced by a symlink, gain a `core.worktree`, or be deleted while the
    /// plugin is running, and the check that matters is the one that ran
    /// immediately before the read.
    pub fn open(&self, repository: &ResolvedRepo) -> PluginResult<Repository> {
        let (handle, root) = open_confined(&repository.root).map_err(|error| {
            PluginError::invalid_request(format!(
                "repository {:?} is not readable: {error}",
                repository.alias
            ))
        })?;
        // The root was canonical when it was stored, so anything other than an
        // exact match means the path now resolves somewhere else.
        if root != repository.root {
            return Err(PluginError::invalid_request(format!(
                "repository {:?} now resolves to a different directory than it did at startup and \
                 was refused",
                repository.alias
            )));
        }
        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::TempTree;

    /// `open_confined` returns an opened `Repository`, which libgit2 does not
    /// make `Debug`, so `expect_err` cannot render it. This unwraps the error
    /// side without asking the compiler for a `Debug` it will never have.
    fn refusal(result: Result<(Repository, PathBuf), ConfinementError>) -> ConfinementError {
        match result {
            Ok((_repository, root)) => {
                panic!(
                    "expected a refusal, got a repository at {}",
                    display_path(&root)
                )
            }
            Err(error) => error,
        }
    }

    #[test]
    fn containment_is_component_wise_not_textual() {
        let root = Path::new("/srv/repo");
        assert!(is_within(root, Path::new("/srv/repo")));
        assert!(is_within(root, Path::new("/srv/repo/.git")));
        // The bug a textual prefix check would have.
        assert!(!is_within(root, Path::new("/srv/repo-backup/.git")));
        assert!(!is_within(root, Path::new("/srv/other")));
        assert!(!is_within(root, Path::new("/")));
    }

    #[test]
    fn the_verbatim_prefix_is_stripped_only_for_display() {
        assert_eq!(display_path(Path::new(r"\\?\C:\src\repo")), r"C:\src\repo");
        assert_eq!(display_path(Path::new("/srv/repo")), "/srv/repo");
    }

    #[test]
    fn a_plain_repository_opens_and_reports_its_canonical_root() {
        let tree = TempTree::new("confine-plain");
        let fixture = tree.repository("repo");

        let (repository, root) = open_confined(fixture.root()).expect("a real repository opens");
        assert!(!repository.is_bare());
        assert_eq!(root, std::fs::canonicalize(fixture.root()).expect("canon"));
    }

    #[test]
    fn a_missing_path_is_unresolvable_rather_than_a_panic() {
        let tree = TempTree::new("confine-missing");
        let error = refusal(open_confined(&tree.path().join("nope")));
        assert!(
            matches!(error, ConfinementError::Unresolvable(_)),
            "{error}"
        );
        assert!(!error.to_string().contains("nope"), "{error}");
    }

    #[test]
    fn a_directory_that_is_not_a_repository_is_refused() {
        let tree = TempTree::new("confine-not-a-repo");
        std::fs::create_dir_all(tree.path().join("plain")).expect("mkdir");
        let error = refusal(open_confined(&tree.path().join("plain")));
        assert!(
            matches!(error, ConfinementError::NotARepository { .. }),
            "{error}"
        );
    }

    /// `NO_SEARCH` in action. Without it libgit2 walks up from `src/` and opens
    /// the parent repository, which would quietly widen the operator's
    /// configuration to a directory they did not name.
    #[test]
    fn a_subdirectory_of_a_repository_does_not_open_the_repository() {
        let tree = TempTree::new("confine-no-search");
        let fixture = tree.repository("repo");
        fixture.write("src/main.rs", "fn main() {}\n");
        fixture.commit("initial");

        let error = refusal(open_confined(&fixture.root().join("src")));
        assert!(
            matches!(error, ConfinementError::NotARepository { .. }),
            "{error}"
        );
    }

    /// A repository whose config redirects the working tree elsewhere would let
    /// `repo_status` read a directory the operator never listed.
    #[test]
    fn a_redirected_working_tree_is_refused() {
        let tree = TempTree::new("confine-worktree");
        let fixture = tree.repository("repo");
        fixture.write("inside.txt", "in\n");
        fixture.commit("initial");

        let elsewhere = tree.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("mkdir");
        std::fs::write(elsewhere.join("secret.txt"), "a password lives here\n").expect("write");

        // Prove the redirection is real before asserting it is refused.
        fixture.set_config(
            "core.worktree",
            &display_path(&std::fs::canonicalize(&elsewhere).expect("canon")),
        );
        let redirected = Repository::open_ext(
            fixture.root(),
            RepositoryOpenFlags::NO_SEARCH,
            std::iter::empty::<&OsStr>(),
        )
        .expect("libgit2 still opens it");
        assert_eq!(
            std::fs::canonicalize(redirected.workdir().expect("a working tree")).expect("canon"),
            std::fs::canonicalize(&elsewhere).expect("canon"),
            "test setup is meaningless unless the redirection actually took effect"
        );
        drop(redirected);

        let error = refusal(open_confined(fixture.root()));
        assert_eq!(error, ConfinementError::WorktreeRedirected);
    }

    /// The `.git`-file case: `git worktree add` and submodules both produce a
    /// directory whose object store lives somewhere else entirely.
    #[test]
    fn a_gitdir_pointing_outside_the_root_is_refused() {
        let tree = TempTree::new("confine-gitlink");
        let real = tree.repository("real");
        real.write("file.txt", "x\n");
        real.commit("initial");

        let linked = tree.path().join("linked");
        std::fs::create_dir_all(&linked).expect("mkdir");
        let gitdir = std::fs::canonicalize(real.root().join(".git")).expect("canon");
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", display_path(&gitdir)),
        )
        .expect("write gitlink");

        // Prove the gitlink resolves before asserting it is refused: a test
        // that passes because libgit2 could not follow the link proves nothing.
        let followed = Repository::open_ext(
            &linked,
            RepositoryOpenFlags::NO_SEARCH,
            std::iter::empty::<&OsStr>(),
        );
        let Ok(followed) = followed else {
            eprintln!("skipping gitlink assertion: libgit2 declined to follow the .git file here");
            return;
        };
        assert_eq!(
            std::fs::canonicalize(followed.path()).expect("canon"),
            gitdir,
            "test setup is meaningless unless the gitlink actually resolves"
        );
        drop(followed);

        let error = refusal(open_confined(&linked));
        assert_eq!(error, ConfinementError::GitdirOutside);
    }

    #[test]
    fn confinement_errors_never_carry_a_filesystem_path() {
        // The rule this pins: a model asking about a repository learns nothing
        // about where a contributor keeps their files.
        let errors = [
            ConfinementError::Unresolvable("entity not found".to_string()),
            ConfinementError::NotADirectory,
            ConfinementError::NotARepository {
                code: "NotFound".to_string(),
                raw: "could not find repository at '/home/ada/secret-project'".to_string(),
            },
            ConfinementError::WorktreeRedirected,
            ConfinementError::GitdirOutside,
        ];
        for error in errors {
            let rendered = error.to_string();
            assert!(!rendered.contains('/'), "{rendered}");
            assert!(!rendered.contains('\\'), "{rendered}");
        }
    }

    #[test]
    fn a_single_repository_may_be_selected_by_omission() {
        let tree = TempTree::new("select-one");
        let fixture = tree.repository("only");
        let registry = Registry::resolve(
            &[RepoSpec {
                alias: "only".to_string(),
                path: fixture.root().to_path_buf(),
            }],
            Limits::default(),
            Disclosure::default(),
        );
        assert!(registry.problems().is_empty(), "{:?}", registry.problems());
        assert_eq!(registry.select(None).expect("the only one").alias, "only");
        assert_eq!(
            registry.select(Some("only")).expect("by name").alias,
            "only"
        );
    }

    #[test]
    fn with_several_repositories_omitting_the_alias_is_an_error_listing_the_choices() {
        let tree = TempTree::new("select-many");
        let one = tree.repository("one");
        let two = tree.repository("two");
        let registry = Registry::resolve(
            &[
                RepoSpec {
                    alias: "one".to_string(),
                    path: one.root().to_path_buf(),
                },
                RepoSpec {
                    alias: "two".to_string(),
                    path: two.root().to_path_buf(),
                },
            ],
            Limits::default(),
            Disclosure::default(),
        );
        assert!(registry.problems().is_empty(), "{:?}", registry.problems());

        let error = registry.select(None).expect_err("ambiguous");
        let message = format!("{error:?}");
        assert!(message.contains("one"), "{message}");
        assert!(message.contains("two"), "{message}");

        let unknown = registry.select(Some("three")).expect_err("no such alias");
        let message = format!("{unknown:?}");
        assert!(message.contains("Configured repositories"), "{message}");
    }

    #[test]
    fn a_traversal_shaped_alias_is_refused_before_it_is_echoed_back() {
        let tree = TempTree::new("select-hostile-alias");
        let fixture = tree.repository("only");
        let registry = Registry::resolve(
            &[RepoSpec {
                alias: "only".to_string(),
                path: fixture.root().to_path_buf(),
            }],
            Limits::default(),
            Disclosure::default(),
        );

        let error = registry
            .select(Some("../../etc/passwd"))
            .expect_err("not an alias");
        let message = format!("{error:?}");
        assert!(message.contains("may only contain"), "{message}");
    }

    #[test]
    fn one_broken_repository_does_not_take_the_working_ones_down_with_it() {
        let tree = TempTree::new("resolve-partial");
        let good = tree.repository("good");
        let registry = Registry::resolve(
            &[
                RepoSpec {
                    alias: "good".to_string(),
                    path: good.root().to_path_buf(),
                },
                RepoSpec {
                    alias: "gone".to_string(),
                    path: tree.path().join("gone"),
                },
            ],
            Limits::default(),
            Disclosure::default(),
        );

        assert_eq!(registry.aliases(), ["good"]);
        assert_eq!(registry.problems().len(), 1);
        assert_eq!(registry.problems()[0].alias, "gone");
    }

    #[test]
    fn opening_for_a_call_re_runs_confinement_and_notices_a_deleted_repository() {
        let tree = TempTree::new("open-per-call");
        let fixture = tree.repository("repo");
        fixture.write("file.txt", "x\n");
        fixture.commit("initial");

        let registry = Registry::resolve(
            &[RepoSpec {
                alias: "repo".to_string(),
                path: fixture.root().to_path_buf(),
            }],
            Limits::default(),
            Disclosure::default(),
        );
        let selected = registry.select(None).expect("selected").clone();
        registry.open(&selected).expect("opens while it exists");

        std::fs::remove_dir_all(fixture.root()).expect("remove the repository under it");

        let error = match registry.open(&selected) {
            Ok(_) => panic!("the repository is gone; opening it must fail"),
            Err(error) => error,
        };
        let message = format!("{error:?}");
        assert!(message.contains("not readable"), "{message}");
    }
}
