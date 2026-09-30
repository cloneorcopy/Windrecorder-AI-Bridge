//! Every way this crate can refuse, as one type with a sentence attached.
//!
//! The distinction the variants make is between *the caller's typo* and *the install's state*. A bad
//! reference is the AI's mistake and the message has to say what it accepted; an unreadable day file
//! is this machine's problem and the message has to name the file, because "no summaries exist" would
//! be a lie that a reader cannot tell from the truth.

use std::fmt;
use std::path::PathBuf;

#[derive(Debug)]
pub enum SummaryError {
    /// A reference that is none of the three accepted shapes (segment filename, segment stamp, an
    /// instant inside a recorded segment).
    BadReference(String),
    /// A well-formed reference that no recorded segment answers to. Distinct from
    /// [`SummaryError::BadReference`] on purpose: one is "you wrote it wrong", the other is "that
    /// stretch was never recorded", and an agent can only fix the first one by rewriting.
    UnknownSegment(String),
    /// A `YYYY-MM-DD` that the calendar does not have, or a date in another shape.
    BadDate(String),
    /// A summary file that exists and will not parse. Carries the path, because the fix is a
    /// filesystem operation somebody has to perform by hand.
    Unreadable { path: PathBuf, why: String },
    /// The write lock is held by a live process other than this one.
    Locked(String),
    Io(std::io::Error),
    Store(wind_store::StoreError),
}

impl fmt::Display for SummaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SummaryError::BadReference(text) => write!(
                f,
                "`{text}` is not a recording segment. Name one by its file (`2026-09-27_15-47-17.mp4`), \
                 by the stamp alone (`2026-09-27_15-47-17`), or by a `timestamp` that falls inside it."
            ),
            SummaryError::UnknownSegment(text) => {
                write!(f, "no recorded segment answers to `{text}` in this range; nothing was written")
            }
            SummaryError::BadDate(text) => {
                write!(f, "`{text}` is not a day; expected YYYY-MM-DD")
            }
            SummaryError::Unreadable { path, why } => {
                write!(f, "{} exists and cannot be read as a summary file: {why}", path.display())
            }
            SummaryError::Locked(why) => write!(f, "cannot take the summary write lock: {why}"),
            SummaryError::Io(e) => write!(f, "io: {e}"),
            SummaryError::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SummaryError {}

impl From<std::io::Error> for SummaryError {
    fn from(e: std::io::Error) -> Self {
        SummaryError::Io(e)
    }
}

impl From<wind_store::StoreError> for SummaryError {
    fn from(e: wind_store::StoreError) -> Self {
        SummaryError::Store(e)
    }
}
