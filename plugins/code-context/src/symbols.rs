//! Declaration extraction.
//!
//! This is a lexical scan, not a parser. It reads one line at a time, strips
//! the modifiers a language puts in front of a declaration, and looks for a
//! keyword followed by an identifier. That is enough to answer "where is
//! `resolve_within` defined" without a language server, a build, or a
//! `compile_commands.json` — and it is honest about what it is: it will miss
//! declarations written in unusual shapes and it will occasionally record one
//! that appears inside a string or a comment.
//!
//! Symbols live in the index, so symbol search costs no file I/O at all.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Symbol {
    pub name: String,
    /// The declaring keyword: `fn`, `struct`, `class`, `def`, ...
    pub kind: &'static str,
    /// 1-based line number, so it can be cited directly.
    pub line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    Go,
    JvmFamily,
    CFamily,
    Ruby,
    Shell,
    Php,
    Lua,
}

/// Which languages get symbols, keyed by file extension.
///
/// A file whose extension is not here is still indexed and still
/// content-searchable; it simply contributes no symbols.
pub fn language_for(relative_path: &str) -> Option<Language> {
    let name = relative_path.rsplit('/').next().unwrap_or(relative_path);
    let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match extension.as_str() {
        "rs" => Language::Rust,
        "py" | "pyi" => Language::Python,
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => Language::JavaScript,
        "go" => Language::Go,
        "java" | "kt" | "kts" | "scala" | "cs" => Language::JvmFamily,
        "c" | "h" | "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => Language::CFamily,
        "rb" => Language::Ruby,
        "sh" | "bash" | "zsh" => Language::Shell,
        "php" => Language::Php,
        "lua" => Language::Lua,
        _ => return None,
    })
}

/// Words that may sit in front of a declaration keyword without changing it.
fn modifiers(language: Language) -> &'static [&'static str] {
    match language {
        Language::Rust => &[
            "pub",
            "pub(crate)",
            "pub(super)",
            "pub(self)",
            "async",
            "unsafe",
            "extern",
            "default",
            // `const` is both a modifier (`const fn`) and a keyword
            // (`const MAX`). The keyword test below runs first and settles it.
            "const",
        ],
        Language::Python => &["async"],
        Language::JavaScript => &[
            "export", "default", "async", "declare", "abstract", "public",
        ],
        Language::Go => &[],
        Language::JvmFamily => &[
            "public",
            "private",
            "protected",
            "internal",
            "static",
            "final",
            "abstract",
            "sealed",
            "open",
            "override",
            "suspend",
            "data",
            "inline",
            "operator",
            "async",
            "partial",
            "readonly",
            "unsafe",
            "virtual",
        ],
        Language::CFamily => &["static", "inline", "extern", "typedef", "template"],
        Language::Ruby => &[],
        Language::Shell => &[],
        Language::Php => &[
            "public",
            "private",
            "protected",
            "static",
            "final",
            "abstract",
        ],
        Language::Lua => &["local"],
    }
}

/// Declaration keywords, in the order they should be tried.
fn keywords(language: Language) -> &'static [&'static str] {
    match language {
        Language::Rust => &[
            "fn",
            "struct",
            "enum",
            "trait",
            "type",
            "union",
            "mod",
            "const",
            "static",
            "macro_rules!",
        ],
        Language::Python => &["def", "class"],
        Language::JavaScript => &[
            "function",
            "class",
            "interface",
            "type",
            "enum",
            "const",
            "let",
            "var",
        ],
        Language::Go => &["func", "type"],
        Language::JvmFamily => &[
            "class",
            "interface",
            "enum",
            "record",
            "struct",
            "fun",
            "object",
            "trait",
            "def",
        ],
        Language::CFamily => &["struct", "class", "enum", "union", "namespace", "#define"],
        Language::Ruby => &["def", "class", "module"],
        Language::Shell => &["function"],
        Language::Php => &["function", "class", "interface", "trait", "enum"],
        Language::Lua => &["function"],
    }
}

/// Extract every declaration this scan recognises, in line order.
pub fn extract(relative_path: &str, text: &str) -> Vec<Symbol> {
    let Some(language) = language_for(relative_path) else {
        return Vec::new();
    };

    let mut symbols = Vec::new();
    for (offset, line) in text.lines().enumerate() {
        if let Some((kind, name)) = declaration_in_line(language, line) {
            symbols.push(Symbol {
                name,
                kind,
                line: offset as u32 + 1,
            });
        }
    }
    symbols
}

/// The single-line rule, exposed so it can be tested one case at a time.
pub fn declaration_in_line(language: Language, line: &str) -> Option<(&'static str, String)> {
    let mut rest = line.trim_start();
    if rest.is_empty() {
        return None;
    }

    // Shell and Lua also write `name() {` with no keyword at all.
    if matches!(language, Language::Shell | Language::Lua)
        && let Some(name) = bare_function_definition(rest)
    {
        return Some(("function", name));
    }

    let modifier_list = modifiers(language);
    let keyword_list = keywords(language);

    loop {
        let (head, tail) = split_word(rest);
        if head.is_empty() {
            return None;
        }

        // `const fn` in Rust is a function, not a constant, so `const` acts as
        // a modifier there and as a keyword everywhere else.
        let next_word = split_word(tail.trim_start()).0;
        let head_is_keyword =
            keyword_list.contains(&head) && !(head == "const" && next_word == "fn");

        if head_is_keyword {
            let name = match language {
                // `func (s *Server) Handle(...)` — skip the receiver.
                Language::Go if head == "func" => identifier_after_receiver(tail),
                _ => leading_identifier(tail.trim_start()),
            }?;
            let kind = keyword_list.iter().copied().find(|word| *word == head)?;
            return Some((kind, name));
        }

        if modifier_list.contains(&head) {
            rest = tail.trim_start();
            continue;
        }

        return None;
    }
}

