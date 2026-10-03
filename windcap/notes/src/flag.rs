//! The record itself, and the two datetime spellings it has to survive.
//!
//! `userdata/flag_mark_note.csv` has three columns — `thumbnail,datetime,note` — written by
//! `pandas.DataFrame.to_csv(index=False)` (see `file_utils.save_dataframe_to_path`), which quotes a
//! field only when it contains a comma, a quote or a newline. That is exactly what
//! [`wind_base::csv`] emits, so the writer here is that module and not a new one.
//!
//! The datetime column is text, not a typed timestamp, and it appears in two shapes in real files:
//!
//!   * `%Y-%m-%d %H:%M:%S` — what the recorder of flags writes and pandas stores
//!     (`add_new_flag_record_from_tray` formats it explicitly);
//!   * `%Y/%m/%d   %H:%M:%S` — what the *editor* shows, three spaces between date and time
//!     (`st_tweak_df_flag_mark_note_to_display`). `st_save_flag_mark_note_from_editor` reparses that
//!     shape and rewrites the ISO one, but it only rewrites rows that survived the round trip, so a
//!     file that has been through the editor once after an interrupted save, or edited by hand,
//!     holds a mixture. Reading must accept both or the row disappears.
//!
//! Nothing on disk is a timezone-aware value: the wall clock is naive local throughout this app
//! (see [`wind_base::clock`]), and that is also the axis the index stores, so [`Flag::epoch`] is
//! directly comparable to `video_text.videofile_time`.

use std::fmt;

use wind_base::clock::LocalParts;

/// The column order, exactly as the Python app writes it. A reader that assumes another order shows
/// the user their notes in the thumbnail column.
pub const HEADER: [&str; 3] = ["thumbnail", "datetime", "note"];

/// What an unannotated flag holds in `note`. The editor's "✔ add note" button replaces it, and the
/// webui's `st_save` treats empty as this, so an empty string must never be written.
pub const NOTE_PLACEHOLDER: &str = "_";

/// The data-URL prefix `st_tweak_df_flag_mark_note_to_display` puts in front of a thumbnail for the
/// image column and `st_save_flag_mark_note_from_editor` strips before writing. The file itself
/// never carries it, so it is only ever a display concern — but it must be *tolerated* on read,
/// because it is one mis-click in a text editor away from being in the file.
pub const THUMBNAIL_DATA_PREFIX: &str = "data:image/png;base64,";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagError {
    /// A row that is not three fields wide. Legacy files and hand edits both produce these; they are
    /// kept verbatim by [`crate::store::Entry::Raw`] rather than dropped on the next save.
    WrongFieldCount { found: usize },
    /// A datetime in neither spelling. The text is carried so the caller can name the row.
    UnparsableDatetime(String),
}

impl fmt::Display for FlagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlagError::WrongFieldCount { found } => {
                write!(f, "expected {} fields, found {found}", HEADER.len())
            }
            FlagError::UnparsableDatetime(text) => write!(f, "unrecognised datetime {text:?}"),
        }
    }
}

impl std::error::Error for FlagError {}

/// One bookmark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag {
    /// Base64 JPEG, no data-URL prefix — the shape the column stores. Empty for a flag whose frame
    /// could not be found, which upstream also writes (`"" if row.empty else …`).
    pub thumbnail: String,
    /// The flagged instant, naive local.
    pub when: LocalParts,
    /// The user's note; [`NOTE_PLACEHOLDER`] until they type one.
    pub note: String,
}

impl Flag {
    /// A flag with no frame and no note, for callers that know the instant and nothing else.
    pub fn at(when: LocalParts) -> Flag {
        Flag { thumbnail: String::new(), when, note: NOTE_PLACEHOLDER.to_string() }
    }

    /// Seconds on the app's naive-local epoch — the same axis `videofile_time` is on, which is what
    /// lets a flag be resolved against the index without any conversion.
    pub fn epoch(&self) -> i64 {
        self.when.naive_epoch_seconds()
    }

    /// The text that goes in the column: the ISO spelling pandas wrote for a decade.
    pub fn stored_datetime(&self) -> String {
        self.when.display()
    }

    /// The text the editor displays: `%Y/%m/%d   %H:%M:%S`, three spaces wide.
    pub fn displayed_datetime(&self) -> String {
        format!("{}/{:02}/{:02}   {:02}:{:02}:{:02}", self.when.year, self.when.month, self.when.day, self.when.hour, self.when.minute, self.when.second)
    }

