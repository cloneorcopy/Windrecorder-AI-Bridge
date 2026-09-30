//! The self-check: what the maintenance pass can see, and how long it took to see it.
//!
//! This is the command a user runs when something is wrong, so it is the one subcommand that must
//! survive a broken install: every step reports `MISSING` or its own error instead of failing, and the
//! timings make a slow disk or a missing ffmpeg obvious without a profiler. Nothing here opens an index
//! file for writing, and nothing creates a directory.

use std::path::Path;
use std::time::Instant;

use wind_base::clock::{self, LocalParts};
use wind_base::config::Config;
use wind_store::read;

use crate::{backup, convert, encode, expire, layout};

/// One measured line of the report.
fn report(label: &str, value: &str, started: Instant) {
    println!("{label:<16} {value}  [{:>7.2} ms]", started.elapsed().as_secs_f64() * 1000.0);
}

/// A month file's row count and time span, as the report shows them.
///
/// The bounds are rendered as dates because "9 rows, 2026-09-21 21:16:12 .. 2026-09-22 19:51:12" is the
/// line that tells a user whether the recorder has been writing at all; the raw integers in the stored
/// naive-local epoch mean nothing to anyone who has not read `wind_base::clock`.
pub fn month_line(label: &str, rows: i64, bounds: Option<(i64, i64)>) -> String {
    match bounds {
        Some((first, last)) => format!(
            "{label}: {rows} rows, {} .. {}",
            LocalParts::from_naive_epoch(first).display(),
            LocalParts::from_naive_epoch(last).display()
        ),
        None if rows == 0 => format!("{label}: empty"),
        None => format!("{label}: {rows} rows, no timestamps"),
    }
}

/// Whether a directory is there, phrased the way the report wants it.
fn presence(path: &Path) -> String {
    if path.is_dir() {
        format!("{}", path.display())
    } else {
        format!("MISSING {}", path.display())
    }
}

/// Number of entries of a directory that satisfy `keep`, or `None` when it cannot be listed.
pub fn count_entries(dir: &Path, keep: &dyn Fn(&Path) -> bool) -> Option<usize> {
    let entries = std::fs::read_dir(dir).ok()?;
    Some(entries.flatten().filter(|e| keep(&e.path())).count())
}

