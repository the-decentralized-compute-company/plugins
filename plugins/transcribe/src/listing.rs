//! Finding the audio inside the configured roots.
//!
//! A model cannot guess a filename, so without this tool `transcribe` is only
//! usable by somebody who already knows the exact path. The walk is bounded in
//! every direction that a caller could otherwise grow — depth, entry count,
//! and per-file work — because this runs on somebody else's disk.
//!
//! Symbolic links are never followed. A link inside a root pointing at
//! `~/Documents` would otherwise turn a listing of one directory into a listing
//! of a home directory, and [`crate::roots`] would then refuse to open anything
//! it named, which is a confusing way to be safe.

use std::path::Path;

use serde::Serialize;

use crate::audio;
use crate::config::MAX_LIST_DEPTH;
use crate::roots::{Roots, relative_display};

/// How much of a WAV is read to recover its duration. Comfortably past any
/// realistic `fmt `/`LIST` block and small enough that listing a thousand files
/// stays cheap.
const HEADER_PROBE_BYTES: usize = 8 * 1_024;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AudioEntry {
    /// The exact string to pass to `transcribe` as `path`.
    pub path: String,
    pub bytes: u64,
    /// The filename extension, lowercased. A hint only: the transcribe path
    /// sniffs the real bytes, so a mislabelled file is caught there.
    pub extension: String,
    /// Present for WAV, whose header states it. Reading a duration out of a
    /// compressed container needs a decoder this plugin does not have.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Listing {
    pub entries: Vec<AudioEntry>,
    /// True when the entry cap was reached, so a caller knows the list is a
    /// prefix of the truth rather than the whole of it.
    pub truncated: bool,
    /// Roots that were walked.
    pub roots: Vec<String>,
    /// Roots that are configured but not currently readable — an unmounted
    /// drive, a directory that was renamed. Reported rather than hidden.
    pub unavailable_roots: Vec<String>,
}

/// Walk the roots and list the audio files.
///
/// `only` restricts the walk to one root by label; `None` walks them all.
/// Entries come back sorted by path so two calls on an unchanged directory
/// return the same list in the same order.
pub fn walk(
    roots: &Roots,
    include_hidden: bool,
    max_entries: usize,
    only: Option<&str>,
) -> Listing {
    let mut listing = Listing::default();
    let mut budget = max_entries;

    for root in roots.entries() {
        if only.is_some_and(|label| label != root.label) {
            continue;
        }
        let Some(canonical) = root.canonical.as_ref() else {
            listing.unavailable_roots.push(root.label.clone());
            continue;
        };
        listing.roots.push(root.label.clone());

        let mut stack: Vec<(std::path::PathBuf, usize)> = vec![(canonical.clone(), 0)];
        while let Some((directory, depth)) = stack.pop() {
            if budget == 0 {
                listing.truncated = true;
                break;
            }
            let Ok(children) = std::fs::read_dir(&directory) else {
                // A directory this process cannot open is not an error worth
                // failing the whole listing over; the files it can see are
                // still useful.
                continue;
            };

            for child in children.flatten() {
                let name = child.file_name().to_string_lossy().into_owned();
                if !include_hidden && name.starts_with('.') {
                    continue;
                }
                // `symlink_metadata` does not follow the link, which is the
                // whole point: a link is skipped rather than descended into.
                let Ok(metadata) = child.metadata_no_follow() else {
                    continue;
                };
                if metadata.is_symlink() {
                    continue;
                }

                if metadata.is_dir() {
                    if depth < MAX_LIST_DEPTH {
                        stack.push((child.path(), depth + 1));
                    }
                    continue;
                }
                if !metadata.is_file() || !audio::has_audio_extension(&name) {
                    continue;
                }
                if budget == 0 {
                    listing.truncated = true;
                    break;
                }

                let path = child.path();
                let relative = relative_display(canonical, &path).unwrap_or(name.clone());
                listing.entries.push(AudioEntry {
                    path: format!("{}/{relative}", root.label),
                    bytes: metadata.len(),
                    extension: extension_of(&name),
                    duration_seconds: wav_duration(&path, metadata.len()),
                });
                budget -= 1;
            }
        }
    }

    listing
        .entries
        .sort_by(|left, right| left.path.cmp(&right.path));
    listing
}