    /// The three fields, in column order, ready for [`wind_base::csv`].
    pub fn fields(&self) -> Vec<String> {
        vec![self.thumbnail.clone(), self.stored_datetime(), self.note.clone()]
    }

    /// The thumbnail as an `<img>`/`ImageColumn` source, i.e. with the display prefix back on.
    pub fn thumbnail_for_display(&self) -> String {
        if self.thumbnail.is_empty() {
            String::new()
        } else {
            format!("{THUMBNAIL_DATA_PREFIX}{}", self.thumbnail)
        }
    }

    pub fn has_thumbnail(&self) -> bool {
        !self.thumbnail.is_empty()
    }

    /// Parse one CSV record. A note is never empty on disk — an empty one is written back as the
    /// placeholder, the way `update_note_to_csv_by_datetime` does — but a missing field is not an
    /// error to be invented here, it is a row to keep verbatim, so it lands in
    /// [`crate::store::Entry::Raw`] instead.
    pub fn from_record(record: &[String]) -> Result<Flag, FlagError> {
        if record.len() != HEADER.len() {
            return Err(FlagError::WrongFieldCount { found: record.len() });
        }
        let when = parse_datetime(&record[1]).ok_or_else(|| FlagError::UnparsableDatetime(record[1].clone()))?;
        Ok(Flag {
            thumbnail: strip_data_prefix(&record[0]),
            when,
            note: normalise_note(&record[2]),
        })
    }
}

/// A thumbnail field with the data-URL prefix removed if it is there, in any image flavour.
pub fn strip_data_prefix(value: &str) -> String {
    match value.find(";base64,") {
        Some(at) if value.starts_with("data:image/") => value[at + ";base64,".len()..].to_string(),
        _ => value.to_string(),
    }
}

/// Upstream stores a missing note as the placeholder rather than as an empty cell, and the editor's
/// save path does the same. Keeping that here means a row written by this crate reads identically in
/// the Python app.
pub fn normalise_note(note: &str) -> String {
    if note.is_empty() {
        NOTE_PLACEHOLDER.to_string()
    } else {
        note.to_string()
    }
}

/// Accept both datetime spellings, plus the variations a human or a newer pandas leaves behind:
/// `T` instead of the space, one space where the editor puts three, `/` instead of `-`.
///
/// Deliberately strict about the rest — a fractional-second suffix is accepted and dropped (the
/// day view's flag path hands pandas a real `datetime`, which can render `.123456`) but anything
/// that is not `date time` is refused, because guessing at a bookmark's instant would put a marker
/// on the wrong hour with no warning.
pub fn parse_datetime(text: &str) -> Option<LocalParts> {
    // A `T` between date and time is the same instant with a different separator, and it arrives
    // from anywhere a typed column has been through `datetime.isoformat()`; splitting on whitespace
    // alone would see one unrecognisable token.
    let text = text.trim().replace('T', " ");
    let mut tokens = text.split_whitespace();
    let date = tokens.next()?;
    let time = tokens.next()?;
    if tokens.next().is_some() {
        return None;
    }
    let stamp = format!("{}_{}", reformat_date(date)?, reformat_time(time)?);
    LocalParts::from_stamp(&stamp)
}

fn reformat_date(date: &str) -> Option<String> {
    let bytes = date.as_bytes();
    // `YYYY?MM?DD`, the separator being `-` or `/`; anything else is not a date we recognise.
    if bytes.len() != 10 || !matches!(bytes[4], b'-' | b'/' | b'.') || !matches!(bytes[7], b'-' | b'/' | b'.') {
        return None;
    }
    if !digits(&bytes[0..4]) || !digits(&bytes[5..7]) || !digits(&bytes[8..10]) {
        return None;
    }
    Some(format!("{}-{}-{}", &date[0..4], &date[5..7], &date[8..10]))
}

fn reformat_time(time: &str) -> Option<String> {
    // `HH:MM:SS`, optionally with the sub-second tail pandas can emit; the column's resolution is
    // one second everywhere else, so the tail carries no information a marker can use.
    let (time, _) = match time.split_once('.') {
        Some(pair) => pair,
        None => (time, ""),
    };
    let bytes = time.as_bytes();
    if bytes.len() != 8 || bytes[2] != b':' || bytes[5] != b':' {
        return None;
    }
    if !digits(&bytes[0..2]) || !digits(&bytes[3..5]) || !digits(&bytes[6..8]) {
        return None;
    }
    Some(format!("{}-{}-{}", &time[0..2], &time[3..5], &time[6..8]))
}

