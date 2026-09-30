//! `wind-summary` — the two artefact families the product did not have: what each recorded stretch
//! was about, and what each day was about.
//!
//! # Why this is a crate and not a module of the bridge
//!
//! The same summaries have two producers from the first release: an external AI writing them over MCP,
//! and this machine's own `windai summarize` writing them through the configured endpoint. Every rule
//! that decides whether a day is *done* — which segments exist, which have text, whether that text
//! still describes what is on disk — has to answer the same way for both of them, or the two producers
//! drift apart and the daily gate becomes a coin flip whose side depends on who asked. That makes the
//! rules a component with its own interface, and the bridge and the CLI its first two thin clients.
//!
//! ```text
//!   windai summarize ──┐                      ┌── windrecorder_summaries_pending
//!                      ├─→ wind-summary ←─────┤
//!   windmaint (idle) ──┘                      └── windrecorder_period_summary_write
//! ```
//!
//! # What it owns, and what it never touches
//!
//! Two fixed directories under `userdata/`, one JSON file per product day:
//!
//!   * `result_ai_period_summary/2026-09-27.json` — a map of segment key to [`entries::PeriodSummary`]
//!   * `result_ai_daily_summary/2026-09-27.json` — one [`entries::DaySummary`]
//!
//! The monthly index is read-only from here, through `wind-store`, for exactly two purposes: which
//! segments a day holds, and what their content fingerprints are. No code in this crate can write a
//! row, and it has no rusqlite dependency to make that possible. The recorder's files, its videos, and
//! the nine-column `video_text` contract are not this crate's business.
//!
//! # Why a product day is the file unit
//!
//! A segment is 1–15 minutes in practice (measured on a live install: 70 segments, 1 752 rows, one
//! afternoon), so a day is tens of entries and a year is thousands. One file per year would mean
//! re-reading and re-writing megabytes to add one afternoon's paragraph; one file per *segment* would
//! mean tens of thousands of dentries in a folder a human might open. Per product day is the width
//! both the gate and a reader actually work at: the day's file *is* the day's coverage.
//!
//! The day is the product's own, beginning at `day_begin_minutes` (03:00 as shipped), resolved by
//! [`keys::day_of`] so a 02:50 segment files under yesterday — the same rule `windrecorder_search
//! --day` already answers for. Two definitions of "which day" is how a gate passes and the reader
//! then shows nothing.
//!
//! # The two fingerprints
//!
//! [`fingerprint`] produces two independent 64-bit digests, and the difference between them is the
//! difference between two real states that would otherwise both look like "there is a summary":
//!
//!   * `source_fingerprint` — over the segment's (time, title, URL, text) sequence. It changes when
//!     the *content* changes: a `windmaint forget` that blanked the rows, an `expire` that removed the
//!     segment, a re-index. Equal means the summary still describes what is on disk, and means nobody
//!     has to redo the work — regardless of who wrote it, which is what keeps the idle pass from
//!     spending a token on a day an outside AI already finished.
//!   * `prompt_fingerprint` — over the prompt text that was in force when the summary was written.
//!     It changes when the *question* changes, i.e. when the user edits a prompt in Settings. This
//!     crate never reads a prompt: the caller passes the current digest in, because the prompt loader
//!     lives with the other text the product shows the user, not with this storage layer.

pub mod coverage;
pub mod entries;
pub mod error;
pub mod files;
pub mod fingerprint;
pub mod keys;
pub mod segments;

pub mod test_support;

pub use coverage::{daily_inputs_for, for_day, for_day_of_segments, for_day_with, DailyReason, DailyState, DayQueue, Item, PromptDigests, Reason};
pub use entries::{Coverage, DaySummary, PeriodSummary};
pub use error::SummaryError;
pub use files::{
    days_in_range, days_present, dir, lock, newest_file, prune_daily, prune_period, read_daily, read_daily_range, read_period,
    read_period_range, mark_daily_stale, now_stamp, write_daily, write_period, DailyFile, DayMap, FileLock, Kind, WriteOutcome, DAILY_DIR, LOCK_NAME, PERIOD_DIR,
};
pub use keys::{canonical_key, day_of, day_span, days_in_range as days_between};
pub use segments::{compact_duration, DayRead, Frame, Reader, Refresh, Segment, STRADDLE_MARGIN};
