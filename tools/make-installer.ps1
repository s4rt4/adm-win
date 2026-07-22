# Bangun installer ADM (Inno Setup) → dist\ADM-Setup-<versi>.exe
# Pakai: powershell -ExecutionPolicy Bypass -File tools\make-installer.ps1 [-Build]
#   -Build : jalankan 'cargo build --release' dulu.
# Sumber file: exe dari target\release; sidecar (deno/ffmpeg/ffprobe/yt-dlp) dari
# target\release bila ada, kalau tidak dari %LOCALAPPDATA%\ADM (instalasi berjalan).
param([switch]$Build)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot

if ($Build) {
    Push-Location $repo
    try { cargo build --release; if ($LASTEXITCODE -ne 0) { throw "cargo build gagal" } }
    finally { Pop-Location }
}

# Versi dari [workspace.package] di Cargo.toml.
$verLine = Select-String -Path (Join-Path $repo "Cargo.toml") -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $verLine) { throw "versi tidak ditemukan di Cargo.toml" }
$version = $verLine.Matches[0].Groups[1].Value

$iscc = Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe"
if (-not (Test-Path $iscc)) { $iscc = "C:\Program Files (x86)\Inno Setup 6\ISCC.exe" }
if (-not (Test-Path $iscc)) { throw "ISCC.exe tidak ditemukan - instal Inno Setup 6 (winget install JRSoftware.InnoSetup)" }

$staging = Join-Path $repo "dist\staging"
$dist    = Join-Path $repo "dist"
if (Test-Path $staging) { Remove-Item -Recurse -Force $staging }
New-Item -ItemType Directory -Force $staging | Out-Null

$release = Join-Path $repo "target\release"
$appdata = Join-Path $env:LOCALAPPDATA "ADM"

foreach ($exe in "adm-app.exe", "adm-bridge.exe") {
    $s = Join-Path $release $exe
    if (-not (Test-Path $s)) { throw "$exe tidak ada di target\release - build dulu (atau pakai -Build)" }
    Copy-Item $s $staging
}
foreach ($sc in "deno.exe", "ffmpeg.exe", "ffprobe.exe", "yt-dlp.exe") {
    $s = Join-Path $release $sc
    if (-not (Test-Path $s)) { $s = Join-Path $appdata $sc }
    if (-not (Test-Path $s)) { throw "$sc tidak ditemukan di target\release maupun $appdata" }
    Copy-Item $s $staging
}
Copy-Item -Recurse (Join-Path $repo "extension") (Join-Path $staging "extension")

Write-Host "== ISCC =="
& $iscc (Join-Path $repo "installer\adm.iss") "/DStaging=$staging" "/DAppVersion=$version" "/O$dist"
if ($LASTEXITCODE -ne 0) { throw "ISCC gagal (exit $LASTEXITCODE)" }

Write-Host ""
Write-Host "Selesai: $(Join-Path $dist "ADM-Setup-$version.exe")"
