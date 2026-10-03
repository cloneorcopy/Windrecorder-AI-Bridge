// The shared Windows version resource, included by every native binary's `build.rs`.
//
// This is a build-script *fragment*, not a module: it is `include!`d into the eleven `build.rs` files
// in this workspace, and each one then calls `version_resource("<binary name>")`. It lives here,
// beside `wind-base`, because that crate is already the single place the eleven binaries agree on (it
// is a dependency of all of them) and because the alternative -- eleven copies of a `.rc` and its
// field list -- is exactly the drift this file exists to prevent. `wind-base` does not *declare* it
// as a module, so nothing here reaches a shipped binary at run time; the only thing that ends up in
// an `.exe` is the compiled resource.
//
// Two consequences of being included rather than linked, both deliberate:
//   * no `pub`, no `//!`, and every item here is reachable from `version_resource`, because the
//     fragment is compiled as part of eleven separate build scripts and an unused item in one of them
//     would be a dead-code warning in somebody's build.
//   * the crate doing the including must carry `embed-resource` in `[build-dependencies]`; that is
//     the one line each of the eleven manifests adds.
//
// `embed-resource` resolves entirely out of this machine's offline registry cache (verified: it
// pulls cc, memchr, rustc_version/semver, toml/toml_edit/winnow, vswhom and winreg, all present),
// so `cargo build --offline` stays the gate it is upstream. It locates `rc.exe` in the Windows
// Kits, compiles the script into `$OUT_DIR`, and emits `cargo:rustc-link-arg-bin=<binary>=<object>`
// -- link args for *binaries only*, which is what we want: libraries and test harnesses are
// untouched, so `cargo test` never grows a dependency on the resource compiler.
//
// The same mechanism also carries icon resources, because an icon is the other thing that has to be
// *in* the .exe rather than beside it. See `icon_resources` for the directory convention that makes
// this optional for nine of the eleven binaries without a second entry point -- a second entry point
// would be dead code in nine build scripts, which is the one thing the shape of this file exists to
// avoid.

/// Every field that is the same for all eleven binaries lives up here, so the eleven Properties tabs can
/// only ever differ where they are meant to.
const PRODUCT_NAME: &str = "Windrecorder";
const COMPANY_NAME: &str = "Windrecorder";
const LEGAL_COPYRIGHT: &str = "GPL-2.0";
/// English (US) / Unicode: the language-and-codepage pair Explorer reads the string block under.
const LANG_CP: &str = "040904b0";
const LANG_ID: u32 = 0x0409;
const CP_ID: u32 = 0x04b0;

/// `FileDescription` per binary -- the short human phrase shown next to the name in Explorer.
///
/// These are not invented here. Each one is the `Role` of the same binary's row in `release.ps1`'s
/// payload table (which reuses `build.ps1`'s artefact note, trimmed), so the release notes on the
/// zip, this resource, and `--version` describe a binary with the same words.
const DESCRIPTIONS: &[(&str, &str)] = &[
    ("windrec", "the recorder main.py supervises, in place of record_screen.py"),
    ("windui", "the native egui front end"),
    ("windmaint", "idle maintenance as a command: convert / refresh / expire / backup / doctor"),
    ("windcapctl", "terminal search and capture benchmarks; a human tool, nothing at runtime needs it"),
    ("windsvc", "the tray and supervisor"),
    ("windmcp", "the HTTP MCP bridge"),
    ("wind-reindex", "indexes already-recorded video"),
    ("windnotes", "the user's own bookmarks: flag/note store, capture, marker geometry"),
    ("windsetup", "first-run layout, engine probing, and the re-entrant migration over existing months"),
    ("windai", "natural-language search and monthly activity tags"),
    ("Windrecorder", "the file you double-click: it starts bin\\windsvc.exe and nothing else"),
];