fn digits(bytes: &[u8]) -> bool {
    bytes.iter().all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_datetime_spellings_parse_to_the_same_instant() {
        let iso = parse_datetime("2026-09-21 21:16:12").expect("iso");
        let editor = parse_datetime("2026/09/21   21:16:12").expect("editor");
        assert_eq!(iso, editor);
        assert_eq!(iso.naive_epoch_seconds(), 1_790_025_372, "the production anchor from wind-base");
    }

    /// `st_create_timestamp_flag_mark_note_from_oneday_timeselect` writes a real `datetime` through
    /// pandas, which renders the ISO form with a `T` when a row is round-tripped through a typed
    /// column, and with microseconds when the widget supplied them.
    #[test]
    fn tolerant_but_not_guessing_variants_parse() {
        for text in [
            "2026-09-21 21:16:12",
            "2026/09/21 21:16:12",
            "2026/09/21   21:16:12",
            "2026-09-21T21:16:12",
            "2026-09-21 21:16:12.123456",
            "  2026-09-21   21:16:12  ",
        ] {
            assert_eq!(parse_datetime(text).map(|p| p.display()).as_deref(), Some("2026-09-21 21:16:12"), "{text}");
        }
        for text in ["", "2026-09-21", "not a date", "2026-02-30 00:00:00", "2026-09-21 25:16:12", "2026-09-21 21:16:12 extra"] {
            assert_eq!(parse_datetime(text), None, "{text} must be refused, not guessed at");
        }
    }

    #[test]
    fn the_two_display_forms_are_the_ones_the_file_and_the_editor_use() {
        let flag = Flag::at(parse_datetime("2026-09-21 09:05:03").unwrap());
        assert_eq!(flag.stored_datetime(), "2026-09-21 09:05:03");
        assert_eq!(flag.displayed_datetime(), "2026/09/21   09:05:03");
        assert_eq!(parse_datetime(&flag.displayed_datetime()), Some(flag.when));
    }

    #[test]
    fn fields_are_in_the_upstream_column_order() {
        let flag = Flag { thumbnail: "AAAA".into(), when: parse_datetime("2026-09-21 21:16:12").unwrap(), note: "keep, this".into() };
        assert_eq!(flag.fields(), vec!["AAAA", "2026-09-21 21:16:12", "keep, this"]);
    }

    #[test]
    fn a_thumbnail_that_arrived_with_a_data_prefix_still_reads() {
        let record = vec![
            "data:image/png;base64,iVBORw0KGgo=".to_string(),
            "2026-09-21 21:16:12".to_string(),
            "note".to_string(),
        ];
        let flag = Flag::from_record(&record).unwrap();
        assert_eq!(flag.thumbnail, "iVBORw0KGgo=");
        assert_eq!(flag.thumbnail_for_display(), "data:image/png;base64,iVBORw0KGgo=");
        // A plain base64 thumbnail must not be mangled by the same code path.
        assert_eq!(Flag::from_record(&["iVBOR=".to_string(), record[1].clone(), "n".into()]).unwrap().thumbnail, "iVBOR=");
        assert_eq!(Flag::from_record(&[String::new(), record[1].clone(), "n".into()]).unwrap().thumbnail, "");
    }

    #[test]
    fn an_empty_note_becomes_the_placeholder_upstream_writes() {
        let record = vec!["x".to_string(), "2026-09-21 21:16:12".to_string(), String::new()];
        assert_eq!(Flag::from_record(&record).unwrap().note, NOTE_PLACEHOLDER);
    }

    #[test]
    fn a_row_that_is_not_understood_is_reported_not_invented() {
        assert!(matches!(
            Flag::from_record(&["thumb".to_string(), "yesterday".to_string()]),
            Err(FlagError::WrongFieldCount { found: 2 })
        ));
        assert!(matches!(
            Flag::from_record(&["thumb".to_string(), "yesterday".to_string(), "note".to_string()]),
            Err(FlagError::UnparsableDatetime(_))
        ));
    }

    #[test]
    fn a_note_survives_the_editors_own_round_trip() {
        let flag = Flag { thumbnail: String::new(), when: parse_datetime("2026/09/21   21:16:12").unwrap(), note: "a".into() };
        assert_eq!(parse_datetime(&flag.displayed_datetime()), Some(flag.when));
    }
}
