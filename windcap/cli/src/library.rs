//! The only place in this binary that opens the user's files.
//!
//! Keeping it in one module buys two things: every command reads the index through exactly the same
//! staleness and locking policy, and the command bodies in `main` stay free of both SQLite and the
//! filesystem, which is what lets them be tested as plain functions over data.
//!
//! Reads follow `wind_store`'s rule and never touch a live month file — they go through the
//! `_TEMP_READ.db` copy — with one deliberate exception: `index` has to write the live file, and it
//! times against that same handle so the before/after numbers describe one set of bytes.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use wind_base::config::Config;
use wind_store::read::{self, Month, Row};
use wind_store::search::{Query, SearchResult};
use wind_store::similar::SimilarChars;

/// Five minutes, which is upstream's own staleness window in `db_manager.get_temp_dbfilepath`. A
/// terminal query that copied the whole database on every run would be slower than the Python app
/// it is replacing.
pub const READ_STALE_AFTER: Duration = Duration::from_secs(300);

/// One month file and the aggregates that are cheaper to ask SQLite for than to compute here.
#[derive(Debug, Clone)]
pub struct MonthFacts {
    pub month: Month,
    pub rows: i64,
    /// Distinct `videofile_name` values, i.e. segments with at least one indexed row.
    pub segments: i64,
    pub bounds: Option<(i64, i64)>,
}

/// A discovered index, its config, and the policy for reading it.
pub struct Library {
    pub root: PathBuf,
    pub config: Config,
    pub months: Vec<Month>,
}

/// `--root`, or the install directory this binary was launched from.
///
/// [`wind_base::install`] holds the rule: an install is the directory carrying its shipped
/// settings, found by walking up from the executable so that one `windcapctl.exe` serves both
/// `C:\Windrecorder\bin` and a development run out of `windcap/target/debug`. Every other binary in
/// the workspace asks the same function, which is the point — the alternative is eight hand-rolled
/// variants that drift, and a reader and an indexer disagreeing about which install they mean.
pub fn resolve_root(explicit: Option<PathBuf>) -> PathBuf {
    wind_base::install::resolve_root_from_exe(explicit)
}

/// Is this a Windrecorder install? Asked of [`wind_base::install`] rather than answered here, for
/// the same reason `resolve_root` no longer walks anything itself. Kept as a name because the test
/// below reads as a claim about an install, not as a call into a shared module.
#[cfg(test)]
fn is_install(path: &std::path::Path) -> bool {
    wind_base::install::is_install_root(path)
}

impl Library {
    /// Load config and discover every month file. A missing directory is an empty library, not a
    /// panic: `windcapctl` is also the tool you reach for to find out why the data is not there.
    pub fn open(root: PathBuf) -> Result<Library, String> {
        let config = Config::load(&root).map_err(|e| e.to_string())?;
        let months = read::discover(&config.db_dir());
        Ok(Library { root, config, months })
    }

    pub fn db_dir(&self) -> PathBuf {
        self.config.db_dir()
    }

    pub fn day_begin_minutes(&self) -> i64 {
        self.config.day_begin_minutes()
    }

    /// True while the idle maintenance pass holds its directory lock with a live process behind it,
    /// in which case the read copies are deliberately left alone — the origin is being rewritten
    /// under the reader. The directory itself says nothing: a pass rmdir's only a directory it
    /// created, so an empty `LOCK_MAINTAIN` is what an install that has finished maintaining looks
    /// like. See [`wind_base::config::Config::maintain_lock_claimed`].
    pub fn maintaining(&self) -> bool {
        self.config.maintain_lock_claimed()
    }

    /// A file inside the settings directory, whichever of the two layouts this install uses.
    ///
    /// Delegated to [`Config::config_src_file`] so the Chinese fuzzy-glyph table is resolved by the
    /// same rule that resolved the config saying whether to use it. The old spelling here defaulted
    /// to `windrecorder\\config_src`, which is the directory a standalone payload does not have.
    pub fn config_src_file(&self, name: &str) -> PathBuf {
        self.config.config_src_file(name)
    }

    /// The shape-similar Chinese table, or `None` when the install has none. Search still works
    /// without it — it just stops fuzzing glyph confusion — so this is not an error.
    pub fn similar(&self) -> Option<SimilarChars> {
        SimilarChars::load(&self.config_src_file("similar_CN_characters.txt")).ok()
    }

    pub fn months_covering(&self, from: i64, to: i64) -> Vec<Month> {
        read::months_in_range(&self.months, from, to).into_iter().cloned().collect()
    }

    /// Rows in a window, oldest first, merged across the months it touches, and how long that took.
    pub fn rows_in(&self, months: &[Month], query: &Query) -> Result<(SearchResult, f64), String> {
        let started = Instant::now();
        let found = wind_store::search::search_months(months, query, READ_STALE_AFTER, self.maintaining())
            .map_err(|e| e.to_string())?;
        Ok((found, started.elapsed().as_secs_f64() * 1000.0))
    }

