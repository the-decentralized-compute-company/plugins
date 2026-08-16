//! Turning a web page into text a model can actually read.
//!
//! The point of `fetch` is that a model asks for one URL and gets prose back,
//! not 400 KB of markup in which the first 2 000 tokens are a cookie banner and
//! a navigation menu. This module is a deliberately small, dependency-free
//! extractor rather than a full HTML parser:
//!
//! 1. Elements that never contain prose (`script`, `style`, `svg`, …) are
//!    dropped along with their contents.
//! 2. Site chrome is dropped — `nav`, `aside` and `footer` outright, and any
//!    container whose `class` or `id` reads like chrome.
//! 3. If the page marks its content with `<main>` or `<article>`, only that
//!    region is rendered.
//! 4. What survives becomes lightly-structured text: headings keep their `#`
//!    markers, list items keep a bullet, block elements keep their line breaks,
//!    and link targets are dropped while the link text stays.
//!
//! Everything here is a pure function over a `&str`, which is what makes it
//! testable without a network or a host.

/// A page reduced to its title and readable body text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Readable {
    pub title: Option<String>,
    pub text: String,
}

/// Below this, a `<main>`/`<article>` region is treated as a mis-identified
/// wrapper and the whole body is rendered instead.
const MIN_REGION_CHARS: usize = 200;

/// Elements whose content is never prose. Dropped with their subtree.
const DROP_TAGS: &[&str] = &[
    "applet", "audio", "button", "canvas", "dialog", "embed", "form", "iframe", "input", "map",
    "math", "menu", "meta", "noscript", "object", "option", "script", "select", "style", "svg",
    "template", "textarea", "video",
];

/// Structural site chrome. Dropped with their subtree wherever they appear.
///
/// `<header>` is deliberately absent: articles routinely put their `<h1>` and
/// byline in one. A site-wide header is usually class-marked and gets caught by
/// [`is_boilerplate_attribute`] instead.
const CHROME_TAGS: &[&str] = &["aside", "footer", "nav"];

/// Elements whose text content is raw — a `<` inside them is not markup, so
/// they are skipped by searching for the literal closing tag.
const RAW_TEXT_TAGS: &[&str] = &["script", "style", "textarea", "title", "xmp"];

/// Elements that never have a closing tag.
const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Containers worth testing against [`is_boilerplate_attribute`]. Restricted to
/// wrappers so that a paragraph tagged `class="social"` is not silently deleted
/// from an article about social policy.
const CONTAINER_TAGS: &[&str] = &[
    "aside", "div", "footer", "form", "header", "nav", "ol", "section", "ul",
];

/// Substrings that mark a container as site chrome.
///
/// Chosen to be unambiguous. `comment` and `related` are deliberately absent:
/// on a forum or a Q&A site those containers hold the actual answer.
const BOILERPLATE_MARKERS: &[&str] = &[
    "adsbygoogle",
    "advert",
    "banner",
    "breadcrumb",
    "consent",
    "cookie",
    "gdpr",
    "masthead",
    "navbar",
    "navigation",
    "newsletter",
    "paywall",
    "popup",
    "screen-reader",
    "sidebar",
    "side-bar",
    "site-footer",
    "site-header",
    "skip-link",
    "skip-to",
    "social",
    "sr-only",
    "subscribe",
    "toolbar",
];

/// Whole-token chrome names, matched exactly so `ad` does not swallow `header`.
const BOILERPLATE_TOKENS: &[&str] = &["ad", "ads", "menu", "nav", "share", "sharing", "promo"];

/// Extract a page's title and readable body text.
pub fn extract_readable(html: &str) -> Readable {
    let title = extract_title(html);

    let body = element_contents(html, "body")
        .into_iter()
        .max_by_key(|region| region.len())
        .unwrap_or(html);

    // Prefer an explicitly marked content region, but only when it actually
    // carries the page: plenty of templates wrap the whole shell in `<main>`,
    // and plenty wrap a teaser in `<article>`.
    let region = element_contents(body, "main")
        .into_iter()
        .chain(element_contents(body, "article"))
        .max_by_key(|region| region.len());

    let text = match region {
        Some(region) => {
            let rendered = render_text(region);
            if rendered.chars().count() >= MIN_REGION_CHARS {
                rendered
            } else {
                let whole = render_text(body);
                if whole.chars().count() > rendered.chars().count() {
                    whole
                } else {
                    rendered
                }
            }
        }
        None => render_text(body),
    };

    Readable { title, text }
}

