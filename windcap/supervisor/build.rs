// The Windows resources, so Explorer's Properties tab, the taskbar, and `windsvc --version` all say
// the same thing about this .exe. The field list and the wording live in one place for all eleven
// binaries; this file only names which of the eleven it is. See that file for why it is included
// rather than linked, and for the offline-resolution check that let `embed-resource` be used at all.
//
// `windsvc` and `Windrecorder` are the two of the eleven that also have an `icons\` directory, and that
// directory is the whole declaration: the shared fragment compiles every `icons\*.ico` into the binary
// as a named icon resource, which is what lets the tray get its two states out of its own .exe instead
// of reaching for `<install>\__assets__\icon-tray.png`. A standalone install has no such directory, and
// a tray that requires one cannot start -- measured on exactly that root, `windsvc run` printed
// "icon-tray.png could not be decoded as an image (GDI+ status 2) / The tray cannot appear without
// its icon." and exited 1. The product's main entry point was disabled by a missing picture.
//
// The two .ico files are containers around the shipped `__assets__` tray PNGs, byte for byte: an ICO
// directory entry may carry a whole PNG from Vista onwards, and `LoadImageW` decodes it, so no
// re-encoding and no second source of truth for the art. See `src\icon.rs` for the provenance check
// that keeps them in step with `__assets__` when that directory is present.
include!("../base/version_resource.rs");

fn main() {
    version_resource("windsvc");
}
