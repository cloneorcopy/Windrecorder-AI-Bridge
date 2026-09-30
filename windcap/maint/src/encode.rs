//! Everything that decides what ffmpeg is told, kept away from the process that runs it.
//!
//! The encoder's command line is the whole contract of the convert step: get the frame order or the
//! per-frame duration wrong and the video still encodes cleanly, it just no longer answers "jump to
//! 14:32" with the picture the index row at 14:32 describes. All of it is therefore pure data in,
//! data out, and the only code in this crate that spawns a process lives in [`crate::layout`].
//!
//! One thing is *not* data-in-data-out, and it is the reason this module owns the encoder question at
//! all: whether the machine can encode with the name the settings page saved. That is answered by
//! asking ffmpeg ([`crate::layout::probe_encoder`]) and decided here in [`resolve_encoder`] and
//! [`resolve_compress`], which take the answer as a closure so every branch of the decision is
//! testable on a machine that has no encoder, no GPU and no ffmpeg at all.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Fewer frames than this cannot make a watchable segment, and upstream throws the slice away
/// rather than emitting a one-second video for it (`MINIMUM_NUMBER_OF_IMAGES_REQUIRED_FOR_A_VIDEO`
/// is 4, and the companion check demands 4 + 2 first-level entries in the directory).
pub const MIN_FRAMES: usize = 5;

/// Seconds the last captured frame stays on screen.
///
/// Upstream adds the same constant (`+ 2` in `calc_screenshot_time_to_video_time`) so a segment ends
/// on its last screen instead of cutting to nothing the moment it was captured.
pub const TAIL_SECONDS: i64 = 2;

/// The record presets, read from `config_src/record_preset.json` — or from the
/// `windrecorder/config_src/` copy an overlay install still keeps.
///
/// The table is loaded rather than mirrored here because the settings page can add a preset to the
/// Python tree without a native rebuild, and an unknown name must surface as a reported fallback
/// rather than as a silently different encoder than the one the user picked.
#[derive(Debug, Clone, Default)]
pub struct PresetTable {
    presets: BTreeMap<String, Vec<String>>,
}

/// The compress presets, from `config_src/video_compress_preset.json`, same two layouts.
#[derive(Debug, Clone, Default)]
pub struct CompressTable {
    presets: BTreeMap<(String, String), CompressPreset>,
}

/// One encoder: the codec name and the flag that carries its rate control, which is not always
/// `-crf` (`-cq` for nvenc/amf, and a whole string of options for av1 on CPU).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressPreset {
    pub encoder: String,
    pub crf_flag: Vec<String>,
}

/// Where the shipped tables live inside an install.
///
/// Spelled the way the payload ships them. The file is *found* through
/// [`wind_base::install::config_src_file`], which also accepts the `windrecorder/config_src/` copy
/// an overlay install still has — these constants are what an error message names, and an error
/// that named the wrong one of the two would send a user looking in the wrong directory.
pub const RECORD_PRESET_RELPATH: &str = "config_src/record_preset.json";
pub const COMPRESS_PRESET_RELPATH: &str = "config_src/video_compress_preset.json";

/// Reads and parses one JSON object of presets.
///
/// `name` is looked up in whichever settings directory this root carries. A root with neither is
/// reported against the payload spelling, because that is the file the user's payload was meant to
/// unpack.
fn read_table(root: &Path, name: &str) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let path = wind_base::install::config_src_file(root, name);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(value.as_object().cloned().unwrap_or_default())
}

impl PresetTable {
    /// `{"cpu_h264": {"ffmpeg_cmd": ["-c:v", "libx264", "-b:v", "BITRATE"]}, …}`.
    pub fn load(root: &Path) -> Result<PresetTable, String> {
        let mut presets = BTreeMap::new();
        for (name, entry) in read_table(root, "record_preset.json")? {
            let args = entry
                .get("ffmpeg_cmd")
                .and_then(|c| c.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect::<Vec<_>>())
                .unwrap_or_default();
            if !args.is_empty() {
                presets.insert(name, args);
            }
        }
        if presets.is_empty() {
            return Err(format!("{} declares no usable ffmpeg_cmd preset", RECORD_PRESET_RELPATH));
        }
        Ok(PresetTable { presets })
    }

    pub fn names(&self) -> Vec<&str> {
        self.presets.keys().map(String::as_str).collect()
    }

    /// The encoder arguments for one preset, with the config's rate-control numbers substituted in.
    ///
    /// Upstream's `_replace_value_in_args` writes `BITRATE` as `"{kbps}k"` and `CRF_NUM` as the
    /// configured CRF, and it is the preset that chooses which of the two it uses. A preset naming
    /// neither gets `-crf` appended: a rate control mode has to be stated, and CRF is what the
    /// shipped compress defaults use.
    pub fn encoder_args(&self, name: &str, bitrate_kbps: i64, crf: i64) -> Result<Vec<String>, String> {
        let template = self.presets.get(name).ok_or_else(|| {
            format!("record_encoder '{name}' is not in {RECORD_PRESET_RELPATH} (known: {})", self.names().join(", "))
        })?;
        let mut args = Vec::with_capacity(template.len() + 2);
        let mut states_rate = false;
        for token in template {
            match token.as_str() {
                "BITRATE" => {
                    args.push(format!("{bitrate_kbps}k"));
                    states_rate = true;
                }
                "CRF_NUM" => {
                    args.push(crf.to_string());
                    states_rate = true;
                }
                "-crf" | "-b:v" | "-cq" => {
                    args.push(token.clone());
                    states_rate = true;
                }
                other => args.push(other.to_string()),
            }
        }
        if !states_rate {
            args.extend(["-crf".to_string(), crf.to_string()]);
        }
        Ok(args)
    }
}

