//! Painting. Nothing here decides anything `model` could not decide on its own.
//!
//! The contract with the rest of the crate is two lines: state goes in, [`Command`]s come out. No
//! file is read, no thread spawned, no database opened between the first line of [`paint`] and the
//! last — which is what makes a frame's cost a function of the rows on screen rather than of the
//! size of the library.
//!
//! Where the WebUI used a dataframe with an image column, this draws cards; where it used
//! `st.area_chart`, this builds the area out of `egui` shapes; where it injected CSS to colour a
//! matched keyword, this splits the label into runs that each carry their own `TextFormat`
//! (`highlight`).
//!
//! Two layout choices are worth stating because they are the opposite of the obvious one:
//!
//!   * the card grid is a *flow* of hand-packed rows, not `egui::Grid`, because `Grid` has no
//!     notion of "as many columns as fit" and the whole point of a card view is that it reflows
//!     with the window;
//!   * the OneDay strip is painted from precomputed spans rather than measured per frame, so a
//!     click is converted to a *time* and resolved by the same relation the store used to build the
//!     strip (`Timeline::index_for`). Position along the strip means position in the day.

use egui::text::LayoutJob;
use egui::{Align2, Color32, FontId, Pos2, Rect, RichText, Shape, Stroke, TextFormat, Vec2, pos2};

use crate::highlight;
use crate::flags::FlagNote;
use crate::model::{
    self, AppState, Command, FrameView, Player, PlayerRequest, RowCard, Tab, LIGHTBOX_COLUMNS, LIGHTBOX_ROWS, LIGHTBOX_SLOTS,
};
use crate::record::{DisplayInfo, RField};
use crate::settings::{Draft, Field, Kind};
use crate::textures::Cache;
use crate::wordcloud;
use wind_base::clock::LocalParts;

const CARD_WIDTH: f32 = 276.0;
/// A card's height, fixed. `show_rows` can only hand back a visible band if rows are uniform, and the
/// text budget and row cap below exist so a card cannot outgrow this; a card that did would make the
/// scroll position drift slightly, not break.
const CARD_HEIGHT: f32 = 140.0;
const STRIP_HEIGHT: f32 = 68.0;
const CHART_HEIGHT: f32 = 96.0;
/// How much recognised text a card lays out. A page of 500 rows each carrying a kilobyte of OCR is
/// half a megabyte of glyph layout that no 276-point-wide card can show; the detail pane paints the
/// whole thing for whichever row is selected.
const CARD_TEXT_BUDGET: usize = 160;
/// Rows of text beside the card's thumbnail.
const CARD_TEXT_ROWS: usize = 3;

pub fn paint(state: &mut AppState, textures: &mut Cache, frames: &mut Cache, ctx: &egui::Context, out: &mut Vec<Command>) {
    tab_bar(state, textures, ctx);
    footer(state, ctx, out);
    egui::CentralPanel::default().show(ctx, |ui| match state.tab {
        Tab::Search => search(state, textures, ui, out),
        Tab::OneDay => oneday(state, textures, ui, out),
        Tab::Stat => stat(state, textures, ui, out),
        Tab::Recording => recording(state, ui, out),
        Tab::Settings => settings(state, ui, out),
        Tab::Ai => assistant(state, ui, out),
    });
    // Last, so it is on top of every panel the click could have come from.
    frame_viewer(state, frames, ctx);

    // The panels above are nested inside `show_inside` closures that cannot reach `out`, so a click
    // in the detail pane parks its request in the state and it is collected here — once per frame,
    // in paint order.
    if let Some(path) = state.pending_locate.take() {
        out.push(Command::Locate { path });
    }
    if let Some(card) = state.pending_frame.take() {
        out.push(Command::ShowFrame(card));
    }
    // The transport row's request, turned into its command. Which row, which file and which stop flag
    // the answer has to carry is decided in `model` — see [`AppState::take_player_request`] — so that the
    // rule is testable without a window; this only puts the result beside the other parked requests.
    if let Some(command) = state.take_player_request() {
        out.push(command);
    }
    if let Some(time) = state.pending_flag.take() {
        let path = state.flag_path.clone();
        out.push(Command::Flag { time, path });
    }
    if let Some((row, new_note)) = state.pending_flag_edit.take() {
        let path = state.flag_path.clone();
        out.push(Command::EditFlag { path, when: row.when, note: row.note, index: row.index, new_note });
    }
    if let Some(row) = state.pending_flag_delete.take() {
        let path = state.flag_path.clone();
        out.push(Command::RemoveFlag { path, when: row.when, note: row.note, index: row.index });
    }
}

fn tab_bar(state: &mut AppState, textures: &Cache, ctx: &egui::Context) {
    egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            for tab in Tab::ALL {
                // Resolved through the window's catalog rather than `tab.label()`, so the six words
                // the user sees first become their language's. In `en` each key's copy is byte-identical
                // to `label()`, which is why the render tests still find "Search"/"OneDay"/"Settings"/"AI".
                let label = state.tr(tab.i18n_key());
                if ui.selectable_label(state.tab == tab, label).clicked() {
                    state.tab = tab;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!(
                        "paint {:.1} ms (max {:.1}) · {} thumbnails · {} cards laid out",
                        state.paint_ms_last,
                        state.paint_ms_max,
                        textures.len(),
                        state.painted_cards,
                    ))
                    .small()
                    .weak(),
                );
            });
        });
    });
}

fn footer(state: &mut AppState, ctx: &egui::Context, out: &mut Vec<Command>) {
    egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(state.footer.line()).small().monospace());
            if let Some(error) = &state.footer.error {
                ui.label(RichText::new(error).small().color(AMBER));
            }
            if state.footer.scanning {
                ui.spinner();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let button = state.tr("windui_footer_refresh");
                let help = state.tr("windui_refresh_help");
                if ui.button(button).on_hover_text(help).clicked() {
                    state.footer.scanning = true;
                    out.push(Command::RefreshSegments);
                    out.push(Command::ScanLibrary);
                }
            });
        });
    });
}

const AMBER: Color32 = Color32::from_rgb(214, 156, 74);
const RED: Color32 = Color32::from_rgb(214, 100, 100);
const HIGHLIGHT: Color32 = Color32::from_rgb(255, 214, 102);
/// A line saying something worked. Only the AI page has anything that can genuinely succeed at a
/// click — every other panel's failure is a missing row set, which is amber rather than red — so this
/// is not a general palette entry and lives beside the one screen that paints it.
const OK: Color32 = Color32::from_rgb(140, 200, 150);

// ---------------------------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------------------------

fn search(state: &mut AppState, textures: &mut Cache, ui: &mut egui::Ui, out: &mut Vec<Command>) {
    egui::SidePanel::right("search-detail")
        .resizable(true)
        .default_width(400.0)
        .min_width(220.0)
        .show_inside(ui, |ui| detail(state, ui));

    toolbar(state, ui, out);

    if state.months.is_empty() {
        // The first thing every new install sees. The WebUI answered this with a spinner that
        // resolved to an empty table; this says which directory is empty and what fills it.
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new(onboarding_hint(state)).heading().weak());
            let then = state.tr("windui_search_then_refresh");
            ui.label(RichText::new(then).small().italics());
        });
        return;
    }

    if let Some(error) = &state.search.error {
        ui.colored_label(RED, error);
    }
    let status = state.search.status();
    if !status.is_empty() {
        ui.label(RichText::new(status).small().monospace());
    }
    if !state.search.ran {
        let hint = state.tr("windui_search_type_hint");
        ui.label(RichText::new(hint).italics().weak());
        return;
    }
    if state.search.pending {
        ui.spinner();
    }
    if state.search.cards.is_empty() {
        let query = state.search.answered.as_ref().map(|a| a.keywords.clone()).unwrap_or_default();
        let message = state.trf("windui_search_no_match", &[("query", &query)]);
        ui.label(RichText::new(message).italics());
        return;
    }
    card_grid(state, textures, ui);
}

fn onboarding_hint(state: &AppState) -> String {
    state.trf("windui_search_onboarding", &[("months", &state.footer.months_total.to_string())])
}

fn toolbar(state: &mut AppState, ui: &mut egui::Ui, out: &mut Vec<Command>) {
    // Resolved up front, outside the `horizontal` closures: those closures hand `&mut` references to
    // individual `state.search.*` fields to the text boxes, and calling `state.tr` while one of those
    // borrows is live would conflict. `tr` returns an owned `String`, so resolving first releases the
    // borrow before any field is handed out.
    let kw_hint = state.tr("windui_kw_hint");
    let excl_hint = state.tr("windui_exclude_hint");
    let search_label = state.tr("windui_search_button");
    let search_help = state.tr("windui_search_help");
    let from_label = state.tr("windui_from");
    let to_label = state.tr("windui_to");
    let bad_date = state.tr("windui_bad_date");
    let page_size_label = state.tr("windui_page_size");

    ui.horizontal(|ui| {
        let width = (ui.available_width() - 360.0).max(140.0);
        let keyword = ui.add(
            egui::TextEdit::singleline(&mut state.search.params.keywords)
                .hint_text(kw_hint)
                .desired_width(width),
        );
        ui.add(
            egui::TextEdit::singleline(&mut state.search.params.exclude)
                .hint_text(excl_hint)
                .desired_width(120.0),
        );
        // Enter, not every keystroke. The WebUI re-ran on a lazy diff, which in practice queried
        // the whole library while the user was still typing the first word.
        let committed = keyword.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if committed || ui.button(search_label).on_hover_text(search_help).clicked() {
            let (id, params) = state.submit_search();
            out.push(Command::Search { request_id: id, params: Box::new(params) });
        }
    });

    ui.horizontal(|ui| {
        date_field(ui, &from_label, &mut state.search.params.from, &mut state.notice, &bad_date);
        date_field(ui, &to_label, &mut state.search.params.to, &mut state.notice, &bad_date);
        for (label, days) in [("7d", -6i64), ("30d", -29), ("90d", -89)] {
            if ui.button(label).clicked() {
                let to = state.search.params.to;
                state.search.params.from = crate::model::shift_date(to, days);
                let (id, params) = state.submit_search();
                out.push(Command::Search { request_id: id, params: Box::new(params) });
            }
        }
        ui.separator();
        ui.label(page_size_label);
        ui.add(egui::DragValue::new(&mut state.search.params.page_size).range(5..=500));
        let max_label = state.trf("windui_max", &[("value", &state.settings.max_page_result.to_string())]);
        ui.label(RichText::new(max_label).small().weak());
        ui.separator();
        let pages = state.search.pages.max(1);
        let current = state.search.params.page;
        if ui.add_enabled(current > 1, egui::Button::new("◀")).clicked() {
            request_page(state, current - 1, out);
        }
        let nav = state.trf("windui_page_nav", &[("current", &current.to_string()), ("pages", &pages.max(current).to_string())]);
        ui.label(nav);
        if ui.add_enabled(current < pages, egui::Button::new("▶")).clicked() {
            request_page(state, current + 1, out);
        }
        if let Some(notice) = &state.notice {
            ui.colored_label(AMBER, notice);
        }
    });
    ui.separator();
}

fn request_page(state: &mut AppState, page: usize, out: &mut Vec<Command>) {
    if let Some((id, params)) = state.goto_page(page) {
        out.push(Command::Search { request_id: id, params: Box::new(params) });
    }
}

/// A `YYYY-MM-DD` field that refuses to move the state on input it cannot read.
///
/// The label and the rejection notice both come from the catalog: `bad_date` is the already-resolved
/// template with `{text}` in it, filled here through the catalog's own `fill` so a Chinese or
/// Japanese install gets the same interpolated-message guarantee the tray's version row has.
fn date_field(ui: &mut egui::Ui, label: &str, into: &mut LocalParts, notice: &mut Option<String>, bad_date: &str) {
    ui.label(label);
    let mut text = into.date_stamp();
    let response = ui.add(egui::TextEdit::singleline(&mut text).desired_width(90.0).font(egui::TextStyle::Monospace));
    if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        match crate::model::parse_date(&text) {
            Some(day) => *into = day,
            None => *notice = Some(wind_base::i18n::fill(bad_date, &[("text", &text)])),
        }
    }
}

