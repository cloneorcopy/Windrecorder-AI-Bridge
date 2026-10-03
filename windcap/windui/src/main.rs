//! `windui` — the native front end for the parts of Windrecorder a person actually uses.
//!
//! It replaces the WebUI's Search and OneDay tabs, plus the settings those two screens read, plus —
//! since [`ai`] — the Lab tab that was the only place the AI features could be configured at all, with
//! one window and no browser. The index it shows is the same monthly SQLite the recorder writes and
//! the Python app still reads: nothing here invents a format, and nothing here opens a live month
//! file (`wind-store` hands every read through a `_TEMP_READ.db` copy, which is the only reason a
//! five-second search cannot lose a recording segment).
//!
//! Shape of the code, and the one rule it exists to enforce: the frame loop draws, everything else
//! reads. `model` is plain data, `view` is a projection of it, `backend` is the only code that
//! touches a disk, `workers` is the only code that touches another thread. That is what makes the
//! headless tests in `view` able to call the real render loop and assert on the shapes it produced.
//!
//! ## Not ported from the Python screens, and why
//!
//! Each of these was looked at and left behind on purpose. A feature that is missing because nobody
//! thought of it is a gap; these are decisions, and the list is here so the next reader does not
//! "restore" one.
//!
//! * **The password gate** (`webui.py:105-115`, md5 of a `webui_access_password_md5`). It exists to
//!   stop a *browser* on a shared machine from opening the page. A native window is not a URL: the
//!   operating system's own session lock is the gate now, and a second one that the user must type
//!   past adds nothing but a forgotten hash.
//! * **Custom CSS and background injection** (`inject_custom_css`, `custom_background_filepath`).
//!   Streamlit cannot be themed any other way. `egui` exposes `Visuals` directly, and a UI that
//!   repaints the user's desktop behind the window was never a feature worth the render-blocking
//!   `<style>` tag.
//! * **The `st.session_state` lazy-diff machinery** (`search_content_lazy`, `*_lazy` everywhere).
//!   Its whole job was to work around Streamlit re-running the entire script on every keystroke. A
//!   retained-mode UI runs once per frame by construction, so the diff is not optimised away here —
//!   it is simply not needed, and the monotonic request id in `model` replaces the one part of it
//!   that was doing real work.
//! * **The Lab tab's disabled selects and `enable_ai_day_poem`.** The setting is initialised and
//!   never read, the widgets are rendered `disabled=True`, and those two controls are therefore a
//!   live bug in the Python app rather than a feature. Porting a bug faithfully is not
//!   compatibility. The tab itself is *not* on this list: it is here as [`model::Tab::Ai`], which
//!   exists because Lab was the only place an `open_ai_base_url`, a key, a model, a tag limit or a
//!   filter word could be set at all, and `windai` — the binary that reads every one of them — ships
//!   in `bin/` with no surface in front of it. What was left out of that port, and why, is in
//!   [`ai`]'s module header.
//! * **`use_random_search`.** Only ever set from the commented-out 🎲 toggle, so every branch that
//!   tests it is dead, including the `if st.session_state.use_random_search else input_value` that
//!   makes the keyword box look conditional.
//! * **The `_cropped` and `-today-.png` cache sweepers** (`oneday.py:288-295`, and the `"_cropped"
//!   in file` filter in `find_closest_video_by_filesys`). They exist because upstream *materialises*
//!   a day's timeline as a PNG on disk and has to garbage-collect it. This paints thumbnails
//!   directly into a bounded texture cache; there is no file to sweep.
//! * **`is_videofile_exist`, `is_picturefile_exist` and `picturefile_name` in the result model.**
//!   The WebUI carried all three into its dataframe, showed one as an untouchable checkbox, dropped
//!   two (`db_refine_search_data_global`'s `df.drop(columns=[...])`) and then recomputed on-disk
//!   presence from a directory listing anyway. What the model carries is that recomputed answer: a
//!   resolved segment path.
//! * **Per-rerun whole-video byte reads** (`open(video_filepath,"rb").read()` into `st.video`).
//!   Upstream slurped the entire MP4 into memory on every Streamlit rerun to hand it to a `<video>`
//!   tag. A native UI has a player; what it needs from the index is the file and the second to seek
//!   to, which is what `Row::offset_in_segment` is for. The Locate action is that pair, honestly
//!   labelled.
//! * **The PIL timeline collage** (`generate_preview_timeline_img`, and the `wordcloud` /
//!   `image_to_base64` path around it). One PNG per day, written into `result_timeline`, re-stitched
//!   when its mtime went stale, with a base64 round trip to get it back onto the page. The strip in
//!   `view` blits the same thumbnails straight from the cache.

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod scratch;
mod app;
mod textures;
mod thumbs;
mod view;
mod workers;
#[cfg(test)]
mod render_tests;

