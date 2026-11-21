# IndexFox M4 Windows end-to-end smoke harness.
#
# Run (Windows PowerShell 5.1, from the repo root or any directory):
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\test-m4-windows.ps1
#
# From cmd.exe:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\test-m4-windows.ps1 -Zip target\IndexFox-windows-x64-portable.zip
#
# From WSL (C: is read-only here, so the script and ZIP may stay in WSL):
#   /mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe -NoProfile `
#     -ExecutionPolicy Bypass -File `
#     '\\wsl.localhost\Ubuntu-26.04\home\developer\work\IndexFox\scripts\test-m4-windows.ps1' `
#     -Zip '\\wsl.localhost\Ubuntu-26.04\home\developer\work\IndexFox\target\IndexFox-windows-x64-portable.zip'
#
# Modes:
#   default   Strict M4 package: runtime\webview2 fixed runtime, ocr\tesseract.exe,
#             ocr\tessdata\{chi_sim,eng}.traineddata, bundled pdfium.dll, fuzzy
#             Q10, scanned/mixed PDF locator+origin, fixed-runtime process proof.
#   -Legacy   Backward-compatible smoke for an M0/M3 ZIP (no fixed runtime/OCR):
#             index text+PDF, native PDF page locator, GUI title/profile isolation.
#             M4-only checks are reported SKIP. Use this while the final M4 ZIP is
#             still being built.
#   -DryRun   Validate token/ZIP/layout and print the planned child commands only.
#             Combine with -Legacy for the current M3 ZIP.
#
# Isolation and safety:
#   - Work directory is new and unique under %TEMP%\IndexFox-M4-E2E\<id>. Cleanup
#     removes only that exact directory.
#   - Child processes get LOCALAPPDATA/APPDATA/TEMP/TMP and the WebView2 user-data
#     folder under the work directory. TESSERACT_PATH, PDFIUM_LIB_PATH,
#     WEBVIEW2_BROWSER_EXECUTABLE_FOLDER and TESSDATA_PREFIX are removed from the
#     child environment only; the host environment is restored immediately.
#   - Only the exact GUI/CLI PIDs and msedgewebview2 processes whose command line
#     carries this run's private user-data path are stopped. No broad process-name
#     kills, no host networking, registry, account, or system changes.
#   - -OfflineApproximation sets a child-only WebView2 proxy to 127.0.0.1:9. This
#     is an isolation approximation, not a disconnected-OS proof; host networking
#     is never touched.
#   - No source, packaging, or documentation files are written. Test inputs are
#     synthetic local files plus the repo's synthetic fixtures.
#   - The current Windows token (medium integrity, Administrators deny-only) is
#     verified. WindowsSandbox.exe is reported when absent; this run never claims
#     a fresh/clean Windows image.

#requires -Version 5.1
[CmdletBinding()]
param(
    [string]$Zip = (Join-Path $PSScriptRoot '..\target\IndexFox-windows-x64-portable.zip'),
    [switch]$Legacy,
    [switch]$DryRun,
    [switch]$KeepWorkDir,
    [switch]$OfflineApproximation,
    [int]$GuiTimeoutSec = 45,
    [int]$IndexTimeoutSec = 300,
    [int]$OcrTimeoutSec = 180
)

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$OutputEncoding = [Text.UTF8Encoding]::new($false)
$ProgressPreference = 'SilentlyContinue'

$script:tempRoot = Join-Path ([IO.Path]::GetFullPath($env:TEMP)) 'IndexFox-M4-E2E'
$script:hostLocalAppData = $env:LOCALAPPDATA
$script:workDir = $null
$script:packageDir = $null
$script:docsDir = $null
$script:localAppData = $null
$script:roamingAppData = $null
$script:childTemp = $null
$script:webViewData = $null
$script:guiPid = 0
$script:guiExe = $null
$script:passed = 0

function Write-Info([string]$Text) { Write-Host "[INFO] $Text" }
function Write-Pass([string]$Text) { $script:passed++; Write-Host "[PASS] $Text" -ForegroundColor Green }
function Write-Skip([string]$Text) { Write-Host "[SKIP] $Text" -ForegroundColor Yellow }
function Write-Warn([string]$Text) { Write-Host "[WARN] $Text" -ForegroundColor Yellow }

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function Assert-Match([string]$Text, [string]$Pattern, [string]$Message) {
    if ($Text -notmatch $Pattern) { throw $Message }
}

function Get-Tail([string]$Text, [int]$Length = 3000) {
    if ($null -eq $Text) { return '' }
    if ($Text.Length -le $Length) { return $Text }
    return $Text.Substring($Text.Length - $Length)
}

function Write-Utf8([string]$Path, [string]$Text) {
    $parent = Split-Path -Parent $Path
    if ($parent) { [IO.Directory]::CreateDirectory($parent) | Out-Null }
    [IO.File]::WriteAllText($Path, $Text, [Text.UTF8Encoding]::new($false))
}

function ConvertTo-AsciiBytes([string]$Text) {
    [Text.Encoding]::ASCII.GetBytes($Text)
}

