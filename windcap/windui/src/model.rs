//! Everything the UI is a function of, with no `egui` type anywhere in it.
//!
//! This split is the reason the render tests can exist at all: the state machine, the paging
//! arithmetic, the request-id race and the day-boundary rule are all exercised without a window,
//! and `view` is then free to be a dumb projection of it. `AppState` holds only plain data —
//! `String`, `i64`, `PathBuf`, `Vec` — because a thumbnail that arrives as an `egui::TextureHandle`
//! could not be asserted on, and because the worker threads have to be able to build one. The one
//! exception is [`AppState::player_run`]: a stop flag is a handle to a process, not a picture, and it
//! is assertable all the same (`load()` answers a test as well as a thread does).
//!
//! Two rules are load-bearing and live here rather than in the view:
//!
//!   * **monotonic request ids.** A search superseded by a newer one must have its reply dropped,
//!     or the user sees page 1 flash in while they are already looking at page 3. `next_request`
//!     is the only source of ids and `apply` is the only place they are compared.
//!   * **the day boundary is data, not decoration.** `day_begin_minutes` decides which calendar day
//!     a row belongs to; a UI that renders 01:00 under "today" shows the user the wrong day.

use serde::{Deserialize, Serialize};

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use wind_base::clock::{self, LocalParts};
use wind_base::i18n::Catalog;
use wind_store::read::Month;

use crate::ai::{AiDraft, AiSettings, AiVerdict, BridgeStatus, KeyState};
use crate::flags::FlagNote;
use crate::record::{DisplayInfo, Rec, RecDraft, RecOptions};
use crate::settings::{Draft, Options, Settings};
use crate::wordcloud::{CloudWord, PlacedWord};

/// How many decodes one visible set may queue at a time. Beyond this, a fast page-flip would leave
/// work in the queue long after the cards it belongs to scrolled away.
pub const PREFETCH_LIMIT: usize = 200;

/// The month lightbox's own geometry, kept from upstream: 25 tiles across, 35 down. It is the
/// definition of the picture, not a layout accident — a month's footage tiled any other way is a
/// different artefact than the one the Python app made.
pub const LIGHTBOX_COLUMNS: usize = 25;
pub const LIGHTBOX_ROWS: usize = 35;
pub const LIGHTBOX_SLOTS: usize = LIGHTBOX_COLUMNS * LIGHTBOX_ROWS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Search,
    OneDay,
    Stat,
    Recording,
    Settings,
    /// Upstream's "Lab" (`windrecorder/ui/lab.py`, routed at `webui.py:85-86`), and the only surface
    /// in the product for the keys `windai` reads. It is a tab rather than a section of Settings
    /// because Settings is documented — in its heading, its subtitle, `Field`'s own comment and
    /// `settings.rs`'s module header — as the keys *these two screens* read, and no screen here reads
    /// an API key. `ai`'s module header carries the full argument.
    Ai,
}

impl Tab {
    pub const ALL: [Tab; 6] = [Tab::Search, Tab::OneDay, Tab::Stat, Tab::Recording, Tab::Settings, Tab::Ai];

    /// The neutral English name, used as the fallback a non-painting caller reads and — load-bearing
    /// for the localisation — as the value the shipped `en` catalog copy is pinned against in
    /// `render_tests` (`the_tab_labels_resolve_to_their_english_copy`). The tab bar paints through
    /// `tr(i18n_key())` instead, so this is dead in the binary but asserted in tests.
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Tab::Search => "Search",
            Tab::OneDay => "OneDay",
            Tab::Stat => "Stat",
            Tab::Recording => "Recording",
            Tab::Settings => "Settings",
            Tab::Ai => "AI",
        }
    }

    /// The `languages.json` key behind this tab's painted label. `label()` stays as the neutral
    /// English fallback a non-painting caller (and the `Tab::Ai.label() == "AI"` pin) can use, while
    /// the tab bar itself resolves through the catalog so the same six words become Chinese or
    /// Japanese for a `sc`/`ja` install. Each key's `en` entry is byte-identical to `label()` above,
    /// which is what keeps the render tests — they run in `en` — asserting the strings they already do.
    pub fn i18n_key(self) -> &'static str {
        match self {
            Tab::Search => "windui_tab_search",
            Tab::OneDay => "windui_tab_oneday",
            Tab::Stat => "windui_tab_stat",
            Tab::Recording => "windui_tab_recording",
            Tab::Settings => "windui_tab_settings",
            Tab::Ai => "windui_tab_ai",
        }
    }
}

/// The `config_src/languages.json` beside this source tree, in the layout the repository ships: two
/// levels up from `windui` is the install root that carries `config_src`. This is only the fallback
/// the headless render tests and any pre-`App::new` paint read; the running window overwrites it with
/// the catalog of the root it actually opened.
fn shipped_catalog_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(std::path::Path::parent).map(std::path::Path::to_path_buf).unwrap_or_else(|| std::path::PathBuf::from("."))
}

impl AppState {
    /// Resolve a UI string through the window's catalog, in the installed locale.
    pub fn tr(&self, key: &str) -> String {
        self.catalog.text(key)
    }

    /// Resolve a UI string and fill its `{field}` placeholders, using the catalog's own interpolation.
    pub fn trf(&self, key: &str, args: &[(&str, &str)]) -> String {
        self.catalog.formatted(key, args)
    }

    /// Resolve a UI string, falling back to the words the binary itself carries when the catalog has no
    /// row. A control with no label is worse than a control labelled in English.
    pub fn tr_or(&self, key: &str, fallback: &str) -> String {
        self.catalog.text_or(key, fallback)
    }

    /// Open the full-frame overlay on a card, before its picture has been read.
    ///
    /// Returns whether the caller should dispatch the read. Re-clicking the row that is already on screen
    /// re-asks only when the last attempt gave up, so a row whose video was momentarily locked by
    /// maintenance can be retried without moving the mouse elsewhere first.
    pub fn open_frame(&mut self, card: &RowCard) -> bool {
        if let Some(view) = self.frame.as_mut() {
            if view.key == card.key {
                if view.loading {
                    return false;
                }
                view.loading = true;
                view.missing = false;
                // The retry re-reads the two doors too. A row whose segment was away while
                // maintenance renamed it had no `segment_path` on the first click, and a viewer that
                // kept answering from the stale copy would offer no play control for footage that
                // arrived a second later — which is the same "a stated answer is retryable" promise
                // the re-ask below is built on.
                view.segment_path = card.segment_path.clone();
                view.start = card.offset.unwrap_or(0).max(0);
                return true;
            }
        }
        self.frame = Some(FrameView {
            key: card.key.clone(),
            clock: card.clock.clone(),
            day: card.day.clone(),
            segment: card.segment.clone(),
            // The two the player needs, carried from the card rather than re-derived: the file the
            // index resolved at query time, and the second inside it this row was indexed from, which
            // is where "play this" has to start. Clamped because `ffmpeg -ss` takes no negative
            // argument, and a row whose capture predates the segment it was indexed into is a reason to
            // start at the beginning, not a reason to refuse the request.
            segment_path: card.segment_path.clone(),
            start: card.offset.unwrap_or(0).max(0),
            loading: true,
            source: None,
            missing: false,
            actual_size: false,
        });
        true
    }

    /// Close the overlay. The texture itself goes when the cache next evicts it, which on a three-entry
    /// cache is two other pictures later.
    pub fn close_frame(&mut self) {
        self.frame = None;
        // The player is part of what was just closed, and the only thing that can end its ffmpeg is the
        // stop flag this parks a `Stop` for. `player_run` deliberately survives the line below: the
        // kill is delivered by the command the painter drains from that request a few statements later,
        // and a flag cleared here would be a flag nobody can raise — leaving a segment streaming behind
        // a window the user has already shut, which is the difference between a player and a leak.
        if self.player_run.is_some() {
            self.pending_player = Some(PlayerRequest::Stop);
        }
        self.player = None;
    }

    /// Turn the parked player request into the one command the frame has to dispatch.
    ///
    /// Out of `paint` and in here because this is the state machine deciding what "play from second N"
    /// means — which row, which file, and which stop flag will own the answer — and a rule the view
    /// holds inline is a rule no window-free test can reach. The run's flag is minted here, once per
    /// command, precisely so that the replies it produces can later be recognised as this stream's or
    /// as somebody superseded's; see [`AppEvent::PlayerFrame`].
    pub fn take_player_request(&mut self) -> Option<Command> {
        match self.pending_player.take()? {
            PlayerRequest::Start(from) => {
                let view = self.frame.as_ref()?;
                let segment = view.segment_path.clone()?;
                Some(Command::PlaySegment { key: view.key.clone(), segment, from, run: Arc::new(AtomicBool::new(false)) })
            }
            // Nothing to stop is not a command: `close_frame` parks the request whenever a run is live,
            // and by the time the frame drains it the worker may already have finished on its own.
            PlayerRequest::Stop => Some(Command::StopSegment { run: self.player_run.clone()? }),
        }
    }

    /// Install the catalog for the root the app actually opened and the user's `lang`, so the window
    /// translates itself with the same file the tray reads. Called once from `App::new`.
    pub fn install_catalog(&mut self, root: &std::path::Path, lang: &str) {
        self.catalog = Catalog::load(root, lang);
    }
}

const fn date(year: i64, month: u32, day: u32) -> LocalParts {
    LocalParts {
        year,
        month,
        day,
        hour: 0,
        minute: 0,
        second: 0,
    }
}

/// `rowid` is only unique inside one month file, so a row's identity is the pair. Without the file
/// name, two months' row 12 would share a thumbnail texture and show each other's screen.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RowKey {
    pub file: String,
    pub rowid: i64,
}

impl RowKey {
    pub fn new(file: impl Into<String>, rowid: i64) -> RowKey {
        RowKey { file: file.into(), rowid }
    }

    pub fn texture_id(&self) -> String {
        format!("{}:{}", self.file, self.rowid)
    }
}

/// One result, projected down to what the two screens render.
///
/// `picturefile_name`, `is_picturefile_exist` and `is_videofile_exist` are deliberately absent: the
/// WebUI carried them into its dataframe, showed a checkbox nobody clicked, and then recomputed
/// on-disk presence from a directory listing anyway (`db_refine_search_data_global`). What survives
/// here is that recomputed answer — a resolved path — because it is the only form the Locate
/// action can use.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RowCard {
    pub key: RowKey,
    pub time: i64,
    /// `HH:MM:SS`, what the card header shows.
    pub clock: String,
    /// `YYYY-MM-DD`, shown when a result set spans more than one day.
    pub day: String,
    pub title: Option<String>,
    pub body: String,
    /// The segment this frame was indexed from, exactly as stored.
    pub segment: String,
    /// Seconds into that segment — the value a player seeks to.
    pub offset: Option<i64>,
    pub deep_link: Option<String>,
    /// Base64 JPEG as stored. Decoded only on a worker; see `thumbs`.
    pub thumbnail: Option<String>,
    /// The segment's path on disk, if it is still there.
    pub segment_path: Option<PathBuf>,
    /// The screenshot this row was indexed from, if that JPEG is still on disk: the product's only
    /// full-resolution picture, which is what a click on a thumbnail has to open.
    ///
    /// Resolved exactly like [`RowCard::segment_path`] — the index's stored existence flag is only the gate
    /// that says "do not bother looking", and the path here is the answer to having looked.
    pub picture_path: Option<PathBuf>,
}

impl RowCard {
    /// Whether `deep_linking` holds a URL rather than some other string the recorder was told to
    /// stash there. The WebUI's test was `"http" in url.lower()`; this is that test, kept.
    pub fn deep_link_is_url(&self) -> bool {
        self.deep_link.as_deref().is_some_and(|v| v.to_lowercase().contains("http"))
    }
}

/// The card grid's contents, plus the paging and selection that give it meaning.
///
/// `camelCase` on the wire because this struct is the HTML window's request body as well as the egui
/// window's in-memory one, and TypeScript spells a two-word field `pageSize`. The Rust fields keep
/// their snake_case; only the JSON form is renamed, so nothing on the egui side notices.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchParams {
    pub keywords: String,
    pub exclude: String,
    pub from: LocalParts,
    pub to: LocalParts,
    pub page: usize,
    pub page_size: usize,
}

impl Default for SearchParams {
    fn default() -> SearchParams {
        SearchParams {
            keywords: String::new(),
            exclude: String::new(),
            from: date(1970, 1, 1),
            to: date(2038, 1, 1),
            page: 1,
            page_size: 20,
        }
    }
}

impl SearchParams {
    /// The epoch range the query will run with.
    ///
    /// The endpoints are pushed out to the day poles, because that is what the WebUI has always
    /// done (`get_datetime_in_day_range_pole_by_config_day_begin(in, "start")` / `(out, "end")`):
    /// a user who picks 2026-09-21 .. 2026-09-21 means the whole product-day, which with
    /// `day_begin_minutes = 180` runs to 02:59:59 on the 22nd.
    /// The whitespace-separated terms, which is how `Query::with_keywords` splits them.
    pub fn tokens(&self) -> Vec<String> {
        self.keywords.split_whitespace().map(str::to_string).collect()
    }