// The data half of this window moved into the crate's library target so the Tauri front end can
// share one copy of it — see `src/lib.rs` for why two front ends must not have two query layers.
// These shims keep every `crate::model::…` path in the drawing code untouched: rewriting 1.9k lines
// of view to a new import path is churn, and copying the modules instead is the divergence this
// workspace refuses.
mod ai {
    pub use wind_ui::ai::*;
}
mod backend {
    pub use wind_ui::backend::*;
}
mod flags {
    pub use wind_ui::flags::*;
}
mod highlight {
    pub use wind_ui::highlight::*;
}
mod model {
    pub use wind_ui::model::*;
}
mod play {
    pub use wind_ui::play::*;
}
mod record {
    pub use wind_ui::record::*;
}
mod settings {
    pub use wind_ui::settings::*;
}
mod wordcloud {
    pub use wind_ui::wordcloud::*;
}

use std::path::PathBuf;
use std::time::Duration;

use app::App;
use wind_base::version;

fn main() -> eframe::Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // Before `parse`, before the install-root walk, before `App::new` opens a month file or a
    // config and long before eframe touches a display: this binary is the one a user is most
    // likely to double-click and least likely to be able to describe, so the question "which
    // build is this" has to be answerable on a machine where the index is locked, the config is
    // broken and there is no window to draw. Scanned across all the arguments rather than only
    // the first because `windui` has no subcommands for it to be ambiguous with.
    if argv.iter().any(|argument| version::is_flag(argument)) {
        println!("{}", version_line());
        return Ok(());
    }
    let options = match parse(argv.as_slice()) {
        Ok(o) => o,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    };

    let root = options.root.clone();
    let exit_after = options.exit_after;
    eprintln!("windui: starting, root {root:?}");

    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1320.0, 840.0])
            .with_min_inner_size([820.0, 560.0])
            .with_title("Windrecorder"),
        // wgpu, not glow. With the glow backend the window appeared with a correct title bar and a
        // completely empty client area on this three-monitor machine, while the app's own frame
        // timings showed it painting — so the widgets were being produced and the GL surface was
        // not compositing them. wgpu goes through the same path the rest of the desktop uses.
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    eframe::run_native(
        "Windrecorder",
        native,
        Box::new(move |cc| {
            install_cjk_fonts(&cc.egui_ctx);
            let app = App::new(root, exit_after, &cc.egui_ctx).map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            Ok(Box::new(app))
        }),
    )
}

/// Hand egui a face that can draw the Chinese in the index; see `backend::cjk_font` for why the
/// built-in fonts are not enough, and why a machine without one is allowed to degrade rather than
/// fail to start.
fn install_cjk_fonts(ctx: &egui::Context) {
    let Some((path, bytes)) = backend::cjk_font() else { return };
    let mut fonts = egui::FontDefinitions::default();
    let name = "cjk".to_owned();
    fonts.font_data.insert(name.clone(), egui::FontData::from_owned(bytes));
    // Appended rather than prepended: egui takes the first face in a family that holds a glyph, so
    // Latin keeps the built-in face and only what it cannot draw falls through to this one.
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push(name.clone());
    }
    ctx.set_fonts(fonts);
    eprintln!("windui: Chinese text will be drawn by {}", path.display());
}

#[derive(Debug)]
struct Options {
    root: PathBuf,
    /// `None` in every release build, by construction: the only arm that assigns it below exists
    /// solely while `debug_assertions` are on. `App`'s `None` fast path is therefore the whole of
    /// the flag's footprint in a shipped binary.
    exit_after: Option<Duration>,
}

/// The `--root` line, worded once because the flag it documents is accepted by every build. The
/// callers below supply their own leading spaces, so that the label and the text line up with
/// whichever other flags their own usage string advertises.
const ROOT_HELP: &str = "install directory holding userdata/ and the shipped settings under \
     config_src/; defaults to the folder this install was built from, found by walking up from this \
     executable, so a windui run out of bin\\ settles on the install and not on bin\\ itself";

/// `--version` is advertised by both builds and accepted by both, and it is the one flag whose
/// wording is identical here and in the other ten binaries because `wind_base::version` formats
/// the answer. Unlike `--exit-after` it is not a development affordance, so there is no second
/// copy of this line behind a `cfg`.
const VERSION_HELP: &str = "print the binary name, the package version and the build profile, then \
     exit without opening a window or reading a config; -V also works";

/// What `windui --version` prints, in the shared format.
fn version_line() -> String {
    version::line("windui", env!("CARGO_PKG_VERSION"))
}

