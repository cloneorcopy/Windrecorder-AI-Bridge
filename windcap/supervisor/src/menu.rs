//! The menu: its rows, their labels, what is enabled, and what clicking does.
//!
//! A notification-area menu is built fresh every time it is opened — pystray's `menu_callback` is a
//! function for exactly this reason, and `TrackPopupMenu` wants the same — so this module is pure:
//! it takes a `Snapshot` of the supervised world and returns the rows. That is what makes the
//! state→label table testable without a desktop, and it is where the mapping is written down once
//! rather than in seven lambdas.

use wind_base::i18n::Catalog;

/// Menu identifiers handed to `AppendMenuW`, above `WM_USER` so no system id can collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Command {
    FlagNow = 0x4000,
    ToggleInterface = 0x4001,
    /// The default item, and the double-click target. With the interface a window there is no
    /// address to raise in a browser, so the row says that in words instead of opening one.
    OpenInterface = 0x4002,
    ToggleRecord = 0x4003,
    /// The tray's own version, compiled into the `.exe`. Display-only: this used to be the "Update"
    /// item that launched `install_update.bat`, but the updater it launched died with the Python
    /// application, and a row that can only ever fail is not a feature worth leaving gray. The id
    /// is kept so nothing downstream has to reason about a renumbered menu.
    Version = 0x4004,
    /// "See what's new" — opens the changelog this install actually carries (`RELEASE-NOTES.txt`
    /// from the zip, or `CHANGELOG.md` in a checkout), and is only ever built when that file is on
    /// disk. Upstream showed this row only beside an available update; with no updater left, the
    /// row is the honest half of what remains — a description of the build you are holding.
    Changelog = 0x4005,
    Exit = 0x4006,
    /// The MCP bridge's state. Display-only and never dispatched: the switch that
    /// controls it is `enable_mcp_server` in the config, not a click, and a menu item that looked
    /// clickable but refused to change anything would be a worse lie than a grayed row.
    BridgeState = 0x4008,
    /// The capture state, in the recorder's own words. Display-only for the same reason as
    /// [`Command::BridgeState`]: nothing about it is a click away — the screen either changed or it
    /// did not — and the row exists because the *action* row below it can only ever say 暂停记录 or
    /// 开始记录, which is a sentence about the process, not about whether frames are being written.
    RecordState = 0x4009,
}

/// The menu identifiers, in the order they are declared.
const ALL_COMMANDS: [Command; 9] = [
    Command::FlagNow,
    Command::ToggleInterface,
    Command::OpenInterface,
    Command::ToggleRecord,
    Command::Version,
    Command::Changelog,
    Command::Exit,
    Command::BridgeState,
    Command::RecordState,
];