/// `std::fs::DirEntry::metadata` already does not follow symlinks on the
/// platforms this runs on, but the guarantee is worth naming at the call site
/// rather than remembering.
trait NoFollow {
    fn metadata_no_follow(&self) -> std::io::Result<std::fs::Metadata>;
}

impl NoFollow for std::fs::DirEntry {
    fn metadata_no_follow(&self) -> std::io::Result<std::fs::Metadata> {
        std::fs::symlink_metadata(self.path())
    }
}

fn extension_of(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Read a WAV duration from a bounded prefix, or nothing.
fn wav_duration(path: &Path, file_len: u64) -> Option<f64> {
    use std::io::Read;

    if !path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
    {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let mut prefix = vec![0u8; HEADER_PROBE_BYTES.min(file_len as usize)];
    let mut filled = 0usize;
    while filled < prefix.len() {
        match file.read(&mut prefix[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(_) => return None,
        }
    }
    prefix.truncate(filled);
    audio::wav_duration_from_prefix(&prefix, file_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RootSpec;
    use crate::testutil::{TempTree, link_directory, wav_fixture};

    fn roots_for(tree: &TempTree, names: &[&str]) -> Roots {
        Roots::open(
            &names
                .iter()
                .map(|name| RootSpec {
                    label: (*name).to_string(),
                    path: tree.path().join(name),
                })
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn audio_files_are_listed_with_the_exact_path_transcribe_accepts() {
        let tree = TempTree::new("list-basic");
        tree.write("audio/takes/one.wav", &wav_fixture(16_000, 1, 2.0));
        tree.write("audio/two.mp3", b"ID3\x04\x00\x00\x00\x00\x00\x00");
        let roots = roots_for(&tree, &["audio"]);

        let listing = walk(&roots, false, 100, None);

        assert_eq!(
            listing
                .entries
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            ["audio/takes/one.wav", "audio/two.mp3"]
        );
        assert!(!listing.truncated);
        assert_eq!(listing.roots, ["audio"]);

        // The listed path resolves back to the same file.
        for entry in &listing.entries {
            assert!(roots.resolve(&entry.path).is_ok(), "{}", entry.path);
        }
    }

    #[test]
    fn a_wav_reports_its_duration_and_a_compressed_file_honestly_does_not() {
        let tree = TempTree::new("list-duration");
        tree.write("audio/one.wav", &wav_fixture(8_000, 1, 3.5));
        tree.write("audio/two.mp3", b"ID3\x04\x00\x00\x00\x00\x00\x00");
        let roots = roots_for(&tree, &["audio"]);

        let listing = walk(&roots, false, 100, None);

        let wav = &listing.entries[0];
        assert_eq!(wav.extension, "wav");
        assert!((wav.duration_seconds.expect("wav duration") - 3.5).abs() < 1e-6);

        let mp3 = &listing.entries[1];
        assert_eq!(mp3.extension, "mp3");
        assert_eq!(
            mp3.duration_seconds, None,
            "no decoder, so no invented number"
        );
    }

    #[test]
    fn files_that_are_not_audio_are_left_out() {
        let tree = TempTree::new("list-filter");
        tree.write("audio/notes.txt", b"not audio");
        tree.write("audio/cover.png", b"\x89PNG\r\n\x1a\n");
        tree.write("audio/real.flac", b"fLaC");
        let roots = roots_for(&tree, &["audio"]);

        let listing = walk(&roots, false, 100, None);
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            ["audio/real.flac"]
        );
    }

    #[test]
    fn dot_directories_are_skipped_until_they_are_asked_for() {
        let tree = TempTree::new("list-hidden");
        tree.write("audio/.trash/deleted.wav", &wav_fixture(8_000, 1, 0.1));
        tree.write("audio/kept.wav", &wav_fixture(8_000, 1, 0.1));
        let roots = roots_for(&tree, &["audio"]);

        let visible = walk(&roots, false, 100, None);
        assert_eq!(
            visible
                .entries
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            ["audio/kept.wav"]
        );

        let all = walk(&roots, true, 100, None);
        assert_eq!(all.entries.len(), 2, "{all:?}");
    }

    #[test]
    fn several_roots_are_listed_under_their_own_labels_and_can_be_filtered() {
        let tree = TempTree::new("list-multi-root");
        tree.write("podcasts/ep1.wav", &wav_fixture(8_000, 1, 0.1));
        tree.write("interviews/ep1.wav", &wav_fixture(8_000, 1, 0.1));
        let roots = roots_for(&tree, &["podcasts", "interviews"]);

        let everything = walk(&roots, false, 100, None);
        assert_eq!(
            everything
                .entries
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            ["interviews/ep1.wav", "podcasts/ep1.wav"]
        );

        let one = walk(&roots, false, 100, Some("podcasts"));
        assert_eq!(
            one.entries
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            ["podcasts/ep1.wav"]
        );
        assert_eq!(one.roots, ["podcasts"]);
    }

    #[test]
    fn the_entry_cap_truncates_and_says_so() {
        let tree = TempTree::new("list-cap");
        for index in 0..10 {
            tree.write(
                &format!("audio/take-{index}.wav"),
                &wav_fixture(8_000, 1, 0.1),
            );
        }
        let roots = roots_for(&tree, &["audio"]);

        let listing = walk(&roots, false, 4, None);

        assert_eq!(listing.entries.len(), 4);
        assert!(listing.truncated, "a caller must know the list is partial");
    }

    #[test]
    fn an_unavailable_root_is_reported_rather_than_silently_missing() {
        let tree = TempTree::new("list-unavailable");
        tree.write("audio/one.wav", &wav_fixture(8_000, 1, 0.1));
        let roots = Roots::open(&[
            RootSpec {
                label: "audio".into(),
                path: tree.path().join("audio"),
            },
            RootSpec {
                label: "removable".into(),
                path: tree.path().join("not-mounted"),
            },
        ]);

        let listing = walk(&roots, false, 100, None);

        assert_eq!(listing.roots, ["audio"]);
        assert_eq!(listing.unavailable_roots, ["removable"]);
        assert_eq!(listing.entries.len(), 1);
    }

    #[test]
    fn a_link_out_of_the_root_is_not_walked_into() {
        let tree = TempTree::new("list-symlink");
        tree.write("audio/inside.wav", &wav_fixture(8_000, 1, 0.1));
        tree.write("private/confession.wav", &wav_fixture(8_000, 1, 0.1));

        let inside = std::fs::canonicalize(tree.path().join("audio")).expect("canonical");
        let outside = std::fs::canonicalize(tree.path().join("private")).expect("canonical");
        let Ok(()) = link_directory(&outside, &inside.join("escape")) else {
            eprintln!("skipping symlink listing assertion: this platform refused a directory link");
            return;
        };

        let roots = Roots::open(&[RootSpec {
            label: "audio".into(),
            path: inside,
        }]);
        let listing = walk(&roots, false, 100, None);

        assert_eq!(
            listing
                .entries
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            ["audio/inside.wav"],
            "the linked directory must not be walked"
        );
    }

    #[test]
    fn an_empty_root_lists_nothing_without_complaining() {
        let tree = TempTree::new("list-empty");
        tree.mkdir("audio");
        let roots = roots_for(&tree, &["audio"]);

        let listing = walk(&roots, false, 100, None);
        assert!(listing.entries.is_empty());
        assert!(!listing.truncated);
        assert_eq!(listing.roots, ["audio"]);
    }

    #[test]
    fn with_no_roots_the_listing_is_empty_rather_than_a_walk_of_the_disk() {
        let listing = walk(&Roots::open(&[]), false, 100, None);
        assert_eq!(listing, Listing::default());
    }
}