/// The card grid, and the arrow keys that move through it.
///
/// Only the rows inside the viewport are laid out. A page can hold 500 rows and each one is a
/// thumbnail plus a highlighted, wrapped label; laying all of them out every frame is tens of
/// milliseconds of glyph work for pixels nobody can see. `show_rows` can hand back the visible band
/// only because every card is exactly `CARD_HEIGHT` tall.
fn card_grid(state: &mut AppState, textures: &mut Cache, ui: &mut egui::Ui) {
    let columns = ((ui.available_width() - 16.0) / (CARD_WIDTH + 8.0)).floor().max(1.0) as usize;
    let count = state.search.cards.len();
    let rows = count.div_ceil(columns);
    state.grid_columns = columns;

    let painted = egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, CARD_HEIGHT, rows, |ui, band| {
            let mut visible = 0usize;
            for row in band {
                ui.horizontal(|ui| {
                    for index in (row * columns)..((row + 1) * columns).min(count) {
                        ui.vertical(|ui| {
                            ui.set_width(CARD_WIDTH);
                            ui.set_min_height(CARD_HEIGHT);
                            let selected = state.search.selected == Some(index);
                            let card = state.search.cards[index].clone();
                            let terms = state.search.terms.clone();
                            // The frame's own rectangle, not the row's: the row is `CARD_HEIGHT` tall
                            // so that `show_rows` can hand back a uniform band, and the painted card is
                            // shorter than that. Outlining the row drew a hover box that hung below the
                            // card it was describing.
                            let (rect, opened) = search_card(ui, textures, &card, &terms, selected);
                            let id = ui.id().with(("card", index));
                            let response = ui.interact(rect, id, egui::Sense::click());
                            if response.clicked() {
                                state.select_search(index);
                            } else if response.hovered() {
                                ui.painter()
                                    .rect_stroke(rect, 3.0, Stroke::new(1.0_f32, Color32::from_rgb(120, 160, 220)));
                            }
                            // The thumbnail's own click is a different request from the card's: it asks for
                            // the frame at the resolution it was recorded, and it selects the row as well.
                            if opened {
                                state.pending_frame = Some(Box::new(card.clone()));
                            }
                        });
                        ui.add_space(8.0);
                        visible += 1;
                    }
                });
            }
            visible
        })
        .inner;
    state.painted_cards = painted;

    let pressed = |key: egui::Key| ui.input(|i| i.key_pressed(key));
    let typing = ui.memory(|m| m.focused().is_some());
    if !typing && count > 0 {
        let step = if pressed(egui::Key::ArrowUp) {
            -(columns as isize)
        } else if pressed(egui::Key::ArrowDown) {
            columns as isize
        } else if pressed(egui::Key::ArrowLeft) {
            -1
        } else if pressed(egui::Key::ArrowRight) {
            1
        } else {
            0
        };
        if step != 0 {
            state.move_search_selection(step);
        }
    }
}

/// The card's painted box, which is what the caller outlines and hit-tests, plus whether its thumbnail was
/// clicked (which is the request to open the original).
fn search_card(ui: &mut egui::Ui, textures: &mut Cache, card: &RowCard, terms: &[String], selected: bool) -> (Rect, bool) {
    let frame = egui::Frame::group(ui.style())
        .fill(if selected { ui.visuals().selection.bg_fill } else { ui.visuals().extreme_bg_color });
    let mut opened = false;
    let rect = frame
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                opened = thumbnail(ui, textures, card, 64.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new(&card.clock).monospace().strong());
                    ui.label(RichText::new(&card.day).small().weak());
                    if let Some(title) = &card.title {
                        ui.label(RichText::new(truncate(title, 34)).small().italics());
                    }
                });
            });
            ui.label(marked_label(ui, &preview(&card.body), terms, CARD_TEXT_ROWS));
        })
        .response
        .rect;
    (rect, opened)
}

fn preview(body: &str) -> String {
    let flat = body.replace('\n', " ");
    flat.chars().take(CARD_TEXT_BUDGET).collect()
}

/// The card's picture, at thumbnail size, and whether it was clicked.
///
/// The picture *is* the control: the stored picture is a preview, not the frame, and clicking it is how a user asks
/// to see the frame that was actually recorded. Sense::click on the image alone, so the rest of the card
/// keeps selecting the row.
fn thumbnail(ui: &mut egui::Ui, textures: &mut Cache, card: &RowCard, height: f32) -> bool {
    let size = Vec2::new(height * 16.0 / 9.0, height);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, Color32::from_gray(28));
    if card.thumbnail.is_some() {
        if let Some((handle, _, _)) = textures.get(&card.key) {
            painter.image(handle.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        } else {
            // Queued, not failed: the mark disappears the frame the decode lands.
            painter.text(rect.center(), Align2::CENTER_CENTER, "…", FontId::proportional(14.0), Color32::DARK_GRAY);
        }
    }
    if card.segment_path.is_none() && card.picture_path.is_none() {
        // The recorder's own "the video is gone" marker, in the corner rather than as a column. There is
        // still something to look at when the screenshot survived, so the mark waits for both to be gone.
        painter.circle_filled(rect.right_top() + Vec2::new(-8.0, 8.0), 4.0, AMBER);
    }
    let clicked = response.clicked();
    // Attached either way: egui shows it only when the pointer is over the picture, and `on_hover_text`
    // consumes the response, so the click has to be read out of it first.
    response.on_hover_text("Click to open the original frame");
    clicked
}

/// The original frame, over everything else, at the resolution it was recorded — and, on a row whose
/// segment is still on disk, that segment playing inside the same box.
///
/// An `Area` in the foreground order rather than a `Window`: the picture has to be able to cover the grid
/// without re-laying-it-out, and it has to be drawn in the same pass the click opened it. A chrome-draggable
/// `Window` spends the pass it is created in on itself, so a viewer built that way paints one frame late and
/// reads as a control that did nothing.
///
/// Three states are said out loud — reading, absent, and shown with the door it came through. Painting the
/// stored preview any bigger is the bug this exists to remove, so when there is no full frame the
/// viewer says so in words and paints nothing.
///
/// The player is part of *this* viewer rather than a window of its own for the same reason the HTML window
/// put it there: a row is one moment in one segment, and a second surface to open — one that loses the
/// arrows, the caption and the row's own second on the way — is a worse answer than the `locate` hand-off
/// it replaces.
fn frame_viewer(state: &mut AppState, frames: &mut Cache, ctx: &egui::Context) {
    let Some(view) = state.frame.clone() else { return };
    let mut close = false;
    // The stream, but only while it belongs to the row on screen. The arrows walk and the stream keeps
    // running, and the pictures coming out of it belong to the row that asked; showing them under another
    // row's caption is the mistake `apply` already refuses to make with the reply.
    let playing = state.player.as_ref().filter(|player| player.key == view.key).cloned();
    // Leave room for the caption, the transport row and its bar; the picture is what the viewer exists to
    // show, and the second row of chrome is the price of putting the player inside this box rather than in
    // one of its own.
    let budget = ctx.screen_rect().size() - Vec2::new(96.0, 210.0);
    // The row's own stamp, not a translated sentence: a clock and a date read the same in every locale, and
    // this is the one title in the product that should never need a catalog row to say it.
    let title = format!("{} · {}", view.clock, view.day);
    egui::Area::new(egui::Id::new("windui_frame_viewer"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(title).strong());
                    // The header keeps describing the row — what the index holds for it, and whether that
                    // is still on disk — even while the segment is moving, because the line under the
                    // picture is the one that says what is actually being shown.
                    if view.loading {
                        ui.label(RichText::new(state.tr_or("windui_frame_loading", "reading the original frame…")).italics());
                        ui.spinner();
                    } else if view.missing {
                        ui.colored_label(
                            AMBER,
                            RichText::new(state.tr_or("windui_frame_missing", "no original frame left on disk for this row")).small(),
                        );
                    } else if let Some(source) = view.source {
                        ui.label(RichText::new(state.tr_or(source.key(), source.label())).small().weak());
                    }
                });
                match &playing {
                    Some(player) => moving_picture(state, frames, ui, player, budget),
                    None => still_picture(state, frames, ui, &view, budget),
                }
                transport_row(state, ui, &view, playing.as_ref(), &mut close);
            });
        });
    // `Esc` closes it, because a picture that covers the screen needs a keyboard way out.
    if close || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        state.close_frame();
    }
}

/// The still: the one frame this row was indexed at, and the fit it is shown at.
fn still_picture(state: &mut AppState, frames: &mut Cache, ui: &mut egui::Ui, view: &FrameView, budget: Vec2) {
    if let Some((handle, width, height)) = frames.get(&view.key) {
        let natural = Vec2::new(width as f32, height as f32);
        let fitted = (budget / natural).min_elem().max(0.0);
        let shown = natural * if view.actual_size { 1.0 } else { fitted };
        let (rect, _) = ui.allocate_exact_size(shown, egui::Sense::hover());
        ui.painter().image(
            handle.id(),
            rect,
            egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        ui.separator();
        ui.horizontal(|ui| {
            let mut actual = view.actual_size;
            if ui
                .checkbox(&mut actual, state.tr_or("windui_frame_actual_size", "1:1 (actual pixels)"))
                .on_hover_text(
                    "One texture pixel per screen pixel. Below the fit the picture is only scaled \
                     down, and this is how small text in a 1080p grab becomes readable.",
                )
                .changed()
            {
                if let Some(open) = state.frame.as_mut() {
                    open.actual_size = actual;
                }
            }
            ui.label(RichText::new(format!("{} × {} px", width, height)).small().monospace().weak());
        });
    } else if !view.loading && !view.missing {
        // The reply said there was a picture and the cache has lost it — which on a three-entry
        // cache means two other frames were opened while this one was in flight.
        ui.label(
            RichText::new(state.tr_or("windui_frame_gone", "that picture is no longer loaded; click the row again"))
                .small()
                .italics(),
        );
    }
}

/// The moving picture: one decoded second of the row's segment, in the same box and at the same fit as
/// the still it replaces.
///
/// The texture comes from a third cache slot (`model::player_key`), never from the row's own: a pause
/// leaves both the frame and the last second of footage on screen, and two slots would have them evict
/// each other once a second — every pause costing the user a fresh read of the thing they were looking
/// at before they pressed play.
fn moving_picture(state: &mut AppState, frames: &mut Cache, ui: &mut egui::Ui, player: &Player, budget: Vec2) {
    if player.waiting {
        // Between the click and the first frame: one short process to ask the file how long it is, then
        // a seek and a decode. A second, on a machine that has ffmpeg — which is why it is said.
        ui.label(RichText::new(state.tr_or("windui_web_play_loading", "opening the segment…")).italics());
        ui.spinner();
        return;
    }
    if let Some((handle, width, height)) = frames.get(&model::player_key()) {
        let natural = Vec2::new(width as f32, height as f32);
        let shown = natural * (budget / natural).min_elem().max(0.0);
        let (rect, _) = ui.allocate_exact_size(shown, egui::Sense::hover());
        ui.painter().image(
            handle.id(),
            rect,
            egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        let at = format!("+{}s", player.at);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(state.trf("windui_web_playing", &[("name", player.name.as_str()), ("at", at.as_str())]))
                    .small()
                    .monospace(),
            );
            // The length as a number and a unit, which is the one measurement here that needs no catalog
            // row — the same argument the viewer's title is built on, and the reason the scrub bar's own
            // range cannot be the only place the answer appears.
            if let Some(seconds) = player.duration {
                ui.label(RichText::new(format!("· {seconds} s")).small().monospace().weak());
            }
        });
    } else if player.failure.is_none() {
        // Not waiting, not broken, and nothing in the slot: the cache evicted the frame the stream drew,
        // which is what walking past two other rows does to it.
        ui.label(
            RichText::new(state.tr_or("windui_frame_gone", "that picture is no longer loaded; click the row again"))
                .small()
                .italics(),
        );
    }
    if let Some(failure) = &player.failure {
        // Amber, and above the transport row rather than instead of it: "this machine cannot decode h265"
        // and "this segment was deleted" have to be tellable apart by the user, and both of them have to
        // be tellable at all. A black box that says nothing is the failure this window keeps having to
        // design out, and the HTML half holds the same line.
        ui.colored_label(AMBER, RichText::new(failure.as_str()).small());
    }
}

/// The row of controls under the picture: play or stop, the seconds inside the segment, and the way out.
///
/// Painted in every state the picture can be in, including the ones with no picture — a control that only
/// appears once something is already moving is a control nobody finds, and a viewer with no way to close it
/// but the keyboard is a viewer that traps the pointer.
fn transport_row(state: &mut AppState, ui: &mut egui::Ui, view: &FrameView, player: Option<&Player>, close: &mut bool) {
    ui.separator();
    ui.horizontal(|ui| {
        match player {
            Some(_) => {
                // The catalog's word for this button is "Frame", because what stopping gives back is the
                // still the row was indexed from — the cheaper answer, and the one the user was looking at
                // before they asked for the moving one.
                let stop = state.tr_or("windui_web_play_stop", "Frame");
                let help = state.tr_or("windui_web_play_stop_hint", "Stop the segment and go back to the single frame the index holds for this row");
                if ui.button(stop).on_hover_text(help).clicked() {
                    state.pending_player = Some(PlayerRequest::Stop);
                }
                let back = state.tr_or("windui_web_play_back_to_moment", "Back to this row");
                let help = state.tr_or("windui_web_play_hint", "Watch the segment this row was taken from, inside the window, starting at this row's second");
                if ui.button(back).on_hover_text(help).clicked() {
                    // The row's own second, which is the moment the user clicked — and the one thing a
                    // bar they have just dragged across two minutes of footage cannot get back to on its
                    // own.
                    state.pending_player = Some(PlayerRequest::Start(view.start));
                }
            }
            None => {
                if view.segment_path.is_some() {
                    let play = state.tr_or("windui_web_play", "Play");
                    let help = state.tr_or("windui_web_play_hint", "Watch the segment this row was taken from, inside the window, starting at this row's second");
                    if ui.button(play).on_hover_text(help).clicked() {
                        // Not second zero: the row's own, which is the moment being asked about.
                        state.pending_player = Some(PlayerRequest::Start(view.start));
                    }
                } else if !view.missing {
                    // The row that kept its screenshot and lost its footage. `windui_frame_missing` is the
                    // line for the row that lost both, and it has already been said in the header, so this
                    // one only speaks when there is a picture to be wondering about the motion of.
                    ui.label(RichText::new(state.tr("windui_no_video_on_disk")).small().color(AMBER));
                }
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button(state.tr_or("windui_web_close", "Close")).clicked() {
                *close = true;
            }
        });
    });
    // The bar gets a row of its own, under the buttons: it is the only control here whose width means
    // something, and a slider sharing a line with three buttons is a slider nobody can stop on a second.
    if let Some(player) = player {
        scrub_bar(state, ui, player);
    }
}