/// Generate, compile and link the version resource for one binary.
///
/// Called from a crate's `build.rs` with cargo's `[[bin]] name`, which is also the word `--version`
/// prints -- `windmaint` rather than the `wind-maint` package. Keeping both spellings downstream of
/// one argument is what stops the Properties tab and the command line naming different things.
fn version_resource(binary: &str) {
    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo always sets CARGO_PKG_VERSION");
    // `PROFILE` is cargo's own word for the profile being built, and it is the same word
    // `native_runtime.describe_build()` derives from the directory an .exe was found in. The
    // resource's `VS_FF_DEBUG` bit and the `(debug)` in the version line therefore flip together.
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let description = DESCRIPTIONS
        .iter()
        .find(|(name, _)| *name == binary)
        .unwrap_or_else(|| panic!("version_resource: {binary} has no FileDescription; add it to base/version_resource.rs"))
        .1;
    let out_dir = std::env::var("OUT_DIR").expect("cargo always sets OUT_DIR");
    let source = std::path::Path::new(&out_dir).join(format!("{binary}.rc"));
    let icons = icon_resources(std::path::Path::new(&out_dir));
    let text = resource_file(binary, &version, description, &profile, &icons);
    std::fs::write(&source, text).unwrap_or_else(|error| panic!("version_resource: cannot write {}: {error}", source.display()));
    embed_resource::compile_for(&source, &[binary], embed_resource::NONE);
}

/// The icon resources this crate asked for, as `.rc` lines already ready to be pasted in.
///
/// A *directory* convention rather than a function argument, for two reasons. The first is the one
/// this whole file is written against: an extra entry point in an `include!`d fragment is dead code
/// in every build script that does not call it, so nine of the eleven crates would grow a warning. The
/// second is that it makes "does this binary carry an icon" answerable by looking at the crate -- a
/// crate with an `icons\` directory carries one, and the nine without one carry nothing, no flag to
/// forget to pass and no `#[allow(dead_code)]` to apologise for it.
///
/// Each `icons\<id>-<name>.ico` becomes one icon resource under the ordinal `<id>` -- so
/// `icons\1-tray-recording.ico` is loaded at run time with `MAKEINTRESOURCE(1)`.
///
/// Ordinals rather than the file's name because the resource compiler on this machine writes a
/// *string* name into the image with its quotation marks still attached: `icons\tray-recording.ico`
/// declared as `"tray_recording" ICON "..."` came out of `rc.exe` as a group named
/// `"TRAY_RECORDING"`, thirteen characters including the quotes, which no run-time lookup can find.
/// Measured here, and the reason the first cut of this feature loaded nothing. An ordinal cannot be
/// mangled that way, and it costs the same one-line declaration.
///
/// Sorting by name keeps the generated `.rc` byte-stable across machines with different directory
/// orderings, and makes the leading number the only thing that decides which id an icon gets.
///
/// Deliberately no `cargo:rerun-if-changed`: emitting any such line *replaces* cargo's default of
/// "rerun when anything in the package changed", and the version fields above are derived from
/// `CARGO_PKG_VERSION`, which no path dependency would cover. Narrowing the trigger to save a
/// fraction of a second would let a bumped version ship in a `.exe` still reporting the old one.
fn icon_resources(out_dir: &std::path::Path) -> Vec<String> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo always sets CARGO_MANIFEST_DIR");
    let dir = std::path::Path::new(&manifest).join("icons");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // No directory is the normal case, and it is not a warning: nine of the eleven binaries have
        // no icon to carry and no reason to be told about one.
        Err(_) => return Vec::new(),
    };
    let mut found: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("ico"))
        .collect();
    found.sort();
    let mut lines = Vec::new();
    let mut used: Vec<u32> = Vec::new();
    for path in found {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_else(|| panic!("icon_resources: {} has no usable file name", path.display()));
        let (id, label) = stem.split_once('-').unwrap_or_else(|| {
            panic!(
                "icon_resources: {} is not named <id>-<label>.ico -- the leading number is the resource \
                 id the binary loads it by",
                path.display()
            )
        });
        let id: u32 = id
            .parse()
            .unwrap_or_else(|_| panic!("icon_resources: {stem} starts with {id:?}, which is not a resource id"));
        if id == 0 || used.contains(&id) {
            panic!("icon_resources: resource id {id} is used twice (or is zero) in {}; ids must be unique and start at 1", dir.display());
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            panic!("icon_resources: {stem} has a label that is not letters, digits, `-` or `_`");
        }
        used.push(id);
        // `rc.exe` resolves a relative resource file against the directory of the script it is
        // compiling, and that script lives in `$OUT_DIR`, so the bytes have to be next to it. The
        // file is copied rather than referenced by absolute path so the generated `.rc` stays
        // readable and portable -- it names `1-tray-recording.ico`, not somebody's build directory.
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_else(|| panic!("icon_resources: {} has no usable file name", path.display()));
        std::fs::copy(&path, out_dir.join(name))
            .unwrap_or_else(|error| panic!("icon_resources: cannot copy {} to {}: {error}", path.display(), name));
        lines.push(format!("{id} ICON \"{name}\"\n"));
    }
    lines
}

