//! Finding the PDFs inside the configured roots.
//!
//! A caller cannot extract from a file it cannot name, and this plugin refuses
//! absolute paths, so discovery has to come from somewhere. It comes from here,
//! and what it returns are exactly the strings [`crate::paths::Roots::resolve`]
//! accepts back.
//!
//! The walk is deliberately unadventurous. It never follows a directory
//! symlink — so a link inside a root pointing at the rest of the disk lists
//! nothing rather than everything — and it stops at a depth, an entry count, a
//! result count, and the call's deadline, so a root that turns out to be a
//! home directory does not become an unbounded scan.

use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::budget::Deadline;
use crate::paths::{Resolved, Roots, display_key, join_components};

/// Directory levels below a root that the walk will descend.
const MAX_DEPTH: u32 = 12;

/// Directory entries the walk will look at, across all roots, in one call.
const MAX_ENTRIES_VISITED: u64 = 200_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentEntry {
    /// `<label>/<path>`, ready to hand back to any other tool here.
    pub path: String,
    pub bytes: u64,
    /// Seconds since the Unix epoch, when the filesystem reports one.
    pub modified_unix: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct Listing {
    pub documents: Vec<DocumentEntry>,
    /// A cap or the deadline stopped the walk, so more PDFs exist.
    pub truncated: bool,
    pub directories_scanned: u64,
}

fn is_pdf(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

fn modified_unix(metadata: &std::fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_secs())
}

/// List the PDFs under one root subtree, or under every root.
///
/// `scope` narrows to one already-resolved directory; `name_contains` filters
/// on the file name, case-insensitively.
pub fn list(
    roots: &Roots,
    scope: Option<&Resolved>,
    name_contains: Option<&str>,
    limit: usize,
    deadline: Deadline,
) -> Listing {
    let needle = name_contains.map(|value| value.to_lowercase());
    let mut listing = Listing::default();
    let mut visited = 0u64;

    let starts: Vec<(String, std::path::PathBuf, String)> = match scope {
        Some(resolved) => {
            let root = roots
                .get(&resolved.label)
                .map(|root| root.directory.clone())
                .unwrap_or_else(|| resolved.absolute.clone());
            let relative = resolved
                .absolute
                .strip_prefix(&root)
                .map(join_components)
                .unwrap_or_default();
            vec![(resolved.label.clone(), resolved.absolute.clone(), relative)]
        }
        None => roots
            .iter()
            .map(|root| (root.label.clone(), root.directory.clone(), String::new()))
            .collect(),
    };

    for (label, directory, relative) in starts {
        let mut stack = vec![(directory, relative, 0u32)];
        while let Some((directory, relative, depth)) = stack.pop() {
            if deadline.expired_now() || visited >= MAX_ENTRIES_VISITED {
                listing.truncated = true;
                break;
            }
            let Ok(entries) = std::fs::read_dir(&directory) else {
                // An unreadable directory is skipped rather than failing the
                // whole listing: one permission-denied folder should not hide
                // every other document on the node.
                continue;
            };
            listing.directories_scanned += 1;

            let mut children: Vec<(std::path::PathBuf, String, u32)> = Vec::new();
            for entry in entries.flatten() {
                visited += 1;
                if visited >= MAX_ENTRIES_VISITED {
                    listing.truncated = true;
                    break;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let child_relative = if relative.is_empty() {
                    name.clone()
                } else {
                    format!("{relative}/{name}")
                };
                // `symlink_metadata` rather than `metadata`: a symlink is
                // classified as a symlink here and skipped, so a link out of
                // the root is never walked and never listed.
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                if metadata.is_symlink() {
                    continue;
                }
                if metadata.is_dir() {
                    if depth < MAX_DEPTH {
                        children.push((entry.path(), child_relative, depth + 1));
                    }
                    continue;
                }
                if !metadata.is_file() || !is_pdf(&entry.path()) {
                    continue;
                }
                if let Some(needle) = &needle
                    && !name.to_lowercase().contains(needle.as_str())
                {
                    continue;
                }
                if listing.documents.len() >= limit {
                    listing.truncated = true;
                    continue;
                }
                listing.documents.push(DocumentEntry {
                    path: display_key(&label, &child_relative),
                    bytes: metadata.len(),
                    modified_unix: modified_unix(&metadata),
                });
            }
            // Reversed so the stack pops them in name order, which makes the
            // walk order the same on every run.
            children.sort_by(|left, right| right.1.cmp(&left.1));
            stack.extend(children);
        }
    }

    listing
        .documents
        .sort_by(|left, right| left.path.cmp(&right.path));
    listing
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::RootSpec;
    use crate::testsupport::{TempTree, link_directory, simple_pdf};

    fn roots_for(tree: &TempTree, labels: &[(&str, &str)]) -> Roots {
        Roots::open(
            &labels
                .iter()
                .map(|(label, relative)| RootSpec {
                    label: (*label).to_string(),
                    directory: tree.path().join(relative),
                })
                .collect::<Vec<_>>(),
        )
        .expect("roots open")
    }

    fn paths(listing: &Listing) -> Vec<&str> {
        listing
            .documents
            .iter()
            .map(|entry| entry.path.as_str())
            .collect()
    }

    #[test]
    fn only_pdfs_are_listed_and_their_paths_are_the_ones_tools_accept() {
        let tree = TempTree::new("list-basic");
        tree.write_bytes("docs/q4.pdf", &simple_pdf("hello"));
        tree.write_bytes("docs/reports/annual.PDF", &simple_pdf("hello"));
        tree.write("docs/notes.txt", "not a pdf");
        tree.write("docs/pdf", "not a pdf either");
        let roots = roots_for(&tree, &[("docs", "docs")]);

        let listing = list(&roots, None, None, 100, Deadline::unlimited());

        assert_eq!(
            paths(&listing),
            vec!["docs/q4.pdf", "docs/reports/annual.PDF"]
        );
        assert!(!listing.truncated);
        // And every listed path resolves back through the confinement layer.
        for entry in &listing.documents {
            roots
                .resolve_file(&entry.path)
                .expect("listed paths resolve");
        }
    }

    #[test]
    fn every_root_is_listed_under_its_own_label() {
        let tree = TempTree::new("list-multi");
        tree.write_bytes("a/one.pdf", &simple_pdf("one"));
        tree.write_bytes("b/two.pdf", &simple_pdf("two"));
        let roots = roots_for(&tree, &[("first", "a"), ("second", "b")]);

        let listing = list(&roots, None, None, 100, Deadline::unlimited());

        assert_eq!(paths(&listing), vec!["first/one.pdf", "second/two.pdf"]);
    }

    #[test]
    fn a_scope_narrows_the_walk_to_one_subdirectory() {
        let tree = TempTree::new("list-scope");
        tree.write_bytes("docs/q4.pdf", &simple_pdf("hello"));
        tree.write_bytes("docs/reports/annual.pdf", &simple_pdf("hello"));
        let roots = roots_for(&tree, &[("docs", "docs")]);
        let scope = roots.resolve_directory("docs/reports").expect("scope");

        let listing = list(&roots, Some(&scope), None, 100, Deadline::unlimited());

        assert_eq!(paths(&listing), vec!["docs/reports/annual.pdf"]);
    }

    #[test]
    fn a_name_filter_matches_case_insensitively_on_the_file_name() {
        let tree = TempTree::new("list-filter");
        tree.write_bytes("docs/Quarterly-Report.pdf", &simple_pdf("hello"));
        tree.write_bytes("docs/invoice.pdf", &simple_pdf("hello"));
        let roots = roots_for(&tree, &[("docs", "docs")]);

        let listing = list(&roots, None, Some("report"), 100, Deadline::unlimited());

        assert_eq!(paths(&listing), vec!["docs/Quarterly-Report.pdf"]);
    }

    #[test]
    fn a_limit_truncates_and_says_so() {
        let tree = TempTree::new("list-limit");
        for index in 0..5 {
            tree.write_bytes(&format!("docs/{index}.pdf"), &simple_pdf("hello"));
        }
        let roots = roots_for(&tree, &[("docs", "docs")]);

        let listing = list(&roots, None, None, 2, Deadline::unlimited());

        assert_eq!(listing.documents.len(), 2);
        assert!(listing.truncated);
    }

    #[test]
    fn an_expired_deadline_stops_the_walk_and_says_so() {
        let tree = TempTree::new("list-deadline");
        tree.write_bytes("docs/q4.pdf", &simple_pdf("hello"));
        let roots = roots_for(&tree, &[("docs", "docs")]);

        let listing = list(&roots, None, None, 100, Deadline::expired());

        assert!(listing.truncated);
        assert!(listing.documents.is_empty());
    }

    #[test]
    fn a_symlinked_directory_is_not_walked_so_a_link_cannot_list_the_disk() {
        let tree = TempTree::new("list-symlink");
        tree.write_bytes("docs/q4.pdf", &simple_pdf("hello"));
        tree.write_bytes("outside/payroll.pdf", &simple_pdf("secret"));
        let roots = roots_for(&tree, &[("docs", "docs")]);

        let root = tree.canonical_root().join("docs");
        let outside =
            std::fs::canonicalize(tree.path().join("outside")).expect("canonical outside");
        let Ok(()) = link_directory(&outside, &root.join("escape")) else {
            eprintln!("skipping symlink listing assertion: this platform refused a directory link");
            return;
        };

        let listing = list(&roots, None, None, 100, Deadline::unlimited());

        assert_eq!(paths(&listing), vec!["docs/q4.pdf"]);
    }

    #[test]
    fn file_sizes_come_back_with_the_listing() {
        let tree = TempTree::new("list-size");
        let bytes = simple_pdf("hello");
        tree.write_bytes("docs/q4.pdf", &bytes);
        let roots = roots_for(&tree, &[("docs", "docs")]);

        let listing = list(&roots, None, None, 100, Deadline::unlimited());

        assert_eq!(listing.documents[0].bytes, bytes.len() as u64);
        assert!(listing.documents[0].modified_unix.is_some());
    }

    #[test]
    fn an_empty_root_lists_nothing_without_failing() {
        let tree = TempTree::new("list-empty");
        tree.mkdir("docs");
        let roots = roots_for(&tree, &[("docs", "docs")]);

        let listing = list(&roots, None, None, 100, Deadline::unlimited());

        assert!(listing.documents.is_empty());
        assert!(!listing.truncated);
    }
}
