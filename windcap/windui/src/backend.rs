//! Every call that touches a disk, a database, or the process table.
//!
//! Nothing here knows about widgets, and nothing here is allowed to be called from the frame loop:
//! `app` hands these functions to `workers` and folds the result into `model`. The seam is
//! deliberately narrow — four functions — so the question "what does the UI thread actually do?"
//! has a four-answer answer, and so a test can drive the real store contract (a real month file on
//! a real temp directory) without a window.
//!
//! Reads go through `wind_store`, never through SQLite: `Month::open_read` is what keeps a query
//! from holding a lock across a segment commit, and `search_months` is what makes paging global
//! across the month files a range touches. Reimplementing either here would be the beginning of a
//! UI that disagrees with the recorder about what the index means.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use wind_base::clock::{self, LocalParts};
use wind_base::config::Config;
use wind_store::aggregate::{self, Timeline};
use wind_store::read::{self, Month, Row};
use wind_store::search::{self, Query};
use wind_store::similar::SimilarChars;

use crate::flags;
use crate::highlight;
use crate::model::{
    BucketCell, DayOutcome, DayPoint, LibraryStats, LightboxTile, MonthDayPoint, MonthTotals, RowCard, RowKey, SearchOutcome,
    SearchParams, StripCell, YearTotals, LIGHTBOX_SLOTS,
};
use crate::record::{DisplayInfo, RecOptions};
use crate::segments::Index as SegmentIndex;
use crate::segments::Pictures;
use crate::settings::Settings;
use crate::wordcloud::{self, CloudWord, StopWords, WordCounts};

/// Upstream's own staleness window for the read copy (`db_manager.get_temp_dbfilepath`: re-copy only
/// when the origin is more than five minutes newer), kept so the native UI and the Python UI leave
/// the same `_TEMP_READ.db` files behind with the same freshness.
pub const STALE_AFTER: Duration = Duration::from_secs(300);

/// A staleness window no clock can exceed, for a read that must open the copy as it is rather than
/// rebuild it. See [`stage`].
const NEVER_REBUILD: Duration = Duration::from_secs(u64::MAX);

/// Refreshing a month's `_TEMP_READ.db` is the one part of a read that cannot be shared.
///
/// The store copies the live file over a fixed name beside it, and this app reads the same months
/// from four threads at once: at boot the footer's scan, the first day and the user's already-typed
/// query all go out together. Unguarded, one thread truncates that copy while another has it open,
/// and the loser gets either a sharing violation or — worse, because it is silent — a copy whose
/// schema pages had landed but not its data pages, which SQLite opens happily as a month with no
/// rows at all. Both are the same lie on screen: `1 month files · 0 rows indexed` about a database
/// that has rows, and a search that answers with nothing and so never asks for a thumbnail. The
/// lie is durable, because the fresh copy then looks younger than `STALE_AFTER` and no later read
/// thinks to rebuild it.
///
/// Held across the copies only, never across a query: the most one read can delay another is the
/// refresh of a copy it would otherwise have had to rebuild itself, in parallel and badly.
static TEMP_READ_REFRESH: Mutex<()> = Mutex::new(());

/// Bring these months' read copies up to date, alone, before anyone opens them.
///
/// The caller then reads with [`NEVER_REBUILD`], so the copy it opens is the one made here. A
/// failure is not reported: the read that follows sees either the copy that is already there or its
/// own error, and that is the answer worth putting in the footer.
fn stage(months: &[Month], maintaining: bool) {
    // Poisoned only if a job panicked mid-copy, which cannot leave the file worse off than a copy
    // that was interrupted by a process dying, so the surviving threads keep going.
    let _guard = TEMP_READ_REFRESH.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    for month in months {
        let _ = read::temp_read_for(&month.path, STALE_AFTER, maintaining);
    }
}

/// The activity chart's resolution. 360 s is `OneDay.get_day_statistic_chart_overview`'s pandas
/// `freq="6min"`, and it is what makes a busy hour readable without 1440 columns.
///
/// It is *only* that. It used to double as the answer to "how long was the user at the machine
/// before we stop believing it", which meant the figure was set by how many bars the window could
/// draw: a 40-minute read with a still screen counted as 6 minutes, and the day read far shorter
/// than the day felt. That question is answered by [`wind_base::config::Config::presence_gap_secs`],
/// which reads the recorder's own segment length and its still-screen pause threshold — settings a
/// user can see and change — and nothing else.
pub const BUCKET_SECS: i64 = 360;

/// A gap this long between two rows of the same window title is two sessions, not one. Upstream
/// clips at 100 s in `get_wintitle_stat_in_day`; the number decides how much "where the time went"
/// a user is told, so it is not adjusted.
pub const TITLE_GAP: i64 = 100;

/// Everything a blocking call needs, cloned into the job. Cheap: a config map, a list of file
/// metadata, and a shared segment cache.
#[derive(Clone)]
pub struct Env {
    pub config: Config,
    pub months: Vec<Month>,
    pub settings: Settings,
    pub similar: Option<SimilarChars>,
    pub segments: SegmentIndex,
    /// The same listing trick as `segments`, for the other half of a row's files: where its picture is.
    pub pictures: Pictures,
}

impl Env {
    /// Everything the workers need, from the install root. One call so `app` never assembles paths
    /// itself.
    pub fn load(root: &Path) -> Result<Env, String> {
        let config = Config::load(root).map_err(|e| e.to_string())?;
        let settings = Settings::load(&config);
        let similar = load_similar(root, settings.use_similar_ch_char_to_search);
        let months = read::discover(&config.db_dir());
        Ok(Env {
            config,
            months,
            settings,
            similar,
            segments: SegmentIndex::new(),
            pictures: Pictures::new(),
        })
    }

    /// Is the maintenance pass rewriting the index right now?
    ///
    /// `maintain_lock_dir` is a *directory* lock, and that is exactly why `exists` cannot answer this:
    /// the pass removes its own `PID` and rmdir's only a directory it created, so a run that reclaimed
    /// a corpse — or an install whose tray swept a Python container empty — leaves the directory
    /// standing with nothing in it. Asking existence read that as "maintenance forever", and the window
    /// kept its `_TEMP_READ.db` copy from the day before the first idle pass, showing an empty day to
    /// a user whose recorder had been committing rows all evening. Ask the claim, which is the `PID`
    /// child, and only while the process it names is alive.
    pub fn maintaining(&self) -> bool {
        self.config.maintain_lock_claimed()
    }

    pub fn videos_dir(&self) -> PathBuf {
        self.config.videos_dir()
    }
}

/// Re-read the glyph table after the setting was toggled, without restarting the app.
pub fn similar_table(root: &Path, enabled: bool) -> Option<SimilarChars> {
    load_similar(root, enabled)
}

/// The similar-glyph table lives beside the recorder's own config, not in `userdata`. A missing
/// file is a degraded search, not a failed one, which is the same choice `SimilarChars::load`'s
/// doc comment makes.
fn load_similar(root: &Path, enabled: bool) -> Option<SimilarChars> {
    if !enabled {
        return None;
    }
    let path = wind_base::install::config_src_file(root, "similar_CN_characters.txt");
    match SimilarChars::load(&path) {
        Ok(table) if !table.is_empty() => Some(table),
        _ => None,
    }
}

/// A system face with Chinese coverage, as `(path, bytes)`, or `None` if this machine has none.
///
/// egui's built-in fonts are Ubuntu-Light, Hack and an emoji font: not one of them contains a
/// single CJK codepoint, so every recognised Chinese sentence in the index — which is most of what
/// this application exists to show — draws as a column of empty boxes. Windows installs a
/// Simplified-Chinese face with the East Asian language support, and reading it is the only way to
/// draw the text the recorder stored.
///
/// Only plain TrueType files are offered. A `.ttc` collection needs a sub-font index that
/// `ttf-parser`'s `Face::parse` will not find on its own, and epaint's loader turns an
/// unparseable face into a panic at startup — a font the UI cannot read must degrade to boxes, not
/// stop the window from opening.
pub fn cjk_font() -> Option<(PathBuf, Vec<u8>)> {
    let dir = match std::env::var_os("WINDIR") {
        Some(windir) => PathBuf::from(windir).join("Fonts"),
        None => PathBuf::from(r"C:\Windows\Fonts"),
    };
    // Ordered by how much of the GB character set each one carries, not by size.
    for name in ["simhei.ttf", "Deng.ttf", "SIMYOU.TTF", "simkai.ttf", "STZHONGS.TTF"] {
        let path = dir.join(name);
        if let Ok(bytes) = std::fs::read(&path) {
            if !bytes.is_empty() {
                return Some((path, bytes));
            }
        }
    }
    None
}

/// One pass over the month files for the footer.
///
/// Emits a `LibraryStats` per file, because the whole pass on a five-year library is a handful of
/// seconds of `COUNT(*)` scans and a footer that stays silent for all of them is indistinguishable
/// from one that hung.
pub fn scan(env: &Env, mut on_progress: impl FnMut(LibraryStats)) {
    // Re-discover rather than reuse the list the app booted with. This is a `read_dir` of
    // `userdata/db`, which is the whole cost, and it is the only way the footer's refresh button can
    // do the thing the onboarding screen tells the user to press it for: notice a month file that did
    // not exist when the window opened.
    let months = read::discover(&env.config.db_dir());
    let total = months.len();
    let (mut rows, mut first, mut last) = (0i64, None::<i64>, None::<i64>);
    let mut errors: Vec<String> = Vec::new();
    for (i, month) in months.iter().enumerate() {
        let maintaining = env.maintaining();
        stage(std::slice::from_ref(month), maintaining);
        match month.open_read(NEVER_REBUILD, maintaining) {
            Ok(conn) => {
                match read::count_rows(&conn) {
                    Ok(count) => rows += count,
                    Err(e) => errors.push(format!("{}: {e}", month.path.display())),
                }
                if let Ok(Some((a, b))) = read::time_bounds(&conn) {
                    first = Some(first.map_or(a, |f: i64| f.min(a)));
                    last = Some(last.map_or(b, |l: i64| l.max(b)));
                }
            }
            Err(e) => errors.push(format!("{}: {e}", month.path.display())),
        }
        let done = i + 1 == total;
        on_progress(LibraryStats {
            months: months.clone(),
            months_total: total,
            months_scanned: i + 1,
            rows,
            first,
            last,
            done,
            error: if done && !errors.is_empty() {
                Some(errors.join("; "))
            } else {
                None
            },
        });
    }
    if total == 0 {
        on_progress(LibraryStats {
            months_total: 0,
            months_scanned: 0,
            rows: 0,
            first: None,
            last: None,
            done: true,
            error: None,
            ..Default::default()
        });
    }
}

