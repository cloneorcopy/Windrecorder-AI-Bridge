//! The render tests: they call the real `egui` render loop offscreen and assert on what came out.
//!
//! Every assertion here is on the frame — the strings `epaint` laid out, the colour of each run
//! inside a label, the geometry of the shapes drawn, the repaint the integration was asked for — and
//! not merely on the state that produced them. A test that only checks `state.search.cards.len()`
//! would still pass if `view` forgot to draw the cards at all, which is the class of bug a UI
//! rewrite produces. Two of them caught exactly that on their first run: a Save button placed after
//! a scroll area that filled the window, and a detail pane whose body was laid out with a row limit
//! of zero. egui does not paint what a panel's fold hides, so both were invisible rather than wrong.
//!
//! `egui::Context::run` in 0.29 takes the viewport rectangle inside `RawInput` rather than as a
//! separate argument, so the helpers below always build the input with an explicit screen rect:
//! without it egui lays every panel out against an unbounded area, and the geometry assertions
//! would describe a window nobody could have.
//!
//! Two preconditions the harness itself has to satisfy before it can say anything true about a
//! frame. A screen that is meant to be showing rows needs an index for it to be showing rows at all
//! — `view` answers an empty library with onboarding rather than a grid, so a test that forgets
//! `state.months` asserts against a panel that is correctly empty. And a click needs three frames on
//! one `Context`; see [`Stage`].

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{Vec2, ViewportId};

use crate::app::App;
use crate::fixtures::{at, clock, date, Library};
use crate::flags::FlagNote;
use crate::model::{AppEvent, AppState, BucketCell, Command, DayOutcome, RowCard, RowKey, SearchOutcome, SearchParams, StripCell, Tab};
use crate::ai::AField;
use crate::record::RField;
use crate::settings::{Field, Settings};
use crate::textures::Cache;
use crate::view;
use wind_base::i18n::Catalog;
use wind_base::LocalParts;

/// The window the tests paint into. Fixed, so a geometry assertion means the same thing twice.
const WIDTH: f32 = 1400.0;
const HEIGHT: f32 = 900.0;

fn input() -> egui::RawInput {
    let mut input = egui::RawInput::default();
    input.screen_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(WIDTH, HEIGHT)));
    input
}

/// One offscreen frame of pure state: no threads, no files, no I/O expected.
fn frame(state: &mut AppState) -> egui::FullOutput {
    let (full, commands) = painted(state, input());
    assert!(
        commands.is_empty(),
        "a paint driven only by state must not ask for I/O: {commands:?}"
    );
    full
}

/// One frame on a context of its own, which is all a state-driven paint needs.
fn painted(state: &mut AppState, events: egui::RawInput) -> (egui::FullOutput, Vec<Command>) {
    Stage::new().paint(state, events)
}

/// A `Context` kept alive across frames, for the tests that point at something.
///
/// One `ctx.run` cannot deliver a click, and that is a fact about egui rather than a quirk to work
/// around: the hit test that turns a press into a widget id runs against the *previous* pass's
/// widget rects (`context.rs` → `hit_test(&viewport.prev_pass.widgets, ..)`), and a press only
/// becomes a click when the button is *released* (`interaction.rs`). A fresh context has no previous
/// pass, and a frame holding a press with no release holds no click — so a pointer test must span
/// frames the way a real window does, or it asserts on an event the application can never receive.
struct Stage {
    ctx: egui::Context,
    textures: Cache,
    frames: Cache,
}

impl Stage {
    fn new() -> Stage {
        Stage {
            ctx: egui::Context::default(),
            textures: Cache::new(),
            frames: Cache::with_cap(crate::textures::FRAME_CAP),
        }
    }

    fn paint(&mut self, state: &mut AppState, events: egui::RawInput) -> (egui::FullOutput, Vec<Command>) {
        let mut commands = Vec::new();
        let ctx = self.ctx.clone();
        let full = ctx.run(events, |ctx| view::paint(state, &mut self.textures, &mut self.frames, ctx, &mut commands));
        (full, commands)
    }

    /// Put the pointer somewhere and leave it there for a frame: the pass that makes the widget
    /// under it known to the next pass's hit test.
    fn hover(&mut self, state: &mut AppState, at: egui::Pos2) {
        let mut events = input();
        events.events = vec![egui::Event::PointerMoved(at)];
        self.paint(state, events);
    }

    /// Press and release, in separate frames, at a point. The commands of all three frames come back
    /// together, because a click that queued I/O would have queued it on the press.
    fn click(&mut self, state: &mut AppState, at: egui::Pos2) -> Vec<Command> {
        self.hover(state, at);
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let mut events = input();
        events.events = vec![egui::Event::PointerMoved(at), button(true)];
        let (_, mut commands) = self.paint(state, events);
        let mut release = input();
        release.events = vec![button(false)];
        commands.extend(self.paint(state, release).1);
        commands
    }
}

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
        exclude_words: vec!["KeePass".into()],
        ocr_image_crop_urbl: vec![6, 6, 6, 3],
        enable_ocr_str_highlight_indicator: true,
        thumbnail_generation_size_width: 70,
        close_window_to_tray: true,
        start_app_on_boot: false,
    }
}

fn state() -> AppState {
    let mut state = AppState::new(settings(), date(2026, 9, 22));
    state.today = date(2026, 9, 22);
    state.notice = None;
    // Every screen of this app is a view over an index, and `view` says so: with no month file to
    // read it paints the onboarding hint instead of a grid or a strip, which is the right answer for
    // a new install but not what a test about painted rows is asking about. A test that wants the
    // empty-library frame says so itself (see `an_empty_library_paints_an_onboarding_hint...`).
    state.months.push(month());
    state
}

fn month() -> wind_store::read::Month {
    wind_store::read::Month {
        user: "default".into(),
        year: 2026,
        month: 9,
        path: PathBuf::from("default_2026-09_wind.db"),
    }
}

fn card(rowid: i64, time: i64, body: &str) -> RowCard {
    let when = LocalParts::from_naive_epoch(time);
    RowCard {
        key: RowKey::new("default_2026-09_wind.db", rowid),
        time,
        clock: format!("{:02}:{:02}:{:02}", when.hour, when.minute, when.second),
        day: when.date_stamp(),
        title: Some("Notepad".into()),
        body: body.into(),
        segment: "2026-09-21_10-00-00.mp4".into(),
        offset: Some(time - at("2026-09-21_10-00-00")),
        deep_link: None,
        thumbnail: None,
        segment_path: Some(PathBuf::from("userdata/videos/2026-09/2026-09-21_10-00-00.mp4")),
        picture_path: None,
    }
}

fn day_outcome(cards: Vec<RowCard>, unindexed_video: bool) -> DayOutcome {
    DayOutcome {
        bounds: (clock(3, 0, 0), at("2026-09-22_02-59-59")),
        strip_span: (clock(3, 0, 0), at("2026-09-22_02-59-59")),
        active_hours: 0.0,
        cards,
        buckets: Vec::new(),
        strip: Vec::new(),
        titles: Vec::new(),
        flags: Vec::new(),
        unindexed_video,
        warnings: Vec::new(),
    }
}

/// Land a page of results the way a worker reply does, so the id and the paging are exercised.
fn deliver(state: &mut AppState, cards: Vec<RowCard>, params: SearchParams, total: i64, pages: usize) {
    let id = state.search.request_id;
    let terms = params.tokens();
    let outcome = SearchOutcome {
        cards,
        total,
        pages,
        elapsed_ms: 12,
        params: Box::new(params),
        terms,
    };
    assert!(state.apply(AppEvent::Search {
        request_id: id,
        outcome: Ok(outcome)
    }));
}

// ---------------------------------------------------------------------------------------------

#[test]
fn an_empty_library_paints_an_onboarding_hint_instead_of_panicking() {
    let mut state = state();
    state.months.clear();
    let full = frame(&mut state);
    let text = view::painted::joined(&full);
    assert!(text.contains("No index files yet"), "got: {text}");
    assert!(
        text.contains("Search") && text.contains("OneDay") && text.contains("Settings"),
        "the tab bar is always there: {text}"
    );
    assert!(text.contains("no index files yet"), "the footer agrees: {text}");
    assert!(view::painted::shape_count(&full) > 20, "the frame drew a window, not a blank");
}

#[test]
fn a_search_that_returns_rows_paints_one_card_each_with_its_own_clock_time() {
    let mut state = state();
    state.search.params.keywords = "revenue".into();
    let (id, params) = state.submit_search();
    assert_eq!(id, 1, "request ids start at one and rise");
    deliver(
        &mut state,
        vec![
            card(1, clock(10, 5, 30), "quarterly revenue summary"),
            card(2, clock(11, 0, 0), "revenue forecast"),
        ],
        params,
        2,
        1,
    );
    assert!(!state.search.pending, "the reply for the live id was folded in");

    let text = view::painted::joined(&frame(&mut state));
    assert!(text.contains("10:05:30"), "the card shows its own frame time: {text}");
    assert!(text.contains("11:00:00"), "{text}");
    assert!(text.contains("quarterly revenue summary"), "{text}");
    assert!(text.contains("2 of 2 results · page 1/1 · 12 ms"), "status line: {text}");
}

#[test]
fn a_matched_term_is_its_own_run_in_its_own_colour_inside_the_label() {
    let mut state = state();
    state.search.params.keywords = "revenue".into();
    let (_, params) = state.submit_search();
    deliver(&mut state, vec![card(1, clock(10, 0, 0), "quarterly revenue total")], params, 1, 1);

    let full = frame(&mut state);
    let jobs = view::painted::jobs(&full);
    let marked = jobs
        .iter()
        .find(|job| job.text.contains("revenue"))
        .expect("a label with the matched text was laid out");
    // Three sections: the text before the hit, the hit, the text after. One string with markup in it
    // would be one section, which is exactly what this asserts against.
    assert_eq!(marked.sections.len(), 3, "sections: {:?}", marked.sections);
    let colours: Vec<_> = marked.sections.iter().map(|s| s.format.color).collect();
    assert_eq!(colours[0], colours[2], "the two plain runs agree with each other");
    assert_ne!(colours[0], colours[1], "the matched run is a different colour");
    assert_eq!(colours[1], egui::Color32::from_rgb(255, 214, 102));
    assert_eq!(&marked.text[marked.sections[1].byte_range.clone()], "revenue");
    let text: String = marked.sections.iter().map(|s| &marked.text[s.byte_range.clone()]).collect();
    assert_eq!(text, "quarterly revenue total", "splitting the label must not lose a character");
}

#[test]
fn the_frame_can_draw_the_chinese_the_index_is_full_of() {
    // egui's own faces (Ubuntu-Light, Hack, an emoji font) contain no CJK codepoint at all, so a page
    // of Chinese OCR text draws as boxes unless the app installs a system face at startup. Stated as
    // an unconditional assertion because this is a Windows front end for a Chinese-language index: a
    // machine that cannot pass it is a machine where the app would show the user nothing but boxes,
    // which is the thing worth failing the build for.
    let body = "文件 ChatGPT 新聊天 置顶";
    let plain = egui::Context::default();
    // egui builds its `Fonts` lazily on the first pass, and refuses `ctx.fonts` before it. The
    // `FullOutput` is not the point of this call, so it is dropped rather than named.
    let _ = plain.run(input(), |_| {});
    assert!(
        !plain.fonts(|f| f.has_glyphs(&egui::FontId::proportional(14.0), body)),
        "the built-in faces must not be trusted with this"
    );
    let ctx = egui::Context::default();
    crate::install_cjk_fonts(&ctx);
    let _ = ctx.run(input(), |_| {});
    assert!(
        ctx.fonts(|f| f.has_glyphs(&egui::FontId::proportional(14.0), body)),
        "no face the app installed can draw 「{body}」; expected one of the files `backend::cjk_font` looks for"
    );
}

/// The repository root — two levels up from `windui` — which is the layout the shipped
/// `config_src/languages.json` sits beside. `App::new` resolves the same file from the install root;
/// these tests read it straight from the checkout so they exercise the real catalog, not a stub.
fn repo_root() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(std::path::Path::parent).map(PathBuf::from).unwrap()
}

/// The localisation must not be able to drift a word the ~40 other render tests assert on. Every tab's
/// `en` catalog copy is pinned to equal the neutral `Tab::label` the tests already trust.
#[test]
fn the_tab_labels_resolve_to_their_english_copy() {
    let state = state();
    for tab in Tab::ALL {
        assert_eq!(state.tr(tab.i18n_key()), tab.label(), "the en catalog drifted from {tab:?}'s label");
    }
}

/// The exactly-one-space guarantee, checked against the *resolved* catalog string rather than the key.
/// The original defect was a lost line-continuation `\` that made a sentence render with a run of
/// spaces; the copy now lives in `languages.json`, so this reads the catalog's answer and would still
/// catch a double space introduced during the move.
#[test]
fn the_resolved_search_help_has_no_run_of_spaces() {
    let state = state();
    let help = state.tr("windui_search_help");
    assert!(!help.contains("  "), "resolved Search help carries a run of spaces: {help:?}");
    assert_eq!(
        help,
        "Every whitespace-separated term must appear, in either the recognised text or the window \
         title; exclude terms remove rows. Each term is also tried as its shape-similar Chinese \
         variants when that setting is on.",
        "the catalog's en copy must be exactly the sentence this replaced"
    );
}

/// The load-bearing proof: a `sc` install paints Chinese, and the Chinese is the catalog's, not a box.
/// `App::new` installs the catalog from the resolved root and the user's `lang`; here we install the
/// shipped catalog at `sc` directly, so this is the same code path a real window takes.
#[test]
fn the_window_paints_chinese_when_the_locale_is_sc() {
    let mut state = state();
    state.catalog = Catalog::load(&repo_root(), "sc");
    assert!(state.catalog.loaded(), "{:?}", state.catalog.read_error);
    let full = frame(&mut state);
    let text = view::painted::joined(&full);
    assert!(text.contains("搜索"), "the Search tab painted in Chinese: {text}");
    assert!(text.contains("设置"), "the Settings tab painted in Chinese: {text}");
    assert!(!text.contains("Settings"), "the English Settings label did not survive the switch: {text}");
    // Those glyphs must be drawable by the face the app installs at startup — a tofu box is not a
    // rendered 搜. Combined with the assertions above this says the Chinese reached a paintable string.
    let ctx = egui::Context::default();
    crate::install_cjk_fonts(&ctx);
    let _ = ctx.run(input(), |_| {});
    assert!(
        ctx.fonts(|f| f.has_glyphs(&egui::FontId::proportional(14.0), "搜索设置")),
        "no face the app installed can draw the Chinese the sc catalog produced"
    );
}