/// The bar across the segment, when the probe has said how long it is.
///
/// No bar at all while the length is unknown rather than a bar over an invented range: a scrub control
/// whose ends are guesses is worse than the two buttons that are not.
fn scrub_bar(state: &mut AppState, ui: &mut egui::Ui, player: &Player) {
    let Some(seconds) = player.duration else { return };
    // The last second that can still answer with a picture. A bar whose end is the file's length parks
    // the seek one frame past the footage, where the stream returns nothing and the row says so — which is
    // honest but pointless, and `play::stream` has no frame to give there.
    let last = (seconds - 1).max(0);
    if last == 0 {
        // One second of footage is one second of footage: a bar from nought to nought has no place to
        // drag, and egui's own arithmetic on a collapsed range is a division the painter need not do.
        return;
    }
    let mut at = player.at.clamp(0, last);
    // Its own row, at the width the box allows: a `Slider` has no width setter in this egui, and a long
    // bar is the point of a bar over two hours of footage.
    let bar = ui.add(egui::Slider::new(&mut at, 0..=last));
    if bar.dragged() {
        // While the pointer owns the bar it shows where the user is aiming, not where the stream is. The
        // run being scrubbed away still delivers one picture a second, and a knob that jumps back under
        // the pointer twice a drag reads as a bar that cannot be held.
        if let Some(live) = state.player.as_mut() {
            live.at = at;
        }
    }
    if bar.drag_stopped() {
        // On release, and not per `changed()`: a seek is a new process — `play::stream` opens its own
        // ffmpeg at `-ss` the scrub second — so one command per frame of a drag across a four-minute
        // segment would ask for sixty of them to stop one that is already on its way out.
        state.pending_player = Some(PlayerRequest::Start(at));
    }
}

/// A label whose matched terms are separately coloured runs.
///
/// This is the real thing: one `LayoutJob`, one `TextFormat` per run from `highlight::runs`, so the
/// colour changes inside the string rather than around it.
fn marked_label(ui: &egui::Ui, text: &str, terms: &[String], rows: usize) -> LayoutJob {
    let size = ui.style().text_styles.get(&egui::TextStyle::Body).map(|f| f.size).unwrap_or(13.0);
    let normal = ui.visuals().text_color().gamma_multiply(0.85);
    let mut job = LayoutJob::default();
    for run in highlight::runs(text, terms) {
        let color = if run.matched { HIGHLIGHT } else { normal };
        job.append(&run.text, 0.0, TextFormat::simple(FontId::proportional(size), color));
    }
    job.break_on_newline = false;
    job.wrap.max_rows = rows;
    job
}

fn detail(state: &mut AppState, ui: &mut egui::Ui) {
    let Some(card) = state.selected_search_card().cloned() else {
        let hint = state.tr("windui_no_selection");
        ui.label(RichText::new(hint).italics().weak());
        return;
    };
    ui.label(RichText::new(format!("{} {}", card.day, card.clock)).heading().monospace());
    if let Some(title) = &card.title {
        ui.label(RichText::new(title).strong());
    }
    ui.separator();
    facts_and_actions(state, ui, &card);
    ui.separator();
    // The text goes last, in its own scroll area. Anything placed after a non-shrinking scroll area
    // is pushed below the panel's fold, and egui does not paint what it cannot see — so an action
    // row after the body would silently vanish for exactly the long rows that need it. The WebUI's
    // detail column had this bug in the other direction: a long OCR blob buried the filename.
    let terms = state.search.terms.clone();
    let body = card.body.clone();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.label(marked_label(ui, &body, &terms, usize::MAX));
    });
}

/// Where the frame came from, how far into it to seek, and the two things you can do about it.
fn facts_and_actions(state: &mut AppState, ui: &mut egui::Ui, card: &RowCard) {
    let segment_label = state.tr("windui_segment");
    ui.label(RichText::new(segment_label).small().weak());
    ui.label(RichText::new(&card.segment).monospace().small());
    let unknown = state.tr("windui_unknown");
    let offset = card.offset.map(wind_base::clock::seconds_to_hhmmss).unwrap_or_else(|| unknown.clone());
    let into_it = state.trf("windui_into_it", &[("offset", &offset)]);
    ui.label(RichText::new(into_it).small());

    match &card.segment_path {
        Some(path) => {
            let locate = state.tr("windui_locate");
            let locate_help = state.tr("windui_locate_help");
            if ui.button(locate).on_hover_text(locate_help).clicked() {
                // Recorded, not executed: a subprocess has no business starting inside a paint call.
                state.pending_locate = Some(path.clone());
            }
            ui.label(RichText::new(path.display().to_string()).small().weak());
        }
        None => {
            let gone = state.tr("windui_no_video_on_disk");
            ui.colored_label(AMBER, gone);
        }
    }

    if card.deep_link_is_url() {
        if let Some(url) = &card.deep_link {
            ui.separator();
            ui.hyperlink_to(url, url);
        }
    }
}

const LOCATE_HELP: &str = "Reveal the segment in Explorer (explorer.exe /select). Offered only when \
     the index says the video exists and its month folder still lists it.";

fn truncate(text: &str, budget: usize) -> String {
    let kept: String = text.chars().take(budget).collect();
    if kept.chars().count() < text.chars().count() {
        format!("{kept}…")
    } else {
        kept
    }
}

// ---------------------------------------------------------------------------------------------
// OneDay
// ---------------------------------------------------------------------------------------------

fn oneday(state: &mut AppState, textures: &mut Cache, ui: &mut egui::Ui, out: &mut Vec<Command>) {
    egui::SidePanel::right("oneday-side").resizable(true).default_width(280.0).min_width(170.0).show_inside(ui, |ui| {
        side_panel(state, ui);
    });

    ui.horizontal(|ui| {
        ui.label(RichText::new("OneDay").heading());
        for (label, days) in [("yesterday", -1i64), ("today", 0), ("tomorrow", 1)] {
            if ui.button(label).clicked() {
                let target = if days == 0 {
                    crate::model::default_day(state.today, state.settings.day_begin_minutes)
                } else {
                    crate::model::shift_date(state.day.date, days)
                };
                let (id, date) = state.set_day(target);
                out.push(Command::LoadDay { request_id: id, date });
            }
        }
        let mut text = state.day.date.date_stamp();
        let response = ui.add(egui::TextEdit::singleline(&mut text).desired_width(96.0).font(egui::TextStyle::Monospace));
        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            match crate::model::parse_date(&text) {
                Some(day) => {
                    let (id, date) = state.set_day(day);
                    out.push(Command::LoadDay { request_id: id, date });
                }
                None => state.notice = Some(format!("「{text}」 is not YYYY-MM-DD")),
            }
        }
        if let Some(notice) = &state.notice {
            ui.colored_label(AMBER, notice);
        }
    });

    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!(
                "{} rows · {} · {:.1} h on screen",
                state.day.all.len(),
                state.date_label(),
                state.day.active_hours
            ))
            .small()
            .monospace(),
        );
        if state.day.pending {
            ui.spinner();
        }
        if let Some(error) = &state.day.error {
            ui.colored_label(AMBER, error);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new("filter").small().weak());
            let filter = ui.add(egui::TextEdit::singleline(&mut state.day.filter).desired_width(140.0));
            if filter.changed() {
                let text = state.day.filter.clone();
                state.apply_filter(&text);
            }
        });
    });

    if state.months.is_empty() {
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("Nothing has been recorded yet — there is no day to open.").heading().weak());
        });
        return;
    }

    if state.day.all.is_empty() {
        // Two different problems. The WebUI kept them apart with two strings and so does this: a day
        // whose recordings were never indexed is fixable, a day with no recordings is not.
        let message = if state.day.unindexed_video {
            "recorded, but not indexed yet"
        } else {
            "no data for this day"
        };
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new(message).heading().weak());
        });
        return;
    }

    ui.add_space(4.0);
    timeline_strip(state, textures, ui);
    scrub(state, ui);
    activity_chart(state, ui);
    ui.separator();

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let visible = state.day.visible();
        if visible.is_empty() {
            ui.label(RichText::new("no row on this day matches the filter").italics());
            return;
        }
        let centre = state.day.selected.and_then(|s| visible.iter().position(|&i| i == s)).unwrap_or(visible.len() - 1);
        for index in visible[centre..visible.len().min(centre + 5)].iter() {
            let card = state.day.all[*index].clone();
            let terms: Vec<String> = if state.day.filter.trim().is_empty() {
                Vec::new()
            } else {
                state.day.filter.split_whitespace().map(str::to_string).collect()
            };
            let selected = state.day.selected == Some(*index);
            let mut opened = false;
            ui.horizontal(|ui| {
                ui.set_max_width(CARD_WIDTH + 40.0);
                let frame = egui::Frame::group(ui.style())
                    .fill(if selected { ui.visuals().selection.bg_fill } else { ui.visuals().extreme_bg_color });
                frame.show(ui, |ui| {
                    ui.horizontal(|ui| {
                        opened = thumbnail(ui, textures, &card, 56.0);
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&card.clock).monospace().strong());
                            if let Some(title) = &card.title {
                                ui.label(RichText::new(truncate(title, 30)).small().italics());
                            }
                        });
                    });
                    ui.label(marked_label(ui, &preview(&card.body), &terms, 2));
                });
                let rect = ui.min_rect();
                let id = ui.id().with(("daycard", *index));
                if ui.interact(rect, id, egui::Sense::click()).clicked() {
                    let time = card.time;
                    state.scrub_to(time);
                }
            });
            if opened {
                state.pending_frame = Some(Box::new(card.clone()));
            }
        }
        detail_day(state, ui);
    });
}

fn detail_day(state: &mut AppState, ui: &mut egui::Ui) {
    let Some(card) = state.selected_day_card().cloned() else { return };
    ui.separator();
    ui.label(RichText::new(format!("{} · {}", card.clock, card.title.clone().unwrap_or_default())).strong());
    let terms: Vec<String> =
        if state.day.filter.trim().is_empty() { Vec::new() } else { state.day.filter.split_whitespace().map(str::to_string).collect() };
    ui.label(marked_label(ui, &card.body, &terms, usize::MAX));
    ui.label(RichText::new(format!(
        "{} · at {}",
        card.segment,
        card.offset.map(wind_base::clock::seconds_to_hhmmss).unwrap_or_else(|| "?".into())
    ))
    .small()
    .weak());
    if let Some(path) = card.segment_path.clone() {
        if ui.button("Locate").on_hover_text(LOCATE_HELP).clicked() {
            state.pending_locate = Some(path);
        }
    }
    if card.deep_link_is_url() {
        if let Some(url) = &card.deep_link {
            ui.hyperlink_to(url, url);
        }
    }
}

