//! JSON written by hand, because `--json` has to be `jq`-safe and a dependency is not free.
//!
//! Only the pieces `windcapctl query --json` needs exist here: a flat object and a string escaper.
//! The one non-obvious rule is what happens to control characters — JSON forbids a raw U+0001..U+001F
//! inside a string, and OCR text pulled off a console can absolutely contain one, so emitting it raw
//! would produce output that `jq` rejects on a single row out of a thousand. Those are escaped as
//! `\u00XX`, which keeps the file valid on a machine nobody can predict the data of.
//!
//! Non-ASCII is emitted as literal UTF-8, not `\uXXXX`: it keeps the lines readable, and every
//! consumer of this format (`jq`, Python, a file redirect) takes UTF-8.

use std::fmt::Write as _;

/// A JSON value narrow enough to build a flat object with, holding nothing but references.
#[derive(Debug, Clone, Copy)]
pub enum Field<'a> {
    Text(&'a str),
    /// `None` renders as `null`, which is how an absent `offset_in_segment` stays distinguishable
    /// from a zero-length one.
    OptionalInt(Option<i64>),
    Int(i64),
    Bool(bool),
}

/// Escape a string for the inside of a JSON quoted value.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Everything else below 0x20 has no short form and must not appear raw.
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

pub fn quoted(text: &str) -> String {
    format!("\"{}\"", escape(text))
}

/// One flat object, keys in the order given, on a single line.
///
/// One line per record is the contract with `jq`: a pretty-printed object would break every
/// `while read` loop and `jq -c` pipeline that swallows this output.
pub fn object(fields: &[(&str, Field)]) -> String {
    let body = fields
        .iter()
        .map(|(key, value)| {
            let rendered = match value {
                Field::Text(text) => quoted(text),
                Field::Int(n) => n.to_string(),
                Field::OptionalInt(Some(n)) => n.to_string(),
                Field::OptionalInt(None) => "null".to_string(),
                Field::Bool(b) => b.to_string(),
            };
            format!("{}:{rendered}", quoted(key))
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{body}}}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four characters that break a hand-rolled escaper, in one string.
    #[test]
    fn quotes_backslashes_newlines_and_control_characters_are_all_escaped() {
        let nasty = "she said \"no\"\rthen a backslash \\\nand a bell \u{7} and a NUL \u{0}";
        let line = object(&[("body", Field::Text(nasty))]);
        assert!(!line.contains('\n'), "a raw newline would split the record: {line:?}");
        assert!(!line.contains('\r'));
        assert!(!line.contains('\u{7}'));
        assert!(!line.contains('\u{0}'));
        assert!(line.contains("\\\"no\\\""), "{line}");
        assert!(line.contains("\\\\"), "{line}");
        assert!(line.contains("\\r"), "{line}");
        assert!(line.contains("\\n"), "{line}");
        assert!(line.contains("\\u0007"), "{line}");
        assert!(line.contains("\\u0000"), "{line}");
        assert!(line.starts_with('{') && line.ends_with('}'));
        assert_eq!(line.chars().filter(|c| *c == '\n').count(), 0);
    }

    #[test]
    fn non_ascii_survives_as_itself() {
        let line = object(&[("body", Field::Text("文件 ChatGPT，新聊天"))]);
        assert_eq!(line, r#"{"body":"文件 ChatGPT，新聊天"}"#);
    }

    #[test]
    fn an_absent_integer_is_null_and_a_real_zero_is_zero() {
        assert_eq!(object(&[("offset", Field::OptionalInt(None))]), r#"{"offset":null}"#);
        assert_eq!(object(&[("offset", Field::OptionalInt(Some(0)))]), r#"{"offset":0}"#);
        assert_eq!(object(&[("exists", Field::Bool(false)), ("rowid", Field::Int(-1))]),
            r#"{"exists":false,"rowid":-1}"#);
    }

    #[test]
    fn key_names_are_escaped_too_because_they_are_still_strings() {
        assert_eq!(object(&[("we\"ird", Field::Int(1))]), r#"{"we\"ird":1}"#);
    }

    #[test]
    fn an_empty_object_is_still_valid_json() {
        assert_eq!(object(&[]), "{}");
    }
}