/// Cut text to at most `max_chars` characters, reporting whether anything was
/// dropped. Splits on a character boundary and prefers the last blank line so
/// the tail is not a half-sentence.
pub fn truncate_chars(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_string(), false);
    }
    let cut = text
        .char_indices()
        .nth(max_chars)
        .map_or(text.len(), |(index, _)| index);
    let head = &text[..cut];
    let trimmed = match head.rfind("\n\n") {
        // Only back off to a paragraph break when it does not throw away most
        // of the budget.
        Some(index) if index * 4 >= head.len() * 3 => &head[..index],
        _ => head,
    };
    (trimmed.trim_end().to_string(), true)
}

fn extract_title(html: &str) -> Option<String> {
    let raw = element_contents(html, "title").into_iter().next()?;
    let title = collapse_whitespace(&decode_entities(raw));
    (!title.is_empty()).then_some(title)
}

/// Every top-level occurrence of `<tag>…</tag>`, as slices of `html`.
///
/// Nesting of the same tag is tracked so `<div><div/></div>` style structures
/// do not close early. This is a scanner, not a parser: markup inside a
/// `<script>` string that looks like `</article>` will fool it. That is an
/// acceptable trade for having no HTML-parser dependency, and the worst case is
/// a slightly wrong region, not a crash.
fn element_contents<'a>(html: &'a str, tag: &str) -> Vec<&'a str> {
    let mut found = Vec::new();
    let mut cursor = 0usize;

    while cursor < html.len() {
        let Some((open_start, open_end, self_closing)) = find_start_tag(html, tag, cursor) else {
            break;
        };
        if self_closing {
            cursor = open_end;
            continue;
        }
        let mut depth = 1usize;
        let mut scan = open_end;
        let mut close_start = None;
        while scan < html.len() {
            let next_open = find_start_tag(html, tag, scan);
            let next_close = find_end_tag(html, tag, scan);
            match (next_open, next_close) {
                (Some((open, after_open, nested_self_closing)), Some((close, after_close))) => {
                    if open < close {
                        if !nested_self_closing {
                            depth += 1;
                        }
                        scan = after_open;
                    } else {
                        depth -= 1;
                        if depth == 0 {
                            close_start = Some(close);
                            scan = after_close;
                            break;
                        }
                        scan = after_close;
                    }
                }
                (_, Some((close, after_close))) => {
                    depth -= 1;
                    if depth == 0 {
                        close_start = Some(close);
                        scan = after_close;
                        break;
                    }
                    scan = after_close;
                }
                _ => break,
            }
        }
        match close_start {
            Some(close) => {
                found.push(&html[open_end..close]);
                cursor = scan;
            }
            // Unclosed element: take the rest of the document and stop.
            None => {
                found.push(&html[open_end..]);
                break;
            }
        }
        let _ = open_start;
    }

    found
}

/// `(tag start, index just past `>`, self-closing)` for the next `<tag …>`.
fn find_start_tag(html: &str, tag: &str, from: usize) -> Option<(usize, usize, bool)> {
    let bytes = html.as_bytes();
    let mut cursor = from;
    while cursor < html.len() {
        let index = html[cursor..].find('<')? + cursor;
        let after = index + 1;
        let rest = html.get(after..)?;
        if rest.len() >= tag.len()
            && rest[..tag.len()].eq_ignore_ascii_case(tag)
            && rest[tag.len()..].chars().next().is_none_or(|character| {
                character.is_ascii_whitespace() || character == '>' || character == '/'
            })
        {
            let close = html[index..].find('>').map(|offset| index + offset)?;
            let self_closing = close > 0 && bytes[close - 1] == b'/';
            return Some((index, close + 1, self_closing || VOID_TAGS.contains(&tag)));
        }
        cursor = after;
    }
    None
}

/// `(tag start, index just past `>`)` for the next `</tag>`.
fn find_end_tag(html: &str, tag: &str, from: usize) -> Option<(usize, usize)> {
    let needle = format!("</{tag}");
    let mut cursor = from;
    while cursor < html.len() {
        let index = html[cursor..].to_ascii_lowercase().find(&needle)? + cursor;
        let after = index + needle.len();
        let terminated = html[after..]
            .chars()
            .next()
            .is_none_or(|character| character.is_ascii_whitespace() || character == '>');
        if terminated {
            let close = html[index..].find('>').map(|offset| index + offset)?;
            return Some((index, close + 1));
        }
        cursor = after;
    }
    None
}

