//! Segment planning: what a "recording" is, and when one ends.
//!
//! The recorder's outer loop is not "grab a frame every N seconds" — it is a state machine that
//! decides when a run of frames becomes one video file and when the machine should stop recording
//! altogether. All of that is here, with no GDI, no subprocess and no clock, because every one of
//! these decisions is a data-loss risk if it is wrong: rotate too early and the user gets a
//! thousand six-second clips; fail to rotate and an interrupted run leaves a directory of frames
//! that nothing will ever turn into a video.
//!
//! The Python loop spreads this logic across `continuously_record_screen`,
//! `record_screen_via_screenshot_process` and a shared mutable counter set. Here it is one struct
//! whose whole state is visible in one `Debug` print.

use std::path::{Path, PathBuf};

use wind_base::clock::LocalParts;
use wind_base::paths;
use wind_store::maintain::duplicate_flags;
use wind_store::Record;

/// Tunables, all of them read from the same config keys the Python app writes.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Seconds of screen time a segment covers before it is closed and a new one opened.
    pub record_seconds: i64,
    /// Nominal gap between kept frames; a frame captured sooner than this after the last is early.
    pub interval_seconds: i64,
    /// Consecutive interruptions tolerated before the segment is abandoned.
    pub interrupt_limit: u32,
    /// Minutes of an unchanged screen before recording pauses entirely.
    pub pause_after_idle_minutes: f64,
    /// Similarity at which a frame repeats the previous one and is not indexed.
    pub repeat_text_similarity: f64,
    /// Similarity at which two rows *inside one segment* are the same screen, collapse-redundant.
    pub in_table_similarity: f64,
    /// Whether to collapse those repeats at all; the user can switch it off.
    pub reduce_duplicates: bool,
    /// `multi_display_record_strategy`: `all` grabs the monitor union, `single` grabs one panel.
    pub display_strategy: String,
    /// `record_single_display_index`, 1-based and matching `mss`'s `monitors[1..]`.
    pub single_display_index: i32,
    /// Seconds of divergence between the tick counter and the wall clock that means the machine
    /// slept. `utils.is_system_awake()` uses the same 30 s line.
    pub sleep_drift_limit_seconds: f64,
    /// Minutes of pause after which the idle maintenance pass is launched: slices become video,
    /// expired video is pruned, the index is corrected. Doing that during an active session is what
    /// made the Python app stutter, so it only ever runs while the screen is idle.
    pub maintain_after_idle_minutes: i64,
    /// Frames below this many characters are noise (a desktop with one icon label).
    pub minimum_text_chars: usize,
    /// `record_deep_linking`. The Python indexer stores a browser's address-bar text per row so a
    /// result can be re-opened at its source. Reading it needs UIAutomation, and the native side has
    /// no UIAutomation yet — so this is carried, reported, and honestly empty rather than silently
    /// absent. See `Recorder::warn_unported`.
    pub record_deep_linking: bool,
    /// Window-title substrings that mean "do not record this".
    pub exclude_words: Vec<String>,
    /// Which `userdata/db/{user}_…_wind.db` the rows belong in. Read at open time; changing it
    /// needs a restart because it selects a file, not a behaviour.
    pub user_name: String,
    /// The hours in which the deferred pass is allowed to run, if this install named them.
    ///
    /// Carried on the plan rather than read off the config at the moment of launch because the plan is
    /// what this recorder re-reads every tick: setting a window in the settings page takes effect on
    /// the next tick, and deleting it takes the automatic pass back to the idle rule just as cleanly.
    pub maintain_window: Option<wind_base::config::MaintainWindow>,
    /// Redraw the stored previews inside the window instead of while the pixels are being caught.
    ///
    /// Set exactly when a window is: a row whose thumbnail is empty is a row the `previews` step
    /// refills from the same JPEG the row already points at, so nothing is lost by waiting — unlike
    /// text, which cannot be waited on without a masked copy of the frame being written at capture
    /// time (see the note in `Recorder::tick`). This is the one deferred-derived-work switch that is
    /// safe to make unattended, and it removes a full 1920-wide JPEG encode from every kept frame.
    pub defer_previews: bool,
    /// Read the pixels' text inside the window instead of while the screen is being caught.
    ///
    /// The engine call is the single most expensive thing the capture tick does (0.22-1.2 s per kept
    /// frame), and it is recomputable from the masked copy the tick writes instead — but only because
    /// that copy is written *here*, where the mask, the geometry and the pixels are all live. A row
    /// whose text never arrives is permanently unsearchable, so this is switched on by exactly the
    /// thing that guarantees a window will come back for it: a named `maintain_window`.
    pub defer_text: bool,
    /// Capture and gate only: no OCR, no rows. A diagnostic mode, so the capture cost can be
    /// measured against a machine that has no OCR engine installed.
    pub gate_only: bool,
}

impl Default for Plan {
    fn default() -> Plan {
        Plan {
            record_seconds: 900,
            interval_seconds: 3,
            interrupt_limit: 40,
            pause_after_idle_minutes: 5.0,
            repeat_text_similarity: 70.0,
            in_table_similarity: 94.0,
            reduce_duplicates: true,
            sleep_drift_limit_seconds: 30.0,
            display_strategy: "all".into(),
            single_display_index: 1,
            maintain_after_idle_minutes: 40,
            minimum_text_chars: 5,
            exclude_words: Vec::new(),
            record_deep_linking: false,
            user_name: "default".into(),
            maintain_window: None,
            defer_previews: false,
            defer_text: false,
            gate_only: false,
        }
    }
}

/// The marker directory a slice gains once its rows are in the index.
///
/// One constant for the writer (`Segment::submit_marker`) and the reader (`status`), because the two
/// disagreeing by one character is how a committed segment becomes invisible to conversion.
pub const SUBMIT_MARKER: &str = paths::SUBMIT_MARKER_DIR;

/// The write-ahead journal: one line per accepted frame, inside the segment's own directory.
///
/// The leading dash is the same trick [`SUBMIT_MARKER`] uses — no `{stamp}.jpg` frame name can be
/// mistaken for it, and `windmaint`'s `read_frames` rejects it twice over (the stem is not a
/// 19-character stamp, the extension is not an image), so a journal can never be encoded into a
/// user's video.
///
/// It exists because rows used to live only in memory until `close_segment` committed them: a hard
/// kill mid-segment left a directory of JPEGs, no `-SUBMIT` marker and no rows, and since those two
/// facts are the same event nothing downstream could ever tell them apart from a segment still being
/// written. The journal is a copy of what the close path already holds, so it changes no decision —
/// it only makes the decision survivable.
pub const JOURNAL_FILE: &str = "-JOURNAL.jsonl";
const JOURNAL_PREFIX: &str = "-JOURNAL";
const JOURNAL_SUFFIX: &str = ".jsonl";
const HEADER_TAG: &str = "h1";
const RECORD_TAG: &str = "r1";
/// A `None` field, written raw.
///
/// Every value is escaped before it is written, and escaping turns `\` into `\\`, so a frame whose
/// OCR text really is the two characters `\N` reaches disk as `\\N` and cannot be confused with this.
const NULL_FIELD: &str = r"\N";
/// Fields per line, tag included. A line that does not have exactly this many is not ours.
const HEADER_FIELDS: usize = 5;
const RECORD_FIELDS: usize = 8;