/// The search the product is named after, at whatever page the user asked for.
pub fn run_search(env: &Env, params: &SearchParams) -> Result<SearchOutcome, String> {
    let started = Instant::now();
    let (from, to) = params.range(env.config.day_begin_minutes());
    let covering: Vec<Month> = read::months_in_range(&env.months, from, to).into_iter().cloned().collect();
    if covering.is_empty() {
        // No file covers the range: an empty answer, not an error. A library that starts in March
        // should say "nothing in January", not "cannot search".
        return Ok(SearchOutcome {
            cards: Vec::new(),
            total: 0,
            pages: 0,
            elapsed_ms: elapsed(started),
            params: Box::new(params.clone()),
            terms: marked_terms(env, &params.tokens()),
        });
    }

    let mut query = Query::new(from, to)
        .with_keywords(&params.keywords)
        .with_exclude(&params.exclude)
        .page(params.page_size.max(1), params.page.max(1));
    if let Some(table) = &env.similar {
        query = query.with_similar(table.clone());
    }

    // `search_months` is all-or-nothing: one unreadable month file fails the whole query. The
    // honest thing to do with that is show it, not to quietly narrow the range and report fewer
    // results than the user asked for.
    let maintaining = env.maintaining();
    stage(&covering, maintaining);
    let result = search::search_months(&covering, &query, NEVER_REBUILD, maintaining).map_err(|e| e.to_string())?;
    let (total, pages) = (result.total, result.page_count());
    let cards = result.rows.into_iter().map(|row| to_card(env, &row)).collect();
    Ok(SearchOutcome {
        cards,
        total,
        pages,
        elapsed_ms: elapsed(started),
        params: Box::new(params.clone()),
        terms: marked_terms(env, &query.tokens),
    })
}

/// The terms the result should be coloured for, which is a display promise and not a query parameter.
///
/// `enable_ocr_str_highlight_indicator` used to be a checkbox that changed nothing: both windows asked
/// this list for their colouring and got it regardless. Emptying it here is the whole fix, because it is
/// the one thing each window reads before painting a match — the egui label falls back to a single
/// unstyled run, and `Search.tsx` renders the text without a `<mark>`. Nothing about which rows matched
/// can move with it, since the search never sees this list.
fn marked_terms(env: &Env, tokens: &[String]) -> Vec<String> {
    if env.settings.enable_ocr_str_highlight_indicator {
        highlight::terms_for(tokens, env.similar.as_ref())
    } else {
        Vec::new()
    }
}

/// The day's rows, fetched once.
///
/// A month file that will not open is reported as a warning and the rest of the day still renders,
/// because the day loop is ours — unlike `search_months`, which gives no per-file granularity.
pub fn load_day(env: &Env, day: LocalParts) -> Result<DayOutcome, String> {
    let dbm = env.config.day_begin_minutes();
    let bounds = clock::day_bounds(day.year, day.month, day.day, dbm);
    let covering = read::months_in_range(&env.months, bounds.0, bounds.1);
    let mut rows: Vec<Row> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    for month in covering {
        let maintaining = env.maintaining();
        stage(std::slice::from_ref(month), maintaining);
        let loaded = month
            .open_read(NEVER_REBUILD, maintaining)
            .and_then(|conn| read::rows_in_window(&conn, Some(bounds.0), Some(bounds.1)));
        match loaded {
            Ok(mut found) => {
                for row in &mut found {
                    row.month_path = Some(month.path.clone());
                }
                rows.append(&mut found);
            }
            Err(e) => warnings.push(format!("{}: {e}", month.path.display())),
        }
    }
    rows.sort_by_key(|r| (r.time, r.rowid));

    let overview = aggregate::overview(&rows, bounds.0, bounds.1, BUCKET_SECS, env.config.presence_gap_secs());
    let active_hours = overview.hours();
    let buckets = overview
        .buckets
        .into_iter()
        .map(|b| BucketCell {
            start: b.start,
            count: b.count,
            label: b.label,
        })
        .collect();

    // The strip spans the day's own data, not the configured day: 24 hours of strip for two hours
    // of screen time would spend 90% of its pixels on nothing, and the scrubber is meant to be a
    // zoom of the part that happened.
    let (span_from, span_to) = match (overview.first, overview.last) {
        (Some(a), Some(b)) if b > a => (a, b),
        _ => (bounds.0, bounds.1),
    };
    let pics = env.settings.oneday_timeline_pic_num.clamp(2, 400) as usize;
    let timeline = Timeline::sample(&rows, span_from, span_to, pics);
    // The store's own spans are carried through untouched: `Timeline::index_for` is defined as
    // "which slot's span covers this time", and the strip's click handling must be that same
    // relation or a click and the picture it selects would disagree.
    let strip = timeline
        .points
        .iter()
        .zip(timeline.spans.iter())
        .map(|(row, (from, to))| StripCell {
            from: *from,
            to: *to,
            time: Some(row.time),
            key: Some(key_of(row)),
            thumbnail: row.thumbnail.clone(),
            // The same wall clock the card of this very row carries. Raw seconds are not a label: a front
            // end that formats `videofile_time` itself reads naive-local seconds as a UTC instant and adds
            // this machine's offset on top, so the strip said 05:57 about a picture the card called 21:57.
            clock: Some(row.when().time_display()),
        })
        .collect();

    let cards = rows.iter().map(|row| to_card(env, row)).collect();
    let flag_note_path = env.config.flag_note_path();
    Ok(DayOutcome {
        cards,
        bounds,
        buckets,
        strip,
        titles: aggregate::title_totals(&rows, TITLE_GAP),
        flags: flags::for_day(&flag_note_path, bounds.0, bounds.1),
        unindexed_video: rows.is_empty() && env.segments.has_any_segment_on_date(&env.videos_dir(), day),
        active_hours,
        strip_span: (span_from, span_to),
        warnings,
    })
}

fn elapsed(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

fn key_of(row: &Row) -> RowKey {
    let file = row
        .month_path
        .as_ref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    RowKey::new(file, row.rowid)
}

/// Which second of its segment a row's picture actually sits at.
///
/// Two numbers claim to answer this and they are not the same number.
///
/// `Row::offset_in_segment` re-derives the second from the row's timestamp, and for a re-indexed row
/// that timestamp was *computed* as `frame / record_framerate` by `wind_reindex::timeline::row_time`.
/// `Row::frame_index` reads the frame number the re-index pass wrote into the row's own picture name,
/// which is the frame the OCR ran on and therefore the frame the stored thumbnail was made from.
///
/// The divisor is the recorder's configured rate; the file the frames came out of is the one
/// `windmaint` wrote, and `maint/src/encode.rs`'s `encode_args` pins it to one frame per second on
/// purpose — "so a player that seeks to second S lands on frame S — the property every
/// `videofile_time` → offset lookup in the product rests on". Measured against this machine's live
/// September index, `record_framerate` is 2 and the segments are 1 fps, so the two disagree by a
/// factor of two on 2 542 of 2 551 rows: a row whose thumbnail is frame 416 asks for second 208 and
/// gets a picture from three and a half minutes earlier. That is the whole of "the thumbnail and the
/// picture I clicked are not the same picture", and the frame number is the one that is right.
///
/// Every row the recorder itself wrote keeps the timestamp-derived answer, because its name is a wall
/// clock rather than a frame and its time is already exact. `None` from both means the row cannot be
/// placed in its segment at all, which the frame door reads as "no frame", not as second zero.
fn seek_second(row: &Row) -> Option<i64> {
    row.frame_index().or_else(|| row.offset_in_segment())
}

/// `wind_store::Row` → the UI's row. The three columns the model refuses to carry are dropped here.
fn to_card(env: &Env, row: &Row) -> RowCard {
    let when = row.when();
    let clock_text = when.time_display();
    // `row.video_exists` is consulted, never stored: it is the cheap gate that says "do not even
    // look for this file", and the answer the user is shown comes from the directory listing.
    let segment_path = if row.video_exists {
        env.segments.resolve(&env.videos_dir(), &row.videofile_name)
    } else {
        None
    };
    // Not gated on `row.picture_exists`: that flag is written by a pass that runs on a schedule, and a
    // row whose frame arrived this morning is flagged absent until it does. One stat against the shared
    // rule is cheaper than the wrong answer, and the wrong answer here is a picture the user can see.
    let picture_path = env.pictures.resolve(
        &env.config.iframe_dir(),
        &env.config.cache_screenshot_dir(),
        &row.videofile_name,
        &row.picturefile_name,
    );
    RowCard {
        key: key_of(row),
        time: row.time,
        clock: clock_text,
        day: when.date_stamp(),
        title: row.title().map(str::to_string),
        body: row.body().to_string(),
        segment: row.videofile_name.clone(),
        offset: seek_second(row),
        deep_link: row.deep_linking.clone().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()),
        thumbnail: row.thumbnail.clone().filter(|v| !v.trim().is_empty()),
        segment_path,
        picture_path,
    }
}

/// Which door a full frame came through, because the two answers are not the same picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSource {
    /// The screenshot the recorder wrote — the frame at its recorded resolution.
    Screenshot,
    /// A frame pulled out of the segment by ffmpeg, for footage whose screenshot slice has been swept.
    Video,
}

impl FrameSource {
    /// The catalog key that names this source.
    pub fn key(self) -> &'static str {
        match self {
            FrameSource::Screenshot => "windui_frame_from_screenshot",
            FrameSource::Video => "windui_frame_from_video",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FrameSource::Screenshot => "from the screenshot cache",
            FrameSource::Video => "taken from the video at this moment",
        }
    }
}

/// The picture behind a card, at the resolution it was recorded.
pub struct Frame {
    pub bytes: Vec<u8>,
    pub source: FrameSource,
    /// Which second of the row's segment this picture *is*, when the door knows.
    ///
    /// The row's own timestamp is not that number and must not be passed off as it. A re-indexed row's
    /// `videofile_time` was computed by dividing its frame number by the configured recording rate,
    /// while the segment on disk runs at one frame per second ([`seek_second`]), so the two can be
    /// minutes apart. A viewer that captions a picture with the row's claimed second while showing the
    /// frame at another one is the same bug wearing a different hat, and this is the field that lets it
    /// say the truth instead.
    ///
    /// Both doors answer with the row's own second, because both are now opened by it: the crop named by
    /// a re-indexed row is that frame, masked, and the video seek is that frame. `None` is a row that
    /// cannot be placed in its segment at all, and such a row gets no picture either.
    pub second: Option<i64>,
}

/// The two doors a card's original frame can come out of, both resolved at query time and tried in order.
///
/// Kept apart from [`frame`], which does the reading, so the ordering is pinned by a test on a machine with
/// no ffmpeg — the only kind of machine a unit test can rely on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FramePlan {
    /// The recorder's own JPEG, at the grab's resolution: the frame the user actually saw.
    pub screenshot: Option<PathBuf>,
    /// The segment and the row's offset into it, for footage whose screenshot slice retention has swept.
    pub video: Option<(PathBuf, i64)>,
}

/// Where to look for a card's picture, in the order the doors are worth trying.
///
/// The offset is used exactly as the row carries it and never clamped. A negative one means the row's
/// second is before the segment's own first second — the file the index names cannot contain the frame
/// the row is — and seeking to zero then answers the click with the segment's opening picture, which is
/// some other row's frame wearing this row's caption. No frame is the right answer for that row; the
/// viewer says so.
pub fn frame_plan(card: &RowCard) -> FramePlan {
    FramePlan {
        screenshot: card.picture_path.clone(),
        video: card.segment_path.clone().zip(card.offset).filter(|(_, offset)| *offset >= 0),
    }
}