/// Render a fragment of HTML as text.
fn render_text(html: &str) -> String {
    let mut writer = TextWriter::default();
    let mut cursor = 0usize;
    // While set, everything is skipped until the matching close of this tag.
    let mut dropping: Option<(String, usize)> = None;

    while cursor < html.len() {
        let Some(open) = html[cursor..].find('<').map(|offset| offset + cursor) else {
            if dropping.is_none() {
                writer.push_text(&html[cursor..]);
            }
            break;
        };

        if open > cursor && dropping.is_none() {
            writer.push_text(&html[cursor..open]);
        }

        let Some((token, next)) = parse_tag(html, open) else {
            // A bare `<` that starts nothing: treat it as text.
            if dropping.is_none() {
                writer.push_text("<");
            }
            cursor = open + 1;
            continue;
        };
        cursor = next;

        match token {
            Token::Ignorable => {}
            Token::Start {
                name,
                attributes,
                self_closing,
            } => {
                if let Some((dropped, depth)) = dropping.as_mut() {
                    if *dropped == name && !self_closing {
                        *depth += 1;
                    }
                    continue;
                }
                if RAW_TEXT_TAGS.contains(&name.as_str()) {
                    // Raw-text element: skip to its literal closing tag rather
                    // than trying to parse `a < b` inside it as markup.
                    cursor = find_end_tag(html, &name, cursor).map_or(html.len(), |(_, end)| end);
                    writer.push_break(1);
                    continue;
                }
                if should_drop(&name, &attributes) {
                    if !self_closing {
                        dropping = Some((name, 1));
                    }
                    writer.push_break(1);
                    continue;
                }
                writer.start_element(&name);
            }
            Token::End { name } => {
                if let Some((dropped, depth)) = dropping.as_mut() {
                    if *dropped == name {
                        *depth -= 1;
                        if *depth == 0 {
                            dropping = None;
                        }
                    }
                    continue;
                }
                writer.end_element(&name);
            }
        }
    }

    writer.finish()
}

fn should_drop(name: &str, attributes: &str) -> bool {
    if DROP_TAGS.contains(&name) || CHROME_TAGS.contains(&name) {
        return true;
    }
    if !CONTAINER_TAGS.contains(&name) {
        return false;
    }
    ["class", "id", "role"]
        .iter()
        .filter_map(|attribute| attribute_value(attributes, attribute))
        .any(|value| is_boilerplate_attribute(&value))
}

/// Does a `class`/`id`/`role` value read like site chrome?
pub fn is_boilerplate_attribute(value: &str) -> bool {
    value.split_whitespace().any(|token| {
        let token = token.to_ascii_lowercase();
        BOILERPLATE_TOKENS.contains(&token.as_str())
            || BOILERPLATE_MARKERS
                .iter()
                .any(|marker| token.contains(marker))
    })
}

/// The value of one attribute inside a start tag's attribute text.
pub fn attribute_value(attributes: &str, name: &str) -> Option<String> {
    let lowered = attributes.to_ascii_lowercase();
    let mut cursor = 0usize;
    while let Some(offset) = lowered[cursor..].find(name) {
        let start = cursor + offset;
        let end = start + name.len();
        let boundary_before = start == 0
            || lowered[..start]
                .chars()
                .next_back()
                .is_some_and(|character| character.is_ascii_whitespace());
        let rest = lowered[end..].trim_start();
        if boundary_before && rest.starts_with('=') {
            let value_start = end + (lowered[end..].len() - rest.len()) + 1;
            let raw = attributes[value_start..].trim_start();
            let value = match raw.chars().next() {
                Some(quote @ ('"' | '\'')) => raw[1..].split(quote).next().unwrap_or_default(),
                _ => raw.split_whitespace().next().unwrap_or_default(),
            };
            return Some(value.to_string());
        }
        cursor = end;
    }
    None
}

enum Token {
    /// A comment, doctype, or processing instruction.
    Ignorable,
    Start {
        name: String,
        attributes: String,
        self_closing: bool,
    },
    End {
        name: String,
    },
}

