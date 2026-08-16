//! Turning one SQLite cell into one JSON value a model can read.
//!
//! Three things make this less trivial than `serde_json::to_value`:
//!
//! * SQLite `TEXT` is arbitrary bytes, not guaranteed UTF-8.
//! * SQLite `REAL` can be NaN or infinity, which JSON has no number for.
//! * A single cell can be a 200 MB BLOB, which would blow the response budget
//!   before the row cap ever came into play.
//!
//! Every conversion also reports an approximate serialized cost, which is what
//! the byte cap in `query.rs` is spent against. The estimate is deliberately
//! cheap: it is used to stop reading early, not to size a buffer.

use rusqlite::types::ValueRef;
use serde_json::{Value, json};

/// Marker appended to a value that was shortened. It is visible in the value
/// itself, on purpose — a model must never be handed a silently truncated
/// string it will then reason about as if it were complete.
const TRUNCATION_SUFFIX_PREFIX: &str = "…[+";

/// One converted cell.
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub value: Value,
    /// Approximate bytes this cell will occupy once serialized.
    pub cost: usize,
    /// True when the stored value was longer than `max_cell_bytes`.
    pub truncated: bool,
}

/// Convert one cell, shortening anything longer than `max_cell_bytes`.
pub fn cell_from(value: ValueRef<'_>, max_cell_bytes: usize) -> Cell {
    match value {
        ValueRef::Null => Cell {
            value: Value::Null,
            cost: 4,
            truncated: false,
        },
        ValueRef::Integer(number) => Cell {
            value: json!(number),
            cost: 20,
            truncated: false,
        },
        ValueRef::Real(number) => real_cell(number),
        ValueRef::Text(bytes) => text_cell(bytes, max_cell_bytes),
        ValueRef::Blob(bytes) => blob_cell(bytes, max_cell_bytes),
    }
}

/// JSON has no NaN or infinity. Rather than quietly emitting `null` — which a
/// model would read as "the column was empty" — those become the strings
/// SQLite itself prints.
fn real_cell(number: f64) -> Cell {
    let value = if number.is_finite() {
        json!(number)
    } else if number.is_nan() {
        json!("NaN")
    } else if number.is_sign_positive() {
        json!("Inf")
    } else {
        json!("-Inf")
    };
    Cell {
        value,
        cost: 24,
        truncated: false,
    }
}

fn text_cell(bytes: &[u8], max_cell_bytes: usize) -> Cell {
    // SQLite does not enforce UTF-8 on TEXT, so a lossy decode is the only
    // conversion that cannot fail on real-world data.
    let text = String::from_utf8_lossy(bytes);
    let (kept, dropped) = truncate_on_char_boundary(&text, max_cell_bytes);
    let value = match dropped {
        0 => kept.to_string(),
        dropped => format!("{kept}{TRUNCATION_SUFFIX_PREFIX}{dropped} bytes truncated]"),
    };
    Cell {
        cost: value.len() + 2,
        value: Value::String(value),
        truncated: dropped > 0,
    }
}

/// A BLOB becomes a small object rather than a string, so a model cannot
/// mistake a hex dump for the column's text value.
fn blob_cell(bytes: &[u8], max_cell_bytes: usize) -> Cell {
    let hex_budget = (max_cell_bytes / 2).max(1);
    let shown = bytes.len().min(hex_budget);
    let hex: String = bytes[..shown]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let truncated = shown < bytes.len();
    let value = json!({
        "type": "blob",
        "bytes": bytes.len(),
        "hex": hex,
        "hex_complete": !truncated,
    });
    Cell {
        cost: shown * 2 + 48,
        value,
        truncated,
    }
}

