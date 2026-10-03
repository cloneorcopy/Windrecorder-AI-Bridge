<#
    smoke.ps1 -- prove the shipped zip works, in a throwaway directory, against a copy of real data.

    ONE COMMAND
        powershell -ExecutionPolicy Bypass -File windcap\smoke.ps1

    WHY THIS EXISTS AT ALL
        release.ps1 can produce a zip whose every byte is correct and which still does not work,
        because the thing that has to be right is not the binaries but the *layout*: whether
        the tray's own candidate order will find bin\windrec.exe where the zip put it, whether
        windcapctl's --root resolves, whether a path in the payload is one the runtime actually
        probes. None of that is testable by reading the code. It is all testable by unpacking the
        zip somewhere disposable, pointing the tools at a copy of somebody's real index, and
        demanding the answers come back.

        The layout gained a second half when the zip stopped being an overlay. It is a standalone
        package now, so the settings it installs *from* have to be in it: hence the config_src\
        assertions, which are the difference between "these binaries run" and "this is something a
        person can unpack into an empty folder and install".

    WHY IT ASSERTS ON OUTPUT AND NOT ON EXIT CODES
        Every one of these binaries exits 0 while reporting that it found nothing. `windrec status`
        on an install with no index says "0 month file(s)" and succeeds. An exit code therefore
        proves the process ran and nothing else, which is exactly the failure mode worth catching:
        a layout that makes the tools look at an empty directory is *silent*. So each check matches
        on the text -- nine rows in, nine rows reported -- and anything else is a FAIL.

    THE GUI SECTIONS ARE NOT ABOUT OUTPUT AT ALL.  Section 4b starts a GUI-subsystem process and asks
    what appeared on disk and on the desktop: a seeded config file, a lock naming its own pid, a window
    handle. Nothing the tray or the window writes to stdout is observable -- they have no console,
    which is the point of shipping them that way -- so the gate reads the effects instead. A build
    that links, exits 0 and never draws a window was invisible to this script until now. Every
    process 4b starts is stopped by the pid `Start-Process` handed back, never by image name: a
    second Windrecorder on this machine is somebody else's recording, and killing by name would
    take it with the test's own.

        Section 4c does the same thing through `bin\Windrecorder.exe` -- the file both READMEs tell a
        stranger to click -- in a third copy of the payload, and 4d asks the one question a
        double-click can never answer: what does the launcher say when there is no tray to start?
        Those sections cannot use the `&` call operator, which neither waits for a GUI-subsystem child
        nor captures what it printed (measured: an empty string and an unset `$LASTEXITCODE` while the
        report was still being written). They go through `Invoke-Redirected`, which is
        `Start-Process -Wait` with both streams in files.

    FAIL LOUD.  build.ps1 is deliberately fail-soft, because an optional engine must never break a
        user's update. This is the opposite case: it is the only thing standing between a broken
        payload and someone's machine, so it exits non-zero on the first failed assertion and says
        which one.

    Windows PowerShell 5.1 is the floor: no `&&`, no ternary, no `??`.
#>

[CmdletBinding()]
param(
    # The zip to test. Default: the one and only Windrecorder-native-*.zip in windcap\dist. Named
    # explicitly when there is more than one, or to point at a doctored copy.
    [string] $Zip,
    # A real month database to copy. Read once, never opened by anything here: the tools write a
    # `_TEMP_READ.db` sibling next to whatever they read, so they are only ever given the copy.
    # Empty means this checkout's own install, resolved below from $PSScriptRoot -- pass a frozen
    # sample with -Source on a machine whose recorder is still growing that file, because the
    # reference checks below count its rows.
    [string] $Source,
    # What that database holds. Nine rows is not a magic number, it is the row count of the frozen
    # sample this gate was built against -- a query that returns nine proves the whole path from
    # --root through the copy into SQLite. Override it with -ExpectRows for a different sample.
    [int] $ExpectRows = 9,
    # The window the reference rows fall inside, in the format windcapctl accepts
    # (YYYY-MM-DD_HH-MM-SS; a colon here is a usage error, by design, so the tool tells you).
    [string] $From = '2026-09-21_00-00-00',
    [string] $To = '2026-09-23_00-00-00',
    # Leave %TEMP%\windrec-smoke-<pid> behind to look inside. It is still only ever deleted by a
    # path that has been checked to be under %TEMP%.
    [switch] $Keep
)

$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'

if ($args -and $args.Count -gt 0) {
    Write-Host 'smoke.ps1: unexpected argument(s):' -ForegroundColor Red
    foreach ($stray in $args) { Write-Host ('    ' + $stray) -ForegroundColor Red }
    Write-Host 'usage: powershell -ExecutionPolicy Bypass -File windcap\smoke.ps1 [-Zip PATH] [-Source PATH] [-ExpectRows N] [-From STAMP] [-To STAMP] [-Keep]'
    exit 2
}

$workspace = $PSScriptRoot
$distDir = Join-Path $workspace 'dist'
# The reference database defaults to this checkout's own install. Resolved here rather than as a
# parameter default, because Windows PowerShell 5.1 has no $PSScriptRoot yet while parameter
# defaults are evaluated -- the expression in the param block failed before the first check ran.
if (-not $Source) {
    $Source = Join-Path (Split-Path -Parent $workspace) 'userdata\db\default_2026-09_wind.db'
}
# Where windcap.dll's ABI contract is defined: the crate that exports it. This used to be
# windrecorder/native_bridge.py, the Python loader that read the number off the module and refused a
# DLL that disagreed with it. That module was deleted in 3f37cbf, so the constant is now read from
# its remaining authoritative home -- the same principle, one fewer language.
$abiSource = Join-Path $workspace 'core\src\ffi_c.rs'

$results = @()
$pass = 0
$fail = 0
$skip = 0

# Tri-state on purpose. A check that could not be made -- no hash sidecar for a zip handed to
# `-Zip` from somewhere else, no sample database to query -- is neither a pass nor a failure, and
# printing "PASS" for it is the exact soft signal this script exists to remove. $null means skipped.
function Add-Result {
    param([string] $Name, $Ok, [string] $Detail)
    if ($null -eq $Ok) {
        $script:skip++
        $script:results += [pscustomobject]@{ Name = $Name; Ok = $null; Detail = $Detail }
        Write-Host ('  SKIP    ' + ('{0,-52}' -f $Name) + $Detail) -ForegroundColor Yellow
        return
    }
    $script:results += [pscustomobject]@{ Name = $Name; Ok = [bool] $Ok; Detail = $Detail }
    if ($Ok) {
        $script:pass++
        Write-Host ('  PASS    ' + ('{0,-52}' -f $Name) + $Detail) -ForegroundColor Green
    }
    else {
        $script:fail++
        Write-Host ('  FAIL    ' + ('{0,-52}' -f $Name) + $Detail) -ForegroundColor Red
    }
}

function Format-Size {
    param([long] $Bytes)
    if ($Bytes -ge 1048576) { return ('{0:N1} MiB' -f ($Bytes / 1MB)) }
    if ($Bytes -ge 1024) { return ('{0:N1} KiB' -f ($Bytes / 1KB)) }
    return ('{0} bytes' -f $Bytes)
}

