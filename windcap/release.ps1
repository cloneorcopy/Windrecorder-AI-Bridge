<#
    release.ps1 -- turn the workspace into one file another machine can be given.

    ONE COMMAND
        powershell -ExecutionPolicy Bypass -File windcap\release.ps1

    WHAT IT PRODUCES
        windcap\dist\Windrecorder-native-<version>.zip          the payload
        windcap\dist\Windrecorder-native-<version>.zip.sha256   the hash of those exact bytes
        windcap\dist\Windrecorder-native-<version>\             the staged tree, kept for inspection

    WHY IT LOOKS LIKE THIS

    1. IT IS A PACKAGE, NOT AN INSTALLER.  The zip carries everything a first run needs: the binaries
       in bin/, the settings it seeds itself from in config_src/, the OCR command line in ocr_lib/,
       and RELEASE-NOTES.txt. There is no Python in it and it needs no Python outside it -- unzip it
       into an empty directory, double-click `bin\Windrecorder.exe`, and that directory *is* the install.
       That file is a name, not a job: it starts `bin\windsvc.exe`, the tray, which does the work. Two
       things
       it still does not do, and both belong in the notes file rather than in this script's console
       output: userdata/ is created by the first run rather than shipped, because it holds one person's
       recordings; and nothing registers the install -- no registry entry, no shortcut, no uninstaller.
       The tray is the installer now (`supervisor.rs::run_startup_init` runs `windsetup init` before it
       takes the lock), so double-clicking is the whole of what a person has to know. There is no second
       implementation behind any of
       it: commit 3f37cbf deleted the Python application and 2b6f318 removed the tray's fallback with
       it, so a binary that is not in bin\ is a missing file that `windsvc doctor` names, never a
       feature that quietly stayed switched off. Instructions belong in a file a person
       holds before they hold the contents, because a zip nobody read the instructions in is a zip
       that gets unpacked and then ignored.

    2. ONE VERSION, PARSED NOT COPIED.  The version comes out of [workspace.package] in Cargo.toml
       -- the same number every crate already inherits -- so there is no second copy to forget to
       update. It used to be cross-read against a second number, the Python app's own __version__,
       with a mismatch reported rather than resolved. That app was deleted in 3f37cbf and nothing
       carries a version any more except this workspace, so the zip's number is the product's
       number and there is nothing left for it to be proved wrong against.

    3. THE BUILD IS build.ps1's JOB.  This script never calls cargo. build.ps1 already knows the
       five artefacts, which package builds each, how to be fail-soft per artefact, and how to
       handle a machine with no Rust toolchain; duplicating that here would give two answers to one
       question. It reports its numbers through a tab-separated manifest rather than this script
       re-reading a console table that is formatted for humans.

    4. THE HASH IS THE POINT.  A zip without a hash is a rumour: nothing later can tell whether the
       bytes that arrived are the bytes that were built. SHA-256 is printed and also written beside
       the zip, because printing it is how a human sees it and writing it is how a machine does.

    Windows PowerShell 5.1 is the floor: no `&&`, no ternary, no `??`.
#>

[CmdletBinding()]
param(
    # The engine is only ever *shipped* as release, but -Profile debug is how this script is
    # rehearsed in 30 seconds instead of three minutes, so the knob exists -- and it is stamped into
    # the notes file, because a debug payload must not be mistaken for a release one later.
    [ValidateSet('release', 'debug')]
    [string] $Profile = 'release',
    [switch] $SkipUi
)

$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'

# A stray positional binds to nothing at all and would otherwise be silently ignored -- see the same
# guard in build.ps1. Shipping a payload that quietly did not honour its own switch is worse.
if ($args -and $args.Count -gt 0) {
    Write-Host 'release.ps1: unexpected argument(s):' -ForegroundColor Red
    foreach ($stray in $args) { Write-Host ('    ' + $stray) -ForegroundColor Red }
    Write-Host 'usage: powershell -ExecutionPolicy Bypass -File windcap\release.ps1 [-Profile release|debug] [-SkipUi]'
    exit 2
}

