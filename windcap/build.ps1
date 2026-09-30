<#
    build.ps1 -- build every native artefact Windrecorder can use, and stage the ones it runs.

    ONE COMMAND, TEN ARTEFACTS
        powershell -ExecutionPolicy Bypass -File windcap\build.ps1 -Stage

    WHY IT LOOKS LIKE THIS

    1. FAIL-SOFT, PER ARTEFACT.  Every artefact is built by its own `cargo build -p ...`
       invocation, so a syntax error in `windui` cannot stop `windrec` from being built and
       staged.  That mirrors the runtime contract exactly: `supervisor\src\native.rs` answers
       "is this binary here?" one binary at a time (`find_binary`, over `candidate_dirs`), and a
       user with a working windrec.exe and a broken winduiweb.exe still gets recording and loses
       only the window.  A script that aborted on the first failure would be *worse* than a broken
       crate -- it would take the three good crates down with it.  So this finishes everything it
       can, then exits non-zero with a summary naming exactly what is missing.

    2. NO RUST TOOLCHAIN IS NOT AN ERROR.  If `cargo` is not on PATH we print the rustup URL
       and `exit 0` (see Test-Toolchain).  A missing compiler is a different fact from a crate
       that does not compile, and this script's job is to report the first without inventing the
       second: the "what this install can do" block still runs and still names every absent
       binary, and `release.ps1` reads that manifest rather than this exit code, so it carries on
       either way and calls the payload NOT SHIPPABLE itself.  Real failures (a crate that does
       not compile) DO exit non-zero.  What is NOT true any more, and used to be the headline
       reason for the soft exit: a machine with no toolchain used to keep recording anyway on the
       Python recorder, which this script only improved.  The Python application was deleted in
       3f37cbf and the tray lost its fallback in 2b6f318, so on this branch "no windrec.exe" is
       not a graceful degradation -- it is an install that cannot record, and the exit status is
       the least important thing this script tells you about it.

    3. LEGIBILITY.  rustc output for a cold release build is thousands of lines and four
       minutes of noise, none of which answers "what do I have now?".  All cargo output goes
       to windcap\target\build.log; the console gets one line per artefact with its status,
       size and elapsed seconds, and the tail of the log only when something failed.

    Windows PowerShell 5.1 is the floor: this box has no pwsh guarantee, so there is no `&&`,
    no ternary and no `??` anywhere below, and every cmdlet used is v5-safe.
#>

[CmdletBinding()]
param(
    # A debug build is honoured by the runtime -- but only when nothing better exists, and it
    # is labelled as a debug build wherever it is reported, because it is roughly an order of
    # magnitude slower and cannot carry the performance claim.  That rule lives in
    # supervisor\src\native.rs: candidate_dirs() lists target\release ahead of target\debug,
    # and describe_build() turns the directory a binary came from into the word in the report.
    [ValidateSet('release', 'debug')]
    [string] $Profile = 'release',
    [switch] $SkipUi,
    [switch] $Stage,
    # release.ps1 needs per-artefact status, size and elapsed seconds, and parsing them back out of
    # the aligned console table would tie a machine reader to a formatting decision made for humans.
    # So the same numbers are also written as tab-separated lines here. Unset for the installer path,
    # which reads nothing.
    [string] $Manifest
)

$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'

# ---------------------------------------------------------------------------
# Reject parameters we do not understand.
#
# A misspelt switch (`-Stagee`) is caught by CmdletBinding itself, but a stray positional
# word (`build.ps1 stage`) binds to nothing at all and would otherwise be silently ignored.
# Silently ignoring "stage" and then printing "build succeeded" is exactly the kind of
# packaging lie that costs an afternoon, so it is fatal.
# ---------------------------------------------------------------------------
if ($args -and $args.Count -gt 0) {
    Write-Host 'build.ps1: unexpected argument(s):' -ForegroundColor Red
    foreach ($stray in $args) { Write-Host ('    ' + $stray) -ForegroundColor Red }
    Write-Host 'usage: powershell -ExecutionPolicy Bypass -File windcap\build.ps1 [-Profile release|debug] [-SkipUi] [-Stage] [-Manifest PATH]'
    exit 2
}

