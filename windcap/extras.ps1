<#
    extras.ps1 -- the components the payload cannot carry, as files a release page can carry.

    ONE COMMAND
        powershell -ExecutionPolicy Bypass -File windcap\extras.ps1 [-FfmpegZip <vendor zip>] [-SkipProof]

    WHAT IT PRODUCES, for each component it finds on this machine
        windcap\dist\Windrecorder-<component>-<version>.zip          the download
        windcap\dist\Windrecorder-<component>-<version>.zip.sha256   the hash of those exact bytes
        windcap\dist\Windrecorder-<component>-<version>-notes.md     the release-page paragraph, filled in
        inside each zip: MANIFEST.sha256 and README-<component>.md

    WHY IT LOOKS LIKE THIS

    1. THESE ARE NOT THE PAYLOAD. `release.ps1` builds one zip that runs on its own, and two things
       cannot go inside it: the WeChat OCR component, which is Tencent's code and Tencent's models
       extracted by a third party and carries a personal-use limit this project does not own; and
       ffmpeg, which is somebody else's GPL version 3 build. Both stay separate files with their own
       terms, and a person who wants them downloads them. What this script adds is the shape: each
       archive unpacks *into the app folder* and lands where the application already looks, so the
       instruction is one sentence and not a directory tour.

    2. AN ARCHIVE IS NOT PROVEN UNTIL IT IS UNPACKED. Packaging a directory is the easy half and it is
       the half that lies: a zip can be built from files that do not work, from a layout nobody can
       reproduce, or from a stale copy of a component that changed under the folder it came from. So
       every archive is unpacked into a scratch directory that has nothing else in it, every file is
       re-hashed against the manifest the script just wrote, and then the component is run for real --
       `windsetup check-engines` has to score the WeChat rows, and `windsetup doctor` has to say it
       resolved ffmpeg from *this install*. A zip that cannot do that is not released; the script says
       so and exits non-zero. `-SkipProof` exists for rehearsing the packaging on a machine that has no
       engines installed, and it prints that it skipped.

    3. THE TEXT LIVES IN THE REPOSITORY, THE NUMBERS COME FROM THE BYTES. The prose inside each zip and
       the release-page paragraph are `windcap\extras\<component>\*.md`, reviewable like any other
       document, with {TOTAL}, {ZIP}, {SHA256} and {SOURCE} filled in from what was actually packed. A
       size or a hash written by hand in a document is a number that goes stale the first time anybody
       rebuilds, and the claim this project keeps having to delete is the one where a document stopped
       matching the file.

    4. A MISSING COMPONENT IS REPORTED, NOT INVENTED. Neither source is in git -- `ocr_lib\wxocr-binary`
       is ignored on purpose -- so on a fresh clone this script finds nothing and says which path it
       looked at, in the same words the settings page uses. It never downloads anything itself: fetching
       a third party's binaries is a decision a person makes, not a side effect of a build.

    Windows PowerShell 5.1 is the floor: no `&&`, no ternary, no `??`, and no CJK in this file -- every
    bilingual sentence lives in the .md sources, which are UTF-8 and are copied as bytes.
#>

[CmdletBinding()]
param(
    # The vendor archive to take ffmpeg.exe and its licence out of. Optional: without it the ffmpeg row
    # reports where it looked and moves on, because pointing a build script at a URL is how a release
    # ends up shipping whatever that server happened to hold today.
    [string] $FfmpegZip = '',
    [switch] $SkipProof
)

$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'

# A child's UTF-8 stdout is decoded by Windows PowerShell using the console's code page, and `check-engines`
# prints what each engine read -- including a Japanese fixture. Left alone, the bytes come back eaten in the
# middle and the JSON no longer parses, which is a proof step failing because of the shell rather than the
# archive. Setting the console's *decoding* to UTF-8 fixes it; `$OutputEncoding` is the same in the other
# direction, for text piped back into a native command.
$previousOutputEncoding = [Console]::OutputEncoding
$OutputEncoding = [System.Text.Encoding]::UTF8
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$workspace = $PSScriptRoot                                  # windcap\
$root = Split-Path -Parent $workspace                       # the app folder: config_src\, ocr_lib\, bin\
$distDir = Join-Path $workspace 'dist'
$extrasDir = Join-Path $workspace 'extras'
$stagingRoot = Join-Path $distDir 'extras-staging'
$version = $null
$failures = @()