    pub fn range(&self, day_begin_minutes: i64) -> (i64, i64) {
        let (from, _) = clock::day_bounds(self.from.year, self.from.month, self.from.day, day_begin_minutes);
        let (_, to) = clock::day_bounds(self.to.year, self.to.month, self.to.day, day_begin_minutes);
        (from, to.max(from))
    }
}

#[derive(Debug, Clone)]
pub struct SearchState {
    pub params: SearchParams,
    /// Set when a request is issued, cleared when the matching reply lands. Drives the "searching"
    /// marker; never inferred from elapsed time.
    pub pending: bool,
    pub request_id: u64,
    pub cards: Vec<RowCard>,
    pub total: i64,
    pub pages: usize,
    pub elapsed_ms: u128,
    pub error: Option<String>,
    /// The terms the cards are highlighted for, as the backend expanded them.
    pub terms: Vec<String>,
    pub selected: Option<usize>,
    /// The parameters `cards` actually answers. The status line quotes this, not the live inputs,
    /// so the text beside a result set can never describe a different query than the one painted.
    pub answered: Option<Box<SearchParams>>,
    pub ran: bool,
}

impl Default for SearchState {
    fn default() -> SearchState {
        SearchState {
            params: SearchParams::default(),
            pending: false,
            request_id: 0,
            cards: Vec::new(),
            total: 0,
            pages: 0,
            elapsed_ms: 0,
            error: None,
            terms: Vec::new(),
            selected: None,
            answered: None,
            ran: false,
        }
    }
}

impl SearchState {
    pub fn status(&self) -> String {
        match &self.answered {
            None if !self.ran => String::new(),
            _ => format!(
                "{} of {} results · page {}/{} · {} ms",
                self.cards.len(),
                self.total,
                self.answered.as_ref().map_or(1, |a| a.page),
                self.pages.max(1),
                self.elapsed_ms
            ),
        }
    }
}

/// One vertical of the activity area chart. `label` is pre-rendered because the painter must not
/// re-derive a date from an epoch inside the frame loop.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BucketCell {
    pub start: i64,
    pub count: usize,
    pub label: String,
}

/// One thumbnail slot of the timeline strip, positioned by the time it stands for.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StripCell {
    pub from: i64,
    pub to: i64,
    pub time: Option<i64>,
    pub key: Option<RowKey>,
    pub thumbnail: Option<String>,
    /// `HH:MM:SS` of the row this cell stands for, formatted where it was read.
    ///
    /// Pre-rendered for the reason `BucketCell::label` is, and a sharper one: `videofile_time` holds
    /// naive-local seconds, so a front end that formats them itself reads them as a UTC instant *and*
    /// adds the machine's offset on top — which is how a picture taken at 21:57 got a tooltip saying
    /// 05:57 the next morning, disagreeing with the very same row's card in the next tab over.
    pub clock: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DayState {
    pub date: LocalParts,
    pub pending: bool,
    pub request_id: u64,
    /// Inclusive `[start, end]` of the product-day, after `day_begin_minutes`.
    pub bounds: (i64, i64),
    /// The day's whole row set, fetched once per date. The in-day filter is a view over this, so a
    /// keystroke never reaches the store.
    pub all: Vec<RowCard>,
    pub filter: String,
    pub buckets: Vec<BucketCell>,
    pub strip: Vec<StripCell>,
    /// `[from, to]` the strip's pixels are stretched over; equals `bounds` when there is no data.
    pub strip_span: (i64, i64),
    pub active_hours: f64,
    pub titles: Vec<(String, i64)>,
    pub flags: Vec<FlagNote>,
    /// The note being typed for a flagged row, keyed by its position in the file. Held apart from
    /// `flags` so a half-finished edit is not clobbered by a redraw of the loaded value, and cleared
    /// on a day reload so a draft never lands on a row whose index has moved.
    pub flag_drafts: HashMap<usize, String>,
    /// The row whose 🗑 was clicked once and is awaiting the second, confirming click. `None` until
    /// the user asks to delete, so no delete is ever one click deep.
    pub flag_confirm: Option<usize>,
    /// Captured but not indexed — the message the user needs is different from "nothing happened".
    pub unindexed_video: bool,
    pub scrub: i64,
    pub selected: Option<usize>,
    pub error: Option<String>,
    pub loaded: bool,
}

impl Default for DayState {
    fn default() -> DayState {
        let now = date(1970, 1, 1);
        DayState {
            date: now,
            pending: false,
            request_id: 0,
            bounds: (0, 0),
            all: Vec::new(),
            filter: String::new(),
            buckets: Vec::new(),
            strip: Vec::new(),
            strip_span: (0, 0),
            active_hours: 0.0,
            titles: Vec::new(),
            flags: Vec::new(),
            flag_drafts: HashMap::new(),
            flag_confirm: None,
            unindexed_video: false,
            scrub: 0,
            selected: None,
            error: None,
            loaded: false,
        }
    }
}

impl DayState {
    /// The cards the grid shows: the day's rows narrowed by the in-day filter.
    pub fn visible(&self) -> Vec<usize> {
        let needle = self.filter.trim().to_lowercase();
        self.all
            .iter()
            .enumerate()
            .filter(|(_, card)| {
                needle.is_empty()
                    || card.body.to_lowercase().contains(&needle)
                    || card.title.as_deref().map(str::to_lowercase).is_some_and(|t| t.contains(&needle))
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Where a click on the strip landed: the sample whose time span covers `time`.
    ///
    /// This is the reverse half of the strip's contract — position along it means position in the
    /// day — and it goes through the spans rather than the pixel index so that a strip with empty
    /// slots (a day with two hours of activity) still maps a midday click to nothing.
    pub fn cell_for_time(&self, time: i64) -> Option<usize> {
        self.strip.iter().position(|c| time >= c.from && time <= c.to)
    }

    /// The row in effect at a moment: the newest one at or before it. Looking backwards is what the
    /// recorder's own "rewind" does, because the frame describing the screen at 10:03:20 was
    /// captured at or before 10:03:20, never after.
    pub fn row_for_time(&self, visible: &[usize], time: i64) -> Option<usize> {
        let mut best: Option<usize> = None;
        for &i in visible {
            if self.all[i].time <= time {
                best = Some(i);
            }
        }
        best.or_else(|| visible.first().copied())
    }
}

/// The bookkeeping one asynchronous panel needs, shared by all four of the Stat tab's queries.
///
/// Each panel has its own `Track` rather than the tab having one, because the four answers land at
/// different times and for different reasons: changing the month must not invalidate a lightbox the
/// user is still looking at, and a lightbox that is still building must not make the scatter's reply
/// look stale.
#[derive(Debug, Clone, Default)]
pub struct Track {
    pub pending: bool,
    pub request_id: u64,
    /// The panel has answered at least once, which is the difference between "loading" and "empty".
    pub loaded: bool,
    pub error: Option<String>,
}

impl Track {
    fn issue(&mut self, id: u64) {
        self.pending = true;
        self.error = None;
        self.request_id = id;
    }

    /// Fold in a reply. `false` when it answers a request that has since been replaced.
    fn accepts(&mut self, id: u64) -> bool {
        if id < self.request_id {
            return false;
        }
        self.pending = false;
        self.loaded = true;
        true
    }
}

/// One dot of the month scatter: a product-day, how many rows it holds and how many hours it spans.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayPoint {
    pub day: u32,
    pub rows: usize,
    pub hours: f64,
}

/// One dot of the year scatter, which is the same statistic seen from a month away.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonthDayPoint {
    pub month: u32,
    pub day: u32,
    pub rows: usize,
}

/// One tile of the lightbox: a row's identity, so its texture can be found in the shared cache, and
/// the time it stands for, which is the caption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LightboxTile {
    pub key: RowKey,
    pub time: i64,
    pub thumbnail: Option<String>,
    /// `YYYY-MM-DD HH:MM:SS`, the same wall clock `time` holds in raw seconds, for the same reason
    /// [`StripCell::clock`] carries one: the front end that formats naive-local seconds as an instant
    /// puts this month's pictures eight hours away from where their own cards say they are.
    pub stamp: String,
}

/// Everything the Stat tab is a function of.
#[derive(Debug, Clone, Default)]
pub struct StatState {
    pub year: i64,
    pub month: u32,
    pub month_track: Track,
    pub days: Vec<DayPoint>,
    pub month_rows: i64,
    pub year_track: Track,
    pub year_points: Vec<MonthDayPoint>,
    pub year_rows: i64,
    pub tiles_track: Track,
    pub tiles: Vec<LightboxTile>,
    pub cloud_track: Track,
    pub words: Vec<CloudWord>,
    /// The laid-out cloud, and the request whose words it was arranged from. Cached because the
    /// spiral is the one piece of work here that is not a memcpy and it must not run every frame.
    pub placed: Vec<PlacedWord>,
    pub placed_for: u64,
    /// Baked into upstream's saved PNG; here it is a paint-time overlay, so this is a view option.
    pub watermark: bool,
}

impl StatState {
    /// The lightbox's own span, taken from the tiles rather than from the calendar month: upstream
    /// passes the month's poles in and prints them, which means a month whose first capture is on the
    /// 20th is still captioned "2026.09.01". The tiles are the truth about what is in the picture.
    pub fn tile_span(&self) -> Option<(i64, i64)> {
        let first = self.tiles.iter().map(|t| t.time).min()?;
        let last = self.tiles.iter().map(|t| t.time).max()?;
        Some((first, last))
    }

    /// `2026.09.01 — 2026.09.30 · 29 d`: the two dates and the gap upstream's band draws between them.
    pub fn lightbox_caption(&self) -> String {
        let Some((from, to)) = self.tile_span() else {
            return format!("{:04}-{:02} · no tiles", self.year, self.month);
        };
        let stamp = |t: i64| {
            let p = LocalParts::from_naive_epoch(t);
            format!("{:04}.{:02}.{:02}", p.year, p.month, p.day)
        };
        let days = (to - from).div_euclid(86_400);
        format!("{} — {} · {days} d", stamp(from), stamp(to))
    }
}

/// The persistent footer. Built by one pass over the month files at startup and on demand.
#[derive(Debug, Clone, Default)]
pub struct Footer {
    pub months_total: usize,
    pub months_scanned: usize,
    pub rows: i64,
    pub first: Option<i64>,
    pub last: Option<i64>,
    pub scanning: bool,
    pub error: Option<String>,
}

impl Footer {
    pub fn line(&self) -> String {
        if self.months_total == 0 {
            return "no index files yet".to_string();
        }
        let progress = if self.scanning {
            format!("{}/{} month files", self.months_scanned, self.months_total)
        } else {
            format!("{} month files", self.months_total)
        };
        let last = match self.last {
            Some(t) => LocalParts::from_naive_epoch(t).display(),
            None => "never".to_string(),
        };
        format!("{progress} · {} rows indexed · last record {last}", self.rows)
    }
}

/// The bookkeeping behind "test the connection".
///
/// Its own struct rather than two fields on `AppState` because the whole point of the id is that a
/// reply to a probe the user has since replaced must not reach the screen, and that rule is already
/// written once — in `Track`'s `issue`/`accepts`, which `model`'s own tests cover.
#[derive(Debug, Clone, Default)]
pub struct AiTest {
    pub track: Track,
    /// `Ok`'s payload is the report line, `Err`'s the failure line. Both are already redacted by
    /// `crate::ai`, so painting this cannot be the moment a token escapes; `None` until a probe has
    /// answered since the window opened.
    pub report: Option<Result<String, String>>,
}

/// One prompt template as the AI page holds it: the text in the box, and what was on disk when the
/// panel was loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptDraft {
    pub name: crate::ai::PromptName,
    /// The file name the user sees, which is also the name `windai prompts` takes.
    pub label: &'static str,
    /// What is in the box. Seeded from the effective text, then from the user's typing.
    pub text: String,
    /// `user` or `shipped`, taken from the loader rather than inferred here.
    pub origin: &'static str,
    /// Where the effective text came from.
    pub path: String,
    /// Whether the shipped words differ, which is the honest reason "Restore" is offered.
    pub overridden: bool,
    /// True when the box no longer matches the file. Only a save clears it.
    pub dirty: bool,
    /// Whether "try it" can run this template: only the four summary ones carry screen material.
    pub testable: bool,
}

impl PromptDraft {
    pub fn load(row: &crate::ai::PromptRow) -> PromptDraft {
        use crate::ai::PromptName;
        PromptDraft {
            testable: matches!(
                row.name,
                PromptName::PeriodSystem | PromptName::PeriodUser | PromptName::DailySystem | PromptName::DailyUser
            ),
            dirty: false,
            name: row.name,
            label: row.name.label(),
            text: row.text.clone(),
            origin: if row.overridden { "user" } else { "shipped" },
            path: row.path.clone(),
            overridden: row.overridden,
        }
    }
}

