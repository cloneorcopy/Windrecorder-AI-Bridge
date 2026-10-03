//! `wind-notes` — the flag/note subsystem, replacing `windrecorder/flag_mark_note.py`.
//!
//! A *flag* is a bookmark the user drops on their own history: an instant, the screen that was on
//! at that instant, and a note. It is the one part of the product whose data the user authored and
//! curates by hand, which sets the rules for everything below:
//!
//!   * **the CSV is the product** — `userdata/flag_mark_note.csv` is plain text the user has been
//!     editing in Excel and in the old webui for years. The column order, the datetime spelling,
//!     pandas' quoting style and the bare (prefix-less) base64 thumbnail are all load-bearing. A
//!     file written here must open unchanged in the Python app and vice versa — see [`flag`].
//!   * **a save never loses a row** — no-op saves leave the file's bytes and mtime alone, an
//!     append never rewrites the rows beside it, and a whole-file rewrite refuses to run over a file
//!     that changed since it was read. Deleting every bookmark empties the table; it does not
//!     remove the file, which is the upstream bug this crate fixes on purpose — see [`store`].
//!   * **the markers have to land on the right hour** — the day view's timeline strip is the only
//!     place a bookmark is *seen*, and its x position is a proportion of recorded time. That
//!     arithmetic is isolated as pure functions over integer seconds so it can be tested and printed
//!     from a terminal — see [`markers`].
//!
//! The tkinter editor that used to be here is now the native one in `windui`, which hosts the same
//! model by calling [`store::FlagStore`] — [`store::FlagStore::set_note_ref`] and
//! [`store::FlagStore::remove_ref`] are the editor's only two whole-table write paths, so the rewrite
//! of the file lives in this crate and nowhere else. The tray reaches [`capture::capture_frame`] for
//! the picture it files a flag with, and the day view draws the strip from [`markers::layout`].

pub mod capture;
pub mod flag;
pub mod markers;
pub mod store;

pub use capture::{Anchor, CreateOutcome, FlagSource};
pub use flag::{Flag, FlagError, NOTE_PLACEHOLDER, THUMBNAIL_DATA_PREFIX};
pub use markers::{DaySpan, Layout, Marker, BAR_WIDTH};
pub use store::{Entry, FlagStore, RowEdit, RowRef, SaveOutcome, StoreError};

/// The header, in the order upstream wrote it. `wind_base::csv`'s own tests use the same triple.
pub use flag::HEADER;
