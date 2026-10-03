//! Reading a CSV side channel without losing everything that is not ASCII.
//!
//! `wind_base::csv::parse_row` walks a record one *byte* at a time and casts each byte with
//! `bytes[i] as char` (base/src/csv.rs:45). For a field that is pure ASCII that is invisible. For the
//! window titles this project actually stores — this machine's OCR language is `zh-Hans-CN`, and a
//! Chinese title is three UTF-8 bytes per character — it turns `项目` into `é¡¹`, i.e. two
//! Latin-1 characters where one Chinese character was meant. Quoting state is decided the same
//! byte-wise way, so a quoted field is at least parsed, but every non-ASCII byte survives as its own
//! character.
//!
//! That is why this module exists rather than a call into `wind_base::csv`: a re-indexed row whose
//! title is mojibake is a permanent wrong answer in the user's index, and the title is also part of the
//! text a search runs over. The reader below is the same grammar — pandas-compatible quoting, doubled
//! quotes, embedded newlines — over `char`s instead of bytes.
//!
//! Reported as a gap: fixing `wind_base::csv` to decode UTF-8 properly would let this module and every
//! other non-ASCII CSV read in the workspace (the flag/note editor and the notes store go through it
//! today) collapse onto one implementation.

use std::io::Read;
use std::path::Path;

/// Every record in `text`, header optionally dropped. Blank lines are skipped, as `wind_base::csv`
/// skips them, so a file appended to by two writers does not yield an empty row.
pub fn parse_all(text: &str, skip_header: bool) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut touched = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        touched = true;
        match c {
            // A doubled quote inside a quoted field is one literal quote.
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            '"' => quoted = !quoted,
            ',' if !quoted => record.push(std::mem::take(&mut field)),
            '\r' if !quoted => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                close(&mut record, &mut field, &mut rows);
                touched = false;
            }
            '\n' if !quoted => {
                close(&mut record, &mut field, &mut rows);
                touched = false;
            }
            other => field.push(other),
        }
    }
    // A final record with no trailing newline is still a record; one that was only a newline is not.
    if touched {
        close(&mut record, &mut field, &mut rows);
    }

    if skip_header {
        rows.remove(0);
    }
    rows.retain(|row| !(row.len() == 1 && row[0].is_empty()));
    rows
}

fn close(record: &mut Vec<String>, field: &mut String, rows: &mut Vec<Vec<String>>) {
    record.push(std::mem::take(field));
    rows.push(std::mem::take(record));
}

/// Read a CSV file as UTF-8 text. A missing file is an empty list, not an error: an unlogged day is
/// normal, and the caller's answer is "no titles", exactly as upstream returns.
pub fn read(path: &Path, skip_header: bool) -> Vec<Vec<String>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };
    let mut text = String::new();
    // `read_to_string` is strict: a file that is not valid UTF-8 is a corrupt side channel, and
    // decoding it as Latin-1 to keep going is precisely the failure this module exists to avoid.
    if file.read_to_string(&mut text).is_err() {
        return Vec::new();
    }
    parse_all(&text, skip_header)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chinese_field_survives_the_round_trip() {
        // The exact shape of the bug: 项 is the three bytes E9 A1 B9, which a byte-wise reader turns
        // into three characters and a correct reader keeps as one.
        let title = "(3) 项目讨论 – (283859)";
        let rows = parse_all(&format!("datetime,window_title,deep_linking\n2026-09-21 21:16:15,{title},\n"), true);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], title);
        assert_eq!(title.as_bytes().len(), 29, "the file is 29 bytes wide");
        assert_eq!(rows[0][1].chars().count(), 19, "but the field is 19 characters, not 29");
    }

    #[test]
    fn quoting_and_embedded_newlines_match_the_grammar_wind_base_documents() {
        let rows = parse_all("a,\"two\nlines\",c\nnext,row,here\n", false);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec!["a", "two\nlines", "c"]);
        assert_eq!(rows[1], vec!["next", "row", "here"]);

        let rows = parse_all("x,\"say \"\"hi\"\"\",y\n", false);
        assert_eq!(rows[0][1], "say \"hi\"");

        let rows = parse_all("h\n2026-01-01 00:00:00,Notepad,\r\n", true);
        assert_eq!(rows, vec![vec!["2026-01-01 00:00:00", "Notepad", ""]]);
    }

    #[test]
    fn an_empty_or_header_only_file_has_no_records() {
        assert!(parse_all("", false).is_empty());
        assert!(parse_all("\n", false).is_empty(), "a lone newline is not a record");
        assert!(parse_all("datetime,window_title,deep_linking\n", true).is_empty());
        assert!(parse_all("a,b\n\n", true).is_empty(), "blank lines are skipped, not blank records");
    }

    #[test]
    fn a_trailing_record_with_no_newline_is_still_read() {
        let rows = parse_all("h\n2026-01-01 00:00:00,Notepad", true);
        assert_eq!(rows, vec![vec!["2026-01-01 00:00:00", "Notepad"]]);
    }

    #[test]
    fn a_missing_or_undecodable_file_is_empty_rather_than_a_panic() {
        let dir = std::env::temp_dir().join(format!("windcap-reindex-csvread-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(read(&dir.join("nope.csv"), true).is_empty());

        // Bytes that cannot be UTF-8 at all — 0x80 and 0x81 are never a leading byte. The reader
        // refuses the file rather than inventing characters for it, which is the failure mode this
        // module exists to avoid. (A GBK file can happen to be valid UTF-8 too; guessing a code page
        // from bytes that decode either way is not this reader's job, and the writer is UTF-8.)
        std::fs::write(dir.join("broken.csv"), [0x80u8, 0x81, 0x0a]).unwrap();
        assert!(read(&dir.join("broken.csv"), false).is_empty(), "an undecodable day yields nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_reader_agrees_with_wind_base_on_pure_ascii() {
        // The one case where the byte-wise reader was already right, pinned so this module can be
        // deleted the moment wind-base is fixed without changing any behaviour that exists today.
        let text = "h\n2026-09-21 21:16:12,Notepad,\n";
        assert_eq!(parse_all(text, true), wind_base::csv::parse_all(text, true));
    }
}