function Quote-Arg([string]$Value) {
    if ($null -eq $Value -or $Value.Length -eq 0) { return '""' }
    if ($Value -notmatch '[\s"]') { return $Value }
    $builder = New-Object Text.StringBuilder
    [void]$builder.Append('"')
    $backslashes = 0
    foreach ($ch in $Value.ToCharArray()) {
        if ($ch -eq '\') { $backslashes++; continue }
        if ($ch -eq '"') {
            [void]$builder.Append([char]'\', ($backslashes * 2 + 1))
            [void]$builder.Append('"')
            $backslashes = 0
        } else {
            if ($backslashes -gt 0) {
                [void]$builder.Append([char]'\', $backslashes)
                $backslashes = 0
            }
            [void]$builder.Append($ch)
        }
    }
    if ($backslashes -gt 0) {
        [void]$builder.Append([char]'\', ($backslashes * 2))
    }
    [void]$builder.Append('"')
    return $builder.ToString()
}

function Get-TokenState {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    $adminRole = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    $groups = (& whoami.exe /groups /fo csv 2>$null | Out-String)
    $integrityMatch = [regex]::Match($groups, 'S-1-16-(\d+)')
    Assert-True $integrityMatch.Success 'could not determine Windows token integrity level (S-1-16-*)'
    $level = [int]$integrityMatch.Groups[1].Value
    $levelName = switch ($level) {
        4096 { 'low' }
        8192 { 'medium' }
        8448 { 'medium-plus' }
        12288 { 'high' }
        16384 { 'system' }
        default { "unknown-$level" }
    }
    [pscustomobject]@{
        Name = $identity.Name
        AdminRole = $adminRole
        AdminSidPresent = ($groups -match 'S-1-5-32-544')
        IntegritySid = "S-1-16-$level"
        IntegrityName = $levelName
        Elevated = ($level -ge 12288)
        Standard = ((-not ($level -ge 12288)) -and (-not $adminRole))
    }
}

function Invoke-ChildProcess {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$Arguments = @(),
        [int]$TimeoutSec = 120,
        [string]$WorkingDirectory,
        [hashtable]$EnvironmentOverrides = @{}
    )
    Assert-True (Test-Path -LiteralPath $FilePath -PathType Leaf) "executable not found: $FilePath"
    if (-not $WorkingDirectory) { $WorkingDirectory = $script:packageDir }

    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = $FilePath
    $psi.Arguments = (($Arguments | ForEach-Object { Quote-Arg $_ }) -join ' ')
    $psi.WorkingDirectory = $WorkingDirectory
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $psi.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)

    foreach ($name in @('TESSERACT_PATH', 'PDFIUM_LIB_PATH', 'WEBVIEW2_BROWSER_EXECUTABLE_FOLDER', 'TESSDATA_PREFIX')) {
        [void]$psi.EnvironmentVariables.Remove($name)
    }
    foreach ($name in @('LOCALAPPDATA', 'APPDATA', 'TEMP', 'TMP', 'WEBVIEW2_USER_DATA_FOLDER')) {
        $psi.EnvironmentVariables[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
    }
    foreach ($name in $EnvironmentOverrides.Keys) {
        $psi.EnvironmentVariables[$name] = [string]$EnvironmentOverrides[$name]
    }

    $process = New-Object Diagnostics.Process
    $process.StartInfo = $psi
    Assert-True $process.Start() "failed to start $FilePath"
    $processId = $process.Id
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()
    $exited = $process.WaitForExit([Math]::Max(1, $TimeoutSec) * 1000)
    if (-not $exited) {
        & taskkill.exe /PID $processId /T /F 2>$null | Out-Null
        [void]$process.WaitForExit(10000)
    }
    $stdout = ''
    $stderr = ''
    try { $stdout = $stdoutTask.GetAwaiter().GetResult() } catch { }
    try { $stderr = $stderrTask.GetAwaiter().GetResult() } catch { }
    $exitCode = -1
    if ($process.HasExited) { $exitCode = $process.ExitCode }
    $process.Dispose()
    [pscustomobject]@{
        ExitCode = $exitCode
        TimedOut = (-not $exited)
        StdOut = $stdout
        StdErr = $stderr
        Pid = $processId
    }
}

function Invoke-Cli {
    param(
        [Parameter(Mandatory = $true)][string]$Cli,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [int]$TimeoutSec = 120
    )
    $result = Invoke-ChildProcess -FilePath $Cli -Arguments $Arguments -TimeoutSec $TimeoutSec
    if ($result.TimedOut) {
        throw "CLI timed out after ${TimeoutSec}s: $($Arguments -join ' ')"
    }
    if ($result.ExitCode -ne 0) {
        throw "CLI failed (exit $($result.ExitCode)): $($Arguments -join ' ')`nstdout:`n$(Get-Tail $result.StdOut)`nstderr:`n$(Get-Tail $result.StdErr)"
    }
    $result
}

function Get-DirSnapshot([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) {
        return [pscustomobject]@{ Exists = $false; LastWriteUtc = $null; Entries = @() }
    }
    $item = Get-Item -LiteralPath $Path -Force
    $entries = @(Get-ChildItem -LiteralPath $Path -Force -ErrorAction SilentlyContinue |
        ForEach-Object { $_.Name } | Sort-Object)
    [pscustomobject]@{
        Exists = $true
        LastWriteUtc = $item.LastWriteTimeUtc.ToString('o')
        Entries = $entries
    }
}

function Get-ProfileWebViewProcesses([string]$ProfilePath) {
    $all = @(Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe'" -ErrorAction SilentlyContinue)
    @($all | Where-Object {
        $_.CommandLine -and $_.CommandLine.IndexOf($ProfilePath, [StringComparison]::OrdinalIgnoreCase) -ge 0
    })
}

function Stop-OwnedProcesses {
    if ($script:guiPid -gt 0) {
        $candidate = Get-CimInstance Win32_Process -Filter "ProcessId=$($script:guiPid)" -ErrorAction SilentlyContinue
        if ($candidate -and $candidate.ExecutablePath -and $script:guiExe -and
            $candidate.ExecutablePath.Equals($script:guiExe, [StringComparison]::OrdinalIgnoreCase)) {
            & taskkill.exe /PID $script:guiPid /T /F 2>$null | Out-Null
        }
    }
    if ($script:webViewData) {
        foreach ($p in @(Get-ProfileWebViewProcesses $script:webViewData)) {
            Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
        }
    }
    Start-Sleep -Milliseconds 500
    if ($script:packageDir) {
        $left = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | Where-Object {
            ($_.ExecutablePath -and $_.ExecutablePath.StartsWith($script:packageDir + '\', [StringComparison]::OrdinalIgnoreCase)) -or
            ($script:webViewData -and $_.CommandLine -and $_.CommandLine.IndexOf($script:webViewData, [StringComparison]::OrdinalIgnoreCase) -ge 0)
        })
        if ($left.Count -gt 0) {
            Write-Warn "leftover processes after cleanup: $($left.ProcessId -join ', ')"
        }
    }
}

function Remove-OwnWorkDir {
    if ($KeepWorkDir) {
        Write-Info "work directory kept: $script:workDir"
        return
    }
    if (-not $script:workDir) { return }
    $root = [IO.Path]::GetFullPath($script:tempRoot).TrimEnd('\') + '\'
    $work = [IO.Path]::GetFullPath($script:workDir)
    if (-not $work.StartsWith($root, [StringComparison]::OrdinalIgnoreCase)) {
        Write-Warn "refusing to remove non-owned work directory: $work"
        return
    }
    for ($i = 0; $i -lt 12; $i++) {
        try {
            if (-not (Test-Path -LiteralPath $work)) { return }
            Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction Stop
            return
        } catch {
            if ($i -eq 11) { Write-Warn "could not remove work directory: $work ($_)" }
            Start-Sleep -Milliseconds 300
        }
    }
}

function Expand-PackageZip([string]$ZipPath, [string]$Destination) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead($ZipPath)
    try {
        $root = [IO.Path]::GetFullPath($Destination).TrimEnd('\') + '\'
        $count = 0
        foreach ($entry in $archive.Entries) {
            $relative = $entry.FullName.Replace('/', '\')
            $target = [IO.Path]::GetFullPath((Join-Path $Destination $relative))
            Assert-True $target.StartsWith($root, [StringComparison]::OrdinalIgnoreCase) "unsafe ZIP entry: $($entry.FullName)"
            if ($entry.Name -eq '' -and $entry.FullName.EndsWith('/')) {
                [IO.Directory]::CreateDirectory($target) | Out-Null
                continue
            }
            [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($target)) | Out-Null
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $target, $true)
            $count++
        }
        Assert-True ($count -gt 0) 'ZIP contains no files'
        Write-Info "extracted $count ZIP entries"
    } finally {
        $archive.Dispose()
    }
}

function Test-PackageLayout {
    param([string]$PackageDir, [bool]$Strict)
    $required = @('IndexFox.exe', 'indexfox-cli.exe', 'WebView2Loader.dll', 'pdfium.dll')
    $missing = @()
    foreach ($name in $required) {
        $path = Join-Path $PackageDir $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { $missing += $name }
        elseif ((Get-Item -LiteralPath $path).Length -le 0) { $missing += "$name (empty)" }
    }
    if ($Strict) {
        foreach ($name in @(
            'runtime\webview2\msedgewebview2.exe',
            'ocr\tesseract.exe',
            'ocr\tessdata\chi_sim.traineddata',
            'ocr\tessdata\eng.traineddata'
        )) {
            $path = Join-Path $PackageDir $name
            if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { $missing += $name }
            elseif ((Get-Item -LiteralPath $path).Length -le 0) { $missing += "$name (empty)" }
        }
    }
    if ($missing.Count -gt 0) {
        throw "package layout is missing: $($missing -join ', ')"
    }

    $fixedRuntime = Join-Path $PackageDir 'runtime\webview2\msedgewebview2.exe'
    $tesseract = Join-Path $PackageDir 'ocr\tesseract.exe'
    $tessdata = Join-Path $PackageDir 'ocr\tessdata'
    $hasFixed = Test-Path -LiteralPath $fixedRuntime -PathType Leaf
    $hasTesseract = (Test-Path -LiteralPath $tesseract -PathType Leaf) -and
        (Test-Path -LiteralPath (Join-Path $tessdata 'chi_sim.traineddata') -PathType Leaf) -and
        (Test-Path -LiteralPath (Join-Path $tessdata 'eng.traineddata') -PathType Leaf)
    if ($Strict) {
        Assert-True $hasFixed 'fixed WebView2 runtime is not at runtime\webview2\msedgewebview2.exe'
        Assert-True $hasTesseract 'portable Tesseract with chi_sim+eng is incomplete under ocr\'
    }
    if (-not (Test-Path -LiteralPath (Join-Path $PackageDir 'THIRD_PARTY_NOTICES.md'))) {
        Write-Warn 'THIRD_PARTY_NOTICES.md is not in the ZIP'
    }
    if (-not (Test-Path -LiteralPath (Join-Path $PackageDir 'README.txt'))) {
        Write-Warn 'README.txt is not in the ZIP'
    }
    [pscustomobject]@{
        HasFixedRuntime = $hasFixed
        HasTesseract = $hasTesseract
        FixedRuntimeDir = (Join-Path $PackageDir 'runtime\webview2')
        Tesseract = $tesseract
        Tessdata = $tessdata
    }
}

function New-MarkerJpeg([string]$Path, [string]$Text) {
    Add-Type -AssemblyName System.Drawing
    $width = 1800
    $height = 520
    $bitmap = New-Object Drawing.Bitmap($width, $height)
    $graphics = [Drawing.Graphics]::FromImage($bitmap)
    $format = New-Object Drawing.StringFormat
    $font = $null
    try {
        $graphics.Clear([Drawing.Color]::White)
        $graphics.SmoothingMode = [Drawing.Drawing2D.SmoothingMode]::HighQuality
        $graphics.TextRenderingHint = [Drawing.Text.TextRenderingHint]::AntiAliasGridFit
        $font = New-Object Drawing.Font('Arial', 96, [Drawing.FontStyle]::Bold, [Drawing.GraphicsUnit]::Pixel)
        $format.Alignment = [Drawing.StringAlignment]::Center
        $format.LineAlignment = [Drawing.StringAlignment]::Center
        $rect = New-Object Drawing.RectangleF(0, 0, $width, $height)
        $graphics.DrawString($Text, $font, [Drawing.Brushes]::Black, $rect, $format)
    } finally {
        if ($font) { $font.Dispose() }
        $format.Dispose()
        $graphics.Dispose()
    }
    try {
        $codec = [Drawing.Imaging.ImageCodecInfo]::GetImageEncoders() |
            Where-Object { $_.MimeType -eq 'image/jpeg' } | Select-Object -First 1
        Assert-True ($null -ne $codec) 'JPEG encoder not available'
        $parameters = New-Object Drawing.Imaging.EncoderParameters(1)
        $parameters.Param[0] = New-Object Drawing.Imaging.EncoderParameter(
            [Drawing.Imaging.Encoder]::Quality, [long]92)
        $bitmap.Save($Path, $codec, $parameters)
    } finally {
        $bitmap.Dispose()
    }
    Assert-True ((Get-Item -LiteralPath $Path).Length -gt 2000) 'generated marker JPEG is too small'
}

function New-MixedPdf([string]$Path, [string]$JpegPath) {
    $jpeg = [IO.File]::ReadAllBytes($JpegPath)
    $objects = New-Object Collections.ArrayList
    [void]$objects.Add((ConvertTo-AsciiBytes '<< /Type /Catalog /Pages 2 0 R >>'))
    [void]$objects.Add((ConvertTo-AsciiBytes '<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>'))
    [void]$objects.Add((ConvertTo-AsciiBytes '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 6 0 R >> >> /Contents 5 0 R >>'))
    [void]$objects.Add((ConvertTo-AsciiBytes '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /XObject << /Im1 7 0 R >> >> /Contents 8 0 R >>'))
    $text = "BT /F1 28 Tf 72 700 Td (MIXEDNATIVE) Tj ET`n"
    $textBytes = ConvertTo-AsciiBytes $text
    [void]$objects.Add((ConvertTo-AsciiBytes ("<< /Length " + $textBytes.Length + " >>`nstream`n" + $text + "endstream")))
    [void]$objects.Add((ConvertTo-AsciiBytes '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>'))
    $imageHeader = ConvertTo-AsciiBytes ("<< /Type /XObject /Subtype /Image /Width 1800 /Height 520 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length " + $jpeg.Length + " >>`nstream`n")
    $imageFooter = ConvertTo-AsciiBytes "`nendstream"
    $imageObject = New-Object byte[] ($imageHeader.Length + $jpeg.Length + $imageFooter.Length)
    [Array]::Copy($imageHeader, 0, $imageObject, 0, $imageHeader.Length)
    [Array]::Copy($jpeg, 0, $imageObject, $imageHeader.Length, $jpeg.Length)
    [Array]::Copy($imageFooter, 0, $imageObject, $imageHeader.Length + $jpeg.Length, $imageFooter.Length)
    [void]$objects.Add($imageObject)
    $draw = "q 540 0 0 156 36 600 cm /Im1 Do Q`n"
    $drawBytes = ConvertTo-AsciiBytes $draw
    [void]$objects.Add((ConvertTo-AsciiBytes ("<< /Length " + $drawBytes.Length + " >>`nstream`n" + $draw + "endstream")))

    $stream = New-Object IO.MemoryStream
    $offsets = New-Object Collections.ArrayList
    try {
        $write = { param([byte[]]$Bytes) $stream.Write($Bytes, 0, $Bytes.Length) }
        & $write (ConvertTo-AsciiBytes "%PDF-1.4`n%\xE2\xE3\xCF\xD3`n")
        for ($i = 0; $i -lt $objects.Count; $i++) {
            [void]$offsets.Add($stream.Position)
            & $write (ConvertTo-AsciiBytes ("$($i + 1) 0 obj`n"))
            & $write $objects[$i]
            & $write (ConvertTo-AsciiBytes "`nendobj`n")
        }
        $xref = $stream.Position
        & $write (ConvertTo-AsciiBytes ("xref`n0 $($objects.Count + 1)`n0000000000 65535 f `n"))
        foreach ($offset in $offsets) {
            & $write (ConvertTo-AsciiBytes ("{0:D10} 00000 n `n" -f $offset))
        }
        & $write (ConvertTo-AsciiBytes ("trailer`n<< /Size $($objects.Count + 1) /Root 1 0 R >>`nstartxref`n$xref`n%%EOF`n"))
        [IO.File]::WriteAllBytes($Path, $stream.ToArray())
    } finally {
        $stream.Dispose()
    }
    Assert-True ((Get-Item -LiteralPath $Path).Length -gt 10000) 'generated mixed PDF is too small'
}

function New-SyntheticDocs {
    param([string]$DocsDir, [bool]$Full)
    [IO.Directory]::CreateDirectory($DocsDir) | Out-Null
    Write-Utf8 (Join-Path $DocsDir 'notes.txt') @'
IndexFox M4 Windows smoke
Linux 内核驱动 dma_alloc_coherent();
VIDIOC_STREAMON 100% a_b #include
'@
    Write-Utf8 (Join-Path $DocsDir 'code.c') @'
#include <linux/videodev2.h>
/* M4 smoke: synthetic, non-personal */
static int probe(void) {
    void *p = dma_alloc_coherent();
    return p ? VIDIOC_STREAMON : 0;
}
'@
    $fixtureRoot = Join-Path (Split-Path -Parent $PSScriptRoot) 'fixtures'
    $sample = Join-Path $fixtureRoot 'pdf\sample.pdf'
    Assert-True (Test-Path -LiteralPath $sample -PathType Leaf) "fixture missing: $sample"
    Copy-Item -LiteralPath $sample -Destination (Join-Path $DocsDir 'sample.pdf')
    $count = 3
    if ($Full) {
        $scanned = Join-Path $fixtureRoot 'pdf\scanned_zh.pdf'
        Assert-True (Test-Path -LiteralPath $scanned -PathType Leaf) "fixture missing: $scanned"
        Copy-Item -LiteralPath $scanned -Destination (Join-Path $DocsDir 'scanned_zh.pdf')
        $jpeg = Join-Path $script:workDir 'mixed-page.jpg'
        New-MarkerJpeg $jpeg 'MIXEDOCR'
        New-MixedPdf (Join-Path $DocsDir 'mixed.pdf') $jpeg
        $count = 5
    }
    Write-Info "synthetic test documents: $count files in $DocsDir"
    $count
}

function Get-SearchResponse {
    param(
        [string]$Cli,
        [string]$Db,
        [string]$Query,
        [string]$Mode = 'substring',
        [int]$TimeoutSec = 60
    )
    $arguments = @('--db', $Db, 'search', $Query, '--json', '--limit', '50')
    if ($Mode) { $arguments += @('--mode', $Mode) }
    $result = Invoke-Cli -Cli $Cli -Arguments $arguments -TimeoutSec $TimeoutSec
    try {
        $response = $result.StdOut | ConvertFrom-Json
    } catch {
        throw "search JSON parse failed for '$Query': $_`nstdout:`n$(Get-Tail $result.StdOut)"
    }
    [pscustomobject]@{ Raw = $result.StdOut; Response = $response }
}

function Get-FileResult($Response, [string]$Filename) {
    @($Response.results) | Where-Object { $_.filename -ieq $Filename } | Select-Object -First 1
}

function Assert-PdfHit {
    param(
        $FileResult,
        [string]$Origin,
        [int]$Page,
        [string]$Label
    )
    Assert-True ($null -ne $FileResult) "$Label : file result missing"
    $bodyHits = @($FileResult.hits | Where-Object { $_.kind -ne 'filename' })
    Assert-True ($bodyHits.Count -gt 0) "$Label : no body hits"
    foreach ($hit in $bodyHits) {
        Assert-True ($hit.origin -eq $Origin) "$Label : expected origin=$Origin, got $($hit.origin)"
        Assert-True ($null -ne $hit.locator) "$Label : locator missing"
        Assert-True ($hit.locator.kind -eq 'pdf_page') "$Label : expected pdf_page locator, got $($hit.locator.kind)"
        Assert-True ([int]$hit.locator.page -eq $Page) "$Label : expected page $Page, got $($hit.locator.page)"
    }
    Write-Pass "$Label : origin=$Origin, PDF p.$Page, $($bodyHits.Count) hit(s)"
}

function Invoke-GuiSmoke {
    param(
        [string]$Exe,
        [string]$PackageDir,
        [string]$FixedRuntimeDir,
        [bool]$RequireFixed,
        [bool]$OfflineApproximation
    )
    $realDataDir = Join-Path $script:hostLocalAppData 'IndexFox'
    $before = Get-DirSnapshot $realDataDir
    Assert-True (-not (Test-Path -LiteralPath $script:webViewData)) 'isolated WebView2 profile unexpectedly exists before launch'
    $tempBefore = @(Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -and $_.CommandLine.IndexOf($script:webViewData, [StringComparison]::OrdinalIgnoreCase) -ge 0 })
    Assert-True ($tempBefore.Count -eq 0) 'a process already uses this run''s WebView2 profile path'

    $keys = @(
        'LOCALAPPDATA', 'APPDATA', 'TEMP', 'TMP',
        'WEBVIEW2_USER_DATA_FOLDER', 'WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS',
        'WEBVIEW2_BROWSER_EXECUTABLE_FOLDER',
        'TESSERACT_PATH', 'PDFIUM_LIB_PATH', 'TESSDATA_PREFIX'
    )
    $saved = @{}
    foreach ($key in $keys) {
        $saved[$key] = [Environment]::GetEnvironmentVariable($key, 'Process')
    }
    try {
        $env:LOCALAPPDATA = $script:localAppData
        $env:APPDATA = $script:roamingAppData
        $env:TEMP = $script:childTemp
        $env:TMP = $script:childTemp
        $env:WEBVIEW2_USER_DATA_FOLDER = $script:webViewData
        [Environment]::SetEnvironmentVariable('WEBVIEW2_BROWSER_EXECUTABLE_FOLDER', $null, 'Process')
        [Environment]::SetEnvironmentVariable('TESSERACT_PATH', $null, 'Process')
        [Environment]::SetEnvironmentVariable('PDFIUM_LIB_PATH', $null, 'Process')
        [Environment]::SetEnvironmentVariable('TESSDATA_PREFIX', $null, 'Process')
        if ($OfflineApproximation) {
            $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = '--proxy-server=http://127.0.0.1:9'
        } else {
            [Environment]::SetEnvironmentVariable('WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS', $null, 'Process')
        }
        $process = Start-Process -FilePath $Exe -WorkingDirectory $PackageDir -PassThru
    } finally {
        foreach ($key in $keys) {
            [Environment]::SetEnvironmentVariable($key, $saved[$key], 'Process')
        }
    }

    $script:guiPid = $process.Id
    $script:guiExe = $Exe
    Write-Info "GUI pid=$($process.Id); WebView2 profile=$($script:webViewData)"

    $deadline = (Get-Date).AddSeconds($GuiTimeoutSec)
    $title = $null
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 250
        $process.Refresh()
        if ($process.HasExited) {
            throw "GUI exited early with code $($process.ExitCode)"
        }
        if ($process.MainWindowTitle -like 'IndexFox*') {
            $title = $process.MainWindowTitle
            break
        }
    }
    Assert-True ($null -ne $title) "GUI window title 'IndexFox*' not observed within ${GuiTimeoutSec}s"
    Assert-True ($process.MainWindowHandle -ne 0) "GUI window handle is zero"
    Write-Pass "GUI window title=$title handle=$($process.MainWindowHandle)"

    $webViewDeadline = (Get-Date).AddSeconds(15)
    $owned = @()
    while ((Get-Date) -lt $webViewDeadline) {
        $owned = @(Get-ProfileWebViewProcesses $script:webViewData)
        if ($owned.Count -gt 0) { break }
        Start-Sleep -Milliseconds 300
    }
    Assert-True ($owned.Count -gt 0) 'no msedgewebview2.exe process uses this run''s isolated profile path'
    $withPath = @($owned | Where-Object { $_.ExecutablePath })
    Assert-True ($withPath.Count -gt 0) 'msedgewebview2.exe executable path unavailable'
    if ($RequireFixed) {
        $fixedRoot = [IO.Path]::GetFullPath($FixedRuntimeDir).TrimEnd('\') + '\'
        Assert-True ($fixedRoot.StartsWith([IO.Path]::GetFullPath($PackageDir).TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) 'fixed runtime is not inside the extracted package'
        $wrong = @($withPath | Where-Object {
            -not $_.ExecutablePath.StartsWith($fixedRoot, [StringComparison]::OrdinalIgnoreCase)
        })
        Assert-True ($wrong.Count -eq 0) "WebView2 used a path outside the extracted fixed runtime: $(($wrong | ForEach-Object { $_.ExecutablePath }) -join ', ')"
        Write-Pass "fixed WebView2 runtime chosen from extracted package: $($withPath[0].ExecutablePath)"
    } else {
        Write-Pass "Evergreen WebView2 process: $($withPath[0].ExecutablePath)"
    }

    $profileDeadline = (Get-Date).AddSeconds(15)
    $profileFiles = @()
    while ((Get-Date) -lt $profileDeadline) {
        if (Test-Path -LiteralPath $script:webViewData) {
            $profileFiles = @(Get-ChildItem -LiteralPath $script:webViewData -Recurse -File -Force -ErrorAction SilentlyContinue)
            if ($profileFiles.Count -gt 0) { break }
        }
        Start-Sleep -Milliseconds 300
    }
    Assert-True ($profileFiles.Count -gt 0) 'isolated WebView2 profile directory stayed empty'
    Write-Pass "new isolated WebView2 profile data: $($profileFiles.Count) files under $($script:webViewData)"

    $after = Get-DirSnapshot $realDataDir
    Assert-True ($before.Exists -eq $after.Exists) "real default data directory existence changed: $realDataDir"
    if ($before.Exists) {
        Assert-True ($before.LastWriteUtc -eq $after.LastWriteUtc) "real default data directory was modified: $realDataDir"
        Assert-True (($before.Entries -join '|') -eq ($after.Entries -join '|')) "real default data directory entries changed: $realDataDir"
    }
    Write-Pass "host default data directory untouched: $realDataDir (exists=$($after.Exists))"
}

$failure = $null
try {
    Write-Info "IndexFox M4 Windows E2E smoke"
    Write-Info "PowerShell $($PSVersionTable.PSVersion); OS $([Environment]::OSVersion.VersionString)"

    $token = Get-TokenState
    Assert-True $token.Standard "Windows token is not standard/non-elevated: user=$($token.Name) integrity=$($token.IntegrityName) ($($token.IntegritySid)) elevated=$($token.Elevated) adminRole=$($token.AdminRole)"
    $adminNote = if ($token.AdminSidPresent) { '; Administrators SID present but not enabled (deny-only)' } else { '' }
    Write-Pass "token standard, non-elevated: user=$($token.Name), integrity=$($token.IntegrityName) ($($token.IntegritySid))$adminNote"

    $systemRoot = if ($env:SystemRoot) { $env:SystemRoot } else { $env:WINDIR }
    $sandbox = $false
    if ($systemRoot) { $sandbox = Test-Path -LiteralPath (Join-Path $systemRoot 'System32\WindowsSandbox.exe') -PathType Leaf }
    if ($sandbox) {
        Write-Info 'WindowsSandbox.exe is visible, but this harness does not launch Windows Sandbox; it does not claim a fresh Windows image.'
    } else {
        Write-Info 'WindowsSandbox.exe is not visible; this run does not claim a clean/fresh Windows environment.'
    }
    Write-Info 'host networking, registry, accounts, and system settings are not modified.'

    Assert-True (Test-Path -LiteralPath $Zip -PathType Leaf) "ZIP not found: $Zip"
    $zipPath = (Get-Item -LiteralPath $Zip).FullName
    Assert-True (Test-Path -LiteralPath $zipPath -PathType Leaf) "ZIP not found: $zipPath"
    Write-Info "ZIP: $zipPath ($((Get-Item -LiteralPath $zipPath).Length) bytes)"

    $runId = 'run-' + (Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
    $script:workDir = Join-Path $script:tempRoot $runId
    $script:packageDir = Join-Path $script:workDir 'package'
    $script:docsDir = Join-Path $script:workDir 'docs'
    $script:localAppData = Join-Path $script:workDir 'isolated\localappdata'
    $script:roamingAppData = Join-Path $script:workDir 'isolated\appdata'
    $script:childTemp = Join-Path $script:workDir 'temp'
    $script:webViewData = Join-Path $script:workDir 'webview2-user-data'
    foreach ($dir in @($script:packageDir, $script:localAppData, $script:roamingAppData, $script:childTemp)) {
        [IO.Directory]::CreateDirectory($dir) | Out-Null
    }
    Write-Info "work directory: $($script:workDir)"

    Expand-PackageZip $zipPath $script:packageDir
    $layout = Test-PackageLayout -PackageDir $script:packageDir -Strict:(-not $Legacy)
    $layoutExtra = @()
    if ($layout.HasFixedRuntime) { $layoutExtra += 'fixed WebView2' }
    if ($layout.HasTesseract) { $layoutExtra += 'Tesseract chi_sim+eng' }
    $layoutNote = if ($layoutExtra.Count -gt 0) { ', ' + ($layoutExtra -join ', ') } else { '' }
    Write-Pass "package layout: IndexFox.exe, indexfox-cli.exe, WebView2Loader.dll, pdfium.dll$layoutNote"

    $cli = Join-Path $script:packageDir 'indexfox-cli.exe'
    $exe = Join-Path $script:packageDir 'IndexFox.exe'

    if ($DryRun) {
        $docsPlan = Join-Path $script:workDir 'docs'
        Write-Host '[DRY] no child process will be launched.'
        $ocrPlan = if ($Legacy) { '' } else { ' --ocr --lang chi_sim+eng' }
        Write-Host ('[DRY] index: "{0}" --db <work>\index.db index "{1}"{2}' -f $cli, $docsPlan, $ocrPlan)
        Write-Host ('[DRY] search: "{0}" --db <work>\index.db search <query> --json' -f $cli)
        if (-not $Legacy) {
            Write-Host ('[DRY] fuzzy Q10: "{0}" --db <work>\index.db search dma_alloc_cohernt --mode fuzzy --json' -f $cli)
            Write-Host ('[DRY] fixed runtime: launch "{0}" with WEBVIEW2_BROWSER_EXECUTABLE_FOLDER removed and profile under "{1}"' -f $exe, $script:webViewData)
        } else {
            Write-Host ('[DRY] legacy GUI: launch "{0}" with isolated profile (fixed runtime check if present)' -f $exe)
        }
        Write-Pass 'dry run validated token, ZIP, extraction, and package layout'
    } else {
        $version = Invoke-Cli -Cli $cli -Arguments @('--version') -TimeoutSec 30
        Write-Pass "CLI version: $(($version.StdOut.Trim() -split "`n")[0])"

        $expectedCount = New-SyntheticDocs -DocsDir $script:docsDir -Full:(-not $Legacy)

        if (-not $Legacy) {
            $renderDb = Join-Path $script:workDir 'render.db'
            $scannedPdf = Join-Path $script:docsDir 'scanned_zh.pdf'
            $scanPng = Join-Path $script:workDir 'scanned-page.png'
            $render = Invoke-Cli -Cli $cli -Arguments @(
                '--db', $renderDb, 'pdf-render', $scannedPdf, '--page', '1', '--out', $scanPng, '--width', '1600'
            ) -TimeoutSec 120
            Assert-True (Test-Path -LiteralPath $scanPng -PathType Leaf) "PDFium render did not produce $scanPng"
            Write-Pass "bundled PDFium rendered scanned PDF without PDFIUM_LIB_PATH ($((Get-Item -LiteralPath $scanPng).Length) bytes)"

            $ocrBase = Join-Path $script:workDir 'ocr-direct\scan'
            [IO.Directory]::CreateDirectory((Split-Path -Parent $ocrBase)) | Out-Null
            $ocrResult = Invoke-ChildProcess -FilePath $layout.Tesseract -Arguments @(
                $scanPng, $ocrBase, '-l', 'chi_sim+eng'
            ) -TimeoutSec $OcrTimeoutSec -WorkingDirectory (Split-Path -Parent $layout.Tesseract)
            Assert-True (-not $ocrResult.TimedOut) 'bundled Tesseract timed out'
            Assert-True ($ocrResult.ExitCode -eq 0) "bundled Tesseract failed (exit $($ocrResult.ExitCode))`n$(Get-Tail $ocrResult.StdErr)"
            $ocrTextPath = $ocrBase + '.txt'
            Assert-True (Test-Path -LiteralPath $ocrTextPath -PathType Leaf) 'bundled Tesseract produced no text file'
            $ocrText = [IO.File]::ReadAllText($ocrTextPath) -replace '\s+', ' '
            Assert-True ($ocrText.Contains('内核')) "bundled Tesseract chi_sim output missing Chinese marker: $ocrText"
            Assert-True ($ocrText.Contains('VIDIOC_STREAMON')) "bundled Tesseract eng output missing identifier: $ocrText"
            Write-Pass "bundled Tesseract at ocr\tesseract.exe used chi_sim+eng with no TESSERACT_PATH/TESSDATA_PREFIX: $($ocrText.Trim())"
        }

        $db = Join-Path $script:workDir 'index.db'
        $indexArgs = @('--db', $db, 'index', $script:docsDir)
        if (-not $Legacy) { $indexArgs += @('--ocr', '--lang', 'chi_sim+eng') }
        $index = Invoke-Cli -Cli $cli -Arguments $indexArgs -TimeoutSec $IndexTimeoutSec
        Assert-Match $index.StdOut "(?m)^indexed\s+$expectedCount\s*$" "expected $expectedCount indexed files:`n$(Get-Tail $index.StdOut)"
        Assert-Match $index.StdOut '(?m)^failed\s+0\s*$' "index reported failures:`n$(Get-Tail $index.StdOut)"
        Assert-True ($index.StdOut -notmatch '(?m)^issue:') "index reported issues:`n$(Get-Tail $index.StdOut)"
        $ocrNote = if ($Legacy) { '' } else { ' (default bundled OCR path, no TESSERACT_PATH)' }
        Write-Pass "indexed $expectedCount synthetic files with failed=0$ocrNote"

        $native = Get-SearchResponse -Cli $cli -Db $db -Query 'VIDIOC_STREAMON'
        $sample = Get-FileResult $native.Response 'sample.pdf'
        Assert-PdfHit -FileResult $sample -Origin 'native' -Page 2 -Label 'native sample.pdf VIDIOC_STREAMON'
        $notes = Get-FileResult (Get-SearchResponse -Cli $cli -Db $db -Query '内核驱动').Response 'notes.txt'
        Assert-True ($null -ne $notes) 'UTF-8 Chinese query did not find notes.txt'
        Write-Pass 'UTF-8 text query found notes.txt'

        if ($Legacy) {
            Write-Skip 'M4 fuzzy Q10 (legacy mode)'
            Write-Skip 'M4 scanned/mixed PDF OCR locator+origin (legacy mode)'
        } else {
            $fuzzy = Get-SearchResponse -Cli $cli -Db $db -Query 'dma_alloc_cohernt' -Mode 'fuzzy'
            Assert-True ($fuzzy.Response.mode -eq 'fuzzy') "fuzzy response mode is $($fuzzy.Response.mode)"
            $corrected = @()
            $fuzzyLabel = $false
            foreach ($file in @($fuzzy.Response.results)) {
                foreach ($hit in @($file.hits)) {
                    if ($hit.matched_text -and $hit.matched_text.IndexOf('dma_alloc_coherent', [StringComparison]::OrdinalIgnoreCase) -ge 0) {
                        $corrected += $hit.matched_text
                    }
                    foreach ($property in $hit.PSObject.Properties) {
                        if ($property.Name -match '(?i)fuzzy') { $fuzzyLabel = $true }
                        if ($property.Value -is [string] -and $property.Value -match '(?i)fuzzy') { $fuzzyLabel = $true }
                    }
                }
            }
            Assert-True ($fuzzy.Response.total_files -gt 0) 'Q10 fuzzy search returned no files'
            Assert-True ($corrected.Count -gt 0) "Q10 fuzzy search did not expose the corrected word dma_alloc_coherent; raw: $(Get-Tail $fuzzy.Raw)"
            Write-Pass "Q10 fuzzy 'dma_alloc_cohernt' -> dma_alloc_coherent ($($corrected[0]))"
            if ($fuzzyLabel) { Write-Pass 'fuzzy hit exposes a fuzzy label/field' }
            else { Write-Info 'fuzzy JSON has mode=fuzzy and the corrected matched_text; no separate hit-level fuzzy label was found in this build' }

            $scanned = Get-FileResult (Get-SearchResponse -Cli $cli -Db $db -Query '内核驱动').Response 'scanned_zh.pdf'
            Assert-PdfHit -FileResult $scanned -Origin 'ocr' -Page 1 -Label 'scanned_zh.pdf Chinese OCR'
            $scannedEn = Get-FileResult (Get-SearchResponse -Cli $cli -Db $db -Query 'VIDIOC_STREAMON').Response 'scanned_zh.pdf'
            Assert-PdfHit -FileResult $scannedEn -Origin 'ocr' -Page 1 -Label 'scanned_zh.pdf English OCR'

            $mixedNative = Get-FileResult (Get-SearchResponse -Cli $cli -Db $db -Query 'MIXEDNATIVE').Response 'mixed.pdf'
            Assert-PdfHit -FileResult $mixedNative -Origin 'native' -Page 1 -Label 'mixed.pdf native page'
            Assert-True ([int]$mixedNative.hit_count -eq 1) "mixed.pdf native hit_count=$($mixedNative.hit_count), expected 1"

            $mixedOcr = Get-FileResult (Get-SearchResponse -Cli $cli -Db $db -Query 'MIXEDOCR').Response 'mixed.pdf'
            Assert-PdfHit -FileResult $mixedOcr -Origin 'ocr' -Page 2 -Label 'mixed.pdf OCR page'
            Assert-True ([int]$mixedOcr.hit_count -eq 1) "mixed.pdf OCR hit_count=$($mixedOcr.hit_count), expected 1 (no duplicate text)"
            Write-Pass 'mixed PDF: native page 1 + OCR page 2, no duplicate hit'
        }

        if ($OfflineApproximation) {
            Write-Info 'offline approximation: child WebView2 gets --proxy-server=http://127.0.0.1:9; host networking is untouched; this is not a disconnected-OS proof.'
        }
        Invoke-GuiSmoke -Exe $exe -PackageDir $script:packageDir -FixedRuntimeDir $layout.FixedRuntimeDir `
            -RequireFixed:($layout.HasFixedRuntime) -OfflineApproximation:$OfflineApproximation
        if (-not $layout.HasFixedRuntime) {
            Write-Skip 'fixed runtime check (legacy package has no runtime\webview2)'
        }
        Write-Pass 'M4 Windows end-to-end smoke completed'
    }
} catch {
    $failure = $_
} finally {
    try {
        Stop-OwnedProcesses
    } catch {
        Write-Warn "process cleanup: $_"
    }
    try {
        Remove-OwnWorkDir
    } catch {
        Write-Warn "work directory cleanup: $_"
    }
}

if ($failure) {
    Write-Host "[FAIL] $($failure.Exception.Message)" -ForegroundColor Red
    if ($failure.ScriptStackTrace) { Write-Host $failure.ScriptStackTrace }
    exit 1
}

Write-Host "[DONE] $($script:passed) checks passed" -ForegroundColor Green
exit 0