/// The strip: samples laid out by the time they stand for, so a pixel is a moment.
fn timeline_strip(state: &mut AppState, textures: &mut Cache, ui: &mut egui::Ui) {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), STRIP_HEIGHT), egui::Sense::click());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, Color32::from_gray(24));
    let (t0, t1) = state.day.strip_span;
    if t1 <= t0 {
        return;
    }
    let to_x = |t: i64| rect.left() + ((t - t0) as f64 / (t1 - t0) as f64) as f32 * rect.width();
    state.strip_screen = [rect.left(), rect.top(), rect.right(), rect.bottom()];
    let cells = state.day.strip.clone();
    for cell in &cells {
        let a = to_x(cell.from).clamp(rect.left(), rect.right());
        let b = to_x(cell.to).clamp(rect.left(), rect.right());
        let box_rect = Rect::from_min_max(pos2(a.min(b), rect.top()), pos2(a.max(b), rect.bottom()));
        if box_rect.width() < 1.0 {
            continue;
        }
        painter.rect_filled(box_rect, 0.0, Color32::from_gray(40));
        match cell.key.as_ref().and_then(|k| textures.get(k)) {
            Some((handle, _, _)) => painter.image(
                handle.id(),
                box_rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            ),
            None => painter.line_segment(
                [box_rect.left_bottom(), box_rect.right_bottom()],
                Stroke::new(2.0_f32, Color32::from_rgb(90, 90, 110)),
            ),
        };
    }
    for flag in &state.day.flags {
        if let Some(t) = flag.time {
            let x = to_x(t);
            painter.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(1.5_f32, RED));
        }
    }
    let x = to_x(state.day.scrub);
    painter.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(2.0_f32, Color32::from_rgb(120, 200, 255)));

    // The pixel becomes a time here and `model` resolves it through the spans painted above, which
    // are `Timeline`'s own: a click in a gap selects nothing, because nothing was captured there.
    // Filtered by the strip's own rectangle: egui reports the last pointer position it was told about
    // even when that point is nowhere near this widget, and an unfiltered readout printed a clamped
    // extrapolation of a click that was on the tab bar.
    let hovered = ui
        .input(|i| i.pointer.interact_pos())
        .filter(|p| rect.contains(*p))
        .map(|p| time_at(p.x, rect, t0, t1));
    if let Some(time) = hovered {
        state.strip_hover = Some(time);
        painter.text(
            pos2(rect.left() + 4.0, rect.top() + 2.0),
            Align2::LEFT_TOP,
            LocalParts::from_naive_epoch(time).display()[11..16].to_string(),
            FontId::monospace(11.0),
            Color32::LIGHT_GRAY,
        );
    } else {
        state.strip_hover = None;
    }
    if let Some(pos) = response.interact_pointer_pos() {
        state.click_strip(time_at(pos.x, rect, t0, t1));
    }
}

/// The moment a pixel of the strip stands for. Inverse of the strip's own layout, and shared with
/// the click handler so the readout under the cursor and the row it selects cannot disagree.
fn time_at(x: f32, rect: Rect, t0: i64, t1: i64) -> i64 {
    let frac = ((x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64;
    t0 + (frac * (t1 - t0) as f64).round() as i64
}

fn scrub(state: &mut AppState, ui: &mut egui::Ui) {
    let (from, to) = state.day.bounds;
    if to <= from {
        return;
    }
    let mut time = state.day.scrub.clamp(from, to);
    let slider = egui::Slider::new(&mut time, from..=to)
        .show_value(false)
        .custom_formatter(|v, _| LocalParts::from_naive_epoch(v as i64).display()[11..16].to_string())
        .text("rewind");
    if ui.add(slider).on_hover_text(REWIND_HELP).changed() {
        state.scrub_to(time);
    }
}

const REWIND_HELP: &str = "A time, not an index: the day's bounds are its ends, and the card shown is \
     the newest capture at or before the moment you stop on.";

/// The day's activity as an area, built from rectangles plus one polyline.
///
/// Not a convex fill: `egui`'s tessellator only supports convex polygons, and a stepped area is
/// not one. Adjacent 6-minute columns abut exactly, so the result reads as a filled area without
/// asking the tessellator for something it cannot do.
fn activity_chart(state: &mut AppState, ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), CHART_HEIGHT), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, Color32::from_gray(20));
    let buckets = state.day.buckets.clone();
    if buckets.is_empty() {
        return;
    }
    let peak = buckets.iter().map(|b| b.count).max().unwrap_or(1).max(1) as f32;
    let (t0, t1) = (buckets.first().map(|b| b.start).unwrap_or(0), buckets.last().map(|b| b.start).unwrap_or(1));
    let span = (t1 - t0).max(1) as f64;
    let step = rect.width() / buckets.len() as f32;
    let fill = Color32::from_rgb(172, 121, 213).gamma_multiply(0.6);
    let edge = Color32::from_rgb(196, 150, 230);
    let mut outline: Vec<Pos2> = Vec::with_capacity(buckets.len() * 2);
    for bucket in &buckets {
        let a = rect.left() + ((bucket.start - t0) as f64 / span) as f32 * rect.width();
        let b = (a + step).min(rect.right());
        let y = rect.bottom() - (bucket.count as f32 / peak) * (rect.height() - 12.0);
        if bucket.count > 0 {
            painter.rect_filled(Rect::from_min_max(pos2(a, y), pos2(b, rect.bottom())), 0.0, fill);
        }
        outline.push(pos2(a, y));
        outline.push(pos2(b, y));
    }
    painter.add(Shape::line(outline, Stroke::new(1.0_f32, edge)));
    for (i, bucket) in buckets.iter().enumerate() {
        if i % 20 != 0 {
            continue;
        }
        let a = rect.left() + ((bucket.start - t0) as f64 / span) as f32 * rect.width();
        painter.text(pos2(a, rect.bottom() - 6.0), Align2::LEFT_BOTTOM, &bucket.label, FontId::monospace(9.0), Color32::GRAY);
    }
    painter.text(rect.right_top() + Vec2::new(-4.0, 4.0), Align2::RIGHT_TOP, format!("peak {peak}"), FontId::proportional(10.0), Color32::GRAY);
}

fn side_panel(state: &mut AppState, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Where the time went").strong());
        if ui.button("🚩").on_hover_text(FLAG_HELP).clicked() {
            state.pending_flag = Some(state.day.scrub);
        }
    });

    // The user's own bookmarks before the generated list, for the same reason the settings buttons
    // are: a busy day's forty window titles must not be able to push the flags below the fold.
    ui.label(RichText::new("Flags and notes").strong());
    if state.day.flags.is_empty() {
        ui.label(RichText::new("nothing flagged for this day").small().italics());
    }
    let flags = state.day.flags.clone();
    // Named id sources: two `ScrollArea`s built in the same `Ui` derive the same auto id, and egui
    // then reports the clash on screen (`warn_on_id_clash` is on by default) while the two lists share
    // one scroll offset — the titles list would jump to wherever the flags list had been scrolled.
    ui.push_id("flags", |ui| {
        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
            for flag in &flags {
                flag_row(state, ui, flag);
                ui.separator();
            }
        });
    });
    ui.separator();

    if state.day.titles.is_empty() {
        ui.label(RichText::new("no window title held focus long enough to count").small().italics());
    }
    let titles = state.day.titles.clone();
    ui.push_id("titles", |ui| {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (title, secs) in &titles {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(truncate(title, 34)).small());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(wind_base::clock::seconds_to_hhmmss(*secs)).small().monospace().weak());
                    });
                });
            }
        });
    });
}

const FLAG_HELP: &str = "Append a row to userdata/flag_mark_note.csv at the moment the scrubber is on.";

/// One flag of the day's list: its time, an editable note, and a delete that takes two clicks.
///
/// The note is edited in a per-row draft (`DayState::flag_drafts`) rather than in place, so a
/// half-typed correction survives a redraw without being overwritten by the value on disk, and the
/// Save button only appears once the draft differs from what the file holds. Save parks a
/// [`Command::EditFlag`] for `app` to write through `wind-notes`; the panel never rewrites a CSV.
///
/// The delete is deliberately two clicks. A first on 🗑 arms the row (`DayState::flag_confirm`), and
/// only an explicit "Yes" parks a [`Command::RemoveFlag`]. Because that command rewrites the whole
/// table, the arming names exactly the row this panel drew, and the store re-checks it against the
/// file before writing — so a flag the tray appended since is kept, not silently dropped.
fn flag_row(state: &mut AppState, ui: &mut egui::Ui, flag: &FlagNote) {
    ui.push_id(flag.index, |ui| {
        let time = flag.when.rsplit(' ').next().unwrap_or("").to_string();
        ui.horizontal(|ui| {
            ui.label(RichText::new(time).monospace().small());
            if !flag.has_thumbnail {
                ui.label(RichText::new("·").weak()).on_hover_text(NO_THUMB_HELP);
            }
        });

        let mut text = state.day.flag_drafts.get(&flag.index).cloned().unwrap_or_else(|| flag.note.clone());
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut text).desired_width(200.0).font(egui::TextStyle::Monospace));
            if text != flag.note && ui.button("💾").on_hover_text(SAVE_NOTE_HELP).clicked() {
                state.pending_flag_edit = Some((flag.clone(), text.clone()));
            }
        });
        // Keep the draft in step with the box so scrolling away and back does not drop a half-typed
        // edit, and clear it once the row says what the file says so a reloaded row starts clean.
        if text != flag.note {
            state.day.flag_drafts.insert(flag.index, text.clone());
        } else {
            state.day.flag_drafts.remove(&flag.index);
        }

        ui.horizontal(|ui| {
            if state.day.flag_confirm == Some(flag.index) {
                ui.label(RichText::new("delete this flag?").small().italics());
                if ui.button("Yes").on_hover_text(CONFIRM_DELETE_HELP).clicked() {
                    state.pending_flag_delete = Some(flag.clone());
                    state.day.flag_confirm = None;
                }
                if ui.button("No").clicked() {
                    state.day.flag_confirm = None;
                }
            } else if ui.button("🗑").on_hover_text(DELETE_FLAG_HELP).clicked() {
                state.day.flag_confirm = Some(flag.index);
            }
        });
    });
}

const SAVE_NOTE_HELP: &str = "Rewrite this flag's note in the table. Only this row changes; the file is rewritten through wind-notes, never by a second writer here.";
const DELETE_FLAG_HELP: &str = "Ask to delete this flag. Nothing is removed until you confirm on this line.";
const CONFIRM_DELETE_HELP: &str = "Delete this flagged row from userdata/flag_mark_note.csv. The row is re-checked against the file first, so a flag added since this list was drawn is kept.";
const NO_THUMB_HELP: &str = "This flag carries no screen thumbnail — the tray flags grab one; a flag from the OneDay scrubber does not.";

// ---------------------------------------------------------------------------------------------
// Axes — the crate's one scatter renderer
// ---------------------------------------------------------------------------------------------

/// One axis of a scatter: the span it draws and where its ticks land.
///
/// A struct rather than six arguments so both charts are built the same way and cannot drift into two
/// different ideas of where a value goes.
pub struct Axis {
    name: String,
    min: f32,
    max: f32,
    ticks: Vec<(f32, String)>,
}

impl Axis {
    /// A linear axis with evenly spaced whole-number ticks.
    fn numbered(name: impl Into<String>, min: f32, max: f32, every: f32) -> Axis {
        let name: String = name.into();
        let mut ticks = Vec::new();
        let every = every.max(1.0);
        let mut value = min;
        while value <= max + f32::EPSILON {
            ticks.push((value, format!("{}", value as i64)));
            value += every;
        }
        Axis { name, min, max, ticks }
    }

    /// An axis whose tick labels are words at positions the data chooses — the year chart's months.
    fn labelled(name: &str, min: f32, max: f32, ticks: Vec<(f32, String)>) -> Axis {
        Axis { name: name.to_string(), min, max, ticks }
    }
}

/// A scatter: background, grid at the ticks, the two axis lines, then the points.
///
/// Every chart in this crate that has an axis goes through here — the month view and the year view —
/// because the failure mode of two hand-rolled ones is not ugliness but two charts that disagree about
/// the same number. The OneDay activity area above is not an axis chart: its x axis *is* the strip
/// beneath it, and naming the time is the strip's job.
///
/// A point is `(x, y, radius)` in data space. `y` runs upwards, so the projection subtracts from
/// `bottom` rather than adding to `top`; a chart that did it the other way would show the busiest day
/// as the emptiest one.
fn scatter(ui: &mut egui::Ui, height: f32, title: &str, caption: &str, x: &Axis, y: &Axis, points: &[(f32, f32, f32)], tint: Color32) {
    let (outer, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), egui::Sense::hover());
    let painter = ui.painter_at(outer);
    painter.rect_filled(outer, 2.0, Color32::from_gray(20));

    // The gutters are fixed, not measured: a plot area that moves because a label grew a character is
    // a plot area whose points cannot be compared between two frames.
    let left = outer.left() + 40.0;
    let bottom = outer.bottom() - 16.0;
    let top = outer.top() + 15.0;
    let right = outer.right() - 8.0;
    if right <= left || bottom <= top {
        // Too narrow to be a chart. Say so, rather than drawing a frame with nothing in it, which is
        // indistinguishable on screen from a chart that found no data.
        painter.text(
            outer.center(),
            Align2::CENTER_CENTER,
            "widen the window to see this chart",
            FontId::proportional(10.0),
            Color32::GRAY,
        );
        return;
    }
    let plot = Rect::from_min_max(pos2(left, top), pos2(right, bottom));
    let span_x = (x.max - x.min).max(f32::EPSILON);
    let span_y = (y.max - y.min).max(f32::EPSILON);
    let px = |v: f32| plot.left() + ((v - x.min) / span_x) * plot.width();
    let py = |v: f32| plot.bottom() - ((v - y.min) / span_y) * plot.height();

    let grid = Stroke::new(1.0_f32, Color32::from_gray(38));
    let axis_line = Stroke::new(1.0_f32, Color32::from_gray(90));
    for (value, label) in &y.ticks {
        let at = py(*value);
        painter.line_segment([pos2(left, at), pos2(right, at)], grid);
        painter.text(pos2(left - 5.0, at), Align2::RIGHT_CENTER, label, FontId::monospace(9.5), Color32::GRAY);
    }
    for (value, label) in &x.ticks {
        let at = px(*value);
        painter.line_segment([pos2(at, top), pos2(at, bottom)], grid);
        painter.text(pos2(at, bottom + 2.0), Align2::CENTER_TOP, label, FontId::monospace(9.5), Color32::GRAY);
    }
    painter.line_segment([pos2(left, top), pos2(left, bottom)], axis_line);
    painter.line_segment([pos2(left, bottom), pos2(right, bottom)], axis_line);

    for (dx, dy, radius) in points {
        let centre = pos2(px(*dx), py(*dy));
        let radius = radius.max(1.5);
        painter.circle_filled(centre, radius, tint.gamma_multiply(0.75));
        painter.circle_stroke(centre, radius + 0.5, Stroke::new(1.0_f32, tint));
    }

    painter.text(pos2(outer.left() + 4.0, outer.top() + 2.0), Align2::LEFT_TOP, title, FontId::proportional(11.5), Color32::LIGHT_GRAY);
    painter.text(pos2(right, outer.top() + 3.0), Align2::RIGHT_TOP, caption, FontId::monospace(9.5), Color32::GRAY);
    painter.text(pos2(right, bottom + 14.0), Align2::RIGHT_TOP, x.name.as_str(), FontId::proportional(9.5), Color32::GRAY);
    // The y axis name sits inside the plot's top-left. egui draws no rotated text, and a vertical
    // strip of single glyphs is worse-looking and unreadable next to the tick labels it describes.
    painter.text(pos2(left + 3.0, top + 1.0), Align2::LEFT_TOP, y.name.as_str(), FontId::proportional(9.5), Color32::GRAY);
}

