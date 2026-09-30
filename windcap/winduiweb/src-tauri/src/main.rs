//! `winduiweb` — the Windrecorder window drawn in HTML.
//!
//! A Tauri shell: the WebView2 window is the front end, and everything behind it is the same Rust the
//! egui window calls. The window draws; the answers come from `commands`, and through them from
//! `wind-ui` and `wind-store`. Nothing here knows what a month file is, and nothing in `commands`
//! knows what a pixel is.
//!
//! ## What this binary must keep answering
//!
//! `--version` and `--root` are the two lines the rest of the product depends on, so they are handled
//! before Tauri is even constructed:
//!
//!   * `--version` is scanned across every argument rather than matched on the first, because the tray
//!     appends `--root <install>` and a user pastes the line with flags in either order. It is the one
//!     question this binary can answer on an install whose config will not parse, so it must not need a
//!     root, a config or a webview.
//!   * `--root` means the same thing it means to the other eleven binaries, resolved by
//!     `wind_base::install` — the folder carrying `config_src/`. The tray launches exactly
//!     `bin\windui.exe --root <install>`, and a front end that resolved its install differently from the
//!     recorder that wrote the data would be two products in one folder.
//!
//! `--exit-after MS` exists for the release gate: `smoke.ps1` needs to start this window, prove a
//! window handle appeared, and get it closed again without anyone reaching for a mouse. A GUI binary
//! that can only be stopped by a click cannot be gated.
//!
//! ## Why a bad install still opens a window
//!
//! Startup does not load the config. If it did, a corrupt `userdata/config_user.json` would mean a
//! double-click that produced no window and no message — the exact failure shape the tray's empty-argv
//! bug had. Instead the root is kept and every command loads what it needs, so the first thing that
//! breaks reports itself through `about` as a sentence inside a window the user can see.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod background;
mod commands;
mod video;

use std::path::PathBuf;
use std::time::Duration;

use tauri::Manager as _;
use wind_base::version;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|argument| version::is_flag(argument)) {
        println!("{}", version::line("winduiweb", env!("CARGO_PKG_VERSION")));
        return;
    }
    let parsed = match parse(&argv) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}\n");
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    };

    let root = wind_base::install::resolve_root_from_exe(parsed.root);
    let builder = tauri::Builder::default()
        // The playback door. A webview cannot read a disk, so a `<video>` element needs some channel that
        // can, and `video` is this crate's own: a custom scheme that answers a *segment name* with the
        // bytes of that segment, in runs, which is what a seek is. Tauri's built-in `asset:` handler does
        // the same job behind a cargo feature this offline workspace cannot turn on — see the module
        // header — and the replacement costs no dependency, because `tauri` re-exports the `http` types
        // the handler speaks.
        .register_asynchronous_uri_scheme_protocol(video::SCHEME, video::serve)
        .manage(commands::State { root, tab: parsed.tab })
        .invoke_handler(tauri::generate_handler![
            commands::about,
            commands::startup_tab,
            commands::library_stats,
            commands::search,
            commands::day,
            commands::totals,
            commands::locate,
            commands::play_source,
            commands::play_prepare,
            commands::frame,
            commands::settings_read,
            commands::settings_save,
            commands::recording_read,
            commands::recording_save,
            commands::displays,
            commands::ai_read,
            commands::ai_save,
            commands::ai_test,
            commands::prompts_read,
            commands::prompt_save,
            commands::prompt_restore,
            commands::prompt_trial,
            commands::day_summaries,
            commands::summary_for_key,
            commands::lightbox,
            commands::word_cloud,
            commands::ui_strings,
            commands::ui_locale,
            commands::maintenance_start,
            commands::maintenance_stop,
            commands::maintenance_progress,
            commands::maintenance_backlog,
            commands::recorder_state
        ]);

    // The close button. `background::decide` is re-read on every click rather than once at startup,
    // because the settings page can change the answer and the tray can come or go while this window is up.
    let builder = builder.on_window_event(|window, event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            if window.label() != "main" {
                return;
            }
            let root = window.state::<commands::State>().root.clone();
            if background::decide(&root) == background::Close::Hide {
                api.prevent_close();
                let _ = window.hide();
            }
        }
    });

    if let Some(after) = parsed.exit_after {
        // On the builder, before `run`, so the timer belongs to the same event loop as the window and
        // `exit` is the runtime's own shutdown rather than a `Stop-Process` the gate would rather not
        // have to reach for.
        let builder = builder.setup(move |app| {
            raise_watcher(app)?;
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(after);
                handle.exit(0);
            });
            Ok(())
        });
        if let Err(error) = builder.run(tauri::generate_context!()) {
            eprintln!("winduiweb: {error}");
            std::process::exit(1);
        }
        return;
    }

    let builder = builder.setup(raise_watcher);
    if let Err(error) = builder.run(tauri::generate_context!()) {
        eprintln!("winduiweb: {error}");
        std::process::exit(1);
    }
}