/// The usage text printed alongside a parse error, and the reason there are two definitions of it:
/// a release binary must not advertise a switch that terminates itself, and must not carry the
/// string either — see `parse`, whose `--exit-after` arm is compiled out in the same profile.
#[cfg(debug_assertions)]
fn usage() -> String {
    format!(
        "usage: windui [--root PATH] [--exit-after MS] [--version]\n\
         \x20 --root        {ROOT_HELP}\n\
         \x20 --exit-after  development flag, present only in debug builds: close the window by \
         itself after N milliseconds, once the footer has filled in\n\
         \x20 --version     {VERSION_HELP}"
    )
}

#[cfg(not(debug_assertions))]
fn usage() -> String {
    format!(
        "usage: windui [--root PATH] [--version]\n\
         \x20 --root     {ROOT_HELP}\n\
         \x20 --version  {VERSION_HELP}"
    )
}

/// Parsing that mirrors `windrec`'s: `--flag value` and `--flag=value` both work, and a missing
/// value is an error message rather than a panic.
fn parse(args: &[String]) -> Result<Options, String> {
    // `Option` rather than a resolved default, because `install_root` must be able to tell "the user
    // said where" from "nobody said", and once `--root` has been written over a default it can't.
    let mut root = None;
    // Two bindings rather than one `mut` shared by both profiles: the arm that would assign to it
    // is debug-only, so a release build would (rightly) report an unneeded `mut`.
    #[cfg(debug_assertions)]
    let mut exit_after: Option<Duration> = None;
    #[cfg(not(debug_assertions))]
    let exit_after: Option<Duration> = None;
    let mut i = 0;
    while i < args.len() {
        let (key, inline) = match args[i].split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (args[i].as_str(), None),
        };
        let mut value = |what: &str| -> Result<String, String> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            i += 1;
            args.get(i).cloned().ok_or_else(|| format!("{what} needs a value"))
        };
        match key {
            "--root" => root = Some(PathBuf::from(value("--root")?)),
            // Consumed, not refused. `main` answers the flag before it ever reaches `parse`, but a
            // flag the usage text names must not be an error in the parser either -- that rule is
            // what `usage_advertises_only_what_this_build_accepts` is guarding.
            flag if version::is_flag(flag) => {}
            // Compiled out of a release build, so `--exit-after` falls through to the arm below
            // and is refused as the unknown argument that it then is.
            #[cfg(debug_assertions)]
            "--exit-after" => {
                let ms: u64 = value("--exit-after")?.parse().map_err(|e| format!("--exit-after: {e}"))?;
                exit_after = Some(Duration::from_millis(ms));
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
        i += 1;
    }
    Ok(Options { root: install_root(root), exit_after })
}