$workspace = $PSScriptRoot                   # windcap\
$root = Split-Path -Parent $workspace        # the install root: the folder holding config_src\ and ocr_lib\
$profileDir = Join-Path $workspace ('target\' + $Profile)
$distDir = Join-Path $workspace 'dist'
$buildScript = Join-Path $workspace 'build.ps1'
$manifestPath = Join-Path $distDir 'build-manifest.tsv'
$logPath = Join-Path $workspace 'target\build.log'

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
function Format-Size {
    param([long] $Bytes)
    if ($Bytes -ge 1048576) { return ('{0:N1} MiB' -f ($Bytes / 1MB)) }
    if ($Bytes -ge 1024) { return ('{0:N1} KiB' -f ($Bytes / 1KB)) }
    return ('{0} bytes' -f $Bytes)
}

# Every staged path is printed through this one width, in this script's console table and in the
# notes file, so the two agree by construction. Widened when config_src\ started shipping: a path
# inside a subdirectory of it (`config_src\synonyms\synonyms_sc.index`) is longer than a binary name
# ever was, and a table that loses its alignment on one row reads as a table that lost its columns.
$targetColumn = '{0,-38}'

function Format-Target {
    param([string] $Target)
    return ($targetColumn -f $Target)
}

# Hashing goes straight to .NET rather than to Get-FileHash: on the Windows PowerShell 5.1 this was
# written against, `Get-FileHash` is not a command in the session at all (while the rest of
# Microsoft.PowerShell.Utility is), and a release script that dies on a missing cmdlet at the last
# step -- after the build, after the zip, with nothing to show for it -- is the worst possible place
# to discover that. SHA256.Create() is in every .NET Framework 4 install, so this has one fewer
# thing to go wrong.
function Get-Sha256 {
    param([string] $Path)
    $stream = [System.IO.File]::OpenRead($Path)
    try {
        $hasher = [System.Security.Cryptography.SHA256]::Create()
        try {
            # Lowercase, because that is the form `sha256sum -c` and every lockfile in existence use.
            $hex = ([System.BitConverter]::ToString($hasher.ComputeHash($stream))) -replace '-', ''
            return $hex.ToLower()
        }
        finally {
            $hasher.Clear()
        }
    }
    finally {
        $stream.Dispose()
    }
}

function Write-Line {
    param([string] $Text, [string] $Color = 'Gray')
    Write-Host $Text -ForegroundColor $Color
}

# `[workspace.package] version = "0.1.0"`, read as a *section* and not as a global regex: a bare
# `^version =` match would also catch a dependency's version and hand the payload somebody else's
# number.
function Get-CargoWorkspaceVersion {
    param([string] $Path)
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    $inPackageTable = $false
    foreach ($line in (Get-Content -LiteralPath $Path)) {
        $text = $line.Trim()
        if ($text.StartsWith('[')) {
            $inPackageTable = ($text -eq '[workspace.package]')
            continue
        }
        if (-not $inPackageTable) { continue }
        if ($text -match '^version\s*=\s*"([^"]+)"') { return $matches[1] }
    }
    return $null
}

function Get-PythonAppVersion {
    param([string] $Path)
    # Kept as a probe, not as a comparison. It used to read the deleted Python app's __version__ out
    # of windrecorder\__init__.py so a mismatch against the workspace version could be printed; that
    # file was removed in 3f37cbf, so on a tree that matches this branch it always returns $null. If
    # it ever returns a version again, something is overlaying a Python app back onto this install and
    # the second number is worth seeing -- but this payload no longer claims it, ships it, or defers
    # to it.
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    foreach ($line in (Get-Content -LiteralPath $Path)) {
        if ($line -match '^\s*__version__\s*=\s*["'']([^"'']+)["'']') { return $matches[1] }
    }
    return $null
}

# Remove a staging path, refusing to touch anything outside windcap\dist. The whole family of scripts
# here follows one rule: the cheapest way to lose a person's work is a stale variable in front of
# Remove-Item.
function Remove-DistPath {
    param([string] $Path)
    if (-not $Path) { return }
    if (-not (Test-Path -LiteralPath $Path)) { return }
    $allowed = (Resolve-Path -LiteralPath $distDir).Path.TrimEnd('\')
    $target = (Resolve-Path -LiteralPath $Path).Path
    if (-not $target.StartsWith($allowed + '\', [System.StringComparison]::OrdinalIgnoreCase)) {
        throw ('refusing to remove ' + $target + ': not under ' + $allowed)
    }
    Remove-Item -LiteralPath $target -Recurse -Force
}

# ---------------------------------------------------------------------------
# 1. Version, from the one place it is declared.
# ---------------------------------------------------------------------------
$cargoToml = Join-Path $workspace 'Cargo.toml'
$version = Get-CargoWorkspaceVersion -Path $cargoToml
if (-not $version) {
    Write-Line ('release.ps1: no [workspace.package] version found in ' + $cargoToml) 'Red'
    exit 1
}
$pythonVersion = Get-PythonAppVersion -Path (Join-Path $root 'windrecorder\__init__.py')

$stagingDir = Join-Path $distDir ('Windrecorder-native-' + $version)
$zipPath = Join-Path $distDir ('Windrecorder-native-' + $version + '.zip')
$hashPath = $zipPath + '.sha256'
$notesPath = Join-Path $stagingDir 'RELEASE-NOTES.txt'

Write-Line ''
Write-Line 'windcap native release' 'Cyan'
Write-Line ('  version     v' + $version + '   (from [workspace.package] in windcap\Cargo.toml)')
if ($pythonVersion -and ($pythonVersion -eq $version)) {
    # Coincidence, not agreement: nothing compares these any more, because the app that carried the
    # second number is gone. Say so rather than printing "-- agrees" and implying a check ran.
    Write-Line ('  note       a Python app v' + $pythonVersion + ' is present in this tree (windrecorder\__init__.py)') 'Yellow'
    Write-Line '               It is not in the zip, does not version it, and no longer has any say' 'Yellow'
    Write-Line '               over what these binaries do. The version that ships is the one above.' 'Yellow'
}
elseif ($pythonVersion) {
    Write-Line ('  note       a Python app v' + $pythonVersion + ' is present in this tree (windrecorder\__init__.py)') 'Yellow'
    Write-Line '               It is not in the zip and does not version it: this payload is the Rust' 'Yellow'
    Write-Line '               workspace alone, and the number above is Windrecorder''s version.' 'Yellow'
}
Write-Line ('  profile     ' + $Profile)
Write-Line ('  staging     ' + $stagingDir)
Write-Line ('  zip         ' + $zipPath)
Write-Line ''

# ---------------------------------------------------------------------------
# 2. Build. build.ps1 owns cargo, and its own per-artefact table is this script's output.
# ---------------------------------------------------------------------------
if (-not (Test-Path -LiteralPath $buildScript)) {
    Write-Line ('release.ps1: ' + $buildScript + ' is missing; there is nothing to build with.') 'Red'
    exit 1
}
if (-not (Test-Path -LiteralPath $distDir)) {
    New-Item -ItemType Directory -Path $distDir -Force | Out-Null
}
Remove-DistPath -Path $manifestPath

$buildArgs = @(
    '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $buildScript,
    '-Profile', $Profile, '-Manifest', $manifestPath
)
if ($SkipUi) { $buildArgs += '-SkipUi' }
$hostExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
# The child writes straight to this console, colours and all: capturing its table to reprint it would
# lose the only part of the build report that is formatted for a person.
& $hostExe @buildArgs
$buildExit = $LASTEXITCODE
Write-Line ''
Write-Line ('  build.ps1 exited ' + $buildExit + ' -- 0 means every artefact it was asked for was built; this script carries on either way') 'DarkGray'

# Fields are positional in a fixed order; `source` and `note` are last so a future column cannot
# shift the numbers a release depends on.
$buildRows = @()
if (Test-Path -LiteralPath $manifestPath) {
    $buildRows = @(
        Get-Content -LiteralPath $manifestPath -Encoding UTF8 |
            Where-Object { $_ -and $_.Trim() } |
            ConvertFrom-Csv -Delimiter "`t" -Header @('name', 'status', 'bytes', 'seconds', 'profile', 'total', 'source', 'note')
    )
}
else {
    Write-Line '  no build manifest: build.ps1 never reached its summary (no cargo, or it was replaced mid-run).' 'Yellow'
}
$totalSeconds = ''
if ($buildRows.Count -gt 0 -and $buildRows[0].total) { $totalSeconds = [string] $buildRows[0].total }

function Get-BuildRow {
    param([string] $Name)
    foreach ($row in $buildRows) {
        if ($row.name -eq $Name) { return $row }
    }
    return $null
}

# ---------------------------------------------------------------------------
# 3. The payload's shape -- and the DLL's placement is the interesting part.
#
# supervisor\src\native.rs's candidate_dirs() gives executables five places and <install>\bin\ is
# second, behind only %WINDCAP_HOME%; base\src\install.rs is the rule that decides the directory they
# are resolved against is an install at all (config_src\config_default.json, or userdata\). windcap.dll
# goes into bin\ as the first and only place it is staged -- the search order this layout was built
# around outlived the Python bridge that first read it, and it is still the order build.ps1 mirrors and
# smoke.ps1 proves. Both orders assume an *installed* layout, and on a machine that received a zip
# rather than built a tree there is no windcap\target\release\ at all -- so bin\ is the only directory
# either of them can find anything in, and it is the one directory the DLL is allowed to live in.
# Nothing is renamed here; the file cargo wrote is the file that ships.
# ---------------------------------------------------------------------------
$payload = @(
    [pscustomobject]@{
        Name     = 'windrec.exe'
        Target   = 'bin\windrec.exe'
        Required = $true
        Role     = 'the recorder: windsvc (the tray) supervises it as `windrec loop --root <root>`'
    },
    [pscustomobject]@{
        Name     = 'windcap.dll'
        Target   = 'bin\windcap.dll'
        Required = $true
        Role     = 'session-state probes behind a C ABI, exported by core\src\ffi_c.rs (cdylib)'
    },
    # No `windui.exe` row. The egui front end was retired from the shipped set by
    # docs\adr\2026-09-27-winduiweb-is-the-only-interface.md: the crate is still built and still tested
    # by build.ps1 (it is the only implementation of the frame door, the prompt panel and the form field
    # declarations, and the web window is a second consumer of that same code), but its binary is no
    # longer staged, no longer reported by `windsvc doctor`, and no longer something a released install
    # is missing if it is absent. Adding a row here would put it back in the product.
    [pscustomobject]@{
        Name     = 'winduiweb.exe'
        Target   = 'bin\winduiweb.exe'
        Required = $false
        Role     = 'the HTML front end: the same six screens over the same query layer; this is the window the tray opens, and the only interface this product ships'
    },
    [pscustomobject]@{
        Name     = 'Windrecorder.exe'
        Target   = 'bin\Windrecorder.exe'
        Required = $false
        Role     = 'the file you double-click: it starts bin\windsvc.exe and nothing else'
    },
    [pscustomobject]@{
        Name     = 'windsvc.exe'
        Target   = 'bin\windsvc.exe'
        Required = $false
        Role     = 'the tray and supervisor; without it nothing starts the recorder for the user'
    },
    [pscustomobject]@{
        Name     = 'windmcp.exe'
        Target   = 'bin\windmcp.exe'
        Required = $false
        Role     = 'the HTTP MCP bridge; without it the screen memory is not reachable by AI assistants'
    },
    [pscustomobject]@{
        Name     = 'wind-reindex.exe'
        Target   = 'bin\wind-reindex.exe'
        Required = $false
        Role     = 'indexes already-recorded video; without it only new recordings become searchable'
    },
    [pscustomobject]@{
        Name     = 'windnotes.exe'
        Target   = 'bin\windnotes.exe'
        Required = $false
        Role     = "the user's own bookmarks: flag/note store, capture, marker geometry"
    },
    [pscustomobject]@{
        Name     = 'windsetup.exe'
        Target   = 'bin\windsetup.exe'
        Required = $false
        Role     = 'first-run layout, engine probing, and the re-entrant migration over existing months'
    },
    [pscustomobject]@{
        Name     = 'windai.exe'
        Target   = 'bin\windai.exe'
        Required = $false
        Role     = 'natural-language search and monthly activity tags'
    },
    [pscustomobject]@{
        Name     = 'windmaint.exe'
        Target   = 'bin\windmaint.exe'
        Required = $false
        Role     = 'idle maintenance as a command: convert / refresh / expire / backup / doctor'
    },
    [pscustomobject]@{
        Name     = 'windcapctl.exe'
        Target   = 'bin\windcapctl.exe'
        Required = $false
        Role     = 'terminal search and capture benchmarks; a human tool, nothing at runtime needs it'
    }
)
foreach ($item in $payload) {
    # The windmaint crate is `wind-maint` and cargo has written both names under target\ over time;
    # build.ps1 resolves that and records the winner in its manifest, so take the path from there
    # rather than re-deciding here and staging the stale one.
    $row = Get-BuildRow -Name $item.Name
    $source = $null
    if ($row -and $row.source) { $source = $row.source }
    if (-not $source -or -not (Test-Path -LiteralPath $source)) {
        $source = Join-Path $profileDir $item.Name
    }
    $item | Add-Member -NotePropertyName Source -NotePropertyValue $source
    $item | Add-Member -NotePropertyName Row -NotePropertyValue $row
}

# ---------------------------------------------------------------------------
# 4. Stage. Wiped and rebuilt from nothing, so a crate that stopped compiling cannot leave its last
#    good binary behind pretending to be in this release.
# ---------------------------------------------------------------------------
Remove-DistPath -Path $stagingDir
Remove-DistPath -Path $zipPath
Remove-DistPath -Path $hashPath
New-Item -ItemType Directory -Path (Join-Path $stagingDir 'bin') -Force | Out-Null

$staged = @()
$skipped = @()
$missingRequired = @()
foreach ($item in $payload) {
    $status = 'not reported'
    if ($item.Row) { $status = [string] $item.Row.status }
    $present = Test-Path -LiteralPath $item.Source
    # BUILT is required, not merely a file lying in target\. build.ps1 exits 0 when there is no cargo
    # at all -- and then every artefact from somebody's last compile is still sitting right there, so
    # a presence test would stage it, stamp this run's date on it and hand out a hash for bytes this
    # release never built.
    if ($status -ne 'BUILT' -or -not $present) {
        $why = 'nothing reported for it by build.ps1'
        if ($status -eq 'SKIPPED') { $why = '-SkipUi' }
        elseif ($status -eq 'not reported') { $why = 'no build manifest: build.ps1 never reached its summary (no Rust toolchain, or it failed to start)' }
        elseif ($item.Row -and $item.Row.note) { $why = [string] $item.Row.note }
        elseif ($status -eq 'BUILT') { $why = 'build.ps1 reported ' + $item.Name + ' BUILT but ' + $item.Source + ' is not there' }
        $skipped += ($item.Target + '  [' + $status + '] ' + $why)
        if ($item.Required) { $missingRequired += $item.Name }
        Write-Line ('  SKIP     ' + (Format-Target -Target $item.Target) + '-- ' + $why) 'Red'
        continue
    }
    $destination = Join-Path $stagingDir $item.Target
    Copy-Item -LiteralPath $item.Source -Destination $destination -Force
    $bytes = (Get-Item -LiteralPath $destination).Length
    $seconds = '--'
    if ($item.Row) { $seconds = [string] $item.Row.seconds }
    $staged += [pscustomobject]@{
        Target  = $item.Target
        Bytes   = $bytes
        Size    = Format-Size -Bytes $bytes
        Seconds = $seconds
        Role    = $item.Role
        Sha256  = (Get-Sha256 -Path $destination)
    }
    Write-Line ('  STAGED   ' + (Format-Target -Target $item.Target) + ('{0,11}' -f (Format-Size -Bytes $bytes)) + ('{0,8}' -f ($seconds + 's'))) 'Green'
}

# The OCR engine the recorder shells out to. Shipped when it is in the tree, because windrec looks
# for <install>\ocr_lib\Windows.Media.Ocr.Cli.exe and its absence means frames recorded with no text
# -- silently, which is the worst way for a search index to fail.
$ocrSource = Join-Path $root 'ocr_lib\Windows.Media.Ocr.Cli.exe'
if (Test-Path -LiteralPath $ocrSource) {
    New-Item -ItemType Directory -Path (Join-Path $stagingDir 'ocr_lib') -Force | Out-Null
    $ocrTarget = Join-Path $stagingDir 'ocr_lib\Windows.Media.Ocr.Cli.exe'
    Copy-Item -LiteralPath $ocrSource -Destination $ocrTarget -Force
    $ocrBytes = (Get-Item -LiteralPath $ocrTarget).Length
    $staged += [pscustomobject]@{
        Target  = 'ocr_lib\Windows.Media.Ocr.Cli.exe'
        Bytes   = $ocrBytes
        Size    = Format-Size -Bytes $ocrBytes
        Seconds = 'copied, not built'
        Role    = 'the Windows OCR command line windrec invokes; taken from the install tree, not from cargo'
        Sha256  = (Get-Sha256 -Path $ocrTarget)
    }
    Write-Line ('  STAGED   ' + (Format-Target -Target 'ocr_lib\Windows.Media.Ocr.Cli.exe') + ('{0,11}' -f (Format-Size -Bytes $ocrBytes)) + ('{0,8}' -f '-')) 'Green'
}
else {
    $skipped += 'ocr_lib\Windows.Media.Ocr.Cli.exe  [not in the tree] the recorder will index nothing searchable'
    Write-Line '  SKIP     ocr_lib\Windows.Media.Ocr.Cli.exe -- not present under <install>\ocr_lib\' 'Yellow'
}

# ---------------------------------------------------------------------------
# The shipped settings: the difference between a pile of binaries and an installable package.
#
# base\src\install.rs makes config_src\config_default.json the *definition* of an install root --
# that is what `resolve_root_from_exe` walks up looking for, and what `windsetup init` seeds
# userdata\config_user.json from. Measured on this machine, twice, against a directory holding
# nothing but this payload's bin\ and ocr_lib\ and no config_src\:
#
#   * with the binaries built before 9d69874, `init` laid its 16 slots under bin\ and then died:
#         error: ...\bin\windrecorder/config_src/config_default.json is missing; there is nothing
#         to seed from
#   * with today's binaries it no longer dies -- the compiled-in defaults catch it -- but it still
#     resolves its root to bin\ and lays the whole writable layout under bin\userdata, reporting
#     "seeded from the defaults compiled into this binary", and `windsvc doctor --root .` refuses
#     the folder outright: "is not a Windrecorder install -- it carries no config_default.json".
#
# So an install built from a zip without this directory is real but mislaid, split in two, and has
# no settings layer for the next upgrade to reconcile against. That is why it ships.
#
# Everything under config_src\ ships, recursed, and the thirteen entries it is specified to carry
# are then checked off by name. Recursion means a setting added to the directory ships on its own;
# the name check means a setting that *disappeared* from it is a loud release failure rather than a
# product with one quietly missing feature -- which is the characteristic failure mode of this
# particular directory, because every one of these files belongs to exactly one subsystem.
# ---------------------------------------------------------------------------
$configSource = Join-Path $root 'config_src'
# Who reads each entry, and what the install loses without it. `synonyms` is a directory.
$configSrcEntries = @(
    [pscustomobject]@{
        Name    = 'config_default.json'
        Role    = 'the factory settings: what userdata\config_user.json is seeded from, and the file whose presence makes a directory an install root'
        Missing = 'windsetup init lays its layout under bin\ instead of the install, and windsvc doctor refuses the folder as not an install'
    },
    [pscustomobject]@{
        Name    = 'languages.json'
        Role    = "the tray menu's strings, in every language the app ships, read by base\src\i18n.rs"
        Missing = 'windsvc has no menu text in any language and answers only in its built-in English keys'
    },
    [pscustomobject]@{
        Name    = 'similar_CN_characters.txt'
        Role    = 'the Chinese fuzzy-glyph table, read by cli\src\library.rs and ai\src\library.rs'
        Missing = 'Chinese search stops forgiving a lookalike character, so a mistyped name finds nothing'
    },
    [pscustomobject]@{
        Name    = 'wordcloud_stopword.txt'
        Role    = 'the stopword list the activity wordcloud is built against'
        Missing = 'the wordcloud is drowned in the words that appear on every screen on this machine'
    },
    [pscustomobject]@{
        Name    = 'record_preset.json'
        Role    = 'the record/convert preset table windmaint substitutes into its command line'
        Missing = 'windmaint convert has no preset to fill in, so it has no way to encode a video'
    },
    [pscustomobject]@{
        Name    = 'video_compress_preset.json'
        Role    = 'the encoder and accelerator capability table windmaint convert picks its codec from'
        Missing = 'windmaint convert cannot tell which encoder this machine has, so it converts nothing'
    },
    [pscustomobject]@{
        Name    = 'about_en.md'
        Role    = 'the About text, in English -- shipped with config_src\, rendered by no window yet'
        Missing = 'nothing reads it today, so this is a packaging note rather than a failure'
    },
    [pscustomobject]@{
        Name    = 'about_sc.md'
        Role    = 'the About text, in Simplified Chinese -- shipped with config_src\, rendered by no window yet'
        Missing = 'nothing reads it today, so this is a packaging note rather than a failure'
    },
    [pscustomobject]@{
        Name    = 'about_ja.md'
        Role    = 'the About text, in Japanese -- shipped with config_src\, rendered by no window yet'
        Missing = 'nothing reads it today, so this is a packaging note rather than a failure'
    },
    [pscustomobject]@{
        Name    = 'onboarding_en.md'
        Role    = 'the first-run walkthrough, in English -- windsetup doctor reads it to tell an empty install from a broken one'
        Missing = 'a user with nothing in the index is pointed at no next step'
    },
    [pscustomobject]@{
        Name    = 'onboarding_sc.md'
        Role    = 'the first-run walkthrough, in Simplified Chinese'
        Missing = 'a user with nothing in the index is pointed at no next step'
    },
    [pscustomobject]@{
        Name    = 'onboarding_ja.md'
        Role    = 'the first-run walkthrough, in Japanese'
        Missing = 'a user with nothing in the index is pointed at no next step'
    },
    [pscustomobject]@{
        Name    = 'synonyms'
        Role    = 'the search-recommendation vocabularies and their embedding indices, one pair per language, and the only large thing the settings directory holds'
        Missing = 'the search box recommends nothing, and languages.json points the user at a directory not in the zip'
    }
)
if (Test-Path -LiteralPath $configSource) {
    New-Item -ItemType Directory -Path (Join-Path $stagingDir 'config_src') -Force | Out-Null
    $configPrefix = (Resolve-Path -LiteralPath $configSource).Path.TrimEnd('\')
    foreach ($sourceFile in @(Get-ChildItem -LiteralPath $configPrefix -File -Recurse | Sort-Object FullName)) {
        $relative = $sourceFile.FullName.Substring($configPrefix.Length + 1)
        $configTarget = Join-Path $stagingDir (Join-Path 'config_src' $relative)
        $configParent = Split-Path -Parent $configTarget
        if (-not (Test-Path -LiteralPath $configParent)) {
            New-Item -ItemType Directory -Path $configParent -Force | Out-Null
        }
        Copy-Item -LiteralPath $sourceFile.FullName -Destination $configTarget -Force
        $configBytes = (Get-Item -LiteralPath $configTarget).Length
        # One Role covers a whole directory's contents: a setting is described by the subsystem it
        # belongs to rather than by its own name, and every file under synonyms\ belongs to one.
        $entry = $configSrcEntries | Where-Object { $_.Name -eq (($relative -split '\\')[0]) } | Select-Object -First 1
        $role = 'a shipped setting, taken from the install tree rather than from cargo'
        if ($entry) { $role = $entry.Role }
        $staged += [pscustomobject]@{
            Target  = 'config_src\' + $relative
            Bytes   = $configBytes
            Size    = Format-Size -Bytes $configBytes
            Seconds = 'copied, not built'
            Role    = $role
            Sha256  = (Get-Sha256 -Path $configTarget)
        }
        Write-Line ('  STAGED   ' + (Format-Target -Target ('config_src\' + $relative)) + ('{0,11}' -f (Format-Size -Bytes $configBytes)) + ('{0,8}' -f '-')) 'Green'
    }
    # Copied, and then checked: the loop above ships whatever is there, this one insists on the
    # thirteen things that are supposed to be there.
    foreach ($entry in $configSrcEntries) {
        $stagedPath = Join-Path $stagingDir (Join-Path 'config_src' $entry.Name)
        if (Test-Path -LiteralPath $stagedPath) { continue }
        $skipped += ('config_src\' + $entry.Name + '  [not in the tree] ' + $entry.Missing)
        Write-Line ('  SKIP     ' + (Format-Target -Target ('config_src\' + $entry.Name)) + '-- ' + $entry.Missing) 'Yellow'
    }
}
else {
    $skipped += ('config_src\  [not in the tree] ' + 'THIS ZIP CANNOT BE INSTALLED WHERE IT IS PUT. base\src\install.rs ' +
            'defines an install root as the directory carrying config_src\config_default.json, so with this directory ' +
            'absent windsetup init resolves its own root into bin\ and lays all 16 slots under bin\userdata, and ' +
            'windsvc doctor refuses the folder as "not a Windrecorder install". The user config still comes out of the ' +
            'defaults compiled into the binaries, so the damage is invisible at first -- and then there is no settings ' +
            'layer for the next upgrade to reconcile against, and the four things the native code opens at runtime (the ' +
            'tray''s menu strings, the Chinese fuzzy-glyph table and windmaint''s two preset tables) are not in the ' +
            'product at all.')
    Write-Line '  SKIP     config_src\ -- NOT IN THE TREE. Without it the zip installs itself in the wrong place:' 'Red'
    Write-Line '            base\src\install.rs defines the install root as the directory carrying this one, so' 'Red'
    Write-Line '            windsetup init resolves its root into bin\ and lays all 16 slots under bin\userdata, and' 'Red'
    Write-Line '            windsvc doctor refuses the folder as not an install. The four settings the binaries open' 'Red'
    Write-Line '            at runtime (menu strings, the Chinese fuzzy-glyph table, the two preset tables) are gone.' 'Red'
}

# ---------------------------------------------------------------------------
# The OCR self-check fixtures.
#
# `windsetup check-engines` answers "is my OCR working?" by running the shipped engine against real
# page images and their word lists: __assets__\OCR_test_1080_<lang>.png paired with
# __assets__\OCR_test_1080_words_<lang>.txt. Without those pairs the check cannot exercise any engine
# and -- since the fix that separates "tested and failed" from "never tested" -- it exits 3 with
# "NOT TESTED -- no OCR fixtures". That is now a *correct* answer, but it is useless to a person
# holding only this zip: a standalone install could never self-check OCR at all, because nothing put
# the fixtures there. They ship now.
#
# The FULL __assets__\ directory is emphatically NOT shipped. It is mostly README header art, the
# Productpost-*.jpg and product-preview-*.jpg marketing screenshots, .otf fonts, contribution
# markdown and a 2.7 MiB test video -- none of which any binary reads at runtime. Selection is by
# name (the two OCR_test_1080_ prefixes), never by recursion, so a stray asset is simply not a
# candidate. This is the opposite of config_src\, where every file is a setting something reads.
#
# The tray icons are deliberately absent too: since commit 044f170 they are compiled into windsvc.exe
# as Windows resources, so icon-tray*.png / icon-tray.ico are no longer a runtime dependency and are
# not staged. Windrecorder.exe carries a compiled copy of the same art for the same reason -- the file
# a stranger sees in Explorer has to show something, and a picture beside the .exe is a picture that
# can go missing.
# ---------------------------------------------------------------------------
$assetsSource = Join-Path $root '__assets__'
# The languages the self-check is expected to be able to run, each as an image/word-list pair.
$fixtureLanguages = @('en-US', 'ja-jp', 'zh-Hans-CN')
$fixtureImagePrefix = 'OCR_test_1080_'
$fixtureWordsPrefix = 'OCR_test_1080_words_'
if (Test-Path -LiteralPath $assetsSource) {
    $assetsPrefix = (Resolve-Path -LiteralPath $assetsSource).Path.TrimEnd('\')
    # Selected by name. The glob is the whole point: only fixture pairs match `OCR_test_1080_*.png` or
    # `OCR_test_1080_words_*.txt`, so a README jpg or a .otf dropped into __assets__ is never considered.
    $fixtureFiles = @(Get-ChildItem -LiteralPath $assetsPrefix -File | Where-Object {
            $_.Name -like ($fixtureImagePrefix + '*.png') -or $_.Name -like ($fixtureWordsPrefix + '*.txt')
        } | Sort-Object Name)
    if ($fixtureFiles.Count -gt 0) {
        New-Item -ItemType Directory -Path (Join-Path $stagingDir '__assets__') -Force | Out-Null
        foreach ($fixture in $fixtureFiles) {
            $assetsTarget = Join-Path $stagingDir (Join-Path '__assets__' $fixture.Name)
            Copy-Item -LiteralPath $fixture.FullName -Destination $assetsTarget -Force
            $fixtureBytes = (Get-Item -LiteralPath $assetsTarget).Length
            $staged += [pscustomobject]@{
                Target  = '__assets__\' + $fixture.Name
                Bytes   = $fixtureBytes
                Size    = Format-Size -Bytes $fixtureBytes
                Seconds = 'copied, not built'
                Role    = 'an OCR self-check fixture, read by `windsetup check-engines` to answer whether this machine can OCR'
                Sha256  = (Get-Sha256 -Path $assetsTarget)
            }
            Write-Line ('  STAGED   ' + (Format-Target -Target ('__assets__\' + $fixture.Name)) + ('{0,11}' -f (Format-Size -Bytes $fixtureBytes)) + ('{0,8}' -f '-')) 'Green'
        }
    }
    # Copied by glob, then checked by name: a language whose pair is incomplete means the check silently
    # exercises one language fewer -- the same quiet-loss failure mode config_src\ guards against.
    foreach ($lang in $fixtureLanguages) {
        foreach ($pairName in @(($fixtureImagePrefix + $lang + '.png'), ($fixtureWordsPrefix + $lang + '.txt'))) {
            $stagedPair = Join-Path $stagingDir (Join-Path '__assets__' $pairName)
            if (Test-Path -LiteralPath $stagedPair) { continue }
            $skipped += ('__assets__\' + $pairName + '  [not in the tree] `windsetup check-engines` cannot test ' + $lang + ' on a standalone install')
            Write-Line ('  SKIP     ' + (Format-Target -Target ('__assets__\' + $pairName)) + '-- the OCR self-check cannot test ' + $lang + ' without it') 'Yellow'
        }
    }
}
else {
    $skipped += '__assets__\  [not in the tree] `windsetup check-engines` has no fixtures to run and can only answer NOT TESTED (exit 3) on a standalone install'
    Write-Line '  SKIP     __assets__\ -- NOT IN THE TREE: this payload cannot self-check OCR at all' 'Yellow'
}

# ---------------------------------------------------------------------------
# 5. The notes file: what a person holding the zip needs to know *before* holding its contents.
# ---------------------------------------------------------------------------
$changes = @()
$gitNote = ''
# cmd /c, as in build.ps1: PowerShell 5.1 wraps every native stderr line in an ErrorRecord, and git
# writes perfectly ordinary progress to stderr.
#
# The console encoding swap is the same one smoke.ps1 makes for the same reason, and here the stakes
# are higher because the result is *shipped*: git stores commit messages as UTF-8, PowerShell 5.1
# decodes a native command's stdout with the console codepage, and on this box that codepage is
# gb2312. Every em dash and every Chinese character in `main..HEAD` therefore arrived as mojibake and
# got written into RELEASE-NOTES.txt that way -- the one section of the payload whose entire job is to
# tell a user what changed, silently garbled for exactly the commits worth reading. Restored after,
# because this console may be the user's own and is not ours to leave in another codepage.
$savedGitEncoding = $null
try {
    $savedGitEncoding = [Console]::OutputEncoding
    [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
}
catch {
    Write-Line '  release.ps1: could not set the console to UTF-8; the changelog section may be mangled' 'Yellow'
}
$gitLines = @(& cmd.exe /c 'git log --oneline --no-decorate main..HEAD 2>&1')
$gitCode = $LASTEXITCODE
if ($savedGitEncoding) {
    try { [Console]::OutputEncoding = $savedGitEncoding } catch { }
}
if ($gitCode -eq 0 -and $gitLines) {
    $changes = @($gitLines | Where-Object { $_ -and ($_ -match '^[0-9a-f]{7,40}\s') })
}
if ($changes.Count -eq 0) {
    $gitNote = '  (no `git log --oneline main..HEAD` output; git exit ' + $gitCode + ' -- a shallow clone, or no main?)'
}

$notes = @()
$notes += 'Windrecorder native engine v' + $version
$notes += 'built ' + (Get-Date -Format 's') + '   profile: ' + $Profile
if ($Profile -ne 'release') {
    $notes += 'WARNING: this is a DEBUG build. It is roughly an order of magnitude slower than a'
    $notes += 'release one and is not what anyone should be shipped. The tray labels a debug build'
    $notes += 'wherever it finds one -- supervisor\src\native.rs::describe_build(), printed by'
    $notes += '`windsvc doctor` in the build column; so does this file.'
}
if ($totalSeconds) { $notes += 'total build time: ' + $totalSeconds + ' s (per artefact below)' }
$notes += ''
$notes += '=============================================================================='
$notes += 'A STANDALONE PACKAGE: UNZIP IT INTO AN EMPTY DIRECTORY'
$notes += '=============================================================================='
$notes += ''
$notes += 'Everything in here is a compiled Windows binary or a data file one of them reads. There is'
$notes += 'no Python in the zip and it needs no Python outside it, and no existing Windrecorder install'
$notes += 'either: unpacked into an empty folder, that folder IS the install. Nothing about where it'
$notes += 'lives is baked in, so the package can be called anything and sit anywhere.'
$notes += ''
$notes += '  1. unzip into an empty directory. You get bin\, config_src\, ocr_lib\, __assets__\ (the'
$notes += '     OCR self-check fixtures) and this file;'
$notes += '  2. double-click bin\Windrecorder.exe -- the one file in this zip that is called'
$notes += '     Windrecorder, and the one to click. It has one job: start bin\windsvc.exe, the'
$notes += '     tray, which does the work. Finding no userdata\config_user.json, the tray runs'
$notes += '     `bin\windsetup.exe init` itself before it does anything else: the 16 writable'
$notes += '     slots (userdata\ -- the month index, the videos, the embedding index, the result_*'
$notes += '     folders the UI writes -- and cache\ -- locks, logs, the window-title side channel,'
$notes += '     i-frames), then seeds userdata\config_user.json from the settings in config_src\ that'
$notes += '     arrived in this zip. Then the icon appears and recording starts. No --root anywhere:'
$notes += '     every binary in this zip finds its own.'
$notes += '  3. bin\windrec.exe run --seconds 60'
$notes += '                                     -- record one minute, then report what it captured.'
$notes += '                                     Again with no --root: this one finds it too.'
$notes += ''
$notes += 'Step 2 is one click, and it is not a formality. config_src\ is the directory the'
$notes += 'product defines an install by (base\src\install.rs): it is what the binaries walk up looking'
$notes += 'for, and what a first run is seeded from. It ships here so that an empty folder becomes an'
$notes += 'install where you put it, with nobody else''s checkout already in it. Without it -- measured,'
$notes += 'with the compiled-in defaults catching the seed but nothing catching the root -- init lays'
$notes += 'the whole layout under bin\userdata and `windsvc doctor` refuses the folder. `init` is'
$notes += 're-entrant either way: an existing config_user.json is left alone, so running it again after'
$notes += 'an update -- or letting the tray run it again -- is safe.'
$notes += ''
$notes += 'WHAT YOU GET TODAY, EXACTLY. Recording, indexing, searching, maintenance and the health'
$notes += 'reports all work out of the bin\ in this zip with nothing installed underneath them, and the'
$notes += 'first double-click builds the tree the rest of them need. All eleven binaries now find their'
$notes += 'own --root by the shared rule, so the commands below need no path on the command line;'
$notes += '--root . is still accepted, still means this folder, and still beats the walk-up if you pass'
$notes += 'it. What the package does NOT have is an installer in the Windows sense: nothing writes a'
$notes += 'registry entry, there is no Start Menu shortcut and no uninstaller, and overwriting this'
$notes += 'folder is the only upgrade path.'
$notes += ''
$notes += '  * THE TRAY LAUNCHES BINARIES AND NOTHING ELSE. `windsvc` starts recording with'
$notes += '    `bin\windrec.exe loop --root <install>` and opens the window with `bin\winduiweb.exe'
$notes += '    --root <install>` -- the HTML front end, and the only interface this zip carries. The'
$notes += '    egui `bin\windui.exe` is retired from the shipped set: its crate is still built and still'
$notes += '    tested by the script that produced these binaries, because it is the only implementation of'
$notes += '    the frame door, the prompt panel and the form field declarations, but the .exe is no longer'
$notes += '    staged into bin\, and `windsvc doctor` no longer counts it as part of an install''s'
$notes += '    integrity. The tray opens exactly one window, by'
$notes += '    `supervisor\src\native.rs::INTERFACE`, and does not fall back to another when it is'
$notes += '    missing, because a window that silently opened a different front end would hide the fact'
$notes += '    that the one you asked for was not in the install. There is no third implementation of'
$notes += '    either job behind them: commit'
$notes += '    3f37cbf deleted the Python application, 2b6f318 deleted the tray''s fallback to it,'
$notes += '    and fc288cb removed the switch from the Settings page. So the `"use_native_core"` key'
$notes += '    that once chose between the two is real but read by nothing: if your'
$notes += '    userdata\config_user.json still carries it from an older install, it is inert, and'
$notes += '    setting it either way changes no behaviour and needs no restart to "take effect".'
$notes += '    It is deliberately not a step in this list, because following a step that does'
$notes += '    nothing is how you end up believing a working install is a broken one.'
$notes += '  * A MISSING BINARY IS NAMED, NOT MASKED. An absent windrec.exe or winduiweb.exe is now the'
$notes += '    only failure the launch path has, and the tray reports which .exe is missing and every'
$notes += '    directory it searched, instead of starting an interpreter with no script to run. Ask'
$notes += '    the same question without clicking anything with `bin\Windrecorder.exe doctor`, below.'
$notes += '  * The OCR self-check fixtures NOW SHIP: a selective __assets__\ with the three'
$notes += '    OCR_test_1080_<lang>.png images (en-US, ja-jp, zh-Hans-CN) and their three'
$notes += '    OCR_test_1080_words_<lang>.txt word lists. With them, `bin\windsetup.exe check-engines'
$notes += '    --root .` can actually run the engine on this install rather than only guessing. Without'
$notes += '    a fixture directory it no longer cries "no OCR engine is usable": check-engines keeps'
$notes += '    three outcomes apart and exits 0 (a fixture was read above threshold), 1 (a fixture was'
$notes += '    read and nothing passed), or 3 (NOT TESTED -- no fixtures present at all). Only exit 1'
$notes += '    means the engine failed a real test. Only the fixture pairs are staged from __assets__\;'
$notes += '    its README header art, Productpost-*/product-preview-*/how-it-work-* screenshots, .otf'
$notes += '    fonts, contribution markdown and the 2.7 MiB test video are deliberately NOT in this zip.'
$notes += '    And the tray no longer needs __assets__: since commit 044f170 the icon is compiled into'
$notes += '    windsvc.exe as a Windows resource, so icon-tray.png, icon-tray-pause.png and'
$notes += '    icon-tray.ico are not staged at all.'
$notes += ''
$notes += 'Ask the install what it is, and what it would do. The --root . on these is explicit by'
$notes += 'choice, so the answer is about the folder you are standing in; none of them needs it.'
$notes += '    bin\windsetup.exe doctor --root .   -- layout, config layer, month files, locks'
$notes += '    bin\windrec.exe status --root .     -- the index, in rows and months'
$notes += '    bin\windrec.exe doctor --root .     -- the engine verdict, and the screen mask in rows'
$notes += '                                       and columns: what the OCR copy will not be shown'
$notes += '    bin\windcapctl.exe query --root . --day <YYYY-MM-DD>'
$notes += '    bin\windmaint.exe forget --root . --day <YYYY-MM-DD> --dry-run'
$notes += '                                       -- the rows that period would erase; drop --dry-run'
$notes += '                                       to erase them, keeping the footage'
$notes += '    bin\Windrecorder.exe doctor --root .'
$notes += '                                       -- what each tray menu item would do right now.'
$notes += '                                          Windrecorder.exe is the launcher and hands'
$notes += '                                          every argument to bin\windsvc.exe, so this is'
$notes += '                                          byte for byte the tray''s own report; the launcher'
$notes += '                                          answers only --version about itself.'
$notes += ''
$notes += 'That last one is the command that answers "what would pressing Record actually run". It'
$notes += 'prints the exact command line recording would start and the exact one opening a window'
$notes += 'would start, each with the file it resolved to and whether that file is a release, a debug'
$notes += 'or an installed build -- and, naming the file, which binary is missing if one is.'
$notes += ''
$notes += 'This is the replacement for `python -m windrecorder.native_runtime status`, which used to'
$notes += 'be the answer here. That module was deleted along with the Python application it belonged'
$notes += 'to, so it is not a status command on this install or on any install that has taken the'
$notes += 'deletion. `windsvc doctor` reads no switch to decide what to report: it runs the same'
$notes += 'lookup the tray runs, and prints what it found.'
$notes += ''
$notes += '=============================================================================='
$notes += 'WHAT IS IN HERE, AND WHERE THE RUNTIME LOOKS FOR IT'
$notes += '=============================================================================='
$notes += ''
$notes += 'Every one of the eleven binaries finds this directory itself: starting from the running'
$notes += 'executable, each ancestor in turn is asked whether it carries'
$notes += 'config_src\config_default.json (or a config_src\ directory, a windrecorder\config_src\ from'
$notes += 'an install that has not moved its settings up, or userdata\), and the first one that does is'
$notes += 'the install. base\src\install.rs is the single place that rule is written, and it is why'
$notes += 'neither `windsetup init` nor `windrec run` needed a --root above. Once the install is known,'
$notes += 'a sibling binary is looked for in this order -- supervisor\src\native.rs::candidate_dirs(),'
$notes += 'which is what the tray itself runs to decide what to launch, and which is unchanged from the'
$notes += 'runtime it replaced:'
$notes += '%WINDCAP_HOME%, then <root>\bin, then <root>, then <root>\windcap\target\release, then'
$notes += '<root>\windcap\target\debug -- release before debug, always, and a debug build is labelled'
$notes += 'as one wherever it is reported. windcap.dll is read from %WINDCAP_DLL% first, then'
$notes += '<root>\bin\windcap.dll; on a payload like this one bin\ is the only candidate that exists.'
$notes += 'So everything below sits in bin\ or in config_src\, and on a machine that was handed this'
$notes += 'zip rather than a Rust tree those are the only places anything can look.'
$notes += 'For the DLL, bin\ is the first one tried.'
$notes += ''
foreach ($item in $staged) {
    $notes += ('  ' + (Format-Target -Target $item.Target) + ('{0,11}' -f $item.Size))
    $notes += ('      sha256 ' + $item.Sha256)
    $notes += ('      ' + $item.Role)
}
if ($skipped.Count -gt 0) {
    $notes += ''
    $notes += 'NOT IN THIS PAYLOAD'
    foreach ($line in $skipped) { $notes += '  ' + $line }
}
$notes += ''
$notes += '=============================================================================='
$notes += 'ARTEFACTS AS BUILT'
$notes += '=============================================================================='
$notes += ('{0,-16} {1,-12} {2,11} {3,9}  {4}' -f 'artefact', 'status', 'size', 'seconds', 'note')
foreach ($item in $payload) {
    $status = 'not reported'
    $size = '--'
    $seconds = '--'
    $note = ''
    if ($item.Row) {
        $status = [string] $item.Row.status
        $seconds = [string] $item.Row.seconds
        $note = [string] $item.Row.note
        $size = Format-Size -Bytes ([long] $item.Row.bytes)
    }
    elseif ($SkipUi -and $item.Name -eq 'winduiweb.exe') {
        # `-SkipUi` skips both slow crates in build.ps1 — the egui binary and the HTML one share
        # `wind-ui`, whose library target the first is and the second depends on — so a `-Profile`
        # rehearsal leaves both windows unbuilt. Only one of them has a row in this table: the retired
        # `windui.exe` is no longer a payload entry, so it is neither staged nor reported nor held
        # against the release, and naming it here would put a binary this zip does not carry back into
        # its own artefact list.
        $status = 'SKIPPED'
        $note = '-SkipUi'
    }
    $notes += ('{0,-16} {1,-12} {2,11} {3,9}  {4}' -f $item.Name, $status, $size, $seconds, $note)
}
if ($totalSeconds) {
    $notes += ('{0,-16} {1,-12} {2,11} {3,9}' -f 'total', '', '', ($totalSeconds + 's'), '')
}
$notes += ''
$notes += 'Those sizes are what cargo wrote; the sha256 lines above are what is in this zip.'
$notes += 'A .pdb is deliberately not shipped: it belongs to the build directory, and the'
$notes += 'windcap\target\build.log there holds the compiler side of any stack trace.'
$notes += ''
$notes += '=============================================================================='
$notes += 'WHAT CHANGED IN THIS BUILD   (git log --oneline main..HEAD)'
$notes += '=============================================================================='
$notes += ''
foreach ($line in $changes) { $notes += '  ' + $line }
if ($gitNote) { $notes += $gitNote }
$notes += ''
$notes += 'Provenance: this is a build of the working tree above, uncommitted local edits included'
$notes += '-- not a checkout of a named commit. Which is exactly why the zip has a hash:'
$notes += '(release.ps1 output, and ' + (Split-Path -Leaf $zipPath) + '.sha256 beside it) is what'
$notes += 'proves the bytes you unpacked are the bytes that were built.'
$notes += ''
Set-Content -LiteralPath $notesPath -Value $notes -Encoding UTF8
$notesBytes = (Get-Item -LiteralPath $notesPath).Length
Write-Line ('  STAGED   ' + (Format-Target -Target 'RELEASE-NOTES.txt') + ('{0,11}' -f (Format-Size -Bytes $notesBytes)) + ('{0,8}' -f '-')) 'Green'

# Anything else in dist for a different version is somebody else's release; named, never deleted.
$staleSiblings = @(Get-ChildItem -LiteralPath $distDir -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -like 'Windrecorder-native-*' -and $_.FullName -ne $stagingDir })
if ($staleSiblings.Count -gt 0) {
    Write-Line ''
    Write-Line '  other staged trees in windcap\dist, left alone:' 'DarkGray'
    foreach ($stale in $staleSiblings) { Write-Line ('    ' + $stale.Name) 'DarkGray' }
}

# ---------------------------------------------------------------------------
# 6. A release with no recorder in it is not a release, so the required check comes *before* the
#    archive. The zip and its sidecar were deleted with the staging tree above and are not rewritten
#    here, which is the point: `dist/` is then never left holding an archive that looks current and
#    is not, and smoke.ps1 refuses to run at all rather than certify half a payload.
# ---------------------------------------------------------------------------
if ($missingRequired.Count -gt 0) {
    Write-Line ''
    Write-Line '  not in this payload:' 'Red'
    foreach ($line in $skipped) { Write-Line ('    ' + $line) 'Red' }
    Write-Line ''
    Write-Line ('result: NOT SHIPPABLE -- required artefact(s) not built: ' + ($missingRequired -join ', ')) 'Red'
    Write-Line ('  ' + (Split-Path -Leaf $stagingDir) + ' holds what did build; there is no zip.') 'Red'
    Write-Line ('  full cargo output: ' + $logPath) 'Red'
    exit 1
}

# ---------------------------------------------------------------------------
# 7. Zip, then hash. The glob rather than the staging directory itself, so the entries are
#    bin\ / config_src\ / ocr_lib\ / RELEASE-NOTES.txt at the archive root -- i.e. ready to unzip
#    into an empty folder and have that folder be the install -- rather than nested one level down
#    inside a folder nobody asked to have.
# ---------------------------------------------------------------------------
Write-Line ''
Compress-Archive -Path (Join-Path $stagingDir '*') -DestinationPath $zipPath -CompressionLevel Optimal -Force
if (-not (Test-Path -LiteralPath $zipPath)) {
    Write-Line 'release.ps1: Compress-Archive produced no zip.' 'Red'
    exit 1
}

$zipItem = Get-Item -LiteralPath $zipPath
$zipHash = Get-Sha256 -Path $zipPath
# `sha256sum -c` compatible, so the sidecar is usable from either end of the transfer.
Set-Content -LiteralPath $hashPath -Value ($zipHash + '  ' + $zipItem.Name) -Encoding ASCII

Write-Line ('  zip        ' + $zipItem.FullName) 'Cyan'
Write-Line ('  zip size   ' + (Format-Size -Bytes $zipItem.Length) + '   (' + $zipItem.Length + ' bytes)') 'Cyan'
Write-Line ('  sha256     ' + $zipHash) 'Cyan'
Write-Line ('  checksum   ' + $hashPath) 'DarkGray'
Write-Line ('  unpacked   ' + $stagingDir) 'DarkGray'

# ---------------------------------------------------------------------------
# 8. Verdict. Optional artefacts are listed and the release still ships; build.ps1 is fail-soft
#    about a crate that does not compile, and a missing winduiweb.exe costs the one front end this
#    payload carries, not a recording.
# ---------------------------------------------------------------------------
if ($skipped.Count -gt 0) {
    Write-Line ''
    Write-Line '  not in this payload:' 'Yellow'
    foreach ($line in $skipped) { Write-Line ('    ' + $line) 'Yellow' }
}

Write-Line ''
Write-Line ('result: ' + (Split-Path -Leaf $zipPath) + ' is built, staged and hashed. Prove it: windcap\smoke.ps1') 'Green'
exit 0