$workspace = $PSScriptRoot
$root = Split-Path -Parent $workspace
$profileDir = Join-Path $workspace ('target\' + $Profile)
# target/ is gitignored, so the log of a broken build lives where the build itself lives.
$logPath = Join-Path $workspace 'target\build.log'
$binDir = Join-Path $root 'bin'

# ---------------------------------------------------------------------------
# The artefacts, and what each one buys.
#
# `Package` is the cargo package name; `Outputs` are the file names cargo may write under
# target\<profile>\, first match wins.  wind-maint lists two names on purpose: the runtime
# looks for `windmaint.exe` (which maint/Cargo.toml now declares), and an older build left a
# `wind-maint.exe` behind in target\ that never goes away on its own.  Preferring the right
# name and staging under it is what keeps a stale artefact from being copied into bin\.
#
# `StageName` is the shipped/not-shipped switch, and it is the only place that question is answered:
# `-Stage` copies a named artefact into `<root>\bin\`, and a `$null` one is built, reported and
# nothing more -- it stays in `target\`, and `release.ps1`, which stages from this script's manifest,
# carries no row for it.
# ---------------------------------------------------------------------------
$artefacts = @(
    [pscustomobject]@{
        Name      = 'windcap.dll'
        Kind      = 'dll'
        Package   = 'windcap-core'
        Outputs   = @('windcap.dll')
        StageName = $null
        Slow      = $false
        Note      = 'session-state probes (locked screen, idle, sleep drift) in ~7us; the 8.2s it replaced was the psutil walk in the deleted Python app (bench/ logs)'
        Absent    = 'windcap.dll not found; the session probes have no exported C ABI (core\src\ffi_c.rs), and release.ps1 refuses to call a payload without it shippable'
    },
    [pscustomobject]@{
        Name      = 'windrec.exe'
        Kind      = 'exe'
        Package   = 'windrec'
        Outputs   = @('windrec.exe')
        StageName = 'windrec.exe'
        Slow      = $false
        Note      = 'native recording: windsvc, the tray, runs `windrec loop --root <root>`'
        Absent    = 'windrec.exe not found; build it with `cargo build --release -p windrec` in windcap/ -- there is no recorder left behind it, so the tray reports the missing file and recording does not start'
    },
    [pscustomobject]@{
        Name      = 'windcapctl.exe'
        Kind      = 'exe'
        Package   = 'windcap-cli'
        Outputs   = @('windcapctl.exe')
        StageName = 'windcapctl.exe'
        Slow      = $false
        Note      = 'terminal front door: query / day / stats / inspect, and grab / bench to measure capture'
        Absent    = 'windcapctl.exe missing -- no way to search or benchmark the index without a UI'
    },
    [pscustomobject]@{
        Name      = 'windmaint.exe'
        Kind      = 'exe'
        Package   = 'wind-maint'
        Outputs   = @('windmaint.exe', 'wind-maint.exe')
        StageName = 'windmaint.exe'
        Slow      = $false
        Note      = 'idle maintenance as a command: convert, refresh, expire, backup, doctor (--dry-run is real)'
        Absent    = 'windmaint.exe not found; build it with `cargo build --release -p wind-maint` in windcap/ -- the recorder looks for it when maintenance is due, finds nothing, and the idle convert/expire/backup pass is skipped'
    },
    # Retired from the shipped set by docs\adr\2026-09-27-winduiweb-is-the-only-interface.md, and
    # built with no `StageName` on purpose: `release.ps1` no longer stages it and
    # `supervisor\src\native.rs::BINARIES` no longer lists it, so nothing in an install is owed this
    # file. The crate is still compiled, still tested and still has to keep compiling, because its
    # library target `wind_ui` is the frame door, the prompt panel and the form field declarations --
    # the only implementation of them, and what `windui-web` links. A build failure here therefore
    # takes the shipped window with it even though this binary ships nowhere.
    [pscustomobject]@{
        Name      = 'windui.exe'
        Kind      = 'exe'
        Package   = 'windui'
        Outputs   = @('windui.exe')
        StageName = $null
        Slow      = $true
        Note      = 'the retired egui front end: built for its `wind_ui` library, which the shipped window reads through; its .exe is no longer staged or shipped'
        Absent    = 'windui.exe not built -- the egui window is retired and nothing ships it, but the `windui` crate is `wind_ui`, the library winduiweb.exe links: if this failed to compile, winduiweb.exe did too'
    },
    [pscustomobject]@{
        Name      = 'winduiweb.exe'
        Kind      = 'exe'
        Package   = 'windui-web'
        Outputs   = @('winduiweb.exe')
        StageName = 'winduiweb.exe'
        Slow      = $true
        # Tauri reads "development shell" versus "shipped app" from this feature and *not* from the
        # cargo profile: without it `generate_context!` embeds no assets and the window opens
        # build.devUrl, so the exe only works while somebody happens to have `pnpm dev` running. The
        # Tauri CLI adds the flag for you; this script calls cargo, so it is declared here.
        Features  = @('custom-protocol')
        # The bundle has to exist before the crate is compiled, because the crate embeds it.
        Frontend  = 'winduiweb'
        Note      = 'the HTML window: the same six screens over the same wind-ui query layer, drawn by Tauri'
        Absent    = 'winduiweb.exe not found -- this is the only window the product ships and the only one the tray opens, so with it absent there is no interface at all (the egui windui.exe is retired: built, not shipped) -- in windcap/winduiweb run `pnpm install`, then `cargo build --release -p windui-web --features windui-web/custom-protocol` from windcap/'
    },
    [pscustomobject]@{
        Name      = 'Windrecorder.exe'
        Kind      = 'exe'
        Package   = 'wind-launcher'
        Outputs   = @('Windrecorder.exe')
        StageName = 'Windrecorder.exe'
        Slow      = $false
        Note      = 'the file to double-click: it starts bin\windsvc.exe and hands it argv, and nothing else'
        Absent    = 'Windrecorder.exe not found; nothing stops -- bin\windsvc.exe is the tray and starts the same way -- but the payload then holds ten executables and not one of them is called Windrecorder, which is the thing a stranger has to be told by a README'
    },
    [pscustomobject]@{
        Name      = 'windsvc.exe'
        Kind      = 'exe'
        Package   = 'windsvc'
        Outputs   = @('windsvc.exe')
        StageName = 'windsvc.exe'
        Slow      = $false
        Note      = 'the tray and supervisor: single-instance lock, menu, and the graceful CTRL_BREAK stop of windrec and the interface'
        Absent    = 'windsvc.exe not found; without it nothing starts the recorder for you -- bin\windrec.exe loop --root <install> by hand is the only way in'
    },
    [pscustomobject]@{
        Name      = 'windmcp.exe'
        Kind      = 'exe'
        Package   = 'wind-mcp'
        Outputs   = @('windmcp.exe')
        StageName = 'windmcp.exe'
        Slow      = $false
        Note      = 'the HTTP MCP bridge: read-only screen memory for AI assistants, loopback and token-gated by default'
        Absent    = 'windmcp.exe not found; the fork''s namesake feature is simply absent'
    },
    [pscustomobject]@{
        Name      = 'wind-reindex.exe'
        Kind      = 'exe'
        Package   = 'wind-reindex'
        Outputs   = @('wind-reindex.exe')
        StageName = 'wind-reindex.exe'
        Slow      = $false
        Note      = 'index already-recorded video, so a library recorded before the move becomes searchable'
        Absent    = 'wind-reindex.exe not found; new recordings index, old video never will'
    },
    [pscustomobject]@{
        Name      = 'windnotes.exe'
        Kind      = 'exe'
        Package   = 'wind-notes'
        Outputs   = @('windnotes.exe')
        StageName = 'windnotes.exe'
        Slow      = $false
        Note      = 'the user''s own bookmarks: the flag/note store, capture-on-flag, and timeline marker geometry'
        Absent    = 'windnotes.exe not found; flags and notes have no native reader or writer'
    },
    [pscustomobject]@{
        Name      = 'windsetup.exe'
        Kind      = 'exe'
        Package   = 'wind-setup'
        Outputs   = @('windsetup.exe')
        StageName = 'windsetup.exe'
        Slow      = $false
        Note      = 'first-run layout, OCR engine probing, and the re-entrant migration over existing month files'
        Absent    = 'windsetup.exe not found; no init to lay the 16 writable slots, no check-engines to prove an OCR engine, no migration over existing month files -- the binaries still run, on the defaults compiled into them rather than on a seeded config_user.json'
    },
    [pscustomobject]@{
        Name      = 'windai.exe'
        Kind      = 'exe'
        Package   = 'wind-ai'
        Outputs   = @('windai.exe')
        StageName = 'windai.exe'
        Slow      = $false
        Note      = 'natural-language search and monthly activity tags against an OpenAI-compatible endpoint'
        Absent    = 'windai.exe not found; natural-language search and monthly activity tags are simply absent -- there is no other implementation of them left'
    }
)

function Format-Size {
    param([long] $Bytes)
    if ($Bytes -ge 1048576) { return ('{0:N1} MiB' -f ($Bytes / 1MB)) }
    if ($Bytes -ge 1024) { return ('{0:N1} KiB' -f ($Bytes / 1KB)) }
    return ('{0} bytes' -f $Bytes)
}

# The same number twice, once for eyes and once for release.ps1's notes file: `Size` is a formatted
# string, and a formatter is not something a second script should have to reverse.
function Get-SizeBytes {
    param([string] $Path)
    if ($Path -and (Test-Path -LiteralPath $Path)) { return (Get-Item -LiteralPath $Path).Length }
    return 0
}

function Get-Artefact {
    param([string] $Name)
    foreach ($artefact in $artefacts) {
        if ($artefact.Name -eq $Name) { return $artefact }
    }
    return $null
}

function Write-LogHeader {
    param([string] $Text)
    $logDir = Split-Path -Parent $logPath
    if (-not (Test-Path -LiteralPath $logDir)) {
        New-Item -ItemType Directory -Path $logDir -Force | Out-Null
    }
    Add-Content -LiteralPath $logPath -Value ('' + [Environment]::NewLine)
    Add-Content -LiteralPath $logPath -Value ('===== ' + $Text + '  ' + (Get-Date -Format 's') + ' =====')
}

function Write-LogChunk {
    param([string[]] $Lines)
    if ($Lines) {
        Add-Content -LiteralPath $logPath -Value ($Lines -join [Environment]::NewLine)
    }
}

function Get-LogTail {
    param([string[]] $Lines, [int] $Count)
    if (-not $Lines) { return @() }
    if ($Lines.Count -le $Count) { return $Lines }
    return $Lines[($Lines.Count - $Count)..($Lines.Count - 1)]
}

# ---------------------------------------------------------------------------
# cargo must exist.  If it does not, we are done -- successfully.
# ---------------------------------------------------------------------------
function Test-Toolchain {
    $found = Get-Command cargo -ErrorAction SilentlyContinue
    if ($found) { return }

    Write-Host ''
    Write-Host 'cargo not found on PATH -- skipping the native engine build.' -ForegroundColor Yellow
    Write-Host '  Read this as a real gap, not a harmless one. The binaries built here ARE' -ForegroundColor Yellow
    Write-Host '  Windrecorder now: the Python application was deleted in 3f37cbf and the tray lost' -ForegroundColor Yellow
    Write-Host '  its interpreter fallback in 2b6f318, so an install with no windrec.exe does not' -ForegroundColor Yellow
    Write-Host '  quietly record some other way -- it reports the file as missing, which is the' -ForegroundColor Yellow
    Write-Host '  honest answer and still not a working one. Whatever is already in bin\ from an' -ForegroundColor Yellow
    Write-Host '  earlier build is left exactly as it is, and will still be found.' -ForegroundColor Yellow
    Write-Host ''
    Write-Host '  To build the native engine later, install the Rust toolchain:'
    Write-Host '      https://rustup.rs'
    Write-Host '  then re-run:  powershell -ExecutionPolicy Bypass -File windcap\build.ps1 -Stage'
    $bundled = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    if (Test-Path -LiteralPath $bundled) {
        Write-Host ''
        Write-Host "  A toolchain does exist at $bundled but is not on this PATH." -ForegroundColor Yellow
        Write-Host '  Open a new terminal after rustup, or add %USERPROFILE%\.cargo\bin to PATH.'
    }
    # Exit 0 on purpose -- see reason 2 in the header. Non-zero here would break the update
    # path, and every existing user's expectation of it, over an optional component.
    exit 0
}

# ---------------------------------------------------------------------------
# One artefact, one cargo invocation, one line of console output.
#
# The single invocation is deliberate: cargo is the only thing that knows whether a crate is
# up to date, and its own answer ("Finished in 0.3s" with no `Compiling` line) is what lets us
# report `up to date` in 0.3s versus a real four-minute compile without lying about either.
#
# `cmd /c "... 2>&1"` rather than `& cargo ... 2>&1`: PowerShell 5.1 wraps every native stderr
# line in an ErrorRecord, which lands in the log as "System.Management.Automation.RemoteException"
# instead of the rustc message we are trying to show, and reorders stdout against stderr while it
# is at it.  Letting cmd.exe merge the two streams at the OS level keeps the log readable and the
# output in the order it was produced.
# ---------------------------------------------------------------------------
function Get-CargoProfileArgs {
    param([string] $Name)
    # Cargo has no `--debug`, and `--profile debug` is a reserved name (`dev` is the real one).
    # The default profile already is the debug profile and already writes target\debug, so the
    # correct way to ask for it is to say nothing.
    if ($Name -eq 'release') { return @('--release') }
    return @()
}

# --color=never: this script never shows cargo's own console output, so ANSI escapes would only
# be noise in target\build.log, where the tail we print comes from.
function Get-CargoBuildArgs {
    param($Artefact, [string] $Name)
    $built = @('build', '--color=never', '-p', $Artefact.Package)
    $built += Get-CargoProfileArgs -Name $Name
    # Fully qualified (`package/feature`) rather than bare: a bare `--features` names a feature of
    # the *selected* package, and this one belongs to `tauri`. Qualifying it works from either
    # directory and survives the crate being built as part of `--workspace` later.
    foreach ($feature in @($Artefact.Features)) {
        if ($feature) { $built += @('--features', ('{0}/{1}' -f $Artefact.Package, $feature)) }
    }
    return $built
}

# ---------------------------------------------------------------------------
# FRONTEND BUNDLE — the one artefact whose inputs are not Rust.
#
# `winduiweb.exe` embeds `winduiweb/dist` at compile time, so the bundle has to exist *before* cargo
# runs or the binary links successfully and shows a blank page: an error that appears only when
# somebody opens the window. `tauri build` would do this for us via beforeBuildCommand, but it also
# wants to produce an MSI and its own target directory, and this script is the thing that decides
# what a release contains.
#
# Fail-soft, like every other artefact: a machine with no Node gets a warning and an absent
# winduiweb.exe, and everything else this script builds is still built and still staged. It is no
# longer true that another window keeps working -- the egui `windui.exe` was retired from the shipped
# set on 2026-09-27, so a payload with no HTML window has no interface in it at all. That is why this
# one is reported as SKIPPED rather than passed over, and why `-SkipUi` is a rehearsal switch.
# ---------------------------------------------------------------------------
function Invoke-FrontendBundle {
    param($Artefact)

    $dir = Join-Path $workspace $Artefact.Frontend
    if (-not (Test-Path -LiteralPath (Join-Path $dir 'package.json'))) {
        Write-LogChunk @("no package.json under $dir -- nothing to bundle")
        return $false
    }
    $pnpm = Get-Command pnpm -ErrorAction SilentlyContinue
    if (-not $pnpm) {
        Write-LogChunk @('pnpm is not on PATH; the HTML window cannot be bundled. Install Node + pnpm and re-run -- this is the only interface the product ships, so a payload built without it has no window in it.')
        return $false
    }

    Push-Location -LiteralPath $dir
    try {
        # `--offline` first because the workspace lock is the source of truth and a machine with no
        # network must still be able to rebuild what it already installed; only if that refuses do we
        # allow a fetch, and the log says which path was taken.
        $install = @(& cmd.exe /c 'pnpm install --offline 2>&1')
        if (0 -ne $LASTEXITCODE) {
            Write-LogChunk @('pnpm install --offline refused; retrying with the network')
            $install += @(& cmd.exe /c 'pnpm install --prefer-offline 2>&1')
            if (0 -ne $LASTEXITCODE) { Write-LogChunk $install; return $false }
        }
        $bundle = @(& cmd.exe /c 'pnpm run build 2>&1')
        Write-LogChunk ($install + $bundle)
        return (0 -eq $LASTEXITCODE)
    }
    finally {
        Pop-Location
    }
}

function Invoke-ArtefactBuild {
    param($Artefact, [string[]] $CargoArgs)

    if ($Artefact.Frontend -and -not (Invoke-FrontendBundle -Artefact $Artefact)) {
        # The bundle failed, so compiling would embed whatever `dist` is already there — a stale page,
        # or none at all — and the only symptom would be a window somebody opens later. Report it the
        # way `-SkipUi` reports a skipped artefact, with the same row shape, so the caller's table and
        # its `Absent` line stay the single source of what is missing.
        Write-LogHeader ('skipped ' + $Artefact.Name + ' -- the front-end bundle step failed')
        return [pscustomobject]@{
            Artefact  = $Artefact
            Status    = 'SKIPPED'
            Size      = '--'
            SizeBytes = 0
            Seconds   = 0.0
            Source    = $null
            Staged    = $null
            Note      = 'front-end bundle failed (see target\build.log)'
            Tail      = @()
        }
    }

    Write-LogHeader ('cargo ' + ($CargoArgs -join ' '))
    $stopwatch = New-Object diagnostics.stopwatch
    # Run from the workspace, and hand cargo no paths: the caller's directory is not ours to
    # assume, and a worktree under a Chinese directory name is one more thing that should not
    # have to survive a trip through cmd.exe's codepage.
    Push-Location -LiteralPath $workspace
    try {
        $stopwatch.Start()
        $output = @(& cmd.exe /c ('cargo ' + ($CargoArgs -join ' ') + ' 2>&1'))
        $stopwatch.Stop()
        $code = $LASTEXITCODE
    }
    finally {
        Pop-Location
    }
    Write-LogChunk $output

    $seconds = $stopwatch.Elapsed.TotalSeconds
    $tail = Get-LogTail -Lines $output -Count 15

    if ($code -ne 0) {
        # The last line of a rustc failure is a documentation URL; the first `error` line is the
        # thing the user has to act on.  Matched as a regex because `-like` treats `[` in
        # `error[E0433]` as the start of a wildcard set and throws on the unbalanced bracket.
        $why = $null
        foreach ($line in $output) {
            if ($line -match '(^|\s)error(\[E\d+\])?:') {
                $why = $line.Trim()
                break
            }
        }
        if (-not $why) {
            # No `error` line at all: cargo died before compiling (a locked target directory, a
            # killed process, a bad command line), so show the last thing it did say.
            if ($tail -and $tail.Count -gt 0 -and $tail[-1]) { $why = ($tail[-1]).Trim() }
            else { $why = 'no output from cargo' }
        }
        return [pscustomobject]@{
            Artefact = $Artefact
            Status   = 'FAILED'
            Size     = '--'
            SizeBytes = 0
            Seconds  = $seconds
            Source   = $null
            Staged   = $null
            Note     = ('cargo exit ' + $code + ': ' + $why)
            Tail     = $tail
        }
    }

    $source = $null
    foreach ($candidate in $Artefact.Outputs) {
        $probe = Join-Path $profileDir $candidate
        if (Test-Path -LiteralPath $probe) { $source = $probe; break }
    }
    if (-not $source) {
        return [pscustomobject]@{
            Artefact = $Artefact
            Status   = 'FAILED'
            Size     = '--'
            SizeBytes = 0
            Seconds  = $seconds
            Source   = $null
            Staged   = $null
            Note     = ('cargo succeeded but no ' + ($Artefact.Outputs -join ' or ') + ' in target\' + $Profile)
            Tail     = $tail
        }
    }

    # Say which of the two things just happened: a warm rebuild of an unchanged tree is a
    # different claim from a compile, and the elapsed seconds mean nothing unless the reader
    # knows which one they are looking at.
    $note = 'up to date'
    foreach ($line in $output) {
        if ($line -like '*Compiling*') { $note = 'compiled deps'; break }
    }
    if ($note -eq 'compiled deps') {
        foreach ($line in $output) {
            if ($line -like ('*Compiling ' + $Artefact.Package + ' v*')) { $note = 'compiled'; break }
        }
    }

    $builtBytes = Get-SizeBytes -Path $source
    return [pscustomobject]@{
        Artefact = $Artefact
        Status   = 'BUILT'
        Size     = Format-Size -Bytes $builtBytes
        SizeBytes = $builtBytes
        Seconds  = $seconds
        Source   = $source
        Staged   = $null
        Note     = $note
        Tail     = $tail
    }
}

# ---------------------------------------------------------------------------
# Where the runtime would find this binary, using the order it actually searches in:
# supervisor\src\native.rs's candidate_dirs(), reproduced here so the script reports the same
# answer the tray will give and never a more optimistic one.
# ---------------------------------------------------------------------------
function Get-CandidateDirs {
    $candidates = @()
    if ($env:WINDCAP_HOME) { $candidates += $env:WINDCAP_HOME }
    $candidates += $binDir
    $candidates += $root
    $candidates += (Join-Path $workspace 'target\release')
    $candidates += (Join-Path $workspace 'target\debug')
    return $candidates
}

function Resolve-NativeBinary {
    param([string] $Name)
    foreach ($directory in (Get-CandidateDirs)) {
        $probe = Join-Path $directory ($Name + '.exe')
        if (Test-Path -LiteralPath $probe) { return $probe }
    }
    return $null
}

# describe_build() in supervisor\src\native.rs: the directory the binary came from *is* the label.
function Get-ProfileLabel {
    param([string] $Path)
    if (-not $Path) { return 'missing' }
    $leaf = Split-Path -Leaf (Split-Path -Parent $Path)
    if ($leaf -eq 'release' -or $leaf -eq 'debug') { return $leaf }
    return 'installed'
}

# Where windcap.dll is looked for, which is NOT the same as the exe list above.  Two
# differences, neither of them an accident to "fix" here: `bin/` is probed first, so a
# dropped-in release payload outranks whatever a developer's `cargo build` left in the tree, and it
# names *release* and nothing else under target/, so a -Profile debug windcap.dll is invisible to
# it.  This is the order release.ps1 stages against and smoke.ps1 proves.  It was first written
# down in the Python bridge, which was deleted in 3f37cbf; the list outlived it, and is stated here
# rather than mirrored from there.  Report what the payload will actually be checked against.
function Resolve-NativeDll {
    $candidates = @()
    if ($env:WINDCAP_DLL) { $candidates += $env:WINDCAP_DLL }
    $candidates += (Join-Path $binDir 'windcap.dll')
    $candidates += (Join-Path $workspace 'target\release\windcap.dll')
    $candidates += (Join-Path $workspace 'windcap.dll')
    $candidates += (Join-Path $root 'windcap.dll')
    $script:dllSearchList = $candidates -join ', '
    foreach ($probe in $candidates) {
        if ($probe -and (Test-Path -LiteralPath $probe)) { return $probe }
    }
    return $null
}

function Show-CapabilityLine {
    param([string] $Label, [string] $Path, [string] $Note, [string] $AbsentNote)
    if ($Path -and (Test-Path -LiteralPath $Path)) {
        $size = Format-Size ((Get-Item -LiteralPath $Path).Length)
        Write-Host ('  PRESENT  ' + ('{0,-13}' -f $Label) + ' ' + (Get-ProfileLabel $Path) + '  ' + ('{0,11}' -f $size)) -ForegroundColor Green
        Write-Host ('           ' + $Note) -ForegroundColor DarkGray
        Write-Host ('           ' + $Path) -ForegroundColor DarkGray
    }
    else {
        Write-Host ('  ABSENT   ' + ('{0,-13}' -f $Label)) -ForegroundColor Red
        Write-Host ('           ' + $AbsentNote) -ForegroundColor Red
        Write-Host ('           searched: ' + ((Get-CandidateDirs) -join ', ')) -ForegroundColor DarkGray
    }
}

# ===========================================================================
# Run
# ===========================================================================
Test-Toolchain

# The last line, not the first: cargo may preface everything with the `~/.cargo/config`
# deprecation warning, which is noise on a status line but a real error in the log.
$rustVersionLines = @(& cmd.exe /c 'cargo --version 2>&1')
$rustVersion = Get-LogTail -Lines $rustVersionLines -Count 1
$rustVersion = ($rustVersion -join '')

Write-Host ''
Write-Host 'windcap native engine build' -ForegroundColor Cyan
Write-Host ('  workspace  ' + $workspace)
Write-Host ('  profile    ' + $Profile)
Write-Host ('  toolchain  ' + $rustVersion)
if ($Stage) { Write-Host ('  staging    ' + $binDir) }
Write-Host ('  log        ' + $logPath)

if (-not (Test-Path -LiteralPath $profileDir)) {
    New-Item -ItemType Directory -Path $profileDir -Force | Out-Null
}
if ($Stage -and -not (Test-Path -LiteralPath $binDir)) {
    New-Item -ItemType Directory -Path $binDir -Force | Out-Null
    Write-Host ('  created    ' + $binDir)
}
Write-Host ''

$entries = @()
$totalStopwatch = New-Object diagnostics.stopwatch
$totalStopwatch.Start()

foreach ($artefact in $artefacts) {
    $cargoArgs = Get-CargoBuildArgs -Artefact $artefact -Name $Profile

    if ($artefact.Slow -and $SkipUi) {
        $entries += [pscustomobject]@{
            Artefact = $artefact
            Status   = 'SKIPPED'
            Size     = '--'
            SizeBytes = 0
            Seconds  = 0.0
            Source   = $null
            Staged   = $null
            Note     = '-SkipUi'
            Tail     = @()
        }
        Write-Host ('  {0,-16} {1,-12} {2,11} {3,8:N1}s  {4}' -f $artefact.Name, 'SKIPPED', '--', 0.0, 'eframe is the slow crate; rebuild without -SkipUi') -ForegroundColor Yellow
        continue
    }

    $entry = Invoke-ArtefactBuild -Artefact $artefact -CargoArgs $cargoArgs
    $entries += $entry

    $line = ('  {0,-16} {1,-12} {2,11} {3,8:N1}s  {4}' -f $artefact.Name, $entry.Status, $entry.Size, $entry.Seconds, $entry.Note)
    if ($entry.Status -eq 'BUILT') {
        Write-Host $line -ForegroundColor Green
    }
    else {
        Write-Host $line -ForegroundColor Red
        Write-Host ('    --- last lines of cargo output for ' + $artefact.Name + ' (full log: target\build.log) ---') -ForegroundColor Red
        foreach ($tailLine in $entry.Tail) {
            Write-Host ('    ' + $tailLine) -ForegroundColor DarkGray
        }
    }

    if ($Stage -and $entry.Source -and $artefact.StageName) {
        $destination = Join-Path $binDir $artefact.StageName
        try {
            Copy-Item -LiteralPath $entry.Source -Destination $destination -Force -ErrorAction Stop
            $entry.Staged = $destination
            Write-Host ('      staged -> ' + $destination) -ForegroundColor DarkGreen
        }
        catch {
            # A running windrec.exe holds its own image open, and -Stage can be run while the tray
            # is recording. That is a reason the *copy* failed, never a
            # reason to abandon the other four artefacts.
            $entry.Status = 'STAGE FAILED'
            $entry.Note = 'copy failed -- ' + $_.Exception.Message
            Write-Host ('      ' + $artefact.StageName + ' STAGE FAILED: ' + $_.Exception.Message) -ForegroundColor Red
            Write-Host '      (is it already running? Quit Windrecorder in the tray, then re-run)' -ForegroundColor Yellow
        }
    }
}

$totalStopwatch.Stop()
Write-Host ''
Write-Host ('  total ' + ('{0:N1}' -f $totalStopwatch.Elapsed.TotalSeconds) + 's')
Write-Host ''

# ---------------------------------------------------------------------------
# The machine-readable twin of the table above.
#
# Tab-separated with no header row, because the reader is `ConvertFrom-Csv -Header ... -Delimiter
# "`t"` in PowerShell 5.1 and the file is also `type`-able by a human. A `note` may contain commas,
# colons and Windows paths, and never a tab. Written even when everything failed: release.ps1 tells
# "the build reported FAILED" apart from "the build never ran" by whether this file exists at all.
# ---------------------------------------------------------------------------
function Write-BuildManifest {
    param(
        [string] $Path,
        $Entries,
        [string] $UsedProfile,
        [double] $TotalSeconds
    )
    if (-not $Path) { return }
    $total = '{0:N1}' -f $TotalSeconds
    $lines = @()
    foreach ($entry in $Entries) {
        $note = ''
        if ($entry.Note) { $note = ($entry.Note -replace "`t", ' ') }
        $source = ''
        if ($entry.Source) { $source = $entry.Source }
        $lines += @(
            $entry.Artefact.Name,
            $entry.Status,
            [string] $entry.SizeBytes,
            ('{0:N1}' -f $entry.Seconds),
            $UsedProfile,
            $total,
            $source,
            $note
        ) -join "`t"
    }
    try {
        $parent = Split-Path -Parent $Path
        if ($parent -and -not (Test-Path -LiteralPath $parent)) {
            New-Item -ItemType Directory -Path $parent -Force | Out-Null
        }
        # UTF-8, not ASCII: this workspace lives under a directory with a Chinese name, and ASCII
        # would silently write `?` into the source paths that release.ps1 and the notes file quote.
        Set-Content -LiteralPath $Path -Value $lines -Encoding UTF8
        Write-Host ('  manifest   ' + $Path) -ForegroundColor DarkGray
    }
    catch {
        # A manifest that cannot be written is a lost sidecar, never a reason to call a completed
        # build a failure.
        Write-Host ('  manifest NOT written: ' + $_.Exception.Message) -ForegroundColor Yellow
    }
}

Write-BuildManifest -Path $Manifest -Entries $entries -UsedProfile $Profile -TotalSeconds $totalStopwatch.Elapsed.TotalSeconds

# ===========================================================================
# What this install can do -- the question the runtime will actually ask.
# Not "did cargo exit 0", but "what happens when I press Record".
# ===========================================================================
Write-Host 'what this install can do' -ForegroundColor Cyan

$dllArtefact = Get-Artefact 'windcap.dll'
$dllPath = Resolve-NativeDll
if ($dllPath) {
    Write-Host ('  PRESENT  ' + ('{0,-13}' -f 'windcap.dll') + ' ' + (Get-ProfileLabel $dllPath) + '  ' + ('{0,11}' -f (Format-Size ((Get-Item -LiteralPath $dllPath).Length)))) -ForegroundColor Green
    Write-Host ('           ' + $dllArtefact.Note) -ForegroundColor DarkGray
    Write-Host ('           resolved to ' + $dllPath + '  (its exports are core\src\ffi_c.rs; $WINDCAP_DLL overrides)') -ForegroundColor DarkGray
}
else {
    Write-Host ('  ABSENT   ' + ('{0,-13}' -f 'windcap.dll')) -ForegroundColor Red
    Write-Host ('           ' + $dllArtefact.Absent) -ForegroundColor Red
    Write-Host ('           searched: ' + $script:dllSearchList + '  ($WINDCAP_DLL overrides)') -ForegroundColor DarkGray
}

Show-CapabilityLine -Label 'windrec.exe' -Path (Resolve-NativeBinary 'windrec') -Note (Get-Artefact 'windrec.exe').Note -AbsentNote (Get-Artefact 'windrec.exe').Absent
# No windui.exe line. This block answers "what happens when I press Record" -- it resolves each name
# through the same candidate order `supervisor\src\native.rs` uses, and that list no longer contains
# windui. The crate is still built above (its `wind_ui` library is what the shipped window links), and
# a crate that stops compiling is still named by the exit-status summary at the end of this script.
Show-CapabilityLine -Label 'winduiweb.exe' -Path (Resolve-NativeBinary 'winduiweb') -Note (Get-Artefact 'winduiweb.exe').Note -AbsentNote (Get-Artefact 'winduiweb.exe').Absent
Show-CapabilityLine -Label 'windmaint.exe' -Path (Resolve-NativeBinary 'windmaint') -Note (Get-Artefact 'windmaint.exe').Note -AbsentNote (Get-Artefact 'windmaint.exe').Absent
Show-CapabilityLine -Label 'windcapctl.exe' -Path (Resolve-NativeBinary 'windcapctl') -Note (Get-Artefact 'windcapctl.exe').Note -AbsentNote (Get-Artefact 'windcapctl.exe').Absent

# What the tray will actually launch. This used to read `"use_native_core"` out of the user config
# and print "recording stays on Python until you set it" -- but the Python application was deleted
# from this branch, the key lost its last reader, and `python -m windrecorder.native_runtime status`
# names a module that no longer exists. A build script that prints a switch nobody reads is how a
# dead setting survives its own removal, so this now reports the one fact that is still true: whether
# the recorder binary the tray will run is actually present.
$recorderForTray = Resolve-NativeBinary 'windrec'
Write-Host ''
if ($recorderForTray) {
    Write-Host ('  recording  ' + $recorderForTray + ' -- the tray launches this; there is no other implementation') -ForegroundColor DarkGray
}
else {
    Write-Host '  recording  NO windrec.exe FOUND -- the tray will report that the recorder is missing' -ForegroundColor Red
    Write-Host '             build it with this script, or unpack the release zip over the install root' -ForegroundColor Red
}
Write-Host ('  check any time:  ' + (Join-Path $root 'bin\windsvc.exe') + ' doctor --root <install>') -ForegroundColor DarkGray
Write-Host ''

# ===========================================================================
# Exit status: anything asked for and not delivered is named.
# ===========================================================================
$missing = @()
foreach ($entry in $entries) {
    if ($entry.Status -eq 'BUILT' -or $entry.Status -eq 'SKIPPED') { continue }
    $missing += ('  ' + $entry.Artefact.Name + '  [' + $entry.Status + '] ' + $entry.Note)
}

if ($missing.Count -eq 0) {
    Write-Host 'result: every requested artefact built.' -ForegroundColor Green
    if ($SkipUi) {
        Write-Host '        winduiweb.exe (and the retired windui.exe) were skipped by -SkipUi, not lost: rerun without it.' -ForegroundColor Yellow
    }
    exit 0
}

Write-Host ('result: ' + $missing.Count + ' artefact(s) missing. What this install can no longer start is named above, and `windsvc doctor` repeats it.') -ForegroundColor Red
foreach ($line in $missing) { Write-Host ('  ' + $line.TrimStart()) -ForegroundColor Red }
Write-Host ''
Write-Host ('  full cargo output: ' + $logPath)
Write-Host '  rebuild one crate alone: cargo build --release -p <package>   (crate map: windcap\README.md)'
exit 1