impl CompressTable {
    /// `{"x264": {"cpu": {"encoder": "libx264", "crf_flag": "-crf"}}, …}`.
    pub fn load(root: &Path) -> Result<CompressTable, String> {
        let mut presets = BTreeMap::new();
        for (encoder, accelerators) in read_table(root, "video_compress_preset.json")? {
            for (accelerator, entry) in accelerators.as_object().into_iter().flatten() {
                let Some(name) = entry.get("encoder").and_then(|v| v.as_str()) else { continue };
                let flag = entry.get("crf_flag").and_then(|v| v.as_str()).unwrap_or("-crf");
                presets.insert(
                    (encoder.clone(), accelerator.clone()),
                    CompressPreset { encoder: name.to_string(), crf_flag: flag.split_whitespace().map(str::to_string).collect() },
                );
            }
        }
        if presets.is_empty() {
            return Err(format!("{} declares no usable compress preset", COMPRESS_PRESET_RELPATH));
        }
        Ok(CompressTable { presets })
    }

    pub fn get(&self, encoder: &str, accelerator: &str) -> Option<&CompressPreset> {
        self.presets.get(&(encoder.to_string(), accelerator.to_string()))
    }
}

// ----------------------------------------------------------------------------------------------
// Availability: the difference between a preset being *named* and a machine being able to encode
// with it.
//
// Every encoder name in both tables is a name the *operator* wrote down, and an ffmpeg can be built
// without it, or a machine can lack the GPU it needs. Neither of those is visible from the name, and
// until this section existed neither was visible to the user either: the convert pass simply failed
// every slice in the cache, left them unmarked, and retried the same failure on every idle window,
// while the footage sat as JPEGs that nothing would ever turn into a video. Falling back to a CPU
// encoder is the only answer that keeps the footage; saying so out loud is the only answer that
// keeps the user's trust. Both halves are mandatory — see the module header.

/// What asking ffmpeg about one encoder could establish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderAvailability {
    /// ffmpeg opened this encoder on a trial frame. Says nothing about speed or quality; says the
    /// pass will produce a file.
    Usable,
    /// ffmpeg ran and refused this encoder. The string is ffmpeg's own last words, so the user can
    /// tell "this build has no AMF" apart from "this machine has no AMD GPU".
    Unusable(String),
    /// ffmpeg could not be executed at all, so nothing was learned about *this* encoder and no
    /// substitution is justified — the real encode's own "cannot start ffmpeg" message is the one
    /// worth reading, and a fallback note would sit in front of it.
    Unknown,
}

/// The preset `encoder_args` falls back to, and the row the compress pass falls back to.
///
/// The same name the settings page's own default carries, and the reason `windui`'s `RecOptions`
/// lists `cpu_h264` first: a pure-CPU H.264 encoder is the one thing a stock ffmpeg always has.
pub const CPU_FALLBACK_PRESET: &str = "cpu_h264";
pub const CPU_FALLBACK_ACCELERATOR: &str = "cpu";
pub const CPU_FALLBACK_ENCODER: &str = "x264";

/// The codec one already-substituted encoder argument vector asks ffmpeg for.
///
/// `None` for a preset that names no `-c:v` at all. Such a preset is the operator's own business —
/// ffmpeg would take its default codec — and there is nothing to probe, so the resolver leaves it
/// exactly as written rather than inventing a question it cannot answer.
pub fn codec_in_args(args: &[String]) -> Option<&str> {
    args.iter().position(|a| a == "-c:v").and_then(|at| args.get(at + 1).map(String::as_str))
}

/// What one convert pass will encode with, and the sentence it owes the user when that is not the
/// name `record_encoder` held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderChoice {
    pub args: Vec<String>,
    /// `Some(reason)` exactly when `args` is *not* what the configured preset asked for.
    pub note: Option<String>,
}

