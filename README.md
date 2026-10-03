<p align="center">
  <img src="__assets__/product-header-en.jpg" alt="Windrecorder" width="700">
</p>

<h1 align="center">Windrecorder — personal memory search engine</h1>

<p align="center">
  A Windows tool that records your screen at a small size, reads the text on it, and lets you rewind and search what you have seen.
</p>

<p align="center">English | <a href="README-zh.md">简体中文</a></p>

---

Windrecorder captures your screen continuously, indexes the OCR text of the frames that actually changed together with the foreground window title, and gives you a search window, a daily rewind view and activity statistics over that history.

Recording, indexing and searching all run on your machine against files in one folder. There is no account, no network service and nothing uploaded. Two features can reach outside, and both are switched off until you switch them on:

- **AI** sends the text you select to the endpoint you type on the AI page.
- **MCP** serves this machine's history to AI assistants over a port you configure.

This repository is a Windows fork of [yuka-friends/Windrecorder](https://github.com/yuka-friends/Windrecorder). The hot paths — capture, change detection, the monthly index, search, the settings window and the background maintenance pass — are Rust binaries built from the [`windcap/`](windcap/README.md) workspace in this tree. The product contains no Python runtime, needs no virtual environment, and has no administrator requirement.

![Windrecorder window](__assets__/product-preview-en.jpg)

## What it does

- Records one screen, several screens, or only the foreground window, at low bitrate and low resource use, so recording can run all day and be rewound live.
- Indexes only frames whose content changed, storing their recognised text and window title in a monthly SQLite file. Skips are configurable by window title, process name, text on screen, or how long the picture has been still.
- Defers everything that can be done later. Index maintenance, turning screenshots into video, applying retention limits and redrawing previews wait for a work window you set (for example `03:30` to `05:00`). Outside that window the background only captures.
- Lets you decide what never becomes searchable, and undo what already is. The settings page masks the edges of each screen: the black band is painted on the copy the recogniser reads, while the video keeps every pixel. `windmaint forget` blanks text, titles and previews already stored for a period you name.
- Provides data summaries: activity statistics, word clouds, timelines, light boxes and scatter plots.
- Rewrites screen history into prose — a paragraph per recorded stretch and one per day — either through your own AI endpoint or through a connected assistant. Both write the same files, in the same folders, as plain JSON you can read.
- Ships three interface languages: English, Simplified Chinese and Japanese.
- Drives any OCR engine that reads an image path and prints text; Windows' built-in recognition, Tesseract and WeChat OCR are all selectable.

## What it does not do

- Image embedding and semantic search are upstream extensions, not part of this engine. The Rust side creates and preserves the index directory for them and reads nothing from it.
- Upstream's second recording mode, handing the screen straight to ffmpeg, is not implemented here either. This engine has one capture mode — the screenshot array — and ffmpeg is used afterwards to fold those frames into the video you watch (`record_mode` stays `screenshot_array`).
- The per-row browser URL (`record_deep_linking`) is a field upstream filled from a UIAutomation reader. The column exists in every month file and stays empty here.
- A machine that has never had ffmpeg produces no video at all. The recorder writes screenshots, and the `.mp4` you watch is encoded from them afterwards; nothing fails, so nothing tells you. See *Install* below.

---

# Part one — using Windrecorder

## Install

Requirements: Windows 10 or 11 on x64, and disk space for the recordings. As a rough guide, video runs 10–20 GB per month depending on screen time and monitor count, and the SQLite index about 160 MB per month.

### Option A — take the release package (no compiler needed)

1. Download `Windrecorder-native-<version>.zip` and its `.sha256` sidecar from [Releases](https://github.com/cloneorcopy/Windrecorder-AI-Bridge/releases).
2. Check the bytes you downloaded:

   ```powershell
   Get-FileHash .\Windrecorder-native-0.1.0.zip -Algorithm SHA256
   ```

   The hash must match the one in the sidecar file.
3. Unzip it into an empty folder on a drive with room. Unzipping *is* the installation: the folder now holds `bin\`, `config_src\`, `ocr_lib\` and `__assets__\`. No path is baked into the binaries, so the folder works wherever you put it.
4. Double-click `bin\Windrecorder.exe`.

The first start creates `userdata\`, seeds `userdata\config_user.json` from the shipped defaults, and puts an icon in the system tray. Nothing registers itself in the registry or the Start menu.

### Option B — build it from source

1. Install [Git](https://git-scm.com/download/win) and [rustup](https://rustup.rs).
2. Clone into the folder where you want to work:

   ```powershell
   git clone https://github.com/cloneorcopy/Windrecorder-AI-Bridge.git
   cd Windrecorder-AI-Bridge
   ```

3. Build and stage:

   ```powershell
   powershell -ExecutionPolicy Bypass -File windcap\build.ps1 -Stage
   ```

   This compiles every crate, copies the executables into `bin\`, and ends with a table that says PRESENT or ABSENT for each binary and what you lose if it is absent. If `windsvc.exe` (the tray) and `windrec.exe` (the recorder) are PRESENT, the install works. Double-click `bin\Windrecorder.exe` to start.

### ffmpeg

The video step needs ffmpeg and the package does not carry one. Without it the screenshots pile up in `cache_screenshot\` and no video is ever finished, and no error is raised because nothing fails. Install ffmpeg yourself — [BtbN's `ffmpeg-master-latest-win64-gpl-shared.zip`](https://github.com/BtbN/FFmpeg-Builds/releases) is the usual choice: take its `bin\` contents out to a directory on your `PATH`. If you would rather hand the next machine a copy that needs no PATH edit, `windcap\extras.ps1` builds a `Windrecorder-ffmpeg-<version>.zip` from the ffmpeg already on this one, and you unzip that into the app folder. Then confirm:

```powershell
bin\windsetup.exe doctor --root .
bin\windmaint.exe convert --root .
```

The `doctor` report prints a `VIDEO STEP` line naming the ffmpeg it resolved and where it came from; `windmaint convert` catches up on the frames already on disk.

## First start

- **`bin\Windrecorder.exe` is the only file you need to know.** It is a launcher and nothing more: it starts `bin\windsvc.exe`, the tray process that owns the icon, the record lock and the recorder, and hands over every argument it is given. So `Windrecorder.exe doctor` is `windsvc.exe doctor` and prints the same report.
- The tray menu (or a double-click on the icon) opens the window, starts and stops recording, and quits. Closing the window keeps the app running in the tray; Settings → *Close button keeps the app in the tray* changes that.
- Recording starts on its own when the tray launches, because `start_recording_on_startup` ships on. Leave it on and you never press the button.
- The rest of `bin\` you do not start by hand: the tray starts them when they are needed. `windrec` records, `winduiweb` is the window, `windmcp` answers AI clients, `windmaint` tidies, `wind-reindex` indexes footage recorded before this install existed, `windsetup` lays out and migrates the tree, and `windnotes`, `windai` and `windcapctl` are terminal commands.

## The window

`winduiweb.exe` is the interface. Six tabs, and all of them read the same index and write the same settings file as every other binary:

| Tab | What you do there |
|---|---|
| **Search** | Find frames by keyword and date range, and open the recorded frame at full resolution. |
| **OneDay** | Rewind one day: the frames that were kept, in the order they happened. |
| **Stat** | Activity statistics, word cloud, timeline, light box and scatter plots over a period. |
| **Recording** | What gets captured, how often, which screens, what to skip, and how long the footage is kept. |
| **Settings** | Interface language, Local OCR Engine, preview width, the edges to ignore, start on sign-in, and the work window. |
| **AI** | The endpoint, the seven prompts, the MCP switch, and the switches that decide what gets sent out. |

The small picture beside each result is a preview, stored at the width the card is drawn at (Settings → *Preview width*, 512 px by default). Click any of them — a result card, a cell in the day's strip, a tile in the light box — and the window reads that frame at the resolution it was recorded, from the screenshot cache while it survives and out of the video afterwards. A row with neither left says so instead of enlarging the preview.

## Keeping things out, and taking them back out

Two separate jobs, and the product keeps them apart.

**Masking** decides what the recogniser ever sees. Settings → *OCR area to ignore on the display edges*, entered as a percentage per edge. The black band is painted only on the copy handed to OCR, so the video keeps every pixel; `windrec doctor` prints what the mask actually covers, so you can check that the taskbar you care about is inside it.

**Forgetting** removes what is already indexed:

```powershell
bin\windmaint.exe forget --day 2026-09-22 --keyword revenue --root .
```

That blanks the text, window titles and previews stored for the period you name. The AI summaries written from those rows go with them, and a day left with nothing standing loses its daily summary too — an outside model's paragraph is the one artefact nothing here can regenerate. `forget` is deliberately in no scheduled pass; you run it, or you do not.

Deleting footage itself is `windmaint expire`, which `forget` never touches.

## Background work happens in a window you name

Settings has two boxes, *Organise the backlog from (HH:MM)* and *Organise the backlog until (HH:MM)*. Fill them (say `03:30` and `05:00`, one appointment if they cross midnight) and everything recomputable waits for that window instead of running while you work: reading text out of each kept frame, drawing previews, folding away repeats, merging screenshots into video, applying retention, tagging, summarising and backing up.

The frame the OCR engine would have read is still produced at capture time, as the masked copy `<stamp>_cropped.jpg` beside the frame, because the mask can only be applied to pixels that are live. The pass reads that copy and nothing else; a row whose masked copy is missing is reported and left for the next pass rather than filled from the unmasked original.

Leave both boxes empty and the older rule stands: the pass waits for `idle_maintain_time_gap` minutes of idle time, and the recorder reads text as it goes.

The same page has **Organise now** and **Stop organising**. Stop means stop: the pass puts down the work in hand at the next item, an encode that was interrupted is discarded rather than left half-written, and the next pass picks up where this one stood down rather than repeating it.

The whole pass as a command, if you would rather read an answer than a progress bar:

```powershell
bin\windmaint.exe all --root . --dry-run
bin\windmaint.exe text --root .
bin\windmaint.exe doctor --root .
```

Steps are `text`, `convert`, `refresh`, `expire`, `reindex`, `previews`, `ai-tags`, `ai-summaries`, `backup`, under `all`. Every pass writes its report to `cache\logs\windmaint-idle.log`, so the deletions a scheduled run made are readable afterwards.

## AI summaries and tags

Two layers of prose, and you choose which side writes them.

```powershell
bin\windai.exe summarize --pending
```

sends the stretches that have no summary yet to the endpoint on the AI page and files the answers. While the machine is idle, the maintenance pass does the same for settled days when `enable_ai_summary_in_idle` is on; `summary_stretch_limit_in_idle` caps how many stretches one pass takes on.

Everything lands in `userdata\result_ai_period_summary\` and `userdata\result_ai_daily_summary\`, one JSON file per product day (the day starts at 03:00 by this product's own reckoning), as plain text you can read and rewrite. No database, no opaque blob. The seven prompts are plain text you edit on the AI page. There is no length cap and no word list screening what comes back.

A day's summary is refused until every stretch of it stands, and written with `partial: true` — and read back as partial — when you ask for `allow_partial`.

To check what your install would actually send: `bin\windai.exe doctor` prints the endpoint, model and key fingerprint, and makes exactly one round trip.

## Letting an AI assistant read the library (MCP)

On the AI page, tick *Expose this library to AI tools over MCP*. The tray then starts `windmcp.exe` as one resident HTTP service that every assistant on the machine shares, so no client spawns its own. Point a client at

```
http://<host>:<port>/mcp        Authorization: Bearer <token>
```

using `mcp_server_host`, `mcp_server_port` and `mcp_server_token`. `windmcp` refuses to start on a non-loopback address with authentication off, and turning the switch back off stops the bridge.

There are eleven tools: nine read (status, search, frames around a timestamp, app usage, a day's summary, a frame, the pending summary queue, what summaries exist, the prompt text) and two write. The two writers put text only into the two summary directories named above, never into the index and never into the footage. `windrecorder_summaries_pending` names what is missing and carries each stretch's whole recognised text, so the reading step is one call.

Use the same questions from a terminal with no client configured:

```powershell
bin\windmcp.exe summaries-pending --day 2026-09-26
bin\windmcp.exe summaries-read --json
bin\windmcp.exe period-summary-write <segment> --text-file notes.md
bin\windmcp.exe day-summary-write 2026-09-26
bin\windmcp.exe doctor
```

`--json` gives the exact bytes an assistant sees.

## Choosing an OCR engine

Settings → *Local OCR Engine*. The recorder indexes with the engine you pick, and `wind-reindex` re-reads old footage with it too. What this product can drive is any program that takes an image path and prints text to stdout:

| Engine | How |
|---|---|
| Windows built-in (`Windows.Media.Ocr`) | the default, shipped in `ocr_lib\`; needs the matching Windows language pack installed |
| [Tesseract](https://github.com/tesseract-ocr/tessdoc) | install it and set `TesseractOCR_filepath`; supports 100+ languages at once |
| Anything else with the same shape | set `ocr_engine_command` to its argv — one argument per token, exactly as the product would pass it |
| [WeChat OCR](https://github.com/kanadeblisst00/wechat_ocr) | drivable natively, over the same mmmojo channel the old Python package used, as one resident child process |

WeChat OCR's binaries are somebody else's extraction of a WeChat component; they are not in this repository and not in the application zip, and that component is marked for study and personal use rather than commercial use. Put the three pieces under `ocr_lib\wxocr-binary\`:

```
ocr_lib\wxocr-binary\WeChatOCR.exe
ocr_lib\wxocr-binary\mmmojo_64.dll
ocr_lib\wxocr-binary\Model\
```

`WeChatOCR` then becomes a selectable row. `bin\windsetup.exe check-engines` scores every engine against the shipped fixtures and names the file it could not find until all three are in place.

Rapid OCR and ChineseOCR-lite are listed as *not driveable* rather than offered: their recognition used to run inside the Python this product no longer contains. An `.exe` speaking the same contract is driveable through `ocr_engine_command`.

## Interface language and start on sign-in

Settings → *Interface language* covers English, Simplified Chinese and Japanese; the picker lists only what `config_src/languages.json` really translates, and each language names itself in its own script. The tray menu, the window and the AI answers all follow it.

Settings → *Start on sign-in* registers the tray for your Windows account — the equivalent of

```powershell
bin\windsetup.exe autostart --enable
```

It writes the current user's `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` entry pointing at `bin\windsvc.exe`: the tray, not the window and not the launcher, because the sign-in entry has to name the process that owns the lock. No administrator rights needed.

## Where your data lives

Everything is under the app folder, and moving the app is moving the folder:

| Path | Contents |
|---|---|
| `userdata\videos\` | the recordings |
| `userdata\db\` | one SQLite file per month, plus disposable `_TEMP_READ.db` copies readers use |
| `userdata\result_*\` | word clouds, timelines, light boxes, tags, and the two AI summary families |
| `userdata\config_user.json` | your settings, seeded from `config_src\config_default.json` |
| `cache_screenshot\` | frames waiting to be folded into video |
| `cache\logs\` | what each background pass did, including what it deleted |

## Troubleshooting

**Nothing seems to work.** Ask the install what it has and what it would run:

```powershell
bin\Windrecorder.exe doctor --root .
```

It prints each binary with its build kind and path, the command line every tray item would run, the lock table (free / live pid / stale), and — naming the file — which binary is missing. A missing `windrec.exe` is a missing file, not a silent fallback.

**Recent period is empty in the window.** A reader never queries a live index file; it works on the `_TEMP_READ.db` copy beside each month file, refreshed whenever the original is more than five minutes newer. Deleting those copies is always safe — they are rebuilt by the next read. `bin\windrec.exe status --root .` and `bin\windmaint.exe doctor --root .` report what the index actually holds, so you can tell an empty day from an unread copy.

**A `_TEMP_READ.db` or `-journal` file it cannot read.** Usually the first look while indexing is still running, and the copy was caught mid-write. Wait for the pass, then delete the `*_TEMP_READ.db` files and refresh.

**Windows OCR finds nothing or reads badly.** Check that the target language's Windows language pack or keyboard is installed, then consider a third-party engine, which is usually more accurate and can read several languages at once.

**The work window never ran.** `bin\windmaint.exe all --root . --dry-run` prints the scheduling judgement without taking a lock or writing anything — for example that the window has already closed, or that no idle pass has been asked for.

---

# Part two — developing Windrecorder

## What is in the tree

| Path | What it is |
|---|---|
| `windcap\` | the Rust workspace: every binary the product runs. See [windcap/README.md](windcap/README.md) for the crate map. |
| `windcap\winduiweb\` | the shipped window: React and TypeScript front end in `src\`, Tauri shell in `src-tauri\`. |
| `bin\` | **staged release binaries, not tracked.** `build.ps1 -Stage` writes it, and on a working machine it is a live install. |
| `config_src\` | shipped defaults, prompts, language catalog, synonym indexes. The app seeds `userdata\config_user.json` from here. |
| `ocr_lib\` | the Windows OCR command line, and where `wxocr-binary\` goes. |
| `__assets__\` | OCR test fixtures the engines are scored against, plus documentation images. |
| `docs\adr\` | architecture decision records — read these before changing a boundary. |

## Build

```powershell
powershell -ExecutionPolicy Bypass -File windcap\build.ps1 -Stage
```

`build.ps1` compiles one crate per cargo invocation and never lets a broken crate cost you the others, then prints PRESENT/ABSENT per artefact. rustc's own output goes to `windcap\target\build.log`; only the tail of a failing crate is echoed.

| switch | what it does |
|---|---|
| `-Profile release\|debug` | cargo profile, and the directory artefacts are taken from. Default `release`. |
| `-SkipUi` | skip both front ends. Rehearsal only — the payload then has no window. |
| `-Stage` | copy the built executables into `bin\`. |
| `-Manifest PATH` | write the per-artefact result as tab-separated lines. `release.ps1` reads it. |

Any other parameter is an error, never a silent no-op. Exit `0` means everything asked for was built, `1` means something is missing, `2` is a bad command line. Note that `0` also happens when there is no Rust toolchain at all — the summary then names every binary you do not have.

## Run a development checkout without installing

The runtime searches for binaries in a fixed order, first hit wins:

| order | directory |
|---|---|
| 1 | `%WINDCAP_HOME%` |
| 2 | `<root>\bin\` |
| 3 | `<root>\` |
| 4 | `<root>\windcap\target\release\` |
| 5 | `<root>\windcap\target\debug\` |

So a plain `cargo build --release` is enough to run the product: point the tray at the repository root, which counts as an install because it carries `config_src\`.

```powershell
cargo build --release --manifest-path windcap\Cargo.toml
windcap\target\release\windsvc.exe run --root .
```

Release always outranks debug, and a debug binary is labelled `debug` wherever a report prints it. `%WINDCAP_HOME%` is the way to test a build from a different tree against an existing install without staging.

## Cargo: three facts that cost a turn each

1. **There is no root manifest.** `Cargo.toml` lives in `windcap\`, so every cargo command from the repository root needs `--manifest-path windcap\Cargo.toml` (or `cd windcap` first).
2. **Package names are not executable names.** `-p windai` matches nothing; the package is `wind-ai`. The sixteen packages are `windcap-core`, `wind-base`, `wind-store`, `wind-summary`, `windcap-cli`, `windrec`, `wind-maint`, `wind-reindex`, `windsvc`, `wind-setup`, `wind-ai`, `wind-mcp`, `wind-notes`, `windui`, `windui-web` and `wind-launcher`. A wrong name aborts before anything compiles.
3. **Do not run `cargo fmt` here.** The tree is hand-formatted with long lines and there is no `rustfmt.toml`, so the tool's defaults rewrite 200+ unrelated hunks. Match the surrounding style by hand.

## Tests

```powershell
cargo test --workspace --no-fail-fast --manifest-path windcap\Cargo.toml
```

Use `--no-fail-fast`: without it cargo stops at the first crate and the rest never run.

This checkout is usually also somebody's live install, and a set of tests reads install state (`userdata\config_user.json`, live record locks, `bin\` contents). On a machine that is recording with the bridge switched on, the expected reds are a handful of tests in `wind-base`, `wind-ai`, `wind-maint`, `windui` and `windsvc` — all of them naming a value that exists only in `userdata\`. That is not a regression, and never fix it by editing somebody's config. To gate a change cleanly, run the suite in a git worktree: a tree with no `userdata\` and no live locks is green.

One further trap for a fresh checkout: `core.autocrlf` with no `.gitattributes` writes `config_src\ai_prompts\*.txt` with CRLF, and a few tests compare those files against LF literals byte for byte. Strip the CR in the working copy; the blobs stay LF.

## The window (`winduiweb`)

The assets are embedded into the executable at compile time, so a front-end change needs both halves:

```powershell
cd windcap\winduiweb
pnpm install          # the lockfile committed here is pnpm-lock.yaml
pnpm build
cargo build --release -p windui-web --features windui-web/custom-protocol
```

`--features windui-web/custom-protocol` is not optional. Without it the binary opens `build.devUrl` (`http://localhost:1421`) and renders a "connection refused" page that looks like a broken install and is not. `pnpm typecheck` is the fast loop, and `node_modules\`, `dist\` and `src-tauri\gen\` are all gitignored.

## Package, then prove it

```powershell
powershell -ExecutionPolicy Bypass -File windcap\release.ps1
powershell -ExecutionPolicy Bypass -File windcap\smoke.ps1
```

`release.ps1` calls `build.ps1 -Profile release`, stages a tree under `windcap\dist\`, and writes `Windrecorder-native-<version>.zip` with its `.sha256`. The version comes from `[workspace.package]` in `windcap\Cargo.toml` — parsed, never copied. An artefact is staged only when this run reported it BUILT; `windrec.exe` or `windcap.dll` missing means no zip at all rather than a stale one with a fresh date on it. `windcap\dist\` and `windcap\target\` are gitignored.

`smoke.ps1` gates the zip, not `bin\`. It unzips into `%TEMP%`, gives the throwaway install the one thing the payload must not carry (a `userdata\` with a frozen month database), runs the shipped binaries against that root, and asserts on output rather than exit codes — these binaries exit `0` while reporting that they found nothing, which is exactly the shape a layout mistake takes. It checks the hash, the archive layout, that every `bin\` member is present, the DLL's candidate order and the ABI version the source declares, search and stats against known reference rows, that the tray and the launcher both lay a tree out on first run, and that no read-only command created or mutated anything. A check that cannot be made prints SKIP and is not counted as a pass. Point `-Zip` at a doctored copy to learn whether the gate is a gate. `-Source` wants a frozen copy of a month database on a machine whose recorder is still running, because three reference checks count the rows in it.

`windcap\extras.ps1` builds the two third-party archives — `Windrecorder-ffmpeg-<version>.zip` and `Windrecorder-wxocr-binary-<version>.zip` — each with its own hash, because neither licence is this project's to fold into a GPL-2.0 application zip. It packs files already on the machine, then unpacks each archive again and runs it, and exits non-zero rather than shipping one that cannot do what its notes say.

## Conventions worth keeping

- **One writer per month database.** Steps are ordered so that no two of them open the same file at the same moment; `busy_timeout` is a fallback for readers, not a lock.
- **A pass must not promise what it cannot reach.** Progress denominators are the work this pass actually took on, and anything left outside is named rather than silently deferred.
- **Anything that cannot be regenerated is handled with more care than anything that can.** That is why masking happens at capture, why `forget` also removes derived AI text, why `backup` carries the summary directories, and why `forget` is in no scheduled pass.
- **Every CLI option is documented in the help text**, and a test checks that. Add the option to the help block or the guard fails.
- **`windcap/core` keeps zero third-party dependencies.** Every Win32 call is a raw `extern "system"` declaration, so `cargo build --offline` works on a machine with nothing but a warm cargo cache. Do not add a Win32 crate there.
- **Stop a recording cleanly.** The graceful path is a `CTRL_BREAK_EVENT` to the recorder, which closes and commits the current segment; a `taskkill` skips that and loses up to the segment's length.

## Contributing

Translations of the interface go in `config_src\languages.json` (and the window's catalog); the guide is [`__assets__/Multilingual_Translation_Contribution_Guide.md`](__assets__/Multilingual_Translation_Contribution_Guide.md). An OCR engine needs nothing but the image-in, text-out contract, stated in [`windcap/base/src/ocr.rs`](windcap/base/src/ocr.rs), and `bin\windsetup.exe check-engines` scores every registered engine against those fixtures. Read the records in `docs\adr\` before proposing a change to a boundary: which interface is the only one, what the AI endpoint may and may not be told to do, and what the organise pass is allowed to run in parallel.

## License and attribution

Windrecorder is licensed under the **GPL-2.0**; see [LICENSE](LICENSE).

This repository is a fork of [yuka-friends/Windrecorder](https://github.com/yuka-friends/Windrecorder) by the Windrecorder project, which is where the product, its interface design and its index schema come from; the fork history carries that work. The Rust engine in `windcap\` re-implements the hot paths, and the Python application that used to carry them was removed from this tree.

Third-party components are not redistributed inside the application package and keep their own terms: [Tesseract](https://github.com/tesseract-ocr/tessdoc), [Windows.Media.Ocr.Cli](https://github.com/zh-h/Windows.Media.Ocr.Cli), [wechat_ocr](https://github.com/kanadeblisst00/wechat_ocr) (whose WeChat component is marked for study and personal use rather than commercial use), [RapidOCR](https://github.com/RapidAI/RapidOCR), [chineseocr_lite](https://github.com/DayBreak-u/chineseocr_lite), [ffmpeg](https://ffmpeg.org/) and [uForm](https://github.com/unum-cloud/uform).