/// How much of `cache\frame_snapshots` the install keeps before the oldest frames go.
///
/// A 1080p JPEG out of a segment is a few hundred KB, so this ceiling is a few thousand rows: more
/// pictures than anybody opens by hand, and a number that can be said in one sentence rather than a
/// folder that grows because a curious user once clicked a lot.
pub const FRAME_SNAPSHOT_LIMIT_BYTES: u64 = 256 * 1024 * 1024;

/// Where a frame the window has already paid to extract is kept.
pub fn frame_snapshot_dir(config: &Config) -> PathBuf {
    config.cache_dir().join("frame_snapshots")
}

/// The name one row's snapshot wears, and the picture it says it holds.
///
/// The row's `videofile_time` is in the name on purpose. A month file that is rewritten can hand its
/// rowids out in another order, and a cache keyed on `file + rowid` alone would then answer a row with a
/// different row's picture — the worst thing a derived cache can do. With the time in the name, a shifted
/// row simply misses and reads again.
///
/// The **segment and the second** are in it for the same reason, and they are the part that was missing.
/// A name built from the row alone caches *a row*, and a row is not a picture: the same row can be
/// indexed twice from two different segments, and the number the last seek used is not the number this
/// one will use — a re-index that fixes an offset moves every row in the segment by it. Without the
/// seek in the key, the first wrong answer is kept and replayed forever at ten milliseconds a click,
/// which is how a wrong picture stops being a bug and starts being the user's data. With it, the frame
/// a snapshot holds can only answer for the frame it was cut from, and a revisit is still one `open()`.
///
/// Only the video door is ever written here. The screenshot door is a file the install already holds and
/// reads for the cost of opening it, so a copy would buy no speed and would keep a picture past the
/// retention rule that swept its original.
pub fn frame_snapshot_name(
    key: &RowKey,
    time: i64,
    segment: &str,
    offset: Option<i64>,
    source: FrameSource,
) -> String {
    let door = match source {
        FrameSource::Screenshot => "screenshot",
        FrameSource::Video => "video",
    };
    // Both names come out of the index, and an index is a file somebody can edit. Neither may carry a
    // separator, a `..` or a drive into a path under `cache\frame_snapshots`.
    let safe = |name: &str| -> String {
        name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
    };
    let at = offset.map(|seconds| seconds.to_string()).unwrap_or_else(|| "none".to_string());
    format!(
        "{}-{}-{time}-{}-{at}-{door}.jpg",
        safe(&key.file),
        key.rowid,
        safe(segment.rsplit(['/', '\\']).next().unwrap_or(segment))
    )
}

fn video_snapshot_path(config: &Config, card: &RowCard) -> PathBuf {
    frame_snapshot_dir(config).join(frame_snapshot_name(&card.key, card.time, &card.segment, card.offset, FrameSource::Video))
}

fn read_video_snapshot(config: &Config, card: &RowCard) -> Option<Frame> {
    let bytes = std::fs::read(video_snapshot_path(config, card)).ok().filter(|bytes| !bytes.is_empty())?;
    Some(Frame { bytes, source: FrameSource::Video, second: card.offset.filter(|seconds| *seconds >= 0) })
}

/// Keep a frame the user has already waited for, then hold the folder to its ceiling.
///
/// A snapshot that will not write is not a reason to fail the click: the frame is already in hand and is
/// handed back regardless.
fn keep_video_snapshot(config: &Config, card: &RowCard, bytes: &[u8]) {
    let dir = frame_snapshot_dir(config);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = video_snapshot_path(config, card);
    if std::fs::write(&path, bytes).is_err() {
        return;
    }
    let Ok(listing) = std::fs::read_dir(&dir) else { return };
    let mut entries = Vec::new();
    for item in listing.flatten() {
        let Ok(meta) = item.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let when = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        entries.push((item.path(), meta.len(), when));
    }
    for victim in snapshots_to_remove(entries, FRAME_SNAPSHOT_LIMIT_BYTES) {
        let _ = std::fs::remove_file(victim);
    }
}

/// Which of these files go so the folder fits `limit`, newest first.
///
/// Pure, because the two cases that matter are the ones a disk test would hide: a folder already under
/// its ceiling loses nothing, and the newest file stays even when it alone is over the ceiling — a cache
/// that deletes the thing it just wrote is not a cache.
fn snapshots_to_remove(entries: Vec<(PathBuf, u64, SystemTime)>, limit: u64) -> Vec<PathBuf> {
    let mut sorted = entries;
    sorted.sort_by(|a, b| b.2.cmp(&a.2));
    let mut kept = 0u64;
    let mut remove = Vec::new();
    for (index, (path, len, _)) in sorted.into_iter().enumerate() {
        if index == 0 || kept.saturating_add(len) <= limit {
            kept = kept.saturating_add(len);
        } else {
            remove.push(path);
        }
    }
    remove
}

/// The picture behind a card: its screenshot when that JPEG is there and readable, else a frame this
/// install has already cut out of the video at this very row's own second, else the video's frame at
/// that second, else nothing.
///
/// The screenshot goes first because it is the real thing and the snapshot is a copy of a derivation of
/// it, and a cache must not outrank the file it was made from: the slice directory gains its
/// `-VIDEO`/`-OCRED` marker while a snapshot of an older seek is sitting in the folder, and the re-index
/// that rewrites a row's picture names happens beside that. The snapshot still goes before ffmpeg, which
/// is the only reason it exists.
///
/// "Nothing" is an answer the viewer says out loud. Enlarging the stored preview instead would be
/// the bug this function exists to remove, and so would clamping a second this segment does not hold
/// down to its opening frame — that picture belongs to another row.
pub fn frame(env: &Env, card: &RowCard) -> Option<Frame> {
    let plan = frame_plan(card);
    if let Some(path) = &plan.screenshot {
        if let Some(bytes) = std::fs::read(path).ok().filter(|bytes| !bytes.is_empty()) {
            // A negative second is reported as nothing rather than as `+-30s`: it says the row sits
            // outside the segment, which the frame door has already refused to draw a picture for.
            return Some(Frame { bytes, source: FrameSource::Screenshot, second: card.offset.filter(|seconds| *seconds >= 0) });
        }
    }
    if let Some(kept) = read_video_snapshot(&env.config, card) {
        return Some(kept);
    }
    if let Some((segment, offset)) = &plan.video {
        if let Ok(bytes) = frame_from_video(&env.config.ffmpeg_path(), segment, *offset) {
            keep_video_snapshot(&env.config, card, &bytes);
            return Some(Frame { bytes, source: FrameSource::Video, second: Some(*offset) });
        }
    }
    None
}

/// The frame as base64 JPEG, for the front end that has no filesystem.
///
/// The other window hands a path to a texture uploader; an HTML window cannot read a disk, so its `frame`
/// command ships the bytes and the source arrives as a *catalog key* rather than a sentence — the words a
/// user reads have to come from the same `languages.json` every other label comes from.
pub fn frame_base64(env: &Env, card: &RowCard) -> Option<(String, &'static str)> {
    let (base64, key, _) = frame_base64_with_second(env, card)?;
    Some((base64, key))
}

/// [`frame_base64`] and the second the returned picture actually is.
///
/// The extra number is the whole point of this one. A row's caption is its `videofile_time`, and for a
/// frame cut out of a segment that is a *claim* about the instant; this is the second the door opened
/// at, and the two can disagree — see [`Frame::second`]. A viewer that shows the picture and captions it
/// with the other number is showing the user a frame and telling them about a different one, so the
/// door reports which frame it held up. `None` is a row that cannot be placed inside its own segment,
/// which is also the one case where no picture comes back — so the front end never has to render
/// "second nothing".
///
/// A sibling rather than a change to [`frame_base64`] because that signature is the one the Tauri
/// `frame` command is written against, and widening it here is what lets the front end say the second
/// without a second read.
pub fn frame_base64_with_second(env: &Env, card: &RowCard) -> Option<(String, &'static str, Option<i64>)> {
    let frame = frame(env, card)?;
    Some((base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &frame.bytes), frame.source.key(), frame.second))
}

/// One seek into a video at a time, for the whole process.
///
/// Walking the results with `→` asks for a frame per row, and every request that reaches this door starts
/// an ffmpeg that reads a whole segment off the disk. Nothing stopped a fast walk from having eight of them
/// running at once — which is not a slow window but a busy machine, and the program that loses from it is
/// the recorder capturing next door. One at a time costs the walk nothing it would not have waited for
/// anyway, and keeps the disk for the thing that has to run in real time.
static SEEK_GATE: Mutex<()> = Mutex::new(());

/// Ask ffmpeg for the one frame at `offset` seconds into `video`, as a JPEG.
///
/// `-ss` before `-i` is the fast seek: it reads the container's index instead of decoding to the mark, which
/// is the difference between a click answering and a click appearing to hang. The scratch file is removed
/// either way, because a leftover frame is screenshot the user did not ask to keep, and it is named for the
/// request rather than for the segment because two requests can be in flight at once — see
/// [`scratch_frame_path`].
fn frame_from_video(ffmpeg: &Path, video: &Path, offset: i64) -> Result<Vec<u8>, String> {
    if !video.is_file() {
        return Err(format!("{} is not there", video.display()));
    }
    // A poisoned gate means an ffmpeg thread died holding it, which must not wedge every later read: the
    // lock is taken anyway, because its only job is to keep the seeks apart, not to guard data.
    let _gate = SEEK_GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let out = scratch_frame_path(video);
    let _ = std::fs::remove_file(&out);
    // argv[0] is the program, exactly as `wind-reindex`'s frame extraction builds it, so one argv-shaped
    // list is what both doors hand to `Command`.
    let args: Vec<String> = vec![
        ffmpeg.to_string_lossy().into_owned(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-ss".into(),
        offset.to_string(),
        "-i".into(),
        video.to_string_lossy().into_owned(),
        "-frames:v".into(),
        "1".into(),
        "-q:v".into(),
        "2".into(),
        "-y".into(),
        out.to_string_lossy().into_owned(),
    ];
    let (program, rest) = args.split_first().expect("an argv builder never returns an empty vector");
    let mut command = std::process::Command::new(program);
    command.args(rest);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // The same CREATE_NO_WINDOW `locate` carries, for the same reason. Both windows are GUI processes,
        // and ffmpeg is a console program: without this, every frame pulled out of a video gives it a
        // console of its own — a black window that flashes over the picture and takes the foreground from
        // the click that asked for it, which is what reads as the whole program freezing.
        command.creation_flags(0x0800_0000);
    }
    let spawned = command.output();
    let result = match spawned {
        Ok(output) if output.status.success() => std::fs::read(&out).ok(),
        Ok(output) => {
            let _ = std::fs::remove_file(&out);
            return Err(format!(
                "ffmpeg exited {:?}: {}",
                output.status.code(),
                wind_base::decode_console_bytes(&output.stderr).trim()
            ));
        }
        Err(e) => {
            // Not on PATH, or not installable: the caller falls back and says which it is.
            return Err(format!("could not start ffmpeg: {e}"));
        }
    };
    let bytes = result.filter(|b| !b.is_empty());
    let _ = std::fs::remove_file(&out);
    bytes.ok_or_else(|| format!("ffmpeg produced no frame at {offset}s of {}", video.display()))
}