/// Cut a string to at most `max_bytes` without splitting a character, and
/// report how many bytes were dropped.
///
/// Returned as `(kept, dropped)` rather than a bool so the marker can name the
/// exact size that went missing.
pub fn truncate_on_char_boundary(text: &str, max_bytes: usize) -> (&str, usize) {
    if text.len() <= max_bytes {
        return (text, 0);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], text.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_integer_and_text_round_trip_unchanged() {
        assert_eq!(cell_from(ValueRef::Null, 64).value, Value::Null);
        assert_eq!(cell_from(ValueRef::Integer(-7), 64).value, json!(-7));
        assert_eq!(
            cell_from(ValueRef::Text(b"hello"), 64).value,
            json!("hello")
        );
        assert!(!cell_from(ValueRef::Text(b"hello"), 64).truncated);
    }

    #[test]
    fn a_finite_real_stays_a_number_and_a_non_finite_one_becomes_a_named_string() {
        assert_eq!(cell_from(ValueRef::Real(1.5), 64).value, json!(1.5));
        assert_eq!(cell_from(ValueRef::Real(f64::NAN), 64).value, json!("NaN"));
        assert_eq!(
            cell_from(ValueRef::Real(f64::INFINITY), 64).value,
            json!("Inf")
        );
        assert_eq!(
            cell_from(ValueRef::Real(f64::NEG_INFINITY), 64).value,
            json!("-Inf")
        );
    }

    #[test]
    fn invalid_utf8_text_is_decoded_lossily_instead_of_failing_the_query() {
        let cell = cell_from(ValueRef::Text(&[b'a', 0xff, b'b']), 64);
        assert_eq!(cell.value, json!("a\u{fffd}b"));
        assert!(!cell.truncated);
    }

    #[test]
    fn long_text_is_cut_and_says_so_inside_the_value() {
        let cell = cell_from(ValueRef::Text(&b"x".repeat(100)), 10);
        let text = cell.value.as_str().expect("text stays a string");
        assert!(cell.truncated);
        assert!(text.starts_with("xxxxxxxxxx"), "{text}");
        assert!(text.contains("90 bytes truncated"), "{text}");
    }

    #[test]
    fn truncation_never_splits_a_multi_byte_character() {
        // "é" is two bytes, so a 3-byte budget must keep one character, not one
        // and a half.
        let (kept, dropped) = truncate_on_char_boundary("éé", 3);
        assert_eq!(kept, "é");
        assert_eq!(dropped, 2);
        assert!(kept.is_char_boundary(kept.len()));

        let (kept, dropped) = truncate_on_char_boundary("é", 1);
        assert_eq!(kept, "");
        assert_eq!(dropped, 2);
    }

    #[test]
    fn short_text_is_returned_whole() {
        assert_eq!(truncate_on_char_boundary("abc", 3), ("abc", 0));
        assert_eq!(truncate_on_char_boundary("abc", 99), ("abc", 0));
    }

    #[test]
    fn a_blob_reports_its_true_size_even_when_the_hex_dump_is_cut() {
        let cell = cell_from(ValueRef::Blob(&[0xde, 0xad, 0xbe, 0xef]), 4);
        assert_eq!(cell.value["type"], json!("blob"));
        assert_eq!(cell.value["bytes"], json!(4));
        assert_eq!(cell.value["hex"], json!("dead"));
        assert_eq!(cell.value["hex_complete"], json!(false));
        assert!(cell.truncated);

        let whole = cell_from(ValueRef::Blob(&[0x01, 0x02]), 64);
        assert_eq!(whole.value["hex"], json!("0102"));
        assert_eq!(whole.value["hex_complete"], json!(true));
        assert!(!whole.truncated);
    }

    #[test]
    fn cost_grows_with_the_kept_payload_not_the_stored_payload() {
        let small = cell_from(ValueRef::Text(b"abc"), 1024);
        let capped = cell_from(ValueRef::Text(&b"z".repeat(100_000)), 64);
        assert!(small.cost < capped.cost);
        assert!(
            capped.cost < 512,
            "a capped cell must not carry a six-figure cost: {}",
            capped.cost
        );
    }
}