/// A key nobody shipped must stay loud: the window renders `(key) not found in i18n…` rather than
/// silently falling back to English, so a missing translation is a bug the user can report. The tray's
/// `doctor` already behaves this way; this pins that the catalog move kept it for the window too, and
/// that it is per-key (the neighbouring tabs still resolve).
#[test]
fn a_window_key_no_one_shipped_still_paints_a_visible_marker() {
    let dir = std::env::temp_dir().join(format!("windui-missingkey-{}", crate::fixtures::next_scratch_id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("config_src")).unwrap();
    // Every tab key *except* the Search tab's, so exactly one label is missing.
    std::fs::write(
        dir.join("config_src/languages.json"),
        r#"{"en": {"windui_tab_oneday": "OneDay", "windui_tab_stat": "Stat", "windui_tab_recording": "Recording", "windui_tab_settings": "Settings", "windui_tab_ai": "AI"}, "sc": {}}"#,
    )
    .unwrap();
    let mut state = state();
    state.catalog = Catalog::load(&dir, "en");
    let text = view::painted::joined(&frame(&mut state));
    assert!(text.contains("(windui_tab_search) not found in i18n"), "a dropped key must render its own marker: {text}");
    assert!(text.contains("Settings"), "but a key that IS shipped still resolves: {text}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn selecting_a_card_moves_the_detail_pane_to_that_row() {
    let mut state = state();
    state.search.params.keywords = "screen".into();
    let (_, params) = state.submit_search();
    deliver(
        &mut state,
        vec![
            card(1, clock(10, 0, 0), "the first screen"),
            card(2, clock(11, 30, 0), "the second screen"),
        ],
        params,
        2,
        1,
    );
    assert_eq!(state.search.selected, Some(0), "a fresh result set selects its first row");

    let text = view::painted::joined(&frame(&mut state));
    assert_eq!(text.matches("the first screen").count(), 2, "card plus detail pane: {text}");
    assert_eq!(text.matches("the second screen").count(), 1);
    assert!(text.contains("0:00:00"), "the offset is formatted for a seek: {text}");

    state.select_search(1);
    let text = view::painted::joined(&frame(&mut state));
    assert_eq!(
        text.matches("the second screen").count(),
        2,
        "the pane followed the selection: {text}"
    );
    assert_eq!(text.matches("the first screen").count(), 1);
    assert!(text.contains("1:30:00"), "5400 s into the segment: {text}");

    // The arrow keys go through the same state transition a click does.
    state.move_search_selection(-1);
    let text = view::painted::joined(&frame(&mut state));
    assert_eq!(text.matches("the first screen").count(), 2, "{text}");
}

#[test]
fn a_card_with_no_segment_on_disk_offers_no_locate_and_says_why() {
    let mut state = state();
    state.search.params.keywords = "gone".into();
    let (_, params) = state.submit_search();
    let mut lost = card(1, clock(10, 0, 0), "a screen whose video was deleted");
    lost.segment_path = None;
    lost.deep_link = Some("https://example.com/keep".into());
    deliver(&mut state, vec![lost], params, 1, 1);

    let text = view::painted::joined(&frame(&mut state));
    assert!(text.contains("not on disk any more"), "{text}");
    assert!(!text.contains("Locate"), "no button for a file that is not there: {text}");
    assert!(text.contains("https://example.com/keep"), "the deep link is still shown: {text}");
}

#[test]
fn page_two_paints_different_rows_than_page_one() {
    // `2` is below the setting's own floor: the page size is clamped up where it is used rather than
    // trusted, so a hand-edited config cannot panic the UI or make the range empty.
    let lib = Library::empty("page-turn").with_config(r#"{"max_page_result": 2}"#);
    lib.month(
        "default",
        2026,
        9,
        &[
            ("2026-09-21_10-00-00.mp4", "2026-09-21_10-00-00", "needle alpha", "T1"),
            ("2026-09-21_10-01-00.mp4", "2026-09-21_10-01-00", "needle beta", "T2"),
            ("2026-09-21_10-02-00.mp4", "2026-09-21_10-02-00", "needle gamma", "T3"),
            ("2026-09-21_10-03-00.mp4", "2026-09-21_10-03-00", "needle delta", "T4"),
            ("2026-09-21_10-04-00.mp4", "2026-09-21_10-04-00", "needle epsilon", "T5"),
            ("2026-09-21_10-05-00.mp4", "2026-09-21_10-05-00", "needle zeta", "T6"),
        ],
    );
    let ctx = egui::Context::default();
    let mut app = App::new(lib.path().to_path_buf(), None, &ctx).expect("the app boots on the fixture");
    app.state.search.params.keywords = "needle".into();

    let (id, params) = app.state.submit_search();
    app.dispatch(Command::Search {
        request_id: id,
        params: Box::new(params),
    });
    let page1 = settle(&mut app, &ctx, |a| a.state.search.ran && !a.state.search.pending);
    let text1 = view::painted::joined(&page1);
    assert_eq!(app.state.settings.max_page_result, 2, "the config is read as written");
    assert_eq!(
        app.state.search.params.page_size, 5,
        "and clamped to the field floor where it is used"
    );
    assert_eq!(app.state.search.pages, 2, "six rows at five per page is two pages");

    let (id2, params2) = app.state.goto_page(2).expect("a second page exists");
    app.dispatch(Command::Search {
        request_id: id2,
        params: Box::new(params2),
    });
    let page2 = settle(&mut app, &ctx, |a| a.state.search.answered.as_ref().is_some_and(|p| p.page == 2));
    let text2 = view::painted::joined(&page2);

    assert!(text1.contains("needle alpha") && text1.contains("needle epsilon"), "{text1}");
    assert!(!text1.contains("needle zeta"), "page one holds only its five rows");
    assert!(text2.contains("needle zeta"), "{text2}");
    assert!(!text2.contains("needle alpha"), "and page two holds the sixth, not the first");
    assert!(text1.contains("page 1/2"), "{text1}");
    assert!(text2.contains("page 2/2"), "{text2}");
    assert_eq!(app.state.dropped_stale, 0, "both replies answered the request that asked");
}

#[test]
fn an_unindexed_day_and_an_empty_day_are_two_different_sentences() {
    let mut nothing = state();
    nothing.tab = Tab::OneDay;
    assert!(nothing.apply(AppEvent::Day {
        request_id: 1,
        date: date(2026, 9, 21),
        outcome: Ok(day_outcome(vec![], false))
    }));
    let text = view::painted::joined(&frame(&mut nothing));
    assert!(text.contains("no data for this day"), "{text}");
    assert!(!text.contains("not indexed"), "{text}");

    let mut unindexed = nothing.clone();
    assert!(unindexed.apply(AppEvent::Day {
        request_id: 2,
        date: date(2026, 9, 21),
        outcome: Ok(day_outcome(vec![], true)),
    }));
    let text = view::painted::joined(&frame(&mut unindexed));
    assert!(text.contains("recorded, but not indexed yet"), "{text}");
    assert!(
        !text.contains("no data for this day"),
        "the two states must not render the same words"
    );
}

#[test]
fn the_day_frame_draws_the_strip_the_area_and_the_side_panel_from_one_fetch() {
    let mut state = state();
    state.tab = Tab::OneDay;
    let rows = vec![
        card(1, clock(9, 0, 0), "quarterly revenue summary"),
        card(2, clock(9, 6, 0), "quarterly budget"),
        card(3, clock(17, 0, 0), "evening mail"),
    ];
    let buckets = (0..240)
        .map(|i| BucketCell {
            start: clock(3, 0, 0) + i * 360,
            // A genuinely busy morning. The chart draws a column only where the count is nonzero, so
            // a fixture that is empty apart from two buckets would leave the rect count below dominated
            // by panel backgrounds and this assertion would no longer be about the chart.
            count: if i < 120 { (i % 4) as usize + 1 } else { 0 },
            label: format!("{:02}:{:02}", 3 + i / 10, (i % 10) * 6),
        })
        .collect();
    let strip = vec![
        StripCell {
            from: clock(8, 0, 0),
            to: clock(13, 0, 0),
            time: Some(clock(9, 0, 0)),
            key: Some(RowKey::new("default_2026-09_wind.db", 1)),
            thumbnail: None,
            clock: Some("09:00:00".into()),
        },
        StripCell {
            from: clock(13, 0, 0),
            to: clock(18, 0, 0),
            time: Some(clock(17, 0, 0)),
            key: Some(RowKey::new("default_2026-09_wind.db", 3)),
            thumbnail: None,
            clock: Some("17:00:00".into()),
        },
    ];
    let mut outcome = day_outcome(rows, false);
    outcome.buckets = buckets;
    outcome.strip = strip;
    outcome.strip_span = (clock(8, 0, 0), clock(18, 0, 0));
    outcome.active_hours = 8.0;
    outcome.titles = vec![("Excel".into(), 3_720), ("Chrome".into(), 60)];
    outcome.flags = vec![FlagNote {
        when: "2026-09-21 09:00:00".into(),
        note: "keep this".into(),
        time: Some(clock(9, 0, 0)),
        index: 0,
        has_thumbnail: true,
    }];
    state.apply(AppEvent::Day {
        request_id: 1,
        date: date(2026, 9, 21),
        outcome: Ok(outcome),
    });

    let full = frame(&mut state);
    let text = view::painted::joined(&full);
    assert!(text.contains("OneDay"), "{text}");
    assert!(text.contains("3 rows"), "the header counts the fetched rows: {text}");
    assert!(
        text.contains("2026-09-21 03:00 → 2026-09-22 02:59"),
        "the day shown is the product-day: {text}"
    );
    assert!(text.contains("1:02:00"), "title totals are a duration: {text}");
    assert!(text.contains("keep this"), "the flag panel survives a long title list: {text}");

    // The strip's separators and the chart's outline are strokes; the chart's columns, the strip's
    // cells and the card frames are rectangles. Text alone would pass a much weaker test.
    let strokes = full
        .shapes
        .iter()
        .filter(|c| matches!(c.shape, egui::Shape::Path(_) | egui::Shape::LineSegment { .. }))
        .count();
    assert!(
        strokes >= 5,
        "the chart polyline, the scrub line and the strip marks are drawn: {strokes}"
    );
    let rects = full.shapes.iter().filter(|c| matches!(c.shape, egui::Shape::Rect(_))).count();
    assert!(rects > 20, "the busy chart columns plus the strip cells: {rects}");
}

#[test]
fn clicking_the_strip_at_a_time_selects_the_row_that_was_on_screen_then() {
    let mut state = state();
    state.tab = Tab::OneDay;
    let rows = vec![card(1, clock(9, 0, 0), "morning work"), card(2, clock(17, 0, 0), "evening mail")];
    let mut outcome = day_outcome(rows, false);
    outcome.strip_span = (clock(8, 0, 0), clock(18, 0, 0));
    outcome.buckets = vec![BucketCell {
        start: clock(9, 0, 0),
        count: 1,
        label: "09:00".into(),
    }];
    outcome.strip = vec![
        StripCell {
            from: clock(8, 0, 0),
            to: clock(13, 0, 0),
            time: Some(clock(9, 0, 0)),
            key: Some(RowKey::new("default_2026-09_wind.db", 1)),
            thumbnail: None,
            clock: Some("09:00:00".into()),
        },
        StripCell {
            from: clock(13, 0, 0),
            to: clock(18, 0, 0),
            time: Some(clock(17, 0, 0)),
            key: Some(RowKey::new("default_2026-09_wind.db", 2)),
            thumbnail: None,
            clock: Some("17:00:00".into()),
        },
    ];
    state.apply(AppEvent::Day {
        request_id: 1,
        date: date(2026, 9, 21),
        outcome: Ok(outcome),
    });
    assert_eq!(state.day.selected, Some(1), "the day opens on its newest row");

    // The strip's own rectangle as the painter measured it, so what follows is a statement about the
    // mapping rather than about the test author's arithmetic.
    let mut stage = Stage::new();
    stage.paint(&mut state, input());
    let [left, top, right, bottom] = state.strip_screen;
    assert!(right - left > 400.0, "the strip got a real width: {left}..{right}");
    let middle = (top + bottom) / 2.0;

    // A fifth of the way along a strip stretched over 08:00 → 18:00 is exactly 10:00, which is the
    // one pixel position whose time can be written down rather than measured back off the rect.
    stage.hover(&mut state, egui::pos2(left + (right - left) * 0.2, middle));
    assert_eq!(
        state.strip_hover,
        Some(clock(10, 0, 0)),
        "the readout names the pixel under the pointer"
    );

    let commands = stage.click(&mut state, egui::pos2(left + 2.0, middle));
    assert_eq!(state.day.scrub, clock(9, 0, 0), "a click near the start is inside the 09:00 span");
    assert_eq!(state.day.selected, Some(0), "and that sample is the morning row");
    assert!(commands.is_empty(), "a click on the strip is a selection, not an I/O request");

    stage.click(&mut state, egui::pos2(right - 2.0, middle));
    assert_eq!(state.day.scrub, clock(17, 0, 0), "the far end is the evening sample");
    assert_eq!(state.day.selected, Some(1));
}

#[test]
fn saving_a_setting_is_read_back_out_of_the_file_the_python_app_will_open() {
    // `user_name` is here because the assertion below needs a key to be *in* the file the app loaded:
    // `Config::save` writes a full snapshot, so an untouched key round-trips — but only if it was
    // ever there. A real install's `config_default.json` does carry it (as `"default"`).
    let lib = Library::empty("save").with_config(r#"{"max_page_result": 20, "day_begin_minutes": 180, "user_name": "default"}"#);
    let ctx = egui::Context::default();
    let mut app = App::new(lib.path().to_path_buf(), None, &ctx).expect("boots");
    assert_eq!(app.state.settings.max_page_result, 20);

    app.state.tab = Tab::Settings;
    app.state.draft.set_text(Field::MaxPageResult, "12");
    app.state.draft.set_text(Field::DayBeginMinutes, "60");
    app.state.draft.set_text(Field::OcrLang, "en-US");
    app.state.draft.set_text(Field::ExcludeWords, "KeePass\nVault\n");
    let before = frame_of(&mut app, &ctx);
    assert!(view::painted::joined(&before).contains("Settings these two screens read"));

    let (validated, notes) = app.state.draft.validate(&app.state.settings, &app.state.settings_options);
    assert!(notes.is_empty(), "{notes:?}");
    app.save(&validated);
    let text = view::painted::joined(&frame_of(&mut app, &ctx));
    assert!(text.contains("wrote"), "the status line names the file: {text}");
    assert!(text.contains("config_user.json"), "{text}");

    let reloaded = lib.reload();
    assert_eq!(reloaded.i64_or("max_page_result", 0), 12);
    assert_eq!(reloaded.i64_or("day_begin_minutes", 0), 60);
    assert_eq!(reloaded.str_or("ocr_lang", "?"), "en-US");
    assert_eq!(reloaded.str_list("exclude_words"), vec!["KeePass".to_string(), "Vault".to_string()]);
    // A key neither screen touches survives the write, because the file is a full snapshot.
    assert_eq!(reloaded.str_or("user_name", "?"), "default");
    // And the live state moved with it, which is what the next search pages by.
    assert_eq!(app.state.settings.max_page_result, 12);
    assert_eq!(app.state.search.params.page_size, 12);
}

/// The flag CSV the day panel reads and the `wind-notes` store writes, at a fixture's real path.
fn flag_csv(lib: &Library) -> PathBuf {
    lib.path().join("userdata").join("flag_mark_note.csv")
}

/// Boot the app over a scratch install whose flag table holds `body`, land on the day the flags are
/// on, and settle until the panel has them. Shared by the edit and delete proofs below.
fn app_with_flags(tag: &str, body: &str) -> (Library, egui::Context, App) {
    let lib = Library::empty(tag).with_config(r#"{"max_page_result": 20, "day_begin_minutes": 180, "user_name": "default"}"#);
    std::fs::write(flag_csv(&lib), body).expect("seed the flag table");
    let ctx = egui::Context::default();
    let mut app = App::new(lib.path().to_path_buf(), None, &ctx).expect("boots over the flag table");
    app.state.tab = Tab::OneDay;
    let (id, day) = app.state.set_day(date(2026, 9, 21));
    app.dispatch(Command::LoadDay { request_id: id, date: day });
    settle(&mut app, &ctx, |a| a.state.day.loaded && a.state.day.flags.len() == 3);
    (lib, ctx, app)
}

/// The edit proof, driven through the panel's own save path: a note is rewritten via `Command::EditFlag`
/// (what the 💾 button emits), and the CSV read back shows the new text with the row count and every
/// other row — quoting included — untouched.
#[test]
fn the_flag_panel_rewrites_one_note_through_its_own_save_path() {
    let (lib, ctx, mut app) = app_with_flags(
        "flag-edit",
        "thumbnail,datetime,note\nAAA,2026-09-21 09:00:00,\"keep, this\"\nBBB,2026-09-21 10:00:00,typo\nCCC,2026-09-21 11:00:00,\"say \"\"hi\"\"\"\n",
    );
    let target = app.state.day.flags.iter().find(|f| f.note == "typo").expect("the typo row is on the day").clone();
    let path = flag_csv(&lib);
    let before = std::fs::read_to_string(&path).unwrap();

    app.dispatch(Command::EditFlag {
        path: path.clone(),
        when: target.when.clone(),
        note: target.note.clone(),
        index: target.index,
        new_note: "corrected, now".to_string(),
    });
    // The save is synchronous in `dispatch`; a frame after it lets the panel refetch and settle.
    settle(&mut app, &ctx, |a| a.state.day.flags.iter().any(|f| f.note == "corrected, now"));

    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        after,
        "thumbnail,datetime,note\nAAA,2026-09-21 09:00:00,\"keep, this\"\nBBB,2026-09-21 10:00:00,\"corrected, now\"\nCCC,2026-09-21 11:00:00,\"say \"\"hi\"\"\"\n",
        "only the target row's note changed, and it gained pandas-style quoting for its new comma"
    );
    assert_eq!(before.lines().count(), after.lines().count(), "the row count is unchanged");
    // Every other physical line is byte-identical: only line 3 (the target) differs.
    let changed: Vec<&str> = before.lines().zip(after.lines()).filter(|(a, b)| a != b).map(|(_, b)| b).collect();
    assert_eq!(changed, vec!["BBB,2026-09-21 10:00:00,\"corrected, now\""], "no neighbour was rewritten: {after}");
}

/// The delete proof, split into the two things that make it safe. Arming a row (the first 🗑 click)
/// changes the frame but writes nothing; only the confirming "Yes" emits the delete, and the delete
/// takes exactly the flagged row while the survivors stay byte-for-byte.
#[test]
fn a_delete_writes_nothing_until_it_is_confirmed_then_takes_only_that_row() {
    let body = "thumbnail,datetime,note\nAAA,2026-09-21 09:00:00,\"keep, this\"\nBBB,2026-09-21 10:00:00,drop me\nCCC,2026-09-21 11:00:00,\"say \"\"hi\"\"\"\n";
    let (lib, ctx, mut app) = app_with_flags("flag-delete", body);
    let target = app.state.day.flags.iter().find(|f| f.note == "drop me").expect("the row to drop").clone();
    let path = flag_csv(&lib);

    // Arm: the row is now awaiting a second click. This is what a lone 🗑 press leaves behind.
    app.state.day.flag_confirm = Some(target.index);
    let armed = view::painted::joined(&frame_of(&mut app, &ctx));
    assert!(armed.contains("delete this flag?"), "the arming shows a confirm, not a done deal: {armed}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), body, "arming wrote nothing");
    assert_eq!(app.state.day.flags.len(), 3, "and the row is still listed");

    // Cancel is available and harmless: "No" clears the arm without deleting.
    app.state.day.flag_confirm = None;
    frame_of(&mut app, &ctx);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), body);

    // Confirm: the panel parks the delete only on "Yes", and `frame` drains it into one command.
    app.state.pending_flag_delete = Some(target.clone());
    settle(&mut app, &ctx, |a| a.state.day.loaded && a.state.day.flags.len() == 2);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "thumbnail,datetime,note\nAAA,2026-09-21 09:00:00,\"keep, this\"\nCCC,2026-09-21 11:00:00,\"say \"\"hi\"\"\"\n",
        "exactly the flagged row is gone; the survivors are byte-for-byte what they were"
    );
}

#[test]
fn a_clamped_setting_is_said_out_loud_in_the_frame_that_clamped_it() {
    let mut state = state();
    state.tab = Tab::Settings;
    state.draft.set_text(Field::MaxPageResult, "9999");
    state.draft.set_text(Field::TimelinePics, "1");
    let (validated, notes) = state.draft.validate(&state.settings, &state.settings_options);
    state.notes = notes;
    state.pending_settings = Some(Box::new(validated));
    let text = view::painted::joined(&frame(&mut state));
    println!(
        "[BEGIN]
{text}
[END]"
    );
    assert_eq!(state.pending_settings.as_ref().unwrap().max_page_result, 500);
    assert_eq!(state.pending_settings.as_ref().unwrap().oneday_timeline_pic_num, 50);
}

/// The Recording panel's honest notice: a config that promises browser deep links is a config the
/// native recorder cannot satisfy, and the only evidence of that today is one `eprintln!` in the
/// recorder going to a log file the supervisor truncates at every start — invisible to anyone who
/// launched from the tray. So the frame has to carry the sentence itself, and carry it only when the
/// promise was actually made.
#[test]
fn the_recording_panel_says_out_loud_what_the_native_recorder_cannot_do_with_deep_links() {
    let mut state = state();
    state.tab = Tab::Recording;

    state.rec.deep_linking_promised = true;
    let full = frame(&mut state);
    let text = view::painted::joined(&full);
    assert!(text.contains("record_deep_linking"), "the notice has to name the key: {text}");
    assert!(text.contains("no URL to reopen"), "and name what the user loses: {text}");
    assert!(text.contains("changes nothing about what is recorded"), "and refuse to imply a fix: {text}");

    // The panel's own warning colour, as a single run: it belongs up here with the header's warning
    // above the Save row, not down in the scroll area where it could be read as one more field label.
    let notice = view::painted::jobs(&full)
        .into_iter()
        .find(|job| job.text.contains("record_deep_linking"))
        .expect("the notice was laid out at all");
    assert_eq!(notice.sections.len(), 1, "one run, not a labelled mix: {:?}", notice.sections);
    assert_eq!(notice.sections[0].format.color, egui::Color32::from_rgb(214, 156, 74), "AMBER");

    // And nothing at all for a config that opted out. On its own this half proves little — a deleted
    // notice paints nothing either — but paired with the assertions above it pins the notice to the
    // flag rather than to the panel, which is the difference between a warning and a disclaimer.
    state.rec.deep_linking_promised = false;
    let quiet = view::painted::joined(&frame(&mut state));
    assert!(!quiet.contains("record_deep_linking"), "a recorder that promises nothing has nothing to explain: {quiet}");
    assert!(!quiet.contains("no URL to reopen"), "{quiet}");
}

/// The Recording tab still carries the switch that has a reader — `start_recording_on_startup`,
/// whether the tray launches a recorder at all — and names out loud that the battery gate a user may
/// have set through Python is inert on this engine. A test that only checked a widget existed would
/// be worthless; this pins each label that the control's round-trip test in `record.rs` proves is
/// actually read by `supervisor`.
#[test]
fn the_recording_panel_offers_the_startup_switch_and_names_the_inert_battery_gate() {
    let mut state = state();
    state.tab = Tab::Recording;

    let plain = view::painted::joined(&frame(&mut state));
    // The label is upstream's own row, `rs_checkbox_is_start_recording_on_start_app`, which the panel
    // now reads through the catalog so the tab follows the chosen language. "…when app started" is the
    // same claim in the wording the user's Chinese install already shows; `record.rs`'s round-trip test,
    // not this one, is what proves the control writes `start_recording_on_startup`.
    assert!(
        plain.contains("Start recording when app started"),
        "start_recording_on_startup has a control: {plain}"
    );

    // A config that asked for the battery gate is met with the honest notice, in the panel's own
    // warning colour as a single run — the same shape `record_deep_linking`'s notice is pinned to.
    state.rec.energy_saving_requested = true;
    let frame_out = frame(&mut state);
    let text = view::painted::joined(&frame_out);
    assert!(text.contains("convert_screenshots_to_vid_energy_saving_mode"), "the notice names the key: {text}");
    assert!(text.contains("changes nothing"), "and says plainly it is inert here: {text}");
    let notice = view::painted::jobs(&frame_out)
        .into_iter()
        .find(|job| job.text.contains("convert_screenshots_to_vid_energy_saving_mode"))
        .expect("the battery notice was laid out at all");
    assert_eq!(notice.sections.len(), 1, "one run, not a labelled mix: {:?}", notice.sections);
    assert_eq!(notice.sections[0].format.color, egui::Color32::from_rgb(214, 156, 74), "AMBER");

    // And nothing about it for a user who never asked for the gate — the notice is tied to the flag.
    state.rec.energy_saving_requested = false;
    let quiet = view::painted::joined(&frame(&mut state));
    assert!(!quiet.contains("convert_screenshots_to_vid_energy_saving_mode"), "no request, no notice: {quiet}");
}

/// Two controls were removed from the Recording tab for telling lies, and this pins them out. The
/// battery-gate radio (`windrec` and `windmaint` never read it) is gone from the page entirely, and
/// `ffmpeg` (the mode the native grabber cannot record) is gone from the record-mode option list. Put
/// either back and this fails — which is the standard: never leave a control that does nothing.
#[test]
fn the_recording_panel_no_longer_offers_the_battery_switch_or_ffmpeg() {
    let mut state = state();
    state.tab = Tab::Recording;
    let text = view::painted::joined(&frame(&mut state));
    assert!(!text.contains("Combine screenshots into video"), "the inert battery radio is gone: {text}");
    assert!(
        !RField::ALL.iter().any(|field| field.key() == "convert_screenshots_to_vid_energy_saving_mode"),
        "no editable field maps to the battery key either"
    );

    let options = crate::record::RecOptions::default();
    match RField::RecordMode.kind(&options, &state.rec) {
        crate::record::Kind::Choice(offered) => {
            assert_eq!(offered, vec!["screenshot_array".to_string()], "the page offers only what windrec records: {offered:?}");
        }
        other => panic!("record mode must still be a choice, was {other:?}"),
    }
}

/// The third control this tab has had to lose, and the guard against it creeping back:
/// `use_native_core`. It chose which of two implementations the tray launched; the Python application
/// was deleted in 3f37cbf, `supervisor/src/native.rs`'s `recorder_argv` and `ui_argv` now hand back a
/// native command whatever the file says, and the key's only remaining reader was this page writing
/// it. A checkbox whose whole effect is to persist a value nothing acts on is worse than no checkbox,
/// because the user believes they configured something.
///
/// `record.rs`'s `a_save_never_writes_the_dead_use_native_core_key` proves the key is not staged. This
/// proves it is not *painted* — the half a state-only test cannot see, since `stage` is reached from
/// the Save button and not from the frame. And because the control shared its section with the one
/// switch that does have a reader, the same frame has to keep painting that group and that control:
/// this is the difference between removing a field and deleting a section.
#[test]
fn the_recording_panel_paints_no_control_for_the_dead_use_native_core_key() {
    let mut state = state();
    state.tab = Tab::Recording;
    let text = view::painted::joined(&frame(&mut state));
    assert!(!text.contains("Use the native (Rust) core"), "the dead switch is back on the page: {text}");
    assert!(!text.contains("native (Rust) core"), "under no spelling of the label: {text}");
    assert!(!text.contains("use_native_core"), "nor as a bare key anywhere on the tab: {text}");
    assert!(
        !RField::ALL.iter().any(|field| field.key() == "use_native_core"),
        "no editable field may map to a key no binary reads"
    );

    // The group survives with its one real member, and the surviving control is still painted.
    assert!(text.contains("Engine and startup"), "the section is still headed: {text}");
    assert!(text.contains("Start recording when app started"), "and its reader-backed control is painted: {text}");
}

/// The claim in the user's own terms, proved on the real path rather than on `stage` directly: a
/// window booted on a scratch install, a setting changed through the draft, and the Save button's own
/// `App::save_recording` handing the merged map to `Config::save`. The bytes that land in
/// `userdata/config_user.json` must not mention `use_native_core`, because no binary in this
/// workspace reads it — and must still mention `start_recording_on_startup`, because `supervisor.rs`
/// does. Both halves are needed: a page that wrote nothing at all would pass the first assertion too.
///
/// Put the `config.set("use_native_core", …)` line back in `Rec::stage` and this fails on the raw
/// text, which is the mutation the assertion exists for.
#[test]
fn a_real_save_writes_the_startup_switch_and_never_the_dead_native_core_one() {
    let lib = Library::empty("save-native-core").with_config(
        r#"{"user_name": "default", "start_recording_on_startup": true, "vid_store_day": 1200}"#,
    );
    let ctx = egui::Context::default();
    let mut app = App::new(lib.path().to_path_buf(), None, &ctx).expect("boots");
    app.state.tab = Tab::Recording;
    frame_of(&mut app, &ctx);

    // Drive the page the way the user does: draft text, then the validation the Save button runs.
    app.state.rec_draft.set_text(RField::StartOnBoot, "false");
    app.state.rec_draft.set_text(RField::VidStoreDay, "900");
    let (validated, notes) = app.state.rec_draft.validate(&app.state.rec, &app.state.rec_options);
    assert!(notes.is_empty(), "{notes:?}");
    assert!(!validated.start_recording_on_startup);
    app.save_recording(&validated);

    let raw = std::fs::read_to_string(lib.root.join("userdata/config_user.json")).expect("Save wrote the file");
    assert!(!raw.contains("use_native_core"), "Save wrote a key nothing reads: {raw}");
    assert!(raw.contains("\"start_recording_on_startup\": false"), "the switch that IS read was written: {raw}");
    assert!(raw.contains("\"vid_store_day\": 900"), "and the page still saves normally: {raw}");

    let reloaded = lib.reload();
    assert!(!reloaded.bool_or("start_recording_on_startup", true), "supervisor.rs's own read sees it off");
}

/// The other half of the promise: this page shows the recorder's key but does not own it. Saving an
/// unrelated recorder setting must leave `record_deep_linking` in the user's file exactly as it was,
/// because a write-back of a carried field would switch a dead feature on without being asked.
#[test]
fn saving_the_recording_page_leaves_record_deep_linking_exactly_where_the_user_had_it() {
    let lib = Library::empty("save-deep")
        .with_config(r#"{"record_deep_linking": false, "vid_store_day": 1200, "user_name": "default"}"#);
    let ctx = egui::Context::default();
    let mut app = App::new(lib.path().to_path_buf(), None, &ctx).expect("boots");
    assert!(!app.state.rec.deep_linking_promised, "the page read the user's own no out of the file");

    app.state.tab = Tab::Recording;
    let before = view::painted::joined(&frame_of(&mut app, &ctx));
    assert!(!before.contains("record_deep_linking"), "and therefore painted no notice: {before}");

    app.state.rec_draft.set_text(RField::VidStoreDay, "900");
    let (validated, notes) = app.state.rec_draft.validate(&app.state.rec, &app.state.rec_options);
    assert!(notes.is_empty(), "{notes:?}");
    assert!(!validated.deep_linking_promised, "validation carries the flag through instead of inventing one");
    app.save_recording(&validated);
    let text = view::painted::joined(&frame_of(&mut app, &ctx));
    assert!(text.contains("config_user.json"), "the status line names the file: {text}");

    let reloaded = lib.reload();
    assert_eq!(reloaded.i64_or("vid_store_day", 0), 900, "the setting that was edited did land");
    assert!(!reloaded.bool_or("record_deep_linking", true), "the user's own false survived the write");
    assert_eq!(reloaded.str_or("user_name", "?"), "default", "and the keys neither page edits ride along");
    let raw = std::fs::read_to_string(lib.root.join("userdata/config_user.json")).unwrap();
    assert!(raw.contains("\"record_deep_linking\": false"), "not rewritten at all: {raw}");
    assert!(!app.state.rec.deep_linking_promised, "the live state agrees with the file afterwards");
}

#[test]
fn a_real_worker_reply_asks_for_a_repaint_and_a_stale_one_does_not() {
    let lib = Library::empty("thread").with_config(r#"{"max_page_result": 5}"#);
    lib.month(
        "default",
        2026,
        9,
        &[("2026-09-21_10-00-00.mp4", "2026-09-21_10-00-00", "needle here", "Notepad")],
    );
    lib.with_segment("2026-09-21_10-00-00.mp4");
    let ctx = egui::Context::default();
    let mut app = App::new(lib.path().to_path_buf(), None, &ctx).expect("boots");

    // The boot jobs are still running. Wait for the footer *and* the day, and check the frame that
    // says "nothing new" before anything else goes out: a reply that lands while a frame is being
    // painted is a repaint that frame legitimately has to ask for, so asserting `MAX` with any
    // worker still busy would be a coin flip about thread scheduling, not about the app.
    settle(&mut app, &ctx, |a| !a.state.footer.scanning && a.state.day.loaded && !a.state.day.pending);
    let text = view::painted::joined(&frame_of(&mut app, &ctx));
    assert!(text.contains("1 month files"), "the footer counted the file: {text}");
    assert!(text.contains("1 rows indexed"), "{text}");
    assert!(text.contains("last record 2026-09-21 10:00:00"), "{text}");

    let idle = frame_of(&mut app, &ctx);
    assert_eq!(
        idle.viewport_output[&ViewportId::ROOT].repaint_delay,
        Duration::MAX,
        "a frame with nothing new must not ask to be redrawn again"
    );

    app.state.search.params.keywords = "needle".into();
    let (id, params) = app.state.submit_search();
    app.dispatch(Command::Search {
        request_id: id,
        params: Box::new(params),
    });
    let after = settle(&mut app, &ctx, |a| a.state.search.ran && !a.state.search.pending);
    assert_eq!(
        after.viewport_output[&ViewportId::ROOT].repaint_delay,
        Duration::ZERO,
        "the frame that folded in the reply asked for the next one"
    );
    let text = view::painted::joined(&after);
    assert!(text.contains("10:00:00") && text.contains("needle here"), "{text}");
    assert!(text.contains("Locate"), "the segment the fixture put on disk is offerable: {text}");

    // A reply for a request that has since been replaced must not reach the screen.
    let (newer, _) = app.state.submit_search();
    let stale = AppEvent::Search {
        request_id: id,
        outcome: Ok(SearchOutcome {
            cards: vec![card(99, clock(23, 59, 0), "should never appear")],
            total: 1,
            pages: 1,
            elapsed_ms: 1,
            params: Box::new(SearchParams::default()),
            terms: vec![],
        }),
    };
    assert!(!app.state.apply(stale), "the stale reply reports no change");
    assert!(newer > id);
    let text = view::painted::joined(&frame_of(&mut app, &ctx));
    assert!(
        !text.contains("should never appear"),
        "a superseded reply leaked into the frame: {text}"
    );
    assert_eq!(app.state.dropped_stale, 1);
}

#[test]
fn a_refresh_notices_the_month_file_the_recorder_wrote_after_the_window_opened() {
    let lib = Library::empty("rescan").with_config(r#"{"max_page_result": 5}"#);
    let ctx = egui::Context::default();
    let mut app = App::new(lib.path().to_path_buf(), None, &ctx).expect("boots on an empty library");
    settle(&mut app, &ctx, |a| !a.state.footer.scanning);
    let text = view::painted::joined(&frame_of(&mut app, &ctx));
    assert!(
        text.contains("No index files yet"),
        "a brand-new install is told what is missing: {text}"
    );

    // The recorder gets to work while the window is open, and the footer's refresh is the answer the
    // onboarding screen itself points at — so it has to re-list the directory, not recount what the
    // boot listing already knew about.
    lib.month(
        "default",
        2026,
        9,
        &[("2026-09-21_10-00-00.mp4", "2026-09-21_10-00-00", "needle here", "Notepad")],
    );
    app.dispatch(Command::ScanLibrary);
    settle(&mut app, &ctx, |a| a.state.months.len() == 1 && !a.state.footer.scanning);
    let text = view::painted::joined(&frame_of(&mut app, &ctx));
    assert!(!text.contains("No index files yet"), "the screen is off the hint: {text}");
    assert!(text.contains("1 month files · 1 rows indexed"), "the footer counted it: {text}");

    // And the workers see it too: they query from `env`, not from the painted state.
    app.state.search.params.keywords = "needle".into();
    let (id, params) = app.state.submit_search();
    app.dispatch(Command::Search {
        request_id: id,
        params: Box::new(params),
    });
    let after = settle(&mut app, &ctx, |a| a.state.search.ran && !a.state.search.pending);
    let text = view::painted::joined(&after);
    assert!(
        text.contains("1 of 1 results"),
        "a query opened a file that did not exist at boot: {text}"
    );
    assert!(text.contains("needle here"), "{text}");
}

#[test]
fn the_thumbnail_pipeline_decodes_off_the_frame_and_uploads_once() {
    let lib = Library::empty("thumb").with_config(r#"{"max_page_result": 5}"#);
    lib.month_with_thumbnail(
        &[("2026-09-21_10-00-00.mp4", "2026-09-21_10-00-00", "needle here", "Notepad")],
        &a_thumbnail(),
    );
    let ctx = egui::Context::default();
    let mut app = App::new(lib.path().to_path_buf(), None, &ctx).expect("boots");
    app.state.search.params.keywords = "needle".into();
    let (id, params) = app.state.submit_search();
    app.dispatch(Command::Search {
        request_id: id,
        params: Box::new(params),
    });

    let key = RowKey::new("default_2026-09_wind.db", 1);
    // The card is painted before its pixels exist: the frame must complete rather than stall, and the
    // texture shows up on a later one.
    frame_of(&mut app, &ctx);
    settle(&mut app, &ctx, |a| a.has_texture(&key));
    assert_eq!(app.texture_size(&key), (70, 39), "the encoder box with the aspect kept");
    assert_eq!(app.decode_requests(), 1, "one row, one decode, however many frames asked for it");
}

/// The three reads a boot issues must not be able to hand each other a half-written index.
///
/// `App::new` sends the footer's scan and the first day, and a user who was already typing has a
/// search behind them: three jobs, four threads, and every one of them reads the same month file
/// through the one disposable `_TEMP_READ.db` copy beside it. Rebuilding that copy truncates it, so
/// the thread that loses the race either fails to open it at all or opens a copy whose schema pages
/// had landed and whose data pages had not — which SQLite reads as a month with no rows, without
/// saying so. That is the whole of `1 month files · 0 rows indexed` about a database that has one,
/// and of the search that then finds nothing and never asks for a thumbnail.
///
/// Each round is one roll of that dice, and a single round passed on the broken code as often as it
/// failed; the assertions are about the reads agreeing, which they cannot do if any one of them
/// read a torn copy.
#[test]
fn every_reader_at_boot_sees_the_rows_the_others_are_copying() {
    for round in 0..8 {
        let lib = Library::empty(&format!("bootrace-{round}")).with_config(r#"{"max_page_result": 5}"#);
        lib.month(
            "default",
            2026,
            9,
            &[("2026-09-21_10-00-00.mp4", "2026-09-21_10-00-00", "needle here", "Notepad")],
        );
        let ctx = egui::Context::default();
        let mut app = App::new(lib.path().to_path_buf(), None, &ctx).expect("boots");
        app.state.search.params.keywords = "needle".into();
        let (id, params) = app.state.submit_search();
        app.dispatch(Command::Search {
            request_id: id,
            params: Box::new(params),
        });
        settle(&mut app, &ctx, |a| {
            !a.state.footer.scanning && a.state.search.ran && !a.state.search.pending && a.state.day.loaded
        });

        assert_eq!(
            app.state.footer.error, None,
            "round {round}: the footer said a month would not open"
        );
        assert_eq!(app.state.footer.rows, 1, "round {round}: the footer's COUNT(*) read the month as empty");
        assert_eq!(
            app.state.dropped_stale, 0,
            "round {round}: a reply was folded in as stale, which is not what this race does"
        );
        assert_eq!(
            app.state.search.total, 1,
            "round {round}: the search answered for an empty month (error {:?})",
            app.state.search.error
        );
        assert_eq!(app.state.day.error, None, "round {round}: the day load reported the month as unreadable");
    }
}

/// A drain that receives a burst of decoded thumbnails uploads the whole burst.
///
/// The decodes run on four threads and the frame they land in takes whatever is queued, so a page of
/// them arrives together by design. Uploading only one per drain leaves the rest to be re-dispatched
/// next frame: the screen fills one card at a time, the same thumbnails are decoded over and over,
/// and an assertion about how many rows ended up decoded cannot see it.
#[test]
fn one_drain_uploads_every_thumbnail_in_the_burst_it_received() {
    const ROWS: i64 = 8;
    let lib = Library::empty("burst").with_config(r#"{"max_page_result": 20}"#);
    lib.month(
        "default",
        2026,
        9,
        &[("2026-09-21_10-00-00.mp4", "2026-09-21_10-00-00", "needle here", "Notepad")],
    );
    let ctx = egui::Context::default();
    let mut app = App::new(lib.path().to_path_buf(), None, &ctx).expect("boots");
    // Let the boot's own replies land first: the batch counted below must be nothing but decodes.
    settle(&mut app, &ctx, |a| !a.state.footer.scanning && a.state.day.loaded);

    let thumbnail = a_thumbnail();
    let cards: Vec<RowCard> = (0..ROWS)
        .map(|rowid| RowCard {
            thumbnail: Some(thumbnail.clone()),
            ..card(rowid, clock(9, 0, 0) + rowid * 60, &format!("needle row {rowid}"))
        })
        .collect();
    let (id, params) = app.state.submit_search();
    assert!(app.state.apply(AppEvent::Search {
        request_id: id,
        outcome: Ok(SearchOutcome {
            cards,
            total: ROWS,
            pages: 1,
            elapsed_ms: 3,
            params: Box::new(params),
            terms: vec!["needle".into()],
        }),
    }));

    // The frame that shows the page is the frame that asks for the decodes.
    frame_of(&mut app, &ctx);
    assert_eq!(app.state.in_flight.len(), ROWS as usize, "every card on the page was asked for");
    // Then stand still until they have all come back: eight sub-millisecond JPEG decodes on four
    // warm threads, so this wait is only the scheduler's, and nothing below is timed against it.
    std::thread::sleep(Duration::from_millis(400));
    frame_of(&mut app, &ctx);

    for rowid in 0..ROWS {
        assert!(
            app.has_texture(&RowKey::new("default_2026-09_wind.db", rowid)),
            "row {rowid}'s pixels arrived in this drain and were not uploaded"
        );
    }
    assert_eq!(app.decode_requests(), ROWS as usize, "one decode per row, none re-asked");
}

#[test]
fn a_page_of_four_hundred_results_paints_in_a_few_milliseconds() {
    let mut state = state();
    state.search.params.keywords = "needle".into();
    state.search.params.page_size = 400;
    let (_, params) = state.submit_search();
    let cards: Vec<RowCard> = (0..400)
        .map(|i| {
            card(
                i,
                clock(9, 0, 0) + i as i64 * 6,
                &format!("needle row {i} with some OCR text behind it"),
            )
        })
        .collect();
    let id = state.search.request_id;
    state.apply(AppEvent::Search {
        request_id: id,
        outcome: Ok(SearchOutcome {
            cards,
            total: 400,
            pages: 1,
            elapsed_ms: 40,
            params: Box::new(params),
            terms: vec!["needle".into()],
        }),
    });

    let ctx = egui::Context::default();
    let mut textures = Cache::new();
    let mut frames = Cache::with_cap(crate::textures::FRAME_CAP);
    let mut samples = Vec::new();
    for _ in 0..8 {
        let mut commands = Vec::new();
        let started = Instant::now();
        let full = ctx.run(input(), |c| view::paint(&mut state, &mut textures, &mut frames, c, &mut commands));
        assert!(view::painted::shape_count(&full) > 100, "the frame drew a page, not a blank");
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    let mut sorted = samples.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let (best, median) = (sorted[0], sorted[sorted.len() / 2]);
    eprintln!(
        "400-card page: best {best:.2} ms, median {median:.2} ms, of {} samples",
        samples.len()
    );

    let mut probe = state.clone();
    let full = frame(&mut probe);
    let text = view::painted::joined(&full);
    // The page holds 400 rows and the screen does not. Painting only the visible band is what keeps
    // the frame in budget, and the status line still says 400, because that is the truth about the
    // query rather than about the viewport.
    assert!(
        probe.painted_cards < 60,
        "{} of 400 cards were laid out: the grid is not virtualised",
        probe.painted_cards
    );
    let drawn: Vec<usize> = view::painted::strings(&full)
        .iter()
        .filter_map(|s| s.strip_prefix("needle row ")?.split(' ').next()?.parse().ok())
        .collect();
    let mut distinct = drawn.clone();
    distinct.sort_unstable();
    distinct.dedup();
    // Laid out and drawn are two different numbers, and the gap is egui's, not this grid's:
    // `show_rows` hands the closure the visible rows *plus* the ones its rounding overhangs the fold
    // by, and `egui::Label` then declines to paint itself once its rect misses the clip rect
    // (`Ui::is_rect_visible`). Requiring the two counts to be equal would require egui not to clip,
    // which is the opposite of the point of a scroll area. So: the drawn cards are a contiguous run
    // from the top of the page — no hole, which is what a band that skipped rows would leave — and
    // exactly one card is drawn twice, the selected one, whose whole body the detail pane also shows.
    assert_eq!(
        distinct,
        (0..distinct.len()).collect::<Vec<usize>>(),
        "drew a contiguous run from the top of the page: {drawn:?}"
    );
    assert_eq!(
        drawn.len(),
        distinct.len() + 1,
        "one card drawn twice (the selected one) and no other: {drawn:?}"
    );
    assert!(distinct.len() > 1, "the band really was drawn, not just measured: {drawn:?}");
    assert!(
        distinct.len() <= probe.painted_cards,
        "cannot draw more cards than the band laid out: {drawn:?}"
    );
    assert!(text.contains("400 of 400 results"), "the whole page is still accounted for: {text}");
    assert!(median < 8.0, "a steady-state frame took {median:.1} ms");
}

/// Nothing may paint outside the window.
///
/// A day whose timeline is crowded, a 40-character window title and 200 words of OCR text are the
/// inputs that used to push a label past a panel edge; the invariant worth keeping is not that any
/// particular widget lands at a particular pixel — that breaks on every layout change — but that
/// nothing escapes the viewport at all. A shape drawn beyond it is a scroll region that stopped
/// clipping, and the user's only clue is text they cannot read or click.
#[test]
fn a_crowded_day_paints_nothing_outside_the_viewport() {
    let mut state = state();
    state.tab = Tab::OneDay;
    let long = "x".repeat(40) + " " + &"word ".repeat(200);
    let rows = vec![card(1, clock(9, 0, 0), &long), card(2, clock(17, 0, 0), "evening mail")];
    let mut outcome = day_outcome(rows, false);
    outcome.strip_span = (clock(8, 0, 0), clock(18, 0, 0));
    outcome.buckets = vec![BucketCell { start: clock(9, 0, 0), count: 1, label: "09:00".into() }];
    outcome.titles = vec![("Excel".into(), 3720)];
    state.apply(AppEvent::Day { request_id: 1, date: date(2026, 9, 21), outcome: Ok(outcome) });

    let full = frame(&mut state);
    let painted = full
        .shapes
        .iter()
        .filter(|c| !matches!(c.shape, egui::Shape::Noop))
        .count();
    assert!(painted > 20, "the day view painted {painted} shapes, which is not a day");

    // Half a pixel of tolerance: a galley's visual bounds round up at the panel edge, and an
    // assertion without it would fail on a sub-pixel of anti-aliasing rather than on a bug.
    let limit = WIDTH + 0.5;
    let worst = full
        .shapes
        .iter()
        .map(|c| c.shape.visual_bounding_rect().right())
        .fold(0.0f32, f32::max);
    assert!(worst <= limit, "a shape reaches x={worst}, past the {WIDTH}-wide viewport");
}

// ---------------------------------------------------------------------------------------------
// helpers that reach into `App`, which these tests may do because they are inside the crate
// ---------------------------------------------------------------------------------------------

fn frame_of(app: &mut App, ctx: &egui::Context) -> egui::FullOutput {
    ctx.run(input(), |c| app.frame(c))
}

/// Paint frames until `until` holds — how an asynchronous worker is tested without a sleep that is
/// either too short to be useful or too long to be tolerable.
fn settle(app: &mut App, ctx: &egui::Context, until: impl Fn(&App) -> bool) -> egui::FullOutput {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let full = frame_of(app, ctx);
        if until(app) {
            return full;
        }
        assert!(Instant::now() < deadline, "the worker never answered");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A real base64 JPEG, made by the recorder's own encoder.
fn a_thumbnail() -> String {
    let rgb = vec![180u8; 320 * 180 * 3];
    wind_base::image::thumbnail_base64(&rgb, 320, 180, 70, 40).expect("encode")
}

// ---------------------------------------------------------------------------------------------
// AI — upstream's Lab tab, and the key it must never show
// ---------------------------------------------------------------------------------------------

/// The character egui substitutes for every character of a `password(true)` field at layout time. A
/// test that asserted on a literal bullet of its own would pass on a box that had simply lost its
/// text, so the same constant the masking code uses is what these assertions count.
const MASK: char = egui::epaint::text::PASSWORD_REPLACEMENT_CHAR;

/// A key worth hunting for, and a configuration that is complete apart from it — which is the state
/// every stock install is in, and the reason any test aimed at a *different* field has to supply one.
const PROOF_KEY: &str = "sk-FRAMEPROOF-0123456789abcdef";

fn ai_form() -> crate::ai::AiSettings {
    crate::ai::AiSettings::default()
}

fn ai_with_key(secret: &str) -> crate::ai::AiSettings {
    ai_form().with_key(secret)
}

/// The two pickers the Settings page was missing: which engine reads the screen, and which language the
/// product speaks. Both are painted from what the install can actually do, and the language row shows each
/// locale's name in its own script — a Chinese user has to be able to find 简体中文.
#[test]
fn the_settings_page_paints_the_engine_and_language_pickers() {
    let mut state = state();
    state.tab = Tab::Settings;
    state.settings_options = crate::settings::Options {
        engines: vec![
            wind_base::ocr::Choice { name: wind_base::ocr::WINDOWS_ENGINE.into(), available: true, detail: String::new() },
            wind_base::ocr::Choice { name: wind_base::ocr::TESSERACT_ENGINE.into(), available: true, detail: String::new() },
            wind_base::ocr::Choice { name: "PaddleOCR".into(), available: false, detail: "ran inside Python".into() },
        ],
        locales: vec![
            ("en".into(), "English".into()),
            ("sc".into(), "简体中文".into()),
            ("ja".into(), "日本語".into()),
        ],
        // Two panels, so a render test can prove the mask row covers the second screen and not just the
        // one the shipped default has a group for.
        mask_panels: vec![(1920, 1080), (2560, 1440)],
    };
    let text = view::painted::joined(&frame(&mut state));
    assert!(text.contains("Fifteen keys"), "the page still counts its fields: {text}");
    assert!(text.contains("Local OCR Engine"), "the engine row is labelled from the catalog: {text}");
    assert!(text.contains(wind_base::ocr::WINDOWS_ENGINE), "the stored engine is what the row shows: {text}");
    assert!(text.contains("English"), "the language row names its locale in its own script: {text}");
    // The refused engine is not an option, but it is named — so the user is not left wondering where the
    // engine their old install registered went.
    assert!(!text.contains(">PaddleOCR<"), "an undrivable engine is not offered: {text}");
    assert!(text.contains("PaddleOCR"), "and its absence is said out loud: {text}");
    // The mask row is the privacy control, and on a two-panel machine it has to show two groups: one row
    // of four boxes would let a person believe they had excluded the taskbar on both screens.
    assert_eq!(
        text.matches("#1 · 1920×1080").count() + text.matches("#2 · 2560×1440").count(),
        2,
        "one heading per screen the machine reports: {text}"
    );
    assert!(text.contains("Top") && text.contains("Left"), "the four edges are named: {text}");
}

/// The original-frame viewer, painted. A `Stage` spans passes because the overlay is an `Area` whose id
/// must be known to a pass before that pass's hit tests can reach it — the same reason the pointer tests
/// below hold a press and a release in separate frames.
#[test]
fn the_frame_viewer_says_what_it_is_showing() {
    let mut state = state();
    let card = card(31, clock(9, 0, 0), "a needle in the frame");
    state.open_frame(&card);

    let mut stage = Stage::new();
    stage.paint(&mut state, input());
    let reading = view::painted::joined(&stage.paint(&mut state, input()).0);
    assert!(reading.contains("reading the original frame"), "the click answers at once: {reading}");

    state.apply(AppEvent::Frame { key: card.key.clone(), source: None, image: None });
    stage.paint(&mut state, input());
    let gone = view::painted::joined(&stage.paint(&mut state, input()).0);
    assert!(gone.contains("No original frame"), "and when there is nothing to show, it says so: {gone}");

    state.open_frame(&card);
    state.apply(AppEvent::Frame {
        key: card.key.clone(),
        source: Some(crate::backend::FrameSource::Video),
        image: Some(crate::model::DecodedImage { width: 4, height: 4, rgba: vec![9u8; 4 * 4 * 4] }),
    });
    stage.paint(&mut state, input());
    let shown = view::painted::joined(&stage.paint(&mut state, input()).0);
    assert!(shown.contains("Frame taken out of the recorded video"), "where the picture came from is on screen: {shown}");
    // The pixels themselves are uploaded by `App`, which owns the `Context` — a headless paint has no
    // texture to show, and the viewer's honest answer for that is its own sentence, not a blank box.
    assert!(shown.contains("no longer loaded"), "and it says so rather than painting nothing: {shown}");
}

/// The player is offered by the row, not by the window: a segment on disk gets a button, and the row that
/// kept its screenshot while losing its footage says which of the two it is missing rather than showing a
/// control that cannot answer.
#[test]
fn the_viewer_offers_the_player_only_to_a_row_whose_segment_is_on_disk() {
    let mut state = state();
    let still = crate::model::DecodedImage { width: 6, height: 3, rgba: vec![7u8; 6 * 3 * 4] };

    let with = card(41, clock(10, 5, 0), "a needle with footage behind it");
    assert!(state.open_frame(&with));
    state.apply(AppEvent::Frame { key: with.key.clone(), source: Some(crate::backend::FrameSource::Screenshot), image: Some(still.clone()) });
    let mut stage = Stage::new();
    stage.paint(&mut state, input());
    let (full, commands) = stage.paint(&mut state, input());
    let text = view::painted::joined(&full);
    assert!(text.contains("Play"), "the row whose segment survived offers to move it: {text}");
    assert!(!text.contains("the video is not on disk"), "{text}");
    assert!(commands.is_empty(), "a play control nobody pressed asks for no I/O: {commands:?}");

    let mut without = card(42, clock(10, 6, 0), "a needle whose video was swept");
    without.segment_path = None;
    assert!(state.open_frame(&without));
    state.apply(AppEvent::Frame { key: without.key.clone(), source: Some(crate::backend::FrameSource::Screenshot), image: Some(still) });
    stage.paint(&mut state, input());
    let (full, commands) = stage.paint(&mut state, input());
    let text = view::painted::joined(&full);
    assert!(!text.contains("Play"), "a row with nothing to stream gets no button: {text}");
    assert!(text.contains("the video is not on disk any more"), "but it says what is missing: {text}");
    assert!(commands.is_empty(), "and neither does the sentence: {commands:?}");
}

/// The moving picture, painted. The viewer shows the second the stream reached and the length it is
/// inside of, and it shows *those* rather than the still the row was indexed from — which is the point of
/// the pause, and the reason the two live in separate slots of the frame cache.
#[test]
fn a_playing_segment_paints_its_own_picture_and_the_second_it_is_on() {
    let mut state = state();
    let row = card(43, clock(10, 7, 0), "a needle in the moving picture");
    assert!(state.open_frame(&row));
    // Both doors answered: the still is in the cache under the row's key, the stream's frame under the
    // player's own, and what the frame has to show is the second one.
    state.apply(AppEvent::Frame {
        key: row.key.clone(),
        source: Some(crate::backend::FrameSource::Screenshot),
        image: Some(crate::model::DecodedImage { width: 6, height: 3, rgba: vec![7u8; 6 * 3 * 4] }),
    });
    state.player = Some(crate::model::Player {
        key: row.key.clone(),
        name: "2026-09-21_10-00-00.mp4".into(),
        at: 42,
        duration: Some(183),
        waiting: false,
        failure: None,
    });
    state.player_run = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
    let mut stage = Stage::new();
    let ctx = stage.ctx.clone();
    stage.frames.insert(&ctx, &row.key, crate::model::DecodedImage { width: 6, height: 3, rgba: vec![7u8; 6 * 3 * 4] });
    stage.frames.insert(&ctx, &crate::model::player_key(), crate::model::DecodedImage { width: 4, height: 4, rgba: vec![9u8; 4 * 4 * 4] });

    stage.paint(&mut state, input());
    let (full, commands) = stage.paint(&mut state, input());
    let text = view::painted::joined(&full);
    assert!(text.contains("2026-09-21_10-00-00.mp4 · playing from +42s"), "the caption names the segment and the second: {text}");
    assert!(text.contains("183 s"), "and how long the thing is: {text}");
    assert!(text.contains("Frame"), "the toggle is labelled with what pressing it gives back: {text}");
    assert!(text.contains("Back to this row"), "and the way back to the moment that was clicked: {text}");
    assert!(!text.contains("1:1 (actual pixels)"), "the still's own controls are not what is on screen: {text}");
    assert!(!text.contains("opening the segment"), "waiting is over, so the waiting line is too: {text}");
    assert!(commands.is_empty(), "a player that is simply being looked at asks for nothing: {commands:?}");
}

/// Two more states the player has to say rather than paint: the second before the first frame, and the
/// run that never started. The rule is the one the HTML half holds to — a window that cannot show the
/// footage says why, in the colour it uses for "this did not work", and never a black box.
#[test]
fn a_player_that_is_opening_or_has_failed_says_which() {
    let mut state = state();
    let row = card(44, clock(10, 8, 0), "a needle behind a broken stream");
    state.open_frame(&row);
    state.apply(AppEvent::Frame {
        key: row.key.clone(),
        source: Some(crate::backend::FrameSource::Screenshot),
        image: Some(crate::model::DecodedImage { width: 6, height: 3, rgba: vec![7u8; 6 * 3 * 4] }),
    });
    let mut stage = Stage::new();

    state.player = Some(crate::model::Player {
        key: row.key.clone(),
        name: "2026-09-21_10-00-00.mp4".into(),
        at: 42,
        duration: None,
        waiting: true,
        failure: None,
    });
    state.player_run = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
    stage.paint(&mut state, input());
    let text = view::painted::joined(&stage.paint(&mut state, input()).0);
    assert!(text.contains("opening the segment"), "the click answers before the footage does: {text}");
    assert!(!text.contains("· playing from"), "there is no second to name yet: {text}");

    let live = state.player_run.clone().expect("the live run");
    state.apply(AppEvent::PlayerDone {
        run: live,
        key: row.key.clone(),
        duration: None,
        failure: Some("could not start ffmpeg: no such file".to_string()),
    });
    stage.paint(&mut state, input());
    let text = view::painted::joined(&stage.paint(&mut state, input()).0);
    assert!(text.contains("could not start ffmpeg: no such file"), "the reason is on screen, in the worker's own words: {text}");
    let job = view::painted::jobs(&stage.paint(&mut state, input()).0)
        .into_iter()
        .find(|job| job.text.contains("could not start ffmpeg"))
        .expect("the failure is laid out as text, not drawn as a box");
    assert_eq!(job.sections[0].format.color, egui::Color32::from_rgb(214, 156, 74), "amber, the colour the window uses for a thing that did not work");
    assert!(!text.contains("opening the segment"), "and the spinner is gone: {text}");
}

#[test]
fn the_ai_tab_is_a_sixth_screen_and_the_settings_page_still_counts_its_own_fields() {
    let mut state = state();
    assert_eq!(Tab::ALL.len(), 6, "Search, OneDay, Stat, Recording, Settings, AI");
    let bar = view::painted::joined(&frame(&mut state));
    assert!(bar.contains("AI"), "the tab is in the bar: {bar}");

    // Adding the sixth screen may not quietly widen the fifth one's contract. `settings.rs`'s header,
    // `Field`'s comment, the page's heading and its subtitle all state the same limit — "these two
    // screens", "fifteen keys" — and an API key on that page would make four documented claims false at
    // once. The tab bar carries the new screen; the old page does not.
    state.tab = Tab::Settings;
    let text = view::painted::joined(&frame(&mut state));
    assert!(text.contains("Settings these two screens read"), "{text}");
    assert!(text.contains("Fifteen keys"), "the page still counts its own fields: {text}");
    for key in ["open_ai_base_url", "open_ai_api_key", "open_ai_modelname", "ai_extract_max_tag_num"] {
        assert!(!text.contains(key), "an AI key leaked onto the Settings page as {key}: {text}");
    }
    assert_eq!(crate::settings::Field::ALL.len(), 15, "and the field list is still fifteen long");
    assert!(!crate::settings::Field::ALL.iter().any(|f| f.key().starts_with("open_ai")));
}

/// A state with the AI page's status lines filled in the way `App::frame` fills them, because a
/// frame-only `state()` has no `Config` to ask and would otherwise paint the `Default` ("not checked
/// yet") over a form that has clearly been filled in.
fn ai_state_with_status(dir: &std::path::Path) -> AppState {
    let mut state = state();
    state.tab = Tab::Ai;
    let config = wind_base::config::Config::load(dir).expect("the scratch install loads");
    state.ai = crate::ai::AiSettings::load(&config);
    state.ai_draft = crate::ai::AiDraft::from(&state.ai);
    let (effective, _) = state.ai_draft.validate(&state.ai);
    state.ai_status = crate::model::AiStatus {
        verdict: crate::ai::verdict(&config, &effective),
        key: crate::ai::key_state(&config, &effective),
    };
    state
}

#[test]
fn the_ai_page_paints_seven_fields_and_windais_own_verdict() {
    let dir = scratch_install("ai-verdict");
    let mut state = ai_state_with_status(&dir);
    let text = view::painted::joined(&frame(&mut state));
    for label in [
        "Endpoint dialect",
        "Base URL",
        "Model name",
        "API key",
        "Window titles per day",
        "Tags kept per day",
        "AI filter words",
        // The two tagger switches `windmaint` gates the idle pass on. `AField::ALL` gained them and
        // `view.rs` paints that list, so a row that reached the file but not the screen is caught here
        // rather than by somebody noticing the page has no door on a pass that spends money.
        "Extract activity tags with AI",
        "Let the tagger run during the idle pass",
    ] {
        assert!(text.contains(label), "{label} has no row: {text}");
    }
    assert!(text.contains("Monthly activity tags"), "the second group is headed: {text}");
    // Above the fold, for the reason `settings.rs` gives twice: a scroll area that will not shrink
    // swallows everything after it, and a button below the fold is a button that does not exist.
    assert!(text.contains("Test connection"), "the button is above the fold: {text}");
    assert!(text.contains("Save"), "{text}");
    assert!(text.contains("windai"), "and the page says who reads this: {text}");

    // The verdict is `require_usable`'s sentence, not a paraphrase, and on a stock install the honest
    // answer is that there is no key — which is exactly the state `windai doctor` reports today with
    // nothing on this screen to fix it with.
    assert!(text.contains("open_ai_api_key"), "the verdict names the key to edit: {text}");
    assert!(text.contains(wind_ai::KEY_PLACEHOLDER), "and names the string to replace: {text}");
    assert!(!text.contains('✓'), "a placeholder key is not \"ready\": {text}");
    let _ = std::fs::remove_dir_all(dir);
}

/// The requirement, painted. A recognisable secret is in the form, was accepted by the form, and is
/// nowhere in the frame the form produced — not as a value, not in a tooltip, not in a "current value"
/// field, and not in a note about a field that had to be corrected.
#[test]
fn a_key_typed_into_the_ai_page_appears_nowhere_in_the_frame_it_painted() {
    let dir = scratch_install("ai-frame");
    let config = wind_base::config::Config::load(&dir).unwrap();
    let mut state = ai_state_with_status(&dir);
    state.ai = state.ai.with_key("");
    state.ai_draft = crate::ai::AiDraft::from(&state.ai);
    // Exactly the call the masked `TextEdit` makes on every keystroke.
    state.ai_draft.set_text(crate::ai::AField::ApiKey, PROOF_KEY);
    state.ai_draft.set_text(crate::ai::AField::BaseUrl, "https://gateway.test/v1");
    state.ai_draft.set_text(crate::ai::AField::Model, "some-model");

    let (validated, notes) = state.ai_draft.validate(&state.ai);
    assert_eq!(validated.key(), PROOF_KEY, "the form did accept what was typed");
    state.ai_notes = notes;
    state.ai_status = crate::model::AiStatus {
        verdict: crate::ai::verdict(&config, &validated),
        key: crate::ai::key_state(&config, &validated),
    };
    assert!(state.ai_status.verdict.ok, "{:?}", state.ai_status.verdict);

    let full = frame(&mut state);
    let text = view::painted::joined(&full);
    assert!(!text.contains(PROOF_KEY), "the frame leaked the key: {text}");
    // The box is holding the text rather than having lost it: egui substitutes one bullet per
    // character of a password field, and those bullets — and only they — are what the frame carries.
    assert_eq!(text.matches(MASK).count(), PROOF_KEY.chars().count(), "the field is not masked, it is empty: {text}");
    assert!(text.contains("some-model"), "the other fields are painted in plain text: {text}");
    assert!(text.contains("https://gateway.test/v1"), "{text}");
    assert!(text.contains("fingerprint"), "the state line says what is stored: {text}");
    assert!(view::painted::shape_count(&full) > 20, "the frame drew a page, not a blank");

    // And the same secret after a Save and a re-read of the page — the second half of the claim, since
    // a field that masks on the way in and reveals on the way out is still a leak.
    let mut reloaded = state.clone();
    reloaded.ai = validated;
    reloaded.ai_draft = crate::ai::AiDraft::from(&reloaded.ai);
    reloaded.ai_notes.clear();
    reloaded.ai_status.key = crate::ai::key_state(&config, &reloaded.ai);
    let text = view::painted::joined(&frame(&mut reloaded));
    assert!(!text.contains(PROOF_KEY), "the re-read page leaked the key: {text}");
    assert_eq!(text.matches(MASK).count(), 0, "and it must not carry a stale mask either: {text}");
    assert!(text.contains("fingerprint"), "the stored key is still reported, by hash only: {text}");
    let _ = std::fs::remove_dir_all(dir);
}

/// The complaint this pins, on the tab that raised it: the interface was switched to Chinese and the
/// Recording page stayed English.
///
/// The expectation is read out of the catalog rather than written here, because a hardcoded 每段录像的秒数
/// would fail the day somebody improved the wording and pass silently the day a key stopped being read —
/// both the wrong lesson. What is pinned is the structure: the label the tab paints is the one the
/// installed locale's catalog holds for that field's key, and it is not the English fallback the
/// descriptor carries.
#[test]
fn the_recording_tab_is_painted_in_the_installed_locale() {
    let mut state = state();
    state.tab = Tab::Recording;
    let english = view::painted::joined(&frame(&mut state));
    assert!(
        english.contains(RField::RecordSeconds.label()),
        "the default locale paints the descriptors' own English: {english}"
    );

    state.catalog = Catalog::load(&repo_root(), "sc");
    assert!(state.catalog.loaded(), "{:?}", state.catalog.read_error);
    let chinese = view::painted::joined(&frame(&mut state));
    for field in [RField::RecordMode, RField::RecordSeconds, RField::CompressCpuThreads] {
        let label = state.catalog.text(field.label_key());
        assert!(!label.is_empty(), "{} resolved to nothing", field.key());
        assert_ne!(label, field.label(), "the sc catalog has no row for {}, so the tab could not tell English from Chinese", field.label_key());
        assert!(chinese.contains(&label), "{label:?} ({}) was not painted: {chinese}", field.key());
        assert!(!chinese.contains(field.label()), "{} survived the switch to Chinese: {chinese}", field.label());
    }
    let heading = state.catalog.text(RField::RecordMode.group_key());
    assert!(!heading.is_empty() && heading != RField::RecordMode.group(), "the section heading is not translated either: {heading:?}");
    assert!(chinese.contains(&heading), "the Capture heading did not follow the locale: {chinese}");
}

/// The same claim for the AI page's bridge rows — the newest fields on any form, and so the ones most
/// likely to have been given a `label_key` nobody shipped a row for.
#[test]
fn the_ai_tab_paints_the_bridge_rows_in_the_installed_locale() {
    let mut state = state();
    state.tab = Tab::Ai;
    state.catalog = Catalog::load(&repo_root(), "sc");
    assert!(state.catalog.loaded(), "{:?}", state.catalog.read_error);
    let text = view::painted::joined(&frame(&mut state));
    for field in [AField::McpEnabled, AField::McpHost, AField::McpPort, AField::McpToken] {
        let label = state.catalog.text(field.label_key());
        assert_ne!(label, field.label(), "the sc catalog has no row for {}", field.label_key());
        assert!(text.contains(&label), "{label:?} ({}) was not painted: {text}", field.key());
        assert!(!text.contains(field.label()), "{} survived the switch to Chinese: {text}", field.label());
    }
    let heading = state.catalog.text(AField::McpPort.group_key());
    assert!(!heading.is_empty() && heading != AField::McpPort.group(), "the bridge heading is not translated: {heading:?}");
    assert!(text.contains(&heading), "the MCP bridge heading did not follow the locale: {text}");
}

/// The round trip that actually matters, done inside the crate: a window booted on a scratch install,
/// the values driven through the draft the widgets write to, the Save button's own `App::save_ai`
/// handing the merged map to `Config::save`, and then **`windai`'s own `Index::open`** reading the
/// file back. A page that wrote a key `windai` does not read is the ninth instance of the defect this
/// branch has fixed eight times, and this is the test that would notice.
#[test]
fn saving_the_ai_page_writes_the_file_windai_opens() {
    const ROUND_TRIP: &str = "sk-ROUNDTRIP-0123456789abcdef";
    let dir = scratch_install("ai-round-trip");
    let ctx = egui::Context::default();
    let mut app = App::new(dir.clone(), None, &ctx).expect("the app boots on the scratch install");
    let before = wind_ai::library::Index::open(&dir).expect("windai can read the install");
    assert!(!before.settings.key_configured(), "a stock install holds no key, which is the whole point");
    assert_eq!(before.settings.base_url, "https://api.openai.com/v1", "and its endpoint is OpenAI's");
    assert!(!before.settings.model.is_empty());

    app.state.tab = Tab::Ai;
    frame_of(&mut app, &ctx);
    app.state.ai_draft.set_text(crate::ai::AField::BaseUrl, "http://127.0.0.1:8931/v1");
    app.state.ai_draft.set_text(crate::ai::AField::Model, "windui-proof-model");
    app.state.ai_draft.set_text(crate::ai::AField::ApiKey, ROUND_TRIP);
    app.state.ai_draft.set_text(crate::ai::AField::TitleLimit, "44");
    app.state.ai_draft.set_text(crate::ai::AField::MaxTags, "7");
    app.state.ai_draft.set_text(crate::ai::AField::FilterWords, "季度营收\nKeePass\n");
    let (validated, notes) = app.state.ai_draft.validate(&app.state.ai);
    assert!(notes.is_empty(), "{notes:?}");
    app.save_ai(&validated);
    let text = view::painted::joined(&frame_of(&mut app, &ctx));
    assert!(text.contains("wrote"), "the status line names the file: {text}");
    assert!(!text.contains(ROUND_TRIP), "and does not name the key: {text}");

    // The consumer's own reader, over the file the UI just wrote.
    let after = wind_ai::library::Index::open(&dir).expect("windai can read what the UI wrote");
    assert_eq!(after.settings.base_url, "http://127.0.0.1:8931/v1");
    assert_eq!(after.settings.model, "windui-proof-model");
    assert!(after.settings.key_configured(), "windai now holds a key the user typed");
    assert_eq!(after.settings.wintitle_limit, 44);
    assert_eq!(after.settings.max_tag_num, 7);
    // Sorted and de-duplicated, exactly as `exclude_words` on the Settings page is: the list is a set
    // of substrings to cut, and `tags::filter_words` applies them in order, so a stable order is the
    // one thing that makes two installs with the same words behave the same.
    assert_eq!(after.settings.filter_words, vec!["KeePass".to_string(), "季度营收".to_string()]);
    assert_eq!(after.settings.chat_completions_url(), "http://127.0.0.1:8931/v1/chat/completions");
    let faults = wind_ai::error::Faults::new(&wind_ai::SecretKey::new(ROUND_TRIP));
    after.settings.require_usable(&faults).expect("the configuration is now usable end to end");
    let _ = std::fs::remove_dir_all(dir);
}

/// The Settings page has always promised to write back only what it owns, and `record.rs` repeats the
/// promise over its own twenty-five keys. This page makes it over a larger set still — the other two
/// forms' keys *and* the AI keys it deliberately refuses — because a settings screen that clobbers a
/// neighbour is worse than one that offers nothing.
#[test]
fn an_ai_save_leaves_every_key_it_does_not_own_exactly_as_it_was() {
    const SURVIVE: &str = "sk-SURVIVE-0123456789abcdef";
    let dir = scratch_install("ai-survive");
    std::fs::write(
        dir.join("userdata/config_user.json"),
        serde_json::json!({
            "user_name": "kept-me",
            "max_page_result": 33,
            "oneday_timeline_pic_num": 77,
            "exclude_words": ["KeePass", "Vault"],
            "enable_ai_extract_tag": true,
            "enable_ai_extract_tag_in_idle": false,
            "ai_extract_tag_in_idle_batch_size": 44,
            "enable_img_embed_search": true,
            "img_embed_module_install": true,
            "ai_extract_tag_result_dir": "result_ai_extract_tag",
        })
        .to_string(),
    )
    .expect("user file");
    let ctx = egui::Context::default();
    let mut app = App::new(dir.clone(), None, &ctx).expect("boots over the user file");
    assert_eq!(app.state.settings.max_page_result, 33, "the Settings page read the user's own value");
    assert!(app.state.ai.image_search_promised, "and the AI page noticed the embedding promise");
    app.state.tab = Tab::Ai;
    let painted = view::painted::joined(&frame_of(&mut app, &ctx));
    assert!(painted.contains("enable_img_embed_search"), "and said so out loud: {painted}");
    assert!(painted.contains("no native image-embedding index"), "{painted}");

    app.state.ai_draft.set_text(crate::ai::AField::BaseUrl, "https://second.test/v1");
    app.state.ai_draft.set_text(crate::ai::AField::ApiKey, SURVIVE);
    let (validated, notes) = app.state.ai_draft.validate(&app.state.ai);
    assert!(notes.is_empty(), "{notes:?}");
    app.save_ai(&validated);

    let back = wind_base::config::Config::load(&dir).unwrap();
    assert_eq!(back.str_or("user_name", "?"), "kept-me", "an unrelated key was lost");
    assert_eq!(back.i64_or("max_page_result", 0), 33, "the Settings page's key was rewritten");
    assert_eq!(back.i64_or("oneday_timeline_pic_num", 0), 77, "and its sibling too");
    assert_eq!(back.str_list("exclude_words"), vec!["KeePass".to_string(), "Vault".to_string()]);
    assert!(back.bool_or("enable_ai_extract_tag", false), "an inert switch was rewritten");
    assert!(!back.bool_or("enable_ai_extract_tag_in_idle", true), "and the other one");
    assert_eq!(back.i64_or("ai_extract_tag_in_idle_batch_size", 0), 44, "the batch size was rewritten");
    assert!(back.bool_or("enable_img_embed_search", false), "the embedding promise was rewritten");
    assert!(back.bool_or("img_embed_module_install", false), "and its gate too");
    assert_eq!(back.str_list("ai_api_endpoint_type"), vec!["OpenAI compatible".to_string()]);
    assert_eq!(back.str_or("ai_extract_tag_result_dir", "?"), "result_ai_extract_tag");
    assert_eq!(back.str_or("open_ai_base_url", "?"), "https://second.test/v1", "the key edited here did land");
    assert_eq!(back.str_or("open_ai_api_key", "?"), SURVIVE);
    let _ = std::fs::remove_dir_all(dir);
}

/// Test connection, over a real socket, on the real path.
///
/// The canned listener answers 401 with a body that reflects the request's own `Authorization` header
/// back — the shape `wind_ai::error::redact` exists for — so one frame has to be true about two
/// opposite things at once: the key genuinely crossed a socket (the server says so) and the key
/// appears nowhere the user can read (the report says so). A stub transport could prove the second and
/// never the first, which is why `wind-ai`'s own tests use a listener too.
#[test]
fn a_test_connection_click_sends_a_real_request_and_reports_it_without_the_key() {
    const LIVE: &str = "sk-LIVEPROOF-0123456789abcdef";
    let server = crate::fixtures::Canned::reflecting(401);
    let dir = scratch_install("ai-live");
    let ctx = egui::Context::default();
    let mut app = App::new(dir.clone(), None, &ctx).expect("boots");

    app.state.tab = Tab::Ai;
    app.state.ai_draft.set_text(crate::ai::AField::BaseUrl, &server.url());
    app.state.ai_draft.set_text(crate::ai::AField::Model, "gpt-4o");
    app.state.ai_draft.set_text(crate::ai::AField::ApiKey, LIVE);
    let (validated, notes) = app.state.ai_draft.validate(&app.state.ai);
    assert!(notes.is_empty(), "{notes:?}");
    let (id, settings) = app.state.begin_ai_test(validated).expect("nothing is in flight");
    app.dispatch(Command::TestAi { request_id: id, settings: Box::new(settings) });
    let full = settle(&mut app, &ctx, |a| a.state.ai_test.report.is_some());
    let text = view::painted::joined(&full);

    assert!(server.saw("POST /v1/chat/completions"), "the request never reached the endpoint");
    assert!(server.saw(&format!("Bearer {LIVE}")), "the key is what makes it a real request");
    assert!(!text.contains(LIVE), "and the frame must not carry it: {text}");
    assert!(text.contains("401"), "the frame does say what came back: {text}");
    assert!(text.contains(wind_ai::error::REDACTED), "with the key removed: {text}");
    assert!(!app.state.ai_test.track.pending, "the reply closed the request it answered");
    // The failure is painted in the panel's own warning colour, as one run, so it reads as an answer
    // rather than as another field label in the list above it.
    let report = view::painted::jobs(&full)
        .into_iter()
        .find(|job| job.text.contains("401"))
        .expect("the report was laid out at all");
    assert_eq!(report.sections.len(), 1, "one run, not a labelled mix: {:?}", report.sections);
    assert_eq!(report.sections[0].format.color, egui::Color32::from_rgb(214, 100, 100), "RED");
    let _ = std::fs::remove_dir_all(dir);
}

/// A successful probe is the branch no error redactor ever sees, because a 200 body is not an error.
/// The canned endpoint therefore puts the key in its *answer*, and the report still has to be clean.
#[test]
fn a_successful_test_reports_the_answer_without_repeating_the_credential_that_bought_it() {
    const OK: &str = "sk-OKPROOF-0123456789abcdef";
    let body = serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": format!("ok, your key is {OK}")}}],
        "usage": {"prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14},
    })
    .to_string();
    let server = crate::fixtures::Canned::answering(200, body);
    let dir = scratch_install("ai-ok");
    let config = wind_base::config::Config::load(&dir).unwrap();
    let staged = ai_with_key(OK).at_url(&server.url());
    let report = crate::ai::probe(&config, &staged);
    assert!(report.ok, "{}", report.message);
    assert!(!report.message.contains(OK), "the success line leaked the key: {}", report.message);
    assert!(report.message.contains("characters back"), "{}", report.message);
    assert!(report.message.contains("14 tokens"), "{}", report.message);
    assert!(server.saw(&format!("Bearer {OK}")), "and it really was a request");
    let _ = std::fs::remove_dir_all(dir);
}

/// A probe's reply is a network answer to a question the user may have replaced, so it goes through
/// the same monotonic-id rule every other panel uses — and here it matters more than usual, because
/// the payload is a sentence about somebody's credential.
#[test]
fn a_stale_probe_reply_never_reaches_the_screen() {
    let mut state = state();
    state.tab = Tab::Ai;
    let (id, _) = state.begin_ai_test(ai_form()).expect("nothing in flight");
    assert!(state.ai_test.track.pending, "a probe in flight says so");
    assert!(state.begin_ai_test(ai_form()).is_none(), "a second click while pending asks for nothing");

    // The live reply lands, and the panel closes the track it was waiting on.
    assert!(state.apply(AppEvent::AiTested { request_id: id, outcome: Ok("first".into()) }));
    assert_eq!(state.ai_test.report, Some(Ok("first".to_string())));
    assert!(!state.ai_test.track.pending);

    // A new probe clears the old answer — the panel is now waiting on something else — and the reply
    // to the superseded request must not even ask for a repaint.
    let (newer, _) = state.begin_ai_test(ai_form()).expect("the first reply closed the track");
    assert!(newer > id);
    assert_eq!(state.ai_test.report, None, "starting over says so rather than showing a stale answer");
    assert!(
        !state.apply(AppEvent::AiTested { request_id: id, outcome: Err("stale".into()) }),
        "a superseded reply must not even ask for a repaint"
    );
    assert_eq!(state.ai_test.report, None, "the replaced reply stayed out");
    assert_eq!(state.dropped_stale, 1);
    assert!(state.apply(AppEvent::AiTested { request_id: newer, outcome: Ok("second".into()) }));
    assert_eq!(state.ai_test.report, Some(Ok("second".to_string())));
}

/// The whole flow, in a child process, with both output streams captured — because "no `eprintln!` in
/// the flow prints it" is a claim about the process's stderr, and reading the pipe is the only way to
/// assert on it without inventing a log sink this app does not have.
#[test]
fn the_whole_ai_flow_puts_the_key_in_no_output_the_process_produces() {
    let output = std::process::Command::new(std::env::current_exe().expect("this test binary"))
        // `--nocapture`, because libtest otherwise swallows a *passing* test's own output — and the
        // whole claim here is about what the child printed. Without it the two leak assertions below
        // would be checking an empty pipe, which is the most vacuous pass available.
        .args(["--ignored", "--exact", "--nocapture", "render_tests::ai_flow_child_drives_the_whole_page"])
        .env("WINDUI_PROOF_KEY", PROOF_KEY)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("the child test runs");
    let out = String::from_utf8_lossy(&output.stdout).to_string();
    let err = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "the child failed:\n{out}\n{err}");
    // Both guards below are vacuous without this: an empty pipe contains nothing, and "nothing" is not
    // evidence until something was certainly written into it.
    assert!(out.contains("AI-FLOW-COMPLETE"), "the child never finished the flow:\n{out}");
    assert!(out.contains(MASK), "the child painted a masked box, so the frame really was captured:\n{out}");
    assert!(out.contains("CHILD STATE:"), "the whole `AppState` was `Debug`-printed:\n{out}");
    // The app's own startup line, which is the one thing it sends to stderr today. Reading the two
    // streams apart is the point: a leak into either one is a leak the user can screenshot.
    assert!(err.contains("windui: window up"), "stderr was not actually captured:\n{err}");
    assert!(!out.contains(PROOF_KEY), "stdout leaked the key:\n{out}");
    assert!(!err.contains(PROOF_KEY), "stderr leaked the key:\n{err}");
}

/// The child of the test above. Ignored, so a normal run does not spawn itself; run by name, with the
/// secret passed in an environment variable so the two copies of the string cannot drift.
///
/// Deliberately noisy: it prints the painted frame, the notes, the save status and the `Debug` of the
/// entire application state, because those are the outputs a real debugging session produces and the
/// parent's claim is about all of the process's output rather than about the widgets.
#[test]
#[ignore = "run as a child process by the test above, which captures this binary's stdout and stderr"]
fn ai_flow_child_drives_the_whole_page() {
    let secret = std::env::var("WINDUI_PROOF_KEY").expect("the parent passes the key");
    let server = crate::fixtures::Canned::reflecting(401);
    let dir = scratch_install("ai-child");
    let ctx = egui::Context::default();
    let mut app = App::new(dir.clone(), None, &ctx).expect("boots");

    app.state.tab = Tab::Ai;
    app.state.ai_draft.set_text(crate::ai::AField::BaseUrl, &server.url());
    app.state.ai_draft.set_text(crate::ai::AField::Model, "gpt-4o");
    app.state.ai_draft.set_text(crate::ai::AField::ApiKey, &secret);
    app.state.ai_draft.set_text(crate::ai::AField::TitleLimit, "9000");
    // `frame_of`, never `app.frame` on a bare context: egui computes the panel's available rectangle
    // during `Context::run`, and a paint outside a run pass debug-asserts rather than drawing.
    println!("CHILD FRAME:\n{}", view::painted::joined(&frame_of(&mut app, &ctx)));
    println!("CHILD STATE: {:?}", app.state);

    let (validated, notes) = app.state.ai_draft.validate(&app.state.ai);
    println!("CHILD VALIDATED: {validated:?}");
    app.save_ai(&validated);
    frame_of(&mut app, &ctx);

    let (id, settings) = app.state.begin_ai_test(validated).expect("nothing in flight");
    app.dispatch(Command::TestAi { request_id: id, settings: Box::new(settings) });
    let full = settle(&mut app, &ctx, |a| a.state.ai_test.report.is_some());
    println!("CHILD REPORT FRAME:\n{}", view::painted::joined(&full));
    println!("CHILD NOTES: {notes:?}");
    println!("CHILD SAVE STATUS: {}", app.state.save_status);
    assert!(server.saw(&format!("Bearer {secret}")), "the key never reached the wire");
    let _ = std::fs::remove_dir_all(dir);
    println!("AI-FLOW-COMPLETE");
}

/// `AppState` derives `Debug`, which is what makes the hand-written `Debug` on the AI form load-bearing
/// rather than decorative: any future `eprintln!("{state:?}")`, panic message or test failure dump
/// takes exactly this shape. Asserted in-process too, so a failure names the field rather than a pipe.
#[test]
fn the_entire_application_state_prints_itself_without_printing_the_key() {
    let mut state = state();
    state.tab = Tab::Ai;
    state.ai = ai_with_key(PROOF_KEY);
    state.ai_draft = crate::ai::AiDraft::from(&state.ai);
    state.ai_draft.set_text(crate::ai::AField::ApiKey, PROOF_KEY);
    state.ai_notes = vec!["typed".to_string()];
    let rendered = format!("{state:?}");
    assert!(!rendered.contains(PROOF_KEY), "AppState::Debug leaked the key");
    assert!(rendered.contains(wind_ai::error::REDACTED), "and still reports that a key is held");
    assert!(rendered.contains("(held)"), "and which draft entry holds it");
}

/// Clearing the key is a change the user has to be able to make, and the page must say so before it
/// writes rather than leaving an empty box to mean three different things.
#[test]
fn clearing_the_stored_key_is_a_button_and_its_effect_is_said_before_the_save() {
    let dir = scratch_install("ai-clear");
    let config = wind_base::config::Config::load(&dir).unwrap();
    let mut state = ai_state_with_status(&dir);
    state.ai = state.ai.with_key(PROOF_KEY);
    state.ai_draft = crate::ai::AiDraft::from(&state.ai);
    state.ai_status.key = crate::ai::key_state(&config, &state.ai);
    let text = view::painted::joined(&frame(&mut state));
    assert!(text.contains("Clear stored key"), "{text}");
    assert!(text.contains("fingerprint"), "the page reports a key without showing one: {text}");
    assert!(!text.contains(PROOF_KEY), "{text}");

    state.ai_draft.clear_key();
    let (validated, notes) = state.ai_draft.validate(&state.ai);
    assert!(validated.key().is_empty());
    state.ai_notes = notes;
    state.ai_status = crate::model::AiStatus {
        verdict: crate::ai::verdict(&config, &validated),
        key: crate::ai::key_state(&config, &validated),
    };
    let text = view::painted::joined(&frame(&mut state));
    assert!(text.contains("cleared"), "the correction is on screen: {text}");
    assert!(text.contains("no key is stored"), "and the state line follows: {text}");
    assert!(!text.contains(PROOF_KEY), "{text}");
    let _ = std::fs::remove_dir_all(dir);
}

/// A scratch install in the layout a standalone payload actually ships: `config_src/` beside
/// `userdata/`, carrying the real shipped defaults so `Config`'s merge and `windai`'s reader both see
/// what they would see on a user's machine — including the placeholder key this page exists to replace.
fn scratch_install(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir()
        .join(format!("windui-{tag}-{}-{}", std::process::id(), crate::fixtures::next_scratch_id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("config_src")).unwrap();
    std::fs::create_dir_all(dir.join("userdata").join("db")).unwrap();
    std::fs::create_dir_all(dir.join("userdata").join("videos")).unwrap();
    let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the repository root")
        .join("config_src");
    std::fs::copy(shipped.join("config_default.json"), dir.join("config_src/config_default.json"))
        .unwrap_or_else(|e| panic!("copy {}: {e}", shipped.join("config_default.json").display()));
    // The window reads `languages.json` at startup, so a scratch install carries it like a real one.
    std::fs::copy(shipped.join("languages.json"), dir.join("config_src/languages.json"))
        .unwrap_or_else(|e| panic!("copy {}: {e}", shipped.join("languages.json").display()));
    dir
}

/// The round trip in its strongest available form: the **real `windai.exe`**, run as a child process
/// against the scratch root this page's own Save button wrote, printing its own report.
///
/// Every other test here asserts that `windai`'s *library* reader sees the keys. This one asserts that
/// the shipped binary — the thing a user types `windai doctor` for — does, over a real socket, on a
/// machine where `windai.exe` was built beside this test's own executable. A UI that wrote a key
/// `windai` did not read would pass nothing here: `doctor` would still say `NOT SET`, and the canned
/// endpoint would never be asked.
///
/// Ignored because it needs `windai.exe` to exist, which `cargo build --workspace` guarantees and a
/// bare `cargo test -p windui` does not. Run it with
/// `cargo test -p windui -- --ignored --nocapture the_real_windai_binary`.
#[test]
#[ignore = "needs the workspace's windai.exe built beside this test binary"]
fn the_real_windai_binary_reads_what_the_ai_page_wrote() {
    const DOCTOR_KEY: &str = "sk-DOCTORPROOF-0123456789abcdef";
    let windai = std::env::current_exe()
        .expect("this test binary")
        .parent()
        .and_then(std::path::Path::parent)
        .expect("target/debug")
        .join("windai.exe");
    assert!(windai.exists(), "build the workspace first: {}", windai.display());

    let reply = serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": "ok"}}],
        "usage": {"prompt_tokens": 11, "completion_tokens": 1, "total_tokens": 12},
    })
    .to_string();
    let server = crate::fixtures::Canned::answering(200, reply);
    let dir = scratch_install("ai-doctor");

    let report = |label: &str| {
        let output = std::process::Command::new(&windai)
            .args(["doctor", "--root"])
            .arg(&dir)
            .output()
            .expect("windai runs");
        let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        println!("---- {label} (exit {}) ----\n{}", output.status.code().unwrap_or(-1), text.trim_end());
        text
    };

    // Before: a stock install, configured nowhere, which is the state this page exists to end.
    let before = report("windai doctor BEFORE the AI page");
    assert!(before.contains(wind_ai::KEY_PLACEHOLDER), "the placeholder is what a stock file holds: {before}");
    assert!(before.contains("NOT SET"), "and doctor says so: {before}");
    assert!(before.contains("cannot make a test request"), "{before}");

    // The page's own path: the draft the widgets write to, the validation the Save button runs, the
    // `App::save_ai` the command dispatches to.
    let ctx = egui::Context::default();
    let mut app = App::new(dir.clone(), None, &ctx).expect("boots");
    app.state.tab = Tab::Ai;
    frame_of(&mut app, &ctx);
    app.state.ai_draft.set_text(crate::ai::AField::BaseUrl, &server.url());
    app.state.ai_draft.set_text(crate::ai::AField::Model, "windui-doctor-model");
    app.state.ai_draft.set_text(crate::ai::AField::ApiKey, DOCTOR_KEY);
    app.state.ai_draft.set_text(crate::ai::AField::TitleLimit, "44");
    app.state.ai_draft.set_text(crate::ai::AField::MaxTags, "7");
    app.state.ai_draft.set_text(crate::ai::AField::FilterWords, "季度营收\n");
    let (validated, notes) = app.state.ai_draft.validate(&app.state.ai);
    assert!(notes.is_empty(), "{notes:?}");
    app.save_ai(&validated);
    assert!(app.state.save_status.contains("wrote"), "{}", app.state.save_status);

    // After: the same binary, the same root, the endpoint the UI wrote, and one real round trip.
    let after = report("windai doctor AFTER the AI page");
    assert!(after.contains(&server.url()), "doctor used the endpoint the page wrote: {after}");
    assert!(after.contains("windui-doctor-model"), "{after}");
    assert!(!after.contains("NOT SET"), "the key line changed its mind: {after}");
    assert!(after.contains("configured, fingerprint "), "{after}");
    assert!(after.contains("the endpoint answers"), "so search and tags are configured: {after}");
    assert!(after.contains("44 titles"), "the tag limit crossed too: {after}");
    assert!(after.contains("7 tags per day"), "{after}");
    assert!(after.contains("季度营收"), "and the filter word: {after}");
    assert!(!after.contains(DOCTOR_KEY), "doctor never printed the key: {after}");
    assert!(server.saw(&format!("Bearer {DOCTOR_KEY}")), "the key did cross the wire: {}", server.requests().join("\n"));
    println!("---- the request windai sent (the key is what the page wrote) ----");
    for line in server.requests().first().map(|r| r.lines().take(6).collect::<Vec<_>>()).unwrap_or_default() {
        println!("{line}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// A real segment, encoded by the same binary the maintenance pass uses, at the path the index resolves.
///
/// `fixtures::Library::with_segment` writes bytes that are not video, which is exactly right for `Locate` —
/// that door only ever asks whether a name exists — and useless for playback, which has to decode them. So
/// the player's fixture is made for real, in the shape this product makes its own footage: one frame per
/// second, silent, index at the head. Returns whether it could, because ffmpeg is not in the payload and a
/// machine without it must be told the proof did not run rather than be told it passed.
fn clip(lib: &Library, segment: &str, seconds: u32) -> bool {
    let ffmpeg = std::path::Path::new("C:/Windows/System32/ffmpeg.exe");
    if !ffmpeg.is_file() {
        return false;
    }
    let stamp = LocalParts::from_stamp(&segment[..19]).expect("a stamped segment name");
    let dir = lib.path().join("userdata").join("videos").join(format!("{:04}-{:02}", stamp.year, stamp.month));
    std::fs::create_dir_all(&dir).expect("the month folder");
    let source = format!("testsrc=duration={seconds}:size=320x240:rate=1");
    let made = std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", &source, "-r", "1", "-an", "-movflags", "+faststart"])
        .arg(dir.join(segment))
        .status();
    made.is_ok_and(|status| status.success())
}

/// The whole player path, with no test doubles in it: a row two seconds into a real segment on the
/// fixture's disk, the click the painter parks, the dispatch that spawns ffmpeg, the decode, the upload,
/// and the caption that says which second is on screen.
///
/// Three claims, and each one fails differently if the wiring is wrong:
///   * the stream **opens at the row's own second**. A player that starts at zero is the exact failure the
///     seek exists to prevent, and every other assertion here would still pass;
///   * the probe's duration reaches the transport row, because a scrub bar over an unknown range is a
///     control that lies;
///   * the row's still is **still in the cache** while the player runs. That is what the third frame slot
///     buys (`textures::FRAME_CAP`), and it is why pressing `Frame` after a pause costs no disk read.
#[test]
fn the_viewer_streams_the_rows_own_segment_from_the_rows_own_second() {
    let lib = Library::empty("player");
    if !clip(&lib, "2026-09-21_10-00-00.mp4", 4) {
        eprintln!("skipped: no ffmpeg at C:/Windows/System32/ffmpeg.exe, and this test is about what ffmpeg produces");
        return;
    }
    lib.month(
        "default",
        2026,
        9,
        &[
            ("2026-09-21_10-00-00.mp4", "2026-09-21_10-00-00", "at the start", "T0"),
            ("2026-09-21_10-00-00.mp4", "2026-09-21_10-00-02", "two seconds in", "T2"),
        ],
    );
    let env = crate::backend::Env::load(&lib.path().to_path_buf()).expect("the fixture's own index opens");
    let card = crate::backend::card_of_key(&env, &RowKey::new("default_2026-09_wind.db", 2)).expect("the second row");
    assert_eq!(card.offset, Some(2), "the fixture is what this test is about, so its own shape is checked first");
    assert!(card.segment_path.is_some(), "and the segment the row names is on disk");

    let ctx = egui::Context::default();
    let mut app = App::new(lib.path().to_path_buf(), None, &ctx).expect("the app boots on the fixture");
    // The caption the transport row paints is interpolated, so it needs the shipped catalog: a fixture root has no
    // `config_src/`, and `Catalog::text` answers a key it cannot read with the loud missing-key marker rather than the
    // English the painter passed as a fallback. Installing the repository catalog is the path a real window takes.
    app.state.catalog = Catalog::load(&repo_root(), "en");
    app.state.pending_frame = Some(Box::new(card.clone()));
    settle(&mut app, &ctx, |a| a.holds_frame(&card.key));
    assert!(!app.state.frame.as_ref().is_some_and(|view| view.loading), "the still arrived before the player started");

    app.state.pending_player = Some(crate::model::PlayerRequest::Start(2));
    let painted = settle(&mut app, &ctx, |a| {
        a.state.player.as_ref().is_some_and(|player| player.at >= 3 && player.duration == Some(4)) && a.holds_frame(&crate::model::player_key())
    });

    let player = app.state.player.clone().expect("a player is standing in the viewer");
    assert_eq!(player.duration, Some(4), "the probe's seconds reached the transport row");
    assert!(player.at >= 2, "a stream that opened at second 0 would paint this test green and still be wrong: at {}", player.at);
    assert_eq!(player.failure, None, "{:?}", player.failure);
    assert!(app.holds_frame(&card.key), "the still survived the player, which is the third cache slot's whole job");
    let text = view::painted::joined(&painted);
    assert!(text.contains("Back to this row"), "the transport row is painted with its own copy: {text}");
    assert!(text.contains("2026-09-21_10-00-00.mp4"), "and it names the segment it is playing: {text}");

    // Pausing hands the picture back: the player is gone, the still is still there, and nothing is left
    // streaming behind a viewer that says it is showing one frame.
    app.state.pending_player = Some(crate::model::PlayerRequest::Stop);
    settle(&mut app, &ctx, |a| a.state.player.is_none());
    assert!(app.holds_frame(&card.key), "stopping costs the user nothing they had to load again");
}
