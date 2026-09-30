//! Comma-separated side channels that are on-disk contracts, not implementation details.
//!
//! Two files matter: `cache/win_title/{date}.csv`, appended by the window-title thread and joined
//! back onto OCR rows by timestamp, and `userdata/flag_mark_note.csv`, the user's own bookmarks.
//! Upstream writes both with pandas, which quotes a field only when it contains a comma, a quote
//! or a newline and doubles an embedded quote — so a window title like `Fix "q", now` survives.
//! Anything that reads or writes these has to agree byte-for-byte or the user's notes break.

use std::io::Read;
use std::path::Path;

/// One parsed record; fields are already unquoted and joined across embedded newlines.
pub type Record = Vec<String>;

pub fn escape_field(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

pub fn format_row(fields: &[impl AsRef<str>]) -> String {
    let mut out = String::new();
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&escape_field(f.as_ref()));
    }
    out
}

/// Split one logical record, honouring quoted fields that contain commas or newlines.
///
/// `None` means the record is still incomplete (an unclosed quote), which for a streamed read is
/// the signal to pull more bytes rather than an error.
pub fn parse_row(text: &str) -> Option<(Record, usize)> {
    let bytes = text.as_bytes();
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut i = 0usize;

    while i < bytes.len() {
        let ch = bytes[i] as char;
        match ch {
            '"' if in_quotes && i + 1 < bytes.len() && bytes[i + 1] == b'"' => {
                field.push('"');
                i += 2;
                continue;
            }
            '"' => {
                in_quotes = !in_quotes;
            }
            ',' if !in_quotes => {
                fields.push(std::mem::take(&mut field));
            }
            '\r' if !in_quotes => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    i += 1;
                }
                fields.push(std::mem::take(&mut field));
                return Some((fields, i + 1));
            }
            '\n' if !in_quotes => {
                fields.push(std::mem::take(&mut field));
                return Some((fields, i + 1));
            }
            other => field.push(other),
        }
        i += 1;
    }
    if in_quotes {
        None
    } else {
        fields.push(field);
        Some((fields, i))
    }
}

/// Read every record, skipping the header line. An empty or missing file yields no records.
pub fn read_rows(path: &Path, skip_header: bool) -> std::io::Result<Vec<Record>> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(parse_all(&text, skip_header))
}

pub fn parse_all(text: &str, skip_header: bool) -> Vec<Record> {
    let mut rows = Vec::new();
    let mut offset = 0usize;
    let mut first = true;
    while offset < text.len() {
        match parse_row(&text[offset..]) {
            Some((row, consumed)) => {
                offset += consumed;
                if first && skip_header {
                    first = false;
                    continue;
                }
                first = false;
                if row.len() == 1 && row[0].is_empty() {
                    continue; // a blank line
                }
                rows.push(row);
            }
            None => break, // trailing partial record: ignore rather than corrupt
        }
    }
    rows
}

/// Append one record, creating the file — and writing the header — on first use.
pub fn append_row(path: &Path, header: &[&str], fields: &[impl AsRef<str>]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let needs_header = !path.exists() || std::fs::metadata(path).map(|m| m.len() == 0).unwrap_or(true);
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    if needs_header {
        writeln!(file, "{}", format_row(header))?;
    }
    writeln!(file, "{}", format_row(fields))?;
    Ok(())
}

/// Rewrite the whole file from records, header included. The flag/note editor saves this way.
pub fn write_rows(path: &Path, header: &[&str], rows: &[Record]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut body = format_row(header);
    body.push('\n');
    for row in rows {
        body.push_str(&format_row(row));
        body.push('\n');
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_matches_pandas() {
        assert_eq!(escape_field("plain"), "plain");
        assert_eq!(escape_field("a,b"), "\"a,b\"");
        assert_eq!(escape_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(escape_field("line\nbreak"), "\"line\nbreak\"");
    }

    #[test]
    fn a_title_with_commas_and_quotes_survives_a_round_trip() {
        let line = format_row(&["2026-09-21 21:16:12", "Fix \"q\", now", ""]);
        let (row, _) = parse_row(&line).unwrap();
        assert_eq!(row, vec!["2026-09-21 21:16:12", "Fix \"q\", now", ""]);
    }

    #[test]
    fn embedded_newlines_are_one_record_not_two() {
        let text = "a,\"two\nlines\",c\nnext,row,here\n";
        let rows = parse_all(text, false);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec!["a", "two\nlines", "c"]);
        assert_eq!(rows[1], vec!["next", "row", "here"]);
    }

    #[test]
    fn crlf_and_a_header_line_are_handled() {
        let text = "datetime,window_title,deep_linking\r\n2026-09-21 21:16:12,Notepad,\r\n";
        let rows = parse_all(text, true);
        assert_eq!(rows, vec![vec!["2026-09-21 21:16:12", "Notepad", ""]]);
    }

    #[test]
    fn a_truncated_quote_is_not_a_record() {
        assert!(parse_row("a,\"unclosed").is_none());
    }

    #[test]
    fn append_then_rewrite_is_stable() {
        let dir = std::env::temp_dir().join(format!("windcap-csv-{}", std::process::id()));
        let path = dir.join("flag_mark_note.csv");
        let _ = std::fs::remove_file(&path);
        const HEADER: &[&str] = &["thumbnail", "datetime", "note"];
        append_row(&path, HEADER, &["base64", "2026-09-21 21:16:12", "keep, this"]).unwrap();
        append_row(&path, HEADER, &["base64", "2026-09-21 22:00:00", "second"]).unwrap();
        let rows = read_rows(&path, true).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][2], "keep, this");

        write_rows(&path, HEADER, &rows[1..]).unwrap();
        assert_eq!(read_rows(&path, true).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