    /// Every row of one month file, for the aggregate views that need the whole set.
    pub fn rows_of(&self, month: &Month) -> Result<Vec<Row>, String> {
        let conn = month.open_read(READ_STALE_AFTER, self.maintaining()).map_err(|e| e.to_string())?;
        read::rows_in_window(&conn, None, None).map_err(|e| e.to_string())
    }

    /// Per-file totals, read with one aggregate statement each.
    ///
    /// `COUNT(DISTINCT videofile_name)` is pushed into SQLite on purpose: the library-wide "how many
    /// segments" figure needs no other column, and pulling a year of `ocr_text` into memory to count
    /// filenames in Rust is the mistake this rewrite exists to stop making.
    pub fn facts(&self) -> Result<Vec<MonthFacts>, String> {
        let mut out = Vec::with_capacity(self.months.len());
        for month in &self.months {
            let conn = month.open_read(READ_STALE_AFTER, self.maintaining()).map_err(|e| e.to_string())?;
            let rows = read::count_rows(&conn).map_err(|e| e.to_string())?;
            let segments: i64 = conn
                .query_row("SELECT COUNT(DISTINCT videofile_name) FROM video_text", [], |r| r.get(0))
                .map_err(|e| e.to_string())?;
            out.push(MonthFacts {
                month: month.clone(),
                rows,
                segments,
                bounds: read::time_bounds(&conn).map_err(|e| e.to_string())?,
            });
        }
        Ok(out)
    }

    /// The newest indexed instant in the library, which is what the fixed benchmark battery anchors
    /// its windows to so that two runs measure the same data.
    pub fn latest_time(&self) -> Result<Option<i64>, String> {
        Ok(self.facts()?.into_iter().filter_map(|f| f.bounds.map(|(_, to)| to)).max())
    }
}

