//! Shared plumbing for every native `windcap` binary.
//!
//! Deliberately free of any Win32 dependency except the two clock/ANSI calls, which are inline
//! `extern "system"` declarations: the whole workspace must stay buildable on a machine with no
//! `windows` crate in the cargo cache, and `windcap-core` already owns the heavier Win32 surface.

pub mod ansi;
pub mod autostart;
pub mod clock;
pub mod config;
pub mod csv;
pub mod fslock;
pub mod i18n;
pub mod image;
pub mod install;
pub mod maintain;
pub mod ocr;
pub mod paths;
pub mod pool;
pub mod prompts;
pub mod range;
pub mod version;
pub mod wxocr;

pub use ansi::decode_console_bytes;
pub use clock::LocalParts;
pub use config::Config;
pub use fslock::{LockFile, PidLock};
pub use install::{defaults_source, is_install_root, resolve_root, resolve_root_from_exe};