/// The `.rc` text.
///
/// Deliberately free of `#include`: the handful of constants a version resource needs are defined
/// numerically below, so nothing depends on `winres.h` / `winver.h` being on the include path of
/// whichever Windows SDK revision happens to be installed. These are those headers' own values,
/// and `rc.exe` parses `VS_VERSION_INFO VERSIONINFO` without help from anyone.
///
/// `icons` are the pre-rendered lines from [`icon_resources`], emitted before the version block: the
/// first icon group in a `.exe` is the one Explorer shows for the file, and putting it first keeps
/// that from depending on how `rc.exe` happens to order two independent statement types.
fn resource_file(binary: &str, version: &str, description: &str, profile: &str, icons: &[String]) -> String {
    let numeric = version_numbers(version);
    // The one place the debug build says so without being asked: Explorer's Properties -> Details
    // shows the flags, and `--version` shows the same word.
    let flags = if profile == "debug" { "VS_FF_DEBUG" } else { "0x0L" };
    let original = format!("{binary}.exe");
    let comments = format!("profile: {profile}");

    let mut text = String::new();
    text.push_str("// generated by the build script from base/version_resource.rs -- do not edit, do not commit\n");
    text.push_str("#define VS_VERSION_INFO      1\n");
    text.push_str("#define VS_FFI_FILEFLAGSMASK 0x3fL\n");
    text.push_str("#define VS_FF_DEBUG          0x1L\n");
    text.push_str("#define VOS_NT_WINDOWS32     0x40004L\n");
    text.push_str("#define VFT_APP              0x1L\n");
    text.push_str("#define VFT2_UNKNOWN         0x0L\n");
    text.push('\n');
    for line in icons {
        text.push_str(line);
    }
    if !icons.is_empty() {
        text.push('\n');
    }
    text.push_str(&format!(
        "VS_VERSION_INFO VERSIONINFO\n\
         \x20FILEVERSION {numeric}\n\
         \x20PRODUCTVERSION {numeric}\n\
         \x20FILEFLAGSMASK VS_FFI_FILEFLAGSMASK\n\
         \x20FILEFLAGS {flags}\n\
         \x20FILEOS VOS_NT_WINDOWS32\n\
         \x20FILETYPE VFT_APP\n\
         \x20FILESUBTYPE VFT2_UNKNOWN\n\
         BEGIN\n\
         \x20   BLOCK \"StringFileInfo\"\n\
         \x20   BEGIN\n\
         \x20      BLOCK \"{lang_cp}\"\n\
         \x20      BEGIN\n",
        lang_cp = LANG_CP
    ));
    for (key, value) in [
        ("CompanyName", COMPANY_NAME),
        ("FileDescription", description),
        ("FileVersion", version),
        ("InternalName", binary),
        ("LegalCopyright", LEGAL_COPYRIGHT),
        ("OriginalFilename", original.as_str()),
        ("ProductName", PRODUCT_NAME),
        ("ProductVersion", version),
        ("Comments", comments.as_str()),
    ] {
        text.push_str(&format!("            VALUE \"{key}\", \"{value}\"\n", value = escaped(value)));
    }
    text.push_str(&format!(
        "        END\n\
         \x20   END\n\
         \x20   BLOCK \"VarFileInfo\"\n\
         \x20   BEGIN\n\
         \x20      VALUE \"Translation\", {lang_id:#06x}, {cp_id:#05x}\n\
         \x20   END\n\
         END\n",
        lang_id = LANG_ID,
        cp_id = CP_ID
    ));
    text
}

/// `0.1.0` -> `0,1,0,0`, the four-part form `FILEVERSION` demands.
///
/// A part that is not a number (a pre-release suffix, a stray tag) becomes `0` rather than a
/// resource `rc.exe` refuses to compile: an unreadable version should cost us the numeric fields,
/// not the build. The human-readable `FileVersion` string still carries the real value verbatim,
/// and that is the one Explorer shows.
fn version_numbers(version: &str) -> String {
    let mut fields: Vec<String> = version
        .split(['.', '-'])
        .map(|part| part.parse::<u32>().unwrap_or(0).to_string())
        .take(4)
        .collect();
    fields.resize(4, "0".to_string());
    fields.join(",")
}

/// A `.rc` string literal's payload: `\` and `"` are the two characters the grammar reserves.
fn escaped(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}