/// The prompt panel's whole state.
///
/// Its own struct for the reason `AiTest` has one: the rule that a reply to a trial the user has since
/// replaced must not reach the screen is written once, in `Track`, and two loose fields on `AppState` is
/// how that rule gets reimplemented slightly differently the second time.
#[derive(Debug, Clone, Default)]
pub struct PromptPanel {
    pub rows: Vec<PromptDraft>,
    /// The one line Save or Restore produced, verbatim from the writer that made it.
    pub status: String,
    pub trial: Track,
    pub report: Option<crate::ai::PromptTrial>,
    /// Which template the in-flight trial belongs to, so the reply is painted on the row that asked.
    pub trial_for: Option<crate::ai::PromptName>,
}

/// The two lines the AI page paints about the configuration it is holding, both computed by asking
/// `windai` rather than by deciding here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiStatus {
    pub verdict: AiVerdict,
    pub key: KeyState,
}

impl Default for AiStatus {
    fn default() -> AiStatus {
        AiStatus {
            verdict: AiVerdict { ok: false, message: "not checked yet".to_string() },
            key: KeyState::Absent,
        }
    }
}

/// The bridge's answer about the install, refreshed when the page is opened and after a save — not on
/// every keystroke, because part of it is a TCP connect and the other part is read from the file the
/// service itself will read when the tray starts it.
impl Default for BridgeStatus {
    fn default() -> BridgeStatus {
        BridgeStatus {
            enabled: false,
            listening: false,
            host: String::new(),
            port: 0,
            url: String::new(),
            auth_required: true,
            token_chars: 0,
            refused: None,
        }
    }
}