/// Resolve `record_encoder` into the arguments to run, probing availability through `check`.
///
/// Three outcomes, in the order they are discovered:
///
///   * the name is not in `record_preset.json` at all → the existing reported fallback, because a
///     name the file has never heard of is a typo or a hand-edit, not a hardware question;
///   * the name resolves and ffmpeg can open it → exactly what was asked for, no note;
///   * the name resolves and ffmpeg *cannot* open it → the CPU preset's arguments plus a note
///     naming both the encoder that failed and ffmpeg's own reason. This is the case the whole
///     section exists for: `AMD_h265` on a machine with no AMD encoder.
///
/// `check` is a parameter rather than a call into [`crate::layout`] so that every branch above is
/// testable without an ffmpeg on the machine — which matters, because the third branch is the one a
/// developer's machine is least likely to reproduce.
pub fn resolve_encoder(
    requested: &str,
    table: &PresetTable,
    bitrate: i64,
    crf: i64,
    check: &dyn Fn(&str) -> EncoderAvailability,
) -> EncoderChoice {
    let cpu_args = || match table.encoder_args(CPU_FALLBACK_PRESET, bitrate, crf) {
        Ok(args) => args,
        // The table is readable but does not carry the fallback row: the same argument vector the
        // rest of this binary already uses when it has nothing else to go on.
        Err(_) => vec!["-c:v".to_string(), "libx264".to_string(), "-b:v".to_string(), format!("{bitrate}k")],
    };
    let args = match table.encoder_args(requested, bitrate, crf) {
        Ok(args) => args,
        Err(e) => {
            let fallback = cpu_args();
            return EncoderChoice {
                args: fallback.clone(),
                note: Some(format!("{e}; encoding this pass with {} instead", codec_in_args(&fallback).unwrap_or("libx264"))),
            };
        }
    };
    let Some(codec) = codec_in_args(&args) else {
        return EncoderChoice { args, note: None };
    };
    match check(codec) {
        EncoderAvailability::Usable | EncoderAvailability::Unknown => EncoderChoice { args, note: None },
        EncoderAvailability::Unusable(reason) => {
            let fallback = cpu_args();
            let to = codec_in_args(&fallback).unwrap_or("libx264");
            let note = if to == codec {
                format!("{codec} asked for by record_encoder '{requested}' is not usable here: {reason}; and no different fallback exists")
            } else {
                format!("record_encoder '{requested}' asks for {codec}, which this ffmpeg cannot open: {reason}; encoding this pass with {to} instead")
            };
            EncoderChoice { args: fallback, note: Some(note) }
        }
    }
}

/// What one retention pass will re-encode with, and the sentence it owes the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressChoice {
    pub preset: CompressPreset,
    pub note: Option<String>,
}

/// Keep [`compress_preset`]'s answer, unless ffmpeg says this machine cannot open it.
///
/// `asked` is the config's own wording (`x264` on `qsv`) so the note can name what the user set
/// rather than only what the table resolved to. Unlike the record path, a failed compress encode
/// costs no footage — the source is kept — but it does mean the retention window silently stops
/// shrinking the library, which is a disk-full problem with no warning at all.
pub fn resolve_compress(
    asked: &str,
    preset: CompressPreset,
    cpu_fallback: Option<CompressPreset>,
    check: &dyn Fn(&str) -> EncoderAvailability,
) -> CompressChoice {
    match check(&preset.encoder) {
        EncoderAvailability::Usable | EncoderAvailability::Unknown => CompressChoice { preset, note: None },
        EncoderAvailability::Unusable(reason) => match cpu_fallback {
            Some(fallback) if fallback.encoder != preset.encoder => CompressChoice {
                note: Some(format!(
                    "{asked} asks for {}, which this ffmpeg cannot open: {reason}; re-encoding this pass with {} instead",
                    preset.encoder, fallback.encoder
                )),
                preset: fallback,
            },
            _ => CompressChoice {
                note: Some(format!("{} asked for by {asked} is not usable here: {reason}; and no different fallback exists", preset.encoder)),
                preset,
            },
        },
    }
}

/// Seconds each captured frame holds the screen: the gap to the next capture, with the tail on the
/// last one.
///
/// `times` must be ascending — it is the sorted frame stamps — because a slice whose frames were
/// written out of order by a clock adjustment would otherwise produce a negative duration, which the
/// concat demuxer reads as a request to drop the entry.
pub fn frame_durations(times: &[i64], tail: i64) -> Vec<i64> {
    let mut out = Vec::with_capacity(times.len());
    for pair in times.windows(2) {
        out.push((pair[1] - pair[0]).max(1));
    }
    if !times.is_empty() {
        out.push(tail.max(1));
    }
    out
}

/// The concat demuxer's list file: one `file` line per second a frame is held.
///
/// Repetition rather than `duration` directives, for two reasons that turn out to be one reason. This
/// ffmpeg's concat demuxer uses a `duration` only to time-stamp the following entry: with the input rate
/// pinned to 1 fps the frame still decodes once, so a six-screen slice came out seven seconds long
/// instead of sixty-two. Writing the frame once per second it owns is what actually puts those seconds
/// in the file — and it is literally what upstream did, which looped
/// `for j in range(duration): video.write(frame)` over a 1 fps writer.
///
/// `list_dir` is the directory the list file itself is written to, and every entry is made relative to
/// it: the concat demuxer resolves a relative entry against the *list's* directory, not the process
/// working directory, so a list written beside the frames that names them by project-root path asks
/// ffmpeg to open `slice/./cache_screenshot/slice/frame.jpg`. Measured, not theorised — that is
/// exactly the error the first real conversion produced.
pub fn concat_list(entries: &[(PathBuf, i64)], list_dir: &Path) -> String {
    let mut out = String::from("ffconcat version 1.0\n");
    for (path, seconds) in entries {
        let held = path
            .strip_prefix(list_dir)
            .unwrap_or(path.as_path());
        let line = format!("file '{}'\n", concat_path(held));
        for _ in 0..(*seconds).max(1) {
            out.push_str(&line);
        }
    }
    out
}