/// `name() {` / `name = function(` with no leading keyword.
fn bare_function_definition(line: &str) -> Option<String> {
    let name = leading_identifier(line)?;
    let rest = line[name.len()..].trim_start();
    let rest = rest.strip_prefix("()").or_else(|| rest.strip_prefix("("))?;
    rest.trim_start().starts_with(['{', ')']).then_some(name)
}

/// After Go's `func`, an optional `(receiver)` may precede the name.
fn identifier_after_receiver(tail: &str) -> Option<String> {
    let trimmed = tail.trim_start();
    match trimmed.strip_prefix('(') {
        Some(after_open) => {
            let close = after_open.find(')')?;
            leading_identifier(after_open[close + 1..].trim_start())
        }
        None => leading_identifier(trimmed),
    }
}

/// Split off the first whitespace-delimited word.
fn split_word(text: &str) -> (&str, &str) {
    match text.find(char::is_whitespace) {
        Some(index) => (&text[..index], &text[index..]),
        None => (text, ""),
    }
}

/// The longest identifier at the start of `text`, or `None` when there is not
/// one. Identifiers may not start with a digit, which is what keeps
/// `const 3 = ...` style noise out of the index.
fn leading_identifier(text: &str) -> Option<String> {
    let mut identifier = String::new();
    for character in text.chars() {
        if character.is_alphanumeric() || character == '_' || character == '$' {
            identifier.push(character);
        } else {
            break;
        }
    }
    if identifier.is_empty() || identifier.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some(identifier)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(symbols: &[Symbol]) -> Vec<(&str, &str, u32)> {
        symbols
            .iter()
            .map(|symbol| (symbol.kind, symbol.name.as_str(), symbol.line))
            .collect()
    }

    #[test]
    fn rust_declarations_carry_their_keyword_and_line() {
        let source = "\
use std::fmt;

pub struct Index {
    files: usize,
}

pub(crate) async fn refresh(force: bool) -> usize {
    0
}

const MAX: usize = 10;
pub const fn ceiling() -> usize { MAX }
macro_rules! shout {
    () => {};
}
";
        assert_eq!(
            names(&extract("src/index.rs", source)),
            vec![
                ("struct", "Index", 3),
                ("fn", "refresh", 7),
                ("const", "MAX", 11),
                ("fn", "ceiling", 12),
                ("macro_rules!", "shout", 13),
            ]
        );
    }

    #[test]
    fn go_methods_skip_the_receiver() {
        let source = "\
package main

func main() {}

func (s *Server) Handle(w http.ResponseWriter) {}

type Server struct{}
";
        assert_eq!(
            names(&extract("main.go", source)),
            vec![
                ("func", "main", 3),
                ("func", "Handle", 5),
                ("type", "Server", 7),
            ]
        );
    }

    #[test]
    fn python_and_javascript_shapes_are_recognised() {
        let python = "class Store:\n    async def load(self):\n        pass\n";
        assert_eq!(
            names(&extract("store.py", python)),
            vec![("class", "Store", 1), ("def", "load", 2)]
        );

        let javascript = "\
export default class Widget {}
export async function render(props) {}
export const DEFAULT_LIMIT = 25
interface Options {}
";
        assert_eq!(
            names(&extract("widget.tsx", javascript)),
            vec![
                ("class", "Widget", 1),
                ("function", "render", 2),
                ("const", "DEFAULT_LIMIT", 3),
                ("interface", "Options", 4),
            ]
        );
    }

    #[test]
    fn shell_functions_are_found_with_and_without_the_keyword() {
        let source = "#!/bin/sh\ndeploy() {\n  echo hi\n}\nfunction rollback {\n  echo no\n}\n";
        assert_eq!(
            names(&extract("deploy.sh", source)),
            vec![("function", "deploy", 2), ("function", "rollback", 5)]
        );
    }

    #[test]
    fn files_without_a_known_extension_contribute_no_symbols() {
        assert!(extract("notes.md", "# Heading\nclass Thing\n").is_empty());
        assert!(extract("Makefile", "build:\n\tcargo build\n").is_empty());
    }

    #[test]
    fn keywords_used_as_prose_do_not_become_symbols() {
        // A keyword with nothing that parses as an identifier after it.
        assert_eq!(declaration_in_line(Language::Rust, "fn"), None);
        assert_eq!(declaration_in_line(Language::Rust, "fn 9lives()"), None);
        assert_eq!(
            declaration_in_line(Language::Rust, "    let value = 1;"),
            None
        );
        assert_eq!(declaration_in_line(Language::Python, "define(x)"), None);
    }

    #[test]
    fn a_lone_modifier_line_is_not_a_declaration() {
        assert_eq!(declaration_in_line(Language::Rust, "pub"), None);
        assert_eq!(declaration_in_line(Language::JavaScript, "export {"), None);
    }
}