/// Why a frame was not indexed. The distinction matters only for the statistics line, but the
/// statistics line is how a user finds out the recorder is silently skipping everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// Nothing recognisable was on screen, and nothing else — not even a window title — can stand
    /// in for it. See [`OcrOutcome`].
    Empty,
    /// The same screen as the previous kept frame.
    SameAsPrevious,
    /// A window the user asked never to record.
    Excluded,
    /// Too little time has passed since the last kept frame.
    TooEarly,
}

/// Whether the text on a candidate row is knowledge, or only an absence.
///
/// This is the content policy for a run whose OCR engine cannot be used, and the two arms are not
/// interchangeable:
///
///   * An engine that *ran* and read almost nothing has told us something — the screen is blank —
///     and `minimum_text_chars` exists so a wallpaper is not indexed two hundred times a day. That
///     verdict is kept exactly as it was.
///   * An engine that could not run has told us *nothing*. The frame still carries a window title,
///     which is captured on a side channel that has no OCR in it at all, and the index already
///     searches it: `wind_store::search` ORs every token across `ocr_text` **and** `win_title`, and
///     `Record::indexed_text` appends the title to the body with `" -||- "`. Dropping the frame
///     over a quarantined .exe is how a user loses a whole day and finds out a week later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrOutcome {
    /// The engine ran. `ocr_text` is what it read, and an empty `ocr_text` is a result, not a gap.
    Read,
    /// The engine did not run — missing, unspawnable, or exited non-zero — or the run asked for no
    /// OCR. `ocr_text` is empty because it could not be filled in.
    Unavailable,
}

impl OcrOutcome {
    /// What a row produced under this outcome can actually be searched by.
    ///
    /// Deliberately *not* a value written to the row: `ocr_text` stays what the engine read (which
    /// for `Unavailable` is nothing), so no text is ever invented. This is the key the repeat test
    /// compares and the floor the emptiness test measures, and nothing else.
    pub fn searchable<'a>(self, candidate: &'a Record) -> &'a str {
        match self {
            // An engine that ran and read nothing has *said* something: this screen is blank. The
            // empty string is the answer, and it is the answer `minimum_text_chars` exists to reject.
            OcrOutcome::Read => candidate.ocr_text.as_str(),
            OcrOutcome::Unavailable => searchable_content(candidate),
        }
    }
}

/// What a row already written can be searched by, once the run that wrote it is over.
///
/// [`Segment::offer`] is handed the [`OcrOutcome`] directly; the close-time collapse and the startup
/// replay only have the row. They agree because `offer` never admits a row with an empty `ocr_text`
/// and nothing else on it, so an empty text in `records` can only mean "the engine did not run for
/// this frame" — and the title is then the whole of what that row can ever match a search on.
pub fn searchable_content(record: &Record) -> &str {
    if !record.ocr_text.is_empty() {
        return record.ocr_text.as_str();
    }
    record
        .win_title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or("")
}


/// One recording: a timestamped directory of frames that becomes exactly one video file.
#[derive(Debug, Clone)]
pub struct Segment {
    /// `%Y-%m-%d_%H-%M-%S` of the moment the segment opened.
    pub stamp: String,
    /// The name the index rows carry, and the name the maintenance pass must produce on disk.
    pub videofile_name: String,
    /// Directory holding this segment's frames, under `cache_screenshot/`.
    pub dir_name: String,
    pub opened_at: i64,
    pub records: Vec<Record>,
    /// Wall-clock seconds of screen covered, counted from kept frames, as upstream does.
    pub covered_seconds: i64,
    pub interruptions: u32,
    /// The searchable content of the last kept frame — its OCR text when there was any, its window
    /// title when the engine could not be used. See [`OcrOutcome::searchable`].
    previous_content: String,
    last_kept_at: i64,
}

impl Plan {
    /// Gate-only mode indexes nothing, so a tick stops after paying for the grab. Used by
    /// `windrec run --gate-only` to price the capture path without the OCR engine.
    pub fn record_only_gate(&self) -> bool {
        self.gate_only
    }
}

impl Segment {
    /// Open a segment at `now`. The name is fixed here for the segment's whole life, so a row
    /// written in its first second and a frame from its last second agree on the filename.
    pub fn opening(now: &LocalParts) -> Segment {
        let stamp = now.stamp();
        Segment {
            videofile_name: format!("{stamp}.mp4"),
            dir_name: stamp.clone(),
            stamp,
            opened_at: now.naive_epoch_seconds(),
            records: Vec::new(),
            covered_seconds: 0,
            interruptions: 0,
            previous_content: String::new(),
            last_kept_at: i64::MIN,
        }
    }

    /// Is this segment old enough to close?
    pub fn is_full(&self, now_seconds: i64, plan: &Plan) -> bool {
        now_seconds - self.opened_at >= plan.record_seconds
    }

    /// Has the segment run into the interruption ceiling and should be abandoned?
    pub fn is_broken(&self, plan: &Plan) -> bool {
        self.interruptions >= plan.interrupt_limit
    }

    /// The single gate every candidate frame passes through.
    ///
    /// `ocr` says whether the text on the candidate is knowledge or an absence, and it is the only
    /// thing that distinguishes "the screen was blank" from "the engine could not be asked" — the
    /// difference between dropping a frame and losing a user's whole day to a quarantined .exe.
    ///
    /// Order is load-bearing: exclusion is checked before content comparison, because an excluded
    /// window must not become `previous_content` — if it did, the frame after it would look like a
    /// repeat of content that was never recorded, and the recorder would silently drop a screen the
    /// user needed.
    pub fn offer(
        &mut self,
        now_seconds: i64,
        candidate: &Record,
        plan: &Plan,
        ocr: OcrOutcome,
    ) -> Result<(), Rejection> {
        if self.last_kept_at != i64::MIN && now_seconds - self.last_kept_at < plan.interval_seconds {
            // Checked first because it is the only rejection that is a timing accident rather than a
            // judgement about the screen: the frame is fine, it is just early.
            return Err(Rejection::TooEarly);
        }
        if let Some(title) = &candidate.win_title {
            let lowered = title.to_lowercase();
            if plan.exclude_words.iter().any(|w| !w.is_empty() && lowered.contains(w.as_str())) {
                self.interruptions += 1;
                return Err(Rejection::Excluded);
            }
        }
        let content = ocr.searchable(candidate);
        // Both judgements below are about *text*, so neither can be answered before the text exists.
        // Guessing from the window title instead would drop every frame whose screen is full of words
        // under a short title — and the pixel gate upstream has already decided this screen changed.
        if !plan.defer_text {
            if content.chars().count() < plan.minimum_text_chars {
                // A blank screen with a working engine, or an unread one with no title to fall back on.
                // Both are rows a search could never return, so neither is worth a directory entry.
                self.interruptions += 1;
                return Err(Rejection::Empty);
            }
            if !self.previous_content.is_empty()
                && wind_store::maintain::text_similarity(&self.previous_content, content) * 100.0
                    >= plan.repeat_text_similarity
            {
                // A repeat is not an interruption: the screen simply did not change. The unchanged case
                // is handled by the pixel gate before OCR is ever paid for.
                return Err(Rejection::SameAsPrevious);
            }
        }
        // Upstream credits each kept frame with one interval's worth of screen time, so a segment's
        // length is what was captured rather than what the wall clock says: a paused recorder must
        // not rotate into an empty video.
        self.covered_seconds += plan.interval_seconds.max(0);
        self.last_kept_at = now_seconds;
        self.interruptions = 0;
        self.previous_content = content.to_string();
        self.records.push(candidate.clone());
        Ok(())
    }