/// A path as the concat demuxer wants it: forward slashes, and `'` escaped by closing the quoted
/// string around it, which is the form ffmpeg's own documentation uses.
fn concat_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").replace('\'', "'\\''")
}

/// A canvas every frame fits inside, or `None` when they already share a usable size.
///
/// The deployed config captures the foreground window, so one slice legitimately mixes a 1920x1080
/// grab with a 900x600 one, and ffmpeg refuses to concatenate mismatched frame sizes. Upstream solves
/// this by rewriting every JPEG onto a max-size canvas before encoding
/// (`convert_screenshots_dir_into_same_size_to_cache`); a filter reaches the same picture without
/// touching the user's frames, and the colour is the config's so the letterbox looks like today's.
///
/// The canvas is rounded down to even because `yuv420p` cannot represent an odd raster — libx264
/// fails the encode outright on one, and a window dragged to a size like 1011 px wide makes that a
/// real, not hypothetical, input.
pub fn video_canvas(sizes: &[(u32, u32)], color: &str) -> Option<Canvas> {
    let (width, height) = *sizes.first()?;
    if sizes.iter().all(|s| *s == (width, height)) && width % 2 == 0 && height % 2 == 0 {
        return None;
    }
    let width = sizes.iter().map(|s| s.0).max()?.max(2) & !1;
    let height = sizes.iter().map(|s| s.1).max()?.max(2) & !1;
    Some(Canvas { width, height, color: color.to_string() })
}

/// The letterbox target for a slice's frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub color: String,
}

impl Canvas {
    /// `scale` then `pad`, centred: shrinking to fit rather than stretching keeps text legible, which
    /// is the only thing a captured screen is for.
    pub fn filter(&self) -> String {
        format!(
            "scale={w}:{h}:force_original_aspect_ratio=decrease:flags=neighbor,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color={c}",
            w = self.width,
            h = self.height,
            c = self.color
        )
    }
}

/// The ffmpeg argument vector for a slideshow encode, without the program itself.
///
/// `-r 1` before `-i` makes each `file` line of the concat list exactly one second of screen, which is
/// how [`concat_list`] spells a hold; `-r 1` after the encoder pins the container's timescale, so a
/// player that seeks to second S lands on frame S — the property every `videofile_time` → offset lookup
/// in the product rests on.
pub fn encode_args(list: &Path, output: &Path, encoder: &[String], canvas: Option<&Canvas>) -> Vec<String> {
    let mut args = vec![
        "-r".to_string(),
        "1".to_string(),
        "-f".to_string(),
        "concat".to_string(),
        "-safe".to_string(),
        // The list holds absolute paths, which is exactly what the default safety rule rejects.
        "0".to_string(),
        "-i".to_string(),
        native_path(list),
    ];
    args.extend(encoder.iter().cloned());
    args.extend(["-pix_fmt".to_string(), "yuv420p".to_string()]);
    if let Some(canvas) = canvas {
        args.extend(["-vf".to_string(), canvas.filter()]);
    }
    args.extend(["-r".to_string(), "1".to_string(), "-an".to_string()]);
    // A `moov` atom at the tail means the file cannot be played while it is still being written, and
    // the UI re-opens a segment for every thumbnail scrub.
    args.extend(["-movflags".to_string(), "+faststart".to_string(), "-y".to_string(), native_path(output)]);
    args
}

/// The argument vector for the retention pass's re-compression of an old segment.
///
/// Mirrors `compress_video_CLI`: hardware accel, a scale step down, the accelerator's own quality
/// flag, `-preset medium`, and the same yuv420p guarantee the encoder was chosen for. The scale is
/// written as a `trunc(.../2)*2` expression for the same even-dimension reason [`video_canvas`]
/// rounds, since a stored 1081-px-high grab must not fail the encode it is being rescued by.
pub fn compress_args(input: &Path, output: &Path, preset: &CompressPreset, crf: i64, scale: f64, threads: Option<i64>) -> Vec<String> {
    let factor = scale.clamp(0.05, 4.0);
    let mut args = vec![
        "-hwaccel".to_string(),
        "auto".to_string(),
        "-i".to_string(),
        native_path(input),
        "-vf".to_string(),
        format!("scale=trunc(iw*{factor}/2)*2:trunc(ih*{factor}/2)*2"),
    ];
    if let Some(threads) = threads {
        args.extend(["-threads".to_string(), threads.to_string()]);
    }
    args.push("-c:v".to_string());
    args.push(preset.encoder.clone());
    args.extend(preset.crf_flag.iter().cloned());
    args.push(crf.to_string());
    args.extend(["-preset".to_string(), "medium".to_string(), "-pix_fmt".to_string(), "yuv420p".to_string()]);
    args.extend(["-y".to_string(), native_path(output)]);
    args
}