// ---------------------------------------------------------------------------------------------
// Stat
// ---------------------------------------------------------------------------------------------

const MONTH_CHART_HEIGHT: f32 = 190.0;
const YEAR_CHART_HEIGHT: f32 = 240.0;
const CLOUD_HEIGHT: f32 = 250.0;
const LIGHTBOX_HEIGHT: f32 = 430.0;
/// Upstream's own two scatter colours, kept because a user who has looked at both UIs should see the
/// same chart twice.
const MONTH_TINT: Color32 = Color32::from_rgb(0xAC, 0x79, 0xD5);
const YEAR_TINT: Color32 = Color32::from_rgb(0xC8, 0x73, 0xA6);
const MONTH_NAMES: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const MONTH_TICKS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

fn stat(state: &mut AppState, textures: &mut Cache, ui: &mut egui::Ui, out: &mut Vec<Command>) {
    egui::SidePanel::right("stat-memory").resizable(true).default_width(560.0).min_width(280.0).show_inside(ui, |ui| {
        memory_column(state, textures, ui, out);
    });

    ui.horizontal(|ui| {
        ui.label(RichText::new("Stat").heading());
        pickers(state, ui, out);
        if state.stat.month_track.pending || state.stat.year_track.pending {
            ui.spinner();
        }
    });
    if let Some(notice) = &state.notice {
        ui.colored_label(AMBER, notice);
    }

    if state.months.is_empty() {
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("There is no month to summarise until the recorder has written one.").heading().weak());
        });
        return;
    }
    for (label, error) in [("month chart", &state.stat.month_track.error), ("year chart", &state.stat.year_track.error)] {
        if let Some(error) = error {
            ui.colored_label(AMBER, format!("{label}: {error}"));
        }
    }

    // The charts go in a scroll area, and nothing interactive may follow one: a non-shrinking scroll
    // area takes every pixel the panel has left and egui does not paint below the fold.
    let days = state.stat.days.clone();
    let year_points = state.stat.year_points.clone();
    let (year, month) = (state.stat.year, state.stat.month);
    let (month_rows, year_rows) = (state.stat.month_rows, state.stat.year_rows);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        month_scatter(ui, year, month, &days, month_rows);
        ui.add_space(8.0);
        year_scatter(ui, year, &year_points, year_rows);
    });
}

/// The year and month boxes, bounded by the library rather than by the calendar.
///
/// Upstream reads the earliest and latest record and makes its pickers' bounds from them, including
/// the asymmetry that the first and last *year* are whole while the first and last *month* are cut to
/// what has data. `model::AppState::set_stat_year` is where that clamp lives, so a value cannot leave
/// the range the way a hand-typed Streamlit number silently can.
fn pickers(state: &mut AppState, ui: &mut egui::Ui, out: &mut Vec<Command>) {
    let (low, high) = state.record_years();
    ui.label(RichText::new("year").small().weak());
    let mut year = state.stat.year as i32;
    let before = year;
    if ui.add(egui::DragValue::new(&mut year).range(low as i32..=high as i32).speed(0.2)).changed() && year != before {
        state.set_stat_year(i64::from(year));
        state.stat_month_changed();
        let (id, year) = state.load_year();
        out.push(Command::LoadYear { request_id: id, year });
        let (id, year, month) = state.load_month();
        out.push(Command::LoadMonth { request_id: id, year, month });
    }

    let (first, last) = state.record_months(state.stat.year);
    ui.label(RichText::new("month").small().weak());
    let mut month = state.stat.month as i32;
    let (first, last) = (first as i32, last as i32);
    let before = month;
    let mut chosen = false;
    if first > last {
        // A year the library holds nothing in offers no months at all, which is only reachable while
        // the first scan is still running. `ComboBox` has no disabled state and an empty popup
        // explains nothing, so a dash is the answer that means "not yet".
        ui.label(RichText::new("--").monospace().weak());
    } else {
        let name = &MONTH_NAMES[month.clamp(first, last) as usize - 1];
        let response = egui::ComboBox::from_id_salt("stat-month")
            .selected_text(format!("{name} {month:02}"))
            .width(110.0)
            .show_ui(ui, |ui| {
                for value in first..=last {
                    ui.selectable_value(&mut month, value, MONTH_NAMES[value as usize - 1]);
                }
            });
        // `selectable_value` writes through the borrow, so the answer is read back out of `month`
        // rather than inferred from which row happened to be clicked.
        chosen = response.inner.is_some() && month != before;
    }
    if chosen {
        let _ = state.set_stat_month(month as u32);
        state.stat_month_changed();
        let (id, year, month) = state.load_month();
        out.push(Command::LoadMonth { request_id: id, year, month });
    }
    ui.label(RichText::new(format!("{:04}-{:02}", state.stat.year, state.stat.month)).monospace().small());
}

fn month_scatter(ui: &mut egui::Ui, year: i64, month: u32, days: &[model::DayPoint], rows: i64) {
    let days_in = wind_base::clock::days_in_month(year, month) as f32;
    let peak_hours = days.iter().map(|d| d.hours).fold(0.0f64, f64::max).max(1.0) as f32;
    let peak_rows = days.iter().map(|d| d.rows).max().unwrap_or(1).max(1) as f64;
    let x = Axis::numbered(&format!("day of {}", MONTH_NAMES[month as usize - 1]), 1.0, days_in, (days_in / 6.0).ceil().max(1.0));
    let y = Axis::numbered("hours", 0.0, peak_hours.ceil().max(1.0), (peak_hours / 4.0).ceil().max(1.0));
    // Area is what a count means. A dot four times the *radius* is sixteen times the ink, and the
    // busiest day would read as far busier than the second-busiest one.
    let points: Vec<(f32, f32, f32)> = days
        .iter()
        .map(|d| {
            let share = (d.rows as f64 / peak_rows).max(0.01).sqrt();
            let r = (13.0f32 * share as f32).clamp(2.0, 13.0);
            (d.day as f32, d.hours as f32, r)
        })
        .collect();
    let caption = format!("{} of {days_in:.0} days · {rows} rows", days.len());
    scatter(ui, MONTH_CHART_HEIGHT, "on-screen hours per day · size = rows", &caption, &x, &y, &points, MONTH_TINT);
}

fn year_scatter(ui: &mut egui::Ui, year: i64, points_in: &[model::MonthDayPoint], rows: i64) {
    let peak = points_in.iter().map(|p| p.rows).max().unwrap_or(1).max(1) as f64;
    let ticks: Vec<(f32, String)> = MONTH_TICKS.iter().enumerate().map(|(index, name)| (index as f32 + 1.0, name.to_string())).collect();
    let x = Axis::labelled("month", 1.0, 12.0, ticks);
    let y = Axis::numbered("day of month", 1.0, 31.0, 5.0);
    let points: Vec<(f32, f32, f32)> = points_in
        .iter()
        .map(|p| {
            let share = (p.rows as f64 / peak).max(0.01).sqrt();
            let r = (11.0f32 * share as f32).clamp(1.5, 11.0);
            (p.month as f32, p.day as f32, r)
        })
        .collect();
    let caption = format!("{year} · {} active days · {rows} rows", points_in.len());
    scatter(ui, YEAR_CHART_HEIGHT, "every day of the year · size = rows", &caption, &x, &y, &points, YEAR_TINT);
}

/// The right-hand column: the month's contact sheet and its word cloud.
///
/// The two buttons come before the painted areas for the reason the settings panel gives for its Save
/// button — a scroll area that will not shrink swallows the rest of the panel, and a button below the
/// fold is a button that does not exist.
fn memory_column(state: &mut AppState, textures: &mut Cache, ui: &mut egui::Ui, out: &mut Vec<Command>) {
    ui.label(RichText::new("This month, all at once").strong());
    ui.horizontal(|ui| {
        if ui.button("Lightbox").on_hover_text(LIGHTBOX_HELP).clicked() {
            let (id, year, month) = state.build_lightbox();
            out.push(Command::BuildLightbox { request_id: id, year, month });
        }
        if ui.button("Word cloud").on_hover_text(CLOUD_HELP).clicked() {
            let (id, year, month) = state.build_cloud();
            out.push(Command::BuildCloud { request_id: id, year, month });
        }
        if state.stat.tiles_track.pending || state.stat.cloud_track.pending {
            ui.spinner();
        }
    });
    ui.checkbox(&mut state.stat.watermark, "watermark band").on_hover_text(WATERMARK_HELP);
    if let Some(error) = &state.stat.tiles_track.error {
        ui.colored_label(AMBER, format!("lightbox: {error}"));
    }
    if let Some(error) = &state.stat.cloud_track.error {
        ui.colored_label(AMBER, format!("word cloud: {error}"));
    }
    lightbox(state, textures, ui);
    ui.separator();
    cloud(state, ui);
}

const LIGHTBOX_HELP: &str = "Tile up to 875 of this month's thumbnails in upstream's 25 x 35 grid. \
     Nothing is written: the Python app composited this into a PNG under result_lightbox and base64'd \
     it back onto the page, and that file round trip is the thing this UI exists to remove. Upstream \
     also refuses to draw anything at all in a month with fewer captures than slots — here the tiles \
     that exist are laid out and the rest is said out loud.";

const WATERMARK_HELP: &str = "Draw the date band and border over the grid. The Python app bakes the \
     same band into the saved file when enable_month_lightbox_watermark is on; nothing is saved here, \
     so this is a view option and the config key keeps its original meaning for the Python lightbox.";

const CLOUD_HELP: &str = "Rank this month's recognised text and lay the words out in the frame — \
     frequency to font size, then a spiral from the centre until a word finds clear room. Upstream \
     hands the same text to a Python word cloud and shows the PNG it wrote.";

fn lightbox(state: &mut AppState, textures: &mut Cache, ui: &mut egui::Ui) {
    if state.stat.tiles.is_empty() {
        let message = if state.stat.tiles_track.pending {
            "reading this month's thumbnails…"
        } else if state.stat.tiles_track.loaded {
            "this month holds nothing to tile"
        } else {
            "press Lightbox to fill this"
        };
        ui.label(RichText::new(message).italics().weak());
        return;
    }
    let tiles = state.stat.tiles.clone();
    let columns = LIGHTBOX_COLUMNS;
    let gap = 1.0f32;
    let tile_w = ((ui.available_width() - gap * (columns - 1) as f32) / columns as f32).clamp(4.0, 72.0);
    let tile_h = (tile_w * 9.0 / 16.0).clamp(3.0, 42.0);
    // Upstream's grid is a fixed 35 rows. A short month leaves the rest blank and the blank *is* the
    // information — but only up to 875 tiles: asking for 35 rows for nine captures would spend the
    // whole scroll height on nothing, so the row count follows the tiles and the caption says by how much.
    let rows = tiles.len().div_ceil(columns).clamp(1, LIGHTBOX_ROWS);
    let out = egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .max_height(LIGHTBOX_HEIGHT)
        .show_rows(ui, tile_h + gap, rows, |ui, band| {
            let mut count = 0usize;
            for row in band {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for index in (row * columns)..((row + 1) * columns).min(tiles.len()) {
                        let tile = &tiles[index];
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(tile_w, tile_h), egui::Sense::hover());
                        let painter = ui.painter_at(rect);
                        painter.rect_filled(rect, 0.0, Color32::from_gray(26));
                        match textures.get(&tile.key) {
                            Some((handle, _, _)) => {
                                painter.image(
                                handle.id(),
                                rect,
                                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                                    Color32::WHITE,
                                );
                            }
                            None => {
                                // A queued decode and a month with fewer captures than slots both
                                // leave a hole. Only the ones that ever had a thumbnail get a mark,
                                // so an empty slot stays empty instead of pretending to load.
                                if tile.thumbnail.is_some() {
                                    painter.text(
                                        rect.center(),
                                        Align2::CENTER_CENTER,
                                        "…",
                                        FontId::proportional(8.0),
                                        Color32::DARK_GRAY,
                                    );
                                }
                            }
                        }
                        count += 1;
                    }
                });
            }
            count
        });
    if state.stat.watermark {
        watermark_band(ui, out.inner_rect, &state.stat.lightbox_caption());
    }
    let filled = tiles.iter().filter(|t| t.thumbnail.is_some()).count();
    ui.label(
        RichText::new(format!("{filled} of {LIGHTBOX_SLOTS} slots · {} rows of {columns}", rows))
            .small()
            .monospace()
            .weak(),
    );
}

