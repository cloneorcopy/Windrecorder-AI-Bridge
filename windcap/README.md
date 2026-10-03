# windcap — the native engine

`windcap/` is a Rust workspace that re-implements Windrecorder's hot paths: the capture loop, the
"did the screen change" gate, the session probes that decide whether to record at all, the monthly
SQLite index, the terminal front door to that index, and the idle maintenance pass.

This workspace **is** Windrecorder. The binaries built here used to be an optional overlay onto a
Python application, and that application was deleted in commit `3f37cbf`; `2b6f318` then removed the
tray's fallback to it, so there is no second implementation left behind any of these binaries.
Nothing here is optional *to run* — an install with no `windrec.exe` cannot record, and says which
file is missing rather than degrading quietly. What is still true is that each artefact is optional
to **build**: `build.ps1` compiles one crate at a time and never lets one broken crate cost you the
others, and that rule is the reason `build.ps1` exits `0` when there is no Rust toolchain at all (the
summary below then names every binary you do not have).

## Build it

```
powershell -ExecutionPolicy Bypass -File windcap\build.ps1 -Stage
```

That is the whole job: it builds `windcap.dll` and the twelve executables, one cargo invocation each,
prints one line per artefact (name, built or skipped, size, seconds), copies the executables into
`bin/`, and finishes with a **"what this install can do"** block that says PRESENT or ABSENT for
each artefact and what is lost if it is absent. rustc's own output goes to
`windcap\target\build.log`; the tail of it is printed only for a crate that failed.