/// A path in the form ffmpeg accepts on every platform.
fn native_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Width and height of a frame without decoding it: the header is enough, and reading 64 KiB of a
/// multi-megabyte JPEG is cheaper than a decode that would need another dependency for one number.
pub fn image_dimensions(path: &Path) -> Option<(u32, u32)> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; 64 * 1024];
    let read = file.read(&mut head).ok()?;
    let bytes = &head[..read];
    jpeg_dimensions(bytes).or_else(|| png_dimensions(bytes))
}

/// Baseline/progressive JPEG: walk the marker segments until a start-of-frame says how big the
/// raster is, stopping at the first scan segment (past which everything is entropy-coded).
pub fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut at = 2usize;
    while at + 1 < bytes.len() {
        if bytes[at] != 0xFF {
            at += 1;
            continue;
        }
        let marker = bytes[at + 1];
        if marker == 0xFF {
            at += 1;
            continue;
        }
        // Standalone markers (RSTn, SOI, EOI, TEM, the fill byte) declare no length.
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            at += 2;
            continue;
        }
        if marker == 0xDA || at + 4 > bytes.len() {
            return None;
        }
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            // segment layout: marker(2) length(2) precision(1) height(2) width(2)
            let body = at + 4;
            if body + 5 > bytes.len() {
                return None;
            }
            let height = u16::from_be_bytes([bytes[body + 1], bytes[body + 2]]);
            let width = u16::from_be_bytes([bytes[body + 3], bytes[body + 4]]);
            return if width == 0 || height == 0 { None } else { Some((u32::from(width), u32::from(height))) };
        }
        let length = u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]) as usize;
        if length < 2 {
            return None;
        }
        at += 2 + length;
    }
    None
}