    /// Fold away frames that captured the same screen at different moments.
    ///
    /// This runs at close time, over the whole segment, and it is the counterpart of the
    /// `index_reduce_same_content_at_different_time` option: a page the user stared at for nine
    /// minutes should be searchable once, not one hundred and eighty times. Returns how many rows
    /// went away.
    ///
    /// It compares [`searchable_content`], not `ocr_text` alone, and that is what keeps a run whose
    /// engine was unavailable from becoming an index of one row per window switch repeated forever:
    /// `offer` only ever looks at the *previous* kept row, so six windows cycled through an afternoon
    /// pass that test sixty times. Folding on the title for the rows that have nothing but a title
    /// is the same dedup the text rows get, applied to the only content those rows have.
    pub fn collapse_repeats(&mut self, plan: &Plan) -> usize {
        if !plan.reduce_duplicates || self.records.len() < 2 {
            return 0;
        }
        let bodies: Vec<&str> = self.records.iter().map(|r| searchable_content(r)).collect();
        let dropped = duplicate_flags(&bodies, plan.in_table_similarity / 100.0);
        let before = self.records.len();
        let mut keep = Vec::with_capacity(before);
        for (at, record) in std::mem::take(&mut self.records).into_iter().enumerate() {
            if !dropped.contains(&at) {
                keep.push(record);
            }
        }
        let after = keep.len();
        self.records = keep;
        before - after
    }

    /// Can a segment this thin be committed? Upstream discards anything under five rows, because a
    /// one-frame "video" produces a file the player cannot open and a row that points at nothing.
    pub fn is_committable(&self) -> bool {
        self.records.len() >= 5
    }



    /// The marker directory created *inside* the slice once its rows are in the index.
    ///
    /// A nested marker, not a rename of the slice itself: the maintenance pass discovers work by
    /// skipping any directory whose name already carries a pipeline marker, so renaming the slice to
    /// `{stamp}-SUBMIT` here hid it from conversion permanently. Upstream writes the same marker for
    /// the same reason (`record.py`), and the name keeps its leading dash so it cannot collide with a
    /// frame filename.
    pub fn submit_marker(&self) -> String {
        SUBMIT_MARKER.to_string()
    }

    pub fn discarded_dir(&self) -> String {
        format!("{}{}", self.dir_name, paths::MARKER_DISCARD)
    }

    /// The directory this segment's frames and journal live in, under the cache root.
    pub fn directory(&self, cache_root: &Path) -> PathBuf {
        cache_root.join(&self.dir_name)
    }

    /// Who is writing this segment's journal, and where its rows belong.
    ///
    /// The pid is the claim rule: a journal is only replayable by an instance that can prove its
    /// writer is gone, and `fslock`'s dead-pid test is the one liveness mechanism this app has.
    pub fn journal_owner(&self) -> JournalOwner {
        JournalOwner {
            owner_pid: std::process::id(),
            dir_name: self.dir_name.clone(),
            videofile_name: self.videofile_name.clone(),
            opened_at: self.opened_at,
        }
    }

    /// The segment a stranded journal names, rebuilt from its directory name alone.
    ///
    /// Going back through [`Segment::opening`] is deliberate: it means `collapse_repeats`,
    /// `is_committable` and `submit_marker` in the recovery path *are* the close path's methods, so
    /// a replay cannot quietly grow a second, laxer commit policy.
    pub fn stranded(dir_name: &str) -> Option<Segment> {
        LocalParts::from_stamp(dir_name).map(|parts| Segment::opening(&parts))
    }
}

/// The first line of a journal: which process wrote it, and which segment it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalOwner {
    /// The writing process. Checked with `fslock::is_process_running` before anything is replayed.
    pub owner_pid: u32,
    /// The segment's directory, which is also its stamp.
    pub dir_name: String,
    /// What every row in the journal names as its video.
    pub videofile_name: String,
    pub opened_at: i64,
}

impl JournalOwner {
    fn encode_line(&self) -> String {
        format!(
            "{HEADER_TAG}\t{}\t{}\t{}\t{}\n",
            self.owner_pid,
            escape(&self.dir_name),
            escape(&self.videofile_name),
            self.opened_at
        )
    }

    fn decode(fields: &[&str]) -> Option<JournalOwner> {
        Some(JournalOwner {
            owner_pid: fields[0].parse().ok()?,
            dir_name: unescape(fields[1])?,
            videofile_name: unescape(fields[2])?,
            opened_at: fields[3].parse().ok()?,
        })
    }
}

/// A journal read back off disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Journal {
    /// `None` when the first line was not ours to read, which means nothing after it can be trusted
    /// either — the header and the first row are written in one `write_all`.
    pub owner: Option<JournalOwner>,
    pub records: Vec<Record>,
    /// Rows dropped because the line was cut short. A hard kill can only truncate the write it was
    /// inside, so this is at most one, and the row it names has no frame on disk either.
    pub torn_tail: usize,
}

impl Journal {
    /// Read a journal, tolerating a line torn by the kill we are recovering from.
    ///
    /// Returns `Err` only when the file cannot be read at all; a file that parses to nothing is an
    /// empty journal, not a failure, because "no rows recoverable" is an answer the sweep has to be
    /// able to report rather than die on.
    pub fn read(path: &Path) -> Result<Journal, String> {
        let body = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Journal::parse(&body))
    }

    pub fn parse(body: &str) -> Journal {
        let mut owner = None;
        let mut records = Vec::new();
        let mut torn_tail = 0;
        for (index, line) in body.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            let parsed = match (index == 0, fields.first().copied().unwrap_or_default()) {
                (true, HEADER_TAG) if fields.len() == HEADER_FIELDS => {
                    JournalOwner::decode(&fields[1..]).map(|o| owner = Some(o))
                }
                (_, RECORD_TAG) if fields.len() == RECORD_FIELDS => {
                    decode_record(&fields[1..]).map(|record| records.push(record))
                }
                _ => None,
            };
            if parsed.is_none() {
                // Stop at the first line that is not whole. Appends are ordered, so a damaged line
                // in the middle means everything after it is already suspect; and the line a kill
                // interrupted is always the last one.
                torn_tail = 1;
                break;
            }
        }
        Journal { owner, records, torn_tail }
    }

    /// Append one accepted frame.
    ///
    /// The header rides along with the first row in the same write, so a journal that exists at all
    /// either names its owner or holds nothing to replay — the sweep never has to guess about a
    /// headerless file. `File` carries no userspace buffer, so this is one `WriteFile` per frame
    /// beside the JPEG write that already happens, and it is on disk before the frame counts as
    /// journaled.
    pub fn append(slice_dir: &Path, owner: &JournalOwner, record: &Record) -> Result<(), String> {
        std::fs::create_dir_all(slice_dir).map_err(|e| format!("{}: {e}", slice_dir.display()))?;
        let path = slice_dir.join(JOURNAL_FILE);
        let mut line = String::new();
        if !path.exists() {
            line.push_str(&owner.encode_line());
        }
        line.push_str(&encode_record(record));
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        file.write_all(line.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// Does this directory entry name a journal, canonical or already claimed for replay?
pub fn is_journal_name(name: &str) -> bool {
    name.starts_with(JOURNAL_PREFIX) && name.ends_with(JOURNAL_SUFFIX)
}

/// The name a journal takes once `pid` has claimed it for replay.
pub fn claimed_journal_name(pid: u32) -> String {
    format!("{JOURNAL_PREFIX}-{pid}{JOURNAL_SUFFIX}")
}

/// Every journal file in a segment directory, canonical name first.
pub fn journal_paths(slice_dir: &Path) -> Vec<PathBuf> {
    let mut found = match std::fs::read_dir(slice_dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let name = entry.file_name().into_string().ok()?;
                is_journal_name(&name).then(|| (name, path))
            })
            .collect::<Vec<_>>(),
        Err(_) => return Vec::new(),
    };
    // The canonical name is the one a live writer appends to, so it is always the first candidate;
    // a claimed name only exists while a replay is in flight or after the replaying process died.
    found.sort_by(|a, b| (a.0 != JOURNAL_FILE, &a.0).cmp(&(b.0 != JOURNAL_FILE, &b.0)));
    found.into_iter().map(|(_, path)| path).collect()
}