/// The band upstream appends below the grid, drawn over it instead of baked into a file.
fn watermark_band(ui: &mut egui::Ui, rect: Rect, caption: &str) {
    if rect.height() <= 0.0 || rect.width() <= 0.0 {
        return;
    }
    let painter = ui.painter_at(rect);
    let band = Rect::from_min_max(pos2(rect.left(), rect.bottom() - 22.0), rect.right_bottom());
    painter.rect_filled(band, 0.0, Color32::from_white_alpha(30));
    painter.text(
        band.left_top() + Vec2::new(6.0, 4.0),
        Align2::LEFT_TOP,
        caption,
        FontId::monospace(11.0),
        Color32::from_rgb(157, 130, 103),
    );
    painter.text(
        band.right_top() + Vec2::new(-4.0, 5.0),
        Align2::RIGHT_TOP,
        "Windrecorder",
        FontId::proportional(10.5),
        Color32::from_rgb(190, 170, 152),
    );
    painter.rect_stroke(rect, 0.0, Stroke::new(2.0_f32, Color32::from_rgb(215, 205, 197)));
}

/// The word cloud: laid out once per answer, drawn every frame after that.
fn cloud(state: &mut AppState, ui: &mut egui::Ui) {
    ui.label(RichText::new("What the month said").strong());
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), CLOUD_HEIGHT), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, Color32::from_gray(20));
    if state.stat.words.is_empty() {
        let message = if state.stat.cloud_track.pending {
            "reading this month's text…"
        } else if state.stat.cloud_track.loaded {
            "nothing left after the stop words"
        } else {
            "press Word cloud to build this"
        };
        painter.text(rect.center(), Align2::CENTER_CENTER, message, FontId::proportional(11.0), Color32::GRAY);
        state.stat.placed.clear();
        return;
    }
    // The spiral is the one piece of work on this screen that is not a memcpy, so it runs when the
    // words change and never again: `placed_for` records the request this geometry belongs to, and a
    // reply that was dropped as stale therefore cannot re-trigger it either.
    if state.stat.placed_for != state.stat.cloud_track.request_id {
        let width = rect.width().max(1.0);
        let height = rect.height().max(1.0);
        let words = state.stat.words.clone();
        let placed = wordcloud::place(&words, width, height, &|text, size| {
            let job = LayoutJob::simple(text.to_string(), FontId::proportional(size), Color32::WHITE, 0.0);
            let galley = ui.fonts(|fonts| fonts.layout_job(job));
            (galley.rect.width(), galley.rect.height())
        });
        state.stat.placed = placed;
        state.stat.placed_for = state.stat.cloud_track.request_id;
    }
    for word in &state.stat.placed {
        // The layout ran in a box the size of this rect, so mapping back is an origin shift and the
        // word lands exactly where the collision test said it would.
        let box_ = Rect::from_min_max(
            pos2(rect.left() + word.box_[0], rect.top() + word.box_[1]),
            pos2(rect.left() + word.box_[2], rect.top() + word.box_[3]),
        );
        // A shallow ramp from near-white to mid grey by rank: once size is taken, colour is the only
        // thing left to carry order, and a hue cycle would read as a category that does not exist.
        let shade = (220i32 - word.rank as i32 * 2).clamp(120, 220) as u8;
        painter.text(box_.center(), Align2::CENTER_CENTER, &word.text, FontId::proportional(word.size), Color32::from_gray(shade));
    }
    ui.label(
        RichText::new(format!("{} of {} words placed", state.stat.placed.len(), state.stat.words.len()))
            .small()
            .monospace()
            .weak(),
    );
}

// ---------------------------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------------------------

fn recording(state: &mut AppState, ui: &mut egui::Ui, out: &mut Vec<Command>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Recording").heading());
        ui.label(RichText::new("read by the recorder and the maintenance pass, not by this window").small().italics().weak());
    });
    ui.colored_label(
        AMBER,
        "Save writes userdata/config_user.json and the recorder reads it on its next start, so a value \
         it does not expect is a broken recording rather than a wrong screen.",
    );

    // A config key that is on by default, does nothing, and whose only evidence is one `eprintln!` in
    // the recorder — into a stderr the supervisor redirects into a log file it truncates on every
    // start, so a user launched from the tray can neither see the notice nor learn that there was one
    // to see. `windrec` indexes every row with `deep_linking: None` and reads the key for nothing but
    // that warning (`recorder.rs`'s `warn_unported`), while `mcp` hands the same column out as the
    // result `url`. So the gap gets named here, in the one screen where the user is reading the
    // recorder's settings, and named as a gap rather than as a pending fix.
    if state.rec.deep_linking_promised {
        ui.colored_label(
            AMBER,
            "record_deep_linking is on, and the native recorder cannot honour it: it indexes every row \
             with an empty deep link, so a search result has no URL to reopen. This page does not offer \
             the switch and Save does not write it — and setting it to false by hand in \
             userdata/config_user.json changes nothing about what is recorded either, because the only \
             thing the recorder does with the key is decide whether to warn about this.",
        );
    }

    // The battery gate on the screenshot→video pass, in the same shape as the deep-link notice above.
    // `record_screen.py` honours all three modes through `is_power_plugged_in`, but `windrec` and
    // `windmaint` never read the key, so on the native path it is inert. The switch was removed from
    // this page for exactly that reason; a user who set it through the Python recorder is told here
    // that the engine they are now looking at ignores it, and Save leaves their value in place.
    if state.rec.energy_saving_requested {
        ui.colored_label(
            AMBER,
            "convert_screenshots_to_vid_energy_saving_mode is set to gate the screenshot→video pass on \
             the charger, but no native binary reads it: `windrec` and `windmaint` stitch footage on \
             their own schedule whatever the battery is doing, so on this engine it changes nothing \
             about when footage is watchable. This page no longer offers the switch — it is honoured \
             only by the Python recorder — and Save leaves the value you set there exactly where it is.",
        );
    }

    // Verdict and buttons first, above the scroll area, for the reason `settings.rs` gives.
    for note in &state.rec_notes {
        ui.colored_label(AMBER, note);
    }
    ui.horizontal(|ui| {
        if ui.button(state.tr_or("windui_web_save", "Save")).on_hover_text(SAVE_RECORD_HELP).clicked() {
            let (validated, notes) = state.rec_draft.validate(&state.rec, &state.rec_options);
            state.rec_notes = notes;
            state.pending_rec = Some(Box::new(validated.clone()));
            out.push(Command::SaveRecording(Box::new(validated)));
        }
        if ui.button(state.tr_or("windui_revert", "Revert")).clicked() {
            state.rec_draft = crate::record::RecDraft::from(&state.rec);
            state.rec_notes = Vec::new();
        }
        if ui.button("Re-detect displays").on_hover_text(PROBE_HELP).clicked() {
            state.displays_pending = true;
            out.push(Command::ProbeDisplays);
        }
        if state.displays_pending {
            ui.spinner();
        }
        if !state.save_status.is_empty() {
            let status = state.save_status.clone();
            ui.label(RichText::new(status).small().monospace());
        }
    });
    ui.separator();

    let options = state.rec_options.clone();
    let displays = state.displays.clone();
    let configured = state.rec.record_single_display_index;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let mut group: Option<&str> = None;
        for field in RField::ALL {
            if group != Some(field.group()) {
                group = Some(field.group());
                ui.add_space(6.0);
                ui.label(RichText::new(state.tr_or(field.group_key(), field.group())).strong());
            }
            // The panel list belongs beside the index rather than in a block of its own: the whole
            // point is that a number and its monitor are read at the same moment.
            if field == RField::SingleDisplayIndex {
                display_list(ui, &displays, configured);
            }
            ui.push_id(field.key(), |ui| {
                ui.horizontal(|ui| {
                    let width = (ui.available_width() - 230.0).clamp(140.0, 340.0);
                    let label = state.tr_or(field.label_key(), field.label());
                    ui.add_sized([width, 20.0], egui::Label::new(label).selectable(false));
                    record_widget(state, ui, field, &options);
                    let help = state.tr_or(field.help_key(), field.help());
                    ui.label(RichText::new("?").weak()).on_hover_text(help);
                });
            });
        }
    });
}

/// The panels the desktop reported, next to the index the config holds.
///
/// The whole reason this tab asks the operating system anything: a picker that shows `3` tells the
/// user nothing about which cable they just pointed the recorder at, on a machine whose fourth panel
/// is a portrait one nobody would choose by number.
fn display_list(ui: &mut egui::Ui, displays: &[DisplayInfo], configured: i64) {
    if displays.is_empty() {
        ui.label(RichText::new("displays not probed yet — press Re-detect displays above").small().italics().weak());
        return;
    }
    for display in displays {
        let text = RichText::new(format!("{} · {:.1} MP", display.label(), display.megapixels())).small();
        if display.index as i64 == configured {
            ui.colored_label(HIGHLIGHT, text.strong());
        } else {
            ui.label(text.weak());
        }
    }
    let count = displays.len() as i64;
    if configured < 1 || configured > count {
        ui.colored_label(
            AMBER,
            format!("display {configured} matches none of the {count} panels above — `single` would capture nothing"),
        );
    }
}

/// One row's editor, bound to the draft text rather than to the typed value, for the reason
/// `setting_widget` gives.
fn record_widget(state: &mut AppState, ui: &mut egui::Ui, field: RField, options: &crate::record::RecOptions) {
    let kind = field.kind(options, &state.rec);
    match kind {
        crate::record::Kind::Int { min, max } => number_box(state, ui, field, &format!("{min}..{max}")),
        crate::record::Kind::Fraction { min, max } => number_box(state, ui, field, &format!("{min}..{max}")),
        crate::record::Kind::Bool => {
            let mut on = state.rec_draft.bool_of(field);
            if ui.checkbox(&mut on, "").changed() {
                state.rec_draft.set_text(field, if on { "true" } else { "false" });
            }
        }
        crate::record::Kind::Choice(list) => {
            let mut value = state.rec_draft.text(field).to_string();
            egui::ComboBox::from_id_salt(field.key())
                .selected_text(if value.is_empty() { "—".to_string() } else { value.clone() })
                .width(180.0)
                .show_ui(ui, |ui| {
                    ui.set_max_width(260.0);
                    for option in &list {
                        ui.selectable_value(&mut value, option.clone(), option.as_str());
                    }
                    if list.is_empty() {
                        ui.label(RichText::new("the preset file offers nothing").small().italics());
                    }
                });
            // `selectable_value` writes through the borrow, so the new value is taken from `value`
            // rather than inferred from which row happened to be clicked.
            state.rec_draft.set_text(field, &value);
        }
    }
}

fn number_box(state: &mut AppState, ui: &mut egui::Ui, field: RField, hint: &str) {
    let mut text = state.rec_draft.text(field).to_string();
    let response = ui.add(
        egui::TextEdit::singleline(&mut text)
            .desired_width(120.0)
            .hint_text(hint)
            .font(egui::TextStyle::Monospace),
    );
    if response.changed() {
        state.rec_draft.set_text(field, &text);
    }
}

const SAVE_RECORD_HELP: &str = "Stage these twenty-five keys into the merged config and write \
     userdata/config_user.json through a temp file and a rename. Every other key in the file — the \
     hundred the recorder owns and the fifteen the Settings tab edits — round-trips exactly as it was \
     read. The recorder picks the changes up on its next start.";

const PROBE_HELP: &str = "Ask the desktop which panels are attached and at what pixel size. Done on a \
     worker thread because the answer is only true for a per-monitor-DPI-aware one, and setting that \
     on the frame thread would rescale this window for the rest of the session.";

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