/// The install this window reads: `--root` when it was given, otherwise the directory carrying the
/// shipped settings, found by walking up from this executable.
///
/// The rule is [`wind_base::install`]'s and is not re-derived here, which is the whole point of
/// routing all eleven binaries through one function. `windui` used to hand-roll a `windrecorder/` walk
/// like `windrec` and `windsvc` did, and on the standalone payload -- which carries `config_src/`
/// and no `windrecorder/` -- that walk gave up and returned the `bin\` directory the window was
/// launched from. The UI then offered a screen of `bin\userdata\db\` frames and wrote every setting
/// the user changed into `bin\userdata\config_user.json`, while `windcapctl`, `windmcp`,
/// `windmaint` and the Python app all read the `userdata\` two levels up: the settings appeared to
/// be ignored, and an upgrade unpacked straight over the footage.
fn install_root(explicit: Option<PathBuf>) -> PathBuf {
    wind_base::install::resolve_root_from_exe(explicit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `&[&str]` is what a test reads as, `&[String]` is what `parse` takes.
    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    /// A debug build accepts both spellings, and keeps doing so: the screenshot harness passes
    /// `--exit-after` so that "the window opened and painted" is something a script can prove, and
    /// then exit 0 rather than leave a process behind.
    #[test]
    #[cfg(debug_assertions)]
    fn root_and_exit_after_parse_in_both_spellings() {
        let parsed = parse(&argv(&["--root", "E:/x", "--exit-after=250"])).expect("parses");
        assert_eq!(parsed.root, PathBuf::from("E:/x"));
        assert_eq!(parsed.exit_after, Some(Duration::from_millis(250)));

        let spaced = parse(&argv(&["--root", "E:/x", "--exit-after", "250"])).expect("parses");
        assert_eq!(spaced.root, PathBuf::from("E:/x"));
        assert_eq!(spaced.exit_after, Some(Duration::from_millis(250)));
    }

    /// The same argument is an unknown one in a release build, which is the point: a shipped binary
    /// must not carry a switch that terminates its own window, reachable or not.
    #[test]
    #[cfg(not(debug_assertions))]
    fn exit_after_is_unknown_to_a_release_build_in_both_spellings() {
        for spelling in [argv(&["--exit-after=250"]), argv(&["--exit-after", "250"])] {
            let err = parse(&spelling).unwrap_err();
            assert!(err.contains("unexpected argument"), "{err}");
        }
        // `--root` is untouched by all of this, in either build.
        let parsed = parse(&argv(&["--root", "E:/x"])).expect("parses");
        assert_eq!(parsed.root, PathBuf::from("E:/x"));
        assert_eq!(parsed.exit_after, None);
    }

    /// What the usage text may name, it must accept. Both builds test the same property, from the
    /// only side of `cfg` each can be on.
    #[test]
    #[cfg(debug_assertions)]
    fn usage_advertises_only_what_this_build_accepts() {
        let text = usage();
        assert!(text.contains("--exit-after"), "{text}");
        assert!(text.contains("debug build"), "{text}");
        assert!(text.contains("--root"), "{text}");
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn usage_advertises_only_what_this_build_accepts() {
        let text = usage();
        assert!(!text.contains("exit-after"), "{text}");
        assert!(text.contains("--root"), "{text}");
    }

    #[test]
    fn an_unfinished_flag_is_an_error_not_a_panic() {
        let args: Vec<String> = ["--root"].iter().map(|s| s.to_string()).collect();
        assert!(parse(&args).unwrap_err().contains("needs a value"));
        let bogus: Vec<String> = ["--nope"].iter().map(|s| s.to_string()).collect();
        assert!(parse(&bogus).unwrap_err().contains("unexpected argument"));
    }

    /// The window and the recorder have to agree on which install they are looking at, and they only
    /// do that because every binary now asks [`wind_base::install`] the same question. `windui`'s own
    /// `windrecorder/` walk resolved a standalone payload to the `bin\` directory it was launched
    /// from, so the UI read the frames `windrec` had mislaid there and wrote its settings into
    /// `bin\userdata\` — invisible to `windmaint`, `windcapctl` and the Python app, which all read
    /// the real `userdata\` two levels up.
    #[test]
    fn the_root_comes_from_the_shared_install_rule_and_an_explicit_root_still_wins() {
        for spelling in [argv(&["--root", "E:/chosen"]), argv(&["--root=E:/chosen"])] {
            assert_eq!(parse(&spelling).expect("parses").root, PathBuf::from("E:/chosen"));
        }
        let parsed = parse(&argv(&[])).expect("a bare launch has a root");
        let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
        assert!(wind_base::install::is_install_root(&parsed.root), "{:?} is not an install root", parsed.root);
        assert!(exe_dir.starts_with(&parsed.root), "{exe_dir:?} is not inside the resolved root {:?}", parsed.root);

        // The payload layout itself: `bin/` under a root carrying only `config_src/`, no
        // `windrecorder/` and no `.venv`, resolves to that root rather than to `bin/`.
        let scratch = std::env::temp_dir().join(format!("windui-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let payload = scratch.join("payload");
        std::fs::create_dir_all(payload.join("config_src")).unwrap();
        std::fs::write(payload.join("config_src").join("config_default.json"), "{}").unwrap();
        std::fs::create_dir_all(payload.join("bin")).unwrap();
        assert_eq!(wind_base::install::resolve_root(None, &payload.join("bin")), payload);
        std::fs::remove_dir_all(scratch).unwrap();
    }

    /// The line a user quotes when the window will not open. It is answered by `main` before
    /// `App::new` -- and so before any config, index or display is touched -- which is the part
    /// that makes it reachable from a broken install; this asserts what it says.
    #[test]
    fn the_version_line_names_the_binary_and_carries_the_package_version() {
        let line = version_line();
        assert!(line.starts_with("windui "), "{line}");
        assert!(line.contains(env!("CARGO_PKG_VERSION")), "{line}");
        assert!(line.ends_with("(debug)") || line.ends_with("(release)"), "{line}");
    }

    /// Both spellings reach the same answer, in either build, and the parser does not refuse a
    /// flag the usage screen advertises.
    #[test]
    fn version_is_accepted_in_both_spellings_by_both_builds() {
        for spelling in ["--version", "-V"] {
            let args = argv(&[spelling]);
            assert!(args.iter().any(|a| version::is_flag(a)), "{spelling}");
            assert!(parse(&argv(&["--root", "E:/x", spelling])).is_ok(), "{spelling} must not be refused");
            assert!(usage().contains(spelling), "{spelling} missing from the usage text:\n{}", usage());
        }
    }
}