/// Start the thread that waits for the tray to ask for this window back.
///
/// A hidden window is a promise the tray has to be able to keep, and the watcher is the other half of it:
/// without it, close-to-tray would be a one-way door.
fn raise_watcher(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let root = app.state::<commands::State>().root.clone();
    if let Some(window) = app.get_webview_window("main") {
        background::watch_for_show(root, move || {
            let _ = window.show();
            let _ = window.set_focus();
        });
    }
    Ok(())
}

struct Parsed {
    /// `None` until somebody says `--root` until somebody says `--root`: an empty path is not a default, it is the
    /// difference between the exe walk deciding and a blank string being handed to it as a choice.
    root: Option<PathBuf>,
    exit_after: Option<Duration>,
    tab: Option<String>,
}

fn parse(args: &[String]) -> Result<Parsed, String> {
    // Distinguish "nobody said" from "somebody said it wrong" the same way `windui` does: once a
    // default has been written over it cannot be told apart from an explicit choice, and this binary
    // resolves a root three ways.
    let mut root: Option<PathBuf> = None;
    let mut exit_after: Option<Duration> = None;
    let mut tab: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        let (key, inline) = match args[index].split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (args[index].as_str(), None),
        };
        let mut value = |what: &str| -> Result<String, String> {
            if let Some(found) = inline.clone() {
                return Ok(found);
            }
            index += 1;
            args.get(index).cloned().ok_or_else(|| format!("{what} needs a value"))
        };
        match key {
            "--root" => root = Some(PathBuf::from(value("--root")?)),
            "--tab" => tab = Some(value("--tab")?),
            "--exit-after" => {
                let ms: u64 = value("--exit-after")?.parse().map_err(|_| format!("--exit-after wants a whole number of milliseconds, not {:?}", args.get(index)))?;
                exit_after = Some(Duration::from_millis(ms));
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
        index += 1;
    }
    Ok(Parsed { root, exit_after, tab })
}

fn usage() -> String {
    format!(
        "usage: winduiweb [--root PATH] [--exit-after MS] [--version]\n\
         \x20 --root        the install to show. Default: the folder carrying config_src/, found by\n\
         \x20                 walking up from this executable — so a tray launch from bin\\ settles on\n\
         \x20                 the install and not on bin\\.\n\
         \x20 --exit-after  close the window after this many milliseconds. For the release gate.\n\
         \x20 --version     print this binary's name, package version and build profile, and read\n\
         \x20                 nothing at all — it answers on a broken install too.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// The two spellings of each flag arrive at the same answer, because the tray uses the spaced
    /// form and a person at a terminal types whichever one they remember.
    #[test]
    fn both_flag_spellings_parse() {
        let spaced = parse(&argv(&["--root", "E:/install", "--exit-after", "250"])).expect("parses");
        let inline = parse(&argv(&["--root=E:/install", "--exit-after=250"])).expect("parses");
        assert_eq!(spaced.root, inline.root);
        assert_eq!(spaced.exit_after, inline.exit_after);
        assert_eq!(spaced.exit_after, Some(Duration::from_millis(250)));
    }

    /// A misspelled flag is a usage error, not a shrug: the release gate depends on this binary
    /// refusing loudly, because a silently ignored `--exit-after` is a window that never closes and a
    /// smoke run that hangs until something kills it.
    #[test]
    fn an_unknown_flag_is_refused_rather_than_ignored() {
        assert!(parse(&argv(&["--rot", "E:/install"])).is_err());
        assert!(parse(&argv(&["--exit-after", "soon"])).is_err());
        assert!(parse(&argv(&["--exit-after"])).is_err(), "a flag with no value is not a default");
    }

    #[test]
    fn no_arguments_still_resolves_a_root_later() {
        let parsed = parse(&argv(&[])).expect("an empty argv is legal");
        assert_eq!(parsed.root, None, "the exe walk decides, not a guess here");
        assert_eq!(parsed.exit_after, None);
    }
}