fn settings(state: &mut AppState, ui: &mut egui::Ui, out: &mut Vec<Command>) {
    ui.label(RichText::new("Settings these two screens read").heading());
    ui.label(
        RichText::new(
            "Fifteen keys, typed and bounded. Everything else in the config belongs to the recorder \
             and is written back untouched.",
        )
        .small()
        .italics(),
    );
    ui.label(RichText::new(format!("day begins at {}", crate::settings::hhmm(state.settings.day_begin_minutes))).small());

    // The verdict and the buttons come before the field list, not after it. A scroll area that
    // refuses to shrink consumes every pixel the panel has left, and egui does not paint what the
    // fold hides: a Save button placed after the list is a Save button that silently stops existing
    // the moment the list is longer than the window.
    for note in &state.notes {
        ui.colored_label(AMBER, note);
    }
    ui.horizontal(|ui| {
        if ui.button(state.tr_or("windui_web_save", "Save")).on_hover_text(SAVE_HELP).clicked() {
            // Validated once here and reported in the frame that follows: the widget that produced
            // the out-of-range value is the one that has to say what it did about it.
            let (validated, notes) = {
                let options = state.settings_options.clone();
                state.draft.validate(&state.settings, &options)
            };
            state.notes = notes;
            state.pending_settings = Some(Box::new(validated.clone()));
            out.push(Command::SaveSettings(Box::new(validated)));
        }
        if ui.button(state.tr_or("windui_revert", "Revert")).clicked() {
            state.draft = Draft::from(&state.settings);
            state.notes = Vec::new();
        }
        if !state.save_status.is_empty() {
            let status = state.save_status.clone();
            ui.label(RichText::new(status).small().monospace());
        }
    });
    ui.separator();

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        // Cloned once for the whole list rather than per row: a picker's list is the machine's answer, and
        // `state` is borrowed mutably by every widget below.
        let options = state.settings_options.clone();
        for field in Field::ALL {
            ui.push_id(field.key(), |ui| {
                ui.horizontal(|ui| {
                    let width = (ui.available_width() - 200.0).clamp(140.0, 320.0);
                    let label = state.tr_or(field.label_key(), field.label());
                    ui.add_sized([width, 20.0], egui::Label::new(label).selectable(false));
                    setting_widget(state, ui, field, &options);
                    let help = state.tr_or(field.help_key(), field.help());
                    ui.label(RichText::new("?").weak()).on_hover_text(help);
                });
            });
        }
        // The engines this install refuses to run are named, not hidden: a user who installed PaddleOCR
        // through the old extension scripts is owed the sentence "there is no Python left to run it" rather
        // than a list that quietly dropped their engine.
        let refused = state.settings_options.unavailable_engines();
        if !refused.is_empty() {
            let joined = refused.join(", ");
            ui.colored_label(
                AMBER,
                state.catalog.formatted_or(
                    "set_refused_ocr_engines",
                    &[("engines", &joined)],
                    "{engines} are listed but this install cannot run them",
                ),
            );
        }
        // The same sentence the HTML window prints, from the same predicate on the same config key: a
        // preview narrower than the box it is drawn in is stretched, and a person who has just raised the
        // number has to be told that the rows already on disk wait for the idle pass. Two windows, one
        // answer — which is the rule this fork keeps having to re-learn.
        if (state.settings.thumbnail_generation_size_width as u32) < wind_base::image::CARD_PREVIEW_FLOOR {
            let stored = state.settings.thumbnail_generation_size_width.to_string();
            let floor = wind_base::image::CARD_PREVIEW_FLOOR.to_string();
            ui.colored_label(
                AMBER,
                state.catalog.formatted_or(
                    "set_note_small_preview",
                    &[("stored", &stored), ("floor", &floor)],
                    "This install stores previews {stored} px wide, and a result card is drawn about {floor} px across, so every picture in the window is being stretched. Raise this row, and the idle maintenance pass redraws the rows already on disk from the screenshot or video behind them.",
                ),
            );
        }
    });
}

/// The prompt editor: every template, what is in force, and a live trial.
///
/// This is the section the whole feature exists to make possible. A prompt that is only editable in a
/// file the user has to find is a prompt that is not editable by most users, and a settings screen that
/// shows a *copy* of the text is worse: it lets them edit something that is not what runs. So the box
/// holds the effective text read from disk, Save writes that same file, and "try it" sends the text in
/// the box — unsaved — to the endpoint against a real stretch of the user's own screen.
fn prompt_panel(state: &mut AppState, ui: &mut egui::Ui, out: &mut Vec<Command>) {
    use crate::ai::PromptName;

    ui.add_space(10.0);
    ui.separator();
    ui.label(
        RichText::new(state.tr_or("ai_group_prompts", "Prompts — the words sent to a model"))
            .strong()
            .heading(),
    );
    ui.label(
        RichText::new(
            "Seven templates. `user` means your file at userdata/ai_prompts/<name>.txt overrides what \
             this build ships; `shipped` means it does not. Editing here writes that file, and \
             restoring deletes it — nothing is copied over the shipped copy.",
        )
        .small()
        .weak(),
    );
    if !state.ai_prompts.status.is_empty() {
        ui.colored_label(AMBER, RichText::new(state.ai_prompts.status.clone()).small().monospace());
    }

    // One row per template, cloned out because the widgets need to mutate the drafts they draw.
    let rows = state.ai_prompts.rows.clone();
    for row in rows {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(row.label).monospace());
            ui.label(
                RichText::new(format!("{} · {}", row.origin, row.path))
                    .small()
                    .weak(),
            );
            if row.dirty {
                ui.colored_label(AMBER, RichText::new("unsaved").small());
            }
        });

        let mut text = row.text.clone();
        let rows = if text.lines().count() > 12 { 12 } else { 5 };
        ui.add(
            egui::TextEdit::multiline(&mut text)
                .desired_rows(rows)
                .desired_width(f32::INFINITY)
                .font(egui::TextStyle::Monospace),
        );
        if text != row.text {
            state.edit_prompt(row.name, &text);
        }

        ui.horizontal(|ui| {
            if ui
                .add_enabled(row.dirty, egui::Button::new("Save"))
                .on_hover_text("Validated before it is written: an unknown {token}, or a missing \
                                {frames_table} / {period_summaries} / {table}, is refused here in the \
                                same words `windai prompts` uses.")
                .clicked()
            {
                out.push(Command::SavePrompt { name: row.name, text: text.clone() });
            }
            if ui
                .add_enabled(row.overridden, egui::Button::new("Restore shipped"))
                .on_hover_text("Deletes your override, so the words this build ships answer again.")
                .clicked()
            {
                out.push(Command::RestorePrompt { name: row.name });
            }
            if row.testable {
                let running = state.ai_prompts.trial.pending;
                let asking = state.ai_prompts.trial_for == Some(row.name) && running;
                if ui
                    .add_enabled(!running, egui::Button::new("Try these words on a real stretch"))
                    .on_hover_text("Sends one request to the endpoint in the form above, built from the \
                                    text in this box even if you have not saved it, about the newest \
                                    stretch the index has. It writes nothing — not the prompt, not a \
                                    summary.")
                    .clicked()
                {
                    let (validated, notes) = state.ai_draft.validate(&state.ai);
                    state.ai_notes = notes;
                    if let Some((id, _text, settings)) = state.begin_prompt_trial(row.name, text.clone(), validated) {
                        out.push(Command::TestPrompt { request_id: id, name: row.name, text: text.clone(), settings });
                    }
                }
                if asking {
                    ui.spinner();
                    ui.label(RichText::new("asking the endpoint…").small().italics().weak());
                }
                if state.ai_prompts.trial_for == Some(row.name) {
                    if let Some(trial) = &state.ai_prompts.report {
                        let colour = if trial.ok { OK } else { RED };
                        ui.colored_label(
                            colour,
                            RichText::new(format!("{} → {}", trial.segment, trial.message)).small().monospace(),
                        );
                    }
                }
            } else {
                ui.label(
                    RichText::new("tried by `windai tags --dry-run` / `windai search --explain`")
                        .small()
                        .weak(),
                );
            }
        });
    }

    let _ = PromptName::ALL;
}

/// One row's editor, bound to the draft text rather than to the typed value.
///
/// The draft is a `String` per field on purpose: a number being deleted down to "" mid-edit must
/// not become 0, and a value that fails to parse must not silently take the previous one's place.
/// `Draft::validate` is where text becomes a number, and it is where an out-of-range number is
/// clamped and the reason recorded.
fn setting_widget(state: &mut AppState, ui: &mut egui::Ui, field: Field, options: &crate::settings::Options) {
    let kind = field.kind(options, &state.settings);
    match kind {
        Kind::Int { min, max } => {
            let mut text = state.draft.text(field).to_string();
            let response = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .desired_width(120.0)
                    .hint_text(&format!("{min}..{max}"))
                    .font(egui::TextStyle::Monospace),
            );
            if response.changed() {
                state.draft.set_text(field, &text);
            }
        }
        Kind::Bool => {
            let mut on = state.draft.bool_of(field);
            if ui.checkbox(&mut on, "").changed() {
                state.draft.set_text(field, if on { "true" } else { "false" });
            }
        }
        Kind::Text { max_chars } => {
            let mut text = state.draft.text(field).to_string();
            let response = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .desired_width(220.0)
                    .hint_text(&format!("up to {max_chars} characters")),
            );
            if response.changed() {
                state.draft.set_text(field, &text);
            }
        }
        Kind::Lines { max_entries } => {
            let mut text = state.draft.text(field).to_string();
            let response = ui.add(
                egui::TextEdit::multiline(&mut text)
                    .desired_rows(5)
                    .desired_width(320.0)
                    .hint_text(&format!("one phrase per line, at most {max_entries}")),
            );
            if response.changed() {
                state.draft.set_text(field, &text);
            }
        }
        // Four edges per screen, because a list that runs short leaves the other panels on the shipped
        // default — a mask row with one group on a four-panel machine is a control that quietly covers
        // one screen and claims to cover the desk.
        Kind::Urbl { slots } => mask_editor(state, ui, field, options, slots),
        // A picker writes the value the row carries and shows the label the machine or the catalog
        // supplies: the config stores `sc`, and the row that picked it must read 简体中文.
        Kind::Choice(choices) => {
            // The row shows the label the machine or the catalog supplied and writes the value the config
            // holds: a locale is stored as `sc` and read as 简体中文.
            let shown = state.draft.label_of(field, options, &state.settings);
            let mut value = state.draft.text(field).to_string();
            egui::ComboBox::from_id_salt(field.key())
                .selected_text(shown)
                .width(200.0)
                .show_ui(ui, |ui| {
                    ui.set_max_width(300.0);
                    for choice in &choices {
                        ui.selectable_value(&mut value, choice.value.clone(), choice.label.as_str());
                    }
                    if choices.is_empty() {
                        ui.label(RichText::new("this install offers nothing").small().italics());
                    }
                });
            state.draft.set_text(field, &value);
        }
    }
}

const SAVE_HELP: &str = "Write userdata/config_user.json with a temp file and a rename. The Python \
     app reads the same file, so a rejected value must never reach it.";

// ---------------------------------------------------------------------------------------------
// AI — the keys `windai` reads, and the only place in the product they can be set
// ---------------------------------------------------------------------------------------------

/// Upstream's Lab tab. Verdict, key state and test report are painted before the field list for the
/// reason `settings.rs` gives twice already: a non-shrinking scroll area consumes every pixel the
/// panel has left, egui does not paint below the fold, and a control the user cannot see is a control
/// that does not exist.
/// The mask row: one group of four percentages per screen, in the key's own top/right/bottom/left order.
///
/// Each group prints the band it actually paints, in pixels, beside the numbers. That is not decoration:
/// "6% of a panel" is not something a person can check against their own screen, and this is the setting
/// that decides what never reaches the recogniser. The pixels come from the same function the recorder
/// calls, so the number on screen and the band in the index cannot be two different answers.
fn mask_editor(
    state: &mut AppState,
    ui: &mut egui::Ui,
    field: Field,
    options: &crate::settings::Options,
    slots: usize,
) {
    let mut values = crate::settings::fit_mask(
        &crate::settings::parse_mask(state.draft.text(field)).unwrap_or_default(),
        slots,
    );
    // Resolved before the loop: the closure below owns `ui` and `values`, and a label looked up from
    // `state` inside it would be a second borrow of the window.
    let edges = [
        ("set_text_top_padding", "Top"),
        ("set_text_right_padding", "Right"),
        ("set_text_bottom_padding", "Bottom"),
        ("set_text_left_padding", "Left"),
    ];
    let names: Vec<String> = edges.iter().map(|(key, fallback)| state.tr_or(key, fallback)).collect();

    let mut changed = false;
    for slot in 0..slots {
        let panel = options.mask_panels.get(slot).copied();
        let heading = match panel {
            Some((width, height)) => format!("#{} · {}×{}", slot + 1, width, height),
            None => format!("#{}", slot + 1),
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(heading).small().strong());
            for (edge, name) in names.iter().enumerate() {
                ui.label(RichText::new(name).small());
                let cell = &mut values[slot * 4 + edge];
                if ui.add(egui::DragValue::new(cell).range(0..=crate::settings::MASK_EDGE_MAX).suffix("%")).changed()
                {
                    changed = true;
                }
            }
            // What those four numbers mean on this panel, from the painter's own arithmetic.
            let band = windcap::crop::Urbl {
                top: values[slot * 4],
                right: values[slot * 4 + 1],
                bottom: values[slot * 4 + 2],
                left: values[slot * 4 + 3],
            };
            let note = match panel {
                Some((width, height)) if !band.is_empty() => {
                    let [top, right, bottom, left] =
                        band.pixel_band(&windcap::crop::Tile::whole_frame(width as u32, height as u32));
                    format!("−{}·{}·{}·{} px", top, right, bottom, left)
                }
                Some(_) => "−0 px".to_string(),
                None => String::new(),
            };
            if !note.is_empty() {
                ui.label(RichText::new(note).small().italics().weak());
            }
        });
    }
    if changed {
        let joined = values.iter().map(i64::to_string).collect::<Vec<_>>().join(", ");
        state.draft.set_text(field, &joined);
    }
}

