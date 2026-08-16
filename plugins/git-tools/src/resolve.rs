//! Turning a validated [`Revision`] into a real object, with errors a caller
//! can act on.
//!
//! Every function here takes a [`Revision`] rather than a `&str`, which is how
//! "no unvalidated revision reaches libgit2" stays true without an audit: there
//! is no other way to construct one. See [`crate::guard`].

use std::path::Path;

use git2::{Blob, Commit, ObjectType, Repository, Tree};
use tdcc_plugin::{PluginError, PluginResult};

use crate::guard::{Revision, TreePath, parse_revision};

/// The revision every tool falls back to when the caller names none.
pub const DEFAULT_REVISION: &str = "HEAD";

/// Validate an optional caller-supplied revision, defaulting to `HEAD`.
pub fn revision_or_head(input: Option<&str>) -> PluginResult<Revision> {
    let raw = input.unwrap_or(DEFAULT_REVISION);
    parse_revision(raw).map_err(|error| PluginError::invalid_params(error.to_string()))
}

/// Validate a required caller-supplied revision.
pub fn required_revision(input: &str, field: &str) -> PluginResult<Revision> {
    parse_revision(input).map_err(|error| PluginError::invalid_params(format!("{field}: {error}")))
}

/// Resolve a revision to the commit it names.
///
/// A revision that resolves to a tag is peeled through to the commit, so
/// `v1.4.0` works whether the tag is annotated or lightweight. A revision that
/// names a tree or a blob is an error rather than a silent nearest-commit
/// guess, because "show me this blob" and "show me the commit that made it" are
/// different questions and answering the wrong one looks like success.
pub fn resolve_commit<'repo>(
    repository: &'repo Repository,
    revision: &Revision,
) -> PluginResult<Commit<'repo>> {
    let object = repository.revparse_single(revision.as_str()).map_err(|_| {
        if revision.as_str() == DEFAULT_REVISION && repository.is_empty().unwrap_or(false) {
            return PluginError::invalid_request(
                "this repository has no commits yet, so there is no history to read",
            );
        }
        PluginError::invalid_params(format!(
            "revision {revision:?} does not exist in this repository. Call refs to list the \
             branches and tags it does have"
        ))
    })?;

    let kind = object.kind();
    object.peel_to_commit().map_err(|_| {
        let named = match kind {
            Some(ObjectType::Tree) => "a tree",
            Some(ObjectType::Blob) => "a blob",
            Some(other) => match other {
                ObjectType::Tag => "an unpeelable tag",
                _ => "a non-commit object",
            },
            None => "an object of unknown type",
        };
        PluginError::invalid_params(format!("revision {revision:?} names {named}, not a commit"))
    })
}

/// The tree of a commit, with the error mapped into the plugin's shape.
pub fn commit_tree<'repo>(commit: &Commit<'repo>) -> PluginResult<Tree<'repo>> {
    commit
        .tree()
        .map_err(|error| PluginError::internal(format!("commit tree could not be read: {error}")))
}