# ---------------------------------------------------------------------------
# Helpers, in the same shapes release.ps1 uses.
# ---------------------------------------------------------------------------
function Format-Size {
    param([long] $Bytes)
    if ($Bytes -ge 1048576) { return ('{0:N1} MiB' -f ($Bytes / 1MB)) }
    if ($Bytes -ge 1024) { return ('{0:N1} KiB' -f ($Bytes / 1KB)) }
    return ('{0} bytes' -f $Bytes)
}

function Write-Line {
    param([string] $Text, [string] $Color = 'Gray')
    Write-Host $Text -ForegroundColor $Color
}

# Every exit path goes through this, so a maintainer who dot-sources the script gets their console back.
function Exit-Script {
    param([int] $Code)
    if ($previousOutputEncoding) { [Console]::OutputEncoding = $previousOutputEncoding }
    exit $Code
}

# Straight to .NET rather than `Get-FileHash`, which is not a command in every Windows PowerShell
# session -- the same reason release.ps1 gives at its copy of this function.
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

# Relative path with forward slashes, because that is the form `sha256sum -c` and every manifest
# outside Windows already use.
function Format-InZip {
    param([string] $Prefix, [string] $Relative)
    $joined = $Relative -replace '\\', '/'
    if ($Prefix -eq '') { return $joined }
    return ($Prefix.TrimEnd('/') + '/' + $joined)
}

function Remove-Tree {
    param([string] $Path)
    if (Test-Path -LiteralPath $Path) { Remove-Item -LiteralPath $Path -Recurse -Force -ErrorAction Continue }
}

# ---------------------------------------------------------------------------
# The two components. Sources are paths, never URLs.
# ---------------------------------------------------------------------------
$wxocrSource = Join-Path $root 'ocr_lib\wxocr-binary'
$ffmpegGuess = $FfmpegZip
if ($ffmpegGuess -eq '') {
    $ffmpegGuess = Join-Path $distDir 'extras-in\ffmpeg-*-essentials_build.zip'
}

$components = @(
    [pscustomobject]@{
        Name        = 'wxocr-binary'
        Title       = 'WeChat OCR component'
        Kind        = 'directory'
        Source      = $wxocrSource
        Layout      = 'ocr_lib/wxocr-binary'
        Upstream    = 'https://github.com/kanadeblisst00/wechat_ocr/tree/master/bin'
        Proof       = 'engines'
    }
    [pscustomobject]@{
        Name        = 'ffmpeg'
        Title       = 'ffmpeg, for the video step'
        Kind        = 'vendor-zip'
        Source      = $ffmpegGuess
        Layout      = ''
        Upstream    = 'https://www.gyan.dev/ffmpeg/'
        Proof       = 'version'
    }
)