/// One scratch file per request, not one per segment.
///
/// A shared name was a real collision: the result page opens the detail drawer *and* the whole-window
/// viewer off one click, both want the same row's frame, and the second ffmpeg run truncated the file
/// the first was still reading. The loser's answer was an empty read, which the window reported as
/// "this row has no picture left on disk" — a footage-loss sentence caused by a temp-file name. The pid
/// and the counter make two requests in one process, or in two windows of one install, write two files;
/// each one is still removed by whoever made it.
fn scratch_frame_path(video: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let stem = video.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "frame".to_string());
    std::env::temp_dir().join(format!("windui_frame_{stem}-{}-{}.jpg", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)))
}

// ---------------------------------------------------------------------------------------------
// Stat: the month and year scatters, the lightbox, the word cloud
//
// All four are a different projection of the same month files, and all four run on a worker. The
// grouping and the sampling are `wind_store::aggregate`'s — `histogram` decides what a product-day
// is and `evenly_by_index` decides which rows stand for a picture — so what these functions add is the
// column-projected read that makes a year affordable to look at.
// ---------------------------------------------------------------------------------------------

/// A month's read copy, opened read-only, as `Result<_, String>`.
///
/// A macro rather than a function because the store's `Connection` type is not nameable from this
/// crate — `wind-store` does not re-export it, and it is right not to — so no signature here can say
/// what it returns. Bound locally, inference is perfectly happy.
macro_rules! read_conn {
    ($month:expr, $maintaining:expr) => {{
        let month: &Month = $month;
        stage(std::slice::from_ref(month), $maintaining);
        month
            .open_read(NEVER_REBUILD, $maintaining)
            .map_err(|e| format!("{}: {e}", month.path.display()))
    }};
}

/// Drain a query's rows, naming the file if one of them will not read.
fn collect<T, E: std::fmt::Display>(rows: impl Iterator<Item = Result<T, E>>, month: &Month) -> Result<Vec<T>, String> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("{}: {e}", month.path.display()))?);
    }
    Ok(out)
}

/// The two columns the Stat tab's grouping needs, for every row in a window.
///
/// `read::rows_in_window` is the store's only row reader and it is right to be complete: a search
/// result wants text, title and picture. A *scatter* wants `(rowid, time)` for possibly a year of
/// rows, and materialising the OCR text and base64 JPEG of 100 000 rows to place dots is hundreds of
/// megabytes for a figure no frame will ever look at. So this projects the two columns through the
/// same `open_read` temp-copy path — the read contract is unchanged, only the column list — and hands
/// the result to `aggregate::histogram`.
///
/// The window is inlined into the statement rather than bound because both ends are `i64`s computed
/// from a validated year and month; there is no string in the SQL to escape.
fn times_in(month: &Month, from: i64, to: i64, maintaining: bool) -> Result<Vec<(i64, i64)>, String> {
    let conn = read_conn!(month, maintaining)?;
    let sql = format!(
        "SELECT rowid, videofile_time FROM video_text \
         WHERE videofile_time >= {from} AND videofile_time <= {to} \
         ORDER BY videofile_time, rowid"
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| format!("{}: {e}", month.path.display()))?;
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?.unwrap_or(0))))
        .map_err(|e| format!("{}: {e}", month.path.display()))?;
    collect(rows, month)
}

/// The stored thumbnails of exactly the rows the sampler kept.
///
/// Batched by `rowid` because `rowid` *is* the primary key: after sampling there are at most
/// `LIGHTBOX_SLOTS` of them, each lookup a seek rather than a scan, which is what lets a month of
/// 100 000 rows cost one small query instead of a hundred megabytes of base64.
fn thumbnails_for(month: &Month, rowids: &[i64], maintaining: bool) -> Result<Vec<(i64, Option<String>)>, String> {
    if rowids.is_empty() {
        return Ok(Vec::new());
    }
    let conn = read_conn!(month, maintaining)?;
    let sql = format!(
        "SELECT rowid, thumbnail FROM video_text WHERE rowid IN ({})",
        rowids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| format!("{}: {e}", month.path.display()))?;
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)))
        .map_err(|e| format!("{}: {e}", month.path.display()))?;
    collect(rows, month)
}

/// The row a [`RowKey`] names, rebuilt as a card.
///
/// A month's lightbox is sampled from `rowid` and time alone — reading a hundred rows of OCR text to
/// fill a grid is the cost that shape exists to avoid — so the window holds a key and nothing else.
/// Enlarging one of those tiles cannot then go through [`frame`], which is handed a card, and it must
/// not go through a *path* the front end chose either: the index is the thing that knows which
/// screenshot and which segment a row was indexed from, and it is the only door that cannot be pointed
/// at a file the user never recorded. So the key comes back, one row is read on the click, and the two
/// frame doors share it.
///
/// A month file that is gone, or that no longer holds that row, is an error the window says out loud.
/// It is a different fact from "this row has no picture left on disk", and answering the first with the
/// second is how a moving index looks like a missing one.
pub fn card_of_key(env: &Env, key: &RowKey) -> Result<RowCard, String> {
    let maintaining = env.maintaining();
    let month = env
        .months
        .iter()
        .find(|month| month.path.file_name().and_then(|name| name.to_str()) == Some(key.file.as_str()))
        .ok_or_else(|| format!("{} is not one of the month files this install can read", key.file))?;
    let conn = read_conn!(month, maintaining)?;
    let mut row = read::row_by_rowid(&conn, key.rowid).map_err(|e| format!("{}: {e}", month.path.display()))?
        .ok_or_else(|| format!("{} no longer holds row {}", key.file, key.rowid))?;
    // [`key_of`] reads the file name back out of `month_path`, and [`to_card`] resolves the two paths a
    // frame plan is made of; a row that arrives here without its month is a card that says nothing about
    // where it came from.
    row.month_path = Some(month.path.clone());
    Ok(to_card(env, &row))
}

/// The picture behind a row the window only holds a key for, as base64 JPEG.
///
/// [`frame_base64`] on the other side of one lookup, so the lightbox tile and the result card promise
/// exactly the same thing and cannot drift apart.
pub fn frame_of_key(env: &Env, key: &RowKey) -> Result<Option<(String, &'static str)>, String> {
    let card = card_of_key(env, key)?;
    Ok(frame_base64(env, &card).map(|(base64, source_key)| (base64, source_key)))
}

/// [`frame_of_key`] and the second of the segment the picture is really from.
///
/// Written as its own door rather than a wider return because the row is read once here and the number
/// comes out of the same read, so the front end gets the picture and its instant from one lookup and
/// cannot be handed a second that belongs to a different row than the bytes.
pub fn frame_of_key_with_second(env: &Env, key: &RowKey) -> Result<Option<(String, &'static str, Option<i64>)>, String> {
    let card = card_of_key(env, key)?;
    Ok(frame_base64_with_second(env, &card))
}

/// One month file's recognised text, folded into a running term count chunk by chunk.
///
/// `LIMIT`/`OFFSET` rather than one statement because the alternative is holding a month's OCR in a
/// `String` before anything counts it — for a heavy user that is a hundred megabytes of transient text
/// whose only purpose is to be reduced to a few hundred counters. Each chunk is tallied and dropped.
///
/// A chunk that fails is recorded and ends this file's pass: half a month's cloud is still a cloud,
/// and the alternative is that one unreadable file takes the whole panel down.
fn tally_into(month: &Month, from: i64, to: i64, maintaining: bool, stop: &StopWords, counts: &mut WordCounts) {
    const CHUNK: i64 = 4_000;
    let conn = match read_conn!(month, maintaining) {
        Ok(conn) => conn,
        Err(e) => {
            counts.note(e);
            return;
        }
    };
    let mut offset = 0i64;
    loop {
        let sql = format!(
            "SELECT ocr_text FROM video_text WHERE videofile_time >= {from} AND videofile_time <= {to} \
             ORDER BY rowid LIMIT {CHUNK} OFFSET {offset}"
        );
        // One scope per chunk: `stmt` borrows `conn` and `rows` borrows `stmt`, so the loop has to
        // drop both before preparing the next page.
        let read = (|| -> Result<i64, String> {
            let mut stmt = conn.prepare(&sql).map_err(|e| format!("{}: {e}", month.path.display()))?;
            let rows = stmt
                .query_map([], |row| Ok(row.get::<_, Option<String>>(0)?.unwrap_or_default()))
                .map_err(|e| format!("{}: {e}", month.path.display()))?;
            let mut seen = 0i64;
            for row in rows {
                let text = row.map_err(|e| format!("{}: {e}", month.path.display()))?;
                seen += 1;
                for term in wordcloud::terms(&text) {
                    if !stop.contains(&term) {
                        counts.add(&term);
                    }
                }
            }
            Ok(seen)
        })();
        match read {
            Ok(seen) if seen >= CHUNK => offset += CHUNK,
            Ok(_) => return,
            Err(e) => {
                counts.note(e);
                return;
            }
        }
    }
}

/// A `Row` carrying only what the shared aggregators look at.
///
/// `aggregate::histogram` reads `time` and `rowid`; `aggregate::evenly_by_index` reads `time` and
/// clones the row through. Building the real struct with empty text and no picture is how those two
/// get to run over a projected read without either a second copy of the arithmetic here, or a wider
/// API in `wind-store` that nothing but the UI would use.
fn stub(month: &Month, rowid: i64, time: i64) -> wind_store::read::Row {
    wind_store::read::Row {
        rowid,
        videofile_name: String::new(),
        picturefile_name: String::new(),
        time,
        ocr_text: String::new(),
        video_exists: false,
        picture_exists: false,
        thumbnail: None,
        win_title: None,
        deep_linking: None,
        month_path: Some(month.path.clone()),
    }
}

/// The inclusive calendar-month window: first day `00:00:00` to last day `23:59:59`.
///
/// Calendar, not product, deliberately. `day_begin_minutes` is applied inside `aggregate::histogram`,
/// which is the only place that knows what a product-day is; clipping the *query* to it as well would
/// drop the first and last days' early hours twice over.
fn month_window(year: i64, month: u32) -> (i64, i64) {
    let start = LocalParts { year, month, day: 1, hour: 0, minute: 0, second: 0 };
    let end = LocalParts {
        year,
        month,
        day: wind_base::clock::days_in_month(year, month),
        hour: 23,
        minute: 59,
        second: 59,
    };
    (start.naive_epoch_seconds(), end.naive_epoch_seconds())
}