impl Command {
    /// Map a `TrackPopupMenuEx` return value back to what was clicked.
    ///
    /// Written as a search rather than a `transmute`: an id this table does not know is a value that
    /// must not become a command.
    pub fn from_id(id: u32) -> Option<Command> {
        ALL_COMMANDS.into_iter().find(|command| *command as u32 == id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub command: Command,
    pub label: String,
    /// Grayed out, not hidden: upstream's `enabled=` keeps "Add mark for now" visible while nothing
    /// is recording, so the user can see the item exists and is waiting for the recorder.
    pub enabled: bool,
    /// The item Enter activates, and the one a double-click on the icon runs.
    pub default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Item(Item),
    Separator,
}

impl Row {
    pub fn as_item(&self) -> Option<&Item> {
        match self {
            Row::Item(item) => Some(item),
            Row::Separator => None,
        }
    }
}

impl Item {
    fn row(command: Command, label: String, enabled: bool, default: bool) -> Row {
        Row::Item(Item { command, label, enabled, default })
    }
}

/// Everything the labels are a function of. Built by the supervisor each time the menu opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Whether a recording is running, read from the recorder's lock and its process rather than
    /// from a flag set when the item was last clicked. See `Supervisor::snapshot`.
    pub recording: bool,
    pub interface_running: bool,
    pub current_version: String,
    /// Whether this install carries a changelog the "See what's new" row could actually open —
    /// [`crate::layout::Layout::changelog_target`] resolving to something on disk. The row exists
    /// exactly when this is true, so no click can point at a missing file.
    pub changelog_present: bool,
    /// Whether `enable_mcp_server` is on. While it is off the menu says nothing about the bridge at
    /// all: a row for a service nobody asked for is noise, and the off case is what
    /// `windsvc doctor` is for.
    pub bridge_enabled: bool,
    /// Whether a bridge is up — one this tray started, or one a live lock names.
    pub bridge_running: bool,
    /// What the recorder said it was doing on its last tick, or `None` when it never said.
    ///
    /// Read from `cache\locks\RECORD_STATE.MD`, which only the recorder writes. `None` is a real state
    /// and not a failure to look: it is a recorder built before this file existed, or one that could
    /// not write it, and the row then says 正在记录 because a live lock is all the tray is entitled to
    /// claim — never more.
    pub capture: Option<wind_base::fslock::Capture>,
}

impl Snapshot {
    /// A tray that has started nothing yet.
    pub fn idle(current_version: String) -> Snapshot {
        Snapshot {
            recording: false,
            interface_running: false,
            current_version,
            changelog_present: false,
            bridge_enabled: false,
            bridge_running: false,
            capture: None,
        }
    }
}

/// The key behind the start/stop-recording label — the whole of "the text agrees with what the
/// recorder is doing".
pub fn record_toggle_key(recording: bool) -> &'static str {
    if recording { "tray_record_stop" } else { "tray_record_start" }
}

/// The key behind the capture-state row and the hover text, in the recorder's own words.
///
/// Five answers, because there are five things worth telling apart: nothing of ours is running; a
/// recorder is running and grabbing frames; it is holding off because the screen has not changed; it
/// cannot see a picture at all because the session is locked; and the machine slept through the tick.
/// The last three all leave the lock held, which is exactly why the lock was never a usable answer.
pub fn capture_state_key(recording: bool, capture: Option<wind_base::fslock::Capture>) -> &'static str {
    use wind_base::fslock::Capture;
    if !recording {
        return "tray_state_off";
    }
    match capture {
        Some(Capture::ScreenUnchanged) => "tray_state_idle",
        Some(Capture::SessionLocked) => "tray_state_locked",
        Some(Capture::SleepDrift) => "tray_state_sleep",
        // Never published (an older recorder, or a write that failed): claim only what the lock proves.
        _ => "tray_state_capturing",
    }
}

/// The key behind the interface start/stop label.
///
/// This was four-way while it had to name both the state and the implementation — `webui` for a
/// Streamlit server, `window (native)` for a window. The server was deleted along with the
/// interpreter that ran it, so the two labels that remain are the only true ones, and the menu
/// cannot promise a port to a user who has no address to open.
pub fn interface_toggle_key(running: bool) -> &'static str {
    if running { "tray_native_window_exit" } else { "tray_native_window_start" }
}

/// The key behind the MCP bridge's state row. Two-way, not four, because there is no second
/// implementation of a bridge to be: either the process the tray started is up or it is not.
pub fn bridge_state_key(running: bool) -> &'static str {
    if running { "tray_mcp_running" } else { "tray_mcp_stopped" }
}

/// The hover text. The recording half is the same truth the menu row shows; the interface half names
/// what is up, which is now always a window — the port a `webui` suffix would have advertised is
/// the thing this tray can no longer start.
pub fn tooltip(snapshot: &Snapshot, catalog: &Catalog) -> String {
    let base = catalog.text(capture_state_key(snapshot.recording, snapshot.capture));
    if !snapshot.interface_running {
        return base;
    }
    let suffix = "tray_tip_interface_native";
    // The raccoon is upstream's separator, kept because it is the only visual break in 63 characters.
    format!("{base} · 🦝 {}", catalog.text(suffix))
}