/// The pid that claimed this journal, or `None` for the canonical, unclaimed name.
pub fn journal_claimant(name: &str) -> Option<u32> {
    let middle = name.strip_prefix(JOURNAL_PREFIX)?.strip_suffix(JOURNAL_SUFFIX)?;
    middle.strip_prefix('-')?.parse().ok()
}

/// Take exclusive ownership of one specific journal file, without deleting anything.
///
/// `from` is the file the caller actually read and judged, not "whatever is in there": renaming a
/// path that a rival already moved fails with `NotFound`, which is what makes two instances sweeping
/// the same cache commit the same stranded segment at most once between them. Adopting a claim left
/// by a dead replay races the same way and loses the same way.
pub fn claim_journal(slice_dir: &Path, from: &Path, pid: u32) -> Result<PathBuf, String> {
    let target = slice_dir.join(claimed_journal_name(pid));
    if from == target {
        return Ok(target);
    }
    std::fs::rename(from, &target)
        .map(|()| target)
        .map_err(|e| format!("{}: {e}", from.display()))
}

/// Hand a claimed journal back to its canonical name, so a decision this instance will not act on
/// (a segment below the floor, a writer that turned out to be alive) is reconsidered by the next one.
///
/// Refuses when the canonical name has reappeared: a writer that resumed after our look would have
/// recreated it, and Windows renames replace, so handing back the claim would quietly delete the
/// journal that writer is now appending to.
pub fn release_journal(slice_dir: &Path, claimed: &Path) -> bool {
    let canonical = slice_dir.join(JOURNAL_FILE);
    if canonical.exists() {
        return false;
    }
    std::fs::rename(claimed, canonical).is_ok()
}

/// Remove every journal in a directory once its segment has reached a terminal state.
///
/// Returns the names it could not remove; a leftover journal is not a failure of the close path —
/// the rows are committed and the marker exists — it is a little extra work for the next sweep,
/// which dedups against the index and so cannot double-commit.
pub fn clear_journals(slice_dir: &Path) -> Vec<String> {
    journal_paths(slice_dir)
        .into_iter()
        .filter_map(|path| {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
            match std::fs::remove_file(&path) {
                Ok(()) => None,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => Some(name),
            }
        })
        .collect()
}

fn encode_record(record: &Record) -> String {
    format!(
        "{RECORD_TAG}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        escape(&record.videofile_name),
        escape(&record.picturefile_name),
        record.videofile_time,
        encode_option(record.win_title.as_deref()),
        encode_option(record.deep_linking.as_deref()),
        encode_option(record.thumbnail.as_deref()),
        escape(&record.ocr_text),
    )
}

fn decode_record(fields: &[&str]) -> Option<Record> {
    Some(Record {
        videofile_name: unescape(fields[0])?,
        picturefile_name: unescape(fields[1])?,
        videofile_time: fields[2].parse().ok()?,
        win_title: decode_option(fields[3]),
        deep_linking: decode_option(fields[4]),
        thumbnail: decode_option(fields[5]),
        ocr_text: unescape(fields[6])?,
    })
}

fn encode_option(value: Option<&str>) -> String {
    match value {
        None => NULL_FIELD.to_string(),
        Some(text) => escape(text),
    }
}

fn decode_option(field: &str) -> Option<String> {
    if field == NULL_FIELD {
        return None;
    }
    unescape(field)
}

/// Escape only what could break the line format. Everything else — CJK, emoji, quotes — is written
/// as itself, so a stranded journal is still readable by a human with a text editor.
fn escape(field: &str) -> String {
    let mut out = String::with_capacity(field.len() + 8);
    for ch in field.chars() {
        match ch {
            '\\' => out.push_str(r"\\"),
            '\t' => out.push_str(r"\t"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            other => out.push(other),
        }
    }
    out
}

fn unescape(field: &str) -> Option<String> {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            // An unknown or truncated escape means the line is not one we wrote, rather than that
            // the user's screen contained a mystery byte.
            _ => return None,
        }
    }
    Some(out)
}

/// Did the machine sleep since the last frame?
///
/// A resume is not just a missed frame: the change gate's anchor may predate the suspend by hours,
/// so the first frame back reads as a huge change and gets indexed no matter what it is, and the
/// anchor for the rest of the segment came from a screen the user was not looking at. Upstream
/// handled this by breaking out of the segment entirely; the native equivalent is to drop the
/// anchor, which `Recorder::tick` does whenever this says yes.
pub fn sleep_intervened(drift_seconds: f64, limit_seconds: f64) -> bool {
    limit_seconds > 0.0 && drift_seconds > limit_seconds
}

/// The pause decision: is the user away, and how do we know?
///
/// An unchanged screen, a locked workstation and a sleeping machine all mean "stop indexing", but
/// they are different signals and the Python loop conflated them into one counter. Idle time
/// accrues in half-minute steps from either the pixel gate or the session state, and the recorder
/// resumes the moment either one says the user is back.
#[derive(Debug, Default, Clone)]
pub struct IdleTracker {
    minutes: f64,
    pub paused: bool,
}

impl IdleTracker {
    pub const STEP_MINUTES: f64 = 0.5;

    pub fn minutes(&self) -> f64 {
        self.minutes
    }

    /// Observe one tick. `unchanged` covers both "the pixels did not move" and "the session is not
    /// recordable", which is deliberately the same treatment: nothing new can be indexed either way.
    pub fn observe(&mut self, unchanged: bool, threshold_minutes: f64) {
        if unchanged {
            self.minutes += Self::STEP_MINUTES;
        } else {
            self.minutes = 0.0;
        }
        self.paused = threshold_minutes > 0.0 && self.minutes > threshold_minutes;
    }

    /// A resume must clear the accumulator, or the next single unchanged tick re-pauses the
    /// recorder immediately after the user comes back.
    pub fn resume(&mut self) {
        self.minutes = 0.0;
        self.paused = false;
    }
}

/// How often a paused capture loop looks at the machine again, in seconds. A pause exists to stop
/// grabbing, recognising and writing, so this is deliberately far slower than
/// `screenshot_interval_second` — and it is also the window inside which input still counts as
/// "somebody came back", which is why the two facts are one constant.
pub const PAUSE_POLL_SECS: u64 = 10;

