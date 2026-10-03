//! `windsvc` — the native replacement for `main.py`: the process behind the notification-area icon.
//!
//! It is a supervisor and nothing else. It takes the tray lock so a second copy cannot start, puts an
//! icon in the shell, and turns the menu into commands against three other executables — `windrec
//! loop` for recording, `winduiweb` for the interface, `windmcp serve` for the MCP bridge whenever
//! `enable_mcp_server` is on — while reading the pid-carrying lock files they all agree on. Before any
//! of that, on a tree that has never been laid out, it runs `windsetup init` and so is the installer
//! of a fresh download: the double-click is the whole of what a person has to know. It does
//! not open the index, run OCR, or touch the screen; every one of those is another binary's job, and
//! a tray that started doing them would be a second implementation of a product that already has
//! one. It ships no updater either: the only thing the menu says about releases is the version this
//! binary carries and an invitation to read the changelog sitting in the install.
//!
//! ## Why there is no `hide_cli_window()`
//!
//! `main.py` spawns a thread that spins for up to twenty minutes hunting for a console window titled
//! "Windrecorder" so it can hide it, triggered by a `hide_CLI_by_python.txt` file the installer leaves
//! behind. None of that is ported, and it is not a gap: the mechanism exists only because Python is a
//! console program that must become a GUI one after the fact. A binary linked with the GUI subsystem
//! has no console to hide, so both halves — the trigger file and the window hunt — become unnecessary
//! rather than being translated. The same reasoning retired `webbrowser.open(path)` in favour of one
//! `ShellExecuteW`.
//!
//! ## Shape of the crate
//!
//! `options` parses argv, `native` finds binaries, `layout` derives paths, the copy is read through
//! the shared `wind_base::i18n`, `menu` turns state into labels, `child` spawns and stops processes,
//! `supervisor` is the state
//! machine, `tray` is the Win32 message loop, `icon` turns the PNG pair into `HICON`s, `ffi` is every
//! Win32 declaration in one place, `update` carries the binary's own version and the fact that there
//! is no remote to ask, `doctor` is the report.
//! Only `supervisor` and `tray` touch the desktop, which is what makes the rest testable here.

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod child;
mod doctor;
mod ffi;
mod icon;
mod layout;
mod menu;
mod native;
mod options;
mod supervisor;
mod tray;
mod update;

use options::Invocation;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match options::classify(&argv) {
        // First arm, and it takes no `Options`: `--version` is the one thing this binary can say
        // about itself without an install root, a config file or a lock.
        //
        // Deliberately *not* wrapped in `attach_parent_console()`, unlike `doctor` and the two
        // usage arms. Attaching is what lets a multi-line report appear for a human at a terminal,
        // and its cost is that it replaces the inherited standard handles with the console's —
        // measured on this machine, `windsvc --version > build.txt` writes an empty file with the
        // attach and the real line without it, in `cmd`, PowerShell and MSYS alike. One short line
        // aimed at a support ticket is worth more arriving intact than arriving in a console that
        // may not be there, so this arm answers through the handle the caller already set up.
        Invocation::Version => {
            println!("{}", options::version_line());
        }
        Invocation::Doctor(options) => {
            attach_parent_console();
            if let Err(error) = doctor::report(&options) {
                eprintln!("doctor: {error}");
                std::process::exit(1);
            }
        }
        Invocation::Run(options) => {
            // No console is attached on purpose. `child::signal_break` attaches to the recorder's
            // console to send its stop event and `FreeConsole`s afterwards, which invalidates this
            // process's standard handles — a tray that printed afterwards would be writing to a
            // dangling handle. Everything the tray has to say goes through a balloon instead, and
            // everything it can be asked goes through `doctor`.
            if let Err(error) = tray::run(&options) {
                eprintln!("windsvc: {error}");
                std::process::exit(1);
            }
        }
        Invocation::Usage(unknown) => {
            attach_parent_console();
            eprintln!("{}", options::usage(unknown.command()));
            std::process::exit(2);
        }
        Invocation::Bad(message) => {
            attach_parent_console();
            eprintln!("{message}\n");
            eprintln!("{}", options::usage(None));
            std::process::exit(2);
        }
    }
}

/// Reconnect stdout to the console this process was started from.
///
/// A GUI-subsystem binary inherits its parent's standard handles but is never *attached* to the
/// console, so on some Windows versions what it writes lands in a buffer the shell has already moved
/// past. Attaching is what makes `windsvc doctor` readable in the terminal it was typed into. It
/// fails quietly when the process was started by Explorer or the shell's autorun, which is the normal
/// case for a tray and is not an error worth reporting.
fn attach_parent_console() {
    unsafe {
        ffi::AttachConsole(ffi::ATTACH_PARENT_PROCESS);
    }
}