/// PNG: IHDR is the first chunk by definition, and its layout is fixed.
pub fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.len() < 24 || bytes[..8] != SIGNATURE || bytes[12..16] != *b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(pairs: &[(&str, i64)]) -> Vec<(PathBuf, i64)> {
        pairs.iter().map(|(p, s)| (PathBuf::from(p), *s)).collect()
    }

    /// Where the install root sits as seen from this crate: two levels up from `windcap/maint`.
    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).unwrap().to_path_buf()
    }

    /// The preset files the install ships, parsed by the code the run path uses.
    #[test]
    fn the_shipped_record_presets_parse_and_substitute() {
        let table = PresetTable::load(&repo_root()).expect("record_preset.json must load");
        assert!(table.names().contains(&"cpu_h264"), "known: {:?}", table.names());
        assert_eq!(
            table.encoder_args("cpu_h264", 200, 39).unwrap(),
            ["-c:v", "libx264", "-b:v", "200k"],
            "byte for byte what the Python recorder passes"
        );
        let av1 = table.encoder_args("SVT-AV1", 400, 39).unwrap();
        assert_eq!(&av1[..4], ["-c:v", "libsvtav1", "-b:v", "400k"]);
        assert!(av1.contains(&"-svtav1-params".to_string()), "the preset's own tuning survives substitution");
        assert!(table.encoder_args("nvidia_rav1e", 200, 39).is_err(), "an invented preset must be reported");
    }

    #[test]
    fn the_shipped_compress_presets_name_an_encoder_per_accelerator() {
        let table = CompressTable::load(&repo_root()).expect("video_compress_preset.json must load");
        assert_eq!(
            table.get("x264", "cpu").unwrap(),
            &CompressPreset { encoder: "libx264".into(), crf_flag: vec!["-crf".into()] }
        );
        assert_eq!(table.get("x265", "nvenc").unwrap().crf_flag, ["-cq"]);
        assert!(table.get("nope", "cpu").is_none());
    }

    /// A preset written in the other rate-control dialect still has to state a rate.
    #[test]
    fn crf_and_implicit_rate_placeholders_are_both_honoured() {
        let table = PresetTable {
            presets: BTreeMap::from([
                ("crf_based".to_string(), vec!["-c:v".into(), "libx264".into(), "-crf".into(), "CRF_NUM".into()]),
                ("bare".to_string(), vec!["-c:v".into(), "libx264".into()]),
            ]),
        };
        assert_eq!(table.encoder_args("crf_based", 200, 28).unwrap(), ["-c:v", "libx264", "-crf", "28"]);
        assert_eq!(table.encoder_args("bare", 200, 28).unwrap(), ["-c:v", "libx264", "-crf", "28"]);
    }

    /// Every shipped record preset states its rate with `-b:v BITRATE`, which is the branch of
    /// [`PresetTable::encoder_args`] that suppresses the appended `-crf`. Pinned here because it is
    /// the fact that makes the settings page's own CRF row a lie on a stock install, and the reason
    /// that row was removed rather than re-worded: `record_crf` reaches ffmpeg only through a preset
    /// whose text names `-crf` or `CRF_NUM`, and none of the five shipped ones does.
    #[test]
    fn no_shipped_record_preset_carries_the_crf_into_the_command_line() {
        let table = PresetTable::load(&repo_root()).expect("record_preset.json must load");
        for name in table.names() {
            let args = table.encoder_args(name, 200, 22).unwrap();
            assert!(!args.iter().any(|a| a == "-crf"), "{name}: {args:?} states its rate another way and never reads record_crf");
            assert!(!args.iter().any(|a| a == "22"), "{name}: the configured CRF leaked into {args:?} anyway");
        }
    }

    fn table_with(entries: &[(&str, &[&str])]) -> PresetTable {
        PresetTable {
            presets: entries
                .iter()
                .map(|(name, args)| (name.to_string(), args.iter().map(|a| a.to_string()).collect::<Vec<_>>()))
                .collect(),
        }
    }

    fn always_usable(_codec: &str) -> EncoderAvailability {
        EncoderAvailability::Usable
    }

    fn only_amd_is_missing(codec: &str) -> EncoderAvailability {
        if codec == "hevc_amf" {
            EncoderAvailability::Unusable("DLL amfrt64.dll failed to open".to_string())
        } else {
            EncoderAvailability::Usable
        }
    }

    /// The three outcomes of the record-path decision, each of which the settings page can put a user
    /// into. The middle one is the defect this section closes: a name that is *in* the preset file and
    /// still cannot encode on this machine used to fail every slice in the cache, silently, forever.
    #[test]
    fn a_record_encoder_the_machine_cannot_open_falls_back_and_says_so() {
        let table = table_with(&[
            ("cpu_h264", &["-c:v", "libx264", "-b:v", "BITRATE"]),
            ("AMD_h265", &["-c:v", "hevc_amf", "-b:v", "BITRATE"]),
        ]);

        // (a) asked for, usable, delivered: no note, because there is nothing to explain.
        let ok = resolve_encoder("cpu_h264", &table, 200, 39, &always_usable);
        assert_eq!(ok.args, ["-c:v", "libx264", "-b:v", "200k"]);
        assert_eq!(ok.note, None, "{:?}", ok.note);

        // (b) a hardware name the machine cannot open: the CPU preset's arguments, and a sentence
        //     naming both what failed and ffmpeg's own reason.
        let fell = resolve_encoder("AMD_h265", &table, 200, 39, &only_amd_is_missing);
        assert_eq!(fell.args, ["-c:v", "libx264", "-b:v", "200k"], "the footage still becomes a video");
        let note = fell.note.expect("and the user is told the encoder was not the one they picked");
        assert!(note.contains("AMD_h265") && note.contains("hevc_amf"), "{note}");
        assert!(note.contains("amfrt64.dll"), "ffmpeg's own reason is carried through: {note}");
        assert!(note.contains("libx264 instead"), "{note}");

        // (c) a name that is not in the file at all — a typo, or an encoder someone deleted from the
        //     preset file by hand. Still encoded, still explained, still without a silent swap.
        let unknown = resolve_encoder("nvidia_rav1e", &table, 200, 39, &always_usable);
        assert_eq!(unknown.args, ["-c:v", "libx264", "-b:v", "200k"]);
        let note = unknown.note.expect("an unresolvable name is reported, not swallowed");
        assert!(note.contains("nvidia_rav1e") && note.contains("known: AMD_h265, cpu_h264"), "{note}");
    }

    /// A probe that could not run must not be read as a refusal. Falling back on `ffmpeg` being
    /// missing would replace the one message that says why nothing worked at all with a story about
    /// hardware nobody asked about.
    #[test]
    fn an_unanswered_probe_changes_nothing_about_which_encoder_runs() {
        let table = table_with(&[("cpu_h264", &["-c:v", "libx264", "-b:v", "BITRATE"]), ("AMD_h265", &["-c:v", "hevc_amf", "-b:v", "BITRATE"])]);
        for unknown in [EncoderAvailability::Unknown, EncoderAvailability::Usable] {
            let choice = resolve_encoder("AMD_h265", &table, 200, 39, &|_| unknown.clone());
            assert_eq!(choice.args, ["-c:v", "hevc_amf", "-b:v", "200k"], "{unknown:?} must not be read as a refusal");
            assert_eq!(choice.note, None, "and must not be explained as a substitution");
        }
    }

    /// When the requested encoder and the only fallback are the same codec, there is no substitution
    /// to make, and a note claiming one would be a second lie on top of the first.
    #[test]
    fn a_fallback_that_is_the_same_encoder_admits_there_is_none() {
        let table = table_with(&[("cpu_h264", &["-c:v", "libx264", "-b:v", "BITRATE"])]);
        let choice = resolve_encoder("cpu_h264", &table, 200, 39, &|_| EncoderAvailability::Unusable("no x264 here".into()));
        assert_eq!(choice.args, ["-c:v", "libx264", "-b:v", "200k"], "the encode still runs, and still fails, as asked");
        let note = choice.note.expect("and the note says there was nowhere to fall back to");
        assert!(note.contains("no different fallback exists"), "{note}");
        assert!(!note.contains("instead"), "{note}");
    }

    /// An empty table is the shape `convert` reaches when `config_src/record_preset.json` could not be
    /// read at all, and it is also the shape of `PresetTable::default()`. The pass must still encode.
    #[test]
    fn an_unreadable_preset_table_still_produces_an_encoder_and_a_note() {
        let choice = resolve_encoder("SVT-AV1", &PresetTable::default(), 200, 39, &always_usable);
        assert_eq!(choice.args, ["-c:v", "libx264", "-b:v", "200k"]);
        let note = choice.note.expect("the fallback is stated");
        assert!(note.contains("SVT-AV1") && note.contains("libx264 instead"), "{note}");
    }

    #[test]
    fn the_codec_a_preset_asked_for_is_recoverable_from_its_own_arguments() {
        assert_eq!(codec_in_args(&["-c:v".into(), "hevc_nvenc".into(), "-b:v".into(), "400k".into()]).map(str::to_string), Some("hevc_nvenc".into()));
        assert_eq!(codec_in_args(&["-b:v".into(), "400k".into()]), None, "a preset that names no codec is ffmpeg's business, not ours");
        assert_eq!(codec_in_args(&["-c:v".into()]), None, "a dangling -c:v is not a codec either");
    }

    #[test]
    fn a_compress_accelerator_the_machine_cannot_open_falls_back_to_the_cpu_row() {
        let cpu = CompressPreset { encoder: "libx264".into(), crf_flag: vec!["-crf".into()] };
        let qsv = CompressPreset { encoder: "h264_qsv".into(), crf_flag: vec!["-global_quality:v".into()] };
        let asked = "compress_encoder 'x264' with compress_accelerator 'qsv'";

        let kept = resolve_compress(asked, qsv.clone(), Some(cpu.clone()), &|_| EncoderAvailability::Usable);
        assert_eq!((kept.preset, kept.note), (qsv.clone(), None));

        let fell = resolve_compress(asked, qsv.clone(), Some(cpu.clone()), &|_| EncoderAvailability::Unusable("no Intel GPU".into()));
        assert_eq!(fell.preset, cpu, "the library still shrinks, on the CPU");
        let note = fell.note.expect("and the substitution is said out loud");
        assert!(note.contains("h264_qsv") && note.contains("no Intel GPU") && note.contains("libx264 instead"), "{note}");

        // No table row to fall back to: keep the asked encoder and admit it, rather than reporting a
        // substitution that did not happen.
        let alone = resolve_compress(asked, qsv, None, &|_| EncoderAvailability::Unusable("no Intel GPU".into()));
        assert_eq!(alone.preset, CompressPreset { encoder: "h264_qsv".into(), crf_flag: vec!["-global_quality:v".into()] });
        let note = alone.note.expect("a refusal with nowhere to go is still reported");
        assert!(note.contains("no different fallback exists"), "{note}");
    }

    #[test]
    fn durations_are_the_gap_to_the_next_capture_plus_the_tail() {
        // A real slice: 21-16-12 → 21-16-20 → 21-16-32 → 21-16-45 → 21-16-49.
        let times = [0, 8, 20, 33, 37];
        assert_eq!(frame_durations(&times, TAIL_SECONDS), vec![8, 12, 13, 4, 2]);
        assert_eq!(frame_durations(&[5], TAIL_SECONDS), vec![2], "one frame is a two second clip");
        assert_eq!(frame_durations(&[], TAIL_SECONDS), Vec::<i64>::new());
        // Identical timestamps, which a coarse clock really can produce, still each show a frame.
        assert_eq!(frame_durations(&[10, 10, 10], TAIL_SECONDS), vec![1, 1, 2]);
    }

    #[test]
    fn the_concat_list_holds_each_frame_for_its_own_seconds() {
        let first = "C:/cache/2026-09-21_21-16-12/2026-09-21_21-16-12.jpg";
        let list = concat_list(
            &entries(&[(first, 8), ("C:/cache/a/b.jpg", 2)]),
            Path::new("C:/cache/2026-09-21_21-16-12"),
        );
        let lines: Vec<&str> = list.lines().collect();
        assert_eq!(lines[0], "ffconcat version 1.0");
        assert_eq!(lines.len(), 1 + 8 + 2, "one line per held second:\n{list}");
        // The frame next to the list file is named by itself, not by the path that got us here:
        // ffmpeg resolves a relative entry against the list's own directory, so the absolute form
        // becomes `slice/./cache/slice/frame.jpg` and the encode fails. This is the regression.
        assert!(
            lines[1..9].iter().all(|l| *l == "file '2026-09-21_21-16-12.jpg'"),
            "{list}"
        );
        assert!(lines[9..].iter().all(|l| *l == "file 'C:/cache/a/b.jpg'"), "{list}");
        // A zero-second gap, which a coarse clock really produces, still shows the frame once.
        assert_eq!(concat_list(&entries(&[("a.jpg", 0)]), Path::new(".")).lines().count(), 2);
        assert_eq!(concat_list(&[], Path::new(".")), "ffconcat version 1.0\n");
    }

    #[test]
    fn concat_paths_survive_backslashes_and_quotes() {
        assert_eq!(concat_path(Path::new("C:\\a b\\c.jpg")), "C:/a b/c.jpg");
        assert_eq!(concat_path(Path::new("C:/it's/x.jpg")), "C:/it'\\''s/x.jpg");
    }

    #[test]
    fn the_encode_command_puts_the_frame_rate_on_both_sides() {
        let args = encode_args(
            &PathBuf::from("C:/cache/slice/windmaint_concat.txt"),
            &PathBuf::from("E:/videos/2026-09/2026-09-21_21-16-12.mp4"),
            &["-c:v".to_string(), "libx264".to_string(), "-b:v".to_string(), "200k".to_string()],
            None,
        );
        assert_eq!(&args[..8], ["-r", "1", "-f", "concat", "-safe", "0", "-i", "C:/cache/slice/windmaint_concat.txt"]);
        assert_eq!(args.last().unwrap(), "E:/videos/2026-09/2026-09-21_21-16-12.mp4");
        let joined = args.join(" ");
        assert!(joined.contains("-pix_fmt yuv420p"), "{joined}");
        assert!(joined.contains("-movflags +faststart"), "{joined}");
        assert!(!joined.contains("-vf"), "uniform frames need no filter: {joined}");
        assert_eq!(args.iter().filter(|a| *a == "-r").count(), 2, "one input rate, one output rate");
        assert_eq!(args.iter().filter(|a| *a == "-y").count(), 1);
    }

    #[test]
    fn mismatched_or_odd_frame_sizes_are_padded_onto_an_even_canvas() {
        assert_eq!(video_canvas(&[(1920, 1080), (1920, 1080)], "#EEE3DA"), None, "uniform is free");
        assert_eq!(video_canvas(&[], "#EEE3DA"), None);
        assert_eq!(video_canvas(&[(1920, 1081)], "#EEE3DA").unwrap().height, 1080, "odd must become even");

        let canvas = video_canvas(&[(1920, 1080), (900, 600)], "#EEE3DA").unwrap();
        assert_eq!((canvas.width, canvas.height), (1920, 1080));
        assert_eq!(
            canvas.filter(),
            "scale=1920:1080:force_original_aspect_ratio=decrease:flags=neighbor,\
             pad=1920:1080:(ow-iw)/2:(oh-ih)/2:color=#EEE3DA"
        );

        let args = encode_args(&PathBuf::from("l.txt"), &PathBuf::from("o.mp4"), &["-c:v".into(), "x".into()], Some(&canvas));
        assert_eq!(args[args.iter().position(|a| a == "-vf").unwrap() + 1], canvas.filter());
    }

    #[test]
    fn the_compress_command_scales_even_and_states_the_accelerator_quality_flag() {
        let preset = CompressPreset { encoder: "libx265".into(), crf_flag: vec!["-global_quality:v".into()] };
        let args = compress_args(
            &PathBuf::from("E:/videos/2026-01/old-OCRED.mp4"),
            &PathBuf::from("E:/videos/2026-01/old-COMPRESS-OCRED.mp4"),
            &preset,
            39,
            0.5,
            Some(2),
        );
        let joined = args.join(" ");
        assert!(joined.starts_with("-hwaccel auto -i E:/videos/2026-01/old-OCRED.mp4 -vf scale=trunc(iw*0.5/2)*2:trunc(ih*0.5/2)*2"), "{joined}");
        assert!(joined.contains("-threads 2 -c:v libx265 -global_quality:v 39 -preset medium -pix_fmt yuv420p"), "{joined}");
        assert!(joined.ends_with("-y E:/videos/2026-01/old-COMPRESS-OCRED.mp4"), "{joined}");

        // A CPU-less accelerator gets no -threads: upstream only passes it when compress_accelerator
        // is cpu, and forcing it on nvenc changes what the encoder does with the frame queue.
        let args = compress_args(&PathBuf::from("i"), &PathBuf::from("o"), &preset, 30, 1.5, None);
        assert!(!args.iter().any(|a| a == "-threads"), "{:?}", args);
        assert!(args.iter().any(|a| a == "scale=trunc(iw*1.5/2)*2:trunc(ih*1.5/2)*2"), "{:?}", args);
    }

    #[test]
    fn jpeg_headers_are_measured_without_a_decode() {
        // A hand-built JFIF header followed by SOF0 declaring 6x4.
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        bytes.extend(b"JFIF\0");
        bytes.resize(20, 0);
        bytes.extend([0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x04, 0x00, 0x06]);
        assert_eq!(jpeg_dimensions(&bytes), Some((6, 4)));

        // A truncated file, a non-JPEG, and a scan reached before any SOF all answer "unknown"
        // rather than guessing a size the encoder would then be blamed for.
        assert_eq!(jpeg_dimensions(&bytes[..12]), None);
        assert_eq!(jpeg_dimensions(b"not a jpeg"), None);
        assert_eq!(jpeg_dimensions(&[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x04, 0x00, 0x00]), None);
        assert_eq!(jpeg_dimensions(&[]), None);
    }

    #[test]
    fn png_headers_are_measured_too() {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend([0x00, 0x00, 0x00, 0x0D]);
        bytes.extend(b"IHDR");
        bytes.extend([0x00, 0x00, 0x02, 0x80, 0x00, 0x00, 0x01, 0xC8]);
        assert_eq!(png_dimensions(&bytes), Some((640, 456)));
        assert_eq!(png_dimensions(&bytes[..20]), None);
        assert_eq!(png_dimensions(b"\x89PNGxxxxxxxxxxxx"), None);
    }
}