| switch | what it does |
|---|---|
| `-Profile release\|debug` | cargo profile, and the directory the artefacts are taken from. Default `release`. |
| `-SkipUi` | do not build either front end — the retired `windui.exe` and the shipped `winduiweb.exe`, both of which are slow because `eframe` is by far the slowest crate in the tree. This is the switch to reach for on a laptop or when you only want the recorder; the payload then has no window in it, so it is a rehearsal switch, not a shipping one. |
| `-Stage` | copy the built executables into `<repo>\bin\`, the first directory the runtime searches. |
| `-Manifest PATH` | also write the per-artefact result (name, status, bytes, seconds, profile, total, source path, note) as tab-separated lines. Only `release.ps1` uses it: the console table is formatted for eyes, and a second script should not have to reverse a formatter to find out what happened. |

Any other parameter is an error, never a silent no-op. Exit status is `0` when everything asked for
was built (including when there is no cargo at all), `1` when something is missing, `2` on a bad
command line.

Without the script, the same work by hand is one command per artefact:

```
cargo build --release -p windcap-core     # -> target\release\windcap.dll
cargo build --release -p windrec          # -> target\release\windrec.exe
cargo build --release -p windcap-cli      # -> target\release\windcapctl.exe
cargo build --release -p wind-maint       # -> target\release\windmaint.exe
cargo build --release -p windui           # -> target\release\windui.exe
```

## Ship it

`build.ps1` puts binaries in a build tree. `release.ps1` produces the one file you can hand to
somebody else:

```
powershell -ExecutionPolicy Bypass -File windcap\release.ps1
```

It takes the version from `[workspace.package]` in `Cargo.toml` — parsed, never copied, so there is
no second number to forget. It used to cross-read that against `windrecorder/__init__.py`'s
`__version__` and print a mismatch rather than resolve one; the app that carried the second number is
gone, so the workspace version is the product's version and the zip only ever claims that. It calls
`build.ps1 -Profile release` rather than invoking cargo again, and writes
these under `windcap\dist\` (gitignored, like `target\`):

| path | what it is |
|---|---|
| `Windrecorder-native-<version>.zip` | the payload — 68 MiB compressed, out of a 94 MiB staged tree: 36 MiB of binaries and 58 MiB of shipped settings, most of that the synonym index |
| `Windrecorder-native-<version>.zip.sha256` | the same hash in `sha256sum -c` form, beside the zip |
| `Windrecorder-native-<version>\` | the staged tree, kept so you can look inside without unzipping |
| `build-manifest.tsv` | what `build.ps1 -Manifest` reported for this run |

Inside the zip the archive root **is** the install root — there is no wrapper folder to unzip past:

```
bin\windrec.exe   bin\winduiweb.exe   bin\windmaint.exe   bin\windcapctl.exe   bin\windcap.dll
ocr_lib\Windows.Media.Ocr.Cli.exe
RELEASE-NOTES.txt
```

An artefact is staged only when `build.ps1` reported it **`BUILT` for this run**. A file merely
sitting in `target\release\` is not enough: with no Rust toolchain on PATH, `build.ps1` exits `0`
having compiled nothing, and every binary from somebody's last build is still right there — a
presence test would stage them, stamp this run's date on them, and hand out a hash for bytes this
release never built. `winduiweb.exe`, `windmaint.exe` or `windcapctl.exe` failing to build costs you that
one component and a line in "not in this payload"; `windrec.exe` or `windcap.dll` missing means **no
zip at all** and exit `1`, so `dist/` is never left holding an archive that looks current and is not.

### It installs itself on the first double-click, and contains no Python

It is a standalone payload. It used to be an overlay that went *onto* an existing Windrecorder
install — the folder with `userdata\`, `.venv\` and `install_update.bat` in it — and every word of
that framing died with commit `3f37cbf`. Unzip it into an empty directory and that directory **is**
the install: `bin\`, `config_src\`, `ocr_lib\`, the `__assets__\` OCR fixtures and
`RELEASE-NOTES.txt`. Double-click `bin\Windrecorder.exe` — the one file in the folder named after the
product, and a launcher and nothing more: `launcher\src\main.rs` finds `bin\windsvc.exe`, starts it,
and hands over every argument it was given, so `Windrecorder.exe doctor` is the tray's `doctor`
byte for byte. The tray runs `windsetup init` itself — it
lays the 16 writable slots and seeds `userdata\config_user.json` from the `config_src\` that arrived
in the zip — before it takes the lock, starts the recorder or shows an icon. `init` is still a
command you can type, and `windsvc doctor` still says what the menu would do without clicking
anything; it is no longer a command you have to type. `config_src\` ships precisely so that no
pre-existing install is assumed; `base\src\install.rs` defines an install root as the directory
carrying it.

The launcher changes no process model, and is deliberately not the thing autostart registers:
`base\src\autostart.rs::target_exe()` names `bin\windsvc.exe`, because the sign-in entry has to name
the long-lived owner of the lock and the icon. It is listed nowhere in
`supervisor\src\native.rs`'s `BINARIES` for the same reason — `doctor` reports on the binaries that do
work, and this one does none.

There is no step that "turns the engine on", because there is no engine left to switch away from:
**`"use_native_core"` is read by nothing** (`2b6f318` removed the tray's last use of it, `fc288cb`
removed the settings-page control that wrote it), and an old `userdata\config_user.json` that still
carries the key behaves identically whether it is true, false or deleted. What an absent binary now
costs you is that job: `supervisor\src\native.rs` returns a `Missing` naming the `.exe` and every
directory it searched, and `windsvc doctor` prints the same verdict without clicking anything.
`RELEASE-NOTES.txt` says all of this, because the zip is the only documentation
that travels with the bytes.

Everything is in `bin\` because that is where both discovery lists look on a machine that has no Rust
tree. `supervisor\src\native.rs`'s `candidate_dirs()` reaches `<install>\bin\` second, behind only
`%WINDCAP_HOME%` and ahead of `<install>\` and `windcap\target\release\`; the DLL's shorter list
reaches `<install>\bin\windcap.dll` *first*. On a recipient's machine there is no `windcap\target\`
at all, so for the DLL `bin\` is not just the fastest hit, it is the only hit.

### Proving it: `smoke.ps1`

A zip whose every byte is correct can still not work, because what has to be right is the *layout*.
`smoke.ps1` is the only way to find out:

```
powershell -ExecutionPolicy Bypass -File windcap\smoke.ps1
```

It unzips the payload into `%TEMP%\windrec-smoke-<pid>`, gives the throwaway install root the one half
the payload is not allowed to bring (an empty `userdata\`, plus **a copy** of a real month
database — `-Source`, defaulting to a live install on this machine), and runs the shipped binaries at
that root. Nothing ever opens the original: `wind-store`'s reader materialises a `_TEMP_READ.db`
beside whatever it reads, so pointing it at a live install would write into it. The script asserts on
**output, not exit codes** — every one of these binaries exits 0 while reporting that it found
nothing, which is exactly the silent failure a layout mistake produces:

| check | what it rules out |
|---|---|
| zip hash matches the `.sha256` sidecar | bytes corrupted in transit (or a stale zip being tested) |
| `bin\` at the archive root | a payload that unzips one level too deep to be found |
| every `bin\` executable present — eleven now, since the launcher joined the staged set | a crate that failed to build, shipped quietly |
| the DLL candidate order picks `bin\windcap.dll` | the DLL sitting where nothing will find it |
| `bin\windcap.dll` loads and reports the ABI `core\src\ffi_c.rs` declares | a shipped DLL that disagrees with the source it was supposed to come from (the expected value is read out of `core\src\ffi_c.rs`, not copied) |
| `windrec status` names the month file and a non-zero row count | an index the recorder cannot find |
| `windcapctl query --from … --to …` reports *9 of 9 hits* | a window that silently searched the wrong directory |
| `windcapctl stats`, `windmaint doctor` | the whole library disagreeing with one file |
| `windmaint convert --dry-run` says nothing was written | a "dry run" that is not |
| `windsvc.exe` started with no arguments lays out `userdata\`, seeds `config_user.json` and takes its lock | a payload that installs only for whoever read the instructions — and a tray that died before the message loop, which has no console to say so in |
| `Windrecorder.exe` — the file the READMEs name — started with no arguments does the same through the launcher, and nothing it started is left running afterwards | a documented entry point that was never clicked by the thing that gates the release |
| `Windrecorder.exe doctor` exits with `windsvc.exe doctor`'s code and prints its report byte for byte | a launcher that rewrote an argument, truncated the report, or turned a failure into exit 0 |
| a launcher with no `windsvc.exe` in the install exits 1 naming the file and every directory searched | the one failure a double-click cannot show, because a GUI-subsystem binary has no console to complain in |
| `winduiweb.exe` started with no arguments comes up with a window, and started with `--tab settings --exit-after` it opens with no dev server running and then closes itself | a build that links, exits 0 and never draws, or a Tauri binary that embedded no assets — the retired egui `windui.exe` is no longer gated because it is no longer in the payload |
| no `cache\`, `cache_screenshot\`, `userdata\videos`, `userdata\config_user.json` afterwards | a read-only command creating directories, or mutating the install it is inspecting |
| the source database's mtime, size and sibling count unchanged | anything at all writing to the real install |

Exit is `1` on any `FAIL`. A check that cannot be made at all — no `.sha256` sidecar for a zip handed
to `-Zip` from elsewhere — prints `SKIP`, is not counted as a pass, and stops the run from claiming
"it works". `-Keep` leaves the temp tree to inspect, and the next run deletes any
`windrec-smoke-*` its predecessors left behind. Every deletion goes through one function that refuses
any path not resolved under `%TEMP%`, including `%TEMP%` itself.
`-Zip <path>` is how you aim it at something else — including a zip you have doctored on purpose,
which is the only way to learn whether the gate works or merely looks like one. `-ExpectRows` and
`-Source` move the whole thing onto a different sample database.

### What the shipped story is still missing

Honest limits of `release.ps1` as it stands:

* **No installer registers it.** No registry entry, no uninstaller, no "keep my `userdata`" logic —
  unzipping *is* the install step, the first double-click lays the tree out, and overwriting is the
  only upgrade path. A Start Menu shortcut is still not created: the shell-side half of
  `upgrade_migration_routine.py`'s shortcut handling was ported only on the *delete* side
  (`setup/src/migrate.rs:353-378`) and nothing replaced it.
* **The zip is not byte-reproducible.** `Compress-Archive` stores each file's own mtime, so two runs
  over identical inputs hash differently (measured: 4 bytes and a different SHA-256 apart). The
  SHA-256 identifies *this build event*, not a content digest; the per-file hashes in
  `RELEASE-NOTES.txt` are the ones that survive a rebuild.
* **`vcruntime140.dll` is assumed present.** Every artefact here imports it. SQLite is embedded and
  there is no `sqlite3.dll` to ship, but a machine with no VC runtime will fail to start them, and
  `smoke.ps1` can only prove that on the machine it runs on.
* **`ocr_lib\Windows.Media.Ocr.Cli.exe` is copied, not built.** It comes from the install tree, so it
  carries the payload's version number without being governed by it.
* Nothing here is signed, and Windows will say so.

## The crate map

| directory | cargo package | artefact | what it owns |
|---|---|---|---|
| `core/` | `windcap-core` | `windcap.dll` (+ rlib) | Raw Win32 bindings, session state (locked screen, idle seconds, sleep drift), the GDI desktop grab, the change gate. Also built as a `cdylib`: that is `windcap.dll`, whose exported C ABI is declared in `core/src/ffi_c.rs`. |
| `base/` | `wind-base` | rlib | Plumbing shared by every binary: the app's own wall clock, config overlay, on-disk naming, lock files, CSV side-channels, JPEG encode. |
| `store/` | `wind-store` | rlib | The monthly SQLite index as a contract: schema-compatible writer, read-only reader that never blocks the recorder, the search the product is named after, the maintenance statements. |
| `cli/` | `windcap-cli` | `windcapctl.exe` | Terminal front door. Probes (`status`, `bench`, `grab`, `snap`) measure capture; readers (`query`, `day`, `stats`, `inspect`, `bench-search`) walk the index; `index` is the one command that writes. Everything prints its own timings. |
| `windrec/` | `windrec` | `windrec.exe` | The recorder as one self-contained executable: config, change gate, OCR (`ocr_lib/`), video slices, and the index write. `loop` is the long-lived daemon `windsvc` — the tray — supervises; `run` is one pass; `doctor` explains the setup. |
| `maint/` | `wind-maint` | `windmaint.exe` | The deferred pass as a command instead of an unobservable thread: `text`, `convert`, `refresh`, `expire`, `reindex`, `previews`, `ai-tags`, `ai-summaries`, `backup`, `doctor` — and `forget`, which blanks the text, titles and previews already indexed for a period you name (and the AI summaries derived from it, which nothing can regenerate) and is deliberately in no `all` pass. `text` is first in `all` because it reads the slice directories the later steps recycle: it fills in the words the recorder left out when a maintenance window is set. `backup` copies the month files *and* the two summary directories, because a paragraph an outside AI wrote is the other thing that cannot be rebuilt. `--dry-run` really is a no-op, `--manual` says a person asked for this pass so the closing of the window does not stop it, and a stop request written by the settings page is honoured between work items. |
| `summary/` | `wind-summary` | rlib | The two AI artefact families as one owner: `userdata\result_ai_period_summary\{day}.json` and `userdata\result_ai_daily_summary\{day}.json`, one file per product day, merged under a two-level lock. It answers "is this day done" for both producers — the MCP bridge and `windai summarize` — from the same code: the segment list, the content and prompt fingerprints, and the queue of what is still missing. No rusqlite dependency, so nothing in here can write an index row. |
| `windui/` | `windui` | `windui.exe` (built, **no longer shipped**) | Native egui/eframe front end: Search, OneDay, and the settings those screens read. Reads through `wind-store` and never touches SQLite directly. The binary was retired from the payload by `docs/adr/2026-09-27-winduiweb-is-the-only-interface.md`; the crate is not, because its library target `wind_ui` is the only implementation of the frame door, the prompt panel and the form field declarations, and `winduiweb/src-tauri` links it. |
| `launcher/` | `wind-launcher` | `Windrecorder.exe` | The file a stranger is told to click, named after the product rather than after a job. Finds `bin\windsvc.exe` by the same candidate order `maint/` uses for its scheduled children, starts it, and hands argv over untouched; answers only `--version` itself. No lock, no menu, no layout, no knowledge of recording. |

`windrec`, `windcapctl` and `windmaint` embed SQLite (`rusqlite` with the `bundled` feature), so the
shipped `.exe` files need no `sqlite3.dll`.

### Why the dependency tree is so small

`core/src/ffi.rs` opens with the reason:

> Deliberately no `windows`/`winapi` crate: the whole point of this core is a dependency tree a
> stranger can build offline.

So `windcap-core` has **zero third-party dependencies** — every Win32 call is a raw
`extern "system"` declaration and the structs it needs. `wind-base` adds three small pure-Rust
crates. That is what makes `cargo build --offline` work on a machine with nothing but a warm cargo
cache, and it is the difference between "a rewrite nobody can compile" and a rewrite a stranger can
audit. Do not add a Win32 crate to `core`.

`core/src/capture.rs` records the matching runtime decision — GDI `StretchBlt` into a small DIB
section instead of `mss`'s full-union blit, and GDI rather than DXGI, because at one frame every few
seconds the GPU path buys nothing and goes stale on every desktop switch.

## How the tray finds what to launch

There is no switch. There used to be two independent mechanisms — a Python supervisor that could
launch a native program instead of its own, and a Python bridge that could load a DLL instead of its
own probes — and both of the Python halves are gone, so one mechanism is left and it has no
alternative to fall back to:

* **The tray** (`windsvc.exe`) resolves every job to a binary through
  `supervisor\src\native.rs`, and supervises the ones it starts: `windrec loop --root <root>` to
  record, `winduiweb --root <root>` for the window, `windmcp serve --root <root>` for the bridge.
  Recording starts because the tray was run and `start_recording_on_startup` was not turned off —
  not because a config key opted in. The tray is also the only thing a double-click reaches:
  `Windrecorder.exe` starts it and steps aside, so there is still exactly one process that knows what
  to launch.
* **The library** (`windcap.dll`) is the same session-state code built as a `cdylib`, with the C ABI
  declared in `core\src\ffi_c.rs`. Every entry point is `catch_unwind`ed at that boundary and the ABI
  version is checked on load, so a mismatched DLL is refused rather than allowed to kill its caller.
  Its first loader was a Python bridge; that bridge was deleted, and what the DLL is loaded by today
  is any caller that speaks the ABI — `smoke.ps1` is the in-tree one, and it is why the file still
  ships and still gates the payload.

The contract that survives is the launch one: a long-lived foreground process that ends its segment
on `CTRL_BREAK_EVENT` and exits. Nothing else on this branch can honour it.

### Where the runtime looks

`supervisor\src\native.rs`'s `candidate_dirs()` — first hit wins, and this is why `build.ps1 -Stage`
copies into `bin/`:

| order | directory | what lives there |
|---|---|---|
| 1 | `%WINDCAP_HOME%` | an explicit install of the binaries, anywhere |
| 2 | `<root>\bin\` | **what `-Stage` writes** — the release layout a source checkout does not have |
| 3 | `<root>\` | a loose drop-in |
| 4 | `<root>\windcap\target\release\` | the cargo tree, so a development checkout works **without staging** |
| 5 | `<root>\windcap\target\debug\` | last, and labelled as such |

Release always beats debug. A debug build is roughly an order of magnitude slower, and running one
silently in a user's background would make a performance claim out of a build artefact that cannot
support it — so it is honoured only when nothing better exists, and `describe_build()` reports the
word `debug` wherever it is found. `bin/` scores as `installed`.

`windcap.dll` has its own, shorter list, mirrored by `build.ps1` and proved by `smoke.ps1`:
`%WINDCAP_DLL%`, then `<install>\bin\windcap.dll`, then
`<install>\windcap\target\release\windcap.dll`, `<install>\windcap\windcap.dll`,
`<install>\windcap.dll`. Two consequences worth knowing. `bin/` is first among the real paths, so a
dropped-in release payload outranks whatever a developer's `cargo build` left in the tree — but
`build.ps1 -Stage` does not write the DLL there, it stages executables; `release.ps1` is what puts
`windcap.dll` in `bin\`. And `target\debug\windcap.dll` is **not** a candidate at all:
`-Profile debug` builds a DLL nothing in this layout will pick up from the cargo tree. Use `release`
for the DLL.

### What is lost when an artefact is absent

| absent | what the runtime does |
|---|---|
| `windrec.exe` | Recording does not start. `recorder_argv()` returns `Missing`, the tray balloons "Cannot start recording", and `windsvc doctor` prints `windrec.exe is not installed, so recording cannot start` with the directories it searched. There is no recorder left behind it. |
| `windcap.dll` | Nothing loads the session probes' C ABI. `release.ps1` treats this as NOT SHIPPABLE rather than staging a payload that cannot answer "is the screen locked?". |
| `winduiweb.exe` | The tray cannot open the search and settings window, and reports `winduiweb.exe` as the missing file. It is the only interface the payload carries, so there is nothing to fall back to — the egui `windui.exe` was retired from the shipped set on 2026-09-27. |
| `windmaint.exe` | The recorder reaches its idle-maintenance step, `maintenance_binary()` finds nothing, and the pass is skipped — silently, by design, because `windmaint` is the one that owns the maintain lock. |
| `windcapctl.exe` | Nothing at runtime: it is a human tool. You lose the terminal search and the capture benchmarks. |
| `windsvc.exe` | Nothing starts the recorder for you. `bin\windrec.exe loop --root <install>` by hand still records; there is no menu, no lock, no graceful stop. `bin\Windrecorder.exe` reports it missing by name — in the terminal for a command, in a dialog box for a double-click, which is the only channel a GUI-subsystem process has. |
| `Windrecorder.exe` | Nothing functionally: `bin\windsvc.exe` starts the same way when it is named. What is lost is the door — eleven executables left in `bin\`, not one of them called Windrecorder, and a stranger who has to read a README to learn which name to click. |

Ask the install what it has, and what it would run:

```
bin\Windrecorder.exe doctor --root <install>
```

That is the tray's report — the launcher forwards the word — and `bin\windsvc.exe doctor` is the same
command with one fewer step removed.

## Staging, and the one name that does not match

`-Stage` copies `windrec.exe`, `windcapctl.exe`, `windmaint.exe`, `winduiweb.exe` and the rest of the
shipped artefacts into `<root>\bin\`. The retired egui `windui.exe` is built but not staged — see the
`StageName` note in `build.ps1`.
The source is whichever file cargo just wrote under `target\<profile>\`, so a staged binary is always
the same file the cargo-tree fallback would have found — staging buys discoverability (order 2, ahead
of the cargo tree and of `%WINDCAP_HOME%` not being set), plus a directory you can ship.

One naming seam to know about: the maintenance crate's cargo package is `wind-maint`, and
`supervisor\src\native.rs`'s `BINARIES` asks for **`windmaint`**. `maint/Cargo.toml` closes that seam with an
explicit `[[bin]] name = "windmaint"`, but a `wind-maint.exe` left in `target\` by an older build
does not go away on its own — one is sitting in `target\debug\` right now — and the runtime would
never look for that name. `build.ps1` therefore prefers `windmaint.exe` when both exist and always
stages under that single name. If you build `wind-maint` by hand outside the script, check which
file cargo wrote.

## Concurrency and failure

The crates are being rewritten in parallel, so "some of it does not compile today" is a normal state
of this tree, not a broken build. That is the design: `build.ps1` runs one `cargo build -p` per
artefact and never aborts on a failure, so a compile error in `windui` cannot cost you a working
`windrec`. Read the summary line — it names exactly what is missing, and say so plainly: since the
Python application was deleted there is no other implementation to fall back to, so every artefact
named ABSENT is a job this install genuinely cannot do.