/// Per-product-day row counts and hours across one calendar month.
///
/// Points are restricted to the month being shown: with `day_begin_minutes = 180` a row at 01:00 on
/// the 1st belongs to the last day of the previous month, and a scatter whose x axis is "day of
/// September" has nowhere to put an August 31st.
pub fn month_totals(env: &Env, year: i64, month: u32) -> Result<MonthTotals, String> {
    let (from, to) = month_window(year, month);
    let mut totals = MonthTotals::default();
    let covering: Vec<&Month> = read::months_in_range(&env.months, from, to).into_iter().collect();
    if covering.is_empty() {
        return Ok(totals);
    }
    let maintaining = env.maintaining();
    let mut rows = Vec::new();
    for file in &covering {
        match times_in(file, from, to, maintaining) {
            Ok(found) => rows.extend(found.into_iter().map(|(rowid, time)| stub(file, rowid, time))),
            Err(e) => totals.warnings.push(e),
        }
    }
    totals.rows = rows.len() as i64;
    totals.points = aggregate::histogram(&rows, env.config.day_begin_minutes(), env.config.presence_gap_secs())
        .into_iter()
        .filter(|stat| stat.year == year && stat.month == month)
        .map(|stat| DayPoint { day: stat.day, rows: stat.rows, hours: stat.hours })
        .collect();
    Ok(totals)
}

/// Per-(month, day) row counts for a whole year: the same histogram, read from a year away.
pub fn year_totals(env: &Env, year: i64) -> Result<YearTotals, String> {
    let from = LocalParts { year, month: 1, day: 1, hour: 0, minute: 0, second: 0 }.naive_epoch_seconds();
    let to = LocalParts { year, month: 12, day: 31, hour: 23, minute: 59, second: 59 }.naive_epoch_seconds();
    let mut totals = YearTotals::default();
    let covering: Vec<&Month> = read::months_in_range(&env.months, from, to).into_iter().collect();
    let maintaining = env.maintaining();
    let mut rows = Vec::new();
    for file in &covering {
        match times_in(file, from, to, maintaining) {
            Ok(found) => rows.extend(found.into_iter().map(|(rowid, time)| stub(file, rowid, time))),
            Err(e) => totals.warnings.push(e),
        }
    }
    totals.rows = rows.len() as i64;
    totals.points = aggregate::histogram(&rows, env.config.day_begin_minutes(), env.config.presence_gap_secs())
        .into_iter()
        .filter(|stat| stat.year == year)
        .map(|stat| MonthDayPoint { month: stat.month, day: stat.day, rows: stat.rows })
        .collect();
    Ok(totals)
}

/// The month's lightbox: up to `LIGHTBOX_SLOTS` tiles, sampled evenly by row rather than by time.
///
/// `distributeavg` is upstream's own mode for this picture — `generate_lightbox_from_datetime_range`'s
/// default — and it is the right one here: a contact sheet is a record of *what there was*, and
/// sampling by time would spend most of the slots on the two hours a day the user was busy.
///
/// Two stated departures from the Python:
///
///   * upstream returns `false` and logs "Not enough images" when the month holds fewer captures than
///     slots, so in a quiet month the button draws *nothing at all*. This tiles what exists and labels
///     the empty remainder, because a half-filled contact sheet is the honest picture of one.
///   * the grid is painted out of the texture cache instead of being composited into a PNG under
///     `result_lightbox` and base64'd back onto the page, which is the file round trip this crate
///     exists to remove. See the module note in `main.rs`.
pub fn lightbox(env: &Env, year: i64, month: u32) -> Result<Vec<LightboxTile>, String> {
    let (from, to) = month_window(year, month);
    let covering: Vec<&Month> = read::months_in_range(&env.months, from, to).into_iter().collect();
    let maintaining = env.maintaining();
    let mut warnings: Vec<String> = Vec::new();
    let mut rows: Vec<wind_store::read::Row> = Vec::new();
    for file in &covering {
        match times_in(file, from, to, maintaining) {
            Ok(found) => rows.extend(found.into_iter().map(|(rowid, time)| stub(file, rowid, time))),
            Err(e) => warnings.push(e),
        }
    }
    if rows.is_empty() {
        return if warnings.is_empty() { Ok(Vec::new()) } else { Err(warnings.join("; ")) };
    }
    rows.sort_by_key(|r| (r.time, r.rowid));
    let picked = aggregate::evenly_by_index(&rows, LIGHTBOX_SLOTS);

    // One thumbnail query per month file, for just the rows the sampler kept.
    let mut tiles: Vec<LightboxTile> = Vec::with_capacity(picked.len());
    for file in &covering {
        let here = file.path.as_path();
        let mine: Vec<&wind_store::read::Row> = picked.iter().filter(|r| r.month_path.as_deref() == Some(here)).collect();
        if mine.is_empty() {
            continue;
        }
        let ids: Vec<i64> = mine.iter().map(|r| r.rowid).collect();
        let found = match thumbnails_for(file, &ids, maintaining) {
            Ok(found) => found,
            Err(e) => {
                warnings.push(e);
                continue;
            }
        };
        let mut by_rowid: std::collections::BTreeMap<i64, Option<String>> = found.into_iter().collect();
        for row in mine {
            let thumbnail = by_rowid.remove(&row.rowid).flatten().filter(|v| !v.trim().is_empty());
            tiles.push(LightboxTile { key: key_of(row), time: row.time, thumbnail, stamp: row.when().display() });
        }
    }
    // Time order across the files, because the grid is meant to read as a month and not as a list.
    tiles.sort_by_key(|t| (t.time, t.key.rowid, t.key.file.clone()));
    if tiles.is_empty() && !warnings.is_empty() {
        return Err(warnings.join("; "));
    }
    Ok(tiles)
}

/// The month's word cloud, ranked.
///
/// The whole reduction happens here rather than in a frame: reading, segmenting and counting a
/// month's text is the one part of this screen whose cost is proportional to how much the user
/// recorded. Only a few hundred counters cross back.
pub fn word_cloud(env: &Env, year: i64, month: u32, stop: &StopWords, limit: usize) -> Result<Vec<CloudWord>, String> {
    let (from, to) = month_window(year, month);
    let covering: Vec<&Month> = read::months_in_range(&env.months, from, to).into_iter().collect();
    let maintaining = env.maintaining();
    let mut counts = WordCounts::default();
    for file in &covering {
        tally_into(file, from, to, maintaining, stop, &mut counts);
    }
    let warnings = std::mem::take(&mut counts.warnings);
    let ranked = counts.rank(limit);
    if ranked.is_empty() && !warnings.is_empty() {
        return Err(warnings.join("; "));
    }
    Ok(ranked)
}

/// The month's stop-word set: the shipped file plus the user's own list, which is exactly how
/// `wordcloud.py` builds its module-level `stopwords` at import time.
pub fn stop_words(root: &Path, config: &Config) -> StopWords {
    let path = wind_base::install::config_src_file(root, "wordcloud_stopword.txt");
    let mut list: Vec<String> = std::fs::read_to_string(&path).unwrap_or_default().split(',').map(str::to_string).collect();
    list.extend(config.str_list("wordcloud_user_stop_words"));
    StopWords::new(list)
}