/// A month file's name as reports quote it: the file itself, not the parsed coordinates.
pub fn file_name(month: &Month) -> String {
    month.path.file_name().and_then(|s| s.to_str()).unwrap_or("?").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    // Imported here rather than at the top of the file: production code in this module spells the
    // type out as `&std::path::Path`, so a file-level `Path` would be an unused-import warning in a
    // non-test build while the tests still needed it.
    use std::path::Path;

    #[test]
    fn an_absent_library_discovers_nothing_instead_of_failing() {
        let missing = Path::new("Z:/definitely/not/here");
        let library = Library::open(missing.to_path_buf()).expect("a missing root must still load");
        assert!(library.months.is_empty());
        assert!(library.facts().unwrap().is_empty());
        assert_eq!(library.latest_time().unwrap(), None);
        assert!(!library.maintaining());
    }

    /// This test used to assert that a `config_src_dir` naming the *legacy* directory wins even
    /// when that directory is not there. That is the behaviour the payload move had to retire:
    /// `Config::save` snapshots the merged config, so every upgraded overlay install's user file
    /// still carries `windrecorder\\config_src` inherited from the defaults it was seeded from, and
    /// honouring it would keep such an install reading the settings layer its own upgrade replaced.
    /// The two shipped default spellings are therefore not overrides; anything else is.
    #[test]
    fn the_settings_directory_follows_the_install_and_only_a_real_override_wins() {
        let dir = std::env::temp_dir().join(format!("windcap-cli-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config_src")).unwrap();
        std::fs::write(
            dir.join("config_src/config_default.json"),
            br#"{"config_src_dir": "windrecorder\\config_src"}"#,
        )
        .unwrap();
        // The inherited legacy literal names a directory that is not here, so the install decides.
        let library = Library::open(dir.clone()).unwrap();
        assert_eq!(
            library.config_src_file("similar_CN_characters.txt"),
            dir.join("config_src").join("similar_CN_characters.txt")
        );

        // A genuine override is honoured, and survives either separator. The pairs are the value as
        // it appears *inside* the JSON document, so the backslash spelling carries its own escape.
        for (spelling, label) in [(r"other/where", "slash"), (r"other\\where", "backslash")] {
            std::fs::write(
                dir.join("config_src/config_default.json"),
                format!(r#"{{"config_src_dir": "{spelling}"}}"#).as_bytes(),
            )
            .unwrap();
            std::fs::create_dir_all(dir.join("other").join("where")).unwrap();
            let library = Library::open(dir.clone()).unwrap();
            assert_eq!(library.config_src_file("x.txt"), dir.join("other").join("where").join("x.txt"), "{label}");
        }

        // And with both layouts present the payload one wins, whatever the literal says.
        std::fs::create_dir_all(dir.join("windrecorder/config_src")).unwrap();
        std::fs::write(
            dir.join("windrecorder/config_src/config_default.json"),
            br#"{"config_src_dir": "windrecorder\\config_src"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("config_src/config_default.json"), br#"{"config_src_dir": "config_src"}"#).unwrap();
        let library = Library::open(dir.clone()).unwrap();
        assert_eq!(
            library.config_src_file("similar_CN_characters.txt"),
            dir.join("config_src").join("similar_CN_characters.txt")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An install whose only settings are the legacy ones is still read, unchanged.
    #[test]
    fn a_legacy_overlay_install_finds_its_tables_under_windrecorder() {
        let dir = std::env::temp_dir().join(format!("windcap-cli-src-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("windrecorder/config_src")).unwrap();
        std::fs::write(
            dir.join("windrecorder/config_src/config_default.json"),
            br#"{"config_src_dir": "windrecorder\\config_src"}"#,
        )
        .unwrap();
        let library = Library::open(dir.clone()).unwrap();
        assert_eq!(
            library.config_src_file("similar_CN_characters.txt"),
            dir.join("windrecorder").join("config_src").join("similar_CN_characters.txt")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A temp install whose live month file holds `rows` rows and whose `_TEMP_READ.db` copy holds
    /// none, with the copy left an hour behind — past the five-minute window every reader uses.
    ///
    /// That is the shape a *frozen* snapshot has on a user's disk: the recorder keeps committing, the
    /// origin keeps growing, and the copy the UI reads stops being refreshed for a reason the reader
    /// itself supplies. Whatever the caller puts in `cache/locks` is the variable under test.
    fn install_with_a_stale_read_copy(label: &str, rows: usize) -> PathBuf {
        use std::time::SystemTime;

        let dir = std::env::temp_dir().join(format!("windcap-cli-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db_dir = dir.join("userdata/db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let origin = db_dir.join("default_2026-09_wind.db");
        write_month(&origin, rows);
        write_month(&wind_base::paths::temp_read_of(&origin), 0);
        // Set the clock after the writes, because an `INSERT` moves the file it touched forward.
        let now = SystemTime::now();
        set_modified(&wind_base::paths::temp_read_of(&origin), now - Duration::from_secs(3600));
        set_modified(&origin, now);
        dir
    }

    fn write_month(path: &std::path::Path, rows: usize) {
        let conn = rusqlite::Connection::open(path).unwrap();
        wind_store::ensure_schema(&conn).unwrap();
        for n in 0..rows {
            conn.execute(
                "INSERT INTO video_text (videofile_name, picturefile_name, videofile_time, ocr_text,
                   is_videofile_exist, is_picturefile_exist, thumbnail, win_title, deep_linking)
                 VALUES ('2026-09-25_21-57-09.mp4', 'f.jpg', 1790300000, ?1, 1, 0, '', '', '')",
                rusqlite::params![format!("row {n}")],
            )
            .unwrap();
        }
    }

    /// `write(true)` is not decoration. `File::set_modified` needs write access on Windows, so a
    /// handle from `File::open` reports success having changed nothing.
    fn set_modified(path: &std::path::Path, when: std::time::SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(when).unwrap();
    }

    /// The symptom this test exists for: the user recorded all evening, the index had the rows, and
    /// the window showed an old snapshot. `cache/locks/LOCK_MAINTAIN` was on disk as an *empty*
    /// directory — the shape a `windmaint` pass leaves behind whenever it was not the one that
    /// created it, and the shape the tray's own marker sweep produces — and every reader answered
    /// "is maintenance running?" by asking whether that directory *exists*.
    #[test]
    fn an_empty_maintain_container_left_behind_does_not_freeze_the_read_copy() {
        let dir = install_with_a_stale_read_copy("empty-container", 3);
        std::fs::create_dir_all(dir.join("cache/locks/LOCK_MAINTAIN")).unwrap();

        let library = Library::open(dir.clone()).unwrap();
        assert!(
            !library.maintaining(),
            "an empty container names no owner, so it claims nothing; `windmaint` documents the case as \
             `a_foreign_container_is_claimed_and_left_behind` and both doctors already report it as \
             `upstream's own container, not a claim`"
        );
        let month = library.months.first().expect("the month file is discovered");
        let rows = library.rows_of(month).unwrap();
        assert_eq!(rows.len(), 3, "the reader refreshed the copy and sees what the recorder committed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of the rule, and the reason the first test is not simply "never defer": a pass
    /// that is genuinely rewriting the origin must still be left alone, or this fix would have traded
    /// a frozen snapshot for a torn one.
    #[test]
    fn a_live_owner_inside_the_container_still_freezes_the_read_copy() {
        let dir = install_with_a_stale_read_copy("live-claim", 3);
        let container = dir.join("cache/locks/LOCK_MAINTAIN");
        std::fs::create_dir_all(&container).unwrap();
        let mut child = std::process::Command::new("ping")
            .args(["-n", "20", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("ping ships with every Windows install");
        std::fs::write(container.join("PID"), child.id().to_string()).unwrap();

        let library = Library::open(dir.clone()).unwrap();
        assert!(library.maintaining(), "the claim belongs to a running process {child:?}");
        let month = library.months.first().expect("the month file is discovered");
        assert!(library.rows_of(month).unwrap().is_empty(), "and its readers take the old copy instead of a torn one");
        let _ = child.kill();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_install_root_is_found_by_its_marker_directories() {
        // A real checkout: the binary lives under target/debug, the install root is the repo.
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo = manifest.parent().and_then(Path::parent).expect("workspace layout");
        let from_deepest = resolve_root(Some(repo.join("windcap/target/debug")));
        assert_eq!(from_deepest, repo.join("windcap/target/debug"), "an explicit root is never second-guessed");
        assert!(is_install(repo), "{repo:?} should look like a Windrecorder install");
    }
}
