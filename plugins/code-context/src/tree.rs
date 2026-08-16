//! Directory tree rendering.
//!
//! The tree is built from the index, not from a fresh directory walk. That is
//! deliberate: what it draws is exactly what `search` and `read` can reach, so
//! a model never sees a path in the tree that the other two tools then refuse.
//! The cost is that a directory holding no indexable file does not appear.

/// One level of the tree. A node is a directory when it has children and a file
/// when it does not.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Node {
    pub children: std::collections::BTreeMap<String, Node>,
    /// A file path ended here.
    pub file: bool,
    /// There was more below, past the requested depth.
    pub elided: bool,
}

/// Build a tree from sorted, root-relative, `/`-separated paths.
///
/// `depth` is the number of path components to keep; anything deeper marks its
/// last visible ancestor as elided so the render can say so out loud rather
/// than silently pretending the directory is empty.
pub fn build(paths: &[String], depth: usize) -> Node {
    let mut root = Node::default();
    for path in paths {
        let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        let mut node = &mut root;
        for (level, segment) in segments.iter().enumerate() {
            if level >= depth {
                node.elided = true;
                break;
            }
            node = node.children.entry((*segment).to_string()).or_default();
            if level + 1 == segments.len() {
                node.file = true;
            }
        }
    }
    root
}

/// Render a tree, stopping after `max_entries` lines.
///
/// Returns the lines and whether the render was cut short.
pub fn render(root: &Node, max_entries: usize) -> (Vec<String>, bool) {
    let mut lines = Vec::new();
    let truncated = walk(root, "", &mut lines, max_entries);
    (lines, truncated)
}

fn walk(node: &Node, prefix: &str, lines: &mut Vec<String>, max_entries: usize) -> bool {
    let entries: Vec<(&String, &Node)> = node.children.iter().collect();
    for (position, (name, child)) in entries.iter().enumerate() {
        if lines.len() >= max_entries {
            return true;
        }
        // The elision marker is printed after the last child, so the last
        // child is not the last line when there is one.
        let last = position + 1 == entries.len() && !node.elided;
        let connector = if last { "└── " } else { "├── " };
        let suffix = if child.children.is_empty() && child.file {
            ""
        } else {
            "/"
        };
        lines.push(format!("{prefix}{connector}{name}{suffix}"));

        let child_prefix = format!("{prefix}{}", if last { "    " } else { "│   " });
        if walk(child, &child_prefix, lines, max_entries) {
            return true;
        }
    }

    if node.elided {
        if lines.len() >= max_entries {
            return true;
        }
        lines.push(format!("{prefix}└── …"));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(values: &[&str]) -> Vec<String> {
        let mut owned: Vec<String> = values.iter().map(|value| (*value).to_string()).collect();
        owned.sort();
        owned
    }

    #[test]
    fn nested_paths_render_as_a_tree() {
        let tree = build(&paths(&["a/b.rs", "a/c/d.rs", "e.rs"]), 8);
        let (lines, truncated) = render(&tree, 100);

        assert!(!truncated);
        assert_eq!(
            lines,
            vec![
                "├── a/",
                "│   ├── b.rs",
                "│   └── c/",
                "│       └── d.rs",
                "└── e.rs",
            ]
        );
    }

    #[test]
    fn depth_cuts_the_tree_and_says_so() {
        let tree = build(&paths(&["a/b/c/d.rs", "e.rs"]), 2);
        let (lines, truncated) = render(&tree, 100);

        assert!(!truncated);
        assert_eq!(
            lines,
            vec!["├── a/", "│   └── b/", "│       └── …", "└── e.rs"]
        );
    }

    #[test]
    fn a_single_file_at_the_root_needs_no_connectors() {
        let tree = build(&paths(&["main.rs"]), 4);
        let (lines, _) = render(&tree, 100);
        assert_eq!(lines, vec!["└── main.rs"]);
    }

    #[test]
    fn the_entry_cap_truncates_rather_than_flooding_the_reply() {
        let many: Vec<String> = (0..50).map(|index| format!("file{index:02}.rs")).collect();
        let tree = build(&many, 4);
        let (lines, truncated) = render(&tree, 10);

        assert!(truncated);
        assert_eq!(lines.len(), 10);
        assert_eq!(lines[0], "├── file00.rs");
    }

    #[test]
    fn an_empty_index_renders_nothing_rather_than_failing() {
        let tree = build(&[], 4);
        let (lines, truncated) = render(&tree, 10);
        assert!(lines.is_empty());
        assert!(!truncated);
    }
}
