param(
    [string]$Zip = (Join-Path $PSScriptRoot '..\target\IndexFox-windows-x64-portable.zip'),
    [string]$FixedRuntime,
    [string]$Tesseract,
    [string]$Tessdata,
    [string]$SmbPath
)

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$zip = (Resolve-Path $Zip).Path
$dir = Join-Path $env:TEMP ("IndexFox-smoke-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory $dir | Out-Null
try {
    Expand-Archive -Path $zip -DestinationPath $dir
    $exe = Join-Path $dir 'IndexFox.exe'
    $cli = Join-Path $dir 'indexfox-cli.exe'
    if (-not (Test-Path $cli)) { throw 'indexfox-cli.exe missing from ZIP' }
    if (-not (Test-Path (Join-Path $dir 'WebView2Loader.dll'))) { throw 'WebView2Loader.dll missing from ZIP' }

    $docs = Join-Path $dir 'docs'
    New-Item -ItemType Directory $docs | Out-Null
    Copy-Item (Join-Path $repo 'fixtures\text\*') $docs
    Copy-Item (Join-Path $repo 'fixtures\docx\*') $docs
    Copy-Item (Join-Path $repo 'fixtures\pdf\*') $docs

    $db = Join-Path $dir 'index.db'
    $index = & $cli --db $db index $docs 2>&1 | Out-String
    if ($index -notmatch 'failed\s+0') { throw "index failed:`n$index" }
    if ($index -notmatch 'indexed\s+8') { throw "expected 8 indexed files:`n$index" }

    $ascii = & $cli --db $db search 'VIDIOC_STREAMON' 2>&1 | Out-String
    if ($ascii -notmatch 'sample\.pdf' -or $ascii -notmatch 'PDF p\.2') { throw "PDF search failed:`n$ascii" }

    $zhQuery = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('5YaF5qC46amx5Yqo'))
    $zh = & $cli --db $db search $zhQuery 2>&1 | Out-String
    if ($zh -notmatch 'cross_chunk\.txt' -or $zh -notmatch 'gbk\.txt') { throw "Chinese search failed:`n$zh" }

    $incremental = & $cli --db $db index $docs 2>&1 | Out-String
    if ($incremental -notmatch 'unchanged\s+8') { throw "incremental index failed:`n$incremental" }

    if ($Tesseract) {
        if ($Tessdata) { $env:TESSDATA_PREFIX = $Tessdata }
        $ocrDir = Join-Path $dir 'ocr'
        New-Item -ItemType Directory $ocrDir | Out-Null
        Copy-Item (Join-Path $repo 'fixtures\pdf\scanned_zh.pdf') $ocrDir
        $ocrDb = Join-Path $dir 'ocr.db'
        $ocr = & $cli --db $ocrDb index $ocrDir --ocr --tesseract $Tesseract --lang chi_sim+eng 2>&1 | Out-String
        if ($ocr -notmatch 'indexed\s+1' -or $ocr -notmatch 'failed\s+0') { throw "OCR index failed:`n$ocr" }
        $ocrJson = & $cli --db $ocrDb search $zhQuery --json 2>&1 | Out-String
        if ($ocrJson -notmatch '"origin": "ocr"' -or $ocrJson -notmatch 'scanned_zh\.pdf') {
            throw "OCR search failed:`n$ocrJson"
        }
        Write-Output 'PASS Windows Tesseract OCR'
    }

    if ($SmbPath) {
        $smbDb = Join-Path $dir 'smb.db'
        $smb = & $cli --db $smbDb index $SmbPath 2>&1 | Out-String
        if ($smb -notmatch 'indexed\s+[1-9]' -or $smb -notmatch 'failed\s+0') { throw "SMB index failed:`n$smb" }
        $smbSearch = & $cli --db $smbDb search 'VIDIOC_STREAMON' 2>&1 | Out-String
        if ($smbSearch -notmatch '\.txt') { throw "SMB search failed:`n$smbSearch" }
        Write-Output "PASS SMB index: $SmbPath"
    }

    if ($FixedRuntime) { $env:WEBVIEW2_BROWSER_EXECUTABLE_FOLDER = $FixedRuntime }
    $process = Start-Process -FilePath $exe -WorkingDirectory $dir -PassThru
    $deadline = (Get-Date).AddSeconds(20)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 500
        $process.Refresh()
        if ($process.HasExited) { throw "GUI exited with code $($process.ExitCode)" }
        if ($process.MainWindowTitle -like 'IndexFox*') { break }
    }
    if ($process.MainWindowTitle -notlike 'IndexFox*') { throw 'GUI window title not found' }
    if ($FixedRuntime) {
        $fixed = Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe'" |
            Where-Object { $_.ExecutablePath -like "$FixedRuntime*" }
        if (-not $fixed) { throw "fixed WebView2 runtime not used: $FixedRuntime" }
        Write-Output "PASS Fixed WebView2: $FixedRuntime"
    }
    Write-Output "PASS GUI title=$($process.MainWindowTitle) handle=$($process.MainWindowHandle)"
    Stop-Process -Id $process.Id -Force
} finally {
    Get-Process IndexFox -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$dir*" } | Stop-Process -Force
    Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
}
