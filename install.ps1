param(
    [string]$InstallDir = $(Join-Path $env:LOCALAPPDATA "ShellMind-Vision")
)

$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

Write-Host "=== ShellMind Vision Windows Installer ===" -ForegroundColor Cyan

$RepoUrl = "https://github.com/qira-dev/ShellMind-Vision.git"
$LocalManifest = if ($PSScriptRoot) { Join-Path $PSScriptRoot "package.json" } else { "" }

if ($LocalManifest -and (Test-Path $LocalManifest)) {
    $TargetDir = $PSScriptRoot
    Write-Host "[*] Using local repository: $TargetDir" -ForegroundColor Blue
} else {
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
        throw "git is required to clone ShellMind-Vision."
    }
    if (Test-Path (Join-Path $InstallDir ".git")) {
        Write-Host "[*] Updating $InstallDir..." -ForegroundColor Blue
        git -C $InstallDir pull --quiet
    } else {
        Write-Host "[*] Cloning ShellMind Vision into $InstallDir..." -ForegroundColor Blue
        git clone --depth 1 $RepoUrl $InstallDir
    }
    $TargetDir = $InstallDir
}

# Ensure Node.js >= 18 (or download portable Node v22 win-x64 with SHA256 verification)
$NeedNode = $true
$NodeCmd = Get-Command node -ErrorAction SilentlyContinue
if ($NodeCmd) {
    $Major = [int](((& node -v) -replace '^v', '').Split('.')[0])
    if ($Major -ge 18) { $NeedNode = $false }
}
$BundledNode = Join-Path $TargetDir "runtime\node\node.exe"
if ($NeedNode -and -not (Test-Path $BundledNode)) {
    Write-Host "[*] Downloading portable Node.js v22.14.0 (win-x64)..." -ForegroundColor Blue
    $NodeVer = "v22.14.0"
    $ZipName = "node-$NodeVer-win-x64.zip"
    $TmpDir = Join-Path $env:TEMP ("smv-node-" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null
    try {
        $ZipPath = Join-Path $TmpDir $ZipName
        $SumsPath = Join-Path $TmpDir "SHASUMS256.txt"
        Invoke-WebRequest -Uri "https://nodejs.org/dist/$NodeVer/$ZipName" -OutFile $ZipPath -UseBasicParsing
        Invoke-WebRequest -Uri "https://nodejs.org/dist/$NodeVer/SHASUMS256.txt" -OutFile $SumsPath -UseBasicParsing
        $ExpectedLine = Get-Content $SumsPath | Where-Object { $_ -match "\s+$([regex]::Escape($ZipName))$" } | Select-Object -First 1
        if (-not $ExpectedLine) { throw "Missing SHA256 entry for $ZipName" }
        $Expected = ($ExpectedLine -split "\s+")[0].ToUpperInvariant()
        $Actual = (Get-FileHash -Path $ZipPath -Algorithm SHA256).Hash.ToUpperInvariant()
        if ($Expected -ne $Actual) { throw "SHA256 mismatch for $ZipName" }
        $RuntimeDir = Join-Path $TargetDir "runtime"
        New-Item -ItemType Directory -Force -Path $RuntimeDir | Out-Null
        Expand-Archive -Path $ZipPath -DestinationPath $RuntimeDir -Force
        $Extracted = Join-Path $RuntimeDir "node-$NodeVer-win-x64"
        $DestNode = Join-Path $RuntimeDir "node"
        if (Test-Path $DestNode) { Remove-Item -Recurse -Force $DestNode }
        Move-Item -Path $Extracted -Destination $DestNode
    } finally {
        Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    }
}

# Build desktop-vision-rs.exe if cargo is available
$ExeDest = Join-Path $TargetDir "bin\desktop-vision-rs.exe"
if ((Get-Command cargo -ErrorAction SilentlyContinue) -and -not (Test-Path $ExeDest)) {
    Write-Host "[*] Building desktop-vision-rs.exe..." -ForegroundColor Blue
    Push-Location (Join-Path $TargetDir "src\desktop-vision-rs")
    try {
        cargo build --release --jobs 2
        Copy-Item "target\release\desktop-vision-rs.exe" $ExeDest -Force
    } finally {
        Pop-Location
    }
}

Write-Host "`n[OK] ShellMind Vision installed at $TargetDir" -ForegroundColor Green
Write-Host "  - Stdio MCP : node `"$TargetDir\mcp-stdio.mjs`""
Write-Host "  - HTTP MCP  : node `"$TargetDir\server.mjs`" (http://127.0.0.1:39200/mcp)"