/// What a worker produced. Every variant carries the id of the request it answers.
#[derive(Debug, Clone)]
pub enum AppEvent {
    Search {
        request_id: u64,
        outcome: Result<SearchOutcome, String>,
    },
    Day {
        request_id: u64,
        date: LocalParts,
        outcome: Result<DayOutcome, String>,
    },
    /// Sent once per month file while the scan runs, so a five-year library fills the footer
    /// progressively instead of going quiet for six seconds.
    Library(LibraryStats),
    SettingsSaved(Result<PathBuf, String>),
    /// The same two-state answer for the recorder's section. A separate variant because the reply
    /// promotes a different struct, and because one Save must not look like it also applied the
    /// other form's draft.
    RecordingSaved(Result<PathBuf, String>),
    /// The AI page's own write, kept a variant apart for the reason `RecordingSaved` is one: the
    /// reply promotes a different struct, and one Save must not look like it also applied another
    /// form's draft.
    AiSaved(Result<PathBuf, String>),
    /// What one "test the connection" round trip produced. `Err` carries the failure *text*, already
    /// scrubbed of the bearer token by `wind_ai::error`, so the frame can paint it without deciding
    /// anything about safety; there is no `AiError` in this type because `wind-ai`'s error is a
    /// different crate's and `AppState` has to stay plain data.
    AiTested {
        request_id: u64,
        outcome: Result<String, String>,
    },
    /// One template tried against a real stretch. The report is already redacted by `crate::ai`: the
    /// reply is model text, and model text is not trusted here.
    PromptTried {
        request_id: u64,
        trial: crate::ai::PromptTrial,
    },
    Thumbnail {
        key: RowKey,
        image: Option<DecodedImage>,
    },
    /// The full frame a click asked for. `image` is `None` when this install could not produce one — no
    /// surviving screenshot and no readable video — which the viewer says rather than showing a stretched
    /// thumbnail and calling it the original.
    Frame {
        key: RowKey,
        source: Option<crate::backend::FrameSource>,
        image: Option<DecodedImage>,
    },
    /// One second of a segment, as the player's ffmpeg produced it.
    ///
    /// `run` is the stop flag of the stream that made it, and it is the staleness check: a seek mints a
    /// new run while the old stream may still have a frame in flight, and a worker cannot be recalled
    /// once it has decided to reply. Without the pointer comparison the late frame from the superseded
    /// run would be painted over the position the user just asked for — one second of footage from
    /// before the seek, under a caption that says it is from after it. [`AppState::player_run`] is what
    /// it is compared against, and a reply whose `run` is not that pointer is dropped without a word.
    PlayerFrame {
        run: Arc<AtomicBool>,
        key: RowKey,
        at: i64,
        image: DecodedImage,
    },
    /// The stream that has been sending [`Self::PlayerFrame`]s has ended, or never started.
    ///
    /// Carries the same `run` for the same reason: only the live stream gets to say what the player
    /// looks like now. A probe that could not read the file answers with `duration: None` and a
    /// `failure` sentence, which the viewer paints rather than a rectangle.
    PlayerDone {
        run: Arc<AtomicBool>,
        key: RowKey,
        duration: Option<i64>,
        failure: Option<String>,
    },
    /// The selected month's per-day totals.
    StatMonth {
        request_id: u64,
        outcome: Result<MonthTotals, String>,
    },
    /// The selected year's per-(month, day) totals.
    StatYear {
        request_id: u64,
        outcome: Result<YearTotals, String>,
    },
    Lightbox {
        request_id: u64,
        outcome: Result<Vec<LightboxTile>, String>,
    },
    Cloud {
        request_id: u64,
        outcome: Result<Vec<CloudWord>, String>,
    },
    /// What the desktop reported when it was enumerated. Not a query the user replaced, so it has no
    /// id: two probes racing is a display list from one of them, which is true either way.
    Displays(Result<Vec<DisplayInfo>, String>),
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MonthTotals {
    pub points: Vec<DayPoint>,
    pub rows: i64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct YearTotals {
    pub points: Vec<MonthDayPoint>,
    pub rows: i64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchOutcome {
    pub cards: Vec<RowCard>,
    pub total: i64,
    pub pages: usize,
    pub elapsed_ms: u128,
    pub params: Box<SearchParams>,
    /// The terms the highlighter will mark, already expanded through the similar-glyph table.
    /// Computed where the query was built, because a variant that matched a row is only known to
    /// the code that generated the variants.
    pub terms: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayOutcome {
    pub cards: Vec<RowCard>,
    pub bounds: (i64, i64),
    pub buckets: Vec<BucketCell>,
    pub strip: Vec<StripCell>,
    /// `[from, to]` the strip's pixels are stretched over; equals `bounds` when there is no data.
    pub strip_span: (i64, i64),
    pub active_hours: f64,
    pub titles: Vec<(String, i64)>,
    pub flags: Vec<FlagNote>,
    pub unindexed_video: bool,
    /// Non-fatal: a month file that refused to open is named here while the rest of the day still
    /// renders. `search_months` has no equivalent, which is why the day path reads the months
    /// itself.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LibraryStats {
    /// Every month file the database directory held when this pass looked at it. The scan carries its
    /// own list because a refresh has to be able to reveal a file that did not exist when the window
    /// opened, and `view` holds both screens on the onboarding hint until one does.
    pub months: Vec<Month>,
    pub months_total: usize,
    pub months_scanned: usize,
    pub rows: i64,
    pub first: Option<i64>,
    pub last: Option<i64>,
    pub done: bool,
    pub error: Option<String>,
}

/// A decoded thumbnail as raw pixels. Deliberately not an `egui::ColorImage`: the worker that
/// decodes has no `Context`, and the bytes are what the test asserts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ThumbnailJob {
    pub key: RowKey,
    pub base64: String,
}

/// The full-frame viewer's whole state: one picture, because that is what a click opens.
///
/// The pixels are not here — they are a texture the `App` uploads the frame the decode lands — so this
/// stays plain data and the viewer remains paintable by a headless render test.
#[derive(Debug, Clone)]
pub struct FrameView {
    pub key: RowKey,
    /// What the header says it is showing, taken from the card rather than re-read from the index.
    pub clock: String,
    pub day: String,
    pub segment: String,
    /// The segment this row's moving picture lives in, if the file is still on disk. The player's only
    /// door: with `None` there is nothing to stream, and the viewer says so instead of offering a
    /// button that cannot answer.
    pub segment_path: Option<PathBuf>,
    /// The second inside [`FrameView::segment`] this row was indexed from, so "play this" opens on the
    /// moment the user clicked rather than on the start of the segment. Never negative — the clamp and
    /// the reason for it are in [`AppState::open_frame`] — and zero both when the footage starts here and
    /// when the index holds no offset at all.
    pub start: i64,
    /// Set between the click and the reply. A click that produced nothing yet must not look like a click
    /// that produced nothing.
    pub loading: bool,
    pub source: Option<crate::backend::FrameSource>,
    /// The install could not produce a full frame for this row. Distinct from `loading`, and stated.
    pub missing: bool,
    /// Paint one texture pixel per screen pixel instead of fitting the window, which is how you actually
    /// read small text out of a 1080p grab.
    pub actual_size: bool,
}

/// What the transport row paints: the segment that is moving, and where it has got to.
///
/// Plain and cloneable like the rest of the state, because the viewer is painted from a copy of it and a
/// reply mutates the original. `None` in [`AppState::player`] is the still; `Some` is the player, and
/// there is no separate "playing" flag to keep in step with the two.
#[derive(Debug, Clone)]
pub struct Player {
    /// The row this was started from. A reply for another row is dropped by `apply`, and so is the
    /// picture: walking with the arrows while a segment runs must not put the previous row's frames
    /// under the new row's caption.
    pub key: RowKey,
    /// The segment's file name, for the caption. The path itself stays in [`FrameView`]; what the user
    /// is shown is the name, which is the part that identifies the hour.
    pub name: String,
    /// The second of the segment the painted frame belongs to.
    pub at: i64,
    /// The segment's length, as soon as the probe has answered. `None` is not "zero seconds" but "the
    /// scrub bar has no ends yet", and the bar refuses to exist until it does.
    pub duration: Option<i64>,
    /// Started, but nothing has arrived yet — the state between the click and the first frame, which
    /// like `FrameView::loading` must be said rather than painted as a black box.
    pub waiting: bool,
    /// A sentence explaining why this is not moving, in the same shape as [`FrameView::missing`]'s: the
    /// machine that cannot decode the file and the file that was deleted look identical from inside an
    /// empty rectangle.
    pub failure: Option<String>,
}

/// A request the viewer made of the player, drained by [`AppState::take_player_request`] into a
/// [`Command`] once per frame.
#[derive(Debug, Clone)]
pub enum PlayerRequest {
    /// Show the row's segment from this second — a fresh play, a scrub, and the jump back to the row's
    /// own moment, which are the same request wearing three labels.
    Start(i64),
    /// Stop the run that is live and put the still back on screen — the other half of the toggle, and
    /// the reason the button is labelled for the picture you get rather than the action you took.
    Stop,
}

/// The texture key the moving picture is cached under.
///
/// A synthetic [`RowKey`] rather than the row's own, because the row's slot holds the still it was
/// indexed from and the two are on screen together the moment the user pauses: one entry each, or every
/// second of playback would evict the 0.5–1 s disk read that made the still (see `FRAME_CAP` in
/// `textures`, the cache this key lives in). It is a function and not a constant because [`RowKey`]
/// holds a `String`, which no `const` can.
pub fn player_key() -> RowKey {
    RowKey::new("player", 0)
}

/// What the user asked for. The view never performs I/O; it pushes commands and `app` turns them
/// into jobs on the worker pool (or, for the two that must be synchronous, into a direct call).
#[derive(Debug, Clone)]
pub enum Command {
    Search {
        request_id: u64,
        params: Box<SearchParams>,
    },
    LoadDay {
        request_id: u64,
        date: LocalParts,
    },
    ScanLibrary,
    SaveSettings(Box<Settings>),
    DecodeThumbnail(Box<ThumbnailJob>),
    /// Open the picture a row was actually recorded at. Carries the card, because reading the frame needs
    /// its segment, its offset and the screenshot name it resolved at query time.
    ShowFrame(Box<RowCard>),
    /// Reveal a segment in Explorer. Only ever emitted for a card whose `segment_path` is `Some`.
    Locate {
        path: PathBuf,
    },
    /// Stream the segment, from one second inside it, into the viewer that is already open.
    ///
    /// `run` is the stop flag this command mints — see [`AppState::take_player_request`] — and it rides
    /// along in both directions, out as the thing to raise and back as the thing to compare, because
    /// the reply channel has no way to say which request it answers otherwise. `key` is the row, not the
    /// file: the viewer that asked can have walked away by the time a frame lands.
    PlaySegment {
        key: RowKey,
        segment: PathBuf,
        from: i64,
        run: Arc<AtomicBool>,
    },
    /// Raise the flag of the stream that is playing, so it kills its own ffmpeg before it returns.
    StopSegment {
        run: Arc<AtomicBool>,
    },
    /// Append a row to the user's flag/note CSV at a moment on the OneDay scrubber.
    Flag {
        time: i64,
        path: PathBuf,
    },
    /// Rewrite the note of one flagged row. Carries the `(datetime, note, position)` the panel showed
    /// so the store can re-find the row by content against the current file — the position alone could
    /// have shifted under a concurrent delete. Handled by `wind-notes`, never by a rewrite here.
    EditFlag {
        path: PathBuf,
        when: String,
        note: String,
        index: usize,
        new_note: String,
    },
    /// Delete one flagged row, addressed by the same identity. Emitted only from the panel's explicit
    /// confirm step — a single click on 🗑 parks a "really?" in the state, and only "Yes" reaches this.
    RemoveFlag {
        path: PathBuf,
        when: String,
        note: String,
        index: usize,
    },
    /// Drop the cached directory listings so a newly-converted segment shows up.
    RefreshSegments,
    LoadMonth {
        request_id: u64,
        year: i64,
        month: u32,
    },
    LoadYear {
        request_id: u64,
        year: i64,
    },
    BuildLightbox {
        request_id: u64,
        year: i64,
        month: u32,
    },
    BuildCloud {
        request_id: u64,
        year: i64,
        month: u32,
    },
    /// Enumerate the desktop. Goes to a worker not because it is slow — it is a `EnumDisplayMonitors`
    /// callback pass — but because the answer is only true pixel sizes if the calling thread is
    /// per-monitor DPI aware, and setting that on the frame thread would change how eframe's own
    /// window is scaled. See `backend::displays`.
    ProbeDisplays,
    SaveRecording(Box<Rec>),
    /// The AI page's fifteen keys, through the same merged-map write as the other two forms.
    SaveAi(Box<AiSettings>),
    /// One real chat round trip against the endpoint the form is holding. Boxed and id-carrying
    /// because it goes to a worker: a hosted inference call is seconds, and seconds do not belong on
    /// the frame thread any more than a month file's `COUNT(*)` does.
    TestAi {
        request_id: u64,
        settings: Box<AiSettings>,
    },
    /// Write one template out as the user's own file. Synchronous on purpose: it is a rename over a
    /// small text file, not a network call, and a worker would only make the page disagree about what it
    /// just showed.
    SavePrompt {
        name: crate::ai::PromptName,
        text: String,
    },
    /// Delete one override so the shipped words answer again.
    RestorePrompt {
        name: crate::ai::PromptName,
    },
    /// One real request built from the text **in the box**, against the newest stretch the index can
    /// offer, to the endpoint the form is holding. Worker-backed for the same reason as `TestAi`.
    TestPrompt {
        request_id: u64,
        name: crate::ai::PromptName,
        text: String,
        settings: Box<AiSettings>,
    },
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub tab: Tab,
    /// The window's copy, read through the same [`wind_base::i18n::Catalog`] the tray uses. Held on
    /// the state — exactly as the tray holds it on its supervisor state — so a frame is still a pure
    /// projection of `AppState` and a headless render test owns its own catalog rather than mutating
    /// a global a test forty rows away is reading. `App::new` installs the real one from the resolved
    /// root and the user's `lang`; until then this is the shipped catalog in `en`, which is what lets
    /// a string assertion read real copy and a missing key still surface as a missing key.
    pub catalog: Catalog,
    pub settings: Settings,
    pub draft: Draft,
    /// What this install can offer the Settings page's two pickers: the OCR engines it can drive, and the
    /// locales the shipped catalog translates. Read once at boot the way `rec_options` is — a picker whose
    /// list is not the machine's answer is where a dead control comes from.
    pub settings_options: Options,
    /// The full-frame overlay, open on one row. `None` is the normal state: nothing is painted, and no
    /// texture is held.
    pub frame: Option<FrameView>,
    /// The segment the overlay is playing, if it is playing one. `Some` is the whole of "playing" — a
    /// second field that said the same thing would be a second answer, and the two would drift the
    /// first time a seek cleared one and not the other.
    pub player: Option<Player>,
    /// The stop flag of the stream that is running, if one is.
    ///
    /// Kept apart from [`AppState::player`] because it has to outlive it: closing the viewer drops the
    /// picture and the transport row, and the process behind them still has to be killable from the
    /// request that closure parked. `Arc`, not a bare flag, because the same handle travels out in a
    /// [`Command`] and back in an [`AppEvent`] — and because the return trip compares *pointers*, which
    /// is how a frame from a superseded stream is recognised as one.
    pub player_run: Option<Arc<AtomicBool>>,
    pub notes: Vec<String>,
    /// The recorder's own section: applied values, what is being typed, the machine's ceilings, and
    /// what the desktop reported. Plain data all the way down, so the Recording panel is testable
    /// with the same offscreen frame as every other screen.
    pub rec: Rec,
    pub rec_draft: RecDraft,
    pub rec_options: RecOptions,
    pub rec_notes: Vec<String>,
    /// The AI page's applied values and what is being typed into them, for the reason `rec` and
    /// `rec_draft` are a pair: a number mid-deletion is not zero, and a half-typed base URL must not
    /// overwrite a working one.
    pub ai: AiSettings,
    pub ai_draft: AiDraft,
    pub ai_notes: Vec<String>,
    /// `windai`'s verdict on the configuration the form is holding, plus the key's state. Refreshed
    /// by `app.rs` through `crate::ai`, which is the only place that owns a `Config`, and cached here
    /// so the frame stays a projection of state rather than a reader of files.
    pub ai_status: AiStatus,
    /// The draft revision `ai_status` describes. Zero until the first computation, which `App::new`
    /// performs before the first frame.
    pub ai_status_for: u64,
    /// What the MCP bridge would do with the settings on disk, and whether anything is answering
    /// there now. Refreshed when the window opens and after an AI save — see [`BridgeStatus`].
    pub bridge: BridgeStatus,
    pub ai_test: AiTest,
    /// The prompt editor: seven templates, their drafts, and one trial reply.
    pub ai_prompts: PromptPanel,
    /// The AI values a `SaveAi` command wrote, kept until the reply confirms the file exists.
    pub pending_ai: Option<Box<AiSettings>>,
    /// What the last write did, or why it did not. Quoted verbatim in
    /// the settings panel so the user can see the file that changed.
    pub save_status: String,
    /// A one-line explanation of a rejected input or a jump the user asked for, shown in the
    /// toolbar and cleared by whatever replaces it.
    pub notice: Option<String>,
    /// The settings a `SaveSettings` command wrote, kept until the reply confirms the file exists.
    pub pending_settings: Option<Box<Settings>>,
    /// The recording values a `SaveRecording` command wrote, for the same reason.
    pub pending_rec: Option<Box<Rec>>,
    pub search: SearchState,
    pub day: DayState,
    pub stat: StatState,
    pub footer: Footer,
    pub months: Vec<Month>,
    /// The panels the desktop enumerated, 1-based, real pixels. Empty until the Recording tab asks,
    /// which is the same order upstream shows the row in — one display means no choice to make.
    pub displays: Vec<DisplayInfo>,
    pub displays_pending: bool,
    /// Wall clock at the start of this frame, injected so "today" is testable and so a frame never
    /// calls the OS twice for the same answer.
    pub today: LocalParts,
    pub next_request: u64,
    pub dropped_stale: u64,
    pub in_flight: BTreeSet<RowKey>,
    /// Rows whose stored thumbnail could not be decoded. Bounded, because a corrupt thumbnail on
    /// every row of an old month would otherwise grow this set for the life of the process.
    pub decode_failures: BTreeSet<RowKey>,
    /// Columns the card grid happened to use last frame. Arrow keys step by this, and only the
    /// painter knows what it was.
    pub grid_columns: usize,
    /// Where the user's flags live; resolved once at startup instead of per click.
    pub flag_path: PathBuf,
    /// A `Locate` the painter asked for. The detail pane is nested three closures deep inside
    /// `paint`, so it leaves the request here and `paint` drains it into the command list.
    pub pending_locate: Option<PathBuf>,
    /// The card whose thumbnail was clicked — the request to open its original frame. Parked here for the
    /// same reason `pending_locate` is: the grid is painted inside nested `show_inside` closures that
    /// cannot reach the frame's command list, and a click that reaches the next collection point is a
    /// click that happened.
    pub pending_frame: Option<Box<RowCard>>,
    /// What the transport row asked the player to do — play, stop, or jump to another second. Parked for
    /// the same reason as the two above it, and drained by [`AppState::take_player_request`] rather than
    /// by the painter, because turning it into a command means minting the stop flag the answer will be
    /// checked against.
    pub pending_player: Option<PlayerRequest>,
    pub pending_flag: Option<i64>,
    /// A flag-note edit the panel asked for: the row it was shown, plus the new text. Collected into a
    /// [`Command::EditFlag`] once per frame, the same way `pending_flag` is, because `side_panel` is
    /// nested inside a closure that cannot reach the command list directly.
    pub pending_flag_edit: Option<(FlagNote, String)>,
    /// A confirmed row deletion — the row the panel drew and the user then said "Yes" to.
    pub pending_flag_delete: Option<FlagNote>,
    /// The moment the pointer is over on the OneDay strip, and the strip's own rectangle as the
    /// painter measured it: `[left, top, right, bottom]`. The readout makes the pixel-to-time
    /// mapping visible on screen, and the rectangle is what lets a test click the strip's far end
    /// without guessing where the panel put it.
    pub strip_hover: Option<i64>,
    pub strip_screen: [f32; 4],
    /// How many cards the last frame actually laid out. The grid is virtualised, so this is far
    /// smaller than the page, and it is the number the frame budget is spent on.
    pub painted_cards: usize,
    /// Frame-time stats, so "a frame must render in a few ms" is visible in the running app.
    pub paint_ms_last: f64,
    pub paint_ms_max: f64,
}

impl AppState {
    /// A state for a config that has only been read as far as these two screens need.
    ///
    /// The recorder's own section starts at the shipped defaults and `App::new` overwrites it with
    /// what the file actually holds: `RecDraft::from` has to run against the loaded values, so the
    /// draft cannot be built here without knowing them, and a second constructor that takes them is
    /// a second set of invariants to keep in step. Both are assignable fields for exactly that reason.
    ///
    /// The AI section is the same shape for the same reason, with one addition: its status line is
    /// `windai`'s answer about the shipped defaults, which `App::new` recomputes against the file it
    /// has actually loaded. The verdict is not "ready" here, because the shipped
    /// `open_ai_api_key` is a placeholder and no constructor should claim otherwise.
    pub fn new(settings: Settings, today: LocalParts) -> AppState {
        let page_size = settings.max_page_result.clamp(5, 500) as usize;
        let draft = Draft::from(&settings);
        let rec = Rec::default();
        let rec_options = RecOptions::default();
        let rec_draft = RecDraft::from(&rec);
        let ai = AiSettings::default();
        let ai_draft = AiDraft::from(&ai);
        let mut state = AppState {
            tab: Tab::Search,
            catalog: Catalog::load(&shipped_catalog_root(), "en"),
            notes: Vec::new(),
            notice: None,
            pending_settings: None,
            pending_rec: None,
            ai: ai.clone(),
            ai_draft,
            ai_notes: Vec::new(),
            ai_status: AiStatus::default(),
            ai_status_for: 0,
            bridge: BridgeStatus::default(),
            ai_test: AiTest::default(),
            ai_prompts: PromptPanel::default(),
            pending_ai: None,
            save_status: String::new(),
            search: SearchState::default(),
            day: DayState::default(),
            stat: StatState::default(),
            footer: Footer {
                scanning: true,
                ..Default::default()
            },
            months: Vec::new(),
            displays: Vec::new(),
            displays_pending: false,
            today,
            next_request: 0,
            dropped_stale: 0,
            in_flight: BTreeSet::new(),
            decode_failures: BTreeSet::new(),
            strip_hover: None,
            strip_screen: [0.0; 4],
            painted_cards: 0,
            grid_columns: 1,
            flag_path: PathBuf::new(),
            pending_locate: None,
            pending_frame: None,
            pending_player: None,
            pending_flag: None,
            pending_flag_edit: None,
            pending_flag_delete: None,
            paint_ms_last: 0.0,
            paint_ms_max: 0.0,
            draft,
            settings_options: Options::default(),
            frame: None,
            player: None,
            player_run: None,
            rec: rec.clone(),
            rec_draft,
            rec_notes: Vec::new(),
            rec_options,
            settings,
        };
        state.search.params.page_size = page_size;
        state.day.date = default_day(today, state.settings.day_begin_minutes);
        // The Stat tab's pickers open on the last month the index actually holds, which is upstream's
        // `value=stat_db_latest_datetime.year` — the newest thing there is to look at.
        state.stat.year = today.year;
        state.stat.month = today.month;
        state.stat.watermark = true;
        state
    }

    fn bump(&mut self) -> u64 {
        self.next_request += 1;
        self.next_request
    }

    /// Keep the counter ahead of any id seen in a reply. Without this, a request id that arrives
    /// from outside the normal path (a test, a re-delivery) would let a later `bump` reuse it and
    /// the "stale" test below would drop a live reply.
    fn note_id(&mut self, id: u64) {
        self.next_request = self.next_request.max(id);
    }

    /// The page size in effect: the user's pick, never more than the configured ceiling.
    ///
    /// `max_page_result` is clamped *upwards* to the field's own floor first, because a config that
    /// was hand-edited below it (`2`, say, or `0`) would otherwise make `clamp(min, max)` panic with
    /// `min > max` — a settings file must never be able to crash the UI.
    pub fn page_size(&self) -> usize {
        let ceiling = self.settings.max_page_result.clamp(5, 500) as usize;
        self.search.params.page_size.clamp(5, ceiling.max(5))
    }

    /// Issue a search for page 1. Re-running resets the page, because paging into a result set the
    /// user has just changed is how you end up showing nothing and blaming the index.
    pub fn submit_search(&mut self) -> (u64, SearchParams) {
        self.search.params.page = 1;
        self.issue_search()
    }

    fn issue_search(&mut self) -> (u64, SearchParams) {
        let id = self.bump();
        self.search.pending = true;
        self.search.error = None;
        self.search.request_id = id;
        self.search.params.page_size = self.page_size();
        (id, self.search.params.clone())
    }

    /// Jump to a page. `None` when the page is unreachable, so a stray click cannot ask for it.
    pub fn goto_page(&mut self, page: usize) -> Option<(u64, SearchParams)> {
        if page == 0 || page > self.search.pages.max(1) || self.search.answered.is_none() {
            return None;
        }
        if page == self.search.params.page && self.search.ran {
            return None;
        }
        self.search.params.page = page;
        self.search.selected = None;
        Some(self.issue_search())
    }

    pub fn select_search(&mut self, index: usize) {
        if index < self.search.cards.len() {
            self.search.selected = Some(index);
        }
    }

    /// Arrow-key navigation. The view converts an up/down press into `±columns` because only the
    /// painter knows how wide the grid got; the state must not care.
    pub fn move_search_selection(&mut self, delta: isize) {
        let count = self.search.cards.len();
        if count == 0 {
            return;
        }
        let current = self.search.selected.unwrap_or(0) as isize;
        // The grid is a flow layout, so it stops at the end of the list rather than wrapping; the
        // selection has to do the same or the caret detaches from the highlighted card.
        self.search.selected = Some((current + delta).clamp(0, count as isize - 1) as usize);
    }

    /// Re-reading the day is the only way a date change reaches the store; nothing else in the app
    /// opens the month files for a specific day.
    pub fn set_day(&mut self, day: LocalParts) -> (u64, LocalParts) {
        let id = self.bump();
        self.day.date = date(day.year, day.month, day.day);
        self.day.pending = true;
        self.day.error = None;
        self.day.request_id = id;
        (id, self.day.date)
    }

    pub fn scrub_to(&mut self, time: i64) {
        self.day.scrub = time;
        let visible = self.day.visible();
        self.day.selected = self.day.row_for_time(&visible, time);
    }

    /// A click on the strip: the pixel is already converted to a time by the view, and the cell
    /// lookup is the store's own `index_for` contract restated over the precomputed spans.
    pub fn click_strip(&mut self, time: i64) {
        if let Some(index) = self.day.cell_for_time(time) {
            if let Some(Some(t)) = self.day.strip.get(index).map(|c| c.time) {
                self.scrub_to(t);
            }
        }
    }

    pub fn apply_filter(&mut self, filter: &str) {
        self.day.filter = filter.to_string();
        let visible = self.day.visible();
        self.day.selected = self.day.row_for_time(&visible, self.day.scrub);
    }

    // -------------------------------------------------------------------------------------
    // Stat: four independent queries over the same month files
    // -------------------------------------------------------------------------------------

    /// The earliest and latest record the library holds, as the year the pickers may reach.
    ///
    /// Upstream reads these once from `db_first_earliest_record_time` / `db_latest_record_time` and
    /// caches them for the session; here they are the footer's running answer, which is the same two
    /// numbers and is already being recomputed per month file. Before the first scan finishes there
    /// is nothing to bound the pickers with, so the picker falls back to the wall clock rather than
    /// offering an empty range.
    pub fn record_years(&self) -> (i64, i64) {
        let earliest = self.footer.first.map(LocalParts::from_naive_epoch).unwrap_or(self.today);
        let latest = self.footer.last.map(LocalParts::from_naive_epoch).unwrap_or(self.today);
        (earliest.year.min(latest.year), latest.year.max(earliest.year))
    }

    /// The months the given year can offer. Exactly upstream's rule, including the asymmetry that
    /// makes it worth a function: the first and last year of the library are cut to the months that
    /// have data, every year between is whole.
    pub fn record_months(&self, year: i64) -> (u32, u32) {
        let earliest = self.footer.first.map(LocalParts::from_naive_epoch).unwrap_or(self.today);
        let latest = self.footer.last.map(LocalParts::from_naive_epoch).unwrap_or(self.today);
        let low = if year == earliest.year { earliest.month } else { 1 };
        let high = if year == latest.year { latest.month } else { 12 };
        (low, high.max(low))
    }

    /// Move the year, clamping the month back into the range the new year can actually show. Without
    /// the clamp, picking 2027 while looking at September would query a month the library cannot have
    /// and paint an empty chart that reads as "you recorded nothing", not as "there is no such month".
    pub fn set_stat_year(&mut self, year: i64) -> (i64, u32) {
        let (low, high) = self.record_years();
        self.stat.year = year.clamp(low, high);
        let (first, last) = self.record_months(self.stat.year);
        self.stat.month = self.stat.month.clamp(first, last);
        (self.stat.year, self.stat.month)
    }

    pub fn set_stat_month(&mut self, month: u32) -> u32 {
        let (first, last) = self.record_months(self.stat.year);
        self.stat.month = month.clamp(first, last);
        self.stat.month
    }

    pub fn load_month(&mut self) -> (u64, i64, u32) {
        let id = self.bump();
        self.stat.month_track.issue(id);
        (id, self.stat.year, self.stat.month)
    }

    pub fn load_year(&mut self) -> (u64, i64) {
        let id = self.bump();
        self.stat.year_track.issue(id);
        (id, self.stat.year)
    }

    pub fn build_lightbox(&mut self) -> (u64, i64, u32) {
        let id = self.bump();
        self.stat.tiles_track.issue(id);
        (id, self.stat.year, self.stat.month)
    }

    pub fn build_cloud(&mut self) -> (u64, i64, u32) {
        let id = self.bump();
        self.stat.cloud_track.issue(id);
        (id, self.stat.year, self.stat.month)
    }

    /// Press "test the connection". `None` while a probe is already in flight, because a second
    /// hosted request the user did not ask for costs a second set of tokens — and because the
    /// spinner is the answer that says "wait and see", not "press again".
    pub fn begin_ai_test(&mut self, settings: AiSettings) -> Option<(u64, AiSettings)> {
        if self.ai_test.track.pending {
            return None;
        }
        let id = self.bump();
        self.ai_test.track.issue(id);
        self.ai_test.report = None;
        Some((id, settings))
    }

    /// Reload the panel from disk. Called when the page opens and after anything that changed a file,
    /// so the box always shows what the next request would send — and an unsaved edit is visibly gone
    /// rather than silently kept.
    pub fn reload_prompts(&mut self, config: &wind_base::config::Config) {
        self.ai_prompts.rows = crate::ai::prompt_rows(config).iter().map(PromptDraft::load).collect();
        self.ai_prompts.status = String::new();
    }

    /// A keystroke in one template's box.
    pub fn edit_prompt(&mut self, name: crate::ai::PromptName, text: &str) {
        if let Some(row) = self.ai_prompts.rows.iter_mut().find(|row| row.name == name) {
            if row.text != text {
                row.text = text.to_string();
                row.dirty = true;
            }
        }
    }

    /// Press "try these words". `None` while a trial is in flight, for the two reasons
    /// [`Self::begin_ai_test`] gives: a second hosted request the user did not ask for costs a second
    /// set of tokens, and the spinner is the answer that says wait rather than press again.
    pub fn begin_prompt_trial(
        &mut self,
        name: crate::ai::PromptName,
        text: String,
        settings: AiSettings,
    ) -> Option<(u64, String, Box<AiSettings>)> {
        if self.ai_prompts.trial.pending {
            return None;
        }
        let id = self.bump();
        self.ai_prompts.trial.issue(id);
        self.ai_prompts.report = None;
        self.ai_prompts.trial_for = Some(name);
        Some((id, text, Box::new(settings)))
    }

    /// A month change invalidates every panel derived from it. The tiles and the cloud are dropped
    /// rather than kept-and-marked, because a texture cache full of last month's rows is the one way
    /// this screen could show a picture the user never asked for.
    pub fn stat_month_changed(&mut self) {
        self.stat.days.clear();
        self.stat.tiles.clear();
        self.stat.words.clear();
        self.stat.placed.clear();
        self.stat.month_track.loaded = false;
        self.stat.tiles_track.loaded = false;
        self.stat.cloud_track.loaded = false;
    }


    /// Fold a worker reply in. Returns `true` when the frame that follows must actually change.
    pub fn apply(&mut self, event: AppEvent) -> bool {
        match event {
            AppEvent::Search { request_id, outcome } => {
                self.note_id(request_id);
                if request_id < self.search.request_id {
                    self.dropped_stale += 1;
                    return false;
                }
                self.search.pending = false;
                self.search.ran = true;
                match outcome {
                    Ok(o) => {
                        self.search.total = o.total;
                        self.search.pages = o.pages;
                        self.search.elapsed_ms = o.elapsed_ms;
                        self.search.cards = o.cards;
                        self.search.terms = o.terms;
                        self.search.answered = Some(o.params);
                        self.search.error = None;
                        if self.search.selected.map_or(true, |s| s >= self.search.cards.len()) {
                            self.search.selected = if self.search.cards.is_empty() { None } else { Some(0) };
                        }
                    }
                    Err(e) => {
                        self.search.error = Some(e);
                        self.search.cards.clear();
                        self.search.total = 0;
                        self.search.pages = 0;
                        self.search.answered = None;
                        self.search.selected = None;
                    }
                }
                true
            }
            AppEvent::Day { request_id, date, outcome } => {
                self.note_id(request_id);
                if request_id < self.day.request_id {
                    self.dropped_stale += 1;
                    return false;
                }
                self.day.pending = false;
                self.day.date = date;
                match outcome {
                    Ok(o) => {
                        self.day.bounds = o.bounds;
                        self.day.all = o.cards;
                        self.day.buckets = o.buckets;
                        self.day.strip = o.strip;
                        self.day.strip_span = o.strip_span;
                        self.day.active_hours = o.active_hours;
                        self.day.titles = o.titles;
                        self.day.flags = o.flags;
                        // A reload reassigns every row's file position, so a draft keyed to the old
                        // positions and a delete awaiting confirmation both stop meaning what they
                        // pointed at. Drop them rather than risk applying them to a shifted row.
                        self.day.flag_drafts.clear();
                        self.day.flag_confirm = None;
                        self.day.unindexed_video = o.unindexed_video;
                        self.day.error = if o.warnings.is_empty() { None } else { Some(o.warnings.join("; ")) };
                        self.day.loaded = true;
                        let last = self.day.all.last().map(|c| c.time).unwrap_or(o.bounds.0);
                        self.scrub_to(last);
                    }
                    Err(e) => {
                        self.day.error = Some(e);
                        self.day.all.clear();
                        self.day.strip.clear();
                        self.day.buckets.clear();
                        self.day.titles.clear();
                        self.day.loaded = true;
                    }
                }
                true
            }
            AppEvent::Library(mut stats) => {
                // Whatever the scan found is the library now, including the case that matters: the
                // recorder wrote its first month file while this window was open.
                self.months = std::mem::take(&mut stats.months);
                self.footer.months_total = stats.months_total;
                self.footer.months_scanned = stats.months_scanned;
                self.footer.rows = stats.rows;
                self.footer.first = stats.first;
                self.footer.last = stats.last;
                self.footer.scanning = !stats.done;
                self.footer.error = stats.error;
                if stats.done && self.search.answered.is_none() {
                    // The default range is "everything the index holds", bounded by what is actually
                    // on disk so the date pickers do not start in 2038.
                    if let Some(last) = stats.last {
                        self.search.params.to = LocalParts::from_naive_epoch(last).date_only();
                    }
                    if let Some(first) = stats.first {
                        self.search.params.from = LocalParts::from_naive_epoch(first).date_only();
                    }
                }
                true
            }
            AppEvent::SettingsSaved(result) => {
                match result {
                    Ok(path) => {
                        self.save_status = format!("wrote {}", path.display());
                        if let Some(settings) = self.pending_settings.take() {
                            self.settings = *settings;
                        }
                        let (validated, notes) = self.draft.validate(&self.settings, &self.settings_options);
                        self.settings = validated;
                        // The draft is re-rendered from what was applied, so a picker the machine refused
                        // shows the value that reached the file rather than the one that was typed.
                        self.draft = Draft::from(&self.settings);
                        self.notes = notes;
                        self.day.date = default_day(self.today, self.settings.day_begin_minutes);
                        self.search.params.page_size = self.page_size();
                    }
                    Err(e) => self.save_status = format!("save failed: {e}"),
                }
                true
            }
            AppEvent::RecordingSaved(result) => {
                match result {
                    Ok(path) => {
                        self.save_status = format!("wrote {}", path.display());
                        if let Some(rec) = self.pending_rec.take() {
                            self.rec = *rec;
                        }
                        // Re-render the draft from what is now applied, so the field a note complained
                        // about shows the number that went to the file rather than the one typed.
                        self.rec_draft = RecDraft::from(&self.rec);
                        self.rec_notes = Vec::new();
                    }
                    Err(e) => self.save_status = format!("save failed: {e}"),
                }
                true
            }
            AppEvent::AiSaved(result) => {
                match result {
                    Ok(path) => {
                        self.save_status = format!("wrote {}", path.display());
                        if let Some(ai) = self.pending_ai.take() {
                            self.ai = *ai;
                        }
                        // Same rule as the Recording tab: the box a note complained about shows the
                        // value that went to the file, not the one that was typed.
                        self.ai_draft = AiDraft::from(&self.ai);
                        self.ai_notes = Vec::new();
                        // The draft was just re-seeded, so the cached status line describes a form
                        // that no longer exists. Force the recompute the next frame owes the user.
                        self.ai_status_for = u64::MAX;
                    }
                    Err(e) => self.save_status = format!("save failed: {e}"),
                }
                true
            }
            AppEvent::AiTested { request_id, outcome } => {
                self.note_id(request_id);
                if !self.ai_test.track.accepts(request_id) {
                    self.dropped_stale += 1;
                    return false;
                }
                self.ai_test.report = Some(outcome);
                true
            }
            AppEvent::PromptTried { request_id, trial } => {
                self.note_id(request_id);
                if !self.ai_prompts.trial.accepts(request_id) {
                    self.dropped_stale += 1;
                    return false;
                }
                self.ai_prompts.report = Some(trial);
                true
            }
            AppEvent::Thumbnail { key, image } => {
                self.in_flight.remove(&key);
                match image {
                    Some(_) => true,
                    // Recorded so the card stops asking; the failure is silent on screen because a
                    // placeholder is drawn either way.
                    None => {
                        if self.decode_failures.len() > 4096 {
                            self.decode_failures.clear();
                        }
                        self.decode_failures.insert(key);
                        true
                    }
                }
            }
            AppEvent::Frame { key, source, image } => {
                // The reply answers the viewer that asked for it. A user who clicked another card, or
                // closed the overlay, has already been served by a newer request or wants nothing — and
                // painting a late frame over a viewer that moved on is how a window shows a picture
                // nobody asked for.
                let Some(view) = self.frame.as_mut() else { return false };
                if view.key != key || !view.loading {
                    return false;
                }
                view.loading = false;
                match image {
                    Some(_) => {
                        view.source = source;
                        view.missing = false;
                    }
                    None => view.missing = true,
                }
                true
            }
            AppEvent::PlayerFrame { run, key, at, image: _ } => {
                // Two gates, and each of them a different bug.
                //
                // The pointer comparison is the staleness rule: a seek mints a new run and installs it as
                // `player_run` while the stream it replaces may still be one frame deep in a reply, and
                // four workers mean that reply can arrive whenever it likes. Painting it would put the
                // second the user scrubbed *away from* back on screen, under a transport row that has
                // already moved. The key comparison catches the other leak — a stream that outlived its
                // viewer because the arrows moved to another row, whose frames belong to a picture nobody
                // is being shown.
                let Some(live) = self.player_run.as_ref() else { return false };
                if !Arc::ptr_eq(live, &run) || self.player.as_ref().map_or(true, |player| player.key != key) {
                    self.dropped_stale += 1;
                    return false;
                }
                let Some(player) = self.player.as_mut() else { return false };
                // `failure` is cleared because a frame after a complaint is the window's way of saying the
                // complaint no longer stands: a stream that resumes after a seek that half-failed should
                // stop apologising, or the amber sentence sits over moving pictures forever.
                let changed = player.at != at || player.waiting || player.failure.is_some();
                player.at = at;
                player.waiting = false;
                player.failure = None;
                changed
            }
            AppEvent::PlayerDone { run, key, duration, failure } => {
                // The same two gates as above, and for the same reason stated once: a stream the user has
                // since replaced does not get to describe the player that replaced it — including by
                // declaring it finished.
                let Some(live) = self.player_run.as_ref() else { return false };
                if !Arc::ptr_eq(live, &run) || self.player.as_ref().map_or(true, |player| player.key != key) {
                    self.dropped_stale += 1;
                    return false;
                }
                let Some(player) = self.player.as_mut() else { return false };
                player.duration = duration;
                player.waiting = false;
                player.failure = failure;
                // Deliberately *not* `player = None` on a failure. Dropping the player would drop the
                // sentence with it, and `frame_viewer` would fall back to painting the still as though
                // nothing had been asked — the black-box silence this whole path exists to avoid. The
                // `Player` stays so the transport row stays up with its reason in it; `Stop` and the next
                // click are what take it away.
                true
            }
            AppEvent::StatMonth { request_id, outcome } => {
                self.note_id(request_id);
                if !self.stat.month_track.accepts(request_id) {
                    self.dropped_stale += 1;
                    return false;
                }
                match outcome {
                    Ok(totals) => {
                        self.stat.days = totals.points;
                        self.stat.month_rows = totals.rows;
                        self.stat.month_track.error = warnings(&totals.warnings);
                    }
                    Err(e) => {
                        self.stat.days.clear();
                        self.stat.month_rows = 0;
                        self.stat.month_track.error = Some(e);
                    }
                }
                true
            }
            AppEvent::StatYear { request_id, outcome } => {
                self.note_id(request_id);
                if !self.stat.year_track.accepts(request_id) {
                    self.dropped_stale += 1;
                    return false;
                }
                match outcome {
                    Ok(totals) => {
                        self.stat.year_points = totals.points;
                        self.stat.year_rows = totals.rows;
                        self.stat.year_track.error = warnings(&totals.warnings);
                    }
                    Err(e) => {
                        self.stat.year_points.clear();
                        self.stat.year_rows = 0;
                        self.stat.year_track.error = Some(e);
                    }
                }
                true
            }
            AppEvent::Lightbox { request_id, outcome } => {
                self.note_id(request_id);
                if !self.stat.tiles_track.accepts(request_id) {
                    self.dropped_stale += 1;
                    return false;
                }
                // A superseded build's tiles are dropped above, and the ones already decoded stay in
                // the texture cache: they are the same rows the strip and the grid may want.
                self.stat.tiles.clear();
                match outcome {
                    Ok(tiles) => self.stat.tiles = tiles,
                    Err(e) => self.stat.tiles_track.error = Some(e),
                }
                true
            }
            AppEvent::Cloud { request_id, outcome } => {
                self.note_id(request_id);
                if !self.stat.cloud_track.accepts(request_id) {
                    self.dropped_stale += 1;
                    return false;
                }
                match outcome {
                    Ok(words) => {
                        self.stat.words = words;
                        // Deliberately not laid out here: the spiral needs glyph metrics, which only
                        // a frame that owns a `Ui` can supply. `view` fills `placed` and `placed_for`.
                        self.stat.placed.clear();
                        self.stat.cloud_track.error = None;
                    }
                    Err(e) => {
                        self.stat.words.clear();
                        self.stat.placed.clear();
                        self.stat.cloud_track.error = Some(e);
                    }
                }
                true
            }
            AppEvent::Displays(result) => {
                self.displays_pending = false;
                match result {
                    Ok(list) => self.displays = list,
                    // The panel keeps the number the config holds and says it could not check it,
                    // which is truer than an empty list that looks like a single display.
                    Err(e) => self.notice = Some(e),
                }
                true
            }
        }
    }

    /// The thumbnails worth decoding right now, oldest request first.
    ///
    /// Called once per frame from `app`, which then filters against the texture cache. Keeping the
    /// "what is on screen" decision here means the prefetch set is testable without a window.
    pub fn thumbnail_jobs(&mut self) -> Vec<ThumbnailJob> {
        let mut seen: VecDeque<ThumbnailJob> = VecDeque::new();
        let push = |card: &RowCard, seen: &mut VecDeque<ThumbnailJob>| {
            if seen.len() >= PREFETCH_LIMIT {
                return;
            }
            if let Some(base64) = card.thumbnail.as_deref() {
                if self.in_flight.contains(&card.key) || self.decode_failures.contains(&card.key) {
                    return;
                }
                seen.push_back(ThumbnailJob {
                    key: card.key.clone(),
                    base64: base64.to_string(),
                });
            }
        };
        match self.tab {
            Tab::Search => {
                for card in &self.search.cards {
                    push(card, &mut seen);
                }
            }
            Tab::OneDay => {
                for cell in &self.day.strip {
                    if seen.len() >= PREFETCH_LIMIT {
                        break;
                    }
                    if let (Some(key), Some(base64)) = (cell.key.clone(), cell.thumbnail.as_deref()) {
                        if self.in_flight.contains(&key) || self.decode_failures.contains(&key) {
                            continue;
                        }
                        seen.push_back(ThumbnailJob {
                            key,
                            base64: base64.to_string(),
                        });
                    }
                }
                let visible = self.day.visible();
                for &i in &visible {
                    if let Some(card) = self.day.all.get(i) {
                        push(card, &mut seen);
                    }
                }
            }
            Tab::Settings => {}
            Tab::Recording => {}
            Tab::Ai => {}
            // The lightbox is the one screen in the app whose whole purpose is a picture of every
            // thumbnail at once, so it is also the one that asks for the most decodes. The cap in
            // `push` is what turns that from a stall into a grid that fills in.
            Tab::Stat => {
                for tile in &self.stat.tiles {
                    if seen.len() >= PREFETCH_LIMIT {
                        break;
                    }
                    if self.in_flight.contains(&tile.key) || self.decode_failures.contains(&tile.key) {
                        continue;
                    }
                    if let Some(base64) = tile.thumbnail.as_deref() {
                        seen.push_back(ThumbnailJob {
                            key: tile.key.clone(),
                            base64: base64.to_string(),
                        });
                    }
                }
            }
        }
        self.in_flight.extend(seen.iter().map(|j| j.key.clone()));
        seen.into_iter().collect()
    }

/// `2026-09-21 03:00 → 2026-09-22 02:59`, the day the panel is actually looking at.
    pub fn date_label(&self) -> String {
        let (from, to) = self.day.bounds;
        if to <= from {
            return "—".to_string();
        }
        let cut = |t: i64| {
            let p = LocalParts::from_naive_epoch(t);
            format!("{} {:02}:{:02}", p.date_stamp(), p.hour, p.minute)
        };
        format!("{} → {}", cut(from), cut(to))
    }

    /// Which search card the detail pane describes.
    pub fn selected_search_card(&self) -> Option<&RowCard> {
        self.search.selected.and_then(|i| self.search.cards.get(i))
    }

    pub fn selected_day_card(&self) -> Option<&RowCard> {
        self.day.selected.and_then(|i| self.day.all.get(i))
    }
}

/// Non-fatal complaints from a read that mostly worked, or `None` when it was clean.
///
/// A month file that will not open must not turn a chart red — the other eleven months of the year
/// still answered — but it must not be silent either, or the total the chart prints is quietly wrong.
fn warnings(list: &[String]) -> Option<String> {
    if list.is_empty() {
        None
    } else {
        Some(list.join("; "))
    }
}

/// "Today", corrected for the day boundary: at 01:30 with `day_begin_minutes = 180` the user is
/// still inside yesterday, and the WebUI has always opened on yesterday in that window.
pub fn default_day(now: LocalParts, day_begin_minutes: i64) -> LocalParts {
    let minutes = i64::from(now.hour) * 60 + i64::from(now.minute);
    let today = date(now.year, now.month, now.day);
    if minutes < day_begin_minutes {
        shift_date(today, -1)
    } else {
        today
    }
}

/// Calendar arithmetic through the shared epoch, so month and year rollovers are not reimplemented
/// here — `from_naive_epoch`/`naive_epoch_seconds` already round-trip them, and the noon offset
/// keeps a sub-day shift from ever crossing the midnight the round trip would round on.
pub fn shift_date(day: LocalParts, delta: i64) -> LocalParts {
    let noon = date(day.year, day.month, day.day).naive_epoch_seconds() + 43_200;
    LocalParts::from_naive_epoch(noon + delta * 86_400).date_only()
}

/// `YYYY-MM-DD` as typed by the user. Anything else is not a date and must not silently move the
/// OneDay view.
pub fn parse_date(text: &str) -> Option<LocalParts> {
    let trimmed = text.trim();
    let parts: Vec<&str> = trimmed.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let year: i64 = parts[0].parse().ok()?;
    let month: u32 = parts[1].parse().ok()?;
    let day: u32 = parts[2].parse().ok()?;
    if !(1970..2200).contains(&year) || month < 1 || month > 12 || day < 1 || day > clock::days_in_month(year, month) {
        return None;
    }
    Some(date(year, month, day))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn settings() -> Settings {
        Settings {
            max_page_result: 20,
            oneday_timeline_pic_num: 50,
            day_begin_minutes: 180,
            maintain_window_start: String::new(),
            maintain_window_end: String::new(),
            use_similar_ch_char_to_search: false,
            ocr_lang: "zh-Hans-CN".into(),
            ocr_engine: wind_base::ocr::WINDOWS_ENGINE.into(),
            lang: "en".into(),
            exclude_words: vec![],
            ocr_image_crop_urbl: vec![6, 6, 6, 3],
            enable_ocr_str_highlight_indicator: true,
            thumbnail_generation_size_width: 70,
            close_window_to_tray: true,
            start_app_on_boot: false,
        }
    }

    fn card(rowid: i64, time: i64) -> RowCard {
        RowCard {
            key: RowKey::new("default_2026-09_wind.db", rowid),
            time,
            clock: String::new(),
            day: String::new(),
            title: Some("Notepad".into()),
            body: "hello world".into(),
            segment: "2026-09-21_10-00-00.mp4".into(),
            offset: Some(0),
            deep_link: None,
            thumbnail: Some("AAA".into()),
            segment_path: None,
            picture_path: None,
        }
    }

    fn at(stamp: &str) -> i64 {
        LocalParts::from_stamp(stamp).unwrap().naive_epoch_seconds()
    }

    fn state_with(cards: Vec<RowCard>, total: i64, pages: usize) -> AppState {
        let mut state = AppState::new(settings(), date(2026, 9, 22));
        state.apply(AppEvent::Search {
            request_id: 1,
            outcome: Ok(SearchOutcome {
                cards,
                total,
                pages,
                elapsed_ms: 7,
                terms: vec![],
                params: Box::new(SearchParams {
                    page: 1,
                    ..SearchParams::default()
                }),
            }),
        });
        state
    }

    #[test]
    fn a_superseded_search_reply_is_dropped_and_counted() {
        let mut state = state_with(vec![card(1, at("2026-09-21_10-00-00"))], 1, 1);
        let (newer, _) = state.submit_search();
        assert!(newer > 1);
        let stale = AppEvent::Search {
            request_id: 1,
            outcome: Ok(SearchOutcome {
                cards: vec![card(99, at("2026-09-21_11-00-00"))],
                total: 1,
                pages: 1,
                elapsed_ms: 1,
                terms: vec![],
                params: Box::new(SearchParams::default()),
            }),
        };
        assert!(!state.apply(stale), "a stale reply must not ask for a repaint");
        assert_eq!(state.search.cards[0].key.rowid, 1, "the visible rows must be the newer set");
        assert_eq!(state.dropped_stale, 1);
    }

    #[test]
    fn a_newer_reply_replaces_the_page_and_clears_pending() {
        let mut state = state_with(vec![card(1, at("2026-09-21_10-00-00"))], 1, 1);
        let (id, params) = state.submit_search();
        assert!(state.search.pending);
        state.apply(AppEvent::Search {
            request_id: id,
            outcome: Ok(SearchOutcome {
                cards: vec![],
                total: 0,
                pages: 0,
                elapsed_ms: 3,
                params: Box::new(params),
                terms: vec![],
            }),
        });
        assert!(!state.search.pending);
        assert!(state.search.cards.is_empty());
        assert_eq!(state.search.status(), "0 of 0 results · page 1/1 · 3 ms");
    }

    #[test]
    fn paging_only_offers_pages_that_exist() {
        let mut state = state_with(vec![card(1, at("2026-09-21_10-00-00"))], 45, 3);
        assert!(state.goto_page(0).is_none());
        assert!(state.goto_page(4).is_none());
        assert!(state.goto_page(1).is_none(), "already on page 1");
        let (_, params) = state.goto_page(3).expect("page 3 of 3 exists");
        assert_eq!(params.page, 3);
        assert!(state.search.pending);
    }

    #[test]
    fn selection_clamps_at_the_end_of_the_grid_instead_of_wrapping() {
        let mut state = state_with(vec![card(1, 10), card(2, 20), card(3, 30)], 3, 1);
        state.select_search(2);
        state.move_search_selection(1);
        assert_eq!(state.search.selected, Some(2), "the last card stays selected");
        state.select_search(0);
        state.move_search_selection(-1);
        assert_eq!(state.search.selected, Some(0));
        state.move_search_selection(2);
        assert_eq!(state.search.selected, Some(2), "a column step moves a whole row");
    }

    #[test]
    fn search_endpoints_are_pushed_out_to_the_day_poles() {
        let params = SearchParams {
            from: date(2026, 9, 21),
            to: date(2026, 9, 21),
            ..SearchParams::default()
        };
        let (from, to) = params.range(180);
        assert_eq!(from, at("2026-09-21_03-00-00"));
        assert_eq!(to, at("2026-09-22_02-59-59"), "01:00 on the 22nd is the 21st's data");
        let (from, to) = params.range(0);
        assert_eq!((from, to), (at("2026-09-21_00-00-00"), at("2026-09-21_23-59-59")));
    }

    #[test]
    fn the_default_day_rolls_back_before_the_day_boundary() {
        let early = LocalParts {
            year: 2026,
            month: 9,
            day: 22,
            hour: 1,
            minute: 30,
            second: 0,
        };
        assert_eq!(default_day(early, 180), date(2026, 9, 21));
        assert_eq!(default_day(early, 0), date(2026, 9, 22));
        let late = LocalParts {
            year: 2026,
            month: 9,
            day: 22,
            hour: 4,
            minute: 0,
            second: 0,
        };
        assert_eq!(default_day(late, 180), date(2026, 9, 22));
    }

    #[test]
    fn day_shifts_cross_month_and_year_boundaries() {
        assert_eq!(shift_date(date(2026, 9, 1), -1), date(2026, 8, 31));
        assert_eq!(shift_date(date(2026, 12, 31), 1), date(2027, 1, 1));
        assert_eq!(shift_date(date(2024, 2, 28), 1), date(2024, 2, 29));
    }

    #[test]
    fn a_malformed_date_does_not_move_the_view() {
        assert!(parse_date("2026-09-21").is_some());
        assert!(parse_date("2026-9-21").is_some());
        assert!(parse_date("2026-02-30").is_none(), "the 30th of February is not a day");
        assert!(parse_date("yesterday").is_none());
        assert!(parse_date("2026-00-10").is_none());
    }

    #[test]
    fn the_scrub_slider_selects_the_latest_row_at_or_before_its_time() {
        let mut state = AppState::new(settings(), date(2026, 9, 22));
        state.apply(AppEvent::Day {
            request_id: 1,
            date: date(2026, 9, 21),
            outcome: Ok(DayOutcome {
                cards: vec![card(1, at("2026-09-21_09-00-00")), card(2, at("2026-09-21_17-00-00"))],
                bounds: (at("2026-09-21_03-00-00"), at("2026-09-22_02-59-59")),
                buckets: vec![],
                strip_span: (at("2026-09-21_00-00-00"), at("2026-09-21_23-59-59")),
                active_hours: 1.0,
                strip: vec![StripCell {
                    from: at("2026-09-21_00-00-00"),
                    to: at("2026-09-21_12-00-00"),
                    time: Some(at("2026-09-21_09-00-00")),
                    key: Some(RowKey::new("default_2026-09_wind.db", 1)),
                    thumbnail: Some("AAA".into()),
                    clock: Some("09:00:00".into()),
                }],
                titles: vec![],
                flags: vec![],
                unindexed_video: false,
                warnings: vec![],
            }),
        });
        state.scrub_to(at("2026-09-21_10-00-00"));
        assert_eq!(state.day.selected, Some(0), "the 09:00 frame is what was on screen at 10:00");
        state.scrub_to(at("2026-09-21_18-00-00"));
        assert_eq!(state.day.selected, Some(1));
        state.click_strip(at("2026-09-21_11-00-00"));
        assert_eq!(state.day.scrub, at("2026-09-21_09-00-00"), "a click jumps to the sample it hit");
    }

    #[test]
    fn an_empty_day_is_not_the_same_state_as_an_unindexed_one() {
        let mut state = AppState::new(settings(), date(2026, 9, 22));
        state.apply(AppEvent::Day {
            request_id: 1,
            date: date(2026, 9, 21),
            outcome: Ok(DayOutcome {
                cards: vec![],
                bounds: (at("2026-09-21_03-00-00"), at("2026-09-22_02-59-59")),
                buckets: vec![],
                strip: vec![],
                strip_span: (0, 0),
                active_hours: 0.0,
                titles: vec![],
                flags: vec![],
                unindexed_video: true,
                warnings: vec![],
            }),
        });
        assert!(state.day.all.is_empty());
        assert!(state.day.unindexed_video);
    }

    #[test]
    fn the_in_day_filter_never_asks_the_store_again() {
        let mut state = AppState::new(settings(), date(2026, 9, 22));
        state.apply(AppEvent::Day {
            request_id: 1,
            date: date(2026, 9, 21),
            outcome: Ok(DayOutcome {
                cards: vec![card(1, 10), {
                    let mut c = card(2, 20);
                    c.body = "other text".into();
                    c
                }],
                bounds: (0, 100),
                buckets: vec![],
                strip: vec![],
                strip_span: (0, 0),
                active_hours: 0.0,
                titles: vec![],
                flags: vec![],
                unindexed_video: false,
                warnings: vec![],
            }),
        });
        let before = state.day.request_id;
        state.apply_filter("hello");
        assert_eq!(state.day.visible().len(), 1);
        assert_eq!(state.day.request_id, before, "filtering is a view over the one fetch");
        state.apply_filter("");
        assert_eq!(state.day.visible().len(), 2);
    }

    #[test]
    fn prefetch_is_bounded_and_never_asks_for_the_same_row_twice() {
        let cards: Vec<RowCard> = (0..(PREFETCH_LIMIT + 50) as i64).map(|i| card(i, i)).collect();
        let mut state = state_with(cards, 250, 2);
        let jobs = state.thumbnail_jobs();
        assert_eq!(jobs.len(), PREFETCH_LIMIT, "the queue is capped per visible set");
        let rest = state.thumbnail_jobs();
        assert_eq!(rest.len(), 50, "the tail of the page is offered next, not dropped");
        assert_eq!(state.thumbnail_jobs().len(), 0, "everything is in flight now");
        assert_eq!(jobs[0].key, RowKey::new("default_2026-09_wind.db", 0));
        state.apply(AppEvent::Thumbnail {
            key: jobs[0].key.clone(),
            image: None,
        });
        assert_eq!(state.thumbnail_jobs().len(), 0, "a failed decode is not retried");
        state.in_flight.clear();
        state.apply(AppEvent::Thumbnail {
            key: RowKey::new("default_2026-09_wind.db", 7777),
            image: Some(DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![0, 0, 0, 0],
            }),
        });
        assert_eq!(state.thumbnail_jobs().len(), PREFETCH_LIMIT, "a hit frees nothing, only replies do");
    }

    #[test]
    fn the_footer_reports_progress_and_survives_an_empty_library() {
        let mut footer = Footer::default();
        assert_eq!(footer.line(), "no index files yet");
        footer.months_total = 2;
        footer.months_scanned = 1;
        footer.scanning = true;
        assert_eq!(footer.line(), "1/2 month files · 0 rows indexed · last record never");
        footer.scanning = false;
        footer.rows = 9;
        footer.last = Some(at("2026-09-22_19-50-17"));
        assert_eq!(footer.line(), "2 month files · 9 rows indexed · last record 2026-09-22 19:50:17");
    }

    #[test]
    fn saving_a_bad_setting_is_reported_without_moving_the_effective_value() {
        let mut state = AppState::new(settings(), date(2026, 9, 22));
        state.draft.set_text(crate::settings::Field::MaxPageResult, "12");
        state.apply(AppEvent::SettingsSaved(Ok(PathBuf::from("userdata/config_user.json"))));
        assert_eq!(state.settings.max_page_result, 12);
        assert_eq!(state.search.params.page_size, 12);
        assert!(state.save_status.contains("wrote"), "{}", state.save_status);

        state.draft.set_text(crate::settings::Field::MaxPageResult, "nope");
        state.apply(AppEvent::SettingsSaved(Err("disk full".into())));
        assert_eq!(state.settings.max_page_result, 12, "a failed write must not restage the draft");
        assert!(state.save_status.contains("disk full"));
    }

    /// The AI page's write, with the one addition the other two forms do not need: the status line
    /// under it is a *derived* reading of the form, so a Save that changes the form has to invalidate
    /// the reading or the screen keeps answering a question nobody is still asking.
    #[test]
    fn a_saved_ai_write_promotes_its_values_and_ages_its_own_status_line() {
        let mut state = AppState::new(settings(), date(2026, 9, 22));
        state.tab = Tab::Ai;
        state.ai_draft.set_text(crate::ai::AField::Model, "windai-proof-model");
        state.ai_draft.set_text(crate::ai::AField::ApiKey, "sk-MODELTEST-0123456789");
        let (validated, notes) = state.ai_draft.validate(&state.ai);
        assert!(notes.is_empty(), "{notes:?}");
        state.pending_ai = Some(Box::new(validated.clone()));
        state.ai_status_for = 7;

        assert!(state.apply(AppEvent::AiSaved(Ok(PathBuf::from("userdata/config_user.json")))));
        assert_eq!(state.ai.model, "windai-proof-model", "the applied values moved with the write");
        assert_eq!(state.ai.key(), "sk-MODELTEST-0123456789", "including the key");
        assert!(state.save_status.contains("wrote"), "{}", state.save_status);
        assert_eq!(state.ai_draft.text(crate::ai::AField::Model), "windai-proof-model");
        assert_eq!(state.ai_draft.text(crate::ai::AField::ApiKey), "", "the box reopens empty, not prefilled");
        assert_eq!(state.ai_status_for, u64::MAX, "and the status line is marked stale for the next frame");

        // A failed write leaves the applied form exactly where it was, same rule as the other tabs.
        let mut failing = state.clone();
        failing.ai_draft.set_text(crate::ai::AField::BaseUrl, "https://typo.test");
        let unchanged = failing.ai.clone();
        failing.pending_ai = Some(Box::new(unchanged));
        assert!(failing.apply(AppEvent::AiSaved(Err("disk full".into()))));
        assert_eq!(failing.ai.model, "windai-proof-model", "a failed write must not restage the draft");
        assert!(failing.save_status.contains("disk full"), "{}", failing.save_status);
    }

    /// A click on a thumbnail opens the viewer immediately, in a "reading" state: the read can take a
    /// second when it has to seek the video, and a click that shows nothing is indistinguishable from a
    /// click that did nothing.
    #[test]
    fn a_thumbnail_click_opens_the_viewer_in_a_reading_state_and_its_reply_fills_it() {
        let mut state = AppState::new(settings(), date(2026, 9, 22));
        let card = card(7, 0);
        assert!(state.open_frame(&card), "the first click asks for the frame");
        let view = state.frame.clone().expect("opened");
        assert!(view.loading, "reading, until the reply says otherwise");
        assert!(!view.missing);
        assert_eq!(view.clock, card.clock, "the header names the row that was clicked");

        // A second click on the same row while it is still reading must not queue a second read.
        assert!(!state.open_frame(&card), "the read in flight is the read the user is waiting for");

        state.apply(AppEvent::Frame {
            key: card.key.clone(),
            source: Some(crate::backend::FrameSource::Screenshot),
            image: Some(DecodedImage { width: 2, height: 2, rgba: vec![0u8; 16] }),
        });
        let view = state.frame.clone().expect("still open");
        assert!(!view.loading && !view.missing);
        assert_eq!(view.source, Some(crate::backend::FrameSource::Screenshot));
    }

    /// A reply that arrives for a row the user has moved on from must not paint over the one they are now
    /// looking at — reads are asynchronous, and four workers can finish in any order.
    #[test]
    fn a_frame_reply_for_another_row_is_dropped() {
        let mut state = AppState::new(settings(), date(2026, 9, 22));
        let mine = card(11, 0);
        let other = card(12, 0);
        state.open_frame(&mine);
        assert!(!state.apply(AppEvent::Frame { key: other.key.clone(), source: None, image: None }), "nothing changed");
        assert!(state.frame.as_ref().unwrap().loading, "the stale failure must not mark the open row missing");

        state.close_frame();
        assert!(state.frame.is_none(), "closed");
        assert!(!state.apply(AppEvent::Frame { key: mine.key.clone(), source: None, image: None }), "a closed viewer is not reopened by a late reply");
    }

    /// "There is no original for this row" is a stated answer, and a stated answer is retryable: the video
    /// may be away because maintenance was rewriting it when the click landed.
    #[test]
    fn a_row_with_no_original_frame_says_so_and_can_be_asked_again() {
        let mut state = AppState::new(settings(), date(2026, 9, 22));
        let card = card(21, 0);
        state.open_frame(&card);
        state.apply(AppEvent::Frame { key: card.key.clone(), source: None, image: None });
        let view = state.frame.clone().expect("still open, to say why");
        assert!(view.missing && !view.loading);
        assert_eq!(view.source, None);
        assert!(state.open_frame(&card), "a gave-up viewer re-asks on the next click");
        assert!(!state.frame.as_ref().unwrap().missing, "and the retry starts clean");
    }

    /// A row the player can be asked about: a segment still on disk, and an offset inside it.
    fn row_with_segment(rowid: i64, offset: i64) -> RowCard {
        let mut card = card(rowid, at("2026-09-21_10-00-30"));
        card.segment_path = Some(PathBuf::from("userdata/videos/2026-09/2026-09-21_10-00-00.mp4"));
        card.offset = Some(offset);
        card
    }

    /// The viewer open on a playing row, with the stream's own stop flag installed as the live one.
    fn playing(rowid: i64, second: i64) -> (AppState, Arc<AtomicBool>) {
        let mut state = AppState::new(settings(), date(2026, 9, 22));
        let card = row_with_segment(rowid, 30);
        state.open_frame(&card);
        let run = Arc::new(AtomicBool::new(false));
        state.player = Some(Player { key: card.key.clone(), name: "2026-09-21_10-00-00.mp4".into(), at: second, duration: Some(183), waiting: true, failure: None });
        state.player_run = Some(run.clone());
        (state, run)
    }

    /// The viewer does not spawn a process and the state machine does not either: a seek is parked, and
    /// the frame that drains it mints the flag. That flag is minted *here* rather than in the widget so
    /// a test can say what the whole design rests on — one stream, one handle, and the handle travels
    /// with the request it answers.
    #[test]
    fn a_seek_parked_by_the_viewer_becomes_one_command_carrying_its_own_stop_flag() {
        let mut state = AppState::new(settings(), date(2026, 9, 22));
        let card = row_with_segment(41, 30);
        state.open_frame(&card);
        assert_eq!(state.frame.as_ref().expect("open").start, 30, "the viewer opens on the row's own second");

        state.pending_player = Some(PlayerRequest::Start(42));
        let command = state.take_player_request().expect("the frame drains the request");
        let Command::PlaySegment { key, segment, from, run } = command else {
            panic!("a parked seek is a play command, not {command:?}");
        };
        assert_eq!(key, card.key, "the command names the row, not just the file, because the answer arrives late");
        assert_eq!(Some(segment), card.segment_path);
        assert_eq!(from, 42);
        assert!(!run.load(Ordering::Relaxed), "a stream starts unstopped");
        assert!(state.pending_player.is_none(), "drained, not left to fire a second time");
        assert!(state.take_player_request().is_none(), "one request, one command");
    }

    /// A row whose footage has gone cannot be played, and the request for it dies rather than becoming a
    /// command with nothing to open.
    #[test]
    fn a_seek_on_a_row_with_no_segment_asks_for_nothing() {
        let mut state = AppState::new(settings(), date(2026, 9, 22));
        state.open_frame(&card(42, 0));
        state.pending_player = Some(PlayerRequest::Start(0));
        assert!(state.take_player_request().is_none());
    }

    /// The rule the `Arc` in every player reply exists for. A seek starts a new stream while the old one
    /// may still have a frame on its way, and the worker cannot be recalled: without the pointer
    /// comparison the window paints the second the user scrubbed away from, under a transport row that
    /// has already moved on.
    #[test]
    fn a_frame_from_a_superseded_stream_is_dropped_and_the_live_one_reaches_the_player() {
        let (mut state, live) = playing(43, 42);
        let replaced = Arc::new(AtomicBool::new(false));
        let key = state.frame.as_ref().expect("open").key.clone();
        let frame = |run: &Arc<AtomicBool>| AppEvent::PlayerFrame {
            run: run.clone(),
            key: key.clone(),
            at: 7,
            image: DecodedImage { width: 1, height: 1, rgba: vec![0, 0, 0, 0] },
        };

        assert!(!state.apply(frame(&replaced)), "a reply from a stream the user replaced is not an answer");
        let player = state.player.clone().expect("still playing");
        assert_eq!(player.at, 42, "and it did not move the picture");
        assert!(player.waiting, "or mark the wait over");
        assert_eq!(state.dropped_stale, 1);

        assert!(state.apply(frame(&live)), "the live stream's frame is a repaint");
        let player = state.player.clone().expect("still playing");
        assert_eq!(player.at, 7);
        assert!(!player.waiting, "one picture is the difference between \"opening\" and a picture");
    }

    /// A reply for another row is dropped too: the arrows walk while a segment runs, and the frames of
    /// the row that was left belong to a caption that no longer says where they came from.
    #[test]
    fn a_frame_for_another_row_is_dropped_even_from_the_live_stream() {
        let (mut state, live) = playing(44, 42);
        let other = RowKey::new("default_2026-09_wind.db", 999);
        assert!(!state.apply(AppEvent::PlayerFrame {
            run: live,
            key: other,
            at: 8,
            image: DecodedImage { width: 1, height: 1, rgba: vec![0, 0, 0, 0] },
        }));
        assert_eq!(state.player.clone().expect("still playing").at, 42);
    }

    /// Closing the viewer is the user's way of saying "stop", and it has to reach the process. The
    /// `Player` goes with the window; the flag that can interrupt the stream stays behind for exactly as
    /// long as it takes the frame to drain the request into a command.
    #[test]
    fn closing_the_viewer_parks_the_stop_that_kills_a_live_stream() {
        let (mut state, run) = playing(45, 42);
        state.close_frame();
        assert!(state.player.is_none(), "the transport row is gone with the viewer");
        assert!(state.player_run.is_some(), "the handle that ends the process is not");
        let command = state.take_player_request().expect("the close is delivered");
        let Command::StopSegment { run: drained } = command.clone() else {
            panic!("a parked stop is a stop command, not {command:?}");
        };
        assert!(Arc::ptr_eq(&run, &drained), "the live flag, not a fresh one that stops nothing");
    }

    #[test]
    fn closing_a_viewer_that_was_never_playing_parks_nothing() {
        let mut state = AppState::new(settings(), date(2026, 9, 22));
        state.open_frame(&row_with_segment(46, 30));
        state.close_frame();
        assert!(state.pending_player.is_none(), "there is no stream to stop");
        assert!(state.take_player_request().is_none());
    }

    /// A run that could not start keeps its `Player`, because the sentence is what the viewer paints in
    /// place of the picture. Dropping the player with the failure would drop the explanation with it and
    /// leave the still on screen as though nothing had been asked.
    #[test]
    fn a_stream_that_never_started_leaves_the_player_up_with_its_reason() {
        let (mut state, live) = playing(47, 42);
        let key = state.frame.as_ref().expect("open").key.clone();
        assert!(state.apply(AppEvent::PlayerDone { run: live, key: key.clone(), duration: None, failure: Some("no ffmpeg here".into()) }));
        let player = state.player.clone().expect("still there, to say why");
        assert_eq!(player.failure.as_deref(), Some("no ffmpeg here"));
        assert!(!player.waiting, "and nothing is coming, so the spinner goes");
        assert_eq!(player.duration, None, "a probe that failed leaves the ends unknown, not at zero");

        // The reply of a stream that has been replaced must not be the one that writes the sentence, and
        // the same pointer rule covers both directions.
        let (mut replaced_state, _) = playing(48, 42);
        assert!(!replaced_state.apply(AppEvent::PlayerDone {
            run: Arc::new(AtomicBool::new(false)),
            key,
            duration: Some(9),
            failure: None,
        }));
        assert_eq!(replaced_state.player.clone().expect("untouched").duration, Some(183));
    }

    #[test]
    fn the_ai_page_is_a_sixth_tab_and_no_other_tab_gained_a_field() {
        assert_eq!(Tab::ALL.len(), 6);
        assert_eq!(Tab::Ai.label(), "AI");
        // The two forms this branch already had are the ones whose documented field counts must not
        // move when a third arrives. Both moved anyway, one from each side of the merge, so the three
        // numbers below are measured off the arrays and not copied from the prose: `settings.rs` owns
        // fifteen, `record.rs` owns twenty-five (twenty-two recording keys plus the three that
        // schedule the idle pass), and `ai.rs` owns fifteen (the Lab page's seven, the tagger's two
        // switches, the bridge's five and the summariser's idle switch). The sentences in those files
        // and in `view.rs` that repeat these counts — the module headers, the `ALL` docs, the page
        // subtitles, the Save tooltips — are what these assertions exist to catch when they drift.
        assert_eq!(crate::settings::Field::ALL.len(), 15);
        assert_eq!(crate::record::RField::ALL.len(), 25);
        assert_eq!(
            crate::ai::AField::ALL.len(),
            15,
            "seven Lab keys, five for the bridge, one for the idle summariser, two for the tagger's own switches"
        );
    }
}