/// Run the report. Always `Ok` unless the config files themselves cannot be parsed, because a report
/// that stops at the first missing directory cannot tell the user what else is missing.
pub fn run(root: &Path, limit: Option<usize>) -> Result<(), String> {
    let started = Instant::now();
    let config = Config::load(root).map_err(|e| e.to_string())?;
    let load_ms = started.elapsed().as_secs_f64() * 1000.0;

    println!("windmaint doctor");
    println!("{:<16} {}", "install root", root.display());
    let defaults = wind_base::install::defaults_file(root);
    for path in [
        defaults.as_deref(),
        Some(root.join("userdata").join("config_user.json")).as_deref(),
    ] {
        let Some(path) = path else { continue };
        println!(
            "{:<16} {}",
            if path.is_file() { "config" } else { "config MISSING" },
            path.display()
        );
    }
    println!("                 {load_ms:.2} ms to read and merge them");

    let started = Instant::now();
    let videos_dir = config.videos_dir();
    let month_dirs = count_entries(&videos_dir, &|p| p.is_dir()).unwrap_or(0);
    let videos = crate::refresh::DiskVideos::scan(&videos_dir);
    report("videos dir", &format!("{} month dir(s), {} segment(s) — {}", month_dirs, videos.segments(), presence(&videos_dir)), started);

    let started = Instant::now();
    let cache_dir = config.cache_screenshot_dir();
    let slices = convert::discover_slices(&cache_dir);
    let all_dirs = count_entries(&cache_dir, &|p| p.is_dir()).unwrap_or(0);
    report(
        "cache dir",
        &format!("{} slice(s) pending of {} dir(s) — {}", slices.len(), all_dirs, presence(&cache_dir)),
        started,
    );
    for slice in slices.iter().take(limit.unwrap_or(5)) {
        let frames = convert::read_frames(&slice.dir);
        let span = match (frames.first(), frames.last()) {
            (Some(first), Some(last)) => format!("{} .. {}", first.stamp(), last.stamp()),
            _ => "no frames".to_string(),
        };
        println!(
            "                 {}: {} frame(s), {span}, {}",
            slice.stamp,
            frames.len(),
            if frames.len() < encode::MIN_FRAMES { "would be DISCARDED" } else { "ready to convert" },
        );
    }
    if let Some(limit) = limit.filter(|n| *n < slices.len()) {
        println!("                 {} more, --limit is {}", slices.len() - limit, limit);
    }

    let started = Instant::now();
    let months = read::discover(&config.db_dir());
    report("db dir", &format!("{} month file(s) — {}", months.len(), presence(&config.db_dir())), started);
    for month in months.iter().take(limit.unwrap_or(usize::MAX)) {
        let started = Instant::now();
        let label = month.path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        let summary = match crate::refresh::open_read_only(&month.path) {
            Err(e) => format!("{label}: cannot open read-only: {e}"),
            Ok(conn) => {
                let columns = read::available_columns(&conn).map(|c| c.len()).unwrap_or(0);
                let rows = match (read::count_rows(&conn), read::time_bounds(&conn)) {
                    (Ok(rows), Ok(bounds)) => month_line(&label, rows, bounds),
                    (Err(e), _) => format!("{label}: {e}"),
                    (Ok(_), Err(e)) => format!("{label}: {e}"),
                };
                format!("{rows} [columns: {columns}, time index {}]", if has_index(&conn) { "yes" } else { "no" })
            }
        };
        report("  month", &summary, started);
    }

    let started = Instant::now();
    let ffmpeg = config.ffmpeg_path();
    let version = layout::ffmpeg_version(&ffmpeg);
    report(
        "ffmpeg",
        &match version {
            Some(line) => format!("{} — {}", ffmpeg.display(), line),
            None => format!("NOT RUNNABLE: {}", ffmpeg.display()),
        },
        started,
    );

    let started = Instant::now();
    let presets = encode::PresetTable::load(root);
    let encoder_name = config.str_or("record_encoder", "cpu_h264");
    let empty = encode::PresetTable::default();
    // Asked through the same resolver `convert` uses, so this line predicts the pass rather than
    // re-deriving half of it and disagreeing with the other half. The probe costs one tiny encode
    // and is the only way to answer the question a user actually brings `doctor` for: "I picked
    // `AMD_h265`, will it work here?"
    let choice = encode::resolve_encoder(
        &encoder_name,
        presets.as_ref().unwrap_or(&empty),
        config.i64_or("record_bitrate", 200),
        config.i64_or("record_crf", 39),
        &|codec| layout::probe_encoder(&ffmpeg, codec),
    );
    report(
        "encoder",
        &format!(
            "{encoder_name} -> {}{}",
            choice.args.join(" "),
            choice.note.as_ref().map(|note| format!("; {note}")).unwrap_or_default()
        ),
        started,
    );
    println!("                 {} preset(s) in {}", presets.as_ref().map(|t| t.names().len()).unwrap_or(0), encode::RECORD_PRESET_RELPATH);
    match &presets {
        Ok(table) => println!("                 {}", table.names().join(", ")),
        Err(e) => println!("                 {e}"),
    }

    // The retention pass's encoder, on the same terms. The settings page offers four accelerators
    // for every codec and the machine can usually only open one of them, so this is the second line
    // of the report a user needs before their library quietly stops shrinking.
    let started = Instant::now();
    let configured = expire::compress_preset(root, &config);
    let compress_table = encode::CompressTable::load(root).ok();
    let cpu_row = compress_table.as_ref().and_then(|t| t.get(encode::CPU_FALLBACK_ENCODER, encode::CPU_FALLBACK_ACCELERATOR).cloned());
    let asked = format!(
        "compress_encoder '{}' with compress_accelerator '{}'",
        config.str_or("compress_encoder", "x264"),
        config.str_or("compress_accelerator", "cpu")
    );
    let compress = encode::resolve_compress(&asked, configured, cpu_row, &|codec| layout::probe_encoder(&ffmpeg, codec));
    report(
        "compress",
        &format!(
            "{asked} -> {}{}",
            compress.preset.encoder,
            compress.note.as_ref().map(|note| format!("; {note}")).unwrap_or_default()
        ),
        started,
    );

    let started = Instant::now();
    let now = clock::now();
    let day_begin = config.day_begin_minutes();
    let trash_root = expire::trash_dir(&config);
    let (trash_runs, trash_bytes) = expire::trash_holdings(&trash_root);
    report(
        "retention",
        &format!(
            "delete before {}, compress before {}, product day starts {} min after midnight, recycle {} \
             ({} run folder(s) held in {}, {} byte(s), pruned after {} day(s))",
            expire::retention_cutoff(&now, config.i64_or("vid_store_day", 0), day_begin)
                .map_or_else(|| "off".to_string(), |at| LocalParts::from_naive_epoch(at).display()),
            expire::retention_cutoff(&now, config.i64_or("vid_compress_day", 0), day_begin)
                .map_or_else(|| "off".to_string(), |at| LocalParts::from_naive_epoch(at).display()),
            day_begin,
            config.bool_or("recycle_deleted_files", true),
            trash_runs,
            trash_root.display(),
            trash_bytes,
            expire::TRASH_KEEP_DAYS
        ),
        started,
    );

    let started = Instant::now();
    let backups: Vec<String> = std::fs::read_dir(backup::backup_dir(&config))
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .filter(|n| backup::backup_stamp_of(n).is_some())
                .collect()
        })
        .unwrap_or_default();
    report(
        "backups",
        &format!(
            "{} stored, {} past the keep limit of {keep}",
            backups.len(),
            backup::prune(&backups, backup::KEEP).len(),
            keep = backup::KEEP
        ),
        started,
    );
    Ok(())
}

