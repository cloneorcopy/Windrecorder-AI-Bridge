//! What closing the window means, and how a hidden one comes back.
//!
//! The window is a child of the tray, and the recorder is a child of the tray too. Closing the window used
//! to end the window's process — which left recording running, behind an icon that Windows keeps in the
//! overflow flyout, so the product read as "it quit on me". The fix is the one every tray app uses: hide,
//! and let the tray bring it back.
//!
//! Hiding is only ever correct when something can undo it. So [`decide`] asks two questions and requires
//! both: the user asked for close-to-tray, and a tray is actually alive holding its lock. Double-click
//! `winduiweb.exe` from Explorer with no tray running and the close button closes — an invisible window
//! with no icon to click is worse than the bug this replaces.
//!
//! The raise itself crosses processes through one file in `cache/locks`, which the tray writes and this
//! process consumes ([`watch_for_show`]). No new dependency, no second channel to keep in sync with the
//! lock conventions the tray already uses, and a request that nobody consumed is a window that simply
//! stays hidden.

use std::path::{Path, PathBuf};
use std::time::Duration;

use wind_base::config::Config;

/// How often the hiding watcher looks at the signal file. Long enough that an idle window costs nothing
/// measurable, short enough that a double-click on the tray feels like a click answered.
pub const POLL: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Close {
    /// Stay alive, unseen, and wait for the tray.
    Hide,
    /// End the process, as a window with no tray behind it should.
    Quit,
}

/// What this install's close button means, read fresh — the two answers can change while the window is
/// open, because the settings page writes one and starting the tray is the other.
///
/// The rule itself is [`Config::window_hides_on_close`]'s, shared with the egui window: two front doors,
/// one answer to "does closing me hide me or end me".
pub fn decide(root: &Path) -> Close {
    match Config::load(root) {
        Ok(config) if config.window_hides_on_close() => Close::Hide,
        // A config that will not parse is a config that says nothing about close behaviour. Quitting is
        // the answer that cannot leave the user with a window they cannot get back.
        _ => Close::Quit,
    }
}

/// Watch for the tray's "come back" request and call `raise` for each one.
///
/// Detached, on its own thread, for the life of the process: the window can be hidden and shown any number
/// of times, and the only alternative is a timer inside the webview's event loop, which this file exists
/// to stay out of. A `raise` that lands on a window already gone is the webview's problem to refuse, and
/// the answer is ignored here rather than ending the watcher — the process is on its way out in that case
/// anyway.
pub fn watch_for_show(root: PathBuf, raise: impl Fn() + Send + 'static) {
    std::thread::Builder::new()
        .name("winduiweb-show-watcher".to_string())
        .spawn(move || {
            loop {
                std::thread::sleep(POLL);
                let signal = match Config::load(&root) {
                    Ok(config) => config.window_show_signal_path(),
                    // Read each pass rather than once: `lock_file_dir` is a config key, and an install
                    // that moves its cache should not need every window restarted to notice.
                    Err(_) => continue,
                };
                if wind_base::fslock::take_show_request(&signal) {
                    raise();
                }
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A scratch install whose settings say one thing and whose lock file says another.
    struct Install {
        dir: PathBuf,
    }

    impl Drop for Install {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn install(tag: &str, settings: &str) -> Install {
        let dir = std::env::temp_dir().join(format!("winduiweb-bg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), settings).unwrap();
        Install { dir }
    }

    /// The tray proves itself by holding its lock with a live pid. This process is live, so writing its
    /// own pid is the honest way to say "a tray is home" in a test.
    fn tray_alive(dir: &Path) {
        let lock = dir.join("cache").join("locks").join("LOCK_FILE_TRAY.MD");
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        std::fs::write(lock, format!("{}", std::process::id())).unwrap();
    }

    #[test]
    fn close_hides_when_the_user_asked_for_it_and_a_tray_is_alive() {
        let install = install("hide", r#"{"close_window_to_tray": true}"#);
        tray_alive(&install.dir);
        assert_eq!(decide(&install.dir), Close::Hide);
    }

    /// No tray, no hiding. The window is the only door back to the settings page, and closing it must not
    /// lock the user out of their own install.
    #[test]
    fn close_quits_when_nothing_is_left_to_bring_the_window_back() {
        let install = install("no-tray", r#"{"close_window_to_tray": true}"#);
        assert_eq!(decide(&install.dir), Close::Quit, "no lock file means no tray");
    }

    /// The other half of the gate: the user's own choice wins, and the default is the choice they made
    /// when they asked for a background mode.
    #[test]
    fn close_quits_when_the_user_switched_background_mode_off() {
        let install = install("off", r#"{"close_window_to_tray": false}"#);
        tray_alive(&install.dir);
        assert_eq!(decide(&install.dir), Close::Quit);
    }

    #[test]
    fn an_unreadable_config_closes_the_window_instead_of_hiding_it_forever() {
        let install = install("broken", "{}");
        std::fs::write(install.dir.join("userdata_config"), b"").ok();
        std::fs::create_dir_all(install.dir.join("userdata")).unwrap();
        std::fs::write(install.dir.join("userdata/config_user.json"), b"{ not json").unwrap();
        assert_eq!(decide(&install.dir), Close::Quit);
    }

    /// The request is consumed, not observed: a second poll must not raise the window again, or a tray
    /// double-click an hour later would steal focus for the click that happened hours ago.
    #[test]
    fn a_show_request_is_taken_once_and_a_second_look_sees_nothing() {
        let dir = std::env::temp_dir().join(format!("winduiweb-show-{}", std::process::id()));
        let signal = dir.join("locks").join("WINDOW_SHOW.MD");
        assert!(!wind_base::fslock::take_show_request(&signal), "nothing written yet");

        wind_base::fslock::request_show(&signal).expect("the writer makes its own directory");
        assert!(wind_base::fslock::take_show_request(&signal), "the request is there");
        assert!(!wind_base::fslock::take_show_request(&signal), "and it is gone after being taken");
        assert!(!signal.exists(), "the file does not outlive its message");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