fn assistant(state: &mut AppState, ui: &mut egui::Ui, out: &mut Vec<Command>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("AI").heading());
        ui.label(
            RichText::new("read by windai — natural-language search and monthly activity tags, not by this window")
                .small()
                .italics()
                .weak(),
        );
    });
    ui.label(
        RichText::new(
            "Fifteen keys, typed and bounded — the AI's seven, the tagger's two switches, the MCP \
             bridge's five and the summariser's idle switch. Every other \
             key in the config — the fifteen the Settings \
             page owns, the twenty-five Recording owns, and the AI keys this page leaves alone — \
             is written back untouched.",
        )
        .small()
        .italics(),
    );

    // `windai`'s own sentence, in `windai`'s own words. Painting the CLI's diagnostic rather than a
    // paraphrase of it is what stops the page and `windai doctor` from ever disagreeing about what a
    // valid configuration is.
    let (mark, color) = if state.ai_status.verdict.ok { ("✓", OK) } else { ("!", AMBER) };
    ui.label(RichText::new(format!("{mark} {}", state.ai_status.verdict.message)).color(color).small());
    // The key line is the one place a user looks to answer "did my save take?", and the state that
    // answers it most often on a real install is the placeholder the installer wrote. Red, not amber:
    // amber here would mean "something to read" and this means "nothing will work until you type it".
    let key_color = if state.ai_status.key.is_unusable() { RED } else { OK };
    ui.colored_label(
        key_color,
        RichText::new(format!(
            "open_ai_api_key: {}",
            state.ai_status.key.describe_in(&state.catalog)
        ))
        .small()
        .monospace(),
    );

    // The bridge's own answer, in the same place a user looks to ask "did my save take?". Which port it
    // listens on is the one thing on this page that a wrong number makes invisible: the service either
    // answers at the address printed here or an assistant gets nothing back, so the row says the
    // address, whether anything is answering it this second, and the URL to paste.
    {
        let bridge = &state.bridge;
        let (key, english) = bridge.state_row();
        let (mark, color) = match key {
            "ai_bridge_state_up" => ("✓", OK),
            "ai_bridge_state_refused" => ("!", RED),
            "ai_bridge_state_idle" => ("!", AMBER),
            _ => ("·", AMBER),
        };
        let state_text = state.tr_or(key, english);
        let line = if bridge.enabled {
            format!("mcp bridge: {state_text} · {} · {}", bridge.authority(), bridge.url)
        } else {
            format!("mcp bridge: {state_text}")
        };
        ui.colored_label(color, RichText::new(format!("{mark} {line}")).small().monospace());
        if let Some(refused) = &bridge.refused {
            // The service's sentence, word for word — the same string `windmcp serve` prints before it
            // exits, so the page and the process cannot describe the refusal differently.
            ui.colored_label(RED, RichText::new(refused).small());
        }
    }

    // The two AI promises this page cannot keep, said out loud rather than left as a key that
    // silently does nothing. Both are read as their upstream reader read them, so a stock install
    // shows neither.
    if state.ai.image_search_promised {
        ui.colored_label(
            AMBER,
            "enable_img_embed_search is on and img_embed_module_install says the embedding module \
             was installed, but no binary in this product reads either: there is no native \
             image-embedding index to search, so this page offers no switch for it and Save leaves \
             both values exactly where they are.",
        );
    }
    if state.ai.exclude_words > 0 {
        ui.label(
            RichText::new(format!(
                "windai also honours exclude_words ({} entries), which drops a window from the index \
                 before any feature sees it. That list is edited on the Settings page, not here.",
                state.ai.exclude_words
            ))
            .small()
            .weak(),
        );
    }

    for note in &state.ai_notes {
        ui.colored_label(AMBER, note);
    }
    if let Some(Err(report)) = &state.ai_test.report {
        ui.colored_label(RED, report);
    }
    if let Some(Ok(report)) = &state.ai_test.report {
        ui.colored_label(OK, report);
    }

    ui.horizontal(|ui| {
        if ui.button(state.tr_or("windui_web_save", "Save")).on_hover_text(SAVE_AI_HELP).clicked() {
            let (validated, notes) = state.ai_draft.validate(&state.ai);
            state.ai_notes = notes;
            state.pending_ai = Some(Box::new(validated.clone()));
            out.push(Command::SaveAi(Box::new(validated)));
        }
        if ui.button(state.tr_or("windui_revert", "Revert")).clicked() {
            state.ai_draft = crate::ai::AiDraft::from(&state.ai);
            state.ai_notes = Vec::new();
            // Revert rewrote the draft, and the status line describes the form that was there before.
            state.ai_status_for = u64::MAX;
        }
        if ui.button("Test connection").on_hover_text(TEST_AI_HELP).clicked() {
            let (validated, notes) = state.ai_draft.validate(&state.ai);
            state.ai_notes = notes;
            if let Some((id, settings)) = state.begin_ai_test(validated) {
                out.push(Command::TestAi { request_id: id, settings: Box::new(settings) });
            }
        }
        if ui.button("Clear stored key").on_hover_text(CLEAR_KEY_HELP).clicked() {
            state.ai_draft.clear_key();
        }
        if state.ai_test.track.pending {
            ui.spinner();
            ui.label(RichText::new("asking the endpoint…").small().italics().weak());
        }
        if !state.save_status.is_empty() {
            let status = state.save_status.clone();
            ui.label(RichText::new(status).small().monospace());
        }
    });
    ui.separator();

    let applied = state.ai.clone();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let mut group: Option<&str> = None;
        for field in crate::ai::AField::ALL {
            if group != Some(field.group()) {
                group = Some(field.group());
                ui.add_space(6.0);
                ui.label(RichText::new(state.tr_or(field.group_key(), field.group())).strong());
            }
            ui.push_id(field.key(), |ui| {
                ui.horizontal(|ui| {
                    let width = (ui.available_width() - 230.0).clamp(140.0, 340.0);
                    let label = state.tr_or(field.label_key(), field.label());
                    ui.add_sized([width, 20.0], egui::Label::new(label).selectable(false));
                    ai_widget(state, ui, field, &applied);
                    ui.label(RichText::new("?").weak()).on_hover_text(state.tr_or(field.help_key(), field.help()));
                });
            });
        }
    });

    prompt_panel(state, ui, out);
}

/// One row's editor, bound to the draft text rather than to the typed value.
///
/// The key row is the one that is not a `Kind` in this crate's other two forms: it draws through
/// `password(true)`, which makes egui lay out bullets instead of the characters — so the value never
/// reaches a galley, a paint command, or a screenshot, rather than merely being hidden behind a font.
/// Its box also starts empty rather than prefilled, which is what lets the page report a stored key
/// without displaying anything about it, not even how long it is.
fn ai_widget(state: &mut AppState, ui: &mut egui::Ui, field: crate::ai::AField, applied: &crate::ai::AiSettings) {
    use crate::ai::AKind;
    match field.kind(applied) {
        AKind::Int { min, max } => {
            let mut text = state.ai_draft.text(field).to_string();
            let response = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .desired_width(120.0)
                    .hint_text(&format!("{min}..{max}"))
                    .font(egui::TextStyle::Monospace),
            );
            if response.changed() {
                state.ai_draft.set_text(field, &text);
            }
        }
        AKind::Bool => {
            let mut on = state.ai_draft.bool_of(field);
            if ui.checkbox(&mut on, "").changed() {
                state.ai_draft.set_text(field, if on { "true" } else { "false" });
            }
        }
        AKind::Text { max_chars } => {
            let mut text = state.ai_draft.text(field).to_string();
            let response = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .desired_width(340.0)
                    .hint_text(&format!("up to {max_chars} characters")),
            );
            if response.changed() {
                state.ai_draft.set_text(field, &text);
            }
        }
        AKind::Secret { .. } => {
            let mut text = state.ai_draft.text(field).to_string();
            let response = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .desired_width(340.0)
                    .password(true)
                    .hint_text(if field == crate::ai::AField::McpToken {
                        "leave empty to keep the stored token"
                    } else {
                        "leave empty to keep the stored key"
                    }),
            );
            if response.changed() {
                state.ai_draft.set_text(field, &text);
            }
        }
        AKind::Lines { max_entries } => {
            let mut text = state.ai_draft.text(field).to_string();
            let response = ui.add(
                egui::TextEdit::multiline(&mut text)
                    .desired_rows(5)
                    .desired_width(340.0)
                    .hint_text(&format!("one phrase per line, at most {max_entries}")),
            );
            if response.changed() {
                state.ai_draft.set_text(field, &text);
            }
        }
        AKind::Choice(list) => {
            let mut value = state.ai_draft.text(field).to_string();
            egui::ComboBox::from_id_salt(field.key())
                .selected_text(if value.is_empty() { "—".to_string() } else { value.clone() })
                .width(200.0)
                .show_ui(ui, |ui| {
                    ui.set_max_width(280.0);
                    for option in &list {
                        ui.selectable_value(&mut value, option.clone(), option.as_str());
                    }
                    if list.is_empty() {
                        ui.label(RichText::new("the config names no endpoint dialect").small().italics());
                    }
                });
            // `selectable_value` writes through the borrow, so the answer is read back out of `value`
            // rather than inferred from which row happened to be clicked.
            state.ai_draft.set_text(field, &value);
        }
    }
}

const SAVE_AI_HELP: &str = "Stage these fifteen keys into the merged config and write \
     userdata/config_user.json through a temp file and a rename. windai is a separate process and \
     reads the file at its own start, so a value that lands here is the value the next search or tag \
     run sends. The API key is written to that file and nowhere else — not to argv, not to the \
     environment, not to a log.";

const TEST_AI_HELP: &str = "Send exactly one chat request to the endpoint this form is holding, \
     before anything is saved, and report what came back. It spends tokens on a hosted endpoint, and \
     it is the same client, transport and error redaction `windai doctor` uses. The reply line is \
     written with the key removed from it, in either percent-encoding spelling.";

const CLEAR_KEY_HELP: &str = "Empty the stored open_ai_api_key on the next Save. There is no \
     \"show current value\" on this page — an empty box normally means \"leave it alone\", and this \
     button is the only way to spell \"remove it\" without displaying it first.";

#[cfg(test)]
pub mod painted {
    use super::*;

    fn walk(shape: &Shape, texts: &mut Vec<String>, jobs: &mut Vec<LayoutJob>) {
        match shape {
            Shape::Vec(inner) => {
                for s in inner {
                    walk(s, texts, jobs);
                }
            }
            Shape::Text(text) => {
                texts.push(text.galley.job.text.clone());
                jobs.push((*text.galley.job).clone());
            }
            _ => {}
        }
    }

    /// Every string the frame laid out, in the order egui emitted it.
    pub fn strings(full: &egui::FullOutput) -> Vec<String> {
        let mut texts = Vec::new();
        let mut jobs = Vec::new();
        for clipped in &full.shapes {
            walk(&clipped.shape, &mut texts, &mut jobs);
        }
        texts
    }

    pub fn joined(full: &egui::FullOutput) -> String {
        strings(full).join("\n")
    }

    /// The laid-out jobs, so a test can look *inside* a label at its coloured sections.
    pub fn jobs(full: &egui::FullOutput) -> Vec<LayoutJob> {
        let mut texts = Vec::new();
        let mut jobs = Vec::new();
        for clipped in &full.shapes {
            walk(&clipped.shape, &mut texts, &mut jobs);
        }
        jobs
    }

    /// A frame's worth of geometry: how many shapes were produced at all.
    pub fn shape_count(full: &egui::FullOutput) -> usize {
        fn count(shape: &Shape) -> usize {
            match shape {
                Shape::Vec(inner) => inner.iter().map(count).sum::<usize>() + 1,
                _ => 1,
            }
        }
        full.shapes.iter().map(|c| count(&c.shape)).sum()
    }
}