/// Parse the tag beginning at `start`, returning it and the index after it.
fn parse_tag(html: &str, start: usize) -> Option<(Token, usize)> {
    let rest = html.get(start + 1..)?;

    if let Some(after) = rest.strip_prefix("!--") {
        let end = after
            .find("-->")
            .map_or(html.len(), |offset| start + 1 + 3 + offset + "-->".len());
        return Some((Token::Ignorable, end));
    }
    if rest.starts_with('!') || rest.starts_with('?') {
        let end = html[start..]
            .find('>')
            .map_or(html.len(), |offset| start + offset + 1);
        return Some((Token::Ignorable, end));
    }

    let is_end = rest.starts_with('/');
    let name_start = start + 1 + usize::from(is_end);
    let name: String = html[name_start..]
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric() || *character == '-')
        .collect();
    if name.is_empty() {
        return None;
    }
    let close = html[start..].find('>').map(|offset| start + offset)?;
    let name = name.to_ascii_lowercase();

    if is_end {
        return Some((Token::End { name }, close + 1));
    }

    let attributes_start = name_start + name.len();
    let attributes = html.get(attributes_start..close).unwrap_or_default();
    let self_closing = attributes.trim_end().ends_with('/') || VOID_TAGS.contains(&name.as_str());
    Some((
        Token::Start {
            name,
            attributes: attributes.to_string(),
            self_closing,
        },
        close + 1,
    ))
}

/// Accumulates rendered text, managing line breaks and inline spacing so the
/// output never has runs of blank lines or lost word boundaries.
#[derive(Default)]
struct TextWriter {
    out: String,
    pending_newlines: usize,
    pending_space: bool,
    pre_depth: usize,
}