# Same reason as in release.ps1: `Get-FileHash` is not a command in this box's Windows PowerShell
# 5.1 session, and comparing the zip against the hash release.ps1 recorded is the first check of the
# whole script. .NET's SHA256 is in every 5.1 install. Lowercase, to match the sidecar.
function Get-Sha256 {
    param([string] $Path)
    $stream = [System.IO.File]::OpenRead($Path)
    try {
        $hasher = [System.Security.Cryptography.SHA256]::Create()
        try {
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

# ---------------------------------------------------------------------------
# The one thing that can destroy a machine from inside a test script: Remove-Item pointed at a path
# that is not the one you meant. Every deletion below goes through here, and this refuses anything
# that is not inside %TEMP% -- including %TEMP% itself, and including a path that came from a
# variable that had been cleared to something else.
# ---------------------------------------------------------------------------
function Remove-TempTree {
    param([string] $Path, [string] $Why = 'cleanup')
    if (-not $Path) { throw 'Remove-TempTree: empty path' }
    if (-not (Test-Path -LiteralPath $Path)) { return }
    if (-not $env:TEMP) { throw 'Remove-TempTree: %TEMP% is not set, so nothing can be proven to be under it' }
    $allowed = (Resolve-Path -LiteralPath $env:TEMP).Path.TrimEnd('\')
    $target = (Resolve-Path -LiteralPath $Path).Path
    if ($target -eq $allowed) {
        throw ('Remove-TempTree: refusing to remove ' + $allowed + ' (' + $Why + ')')
    }
    if (-not $target.StartsWith($allowed + '\', [System.StringComparison]::OrdinalIgnoreCase)) {
        throw ('Remove-TempTree: refusing to remove ' + $target + ' -- not under ' + $allowed + ' (' + $Why + ')')
    }
    Remove-Item -LiteralPath $target -Recurse -Force
    Write-Host ('  removed  ' + $target + '  (' + $Why + ')') -ForegroundColor DarkGray
}

# A GUI-subsystem child is neither waited for nor captured by the `&` call operator -- measured on this
# box, `& Windrecorder.exe doctor` handed back an empty string and left `$LASTEXITCODE` unset while the
# report was still being written. `Start-Process -Wait` with both streams in files is the only honest
# way to ask what such a binary printed and what it exited with, and sections 4c and 4d both need it.
function Invoke-Redirected {
    param([string] $Exe, [string[]] $Arguments, [string] $Tag)
    $out = Join-Path $work ($Tag + '.out')
    $err = Join-Path $work ($Tag + '.err')
    foreach ($file in @($out, $err)) { if (Test-Path -LiteralPath $file) { Remove-Item -LiteralPath $file -Force } }
    $proc = Start-Process -FilePath $Exe -ArgumentList $Arguments -WorkingDirectory $work -Wait -PassThru `
        -RedirectStandardOutput $out -RedirectStandardError $err
    $text = ''
    if (Test-Path -LiteralPath $out) { $text += (Get-Content -LiteralPath $out -Raw -ErrorAction SilentlyContinue) }
    if (Test-Path -LiteralPath $err) { $text += (Get-Content -LiteralPath $err -Raw -ErrorAction SilentlyContinue) }
    if ($null -eq $text) { $text = '' }
    return @{ Code = $proc.ExitCode; Text = ($text -replace "`r`n", "`n") }
}

# A tray that has just been clicked is not finished starting. `boot` runs `windsetup init` -- and on an
# older tree a `migrate` too -- before it spawns the recorder, so a single child snapshot taken four
# seconds in can catch it mid-layout and report `children: windsetup.exe` as though no recorder had
# been started. Seen twice on this box with a payload that was recording fine a second later. Poll, and
# judge what the tray ended up doing.
function Wait-ForTrayChild {
    param([int] $TrayPid, [string] $Name = 'windrec.exe', [int] $LimitSeconds = 25)
    $kids = @()
    $names = @()
    $deadline = (Get-Date).AddSeconds($LimitSeconds)
    while ((Get-Date) -lt $deadline) {
        $kids = @(Get-CimInstance Win32_Process -Filter ('ParentProcessId=' + $TrayPid) -ErrorAction SilentlyContinue)
        $names = @($kids | ForEach-Object { $_.Name })
        if ($names -contains $Name) { break }
        Start-Sleep -Milliseconds 500
    }
    return @{ Kids = $kids; Names = $names }
}

# ---------------------------------------------------------------------------
# A temp install, unpacked payload, and one copy of a real database.
# ---------------------------------------------------------------------------
# Rust writes UTF-8; PowerShell 5.1 decodes a native command's stdout with the console codepage,
# which on this box is not UTF-8. Without this the Chinese OCR text comes back as mojibake and an
# assertion about it cannot be read by anyone.
$savedOutputEncoding = $null
$work = $null

try {
    try {
        $savedOutputEncoding = [Console]::OutputEncoding
        [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
    }
    catch {
        Write-Host '  (could not set the console to UTF-8; Chinese text in the report will be mangled)' -ForegroundColor Yellow
    }

    # Two environment variables can outrank the whole layout being tested, and then this script
    # would be measuring somebody's development tree instead of the zip.
    if ($env:WINDCAP_HOME) {
        Write-Host ('  clearing WINDCAP_HOME=' + $env:WINDCAP_HOME + ' for this run') -ForegroundColor Yellow
        $env:WINDCAP_HOME = $null
    }
    if ($env:WINDCAP_DLL) {
        Write-Host ('  clearing WINDCAP_DLL=' + $env:WINDCAP_DLL + ' for this run') -ForegroundColor Yellow
        $env:WINDCAP_DLL = $null
    }

    $temp = (Resolve-Path -LiteralPath $env:TEMP).Path
    Write-Host ''
    Write-Host 'windcap release smoke test' -ForegroundColor Cyan
    Write-Host ('  temp root  ' + $temp)

    # Re-runnable: a previous run that was killed before its cleanup leaves a directory here, and
    # directories named by pid would otherwise pile up one per run. Matched by prefix, so the only
    # paths that can reach Remove-Item are ones this listing produced.
    foreach ($stale in @(Get-ChildItem -LiteralPath $temp -Directory -Filter 'windrec-smoke-*' -ErrorAction SilentlyContinue)) {
        Remove-TempTree -Path $stale.FullName -Why 'previous run'
    }
    $work = Join-Path $temp ('windrec-smoke-' + $PID)
    New-Item -ItemType Directory -Path $work -Force | Out-Null

    # -----------------------------------------------------------------------
    # 1. The zip, and whether it is the bytes release.ps1 hashed.
    # -----------------------------------------------------------------------
    $zipPath = $Zip
    if (-not $zipPath) {
        $candidates = @(Get-ChildItem -LiteralPath $distDir -Filter 'Windrecorder-native-*.zip' -File -ErrorAction SilentlyContinue)
        if ($candidates.Count -eq 0) {
            Write-Host ''
            Write-Host ('  no Windrecorder-native-*.zip in ' + $distDir) -ForegroundColor Red
            Write-Host '  build one first:  powershell -ExecutionPolicy Bypass -File windcap\release.ps1' -ForegroundColor Red
            exit 1
        }
        if ($candidates.Count -gt 1) {
            Write-Host ('  ' + $candidates.Count + ' zips in windcap\dist, testing the newest:') -ForegroundColor Yellow
            foreach ($c in $candidates) { Write-Host ('    ' + $c.Name) -ForegroundColor DarkGray }
        }
        $zipPath = ($candidates | Sort-Object LastWriteTime -Descending | Select-Object -First 1).FullName
    }
    if (-not (Test-Path -LiteralPath $zipPath)) {
        Write-Host ('  no such zip: ' + $zipPath) -ForegroundColor Red
        exit 1
    }
    $zipItem = Get-Item -LiteralPath $zipPath
    Write-Host ('  zip        ' + $zipItem.FullName + '  (' + (Format-Size -Bytes $zipItem.Length) + ')')

    $actualHash = Get-Sha256 -Path $zipPath
    $sidecar = $zipPath + '.sha256'
    if (Test-Path -LiteralPath $sidecar) {
        # Two spaces and a bare file name, the shape `sha256sum -c` reads.
        $recorded = ((Get-Content -LiteralPath $sidecar -Raw) -split '\s+')[0]
        $hashOk = ($recorded -and ($recorded -ieq $actualHash))
        Add-Result -Name 'zip sha256 matches release.ps1' -Ok $hashOk `
            -Detail $(if ($hashOk) { $actualHash } else { 'sidecar says ' + $recorded + ', the bytes are ' + $actualHash })
    }
    else {
        Add-Result -Name 'zip sha256 matches release.ps1' -Ok $null `
            -Detail ('no ' + (Split-Path -Leaf $sidecar) + ' beside this zip, so there is nothing to compare; this zip is ' + $actualHash)
    }

    # -----------------------------------------------------------------------
    # 2. Unpack it onto a minimal install root.
    # -----------------------------------------------------------------------
    $install = Join-Path $work 'install'
    New-Item -ItemType Directory -Path $install -Force | Out-Null
    Expand-Archive -LiteralPath $zipPath -DestinationPath $install -Force
    $installed = $true

    # A zip made by `Compress-Archive -Path <staging>\*` has bin\ at its root, which is what a
    # person unzipping onto an install needs. One made by pointing at the folder itself has it one
    # level down. Accept either rather than failing on a packaging choice that still works, and say
    # which was found -- because the whole point is where bin\ ended up relative to the root.
    $payloadRoot = $install
    if (-not (Test-Path -LiteralPath (Join-Path $payloadRoot 'bin'))) {
        $nested = @(Get-ChildItem -LiteralPath $install -Directory | Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'bin') })
        if ($nested.Count -eq 1) { $payloadRoot = $nested[0].FullName }
    }
    Add-Result -Name 'unpacked payload has bin\ at the install root' `
        -Ok (Test-Path -LiteralPath (Join-Path $payloadRoot 'bin')) `
        -Detail $(if ($payloadRoot -eq $install) { $install + '\bin' } else { 'found one level down: ' + $payloadRoot })

    # The payload carries its own settings now -- that is what the config_src\ assertions below are
    # for -- so nothing is fabricated here any more. The one half of a real install this test still
    # has to supply is the half the payload is *not* allowed to bring: a userdata\ tree, because it
    # holds somebody's recordings. (This used to copy config_default.json out of
    # `windrecorder\config_src\` to stand in for an existing install. That directory moved to the
    # install root, the copy started failing every run, and the payload stopped needing it.)
    New-Item -ItemType Directory -Path (Join-Path $install 'userdata\db') -Force | Out-Null

    if (-not (Test-Path -LiteralPath $Source)) {
        Add-Result -Name 'reference database found' -Ok $false -Detail ('no month file at ' + $Source + ' -- pass -Source <path>')
        throw 'no sample database to query'
    }
    # Copy, never open: wind-store's reader materialises <file>_TEMP_READ.db beside whatever it
    # reads, so pointing it at E:\ would write into a live install's userdata.
    $dbDir = Join-Path $install 'userdata\db'
    $dbCopy = Join-Path $dbDir (Split-Path -Leaf $Source)
    Copy-Item -LiteralPath $Source -Destination $dbCopy -Force
    Add-Result -Name 'reference database copied into the temp install' -Ok $true `
        -Detail ((Split-Path -Leaf $dbCopy) + '  ' + (Format-Size -Bytes ((Get-Item -LiteralPath $dbCopy).Length)))

    $sourceBefore = Get-Item -LiteralPath $Source
    $sourceSiblingsBefore = @(Get-ChildItem -LiteralPath (Split-Path -Parent $Source) -File | ForEach-Object { $_.Name }).Count

    # `default_2026-09_wind.db` -> `2026-09`, the label every report quotes. Parsed from the file
    # name rather than hardcoded, so the month assertions still hold for a different -Source.
    $monthLabel = ''
    if ((Split-Path -Leaf $Source) -match '(\d{4}-\d{2})') { $monthLabel = $matches[1] }

    # -----------------------------------------------------------------------
    # 3. The layout contract: is bin\windcap.dll the file a loader would pick up first?
    #    The candidate order release.ps1 stages against, mirrored here, first existing wins.
    # -----------------------------------------------------------------------
    $bridgeOrder = @(
        (Join-Path $install 'bin\windcap.dll'),
        (Join-Path $install 'windcap\target\release\windcap.dll'),
        (Join-Path $install 'windcap\windcap.dll'),
        (Join-Path $install 'windcap.dll')
    )
    $bridgePicks = $null
    foreach ($probe in $bridgeOrder) {
        if (Test-Path -LiteralPath $probe) { $bridgePicks = $probe; break }
    }
    $expectedDll = Join-Path $install 'bin\windcap.dll'
    Add-Result -Name 'the dll candidate order picks bin\windcap.dll' `
        -Ok ($bridgePicks -and ($bridgePicks -ieq $expectedDll)) `
        -Detail $(if ($bridgePicks) { 'it picks ' + $bridgePicks } else { 'nothing finds a windcap.dll at all: the session probes have no exported ABI on this payload' })

    # The same, for the executables: supervisor\src\native.rs's candidate_dirs() puts <root>\bin
    # second, and
    # the payload's whole claim is that a machine with no cargo tree still finds everything.
    # Every executable the release stages, not the four that happened to exist when this script
    # was written: a payload missing windsvc.exe has no way to start the recorder, and the check
    # below would still have passed. `windui` is off this list on purpose -- the egui front end was
    # retired from the shipped set by docs\adr\2026-09-27-winduiweb-is-the-only-interface.md, so
    # asserting it is in bin\ would fail a payload that is exactly as complete as it should be.
    foreach ($exe in @('windrec', 'winduiweb', 'windmaint', 'windcapctl', 'windsvc', 'windmcp',
                       'wind-reindex', 'windnotes', 'windsetup', 'windai')) {
        $probe = Join-Path $install ('bin\' + $exe + '.exe')
        Add-Result -Name ('bin\' + $exe + '.exe present in the payload') -Ok (Test-Path -LiteralPath $probe) `
            -Detail $(if (Test-Path -LiteralPath $probe) { Format-Size -Bytes ((Get-Item -LiteralPath $probe).Length) } else { 'missing from the zip' })
    }

    # -----------------------------------------------------------------------
    # The settings. Ten binaries that all run is still not a package if none of them can
    # be installed where they are put: base\src\install.rs defines an install root as the
    # directory carrying config_src\config_default.json, and that is what `windsetup init`
    # walks up looking for. Measured on this machine against a directory holding nothing
    # but this payload's bin\ and ocr_lib\: `init` resolves its root into bin\ and lays all
    # 16 writable slots under bin\userdata -- the compiled-in defaults catch the seed, so it
    # does not even fail loudly any more -- and `windsvc doctor` refuses the folder with
    # "is not a Windrecorder install". A zip that gets this wrong installs the user's data
    # in the wrong half of their own install. And every check below that runs a binary only
    # means something once the settings are somewhere the binaries will look for them.
    #
    # Sizes, not just names: a zero-byte settings file passes a presence test and fails a
    # person, and each of these is a table something has to parse.
    # -----------------------------------------------------------------------
    $configSrcHere = Join-Path $install 'config_src'
    $configSrcEntries = @()
    if (Test-Path -LiteralPath $configSrcHere) {
        $configSrcEntries = @(Get-ChildItem -LiteralPath $configSrcHere -Force)
    }
    $synonymFiles = @(Get-ChildItem -LiteralPath (Join-Path $configSrcHere 'synonyms') -File -Force -ErrorAction SilentlyContinue)
    Add-Result -Name 'config_src\ in the payload' -Ok (($configSrcEntries.Count -gt 0) -and ($synonymFiles.Count -gt 0)) `
        -Detail $(if ($configSrcEntries.Count -eq 0) {
                'no config_src\ in the unpacked zip: init resolves its root into bin\ and lays the whole layout under bin\userdata, and windsvc doctor refuses this folder as not an install'
            }
            elseif ($synonymFiles.Count -eq 0) {
                '' + $configSrcEntries.Count + ' entries, but config_src\synonyms\ is absent or empty -- the search-recommendation vocabularies and their indices did not ship'
            }
            else {
                '' + $configSrcEntries.Count + ' entries at the top level, synonyms\ included with ' + $synonymFiles.Count + ' file(s) in it'
            })
    # The four the native code opens while it is running, not merely while it installs.
    foreach ($setting in @(
            [pscustomobject]@{
                Name    = 'config_default.json'
                Role    = 'what userdata\config_user.json is seeded from, and the file that makes a directory an install root'
                Missing = 'windsetup init resolves its root into bin\ and lays the layout there; windsvc doctor refuses the folder'
            },
            [pscustomobject]@{
                Name    = 'languages.json'
                Role    = "the tray menu's strings, read by supervisor\src\i18n.rs"
                Missing = 'windsvc has no menu text for any language'
            },
            [pscustomobject]@{
                Name    = 'similar_CN_characters.txt'
                Role    = 'the Chinese fuzzy-glyph table, read by the search side of cli and ai'
                Missing = 'Chinese search stops forgiving a lookalike character, so a mistyped name finds nothing'
            },
            [pscustomobject]@{
                Name    = 'wordcloud_stopword.txt'
                Role    = 'the stopword list the activity wordcloud is built against'
                Missing = 'the wordcloud fills itself with the words that appear on every screen'
            })) {
        $probe = Join-Path $configSrcHere $setting.Name
        $bytes = -1
        if (Test-Path -LiteralPath $probe) { $bytes = (Get-Item -LiteralPath $probe).Length }
        Add-Result -Name ('config_src\' + $setting.Name + ' in the payload') -Ok ($bytes -gt 0) `
            -Detail $(if ($bytes -gt 0) { (Format-Size -Bytes $bytes) + ' -- ' + $setting.Role }
                elseif ($bytes -eq 0) { 'shipped empty, which breaks the same feature as not shipping it: ' + $setting.Missing }
                else { 'missing from the zip: ' + $setting.Missing })
    }

    # -----------------------------------------------------------------------
    # The OCR self-check fixtures. `windsetup check-engines` can only answer "is my OCR working?"
    # if the payload carries the fixture pairs it reads -- without them it now (correctly) exits 3
    # with "NOT TESTED -- no OCR fixtures", but a person who unpacked only this zip has no way to get
    # a real answer. These checks prove the pairs shipped, prove each is non-empty, prove that ONLY
    # the pairs shipped (not the README art / fonts / product screenshots the directory is full of),
    # and then run check-engines against the payload to prove it no longer falls into the no-fixture
    # outcome that used to be misreported as "no OCR engine is usable".
    # -----------------------------------------------------------------------
    $assetsHere = Join-Path $install '__assets__'
    $fixtureLanguages = @('en-US', 'ja-jp', 'zh-Hans-CN')
    $allAssets = @(Get-ChildItem -LiteralPath $assetsHere -File -Force -ErrorAction SilentlyContinue)
    $fixtureFiles = @($allAssets | Where-Object { $_.Name -like 'OCR_test_1080_*.png' -or $_.Name -like 'OCR_test_1080_words_*.txt' })
    $fixtureNames = @($fixtureFiles | ForEach-Object { $_.Name })
    $nonFixtureAssets = @($allAssets | Where-Object { $fixtureNames -notcontains $_.Name })
    Add-Result -Name '__assets__\ ships the OCR self-check fixtures' `
        -Ok ($fixtureFiles.Count -ge 6) `
        -Detail $(if ($allAssets.Count -eq 0) {
                'no __assets__\ in the unpacked zip: windsetup check-engines can only answer NOT TESTED (exit 3) on a standalone install'
            }
            else {
                '' + $fixtureFiles.Count + ' fixture file(s) shipped' + $(if ($nonFixtureAssets.Count -gt 0) { ', plus ' + $nonFixtureAssets.Count + ' unexpected non-fixture file(s)' } else { ', and nothing else' })
            })
    # One check per language pair: the check runs one engine pass per pair, so a half-pair silently
    # drops a language from the self-check rather than failing loudly.
    foreach ($lang in $fixtureLanguages) {
        $img = Join-Path $assetsHere ('OCR_test_1080_' + $lang + '.png')
        $words = Join-Path $assetsHere ('OCR_test_1080_words_' + $lang + '.txt')
        $imgOk = (Test-Path -LiteralPath $img) -and ((Get-Item -LiteralPath $img).Length -gt 0)
        $wordsOk = (Test-Path -LiteralPath $words) -and ((Get-Item -LiteralPath $words).Length -gt 0)
        Add-Result -Name ('__assets__\ OCR fixture pair for ' + $lang + ' ships non-empty') `
            -Ok ($imgOk -and $wordsOk) `
            -Detail $(if (-not $imgOk) { 'the image OCR_test_1080_' + $lang + '.png is missing or empty' }
                elseif (-not $wordsOk) { 'the word list OCR_test_1080_words_' + $lang + '.txt is missing or empty' }
                else { 'image + word list, both non-empty' })
    }
    # Selective staging, asserted as a negative: __assets__ is full of README header art, Productpost
    # and product-preview jpgs, .otf fonts and contribution markdown. None belongs in a payload; any
    # non-fixture file here is exactly the junk release.ps1 was told to leave out.
    Add-Result -Name '__assets__\ ships ONLY the OCR fixtures (no README art, fonts, product shots)' `
        -Ok (($allAssets.Count -gt 0) -and ($nonFixtureAssets.Count -eq 0)) `
        -Detail $(if ($allAssets.Count -eq 0) { 'no __assets__\ at all, so nothing leaked but there is nothing to check either -- see the fixtures-shipped result above' }
            elseif ($nonFixtureAssets.Count -gt 0) { 'unexpected file(s): ' + (($nonFixtureAssets | ForEach-Object { $_.Name }) -join ', ') }
            else { 'every file under __assets__\ matches the OCR fixture naming; nothing else leaked in' })

    # windcap.dll must load and answer with the ABI this tree exports, or a caller that checks the
    # version on load walks away and nothing here would have caught it. ABI_VERSION is read from
    # core\src\ffi_c.rs -- the file that declares it and the file the cdylib is built from, because a
    # contract is not something to copy a number out of. Comparing the shipped DLL against the source
    # that is supposed to have produced it is also what catches a windcap.dll left behind by an older
    # build. Unreadable source leaves this at -1, which cannot match anything: no source, no pass.
    $expectedAbi = -1
    if (Test-Path -LiteralPath $abiSource) {
        foreach ($line in (Get-Content -LiteralPath $abiSource)) {
            if ($line -match 'ABI_VERSION\s*:\s*u32\s*=\s*(\d+)') { $expectedAbi = [int] $matches[1]; break }
        }
    }
    $dllAbi = -999
    $dllWhy = 'the probe itself could not run'
    try {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class WindcapSmokeProbe {
    [DllImport("kernel32", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr LoadLibraryW(string path);
    [DllImport("kernel32", SetLastError = true)]
    static extern IntPtr GetProcAddress(IntPtr module, string name);
    [DllImport("kernel32")]
    static extern bool FreeLibrary(IntPtr module);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    delegate uint AbiFn();
    public static int AbiVersion(string path) {
        IntPtr module = LoadLibraryW(path);
        if (module == IntPtr.Zero) { return -1000 - Marshal.GetLastWin32Error(); }
        try {
            IntPtr entry = GetProcAddress(module, "windcap_abi_version");
            if (entry == IntPtr.Zero) { return -1; }
            AbiFn abi = (AbiFn) Marshal.GetDelegateForFunctionPointer(entry, typeof(AbiFn));
            return (int) abi();
        }
        finally { FreeLibrary(module); }
    }
}
'@
        $dllAbi = [WindcapSmokeProbe]::AbiVersion($expectedDll)
        if ($dllAbi -lt -1000) { $dllWhy = 'LoadLibrary failed, win32 error ' + ([Math]::Abs($dllAbi + 1000)) + ' (a missing DLL dependency, not a missing file)' }
        elseif ($dllAbi -eq -1) { $dllWhy = 'loaded but exports no windcap_abi_version' }
        else { $dllWhy = 'ABI ' + $dllAbi + ', core\src\ffi_c.rs declares ' + $expectedAbi }
    }
    catch {
        $dllWhy = 'could not compile the loader probe: ' + $_.Exception.Message
    }
    Add-Result -Name 'bin\windcap.dll loads and reports the expected ABI' -Ok ($dllAbi -eq $expectedAbi) -Detail $dllWhy

    # -----------------------------------------------------------------------
    # 4. Run the tools the way a person would: from the payload, at the install root.
    # -----------------------------------------------------------------------
    function Invoke-Native {
        param([string] $Exe, [string[]] $Arguments)
        $path = Join-Path $install ('bin\' + $Exe)
        if (-not (Test-Path -LiteralPath $path)) { return $null }
        # 2>&1 so a Rust `eprintln!` is part of what gets asserted on: for these binaries the
        # diagnosis and the failure are both on stderr, and ignoring one stream is how a broken
        # layout reads as a clean run.
        return (@(& $path @Arguments 2>&1 | ForEach-Object { [string] $_ }) -join [Environment]::NewLine)
    }

    $cli = @('--root', $install)

    Write-Host ''
    Write-Host '--- captured output --------------------------------------------------------' -ForegroundColor DarkGray
    $statusText = Invoke-Native -Exe 'windrec.exe' -Arguments (@('status') + $cli)
    Write-Host '$ bin\windrec.exe status --root <install>' -ForegroundColor DarkGray
    foreach ($line in @($statusText -split [Environment]::NewLine)) { Write-Host ('  ' + $line) }
    Write-Host ''
    $queryArgs = @('query', '--from', $From, '--to', $To) + $cli
    $queryText = Invoke-Native -Exe 'windcapctl.exe' -Arguments $queryArgs
    Write-Host ('$ bin\windcapctl.exe query --from ' + $From + ' --to ' + $To + ' --root <install>') -ForegroundColor DarkGray
    foreach ($line in @($queryText -split [Environment]::NewLine)) { Write-Host ('  ' + $line) }
    Write-Host '-------------------------------------------------------------------------------' -ForegroundColor DarkGray
    Write-Host ''

    if ($null -eq $statusText) {
        Add-Result -Name 'windrec status ran' -Ok $false -Detail 'bin\windrec.exe is not there to run'
    }
    else {
        $statusRows = 0
        if ($statusText -match 'rows total\s+(\d+)') { $statusRows = [int] $matches[1] }
        Add-Result -Name 'windrec status reports a non-zero row count' -Ok ($statusRows -gt 0) `
            -Detail ('rows total = ' + $statusRows + ' (expected ' + $ExpectRows + ')')
        Add-Result -Name 'windrec status agrees with the reference database' -Ok ($statusRows -eq $ExpectRows) `
            -Detail $(if ($statusRows -eq $ExpectRows) { 'indexed through the copy, not the original' } else { 'the index was read but not the rows in it' })
        Add-Result -Name 'windrec status names the month file' `
            -Ok ($monthLabel -and ($statusText -match [regex]::Escape($monthLabel) -and $statusText -match 'month file\(s\)')) `
            -Detail ('looked for ' + $monthLabel + ' and a month-file count in the report')
    }

    if ($null -eq $queryText) {
        Add-Result -Name 'windcapctl query ran' -Ok $false -Detail 'bin\windcapctl.exe is not there to run'
    }
    else {
        # `N of M hits`, both numbers, and anchored to the start of the footer line: the report
        # contains a second "of 9 hits" in its pagination header, and a match against that would
        # pass on a result page that showed one row of nine. 0 of 9 means the window was wrong,
        # 1 of 9 means paging ate the answer, and neither is a passing search.
        $queryFooter = $null
        $shown = -1
        $total = -1
        if ($queryText -match '(?m)^(\d+) of (\d+) hits') {
            $queryFooter = $matches[0]
            $shown = [int] $matches[1]
            $total = [int] $matches[2]
        }
        $hitsOk = ($shown -eq $ExpectRows -and $total -eq $ExpectRows)
        $hitsDetail = 'no "N of M hits" footer at all -- the query never reached an index'
        if ($queryFooter) {
            $hitsDetail = 'footer reads "' + $queryFooter + '"; wanted ' + $ExpectRows + ' shown of ' + $ExpectRows + ' total'
        }
        Add-Result -Name ('windcapctl query returns the ' + $ExpectRows + ' rows') -Ok $hitsOk -Detail $hitsDetail
        Add-Result -Name 'windcapctl query routed to the month file' `
            -Ok ($queryText -match 'month file') `
            -Detail $(if ($queryText -match '\d+ month file\(s\): [^\r\n]*') { $matches[0].Trim() } else { 'the report never named a month file' })
    }

    $statsText = Invoke-Native -Exe 'windcapctl.exe' -Arguments (@('stats') + $cli)
    Add-Result -Name 'windcapctl stats totals the index' `
        -Ok ($statsText -and ($statsText -match ('(?m)^rows\s+' + $ExpectRows + '\s*$'))) `
        -Detail $(if ($statsText -match '(?m)^rows\s+(\d+)') { 'rows = ' + $matches[1] } else { 'stats printed no row total' })

    $doctorText = Invoke-Native -Exe 'windmaint.exe' -Arguments (@('doctor') + $cli)
    # `'' +` because $ExpectRows is an [int] and PowerShell resolves `int + string` by converting the
    # *string* to an int, which throws on " rows" -- a crash dressed up as a typo.
    $doctorRows = '' + $ExpectRows + ' rows'
    Add-Result -Name 'windmaint doctor sees the month file' `
        -Ok ($doctorText -and $monthLabel -and ($doctorText -match [regex]::Escape((Split-Path -Leaf $Source))) -and ($doctorText -match [regex]::Escape($doctorRows))) `
        -Detail $(if ($doctorText) { (Split-Path -Leaf $Source) + ' reported with ' + $ExpectRows + ' rows' } else { 'doctor did not run -- windmaint.exe missing from the payload' })

    $convertText = Invoke-Native -Exe 'windmaint.exe' -Arguments (@('convert', '--dry-run') + $cli)
    Add-Result -Name 'windmaint convert --dry-run says it wrote nothing' `
        -Ok ($convertText -and ($convertText -match 'dry run: nothing written')) `
        -Detail $(if ($convertText -match 'convert: [^\r\n]*') { ($matches[0]).Trim() } else { 'convert --dry-run never reported' })

    # The end-to-end point of shipping the fixtures: on THIS payload, `windsetup check-engines` must
    # not fall back into the "no fixtures" outcome that used to be misreported as "no OCR engine is
    # usable". The exact verdict (usable vs unusable) depends on this machine's Windows language packs,
    # so only the not-testable case -- the false negative this release kills -- is a failure here.
    $ocrCheckText = Invoke-Native -Exe 'windsetup.exe' -Arguments (@('check-engines', '--json') + $cli)
    $ocrOutcome = $null
    $ocrExit = $null
    if ($null -ne $ocrCheckText) {
        if ($ocrCheckText -match '"outcome"\s*:\s*"([^"]*)"') { $ocrOutcome = $matches[1] }
        if ($ocrCheckText -match '"exit_code"\s*:\s*(-?\d+)') { $ocrExit = [int] $matches[1] }
    }
    Add-Result -Name 'windsetup check-engines runs against the shipped fixtures (not the NOT-TESTED fallback)' `
        -Ok (($null -ne $ocrOutcome) -and ($ocrOutcome -ne 'untestable-no-fixtures') -and ($ocrExit -ne 3)) `
        -Detail $(if ($null -eq $ocrOutcome) { 'check-engines --json produced no outcome -- bin\windsetup.exe did not run' }
            else { 'outcome=' + $ocrOutcome + ', exit_code=' + $ocrExit + ' (exit 3 / untestable-no-fixtures would mean __assets__ did not ship)' })

    # -----------------------------------------------------------------------
    # 4b. The two binaries nothing else in this script ever ran, in a root nothing else touched.
    #
    #     Until the tray learned to lay the install out itself, the only way to install a payload was
    #     to read the notes and type `bin\windsetup.exe init` -- and this gate could not see that,
    #     because it never started a GUI-subsystem process. Everything above proves CLI commands
    #     answer when handed `--root`; a person double-clicking an icon hands nothing. These checks are
    #     the delivery promise, asserted: unzip, double-click, and you have an install that records.
    #
    #     The root is a SECOND copy of the payload with no `userdata\` at all, for two reasons. It has
    #     to start uninitialised or there is no first run to prove. And section 5 asserts
    #     `userdata\config_user.json` is absent in `$install` afterwards -- a tree the tray had
    #     initialised would fail that check for the right reason, which is the worst kind of failure a
    #     gate can carry. `$payloadRoot`, not `$install`: the unpack accepts a payload that nests one
    #     level down, and a copy of `$install` would drag the sample database along with it.
    # -----------------------------------------------------------------------
    $gui = Join-Path $work 'gui-root'
    Copy-Item -LiteralPath $payloadRoot -Destination $gui -Recurse -Force
    $guiSvc = Join-Path $gui 'bin\windsvc.exe'
    $guiUi = Join-Path $gui 'bin\winduiweb.exe'
    if (-not (Test-Path -LiteralPath $guiSvc)) {
        Add-Result -Name 'the copied payload holds bin\windsvc.exe' -Ok $false -Detail ('no ' + $guiSvc)
    }
    else {
        # Start-Process, not `&`: a GUI-subsystem binary run with `&` blocks this script until somebody
        # dismisses the icon, and the whole point is that nobody is there to dismiss it. -PassThru is
        # how the pid is known, because every process below is stopped by that number and never by its
        # image name -- a second Windrecorder on this machine is somebody else's recording.
        $tray = Start-Process -FilePath $guiSvc -WorkingDirectory $gui -PassThru
        Add-Result -Name 'windsvc.exe started with no arguments and no --root' -Ok ($null -ne $tray) `
            -Detail ('pid ' + $tray.Id + ' from ' + $guiSvc)

        # Polled, not slept-once: `boot` runs the layout before it takes the message loop, so this is a
        # second or two of process startup -- but a cold file cache and an antivirus first-look are
        # real, and a fixed sleep would make the gate flake on the machine it matters to.
        $userConfig = Join-Path $gui 'userdata\config_user.json'
        $deadline = (Get-Date).AddSeconds(30)
        while (((Get-Date) -lt $deadline) -and (-not (Test-Path -LiteralPath $userConfig))) { Start-Sleep -Milliseconds 500 }
        Add-Result -Name 'the first double-click seeded userdata\config_user.json' -Ok (Test-Path -LiteralPath $userConfig) `
            -Detail $userConfig
        # `result_*` is the slot that makes this more than a directory listing: the interface writes
        # into those folders without checking, `init` is the only thing that creates them, and the
        # recorder's own half-creation never touches them. So this is the line between "a tree exists"
        # and "the interface will not fail the first time somebody generates a wordcloud".
        foreach ($slot in @('userdata\db', 'userdata\videos', 'userdata\result_wordcloud', 'cache\locks', 'cache\logs')) {
            Add-Result -Name ('the first double-click created ' + $slot) -Ok (Test-Path -LiteralPath (Join-Path $gui $slot)) `
                -Detail (Join-Path $gui $slot)
        }

        # The lock is the proof it got past `boot` into the message loop rather than exiting after an
        # error no console would ever show: the file carries the owner's pid, written after the
        # already-running check and before the recorder is spawned.
        # Alive is the assertion that carries, and it is the one this gate used to be missing. A
        # GUI-subsystem binary that prints a usage block and exits 2 leaves nothing observable behind:
        # no console to show the words, no window, no lock, no error -- and `Start-Process` reports
        # success because the OS happily started it. That is precisely how `windsvc.exe` answered a
        # double-click until an empty argv was made to mean `run`.
        $trayAlive = Get-Process -Id $tray.Id -ErrorAction SilentlyContinue
        Add-Result -Name 'the tray is still running, not started-and-exited' -Ok ($null -ne $trayAlive) `
            -Detail $(if ($trayAlive) { 'pid ' + $tray.Id + ' alive after the first-run layout' } else { 'pid ' + $tray.Id + ' is gone: an empty argv was not accepted as `run`'})
        $lockPid = $null
        $trayLock = @(Get-ChildItem -LiteralPath (Join-Path $gui 'cache\locks') -Filter 'LOCK_FILE_TRAY*' -ErrorAction SilentlyContinue)
        if ($trayLock.Count -eq 1) { $lockPid = (Get-Content -LiteralPath $trayLock[0].FullName -Raw).Trim() }
        Add-Result -Name 'the tray took its lock and named its own pid' -Ok ($lockPid -eq [string] $tray.Id) `
            -Detail ('lock says ' + $lockPid + ', Start-Process says ' + $tray.Id)

        # Recording is running because the icon was clicked and nothing else. Queried off the tray's
        # own pid while it is alive to be asked -- the children are then stopped by number, because a
        # gate that left `windrec loop` running would go on filming this machine's screen after it
        # finished reporting.
        $started = Wait-ForTrayChild -TrayPid $tray.Id
        $kids = $started.Kids
        $kidNames = $started.Names
        Add-Result -Name 'the tray started the recorder by itself' -Ok ($kidNames -contains 'windrec.exe') `
            -Detail ('children: ' + (($kidNames | Select-Object -Unique) -join ', '))

        # The interface binary, started the way the tray's own menu starts it: no arguments, finding its
        # own root, in a tree the tray has just initialised. A window handle is the only answer it gives
        # that a script can read -- it prints nothing, serves nothing, and a front-end build that links
        # and exits 0 without ever drawing is exactly what this catches. Retargeted here from the retired
        # egui `windui.exe` on 2026-09-27: the case is "the window the tray opens comes up when clicked
        # in a payload nobody has typed a command into", and that is now this binary's case to carry. The
        # check below is a different question -- that one passes `--exit-after` to prove the Tauri assets
        # are embedded and no dev server is being reached.
        $ui = Start-Process -FilePath $guiUi -WorkingDirectory $gui -PassThru
        $shown = $false
        $uiProc = $null
        $deadline = (Get-Date).AddSeconds(30)
        while (((Get-Date) -lt $deadline) -and (-not $shown)) {
            Start-Sleep -Milliseconds 500
            $uiProc = Get-Process -Id $ui.Id -ErrorAction SilentlyContinue
            if ($null -eq $uiProc) { break }
            if ($uiProc.MainWindowHandle -ne 0) { $shown = $true }
        }
        $uiDetail = 'no window handle within 30 s (a front end that links and never draws, a root nothing resolves, or no WebView2 to draw in)'
        if ($shown) { $uiDetail = 'hwnd ' + $uiProc.MainWindowHandle + ', title "' + $uiProc.MainWindowTitle + '"' }
        elseif ($null -eq $uiProc) { $uiDetail = 'the process exited before it drew anything -- see cache\logs\webui.err' }
        Add-Result -Name 'winduiweb.exe came up with a window on no arguments' -Ok $shown -Detail $uiDetail

        # The HTML window, and the two things only it can prove. A window handle alone is weak here:
        # a Tauri build that was compiled without `custom-protocol` embeds no assets, opens
        # `build.devUrl`, and *still* has a handle — it is showing WebView2's "cannot reach this
        # page". So this check also passes `--exit-after`, which only the real runtime reaches: the
        # binary has to start, parse its argv, and shut itself down on a timer. A machine with no
        # `pnpm dev` running is exactly the case that catches the un-embedded build, and the gate
        # never has one running.
        $web = Join-Path $gui 'bin\winduiweb.exe'
        if (-not (Test-Path -LiteralPath $web)) {
            Add-Result -Name 'winduiweb.exe is in the payload' -Ok $false -Detail ('no ' + $web)
        }
        else {
            $webProc = Start-Process -FilePath $web -ArgumentList @('--tab', 'settings', '--exit-after', '12000') -WorkingDirectory $gui -PassThru
            # Polled, not sampled at a fixed 4 s. The window handle and its caption do not appear in
            # the same instant: the shell creates the HWND, and the title arrives with the first
            # painted frame. A single sleep-once read is a check that fails when the machine is busy
            # and passes when it is idle, which is the worst property a release gate can have -- it
            # teaches whoever runs it next to re-run the script instead of reading the failure.
            $webHwnd = 0
            $webTitle = ''
            $deadline = (Get-Date).AddSeconds(14)
            while ((Get-Date) -lt $deadline) {
                Start-Sleep -Milliseconds 500
                $one = Get-Process -Id $webProc.Id -ErrorAction SilentlyContinue
                if ($null -eq $one) { break }
                if ($one.MainWindowHandle -ne 0 -and $one.MainWindowTitle -ne '') {
                    $webHwnd = $one.MainWindowHandle
                    $webTitle = $one.MainWindowTitle
                    break
                }
            }
            Add-Result -Name 'winduiweb.exe opened its window with no dev server' -Ok ($webHwnd -ne 0) `
                -Detail $(if ($webHwnd -eq 0) { 'no titled window within 14 s -- exited early (no embedded assets? root refused?), or the caption never arrived' } else { 'hwnd ' + $webHwnd + ', title "' + $webTitle + '"' })
            # It was told to leave after 12 s; give it that much and then require it actually did.
            $waited = 0
            while (((Get-Process -Id $webProc.Id -ErrorAction SilentlyContinue)) -and ($waited -lt 25)) {
                Start-Sleep -Seconds 1
                $waited = $waited + 1
            }
            $gone = -not (Get-Process -Id $webProc.Id -ErrorAction SilentlyContinue)
            Add-Result -Name 'winduiweb.exe honoured --exit-after' -Ok $gone `
                -Detail $(if ($gone) { 'self-closed within ' + $waited + ' s of the window appearing' } else { 'still running after --exit-after 12000; the argv never reached the runtime' })
            if (-not $gone) { Stop-Process -Id $webProc.Id -Force -ErrorAction SilentlyContinue }
        }

        # Down by pid, children before the parent is forgotten, and a hard kill of the recorder is
        # survivable by design (`b47b439` journals pending rows so a killed run stops abandoning
        # footage) -- which is the only reason a gate may stop a recording this way at all.
        foreach ($kid in $kids) { Stop-Process -Id $kid.ProcessId -Force -ErrorAction SilentlyContinue }
        Stop-Process -Id $tray.Id -Force -ErrorAction SilentlyContinue
        Stop-Process -Id $ui.Id -Force -ErrorAction SilentlyContinue
        # `Stop-Process -Force` returns once the request has been made, not once the process is gone:
        # the kernel still has to tear the image down, and on a busy machine that takes long enough for
        # the immediate re-check to see it alive. Asserting on that race is how this check failed one
        # run in three while nothing about the payload differed -- a gate that flickers teaches people
        # to re-run it rather than read it. Poll for the exit, then judge.
        $deadline = (Get-Date).AddSeconds(10)
        while ((Get-Date) -lt $deadline) {
            $still = @(Get-Process -Id $tray.Id, $ui.Id -ErrorAction SilentlyContinue)
            if ($still.Count -eq 0) { break }
            Start-Sleep -Milliseconds 400
        }
        $left = @(Get-Process -Id $tray.Id, $ui.Id -ErrorAction SilentlyContinue)
        Add-Result -Name 'the tray and its window left no process behind' `
            -Ok ($left.Count -eq 0) `
            -Detail $(if ($left.Count -eq 0) { 'pid ' + $tray.Id + ' and ' + $ui.Id + ' gone' } else { 'still there after 10 s: ' + (($left | ForEach-Object { $_.Name + '(' + $_.Id + ')' }) -join ', ') })
    }

    # -----------------------------------------------------------------------
    # 4c. The door. `bin\Windrecorder.exe` is the file both READMEs and RELEASE-NOTES.txt tell a
    #     stranger to click, so this gate clicks it. A third copy of the payload is started through
    #     the launcher and through nothing else, and has to end up laid out, locked and recording.
    #
    #     A separate root, not more assertions inside `$gui`: by the time section 4b finishes there is
    #     already a tray holding the lock in `$gui`, and a launcher whose child loses the
    #     single-instance race immediately proves nothing except that the lock works. What is being
    #     proved here is the delivery promise through the *named* entry point.
    #
    #     A GUI-subsystem child is neither waited for nor captured by `&` -- measured on this machine,
    #     `& Windrecorder.exe doctor` returned an empty string and an unset `$LASTEXITCODE` while the
    #     report was still being written. `Start-Process -Wait` with redirected files is the only
    #     honest way to ask what it printed and what it exited with, so every call below goes through
    #     the `Invoke-Redirected` helper defined beside `Add-Result` at the top of this file.
    # -----------------------------------------------------------------------
    $door = Join-Path $work 'door-root'
    Copy-Item -LiteralPath $payloadRoot -Destination $door -Recurse -Force
    $launcherExe = Join-Path $door 'bin\Windrecorder.exe'
    $doorTrayExe = Join-Path $door 'bin\windsvc.exe'
    if (-not (Test-Path -LiteralPath $launcherExe)) {
        Add-Result -Name 'the copied payload holds bin\Windrecorder.exe' -Ok $false -Detail ('no ' + $launcherExe)
    }
    else {
        # The one thing the launcher answers itself, because it is the one thing that is about the file
        # a person can see in Explorer: a support ticket about a double-click names the icon clicked.
        $ver = Invoke-Redirected -Exe $launcherExe -Arguments @('--version') -Tag 'door-version'
        Add-Result -Name 'Windrecorder.exe --version speaks for the file that was clicked' `
            -Ok (($ver.Code -eq 0) -and ($ver.Text -like 'Windrecorder *')) `
            -Detail ('exit ' + $ver.Code + ', reads: ' + $ver.Text.Trim())

        # Everything else belongs to the tray, unchanged. Both halves are asserted -- the same exit code
        # and the same bytes -- because a launcher that rewrote an argument, or swallowed a failure into
        # exit 0, would still look like it worked from the double-click.
        $viaDoor = Invoke-Redirected -Exe $launcherExe -Arguments @('doctor', '--root', $door) -Tag 'door-doctor'
        $viaTray = Invoke-Redirected -Exe $doorTrayExe -Arguments @('doctor', '--root', $door) -Tag 'tray-doctor'
        Add-Result -Name 'Windrecorder.exe doctor exits exactly as windsvc.exe doctor does' `
            -Ok (($viaDoor.Code -eq $viaTray.Code) -and ($viaDoor.Code -eq 0)) `
            -Detail ('through the door: ' + $viaDoor.Code + ', through the tray: ' + $viaTray.Code)
        Add-Result -Name 'and prints the tray''s own report, byte for byte' `
            -Ok ($viaDoor.Text -eq $viaTray.Text) `
            -Detail ($viaDoor.Text.Length.ToString() + ' chars via the door vs ' + $viaTray.Text.Length.ToString() + ' via windsvc')

        # The double-click. Nothing on this command line but the file itself.
        $doorProc = Start-Process -FilePath $launcherExe -WorkingDirectory $door -PassThru
        $doorExited = $doorProc.WaitForExit(20000)
        Add-Result -Name 'the launcher starts the tray and gets out of the way' `
            -Ok ($doorExited -and ($null -eq (Get-Process -Id $doorProc.Id -ErrorAction SilentlyContinue))) `
            -Detail $(if (-not $doorExited) { 'still running after 20 s: a launcher that sits in front of the tray is a second handle on the icon' }
                else { 'gone, exit ' + $doorProc.ExitCode + '; the tray it started is asserted below' })

        $doorConfig = Join-Path $door 'userdata\config_user.json'
        $deadline = (Get-Date).AddSeconds(40)
        while (((Get-Date) -lt $deadline) -and (-not (Test-Path -LiteralPath $doorConfig))) { Start-Sleep -Milliseconds 500 }
        Add-Result -Name 'one click on Windrecorder.exe laid out the install' -Ok (Test-Path -LiteralPath $doorConfig) `
            -Detail $doorConfig

        # The lock file, not a process name, is what identifies the tray this click started: a second
        # Windrecorder on this machine is somebody else's recording, so nothing below is ever looked up
        # by image name.
        $doorTrayPid = $null
        $deadline = (Get-Date).AddSeconds(25)
        while (((Get-Date) -lt $deadline) -and ($null -eq $doorTrayPid)) {
            $lockFile = @(Get-ChildItem -LiteralPath (Join-Path $door 'cache\locks') -Filter 'LOCK_FILE_TRAY*' -ErrorAction SilentlyContinue)
            if ($lockFile.Count -ge 1) {
                $held = (Get-Content -LiteralPath $lockFile[0].FullName -Raw -ErrorAction SilentlyContinue)
                if ($held) {
                    $candidate = [int] $held.Trim()
                    if (Get-Process -Id $candidate -ErrorAction SilentlyContinue) { $doorTrayPid = $candidate }
                }
            }
            if ($doorTrayPid) { break }
            Start-Sleep -Milliseconds 500
        }
        $doorTrayOk = $false
        if ($doorTrayPid) {
            $owner = Get-CimInstance Win32_Process -Filter ('ProcessId=' + $doorTrayPid) -ErrorAction SilentlyContinue
            $doorTrayOk = ($owner.Name -eq 'windsvc.exe') -and ($owner.ExecutablePath -eq $doorTrayExe)
        }
        Add-Result -Name 'the running tray is windsvc.exe from this tree, started by the door' `
            -Ok $doorTrayOk `
            -Detail $(if ($doorTrayPid) { 'lock pid ' + $doorTrayPid } else { 'no live pid in cache\locks\LOCK_FILE_TRAY* within 25 s' })

        $doorKids = @()
        if ($doorTrayPid) { $doorKids = (Wait-ForTrayChild -TrayPid $doorTrayPid).Kids }
        Add-Result -Name 'and the recorder is running under it' `
            -Ok (@($doorKids | Where-Object { $_.Name -eq 'windrec.exe' }).Count -ge 1) `
            -Detail $(if (-not $doorTrayPid) { 'not asked: there was no tray pid to enumerate children of' }
                else { 'children: ' + ((@($doorKids | ForEach-Object { $_.Name }) | Select-Object -Unique) -join ', ') })

        # Down by number, and the sweep is the assertion: the tray spawns `windrec` a moment after its
        # own children are first listed, so killing what was enumerable at one instant leaves a
        # recorder running. Everything whose image lives under this root is stopped, then counted.
        foreach ($kid in $doorKids) { Stop-Process -Id $kid.ProcessId -Force -ErrorAction SilentlyContinue }
        if ($doorTrayPid) { Stop-Process -Id $doorTrayPid -Force -ErrorAction SilentlyContinue }
        $deadline = (Get-Date).AddSeconds(12)
        $doorLeft = @()
        while ((Get-Date) -lt $deadline) {
            $doorLeft = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | Where-Object { $_.ExecutablePath -like ($door + '*') })
            if ($doorLeft.Count -eq 0) { break }
            foreach ($straggler in $doorLeft) { Stop-Process -Id $straggler.ProcessId -Force -ErrorAction SilentlyContinue }
            Start-Sleep -Milliseconds 500
        }
        Add-Result -Name 'the door and everything it started left no process behind' `
            -Ok ($doorLeft.Count -eq 0) `
            -Detail $(if ($doorLeft.Count -eq 0) { 'nothing under ' + $door + ' is running' } else { 'still running: ' + (($doorLeft | ForEach-Object { $_.Name + '(' + $_.Id + ')' }) -join ', ') })
    }

    # -----------------------------------------------------------------------
    # 4d. The launcher on its own, with no tray to find. The failure the double-click cannot show is
    #     the one this proves: a GUI-subsystem binary with no console that cannot find its tray has to
    #     say so somewhere, and the console path is the half a script can read.
    # -----------------------------------------------------------------------
    $lonely = Join-Path $work 'door-lonely'
    New-Item -ItemType Directory -Path (Join-Path $lonely 'bin') -Force | Out-Null
    if (-not (Test-Path -LiteralPath $launcherExe)) {
        Add-Result -Name 'a launcher with no windsvc.exe fails loudly, naming the file and where it looked' -Ok $null `
            -Detail 'not asked: bin\Windrecorder.exe was not in the payload, and 4c already said so'
    }
    else {
        Copy-Item -LiteralPath $launcherExe -Destination (Join-Path $lonely 'bin\Windrecorder.exe') -Force
        $missing = Invoke-Redirected -Exe (Join-Path $lonely 'bin\Windrecorder.exe') -Arguments @('doctor') -Tag 'door-lonely'
        Add-Result -Name 'a launcher with no windsvc.exe fails loudly, naming the file and where it looked' `
            -Ok (($missing.Code -eq 1) -and ($missing.Text -like '*windsvc.exe was not found*') -and ($missing.Text -like '*Looked in*')) `
            -Detail ('exit ' + $missing.Code + ': ' + (($missing.Text -split "`n")[0]))
    }

    # -----------------------------------------------------------------------
    # 5. Two negative checks: nothing was created that should not be, and nothing on the source
    #    machine was touched. `doctor` and `--dry-run` both claim to be side-effect free, and a
    #    maintenance tool that lies about that is a data-loss bug waiting for someone's real install.
    # -----------------------------------------------------------------------
    foreach ($shouldNotExist in @('cache', 'cache_screenshot', 'userdata\videos', 'userdata\config_user.json')) {
        $probe = Join-Path $install $shouldNotExist
        Add-Result -Name ('dry run created no ' + $shouldNotExist) -Ok (-not (Test-Path -LiteralPath $probe)) `
            -Detail $(if (Test-Path -LiteralPath $probe) { 'it is there: ' + $probe } else { 'absent, as promised' })
    }

    $sourceAfter = Get-Item -LiteralPath $Source
    $sourceSiblingsAfter = @(Get-ChildItem -LiteralPath (Split-Path -Parent $Source) -File | ForEach-Object { $_.Name }).Count
    $untouched = ($sourceBefore.LastWriteTime -eq $sourceAfter.LastWriteTime) -and
                 ($sourceBefore.Length -eq $sourceAfter.Length) -and
                 ($sourceSiblingsBefore -eq $sourceSiblingsAfter)
    Add-Result -Name 'the source install was not written to' -Ok $untouched `
        -Detail ($Source + '  mtime ' + (Get-Date -Format 's' -Date $sourceAfter.LastWriteTime) +
                 ', ' + $sourceSiblingsAfter + ' file(s) beside it (was ' + $sourceSiblingsBefore + ')')

    # The copy did take its _TEMP_READ.db sibling -- proof the queries ran against *this* tree.
    Add-Result -Name 'a _TEMP_READ.db sits beside the temp copy' `
        -Ok (Test-Path -LiteralPath ($dbCopy + '_TEMP_READ.db')) `
        -Detail 'which is exactly what must never appear next to the original'
}
catch {
    Write-Host ''
    Write-Host ('  smoke.ps1 aborted: ' + $_.Exception.Message) -ForegroundColor Red
    # The line, not just the message: this script drives four external binaries and their output is
    # where a failure starts but rarely where it is, and "something threw" is not actionable at 3am.
    if ($_.InvocationInfo -and $_.InvocationInfo.ScriptLineNumber) {
        Write-Host ('  at smoke.ps1 line ' + $_.InvocationInfo.ScriptLineNumber + ' (in ' + $_.InvocationInfo.MyCommand + ')') -ForegroundColor Red
    }
    $fail++
}
finally {
    Write-Host ''
    if ($work -and -not $Keep) {
        try {
            Remove-TempTree -Path $work -Why 'smoke tree'
        }
        catch {
            # A file still locked by a process that has not quite exited is not a reason to leave
            # the tree lying around unexplained.
            Write-Host ('  could not clean ' + $work + ': ' + $_.Exception.Message) -ForegroundColor Yellow
        }
    }
    elseif ($work) {
        Write-Host ('  kept     ' + $work + '  (-Keep)') -ForegroundColor DarkGray
    }
    if ($savedOutputEncoding) {
        try { [Console]::OutputEncoding = $savedOutputEncoding } catch { }
    }
}

# ---------------------------------------------------------------------------
# The verdict: one line per check, then a number, then the exit code.
# ---------------------------------------------------------------------------
Write-Host ''
Write-Host 'assertions' -ForegroundColor Cyan
foreach ($result in $results) {
    $label = 'FAIL'
    $color = 'Red'
    if ($null -eq $result.Ok) {
        $label = 'SKIP'
        $color = 'Yellow'
    }
    elseif ($result.Ok) {
        $label = 'PASS'
        $color = 'Green'
    }
    Write-Host ('  ' + ('{0,-8}' -f $label) + ('{0,-52}' -f $result.Name) + $result.Detail) -ForegroundColor $color
}
Write-Host ''
$verdictColor = 'Red'
if ($fail -eq 0) { $verdictColor = 'Green' }
Write-Host ('  ' + $pass + ' passed, ' + $fail + ' failed, ' + $skip + ' skipped, ' + ($pass + $fail + $skip) + ' checks') -ForegroundColor $verdictColor
if ($fail -gt 0) {
    Write-Host '  the shipped layout is not proven. Do not hand this zip to anyone.' -ForegroundColor Red
    exit 1
}
if ($skip -gt 0) {
    # Say it out loud: a run with nothing to compare against is not the same claim as a run that
    # checked everything.
    Write-Host ('  the layout held up, but ' + $skip + ' check(s) could not be made -- see the SKIP lines above.') -ForegroundColor Yellow
    exit 0
}
Write-Host '  the layout in that zip is what the runtime will find. It works.' -ForegroundColor Green
exit 0