/// The notification the icon raises when it appears and whenever recording starts or stops.
pub fn balloon(snapshot: &Snapshot, catalog: &Catalog) -> (String, String) {
    if snapshot.recording {
        (catalog.text("tray_notify_title"), catalog.text("tray_notify_text"))
    } else {
        (catalog.text("tray_notify_title_record_pause"), catalog.text("tray_notify_text_start_without_record"))
    }
}

/// The rows, in the order `main.py::menu_callback` returns them.
pub fn rows(snapshot: &Snapshot, catalog: &Catalog) -> Vec<Row> {
    let mut rows = vec![
        Item::row(
            Command::FlagNow,
            catalog.text("tray_add_flag_mark_note_for_now"),
            snapshot.recording,
            false,
        ),
        Row::Separator,
        Item::row(
            Command::ToggleInterface,
            catalog.text(interface_toggle_key(snapshot.interface_running)),
            true,
            false,
        ),
    ];
    if snapshot.interface_running {
        // The row that used to announce there was no address to open. It was inherited from upstream,
        // whose default item handed a URL to a browser, and it stayed true *as a statement* after the
        // browser was gone — but a window is openable, and this row is the default item and the
        // double-click target. So it now says what it does: bring that window forward, whether it is
        // behind another or hidden by its own close button.
        rows.push(Item::row(Command::OpenInterface, catalog.text("tray_native_window_raise"), true, true));
    }
    if snapshot.bridge_enabled || snapshot.bridge_running {
        // The bridge listens on a port other people's tools can reach, so whether it is up is not a
        // detail to go and re-derive from `netstat`: it is on the menu. Shown for the running case
        // even when the switch has since gone off — a process still holding a port is the one state
        // a user must never be able to lose sight of by editing a config file. Grayed either way,
        // because the switch that moves it is `enable_mcp_server` and a click that appeared to
        // change a network setting it could not change would be the worse design.
        rows.push(Item::row(
            Command::BridgeState,
            catalog.text(bridge_state_key(snapshot.bridge_running)),
            false,
            false,
        ));
    }
    // The state, in its own row, grayed: the action below can only ever name the process ("stop this
    // recorder" / "start one"), and a user who hovers an icon that says 暂停记录 while nothing of ours
    // is running has no way to tell that from a recorder that is alive and holding off because the
    // screen did not change. This row is the difference, and it is the recorder's own answer rather
    // than the tray's guess from a lock file.
    rows.push(Item::row(
        Command::RecordState,
        catalog.text(capture_state_key(snapshot.recording, snapshot.capture)),
        false,
        false,
    ));
    rows.push(Item::row(Command::ToggleRecord, catalog.text(record_toggle_key(snapshot.recording)), true, false));
    rows.push(Row::Separator);
    // The version, as a label: grayed because this tray has no updater to launch and no remote to
    // compare against, and truthful because the number is the one `windsvc --version` answers with.
    // Upstream's no-update-available state showed the same row in the same words — the offer to
    // update is gone, the statement of what is running is not.
    rows.push(Item::row(Command::Version, catalog.formatted("tray_version_info", &[("version", &snapshot.current_version)]), false, false));
    if snapshot.changelog_present {
        // "See what's new" opens the changelog file this install carries — the release notes that
        // shipped in the zip, or a checkout's `CHANGELOG.md` — never a URL and never a file that is
        // not on disk. Upstream reached for this row only when an update was pending; with the
        // updater deleted, the row still has a true thing to do: describe the build in front of the
        // user, which is precisely what both files do.
        rows.push(Item::row(Command::Changelog, catalog.text("tray_updatelog"), true, false));
    }
    rows.push(Item::row(Command::Exit, catalog.text("tray_exit"), true, false));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).map(Path::to_path_buf).unwrap()
    }

    fn catalog(lang: &str) -> Catalog {
        let c = Catalog::load(&repo_root(), lang);
        assert!(c.loaded(), "the shipped languages.json must be readable: {:?}", c.read_error);
        c
    }

    fn snapshot(recording: bool, running: bool) -> Snapshot {
        let mut snap = Snapshot::idle("0.0.31".into());
        snap.recording = recording;
        snap.interface_running = running;
        snap
    }

    fn find(rows: &[Row], command: Command) -> Option<&Item> {
        rows.iter().filter_map(|row| row.as_item()).find(|item| item.command == command)
    }

    #[test]
    fn the_recording_state_drives_the_record_label_both_ways() {
        assert_eq!(record_toggle_key(false), "tray_record_start");
        assert_eq!(record_toggle_key(true), "tray_record_stop");
        let c = catalog("en");
        let built = rows(&snapshot(true, false), &c);
        assert_eq!(find(&built, Command::ToggleRecord).unwrap().label, "⏸️ Pause Recording");
        let built = rows(&snapshot(false, false), &c);
        assert_eq!(find(&built, Command::ToggleRecord).unwrap().label, "▶️ Start Recording");
    }

    /// This test was four-way while the label had to name both the state and the implementation, and
    /// two of those four arms were `tray_webui_start` / `tray_webui_exit`. There is one interface left
    /// to launch, so the question is no longer "which of four was chosen" but "can the menu choose a
    /// webui label at all" -- and it can not, in either language it ships.
    #[test]
    fn the_interface_label_names_the_window_and_can_never_name_a_webui() {
        assert_eq!(interface_toggle_key(false), "tray_native_window_start");
        assert_eq!(interface_toggle_key(true), "tray_native_window_exit");
        let c = catalog("en");
        for running in [false, true] {
            let built = rows(&snapshot(false, running), &c);
            let item = find(&built, Command::ToggleInterface).unwrap();
            let expected = if running { "🦝 Stop the search/settings window" } else { "🦝 Start the search/settings window" };
            assert_eq!(item.label, expected, "running={running}");
            assert!(!item.label.contains("webui"), "the menu advertises a server that cannot start: {}", item.label);
        }
        for locale in ["en", "sc", "ja"] {
            let catalog = catalog(locale);
            for key in ["tray_webui_start", "tray_webui_exit", "tray_tip_interface_webui"] {
                let unreachable = catalog.text(key);
                assert!(!rows(&snapshot(false, true), &c).iter().any(|row| row.as_item().map(|i| i.label.clone()) == Some(unreachable.clone())),
                        "{locale}'s {key} leaked into a native menu");
            }
        }
    }

    #[test]
    fn nothing_is_running_so_the_menu_describes_the_next_click() {
        let c = catalog("en");
        let built = rows(&snapshot(false, false), &c);
        assert!(find(&built, Command::OpenInterface).is_none(), "an interface that is down has no row about it");
        assert!(find(&built, Command::Changelog).is_none(), "a root with no changelog on disk has no row that opens one");
        assert_eq!(find(&built, Command::Version).unwrap().label, "🚀 Version 0.0.31");
        assert!(!find(&built, Command::Version).unwrap().enabled, "the version is a label; this tray ships no updater to launch");
        assert!(!find(&built, Command::FlagNow).unwrap().enabled, "a mark with nothing recording marks nothing");
        assert!(find(&built, Command::FlagNow).is_some(), "but the item stays visible while it waits");
    }

    #[test]
    fn a_running_window_offers_to_bring_it_forward() {
        let c = catalog("en");
        let built = rows(&snapshot(true, true), &c);
        let open = find(&built, Command::OpenInterface).expect("open row");
        assert_eq!(open.label, "🗔 Bring the window forward");
        // Clickable, and the default item: with close-to-tray on, this is the only gesture that undoes a
        // hidden window, so a grayed row here would strand the user's own install.
        assert!(open.enabled, "there is a window to raise");
        assert!(open.default, "and the double-click runs it");
    }

    /// Replaces `a_running_webui_shows_both_addresses`, which proved the menu could carry a scraped
    /// `Local URL:` row and a LAN row under it. The server that printed those lines is gone, so what is
    /// provable is the stronger statement: in no state, in no locale, does any row of this menu put an
    /// address or the word `webui` in front of a user.
    #[test]
    fn no_row_of_the_menu_offers_an_address_in_any_state_or_locale() {
        for locale in ["en", "sc"] {
            let c = catalog(locale);
            for recording in [false, true] {
                for running in [false, true] {
                    for changelog in [false, true] {
                        for bridge in [(false, false), (true, false), (true, true)] {
                            let mut snap = snapshot(recording, running);
                            snap.changelog_present = changelog;
                            snap.bridge_enabled = bridge.0;
                            snap.bridge_running = bridge.1;
                            let built = rows(&snap, &c);
                            assert!(
                                ALL_COMMANDS.iter().all(|command| *command as u32 != 0x4007),
                                "the LAN-address id is gone from the enum, not merely unrowed"
                            );
                            for row in built.iter().filter_map(|row| row.as_item()) {
                                let lower = row.label.to_ascii_lowercase();
                                assert!(!lower.contains("http"), "{locale} {changelog} {bridge:?}: {}", row.label);
                                assert!(!lower.contains("webui"), "{locale} {changelog} {bridge:?}: {}", row.label);
                                assert!(!lower.contains("lan address"), "{locale} {changelog} {bridge:?}: {}", row.label);
                            }
                        }
                    }
                }
            }
        }
    }

    /// The updater's deletion, asserted from the menu side: in no state, in no locale, does any row
    /// take the words the offer used to carry. The offer itself was `tray_update_cta` — "Update to
    /// new version: {version}" — and its label is built here from the shipped catalog rather than
    /// typed, so a relabeling of the row cannot slip the claim back in under different letters.
    #[test]
    fn the_menu_never_offers_an_update_it_cannot_perform() {
        for locale in ["en", "sc", "ja"] {
            let c = catalog(locale);
            let offer = c.formatted("tray_update_cta", &[("version", "9.9.9")]);
            let offer_current = c.formatted("tray_update_cta", &[("version", "0.0.31")]);
            for changelog in [false, true] {
                for recording in [false, true] {
                    let mut snap = snapshot(recording, false);
                    snap.changelog_present = changelog;
                    let built = rows(&snap, &c);
                    let labels: Vec<&str> = built.iter().filter_map(|row| row.as_item()).map(|item| item.label.as_str()).collect();
                    assert!(!labels.contains(&offer.as_str()) && !labels.contains(&offer_current.as_str()), "{locale}: the update offer is back: {labels:?}");
                    let version = find(&built, Command::Version).expect("the version row is always built");
                    assert!(!version.enabled, "{locale}: the version row became clickable — there is nothing it could launch");
                }
            }
        }
    }

    /// "See what's new" exists exactly when there is something to see on disk, and opens it; it is
    /// never a row whose target the install does not carry.
    #[test]
    fn the_changelog_row_appears_only_when_its_target_is_a_real_file() {
        let c = catalog("en");
        let mut with = snapshot(false, false);
        with.changelog_present = true;
        let built = rows(&with, &c);
        let row = find(&built, Command::Changelog).expect("a root that carries a changelog shows the row");
        assert_eq!(row.label, "🚀 See what's new");
        assert!(row.enabled, "opening a file the install carries is a real action");
        let without = rows(&snapshot(false, false), &c);
        assert!(find(&without, Command::Changelog).is_none());
    }

    #[test]
    fn the_menu_ends_with_exit_and_holds_the_two_documented_dividers() {
        let c = catalog("en");
        let built = rows(&snapshot(false, false), &c);
        assert_eq!(find(&built, Command::Exit).unwrap().label, c.text("tray_exit"));
        match built.last().unwrap() {
            Row::Item(item) => assert_eq!(item.command, Command::Exit, "the last thing in the menu must be Exit"),
            Row::Separator => panic!("the menu must end on Exit, not on a divider"),
        }
        assert_eq!(built.iter().filter(|r| matches!(r, Row::Separator)).count(), 2);
    }

    #[test]
    fn the_tooltip_names_the_window_only_while_one_is_up() {
        let c = catalog("en");
        assert_eq!(tooltip(&snapshot(false, false), &c), "🎥 Windrecorder — not recording");
        assert_eq!(tooltip(&snapshot(true, false), &c), "🎥 Windrecorder — recording");
        assert_eq!(tooltip(&snapshot(true, true), &c), "🎥 Windrecorder — recording · 🦝 window is up");
        assert_eq!(tooltip(&snapshot(false, true), &c), "🎥 Windrecorder — not recording · 🦝 window is up");
        assert!(
            !tooltip(&snapshot(false, true), &c).contains("webui"),
            "a hover text pointing at a port is exactly the lie the webui suffix told"
        );
    }

    /// The whole point of `RECORD_STATE.MD`: four different things a live record lock used to mean the
    /// same thing about. The lock alone can tell "our recorder is running" from "nothing is", and that
    /// is all it has ever been able to say — which is how a machine that has not changed its screen for
    /// an hour and a machine whose recorder was never started could both be described by the icon.
    #[test]
    fn the_capture_state_separates_a_paused_recorder_from_no_recorder_at_all() {
        use wind_base::fslock::Capture;
        let c = catalog("en");
        for (state, expected) in [
            (Capture::Capturing, "🎥 Windrecorder — recording"),
            (Capture::ScreenUnchanged, "🎥 Windrecorder — holding off: the screen has not changed"),
            (Capture::SessionLocked, "🎥 Windrecorder — holding off: the screen is locked"),
            (Capture::SleepDrift, "🎥 Windrecorder — holding off: the machine just woke up"),
        ] {
            let mut snap = snapshot(true, false);
            snap.capture = Some(state);
            assert_eq!(tooltip(&snap, &c), expected, "{state:?}");
            // The action row is unchanged by all of this: stopping a recorder that is holding off is
            // still a thing a user can ask for, and it is the only action the tray has on this subject.
            assert_eq!(find(&rows(&snap, &c), Command::ToggleRecord).unwrap().label, "⏸️ Pause Recording");
        }

        // Nothing published — an older recorder, or a write that failed — and the tray claims only what
        // the lock proves, never more.
        let mut unknown = snapshot(true, false);
        unknown.capture = None;
        assert_eq!(tooltip(&unknown, &c), "🎥 Windrecorder — recording");
        // And a dead recorder is not "holding off", whatever its last file said.
        let mut dead = snapshot(false, false);
        dead.capture = Some(Capture::Capturing);
        assert_eq!(tooltip(&dead, &c), "🎥 Windrecorder — not recording", "a stale file cannot outlive the lock");
    }

    #[test]
    fn the_balloon_follows_the_recording_state() {
        let c = catalog("en");
        let (title, body) = balloon(&snapshot(true, false), &c);
        assert_eq!((title.as_str(), body.as_str()), ("Windrecorder is recording", "Use the right-click menu on tray to Pause Recording or Rewind Memories"));
        let (title, _) = balloon(&snapshot(false, false), &c);
        assert_eq!(title, "Windrecorder has paused recording");
    }

    /// The Simplified Chinese rows, spelled as upstream's `main.py` spelled them — with one deliberate
    /// exception. Upstream wrote "（原生）" because a second, non-native front end existed to tell it
    /// apart from; that window left the shipped set on 2026-09-27
    /// (`docs/adr/2026-09-27-winduiweb-is-the-only-interface.md`), so the qualifier now distinguishes
    /// nothing and would only mislead. Everything else stays byte-for-byte what users had.
    #[test]
    fn the_simplified_chinese_labels_are_the_ones_main_py_shows() {
        let c = catalog("sc");
        let built = rows(&snapshot(true, true), &c);
        let got: Vec<&str> = built.iter().filter_map(|r| r.as_item()).map(|i| i.label.as_str()).collect();
        assert_eq!(
            got,
            vec![
                "🚩 为现在时间添加标记",
                "🦝 关闭搜索/设置窗口",
                "🗔 把窗口调到前台",
                // The state row, above the action row: what the recorder is doing, in its own words.
                "🎥 捕风记录仪 - 正在记录",
                "⏸️ 暂停记录",
                // The version row no longer claims "已是最新版" ("already the latest version") — the
                // remote version poll was removed, so nothing in this install can verify that, and a
                // menu row may not assert what the code cannot know. See `base/src/i18n.rs`.
                "🚀 版本 0.0.31",
                "❌ 退出",
            ]
        );
    }

    #[test]
    fn an_unknown_locale_still_builds_the_whole_menu_in_english() {
        // `d_lang[config.lang]` raises for a locale the file does not hold; the tray falls back on
        // `en` per label, so someone who typed `zh` into the settings field still gets a working menu.
        let c = catalog("zz");
        let built = rows(&snapshot(true, false), &c);
        assert_eq!(built.iter().filter(|r| matches!(r, Row::Item(_))).count(), 6);
        assert_eq!(find(&built, Command::Exit).unwrap().label, "❌ Exit");
    }

    fn bridge_snapshot(enabled: bool, running: bool) -> Snapshot {
        let mut snap = Snapshot::idle("0.0.31".into());
        snap.bridge_enabled = enabled;
        snap.bridge_running = running;
        snap
    }

    /// The whole point of the row: a switch wired to nothing was the defect, so the menu has to say
    /// which of the three states the bridge is in — off, on and up, on and down.
    #[test]
    fn the_bridge_row_names_its_state_and_only_applies_to_a_bridge_someone_asked_for() {
        let c = catalog("en");
        assert!(find(&rows(&bridge_snapshot(false, false), &c), Command::BridgeState).is_none(), "off says nothing");
        let off = rows(&bridge_snapshot(true, false), &c);
        let stopped = find(&off, Command::BridgeState).expect("an enabled bridge has a row");
        assert_eq!(stopped.label, "🤖 MCP AI bridge is enabled but not running");
        let on = rows(&bridge_snapshot(true, true), &c);
        let running = find(&on, Command::BridgeState).expect("a running bridge has a row");
        assert_eq!(running.label, "🤖 MCP AI bridge is running");
        for item in [stopped, running] {
            assert!(!item.enabled, "the switch is a config key, not a click");
            assert!(!item.default, "it is never the double-click target");
        }
    }

    /// The state the row exists to prevent: a listener still holding a port, invisible because the
    /// config was edited underneath a running tray.
    #[test]
    fn a_bridge_still_up_still_has_a_row_after_its_switch_goes_off() {
        let c = catalog("en");
        let built = rows(&bridge_snapshot(false, true), &c);
        let row = find(&built, Command::BridgeState).expect("a live process must not become invisible");
        assert_eq!(row.label, "🤖 MCP AI bridge is running");
    }

    /// `windsvc doctor` proves what happens when a tray label has no translation: the menu prints
    /// `(tray_mcp_running) not found in i18n, please feedback to contributors.` to the user.
    #[test]
    fn every_bridge_label_is_translated_in_every_locale_the_app_ships() {
        for locale in ["en", "sc", "ja"] {
            let c = catalog(locale);
            for key in ["tray_mcp_running", "tray_mcp_stopped"] {
                let text = c.text(key);
                assert!(!text.starts_with(&format!("({key})")), "{locale}/{key} is untranslated: {text}");
                assert!(text.starts_with('🤖'), "{locale}/{key} lost its menu prefix: {text}");
            }
        }
    }

    /// The two rows that outlived the updater are fully translated in every shipped locale — the
    /// version label and "See what's new" are the only things the menu still says about releases,
    /// and an untranslated `(key) not found` line there would be the report card of a tray that
    /// ships rows it did not proofread.
    #[test]
    fn every_release_label_is_translated_in_every_locale_the_app_ships() {
        for locale in ["en", "sc", "ja"] {
            let c = catalog(locale);
            for key in ["tray_updatelog", "tray_version_info"] {
                let text = c.text(key);
                assert!(!text.starts_with(&format!("({key})")), "{locale}/{key} is untranslated: {text}");
                assert!(text.starts_with('🚀'), "{locale}/{key} lost its menu prefix: {text}");
            }
        }
    }
}