/// The encoder and accelerator lists, from the two preset files beside the config.
///
/// Read once at boot into `AppState::rec_options`, because `recording.py` does the same at import
/// (`get_record_preset_json`) and a settings screen whose encoder box is empty cannot be repaired by
/// pressing Save. A missing or unreadable file falls back to the shipped contents rather than to an
/// empty list. `serde_json`'s default map is sorted, so the order here is alphabetical rather than the
/// file's, which is the better answer: the list survives someone reordering the preset file.
pub fn rec_options(root: &Path) -> RecOptions {
    let src = wind_base::install::config_src_dir(root).unwrap_or_else(|| root.join("config_src"));
    let read = |name: &str| -> Option<serde_json::Value> {
        std::fs::read_to_string(src.join(name)).ok().and_then(|text| serde_json::from_str(&text).ok())
    };
    let defaults = RecOptions::default();
    let record_encoders = read("record_preset.json")
        .and_then(|v| v.as_object().map(|map| map.keys().cloned().collect::<Vec<_>>()))
        .unwrap_or_default();
    let compress = read("video_compress_preset.json")
        .and_then(|v| {
            v.as_object().map(|map| {
                map.iter()
                    .map(|(encoder, row)| {
                        (encoder.clone(), row.as_object().map(|row| row.keys().cloned().collect()).unwrap_or_default())
                    })
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default();
    RecOptions {
        record_encoders: if record_encoders.is_empty() { defaults.record_encoders } else { record_encoders },
        compress: if compress.is_empty() { defaults.compress } else { compress },
        cpu_cores: std::thread::available_parallelism().map(|n| n.get() as i64).unwrap_or(1),
    }
}

/// Every panel on the desktop, at the pixel sizes it actually has.
///
/// Runs on a worker thread, and that is load-bearing rather than tidy: `EnumDisplayMonitors` reports
/// *virtualised* rectangles to a thread that is not DPI aware, so on a scaled display the number this
/// reports is not the number that gets recorded. `make_thread_dpi_aware` pins the calling thread,
/// which is precisely why it must not be the frame thread — doing it there would change how eframe
/// scales its own window for the rest of the session.
pub fn displays() -> Vec<DisplayInfo> {
    windcap::capture::make_thread_dpi_aware();
    windcap::capture::monitors()
        .into_iter()
        .map(|m| DisplayInfo { index: m.index, width: m.width, height: m.height, primary: m.primary })
        .collect()
}

/// The one command that has to be answered by a subprocess rather than a thread.
///
/// `explorer.exe /select,<path>` is the whole of "reveal in Explorer"; no Win32 call is needed, so
/// none is made. Explorer exits nonzero even when it worked, which is why the child is spawned and
/// never waited on.
pub fn locate(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("{} is no longer on disk", path.display()));
    }
    let mut command = std::process::Command::new("explorer.exe");
    command.arg(format!("/select,{}", path.display()));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: explorer's helper process otherwise flashes a console for a frame.
        command.creation_flags(0x0800_0000);
    }
    command.spawn().map_err(|e| format!("explorer: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {

    use super::*;
    use wind_store::write::{Record, Store};

    fn fixture(tag: &str, rows: &[(&str, i64, &str, &str)]) -> (PathBuf, Config, Vec<Month>) {
        let dir = std::env::temp_dir().join(format!("windui-backend-{tag}-{}-{}", std::process::id(), crate::fixtures::next_scratch_id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = dir.join("userdata").join("db");
        let mut store = Store::open_month(&db, "default", 2026, 9).expect("month file");
        let batch: Vec<Record> = rows
            .iter()
            .map(|(name, time, text, title)| Record {
                videofile_name: name.to_string(),
                picturefile_name: String::new(),
                videofile_time: *time,
                ocr_text: text.to_string(),
                win_title: Some(title.to_string()),
                deep_linking: None,
                // No thumbnail: a test that decodes a JPEG is testing the codec, not this code path.
                thumbnail: None,
            })
            .collect();
        store.append(&batch).expect("append");
        drop(store);
        let config = Config::load(&dir).expect("an absent config still loads, with defaults");
        let months = read::discover(&config.db_dir());
        (dir, config, months)
    }

    fn env_with(config: Config, months: Vec<Month>) -> Env {
        let settings = Settings::load(&config);
        Env {
            config,
            months,
            settings,
            similar: None,
            segments: SegmentIndex::new(),
            pictures: Pictures::new(),
        }
    }

    fn at(stamp: &str) -> i64 {
        LocalParts::from_stamp(stamp).unwrap().naive_epoch_seconds()
    }

    /// The highlight checkbox is a promise about colouring, and both windows colour from the one list
    /// this function returns — so switching it off has to empty *that*, which is the only place the
    /// promise can be kept for both doors at once. What must not move is the answer: the same rows, the
    /// same total, with nothing marked.
    #[test]
    fn turning_the_highlight_off_empties_the_terms_and_touches_nothing_else() {
        let (dir, config, months) = fixture(
            "highlight",
            &[("2026-09-21_10-00-00.mp4", at("2026-09-21_10-05-30"), "quarterly revenue", "Excel")],
        );
        let params = SearchParams {
            keywords: "revenue".into(),
            exclude: String::new(),
            from: LocalParts::from_stamp("2026-09-21_00-00-00").unwrap(),
            to: LocalParts::from_stamp("2026-09-21_23-59-59").unwrap(),
            page: 1,
            page_size: 20,
        };
        let mut env = env_with(config, months);

        let on = run_search(&env, &params).expect("the switch is on by default");
        assert_eq!(on.terms, vec!["revenue".to_string()], "and the terms reach the highlighter");

        env.settings.enable_ocr_str_highlight_indicator = false;
        let off = run_search(&env, &params).expect("the same query, one switch lower");
        assert!(off.terms.is_empty(), "{:?}", off.terms);
        assert_eq!(off.total, on.total, "which rows matched cannot depend on the colouring");
        assert_eq!(off.cards.len(), on.cards.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_real_month_file_answers_a_real_query() {
        let (dir, config, months) = fixture(
            "search",
            &[
                ("2026-09-21_10-00-00.mp4", at("2026-09-21_10-05-30"), "quarterly revenue", "Excel"),
                ("2026-09-21_11-00-00.mp4", at("2026-09-21_11-00-00"), "nothing relevant", "Notepad"),
            ],
        );
        let env = env_with(config, months);
        let params = SearchParams {
            keywords: "revenue".into(),
            exclude: String::new(),
            from: LocalParts::from_stamp("2026-09-21_00-00-00").unwrap(),
            to: LocalParts::from_stamp("2026-09-21_23-59-59").unwrap(),
            page: 1,
            page_size: 20,
        };
        let outcome = run_search(&env, &params).expect("search");
        assert_eq!(outcome.total, 1);
        assert_eq!(outcome.cards.len(), 1);
        assert_eq!(outcome.cards[0].clock, "10:05:30");
        assert_eq!(outcome.cards[0].body, "quarterly revenue", "the title is split out of the text");
        assert_eq!(outcome.cards[0].title.as_deref(), Some("Excel"));
        assert_eq!(outcome.cards[0].offset, Some(330), "330 s into the segment");
        assert_eq!(outcome.terms, vec!["revenue".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_two_of_the_same_query_is_a_different_set_of_rows() {
        let rows: Vec<(String, i64, String, String)> = (0..5)
            .map(|i| {
                let stamp = format!("2026-09-21_10-0{}-00", i);
                (format!("{stamp}.mp4"), at(&stamp), format!("needle number {i}"), "x".into())
            })
            .collect();
        let refs: Vec<(&str, i64, &str, &str)> = rows.iter().map(|r| (r.0.as_str(), r.1, r.2.as_str(), r.3.as_str())).collect();
        let (dir, config, months) = fixture("paging", &refs);
        let env = env_with(config, months);
        let base = SearchParams {
            keywords: "needle".into(),
            exclude: String::new(),
            from: LocalParts::from_stamp("2026-09-21_00-00-00").unwrap(),
            to: LocalParts::from_stamp("2026-09-21_23-59-59").unwrap(),
            page: 1,
            page_size: 2,
        };
        let first = run_search(&env, &base).expect("page 1");
        let second = run_search(&env, &SearchParams { page: 2, ..base.clone() }).expect("page 2");
        assert_eq!((first.total, first.pages), (5, 3));
        assert_eq!(first.cards.len(), 2);
        assert_ne!(
            first.cards.iter().map(|c| c.key.clone()).collect::<Vec<_>>(),
            second.cards.iter().map(|c| c.key.clone()).collect::<Vec<_>>()
        );
        assert_eq!(second.cards[0].body, "needle number 2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_day_fetch_honours_the_day_boundary_and_fills_the_chart() {
        let (dir, config, months) = fixture(
            "day",
            &[
                // 01:00 on the 22nd is the 21st's work with the shipped day_begin_minutes = 180.
                ("2026-09-22_01-00-00.mp4", at("2026-09-22_01-00-00"), "after midnight", "Late"),
                ("2026-09-21_09-00-00.mp4", at("2026-09-21_09-00-00"), "morning", "Excel"),
                ("2026-09-21_10-00-00.mp4", at("2026-09-21_10-00-00"), "still morning", "Excel"),
                // Belongs to the 22nd's day, must not leak into the 21st.
                ("2026-09-21_02-00-00.mp4", at("2026-09-21_02-00-00"), "before the boundary", "Sleep"),
            ],
        );
        let env = env_with(config, months);
        let day = load_day(&env, LocalParts::from_stamp("2026-09-21_12-00-00").unwrap()).expect("day");
        let bodies: Vec<&str> = day.cards.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, vec!["morning", "still morning", "after midnight"]);
        assert_eq!(day.bounds, (at("2026-09-21_03-00-00"), at("2026-09-22_02-59-59")));
        assert_eq!(day.buckets.len(), 240, "a whole day at six-minute resolution");
        assert_eq!(
            day.buckets.iter().filter(|b| b.count > 0).count(),
            3,
            "09:00, 10:00 and 01:00 are three slots"
        );
        assert_eq!(day.titles[0].0, "Excel");
        assert!(day.active_hours > 0.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_day_with_nothing_indexed_is_distinguished_from_a_day_with_unindexed_video() {
        let (dir, config, months) = fixture("emptyday", &[("2026-09-21_10-00-00.mp4", at("2026-09-21_10-00-00"), "x", "y")]);
        let env = env_with(config, months);
        let day = load_day(&env, LocalParts::from_stamp("2026-01-01_12-00-00").unwrap()).expect("no data at all");
        assert!(day.cards.is_empty());
        assert!(!day.unindexed_video, "nothing on disk either");

        let videos = dir.join("userdata").join("videos").join("2026-03");
        std::fs::create_dir_all(&videos).unwrap();
        std::fs::write(videos.join("2026-03-05_10-00-00-VIDEO.mp4"), b"mp4").unwrap();
        let day = load_day(&env, LocalParts::from_stamp("2026-03-05_12-00-00").unwrap()).expect("video, no index");
        assert!(day.cards.is_empty());
        assert!(day.unindexed_video, "recorded but never indexed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_month_outside_the_requested_range_is_never_opened() {
        let (dir, config, months) = fixture("routing", &[("2026-09-21_10-00-00.mp4", at("2026-09-21_10-00-00"), "x", "y")]);
        let env = env_with(config, months);
        assert_eq!(env.months.len(), 1);
        let params = SearchParams {
            keywords: "x".into(),
            exclude: String::new(),
            from: LocalParts::from_stamp("2020-01-01_00-00-00").unwrap(),
            to: LocalParts::from_stamp("2020-01-02_00-00-00").unwrap(),
            page: 1,
            page_size: 20,
        };
        let outcome = run_search(&env, &params).expect("an out-of-range query is an answer");
        assert_eq!(outcome.total, 0);
        assert_eq!(outcome.pages, 0);
        // The temp copy is created next to the origin, so its absence proves no file was opened.
        assert!(!dir.join("userdata/db/default_2026-09_wind.db_TEMP_READ.db").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_footer_pass_counts_rows_and_the_extremes_of_the_index() {
        let (dir, config, months) = fixture(
            "footer",
            &[
                ("2026-09-21_10-00-00.mp4", at("2026-09-21_10-00-00"), "a", "b"),
                ("2026-09-22_19-48-12.mp4", at("2026-09-22_19-48-12"), "c", "d"),
            ],
        );
        let env = env_with(config, months);
        let mut seen = Vec::new();
        scan(&env, |stats| seen.push(stats));
        assert_eq!(seen.len(), 1, "one month file, one progress event");
        let last = seen.last().unwrap();
        assert!(last.done);
        assert_eq!((last.months_total, last.rows), (1, 2));
        assert_eq!(last.first, Some(at("2026-09-21_10-00-00")));
        assert_eq!(last.last, Some(at("2026-09-22_19-48-12")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A payload-shaped root carries `config_src/` at the top level and no
    /// `windrecorder/` at all, and every shipped settings file must still be found.
    ///
    /// This is the case the rest of the suite cannot catch: the repository has both
    /// locations, so a hardcoded `root/windrecorder/config_src/...` passes every other
    /// test while silently returning nothing on an installed machine. The failure is
    /// invisible by design -- an absent stop-word file yields a word cloud full of
    /// "the", an absent similarity table yields a search that quietly stops matching
    /// look-alike Chinese glyphs, and an absent preset file falls back to compiled-in
    /// encoder names. None of them raise an error, which is why this asserts on
    /// contents rather than on the call not panicking.
    #[test]
    fn a_standalone_payload_root_still_finds_its_settings() {
        let shipped = {
            let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
            manifest.parent().and_then(Path::parent).expect("workspace layout").join("config_src")
        };
        let dir = std::env::temp_dir().join(format!("windui-standalone-{}-{}", std::process::id(), crate::fixtures::next_scratch_id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("install");
        let src = root.join("config_src");
        std::fs::create_dir_all(&src).unwrap();
        for name in ["wordcloud_stopword.txt", "similar_CN_characters.txt", "record_preset.json", "video_compress_preset.json"] {
            std::fs::copy(shipped.join(name), src.join(name)).expect("fixture is shipped");
        }
        assert!(!root.join("windrecorder").exists(), "a standalone install has no Python package directory");

        let config = Config::load(&root).expect("config loads from the payload layout");
        let words = stop_words(&root, &config);
        assert!(words.contains("我"), "the shipped stop-word list was not read from config_src/");

        let options = rec_options(&root);
        assert!(!options.record_encoders.is_empty(), "record_preset.json was not read from config_src/");
        assert!(!options.compress.is_empty(), "video_compress_preset.json was not read from config_src/");

        let similar = load_similar(&root, true).expect("similar_CN_characters.txt was not read from config_src/");
        assert!(similar.covered_characters() > 100, "the table loaded but covers {0} characters", similar.covered_characters());
        assert!(!similar.alternatives_for('风').is_empty(), "a known group resolves to nothing");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- the original frame behind a card ---------------------------------------------------------

    fn env_for(root: &Path) -> Env {
        let config = Config::load(root).expect("a root with no settings still loads on the compiled-in defaults");
        Env {
            settings: Settings::load(&config),
            months: Vec::new(),
            similar: None,
            segments: SegmentIndex::new(),
            pictures: Pictures::new(),
            config,
        }
    }

    fn card_at(picture: Option<PathBuf>, segment: Option<PathBuf>, offset: Option<i64>) -> RowCard {
        RowCard {
            key: crate::model::RowKey::new("default_2026-09_wind.db", 1),
            time: 0,
            clock: "10:00:08".into(),
            day: "2026-09-21".into(),
            title: None,
            body: String::new(),
            segment: "2026-09-21_10-00-00.mp4".into(),
            offset,
            deep_link: None,
            thumbnail: None,
            segment_path: segment,
            picture_path: picture,
        }
    }

    #[test]
    fn the_screenshot_is_tried_before_the_video() {
        let card = card_at(Some(PathBuf::from("C:/cache/8.jpg")), Some(PathBuf::from("C:/v/8.mp4")), Some(12));
        let plan = frame_plan(&card);
        assert_eq!(plan.screenshot.as_deref(), Some(Path::new("C:/cache/8.jpg")));
        assert_eq!(plan.video, Some((PathBuf::from("C:/v/8.mp4"), 12)));
    }

    /// The bug, in the user's words: the small picture in a row and the full-size picture the row's
    /// click opens are not the same picture.
    ///
    /// The thumbnail is stored bytes, made from one specific frame; the click is a seek. When the two
    /// disagree about which frame, the row shows one moment and opens another, and "a different
    /// segment, or an old picture" is what it looks like from the outside.
    ///
    /// The numbers here are this machine's live September index, not a fiction. `record_framerate` is
    /// 2 and `windmaint`'s `encode_args` writes every segment at one frame per second, so a row
    /// indexed from frame 416 was given a `videofile_time` 208 seconds into its segment — half the
    /// second its own frame is at. Verified against `userdata/videos/2026-09/…_14-19-40-OCRED.mp4`:
    /// its rows' stored thumbnails match the video at second `frame_index` and do not match it at
    /// `frame_index / 2`. The same check against `…_12-19-42-OCRED.mp4` puts row 92's picture at
    /// second 216 and its click at 108 — three and a half minutes of someone's afternoon, gone.
    #[test]
    fn the_frame_a_row_shows_is_the_frame_that_row_came_from() {
        let root = std::env::temp_dir().join(format!("windui-frame-right-row-{}-{}", std::process::id(), crate::fixtures::next_scratch_id()));
        let _ = std::fs::remove_dir_all(&root);
        let videos = root.join("userdata").join("videos").join("2026-09");
        std::fs::create_dir_all(&videos).unwrap();
        std::fs::write(videos.join("2026-09-21_10-00-00.mp4"), b"mp4").unwrap();
        let env = env_for(&root);
        let start = at("2026-09-21_10-00-00");
        let segment = videos.join("2026-09-21_10-00-00.mp4");

        let row = |picture: &str, time: i64| Row {
            rowid: 416,
            videofile_name: "2026-09-21_10-00-00.mp4".into(),
            picturefile_name: picture.into(),
            time,
            ocr_text: String::new(),
            win_title: None,
            deep_linking: None,
            thumbnail: None,
            video_exists: true,
            picture_exists: false,
            month_path: None,
        };

        // A row the re-index pass wrote: named for frame 416, stamped as if the segment ran at 2 fps.
        let card = to_card(&env, &row("416_cropped.jpg", start + 208));
        assert_eq!(card.offset, Some(416), "the row's own picture name is the frame it came from");
        assert_eq!(frame_plan(&card).video, Some((segment.clone(), 416)), "and the click seeks there, not to the stamped half");

        // A row the recorder wrote is already exact: its name is the wall clock, not a frame number.
        let card = to_card(&env, &row("2026-09-21_10-03-30.jpg", start + 210));
        assert_eq!(card.offset, Some(210), "a wall-clock name is not read as a frame index");
        assert_eq!(frame_plan(&card).video, Some((segment, 210)));

        // A row stamped before its own segment started — the first frame of a restarted recording — has
        // no frame in the file the index names, and gets no seek rather than an invented one.
        let card = to_card(&env, &row("who-knows.jpg", start - 30));
        assert_eq!(card.offset, Some(-30), "the row's own arithmetic, reported honestly and not clamped");
        assert_eq!(frame_plan(&card).video, None, "and the door closed on it, because second 0 is another row's picture");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The other half of the promise: the frame a row has already paid ffmpeg for may only ever answer
    /// *that* frame. A cache keyed on the row alone freezes the first wrong answer and replays it
    /// forever, and the freeze is what turns an occasional wrong picture into the user's data.
    #[test]
    fn a_snapshot_only_ever_answers_for_the_frame_it_was_cut_from() {
        let root = std::env::temp_dir().join(format!("windui-frame-stale-cache-{}-{}", std::process::id(), crate::fixtures::next_scratch_id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let env = env_for(&root);

        let kept = card_at(None, None, Some(416));
        keep_video_snapshot(&env.config, &kept, b"the frame at second 416");

        // The same row, seeked at the second the index used to believe. It must miss.
        let moved = card_at(None, None, Some(208));
        assert!(read_video_snapshot(&env.config, &moved).is_none(), "a different second is a different picture");
        // And the same second under another segment's name, and another row's, miss too.
        let other_segment = RowCard { segment: "2026-09-21_11-00-00.mp4".into(), ..kept.clone() };
        assert!(read_video_snapshot(&env.config, &other_segment).is_none());
        let other_row = RowCard { key: crate::model::RowKey::new("default_2026-09_wind.db", 99), ..kept.clone() };
        assert!(read_video_snapshot(&env.config, &other_row).is_none());

        let found = frame(&env, &kept).expect("its own frame is still there");
        assert_eq!(found.bytes, b"the frame at second 416");
        assert_eq!(found.second, Some(416), "and it says which second it is, so the viewer can say it out loud");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A copy of a picture must not outrank the picture. The screenshot door is the file the recorder
    /// wrote; the snapshot is one ffmpeg's output. Reading the cache first answered a row whose slice
    /// had just arrived (or just been re-indexed) with the older of the two.
    #[test]
    fn a_rows_own_screenshot_beats_a_frame_this_window_cached_for_it() {
        let root = std::env::temp_dir().join(format!("windui-frame-order-{}-{}", std::process::id(), crate::fixtures::next_scratch_id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let env = env_for(&root);
        let picture = root.join("2026-09-21_10-00-08.jpg");
        std::fs::write(&picture, b"the recorder's own JPEG").unwrap();

        let card = card_at(Some(picture), None, Some(416));
        keep_video_snapshot(&env.config, &card, b"an older ffmpeg answer");
        let found = frame(&env, &card).expect("the screenshot is there");
        assert_eq!(found.source, FrameSource::Screenshot);
        assert_eq!(found.bytes, b"the recorder's own JPEG");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A row whose second is before its segment's own first second names a frame the file cannot hold.
    /// Seeking to zero then answers the click with the segment's opening picture — another row's frame
    /// under this row's caption — so the door closes instead, and the viewer says "no frame for this
    /// row". That sentence is acceptable; a wrong picture is not.
    #[test]
    fn a_row_from_before_its_segment_starts_gets_no_frame_instead_of_the_segments_first() {
        let card = card_at(None, Some(PathBuf::from("C:/v/8.mp4")), Some(-30));
        assert_eq!(frame_plan(&card).video, None, "a negative second is not a seek, and it is not second zero either");
        // The row still gets an answer when its screenshot survived, because that door has no seek in it.
        let card = card_at(Some(PathBuf::from("C:/cache/8.jpg")), Some(PathBuf::from("C:/v/8.mp4")), Some(-30));
        assert_eq!(frame_plan(&card).screenshot.as_deref(), Some(Path::new("C:/cache/8.jpg")));
        assert_eq!(frame_plan(&card).video, None);
    }

    #[test]
    fn a_card_reads_its_surviving_screenshot_at_full_size() {
        let root = std::env::temp_dir().join(format!("windui-frame-read-{}-{}", std::process::id(), crate::fixtures::next_scratch_id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let picture = root.join("2026-09-21_10-00-08.jpg");
        std::fs::write(&picture, b"the whole frame").unwrap();
        let env = env_for(&root);

        let found = frame(&env, &card_at(Some(picture.clone()), None, None)).expect("the screenshot is there");
        assert_eq!(found.source, FrameSource::Screenshot);
        assert_eq!(found.bytes, b"the whole frame");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The recorder's video is encoded *out of* the JPEGs it captured, so a frame pulled out of a segment
    /// is a re-derivation of a picture this program already had once. Paying ffmpeg again for the same row
    /// — every session, forever — is the thing this folder exists to stop.
    #[test]
    fn a_frame_pulled_out_of_a_video_is_read_from_the_folder_next_time() {
        let root = std::env::temp_dir().join(format!("windui-frame-snapshot-{}-{}", std::process::id(), crate::fixtures::next_scratch_id()));
        let _ = std::fs::remove_dir_all(&root);
        let env = env_for(&root);
        let card = card_at(None, None, None);
        keep_video_snapshot(&env.config, &card, b"the frame ffmpeg made");

        // Both doors are gone — no screenshot, no video — and the row still answers, from the copy.
        let found = frame(&env, &card).expect("the snapshot is the frame this window already paid for");
        assert_eq!(found.source, FrameSource::Video);
        assert_eq!(found.bytes, b"the frame ffmpeg made");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The asymmetry is on purpose, and only a test keeps it: the screenshot door is never copied, because
    /// reading it costs one `open()` and a copy would keep a picture alive past the retention rule that
    /// swept its original.
    #[test]
    fn a_screenshot_is_never_copied_into_the_snapshot_folder() {
        let root = std::env::temp_dir().join(format!("windui-frame-no-copy-{}-{}", std::process::id(), crate::fixtures::next_scratch_id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let picture = root.join("2026-09-21_10-00-08.jpg");
        std::fs::write(&picture, b"the whole frame").unwrap();
        let env = env_for(&root);

        assert!(frame(&env, &card_at(Some(picture.clone()), None, None)).is_some());
        std::fs::remove_file(&picture).unwrap();
        assert!(frame(&env, &card_at(Some(picture), None, None)).is_none(), "the sweep still means what it says");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_snapshot_name_carries_the_row_its_time_its_segment_and_the_second_it_was_cut_at() {
        let key = crate::model::RowKey::new("default_2026-09_wind.db", 12);
        let name = |rowid: i64, time: i64, segment: &str, offset: Option<i64>, source: FrameSource| {
            frame_snapshot_name(&crate::model::RowKey::new(key.file.clone(), rowid), time, segment, offset, source)
        };
        let one = name(12, 100, "2026-09-21_10-00-00.mp4", Some(416), FrameSource::Video);
        assert_eq!(one, name(12, 100, "2026-09-21_10-00-00.mp4", Some(416), FrameSource::Video), "a frame is its own key");

        assert_ne!(one, name(101, 100, "2026-09-21_10-00-00.mp4", Some(416), FrameSource::Video), "a moved rowid must miss, not lie");
        assert_ne!(one, name(12, 101, "2026-09-21_10-00-00.mp4", Some(416), FrameSource::Video), "a re-timed row too");
        assert_ne!(one, name(12, 100, "2026-09-21_10-00-00.mp4", Some(208), FrameSource::Video), "a moved seek must miss, not lie");
        assert_ne!(one, name(12, 100, "2026-09-21_11-00-00.mp4", Some(416), FrameSource::Video), "another segment's frame is another picture");
        assert_ne!(one, name(12, 100, "2026-09-21_10-00-00.mp4", None, FrameSource::Video), "an unplaced frame is not this one");
        assert_ne!(one, name(12, 100, "2026-09-21_10-00-00.mp4", Some(416), FrameSource::Screenshot));
    }

    /// The name is built from two strings the index holds, and an index is a file somebody can edit.
    #[test]
    fn a_month_file_name_cannot_walk_out_of_the_snapshot_folder() {
        for hostile in ["..\\..\\windows\\system32\\x.db", "../../etc/passwd", "a/b.db", "  "] {
            let name = frame_snapshot_name(&crate::model::RowKey::new(hostile, 1), 1, "ok.mp4", Some(1), FrameSource::Video);
            assert!(!name.contains('/') && !name.contains('\\') && !name.contains(".."), "{name} escapes");
        }
        for hostile in ["..\\..\\windows\\system32\\x.mp4", "../../etc/passwd", "C:/windows/x.mp4", ".."] {
            let name = frame_snapshot_name(&crate::model::RowKey::new("default_2026-09_wind.db", 1), 1, hostile, Some(1), FrameSource::Video);
            assert!(!name.contains('/') && !name.contains('\\') && !name.contains(".."), "{name} escapes through the segment name");
        }
    }

    #[test]
    fn the_prune_keeps_the_newest_and_never_the_frame_it_just_wrote() {
        let now = SystemTime::now();
        let older = |secs: u64| now - Duration::from_secs(secs);
        let file = |n: u64| PathBuf::from(format!("{n}.jpg"));

        assert!(snapshots_to_remove(vec![], 100).is_empty(), "nothing to prune");
        assert!(snapshots_to_remove(vec![(file(1), 40, older(0)), (file(2), 40, older(5))], 100).is_empty(), "under the ceiling");

        let removed = snapshots_to_remove(vec![(file(1), 40, older(0)), (file(2), 40, older(5)), (file(3), 40, older(10))], 100);
        assert_eq!(removed, vec![file(3)], "the oldest goes, and the newest stays");

        assert!(snapshots_to_remove(vec![(file(1), 400, older(0))], 100).is_empty(), "one frame over the ceiling is still the frame in hand");
        let removed = snapshots_to_remove(vec![(file(1), 400, older(0)), (file(2), 10, older(5))], 100);
        assert_eq!(removed, vec![file(2)], "and the one after it is what goes");
    }

    /// A row whose screenshot was swept and whose video is gone has no full frame. `None` is the honest
    /// answer, and the overlay's job is to say it — not to paint the stored preview larger.
    #[test]
    fn a_row_with_no_picture_and_no_video_produces_no_frame() {
        let root = std::env::temp_dir().join(format!("windui-frame-nothing-{}", std::process::id()));
        let env = env_for(&root);
        assert!(frame(&env, &card_at(None, None, None)).is_none());
        assert!(frame(&env, &card_at(None, Some(PathBuf::from("Z:/definitely-not-here/8.mp4")), Some(4))).is_none());
    }

    /// A month's lightbox is a `rowid`, a time and a preview — reading a hundred rows of OCR text to fill
    /// a grid is the cost that shape exists to avoid — so enlarging one of those tiles starts from a key
    /// and nothing else. Three things this pins: the key is enough to reach the frame behind the row; a
    /// month this install cannot read is an *error*, not the sentence "this row has no picture"; and so is
    /// a row the month no longer holds, because a library that moved looks exactly like footage that was
    /// swept, and only one of the two is the user's problem.
    #[test]
    fn a_key_alone_reaches_the_row_and_the_frame_behind_it() {
        let dir = std::env::temp_dir().join(format!("windui-keyed-frame-{}-{}", std::process::id(), crate::fixtures::next_scratch_id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = Store::open_month(&dir.join("userdata").join("db"), "default", 2026, 9).expect("month file");
        store
            .append(&[Record {
                videofile_name: "2026-09-21_10-00-00.mp4".into(),
                picturefile_name: "2026-09-21_10-05-30.jpg".into(),
                videofile_time: at("2026-09-21_10-05-30"),
                ocr_text: "quarterly revenue".into(),
                win_title: Some("Excel".into()),
                deep_linking: None,
                thumbnail: None,
            }])
            .expect("append");
        drop(store);
        let slice = dir.join("cache_screenshot").join("2026-09-21_10-00-00-VIDEO");
        std::fs::create_dir_all(&slice).unwrap();
        std::fs::write(slice.join("2026-09-21_10-05-30.jpg"), b"the whole frame").unwrap();

        let config = Config::load(&dir).expect("an absent config still loads, with defaults");
        let months = read::discover(&config.db_dir());
        let file = months[0].path.file_name().and_then(|n| n.to_str()).unwrap().to_string();
        let env = env_with(config, months);
        let key = RowKey::new(file, 1);

        let card = card_of_key(&env, &key).expect("the index holds the row the key names");
        assert_eq!(card.key, key, "and the card it builds answers for the same row");
        assert_eq!(card.body, "quarterly revenue");
        assert_eq!(card.title.as_deref(), Some("Excel"), "the words the viewer puts over the picture");
        assert_eq!(card.clock, "10:05:30");
        assert_eq!(card.offset, Some(330), "five and a half minutes into the segment it was recorded in");

        let (base64, source) = frame_of_key(&env, &key).expect("a frame").expect("the slice is on disk");
        assert_eq!(source, FrameSource::Screenshot.key());
        assert_eq!(
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &base64).unwrap(),
            b"the whole frame",
            "the bytes are the recorder's own JPEG, not a re-encode",
        );

        // A month the window cannot read is a different fact from a row with no picture left.
        let absent_month = frame_of_key(&env, &RowKey::new("default_2031-12_wind.db".to_string(), 1)).expect_err("not a month this install holds");
        assert!(absent_month.contains("is not one of the month files"), "{absent_month}");
        let moved_row = frame_of_key(&env, &RowKey::new(key.file.clone(), 999)).expect_err("the file has no row 999");
        assert!(moved_row.contains("no longer holds row 999"), "{moved_row}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The seek is only ever attempted against a file that exists, and a program that is not there is an
    /// error rather than a panic — this is the path a machine with no ffmpeg on it takes.
    #[test]
    fn a_video_seek_fails_as_a_message_not_as_a_crash() {
        let root = std::env::temp_dir().join(format!("windui-frame-seek-{}-{}", std::process::id(), crate::fixtures::next_scratch_id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let video = root.join("2026-09-21_10-00-00.mp4");

        let missing_file = frame_from_video(Path::new("ffmpeg"), &video, 3).expect_err("no file, no frame");
        assert!(missing_file.contains("is not there"), "{missing_file}");

        std::fs::write(&video, b"not an mp4 at all").unwrap();
        let absent_program = frame_from_video(Path::new("Z:/definitely-not-here/ffmpeg.exe"), &video, 3).expect_err("no program, no frame");
        assert!(absent_program.contains("could not start ffmpeg"), "{absent_program}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two requests for the frames of one segment must not share a scratch file. A click on a result
    /// card opens the detail drawer *and* the whole-window viewer, both want the same row's frame, and a
    /// shared name let the second ffmpeg run truncate the file the first was still reading. The loser
    /// came back empty, and the window told the user the footage had been swept — a temp-file collision
    /// reported as lost recordings.
    #[test]
    fn two_frame_requests_get_two_scratch_files() {
        let video = Path::new("2026-09-25_21-57-09-OCRED.mp4");
        let first = scratch_frame_path(video);
        let second = scratch_frame_path(video);
        assert_ne!(first, second, "one segment, two requests, two files");
        assert_eq!(first.parent(), second.parent(), "and both still land in the temp directory");
        let name = first.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("windui_frame_2026-09-25_21-57-09-OCRED-"), "{name}");
        assert!(name.ends_with(".jpg"), "{name}");
    }

    /// The strip's cell and the lightbox's tile label themselves with the wall clock the index stored.
    ///
    /// `videofile_time` is naive-local seconds, which is not an instant: read as one, it lands eight hours
    /// from where the recorder put it, and the strip and the month's grid then disagree with the result
    /// card for the same row — which is exactly what they did while the only time they carried was raw
    /// seconds and the front end was left to phrase it. The strings are asserted rather than the arithmetic
    /// because the arithmetic is what was wrong; the expected values are the stamps the rows were built at,
    /// on any machine in any zone.
    #[test]
    fn a_cell_and_a_tile_carry_the_clock_the_index_stored() {
        let (dir, config, months) = fixture(
            "clock-labels",
            &[
                ("2026-09-25_21-57-09.mp4", at("2026-09-25_21-57-19"), "quarterly revenue", "Excel"),
                ("2026-09-25_21-57-09.mp4", at("2026-09-25_22-03-52"), "budget draft", "Writer"),
            ],
        );
        let env = env_with(config, months);

        let day = load_day(&env, LocalParts { year: 2026, month: 9, day: 25, hour: 0, minute: 0, second: 0 }).expect("the day reads");
        assert_eq!(day.cards.iter().map(|c| c.clock.as_str()).collect::<Vec<_>>(), vec!["21:57:19", "22:03:52"], "the card was already right");
        // The strip samples, so it holds fewer cells than the day has rows; what every one of them must
        // agree on is the minute, because a strip cell and the card of the row it stands for are the same
        // picture shown two ways, and they used to be eight hours apart.
        let labelled: Vec<&StripCell> = day.strip.iter().filter(|cell| cell.key.is_some()).collect();
        assert!(!labelled.is_empty(), "the strip holds the day's rows");
        for cell in labelled {
            let card = day.cards.iter().find(|one| Some(&one.key) == cell.key.as_ref()).expect("the strip samples rows the day already holds");
            assert_eq!(cell.clock.as_deref(), Some(card.clock.as_str()), "a cell wears its own row's clock");
        }
        assert!(
            day.strip.iter().filter_map(|cell| cell.clock.as_deref()).all(|clock| day.cards.iter().any(|card| card.clock == clock)),
            "no cell is labelled with a minute the day's own cards do not also show",
        );

        let tiles = lightbox(&env, 2026, 9).expect("the month reads");
        assert!(!tiles.is_empty(), "the month holds the two rows");
        for tile in &tiles {
            let expected = day.cards.iter().find(|card| card.key.rowid == tile.key.rowid).map(|card| format!("{} {}", card.day, card.clock)).unwrap();
            assert_eq!(tile.stamp, expected, "the tile's caption is the row's own stamp, not an instant");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Where a frame came from is two different claims about the pixels, so the two doors have to name
    /// themselves distinctly — and each one has a catalog row, because this text is shown to a user.
    #[test]
    fn each_frame_source_names_itself_in_the_catalog() {
        for source in [FrameSource::Screenshot, FrameSource::Video] {
            assert_ne!(source.key(), source.label(), "{source:?}");
            assert!(source.key().starts_with("windui_frame_"), "{source:?}");
        }
        assert_ne!(FrameSource::Screenshot.key(), FrameSource::Video.key());
    }
}