/// Look up one file inside a commit.
///
/// Returns a clear error for the two cases a caller most often hits: the path
/// is not in that commit at all, and the path is a directory rather than a
/// file.
pub fn blob_at<'repo>(
    repository: &'repo Repository,
    commit: &Commit<'repo>,
    path: &TreePath,
) -> PluginResult<Blob<'repo>> {
    let tree = commit_tree(commit)?;
    let entry = tree.get_path(Path::new(path.as_str())).map_err(|_| {
        PluginError::invalid_params(format!(
            "{path:?} does not exist at that revision. It may have been added later, deleted \
             earlier, or renamed; call log with this path to find out which"
        ))
    })?;

    match entry.kind() {
        Some(ObjectType::Blob) => {}
        Some(ObjectType::Tree) => {
            return Err(PluginError::invalid_params(format!(
                "{path:?} is a directory at that revision, and this tool reads one file"
            )));
        }
        Some(ObjectType::Commit) => {
            return Err(PluginError::invalid_params(format!(
                "{path:?} is a submodule reference, whose content lives in another repository \
                 this plugin is not configured to read"
            )));
        }
        _ => {
            return Err(PluginError::invalid_params(format!(
                "{path:?} is not a regular file at that revision"
            )));
        }
    }

    entry
        .to_object(repository)
        .and_then(|object| object.peel_to_blob())
        .map_err(|error| PluginError::internal(format!("file content could not be read: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::parse_tree_path;
    use crate::testsupport::TempTree;

    #[test]
    fn head_is_the_default_and_an_explicit_revision_wins() {
        assert_eq!(revision_or_head(None).expect("default").as_str(), "HEAD");
        assert_eq!(
            revision_or_head(Some("v1.0.0")).expect("explicit").as_str(),
            "v1.0.0"
        );
    }

    #[test]
    fn a_hostile_revision_is_refused_before_it_reaches_git() {
        let error = revision_or_head(Some("--upload-pack=x")).expect_err("refused");
        assert!(format!("{error:?}").contains("must not start with '-'"));
    }

    #[test]
    fn a_required_revision_names_its_field_in_the_error() {
        let error = required_revision("HEAD:secret", "from_rev").expect_err("refused");
        assert!(format!("{error:?}").contains("from_rev"));
    }

    #[test]
    fn a_branch_a_tag_and_an_oid_all_resolve_to_the_same_commit() {
        let tree = TempTree::new("resolve-commit");
        let fixture = tree.repository("repo");
        fixture.write("a.txt", "one\n");
        let head = fixture.commit("only");
        fixture.branch("topic");
        fixture.tag_annotated("v1.0.0", "first release");
        fixture.tag_light("v1.0.0-light");

        let repository = fixture.repository();
        for spec in ["HEAD", "topic", "v1.0.0", "v1.0.0-light", &head.to_string()] {
            let revision = parse_revision(spec).expect("valid");
            let commit = resolve_commit(repository, &revision).expect("resolves");
            assert_eq!(commit.id(), head, "spec {spec:?}");
        }
    }

    #[test]
    fn an_unknown_revision_points_the_caller_at_refs() {
        let tree = TempTree::new("resolve-unknown");
        let fixture = tree.repository("repo");
        fixture.write("a.txt", "one\n");
        fixture.commit("only");

        let revision = parse_revision("v9.9.9").expect("valid shape");
        let error = resolve_commit(fixture.repository(), &revision).expect_err("no such tag");
        let message = format!("{error:?}");
        assert!(message.contains("v9.9.9"), "{message}");
        assert!(message.contains("refs"), "{message}");
    }

    #[test]
    fn an_empty_repository_says_so_rather_than_reporting_a_missing_ref() {
        let tree = TempTree::new("resolve-empty");
        let fixture = tree.repository("repo");

        let revision = parse_revision("HEAD").expect("valid");
        let error = resolve_commit(fixture.repository(), &revision).expect_err("no commits");
        let message = format!("{error:?}");
        assert!(message.contains("no commits yet"), "{message}");
    }

    #[test]
    fn a_file_is_read_at_the_revision_that_contained_it() {
        let tree = TempTree::new("resolve-blob");
        let fixture = tree.repository("repo");
        fixture.write("src/main.rs", "fn main() {}\n");
        fixture.commit("first");
        fixture.write("src/main.rs", "fn main() { println!(); }\n");
        fixture.commit("second");

        let repository = fixture.repository();
        let path = parse_tree_path("src/main.rs").expect("valid");

        let head =
            resolve_commit(repository, &parse_revision("HEAD").expect("valid")).expect("resolves");
        let blob = blob_at(repository, &head, &path).expect("present");
        assert_eq!(blob.content(), b"fn main() { println!(); }\n");

        let previous = resolve_commit(repository, &parse_revision("HEAD~1").expect("valid"))
            .expect("resolves");
        let older = blob_at(repository, &previous, &path).expect("present");
        assert_eq!(older.content(), b"fn main() {}\n");
    }

    #[test]
    fn a_missing_path_and_a_directory_are_distinguished() {
        let tree = TempTree::new("resolve-blob-errors");
        let fixture = tree.repository("repo");
        fixture.write("src/main.rs", "fn main() {}\n");
        fixture.commit("first");

        let repository = fixture.repository();
        let head =
            resolve_commit(repository, &parse_revision("HEAD").expect("valid")).expect("resolves");

        let missing = blob_at(
            repository,
            &head,
            &parse_tree_path("src/nope.rs").expect("valid"),
        )
        .expect_err("not there");
        assert!(format!("{missing:?}").contains("does not exist at that revision"));

        let directory = blob_at(repository, &head, &parse_tree_path("src").expect("valid"))
            .expect_err("a directory");
        assert!(format!("{directory:?}").contains("is a directory"));
    }
}