/// Whether recent input ends a still-screen pause.
///
/// The pause cannot be lifted by the pixel gate, because the loop that owns the gate stopped reading
/// pixels the moment it paused: [`IdleTracker::observe`] sits past the grab, so a pause judged from
/// the screen could only ever end on a screen nobody looks at again. That is not a hypothetical —
/// the recorder used to close its segment on pausing, find nothing left committable on every tick
/// after that, sleep, and never evaluate the condition again, so one 5-minute read ended the day's
/// recording. Input is cheaper than a grab and it is the thing [`IdleTracker`]'s own doc promises:
/// "the recorder resumes the moment either one says the user is back".
///
/// `None` — input idle not measurable — resumes rather than holding the pause. A recorder deaf about
/// the mouse costs itself one pause; the gate re-pauses it within `pause_after_idle_minutes` if the
/// screen really is still. The other direction costs every hour spent in front of a stable picture.
pub fn resumed_from_input(idle_seconds: Option<f64>) -> bool {
    match idle_seconds {
        Some(seconds) => seconds < PAUSE_POLL_SECS as f64,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(stamp: &str) -> LocalParts {
        LocalParts::from_stamp(stamp).unwrap()
    }

    /// A candidate frame. `label` is a test-local tag for the filename; timing is always supplied
    /// explicitly through `offer`, which is where the segment's clock actually lives.
    fn rec(label: &str, text: &str) -> Record {
        Record {
            videofile_name: "unused".into(),
            picturefile_name: format!("{label}.jpg"),
            videofile_time: 0,
            ocr_text: text.into(),
            win_title: Some("Editor".into()),
            deep_linking: None,
            thumbnail: Some("AAA".into()),
        }
    }

    #[test]
    fn a_segment_borrows_its_name_from_its_opening_instant() {
        let s = Segment::opening(&at("2026-09-21_10-00-00"));
        assert_eq!(s.videofile_name, "2026-09-21_10-00-00.mp4");
        assert_eq!(s.dir_name, "2026-09-21_10-00-00");
        // The 19-character prefix rule the whole ecosystem reads names by.
        assert_eq!(&s.videofile_name[..paths::STAMP_LEN], "2026-09-21_10-00-00");
    }

    #[test]
    fn rotation_happens_at_the_configured_length_and_not_before() {
        let plan = Plan { record_seconds: 900, ..Default::default() };
        let s = Segment::opening(&at("2026-09-21_10-00-00"));
        assert!(!s.is_full(at("2026-09-21_10-14-59").naive_epoch_seconds(), &plan));
        assert!(s.is_full(at("2026-09-21_10-15-00").naive_epoch_seconds(), &plan));
    }

    #[test]
    fn a_short_screen_is_not_indexed() {
        let plan = Plan::default();
        let mut s = Segment::opening(&at("2026-09-21_10-00-00"));
        let err = s.offer(
            at("2026-09-21_10-00-00").naive_epoch_seconds(),
            &rec("2026-09-21_10-00-00", "hi"),
            &plan,
            OcrOutcome::Read,
        );
        assert_eq!(err, Err(Rejection::Empty));
        assert_eq!(s.interruptions, 1);
    }

    /// The defect this whole path exists for, stated as a test: an OCR engine that cannot run used to
    /// cost the user the frame, which — multiplied across a day — cost them the day. The frame is
    /// worth keeping because `win_title` is captured on a channel that has no OCR in it, and the
    /// index already searches that column (`wind_store::search` ORs every token across `ocr_text`
    /// *and* `win_title`).
    #[test]
    fn an_unreadable_frame_is_still_indexed_on_the_strength_of_its_title() {
        let plan = Plan::default();
        let mut s = Segment::opening(&at("2026-09-21_10-00-00"));
        let base = at("2026-09-21_10-00-00").naive_epoch_seconds();
        let unread = rec("2026-09-21_10-00-00", "");
        assert!(s.offer(base, &unread, &plan, OcrOutcome::Unavailable).is_ok());
        assert_eq!(s.records.len(), 1);
        assert_eq!(s.interruptions, 0, "a kept frame ends the interruption streak like any other");
        // Nothing is invented: the field the user reads and searches stays honestly empty.
        assert_eq!(s.records[0].ocr_text, "");
        assert_eq!(s.records[0].win_title.as_deref(), Some("Editor"));
    }

    /// The guard the fix must not defeat. An engine that *ran* and read nothing has reported a blank
    /// screen, and a blank screen is exactly what `minimum_text_chars` exists to keep out of the
    /// index hundreds of times a day.
    #[test]
    fn a_blank_screen_with_a_working_engine_is_still_rejected() {
        let plan = Plan::default();
        let mut s = Segment::opening(&at("2026-09-21_10-00-00"));
        let base = at("2026-09-21_10-00-00").naive_epoch_seconds();
        assert_eq!(
            s.offer(base, &rec("t", ""), &plan, OcrOutcome::Read),
            Err(Rejection::Empty),
            "the empty result is an answer, not a gap"
        );
        assert_eq!(s.interruptions, 1);
        assert!(s.records.is_empty());
    }

    /// The other half of the same guard: a titled row survives an unread screen, an *untitled* one
    /// has nothing left to be searched by and so is still refused.
    #[test]
    fn an_unreadable_frame_with_no_title_either_has_nothing_to_index() {
        let plan = Plan::default();
        let mut s = Segment::opening(&at("2026-09-21_10-00-00"));
        let base = at("2026-09-21_10-00-00").naive_epoch_seconds();
        let blank = Record { win_title: None, ..rec("t", "") };
        assert_eq!(s.offer(base, &blank, &plan, OcrOutcome::Unavailable), Err(Rejection::Empty));
        let spaces = Record { win_title: Some("   ".into()), ..rec("t", "") };
        assert_eq!(s.offer(base + 30, &spaces, &plan, OcrOutcome::Unavailable), Err(Rejection::Empty));
        assert!(s.records.is_empty());
    }

    /// The throttle that keeps "OCR is broken" from becoming "one row every interval". With no text
    /// to compare, the window title is the content, and content that has not changed is a repeat —
    /// so a broken engine indexes *window changes*, not seconds.
    #[test]
    fn an_unchanged_window_is_a_repeat_even_when_no_text_can_prove_it() {
        let plan = Plan::default();
        let mut s = Segment::opening(&at("2026-09-21_10-00-00"));
        let base = at("2026-09-21_10-00-00").naive_epoch_seconds();
        let same_title = || Record { win_title: Some("qxkzj".into()), ..rec("t", "") };
        assert!(s.offer(base, &same_title(), &plan, OcrOutcome::Unavailable).is_ok());
        for step in 1..=20 {
            assert_eq!(
                s.offer(base + step * 30, &same_title(), &plan, OcrOutcome::Unavailable),
                Err(Rejection::SameAsPrevious),
                "the pixels may have moved but nothing the row could be found by did"
            );
        }
        assert_eq!(s.records.len(), 1, "one row for twenty ticks of the same unread window");
        assert_eq!(s.interruptions, 0, "a steady screen is not an error");
    }

    /// `offer` only ever compares against the last kept frame, so six windows cycled through an
    /// afternoon pass it every time. The close-time fold is what catches that, and for these rows it
    /// has to fold on the title or it folds on nothing at all.
    #[test]
    fn a_cycled_set_of_title_only_rows_folds_at_close_time() {
        let plan = Plan::default();
        let mut s = Segment::opening(&at("2026-09-21_10-00-00"));
        let base = at("2026-09-21_10-00-00").naive_epoch_seconds();
        // Disjoint character sets, so only an actual repeat can read as one.
        let titles = ["qxkzj", "wpvtm", "syrbn"];
        for round in 0..4 {
            for (index, title) in titles.iter().enumerate() {
                let row = Record { win_title: Some((*title).into()), ..rec("t", "") };
                assert!(
                    s.offer(base + (round * 3 + index as i64) * 30, &row, &plan, OcrOutcome::Unavailable)
                        .is_ok(),
                    "round {round} title {title}"
                );
            }
        }
        assert_eq!(s.records.len(), 12, "each differs from the window before it");
        assert_eq!(s.collapse_repeats(&plan), 9, "and the segment still holds three screens");
        assert_eq!(
            s.records.iter().map(|r| r.win_title.clone().unwrap_or_default()).collect::<Vec<_>>(),
            vec!["qxkzj", "wpvtm", "syrbn"]
        );
    }

    /// A text-bearing row must fold on its text exactly as it always did. This is the no-regression
    /// half of the close-time change above: the title only stands in where there is no text.
    #[test]
    fn a_row_with_text_still_folds_on_its_text_and_ignores_its_title() {
        let plan = Plan::default();
        let mut s = Segment::opening(&at("2026-09-21_10-00-00"));
        for (index, text) in ["quarterly revenue report", "quarterly revenue report"].iter().enumerate() {
            s.records.push(Record {
                win_title: Some(format!("qxkzj wpvtm syrbn {index}")),
                ..rec("2026-09-21_10-00-00", text)
            });
        }
        assert_eq!(s.collapse_repeats(&plan), 1, "the identical text folds, titles notwithstanding");
    }

    #[test]
    fn the_searchable_key_reads_the_text_when_there_is_any_and_the_title_only_otherwise() {
        let text = Record { win_title: Some("qxkzj".into()), ..rec("t", "quarterly revenue report") };
        assert_eq!(OcrOutcome::Read.searchable(&text), "quarterly revenue report");
        assert_eq!(OcrOutcome::Unavailable.searchable(&text), "quarterly revenue report");
        let unread = Record { win_title: Some("  qxkzj  ".into()), ..rec("t", "") };
        assert_eq!(OcrOutcome::Read.searchable(&unread), "", "an engine that ran and read nothing said so");
        assert_eq!(OcrOutcome::Unavailable.searchable(&unread), "qxkzj");
        assert_eq!(searchable_content(&unread), "qxkzj", "and the close-time fold agrees with the offer");
        assert_eq!(searchable_content(&Record { win_title: None, ..rec("t", "") }), "");
    }

    #[test]
    fn a_repeat_of_the_last_kept_screen_is_dropped_without_counting_against_the_segment() {
        let plan = Plan::default();
        let mut s = Segment::opening(&at("2026-09-21_10-00-00"));
        let base = at("2026-09-21_10-00-00").naive_epoch_seconds();
        s.offer(base, &rec("2026-09-21_10-00-00", "revenue report 2026"), &plan, OcrOutcome::Read).unwrap();
        let again = s.offer(
            base + 30,
            &rec("2026-09-21_10-00-30", "revenue report 2026"),
            &plan,
            OcrOutcome::Read,
        );
        assert_eq!(again, Err(Rejection::SameAsPrevious));
        assert_eq!(s.interruptions, 0, "a steady screen is not an error");
        assert_eq!(s.records.len(), 1);
    }

    /// The ordering rule that keeps a private window from poisoning the comparison chain.
    #[test]
    fn an_excluded_window_never_becomes_the_baseline_for_the_next_frame() {
        let plan = Plan { exclude_words: vec!["keepass".into()], ..Default::default() };
        let mut s = Segment::opening(&at("2026-09-21_10-00-00"));
        let base = at("2026-09-21_10-00-00").naive_epoch_seconds();
        s.offer(base, &rec("keep", "public document about budgets"), &plan, OcrOutcome::Read).unwrap();

        let secret = Record { win_title: Some("KeePass Password Safe".into()), ..rec("vault", "xylophone quagmism frozen") };
        assert_eq!(s.offer(base + 10, &secret, &plan, OcrOutcome::Read), Err(Rejection::Excluded));

        // The excluded frame's own text, offered again from an ordinary window. If exclusion had
        // written `previous_content`, this would be thrown away as a repeat of something never recorded.
        let again = Record { win_title: Some("Editor".into()), ..rec("plain", "xylophone quagmism frozen") };
        assert!(s.offer(base + 20, &again, &plan, OcrOutcome::Read).is_ok());
    }

    #[test]
    fn a_frame_arriving_early_is_parked_until_the_interval_elapses() {
        let plan = Plan { interval_seconds: 3, ..Default::default() };
        let mut s = Segment::opening(&at("2026-09-21_10-00-00"));
        let base = at("2026-09-21_10-00-00").naive_epoch_seconds();
        s.offer(base, &rec("t0", "quarterly revenue report"), &plan, OcrOutcome::Read).unwrap();
        assert_eq!(
            s.offer(base, &rec("t0b", "shopping cart with socks"), &plan, OcrOutcome::Read),
            Err(Rejection::TooEarly),
            "timing is judged before content, so an early frame is not also a content decision"
        );
        assert!(s.offer(base + 3, &rec("t1", "shopping cart with socks"), &plan, OcrOutcome::Read).is_ok());
    }

    #[test]
    fn interruptions_accumulate_until_the_segment_is_abandoned() {
        let plan = Plan { interrupt_limit: 3, minimum_text_chars: 5, ..Default::default() };
        let mut s = Segment::opening(&at("2026-09-21_10-00-00"));
        assert!(!s.is_broken(&plan));
        for i in 0..3 {
            let base = at("2026-09-21_10-00-00").naive_epoch_seconds() + i * 10;
            let _ = s.offer(base, &rec("t", "tiny"), &plan, OcrOutcome::Read);
        }
        assert_eq!(s.interruptions, 3);
        assert!(s.is_broken(&plan));
    }

    #[test]
    fn a_successful_frame_clears_the_interruption_streak() {
        let plan = Plan { interrupt_limit: 40, ..Default::default() };
        let mut s = Segment::opening(&at("2026-09-21_10-00-00"));
        let base = at("2026-09-21_10-00-00").naive_epoch_seconds();
        let _ = s.offer(base, &rec("t", "tiny"), &plan, OcrOutcome::Read);
        assert_eq!(s.interruptions, 1);
        s.offer(base + 5, &rec("t2", "a real screen with content"), &plan, OcrOutcome::Read).unwrap();
        assert_eq!(s.interruptions, 0);
    }

    #[test]
    fn a_thin_segment_is_discarded_rather_than_committed() {
        let plan = Plan::default();
        let mut s = Segment::opening(&at("2026-09-21_10-00-00"));
        assert!(!s.is_committable(), "nothing captured yet");
        let base = at("2026-09-21_10-00-00").naive_epoch_seconds();
        // Character-set similarity only lets through frames that share few letters.
        let screens = [
            "quarterly revenue report",
            "shopping cart with socks",
            "video call with the team",
            "database query running slow",
            "pdf manual page twelve",
        ];
        for (i, text) in screens.iter().enumerate().take(4) {
            assert!(s.offer(base + i as i64 * 10, &rec("f", text), &plan, OcrOutcome::Read).is_ok());
        }
        assert!(!s.is_committable(), "four rows is still noise");
        assert!(s.offer(base + 40, &rec("f", screens[4]), &plan, OcrOutcome::Read).is_ok());
        assert!(s.is_committable());
        assert_eq!(s.discarded_dir(), "2026-09-21_10-00-00-DISCARD");
    }

    /// The collapse must agree with the indexer's own rule: the earliest frame of a repeated screen
    /// survives, later ones go, and a genuinely different screen in between is unaffected.
    #[test]
    fn repeats_inside_a_segment_collapse_to_their_first_occurrence() {
        let plan = Plan::default();
        let mut s = Segment::opening(&at("2026-09-21_10-00-00"));
        // Pushed straight into `records`: `offer` applies the previous-frame threshold, and what is
        // under test here is the segment-wide collapse that runs at close time.
        let screens = [
            "quarterly revenue report 2026",
            "a completely different page about socks",
            "quarterly revenue report 2026",
            "quarterly revenue report 2026",
            "yet another screen entirely, with tea",
        ];
        for text in screens {
            s.records.push(rec("2026-09-21_10-00-00", text));
        }
        assert_eq!(s.records.len(), 5);
        let dropped = s.collapse_repeats(&plan);
        assert_eq!(dropped, 2, "the two later copies of the revenue screen go");
        assert_eq!(
            s.records.iter().map(|r| r.ocr_text.as_str()).collect::<Vec<_>>(),
            vec![
                "quarterly revenue report 2026",
                "a completely different page about socks",
                "yet another screen entirely, with tea",
            ]
        );
    }

    #[test]
    fn collapsing_can_be_switched_off_by_the_user() {
        let plan = Plan { reduce_duplicates: false, ..Default::default() };
        let mut s = Segment::opening(&at("2026-09-21_10-00-00"));
        for text in ["same screen content here", "same screen content here"] {
            s.records.push(rec("2026-09-21_10-00-00", text));
        }
        assert_eq!(s.collapse_repeats(&plan), 0);
        assert_eq!(s.records.len(), 2);
    }

    #[test]
    fn a_threshold_of_one_only_collapses_exact_duplicates() {
        let plan = Plan { in_table_similarity: 100.0, ..Default::default() };
        let mut s = Segment::opening(&at("2026-09-21_10-00-00"));
        // Distinct character sets, so only an exact match can collapse them.
        for text in ["screen alpha beta", "screen gamma delta"] {
            s.records.push(rec("2026-09-21_10-00-00", text));
        }
        assert_eq!(s.collapse_repeats(&plan), 0, "one character apart is a different screen");
        s.records.push(rec("2026-09-21_10-00-00", "screen alpha beta"));
        assert_eq!(s.collapse_repeats(&plan), 1, "an identical screen still collapses");
    }

    #[test]
    fn a_resume_is_detected_from_the_clock_gap() {
        assert!(sleep_intervened(45.0, 30.0));
        assert!(!sleep_intervened(29.9, 30.0));
        assert!(!sleep_intervened(0.0, 30.0));
        // A negative drift is a clock correction, not a sleep; the sign matters because the drift is
        // measured as tick-minus-wall and an NTP step backwards must not pause the recorder.
        assert!(!sleep_intervened(-9999.0, 30.0));
        assert!(!sleep_intervened(9999.0, 0.0), "a zero limit switches the check off");
    }

    #[test]
    fn idle_pauses_after_the_threshold_and_resumes_cleanly() {
        let mut idle = IdleTracker::default();
        for _ in 0..10 {
            idle.observe(true, 5.0);
        }
        assert!(!idle.paused, "5.0 minutes is not yet past the threshold");
        idle.observe(true, 5.0);
        assert!(idle.paused);
        idle.observe(false, 5.0);
        assert!(!idle.paused);
        assert_eq!(idle.minutes(), 0.0);

        // A single tick must not re-pause right after the user comes back.
        idle.observe(true, 5.0);
        assert!(!idle.paused);
    }

    /// The whole reason [`resumed_from_input`] exists: a pause whose only exit was the pixel gate
    /// could never end, because the loop stopped reading pixels while it was paused. The day simply
    /// stopped being recorded, quietly, one still picture at a time.
    #[test]
    fn a_pause_ends_on_input_and_holds_without_it() {
        assert!(resumed_from_input(Some(0.125)), "input a moment ago is a user at the desk");
        assert!(resumed_from_input(Some(PAUSE_POLL_SECS as f64 - 1.0)), "touched since the last look");
        assert!(!resumed_from_input(Some(PAUSE_POLL_SECS as f64 + 20.0)), "untouched, so still paused");
        assert!(resumed_from_input(None), "a recorder deaf about the mouse must not pause forever");
    }

    #[test]
    fn resuming_clears_the_accumulator_so_the_next_still_tick_is_not_a_new_pause() {
        let mut idle = IdleTracker::default();
        for _ in 0..20 {
            idle.observe(true, 5.0);
        }
        assert!(idle.paused, "ten minutes of a still screen passes the five-minute threshold");
        idle.resume();
        assert!(!idle.paused && idle.minutes() == 0.0);
        idle.observe(true, 5.0);
        assert!(!idle.paused, "one unchanged tick after a resume is not a new pause");
    }

    #[test]
    fn pausing_can_be_disabled_by_setting_the_threshold_to_zero() {
        let mut idle = IdleTracker::default();
        for _ in 0..200 {
            idle.observe(true, 0.0);
        }
        assert!(!idle.paused, "screentime_not_change_to_pause_record = 0 means never pause");
    }

    // --- The write-ahead journal. -------------------------------------------------------------

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windcap-journal-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn owner_of(dir_name: &str, pid: u32) -> JournalOwner {
        JournalOwner {
            owner_pid: pid,
            dir_name: dir_name.to_string(),
            videofile_name: format!("{dir_name}.mp4"),
            opened_at: at(dir_name).naive_epoch_seconds(),
        }
    }

    fn journaled(label: &str, text: &str) -> Record {
        Record { videofile_name: "2026-09-21_10-00-00.mp4".into(), ..rec(label, text) }
    }

    /// The journal exists to carry what the JPEGs cannot: `win_title`, and the text that was read
    /// while the frame was still in memory. So the codec has to survive exactly the strings a real
    /// screen produces — tabs from a table, newlines from a terminal, CJK, emoji, and a backslash in
    /// a path — and come back byte-for-byte, or recovery degrades into the lossy thing it replaced.
    #[test]
    fn a_journaled_row_comes_back_exactly_as_it_went_in() {
        let dir = scratch("roundtrip");
        let owner = owner_of("2026-09-21_10-00-00", 4_000_000);
        let rows = vec![
            journaled("2026-09-21_10-00-00", "quarterly revenue report"),
            Record {
                ocr_text: "col1\tcol2\nC:\\Users\\me\\note.txt  季度收入报告 🙂 \"quoted\"".into(),
                win_title: Some(r"Untitled - Notepad\N".into()),
                deep_linking: Some("https://example.com/a?b=c&d=\t".into()),
                thumbnail: Some("AAAA".into()),
                videofile_time: 1_762_310_400,
                ..journaled("2026-09-21_10-00-03", "")
            },
            Record { win_title: None, deep_linking: None, thumbnail: None, ..journaled("2026-09-21_10-00-06", "a third screen with tea") },
        ];
        for row in &rows {
            Journal::append(&dir, &owner, row).unwrap();
        }

        let read = Journal::read(&dir.join(JOURNAL_FILE)).unwrap();
        assert_eq!(read.owner.as_ref().unwrap(), &owner, "the header names its writer and its segment");
        assert_eq!(read.records, rows, "every field round-trips, hostile text included");
        assert_eq!(read.torn_tail, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The header rides in the same write as the first row, so the two states a kill can leave are
    /// "nothing" and "header plus rows" — never a file full of rows belonging to nobody.
    #[test]
    fn the_first_write_is_a_header_and_a_row_together() {
        let dir = scratch("header");
        Journal::append(&dir, &owner_of("2026-09-21_10-00-00", 4_000_000), &journaled("2026-09-21_10-00-00", "one screen")).unwrap();
        let body = std::fs::read_to_string(dir.join(JOURNAL_FILE)).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("h1\t4000000\t2026-09-21_10-00-00\t"), "{}", lines[0]);
        assert!(lines[1].starts_with("r1\t"), "{}", lines[1]);
        assert_eq!(Journal::read(&dir.join(JOURNAL_FILE)).unwrap().records.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `taskkill /F` can land inside a `WriteFile`. Half a line is not a row: it is dropped, the rows
    /// before it are kept, and the report says one row went missing rather than indexing a fragment
    /// with a shifted column set.
    #[test]
    fn a_line_torn_by_a_kill_is_dropped_and_counted_not_guessed_at() {
        let dir = scratch("torn");
        let owner = owner_of("2026-09-21_10-00-00", 4_000_000);
        for (at, text) in [("2026-09-21_10-00-00", "quarterly revenue report"), ("2026-09-21_10-00-03", "shopping cart with socks")] {
            Journal::append(&dir, &owner, &journaled(at, text)).unwrap();
        }
        let whole = std::fs::read_to_string(dir.join(JOURNAL_FILE)).unwrap();
        std::fs::write(dir.join(JOURNAL_FILE), format!("{whole}r1\t2026-09-21_10-00-06.mp4\t2026-09-21_10-00")).unwrap();

        let read = Journal::read(&dir.join(JOURNAL_FILE)).unwrap();
        assert_eq!(read.records.len(), 2, "the two whole rows are still there");
        assert_eq!(read.torn_tail, 1);
        assert_eq!(read.owner.unwrap().owner_pid, 4_000_000, "the header is intact");

        // A damaged line in the middle invalidates everything after it, because appends are ordered
        // and a torn line can only be the last write that ever landed.
        std::fs::write(
            dir.join(JOURNAL_FILE),
            format!("{whole}r1\tbroken\nr1\t2026-09-21_10-00-06.mp4\t2026-09-21_10-00-06.jpg\t7\t-\t\\N\t\\N\tlater row\n"),
        )
        .unwrap();
        let read = Journal::read(&dir.join(JOURNAL_FILE)).unwrap();
        assert!(read.records.iter().all(|r| r.picturefile_name != "2026-09-21_10-00-06.jpg"), "nothing after the break is trusted");
        assert_eq!(read.torn_tail, 1);

        // A file that is only a torn header has nothing recoverable in it.
        std::fs::write(dir.join(JOURNAL_FILE), "h1\t40000").unwrap();
        assert_eq!(Journal::read(&dir.join(JOURNAL_FILE)).unwrap(), Journal { owner: None, records: Vec::new(), torn_tail: 1 });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_journal_file_cannot_be_mistaken_for_a_frame_or_a_marker() {
        assert!(is_journal_name(JOURNAL_FILE));
        assert!(is_journal_name(&claimed_journal_name(4_000_000)));
        for not in ["2026-09-21_10-00-00.jpg", "-SUBMIT", "windmaint_concat.txt", "-JOURNAL.txt", "JOURNAL.jsonl"] {
            assert!(!is_journal_name(not), "{not} is not a journal");
        }
        assert_eq!(journal_claimant(JOURNAL_FILE), None, "the canonical name is unclaimed");
        assert_eq!(journal_claimant(&claimed_journal_name(4_000_000)), Some(4_000_000));
        assert_eq!(journal_claimant("-JOURNAL-.jsonl"), None);
    }

    /// Exclusivity comes from the rename, not from a lock: two instances that both read the same
    /// journal path and both try to claim it leave exactly one winner, because the loser's source is
    /// gone. Who may *pass* a given path to this function is the sweep's judgement, made against
    /// `fslock`'s dead-pid test on the writer and the claimant.
    #[test]
    fn only_one_of_two_claimants_can_win_the_same_journal() {
        let dir = scratch("claim");
        Journal::append(&dir, &owner_of("2026-09-21_10-00-00", 4_000_000), &journaled("2026-09-21_10-00-00", "one screen")).unwrap();
        let canonical = dir.join(JOURNAL_FILE);
        assert_eq!(journal_paths(&dir), vec![canonical.clone()]);

        let claimed = claim_journal(&dir, &canonical, 1234).unwrap();
        assert_eq!(claimed, dir.join(claimed_journal_name(1234)));
        assert!(!canonical.exists(), "the writer's name is gone, so it cannot be claimed twice");
        assert_eq!(claim_journal(&dir, &claimed, 1234).unwrap(), claimed, "our own claim is not a rival");
        assert!(claim_journal(&dir, &canonical, 9999).is_err(), "a rival that read the same name finds it gone");
        assert!(journal_paths(&dir).contains(&claimed), "the winner's claim is what is left on disk");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handing_a_claim_back_never_eats_a_journal_that_reappeared() {
        let dir = scratch("release");
        let owner = owner_of("2026-09-21_10-00-00", 4_000_000);
        Journal::append(&dir, &owner, &journaled("2026-09-21_10-00-00", "one screen")).unwrap();
        let claimed = claim_journal(&dir, &dir.join(JOURNAL_FILE), 1234).unwrap();

        // The writer came back to life mid-replay and recreated its journal: ours goes back to a
        // name that would overwrite it, so it stays where it is.
        Journal::append(&dir, &owner, &journaled("2026-09-21_10-00-03", "a second screen")).unwrap();
        assert!(!release_journal(&dir, &claimed), "a rename that would clobber a live journal is refused");
        assert!(dir.join(JOURNAL_FILE).exists());
        assert!(claimed.exists());

        let _ = std::fs::remove_file(dir.join(JOURNAL_FILE));
        assert!(release_journal(&dir, &claimed));
        assert!(dir.join(JOURNAL_FILE).exists() && !claimed.exists());

        assert_eq!(clear_journals(&dir), Vec::<String>::new(), "a clean sweep reports nothing left over");
        assert!(journal_paths(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The stranded segment must be judged by the very same code as a segment reaching its close
    /// intact, or "recovery" is just a second, laxer commit policy with a better name.
    #[test]
    fn a_stranded_segment_is_a_segment_in_every_way_that_matters() {
        let stranded = Segment::stranded("2026-09-21_10-00-00").unwrap();
        let opened = Segment::opening(&at("2026-09-21_10-00-00"));
        assert_eq!(stranded.dir_name, opened.dir_name);
        assert_eq!(stranded.videofile_name, opened.videofile_name);
        assert_eq!(stranded.opened_at, opened.opened_at);
        assert_eq!(stranded.submit_marker(), SUBMIT_MARKER);
        assert!(Segment::stranded("not-a-stamp").is_none());

        let plan = Plan::default();
        let mut stranded = stranded;
        for text in ["quarterly revenue report", "quarterly revenue report", "shopping cart with socks"] {
            stranded.records.push(journaled("2026-09-21_10-00-00", text));
        }
        assert_eq!(stranded.collapse_repeats(&plan), 1, "the close path's dedup runs on replayed rows too");
        assert!(!stranded.is_committable(), "and so does the close path's floor");
    }

    #[test]
    fn a_journal_is_written_into_the_directory_it_belongs_to() {
        let root = scratch("directory");
        let segment = Segment::opening(&at("2026-09-21_10-00-00"));
        let dir = segment.directory(&root);
        assert_eq!(dir, root.join("2026-09-21_10-00-00"));
        Journal::append(&dir, &segment.journal_owner(), &journaled("2026-09-21_10-00-00", "one screen")).unwrap();
        let owner = Journal::read(&dir.join(JOURNAL_FILE)).unwrap().owner.unwrap();
        assert_eq!(owner.owner_pid, std::process::id(), "the writer names itself, so a live one can be recognised");
        assert_eq!(owner.videofile_name, "2026-09-21_10-00-00.mp4");
        assert_eq!(owner.dir_name, "2026-09-21_10-00-00");
        let _ = std::fs::remove_dir_all(&root);
    }
}