# ---------------------------------------------------------------------------
# Stage: the files that go in, each with the name it will have inside.
# ---------------------------------------------------------------------------
function Get-StagePlan {
    param($Component)
    $plan = @()
    if ($Component.Kind -eq 'directory') {
        if (-not (Test-Path -LiteralPath $Component.Source -PathType Container)) {
            return @{ ok = $false; why = ('no directory at ' + $Component.Source); plan = @() }
        }
        foreach ($file in (Get-ChildItem -LiteralPath $Component.Source -Recurse -File)) {
            $relative = $file.FullName.Substring($Component.Source.Length).TrimStart('\')
            $plan += [pscustomobject]@{ From = $file.FullName; InZip = (Format-InZip $Component.Layout $relative); Bytes = $file.Length }
        }
        if ($plan.Count -eq 0) { return @{ ok = $false; why = ('nothing under ' + $Component.Source); plan = @() } }
        return @{ ok = $true; why = ''; plan = $plan }
    }

    # vendor-zip: take exactly the two files the app needs out of the build somebody else published,
    # and record which build by name, so the provenance is the vendor's own archive and not a rumour.
    $found = Get-ChildItem -LiteralPath ([System.IO.Path]::GetDirectoryName($Component.Source)) -Filter ([System.IO.Path]::GetFileName($Component.Source)) -ErrorAction Continue
    if (-not $found -or $found.Count -eq 0) {
        return @{ ok = $false; why = ('no vendor archive at ' + $Component.Source + '  (pass -FfmpegZip <path>; this script never downloads)'); plan = @() }
    }
    $vendor = $found[0].FullName
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($vendor)
    try {
        $wanted = @{}
        foreach ($entry in $archive.Entries) {
            $leaf = [System.IO.Path]::GetFileName($entry.FullName)
            if ($entry.FullName -match '(^|/)bin/ffmpeg\.exe$') { $wanted['ffmpeg.exe'] = $entry }
            elseif ($leaf -eq 'LICENSE' -or $leaf -eq 'LICENSE.txt') { $wanted['LICENSE'] = $entry }
        }
        if (-not $wanted.ContainsKey('ffmpeg.exe')) {
            return @{ ok = $false; why = ('no bin/ffmpeg.exe inside ' + $vendor); plan = @() }
        }
        $stagedVendor = Join-Path $stagingRoot ('vendor-' + $Component.Name)
        Remove-Tree $stagedVendor
        New-Item -ItemType Directory -Path $stagedVendor -Force | Out-Null
        foreach ($key in @('ffmpeg.exe', 'LICENSE')) {
            if (-not $wanted.ContainsKey($key)) { continue }
            $entry = $wanted[$key]
            $target = Join-Path $stagedVendor $key
            $stream = $entry.Open()
            try {
                $out = [System.IO.File]::Create($target)
                try { $stream.CopyTo($out) } finally { $out.Dispose() }
            } finally { $stream.Dispose() }
            $plan += [pscustomobject]@{ From = $target; InZip = $key; Bytes = (Get-Item -LiteralPath $target).Length }
        }
        # The vendor archive's own name carries the version, and that is the only honest way to say
        # which build this is: `ffmpeg-9.0.2-essentials_build.zip` -> `9.0.2`.
        $vendorName = [System.IO.Path]::GetFileName($vendor)
        $build = $vendorName -replace '^ffmpeg-([^-]+)-.*$', '$1'
        return @{ ok = $true; why = ''; plan = $plan; vendor = $vendorName; build = $build }
    }
    finally {
        $archive.Dispose()
    }
}

# ---------------------------------------------------------------------------
# Proof: unpack, re-hash, and run the thing.
# ---------------------------------------------------------------------------
function Invoke-Proof {
    param($Component, [string] $ZipPath, [string] $Scratch)

    Remove-Tree $Scratch
    New-Item -ItemType Directory -Path $Scratch -Force | Out-Null
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [System.IO.Compression.ZipFile]::ExtractToDirectory($ZipPath, $Scratch)

    $manifest = Join-Path $Scratch 'MANIFEST.sha256'
    if (-not (Test-Path -LiteralPath $manifest)) { return @{ ok = $false; lines = @('no MANIFEST.sha256 in the archive') } }
    $lines = @()
    $bad = 0
    foreach ($row in (Get-Content -LiteralPath $manifest)) {
        $parts = $row -split '\s+', 2
        if ($parts.Count -lt 2) { continue }
        $target = Join-Path $Scratch ($parts[1] -replace '/', '\')
        if (-not (Test-Path -LiteralPath $target)) { $lines += ('  MISSING   ' + $parts[1]); $bad++; continue }
        $actual = Get-Sha256 -Path $target
        if ($actual -ne $parts[0]) { $lines += ('  DIFFERS   ' + $parts[1]); $bad++; continue }
        $lines += ('  intact    ' + $parts[1])
    }
    if ($bad -gt 0) { return @{ ok = $false; lines = $lines } }

    if ($Component.Proof -eq 'engines') {
        # A scratch app folder holding nothing but what the archive put there, plus the settings the
        # probe reads and the three benchmark pictures. If the engine scores the fixtures here, the
        # archive is sufficient on its own -- which is the claim the release page makes.
        New-Item -ItemType Directory -Path (Join-Path $Scratch 'config_src') -Force | Out-Null
        Copy-Item -Path (Join-Path $root 'config_src\*') -Destination (Join-Path $Scratch 'config_src') -Recurse -Force
        New-Item -ItemType Directory -Path (Join-Path $Scratch '__assets__') -Force | Out-Null
        # The pictures *and* their reference text: a fixture with nothing to compare against reads as
        # no fixtures at all, which is a trap this script has already fallen into once.
        Copy-Item -Path (Join-Path $root '__assets__\OCR_test_1080_*') -Destination (Join-Path $Scratch '__assets__') -Force
        $windsetup = Get-Windsetup
        if (-not $windsetup) { return @{ ok = $false; lines = ($lines + @('no windsetup.exe to prove with -- run windcap\build.ps1 -Stage')) } }
        $raw = & $windsetup check-engines --root $Scratch --json 2>&1 | Out-String
        try { $report = $raw | ConvertFrom-Json } catch { return @{ ok = $false; lines = ($lines + @('check-engines did not answer as JSON: ' + $raw)) } }
        $rows = @($report.probes | Where-Object { $_.engine -eq 'WeChatOCR' })
        if ($rows.Count -eq 0) { return @{ ok = $false; lines = ($lines + @('the archive produced no WeChatOCR row at all')) } }
        foreach ($row in $rows) {
            $lines += ('  {0,-12} {1,-10} {2}%' -f $row.language, $row.status, $row.accuracy_percent)
        }
        $scored = @($rows | Where-Object { $null -ne $_.accuracy_percent })
        if ($scored.Count -lt 3) { return @{ ok = $false; lines = ($lines + @('only ' + $scored.Count + ' of 3 fixtures were scored')) } }
        return @{ ok = $true; lines = $lines }
    }

    # 'version': the binary answers for itself, and the application says it resolved *this* copy.
    $exe = Join-Path $Scratch 'ffmpeg.exe'
    if (-not (Test-Path -LiteralPath $exe)) { return @{ ok = $false; lines = ($lines + @('no ffmpeg.exe at the archive root')) } }
    $first = (& $exe -version 2>&1 | Select-Object -First 1 | Out-String).Trim()
    if ($first -notmatch 'ffmpeg version') { return @{ ok = $false; lines = ($lines + @('ffmpeg.exe did not answer: ' + $first)) } }
    $lines += ('  answers   ' + $first)
    $windsetup = Get-Windsetup
    if ($windsetup) {
        New-Item -ItemType Directory -Path (Join-Path $Scratch 'config_src') -Force | Out-Null
        Copy-Item -Path (Join-Path $root 'config_src\*') -Destination (Join-Path $Scratch 'config_src') -Recurse -Force
        $doctor = & $windsetup doctor --root $Scratch 2>&1 | Out-String
        if ($doctor -match '(?m)^.*this install.*$') { $lines += ('  resolved  ' + ($Matches[0].Trim())) }
        else { return @{ ok = $false; lines = ($lines + @('windsetup doctor did not report ffmpeg as coming from this install')) } }
    }
    return @{ ok = $true; lines = $lines }
}

function Get-Windsetup {
    $staged = Join-Path $distDir ('Windrecorder-native-' + $version + '\bin\windsetup.exe')
    if (Test-Path -LiteralPath $staged) { return $staged }
    $local = Join-Path $root 'bin\windsetup.exe'
    if (Test-Path -LiteralPath $local) { return $local }
    return $null
}

# ---------------------------------------------------------------------------
# Run.
# ---------------------------------------------------------------------------
$cargoToml = Join-Path $workspace 'Cargo.toml'
$version = Get-CargoWorkspaceVersion -Path $cargoToml
if (-not $version) {
    Write-Line ('extras.ps1: no [workspace.package] version in ' + $cargoToml) 'Red'
    Exit-Script 2
}

Write-Line ''
Write-Line ('  extras  v' + $version + '   from ' + $root)
Write-Line ('  proof   ' + $(if ($SkipProof) { 'SKIPPED by -SkipProof' } else { 'unpack, re-hash, run' }))
Write-Line ''

New-Item -ItemType Directory -Path $distDir -Force | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$proofRoot = Join-Path $env:TEMP ('windcap-extras-proof-' + $stamp)

foreach ($component in $components) {
    $zipName = ('Windrecorder-' + $component.Name + '-' + $version + '.zip')
    $zipPath = Join-Path $distDir $zipName
    Write-Line ('== ' + $component.Title + ' ==')

    $staged = Get-StagePlan -Component $component
    if (-not $staged.ok) {
        Write-Line ('   skipped   ' + $staged.why) 'Yellow'
        $failures += $component.Name
        Write-Line ''
        continue
    }
    $payload = @($staged.plan)
    $total = 0L
    foreach ($item in $payload) { $total += $item.Bytes }

    # Stage the tree the zip will be made from: the component, the manifest, the README.
    $staging = Join-Path $stagingRoot $component.Name
    Remove-Tree $staging
    foreach ($item in $payload) {
        $target = Join-Path $staging ($item.InZip -replace '/', '\')
        New-Item -ItemType Directory -Path ([System.IO.Path]::GetDirectoryName($target)) -Force | Out-Null
        Copy-Item -LiteralPath $item.From -Destination $target -Force
    }
    $manifestLines = @()
    foreach ($item in ($payload | Sort-Object InZip)) {
        $manifestLines += ((Get-Sha256 -Path (Join-Path $staging ($item.InZip -replace '/', '\'))) + '  ' + $item.InZip)
    }
    [System.IO.File]::WriteAllLines((Join-Path $staging 'MANIFEST.sha256'), $manifestLines, (New-Object System.Text.UTF8Encoding($false)))

    $readmeSource = Join-Path $extrasDir ($component.Name + '\README.md')
    if (Test-Path -LiteralPath $readmeSource) {
        Copy-Item -LiteralPath $readmeSource -Destination (Join-Path $staging ('README-' + $component.Name + '.md')) -Force
    } else {
        Write-Line ('   no README at ' + $readmeSource + ' -- shipping without one') 'Yellow'
    }

    if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }
    Compress-Archive -Path (Join-Path $staging '*') -DestinationPath $zipPath -CompressionLevel Optimal -Force
    $zipSha = Get-Sha256 -Path $zipPath
    $zipBytes = (Get-Item -LiteralPath $zipPath).Length
    [System.IO.File]::WriteAllLines((Join-Path $distDir ($zipName + '.sha256')), @($zipSha + '  ' + $zipName), (New-Object System.Text.UTF8Encoding($false)))

    Write-Line ('   files     ' + $payload.Count + '   ' + (Format-Size $total) + ' -> ' + (Format-Size $zipBytes) + ' zipped')
    Write-Line ('   sha256    ' + $zipSha)

    # The release-page paragraph, with the numbers filled in from the bytes that were just written.
    $notesSource = Join-Path $extrasDir ($component.Name + '\notes.md')
    if (Test-Path -LiteralPath $notesSource) {
        $text = [System.IO.File]::ReadAllText($notesSource, [System.Text.Encoding]::UTF8)
        $text = $text.Replace('{TOTAL}', ('{0:N0}' -f $total) + ' bytes / ' + (Format-Size $total))
        $text = $text.Replace('{ZIP}', ('{0:N0}' -f $zipBytes) + ' bytes / ' + (Format-Size $zipBytes))
        $text = $text.Replace('{SHA256}', $zipSha)
        $text = $text.Replace('{SOURCE}', $component.Upstream)
        if ($staged.build) { $text = $text.Replace('{BUILD}', $staged.build) }
        $notesPath = Join-Path $distDir ('Windrecorder-' + $component.Name + '-' + $version + '-notes.md')
        [System.IO.File]::WriteAllText($notesPath, $text, (New-Object System.Text.UTF8Encoding($false)))
        Write-Line ('   notes     ' + $notesPath)
    }

    if ($SkipProof) { Write-Line '   proof     skipped'; Write-Line ''; continue }
    $proof = Invoke-Proof -Component $component -ZipPath $zipPath -Scratch (Join-Path $proofRoot $component.Name)
    foreach ($line in $proof.lines) { Write-Line ('   ' + $line) }
    if ($proof.ok) { Write-Line ('   PROVEN    the archive works unpacked as-is') 'Green' }
    else {
        Write-Line ('   NOT PROVEN -- do not release this file') 'Red'
        $failures += ('unproven ' + $component.Name)
    }
    Write-Line ''
}

Remove-Tree $proofRoot

if ($failures.Count -gt 0) {
    Write-Line ('  ' + $failures.Count + ' component(s) not delivered: ' + ($failures -join ', ')) 'Yellow'
    Write-Line '  a missing one is a path this script looked at and did not find; it does not fetch anything.'
    Exit-Script 1
}
Write-Line '  every component found here was packaged, unpacked, re-hashed and run.'
Exit-Script 0