fn has_index(conn: &rusqlite::Connection) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
        [crate::refresh::TIME_INDEX_NAME],
        |r| r.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn at(stamp: &str) -> i64 {
        LocalParts::from_stamp(stamp).unwrap().naive_epoch_seconds()
    }

    #[test]
    fn a_month_line_quotes_the_span_it_covers_in_wall_clock_terms() {
        assert_eq!(
            month_line("default_2026-09_wind.db", 2, Some((at("2026-09-21_21-16-12"), at("2026-09-22_19-51-12")))),
            "default_2026-09_wind.db: 2 rows, 2026-09-21 21:16:12 .. 2026-09-22 19:51:12"
        );
        assert_eq!(month_line("m.db", 0, None), "m.db: empty");
        // Rows exist but carry no usable timestamp, which is a real legacy condition and must be said.
        assert_eq!(month_line("m.db", 7, None), "m.db: 7 rows, no timestamps");
    }

    #[test]
    fn counting_entries_distinguishes_absent_from_empty() {
        let dir = std::env::temp_dir().join(format!("windmaint-doctor-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.db"), b"x").unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        let is_dir = |p: &Path| p.is_dir();
        let is_file = |p: &Path| p.is_file();
        assert_eq!(count_entries(&dir, &is_dir), Some(1));
        assert_eq!(count_entries(&dir, &is_file), Some(1));
        assert_eq!(count_entries(&dir.join("nope"), &is_dir), None, "a missing directory is reported, not invented");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bare_directory_reports_ok_rather_than_panicking() {
        let dir = std::env::temp_dir().join(format!("windmaint-doctor-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(run(&dir, None).is_ok(), "an install with nothing in it must still produce a report");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_real_install_reports_its_index() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).unwrap().to_path_buf();
        assert!(run(&root, Some(1)).is_ok());
        assert_eq!(presence(&root.join("nope")), format!("MISSING {}", root.join("nope").display()));
    }
}