impl TextWriter {
    fn start_element(&mut self, name: &str) {
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = name[1..].parse::<usize>().unwrap_or(1);
                self.push_break(2);
                self.push_marker(&format!("{} ", "#".repeat(level)));
            }
            "li" => {
                self.push_break(1);
                self.push_marker("- ");
            }
            "br" => self.push_break(1),
            "pre" => {
                self.pre_depth += 1;
                self.push_break(2);
            }
            "p" | "div" | "section" | "article" | "main" | "header" | "blockquote" | "table"
            | "tr" | "ul" | "ol" | "dl" | "dd" | "dt" | "figcaption" | "address" | "details"
            | "summary" | "hr" => self.push_break(1),
            "td" | "th" => self.pending_space = true,
            _ => {}
        }
    }

    fn end_element(&mut self, name: &str) {
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "p" | "blockquote" | "table" | "ul"
            | "ol" | "dl" => self.push_break(2),
            "pre" => {
                self.pre_depth = self.pre_depth.saturating_sub(1);
                self.push_break(2);
            }
            "div" | "section" | "article" | "main" | "header" | "li" | "tr" | "dd" | "dt"
            | "figcaption" | "address" | "details" | "summary" => self.push_break(1),
            "td" | "th" => self.pending_space = true,
            _ => {}
        }
    }

    fn push_text(&mut self, raw: &str) {
        if raw.is_empty() {
            return;
        }
        let decoded = decode_entities(raw);
        if self.pre_depth > 0 {
            self.flush_breaks();
            self.out.push_str(&decoded);
            return;
        }
        let leading = decoded.starts_with(char::is_whitespace);
        let trailing = decoded.ends_with(char::is_whitespace);
        let collapsed = collapse_whitespace(&decoded);
        if collapsed.is_empty() {
            self.pending_space |= leading || trailing;
            return;
        }
        self.pending_space |= leading;
        self.write(&collapsed);
        self.pending_space = trailing;
    }

    /// Ask for at least `count` line breaks before the next text.
    fn push_break(&mut self, count: usize) {
        if self.out.is_empty() {
            return;
        }
        self.pending_newlines = self.pending_newlines.max(count.min(2));
        self.pending_space = false;
    }

    /// Write a structural marker (`# `, `- `) without inline-space handling.
    fn push_marker(&mut self, marker: &str) {
        self.flush_breaks();
        self.out.push_str(marker);
        self.pending_space = false;
    }

    fn write(&mut self, text: &str) {
        self.flush_breaks();
        if self.pending_space && !self.out.is_empty() && !self.out.ends_with(char::is_whitespace) {
            self.out.push(' ');
        }
        self.pending_space = false;
        self.out.push_str(text);
    }

    fn flush_breaks(&mut self) {
        if self.pending_newlines == 0 {
            return;
        }
        while self.out.ends_with(' ') || self.out.ends_with('\t') {
            self.out.pop();
        }
        let existing = self
            .out
            .chars()
            .rev()
            .take_while(|character| *character == '\n')
            .count();
        for _ in existing..self.pending_newlines {
            self.out.push('\n');
        }
        self.pending_newlines = 0;
        self.pending_space = false;
    }

    fn finish(mut self) -> String {
        while self.out.ends_with(char::is_whitespace) {
            self.out.pop();
        }
        self.out.trim_start().to_string()
    }
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Decode the HTML entities that actually show up in prose, plus every numeric
/// reference. An unrecognised entity is left as written rather than dropped.
pub fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        let tail = &rest[index..];
        // Entities are short; anything longer is a stray ampersand.
        let end = tail[1..]
            .char_indices()
            .take(32)
            .find(|(_, character)| *character == ';')
            .map(|(offset, _)| offset + 1);
        match end.and_then(|end| decode_entity(&tail[1..end])) {
            Some(decoded) => {
                out.push_str(&decoded);
                rest = &tail[end.expect("end is Some in this branch") + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_entity(body: &str) -> Option<String> {
    if let Some(digits) = body.strip_prefix('#') {
        let code = match digits.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => digits.parse::<u32>().ok()?,
        };
        return char::from_u32(code).map(String::from);
    }
    let replacement = match body {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" | "#39" => "'",
        // A non-breaking space becomes an ordinary one so whitespace
        // collapsing treats it like any other gap.
        "nbsp" => " ",
        "ndash" => "–",
        "mdash" => "—",
        "hellip" => "…",
        "lsquo" => "‘",
        "rsquo" => "’",
        "ldquo" => "“",
        "rdquo" => "”",
        "middot" => "·",
        "bull" => "•",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "deg" => "°",
        "laquo" => "«",
        "raquo" => "»",
        "times" => "×",
        "divide" => "÷",
        "plusmn" => "±",
        "euro" => "€",
        "pound" => "£",
        "yen" => "¥",
        "sect" => "§",
        "para" => "¶",
        "dagger" => "†",
        "prime" => "′",
        "shy" => "",
        _ => return None,
    };
    Some(replacement.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_and_styles_never_reach_the_output() {
        let html = "<p>before</p><script>var x = 1 < 2 && 3 > 2;</script>\
                    <style>body { color: red }</style><p>after</p>";

        let text = extract_readable(html).text;

        assert!(text.contains("before"), "{text}");
        assert!(text.contains("after"), "{text}");
        assert!(!text.contains("var x"), "{text}");
        assert!(!text.contains("color: red"), "{text}");
    }

    #[test]
    fn navigation_and_footers_are_dropped_with_their_contents() {
        let html = "<nav><a href='/'>Home</a><a href='/about'>About</a></nav>\
                    <p>The actual article.</p>\
                    <footer>Copyright 2026 Example Inc</footer>";

        let text = extract_readable(html).text;

        assert_eq!(text, "The actual article.");
    }

    #[test]
    fn a_cookie_banner_is_recognised_by_its_class() {
        let html = "<div class=\"cookie-consent-banner\">We value your privacy. Accept all?</div>\
                    <p>Real content here.</p>";

        let text = extract_readable(html).text;

        assert!(!text.contains("privacy"), "{text}");
        assert_eq!(text, "Real content here.");
    }

    #[test]
    fn a_paragraph_that_merely_mentions_chrome_words_survives() {
        // `p` is not a container, so the class is never consulted, and the word
        // "social" in prose must not delete the sentence.
        let html = "<p class=\"social\">Social policy is the topic of this page.</p>";

        assert!(extract_readable(html).text.contains("Social policy"));
    }

    #[test]
    fn link_text_is_kept_and_link_targets_are_not() {
        let html = "<p>See <a href=\"https://example.com/very/long/tracking?utm=1\">the docs</a> \
                    for more.</p>";

        let text = extract_readable(html).text;

        assert_eq!(text, "See the docs for more.");
    }

    #[test]
    fn headings_and_list_items_keep_their_structure() {
        let html = "<h2>Setup</h2><ul><li>Install it</li><li>Run it</li></ul>";

        let text = extract_readable(html).text;

        assert!(text.contains("## Setup"), "{text}");
        assert!(text.contains("- Install it"), "{text}");
        assert!(text.contains("- Run it"), "{text}");
    }

    #[test]
    fn the_title_element_becomes_the_title_and_not_body_text() {
        let page = "<html><head><title>Example &amp; Co — Home</title></head>\
                    <body><p>Body copy.</p></body></html>";

        let readable = extract_readable(page);

        assert_eq!(readable.title.as_deref(), Some("Example & Co — Home"));
        assert_eq!(readable.text, "Body copy.");
    }

    #[test]
    fn a_main_region_wins_over_surrounding_chrome() {
        let filler = "Substantive sentence about the subject. ".repeat(10);
        let html = format!(
            "<body><div id=\"sidebar\">Ads and links</div>\
             <main><p>{filler}</p></main>\
             <div class=\"site-footer\">Legal text</div></body>"
        );

        let text = extract_readable(&html).text;

        assert!(text.starts_with("Substantive sentence"), "{text}");
        assert!(!text.contains("Ads and links"), "{text}");
        assert!(!text.contains("Legal text"), "{text}");
    }

    #[test]
    fn a_nearly_empty_main_falls_back_to_the_whole_body() {
        let html = "<body><main><span>Hi</span></main>\
                    <div><p>The real content lives outside main on this template, and it is \
                    considerably longer than the wrapper element that the theme emitted.</p>\
                    <p>Second paragraph so the body clears the minimum length.</p></div></body>";

        let text = extract_readable(html).text;

        assert!(text.contains("real content lives outside"), "{text}");
    }

    #[test]
    fn entities_are_decoded_including_numeric_ones() {
        assert_eq!(decode_entities("a &amp; b"), "a & b");
        assert_eq!(decode_entities("&lt;tag&gt;"), "<tag>");
        assert_eq!(decode_entities("caf&#233;"), "café");
        assert_eq!(decode_entities("&#x1F600;"), "\u{1F600}");
        assert_eq!(decode_entities("5 &amp;&amp; 6"), "5 && 6");
        // A stray ampersand is left alone rather than eating the next words.
        assert_eq!(decode_entities("Tom & Jerry"), "Tom & Jerry");
        assert_eq!(decode_entities("&notanentity;"), "&notanentity;");
    }

    #[test]
    fn whitespace_between_inline_elements_is_preserved() {
        let html = "<p><b>bold</b> <i>italic</i>text</p>";

        assert_eq!(extract_readable(html).text, "bold italictext");
    }

    #[test]
    fn preformatted_blocks_keep_their_line_breaks() {
        let html = "<pre>line one\n  indented\nline three</pre>";

        let text = extract_readable(html).text;

        assert!(text.contains("line one\n  indented"), "{text:?}");
    }

    #[test]
    fn comments_and_doctypes_are_skipped() {
        let html = "<!DOCTYPE html><!-- hidden note --><p>Visible.</p>";

        assert_eq!(extract_readable(html).text, "Visible.");
    }

    #[test]
    fn unclosed_and_malformed_markup_does_not_panic() {
        for html in [
            "<p>unclosed",
            "<div><span>a</div>",
            "<<>>",
            "<p>a < b</p>",
            "<div class=",
            "",
            "&#xZZ;",
        ] {
            let _ = extract_readable(html);
        }
    }

    #[test]
    fn attribute_values_are_read_in_either_quoting_style() {
        assert_eq!(
            attribute_value(" class=\"a b\" id='x'", "class").as_deref(),
            Some("a b")
        );
        assert_eq!(
            attribute_value(" class=\"a\" id='x'", "id").as_deref(),
            Some("x")
        );
        assert_eq!(attribute_value(" data-class=\"a\"", "class"), None);
        assert_eq!(attribute_value(" hidden", "class"), None);
    }

    #[test]
    fn boilerplate_detection_is_token_based() {
        assert!(is_boilerplate_attribute("primary-navbar"));
        assert!(is_boilerplate_attribute("wrapper cookie-banner"));
        assert!(is_boilerplate_attribute("nav"));
        assert!(!is_boilerplate_attribute("advanced"));
        assert!(!is_boilerplate_attribute("header content"));
        assert!(!is_boilerplate_attribute("comments answers"));
    }

    #[test]
    fn truncation_is_character_safe_and_reports_itself() {
        let (kept, truncated) = truncate_chars("héllo wörld", 100);
        assert_eq!(kept, "héllo wörld");
        assert!(!truncated);

        let (kept, truncated) = truncate_chars("héllo wörld", 4);
        assert_eq!(kept, "héll");
        assert!(truncated);

        // A paragraph break near the end is preferred as the cut point.
        let text = format!("{}\n\ntail", "a".repeat(80));
        let (kept, truncated) = truncate_chars(&text, 84);
        assert_eq!(kept, "a".repeat(80));
        assert!(truncated);
    }
}
