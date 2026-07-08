# Salin build rilis ke folder produksi (%LOCALAPPDATA%\ADM) + register ulang native host.
# Pakai: powershell -ExecutionPolicy Bypass -File tools\deploy.ps1 [-Build] [-ExtensionId <id>]
#   -Build        : jalankan 'cargo build --release' dulu sebelum menyalin.
#   -ExtensionId  : ID extension Chrome/Edge (default = ID unpacked saat ini).
param(
    [string]$ExtensionId = "jdgkcdnbhdjkahjfeadeplbdinabncci",
    [switch]$Build
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$src  = Join-Path $repo "target\release"
$dst  = Join-Path $env:LOCALAPPDATA "ADM"

if ($Build) {
    Write-Host "== cargo build --release =="
    Push-Location $repo
    try { cargo build --release; if ($LASTEXITCODE -ne 0) { throw "cargo build gagal (exit $LASTEXITCODE)" } }
    finally { Pop-Location }
}

foreach ($exe in "adm-app.exe", "adm-bridge.exe") {
    if (-not (Test-Path (Join-Path $src $exe))) {
        throw "$exe tidak ditemukan di $src - jalankan 'cargo build --release' dulu (atau pakai -Build)."
    }
}
if (-not (Test-Path $dst)) { New-Item -ItemType Directory -Force $dst | Out-Null }

# Hentikan proses yang sedang memakai exe tujuan agar Copy-Item tidak gagal file-locked.
foreach ($name in "adm-app", "adm-bridge") {
    $proc = Get-Process -Name $name -ErrorAction SilentlyContinue
    if ($proc) {
        Write-Host "Menghentikan proses $name (PID $($proc.Id -join ', '))..."
        $proc | Stop-Process -Force
        $proc | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
    }
}

# Exe hasil build: selalu disalin.
foreach ($exe in "adm-app.exe", "adm-bridge.exe") {
    Copy-Item (Join-Path $src $exe) (Join-Path $dst $exe) -Force
    Write-Host "Disalin: $exe"
}

# Sidecar pihak ketiga: salin hanya bila ada di target\release dan berbeda dari tujuan.
foreach ($sc in "deno.exe", "ffmpeg.exe", "ffprobe.exe", "yt-dlp.exe") {
    $s = Join-Path $src $sc
    $d = Join-Path $dst $sc
    if (Test-Path $s) {
        if (-not (Test-Path $d) -or (Get-Item $s).LastWriteTime -ne (Get-Item $d).LastWriteTime) {
            Copy-Item $s $d -Force
            Write-Host "Disalin: $sc"
        }
    } elseif (-not (Test-Path $d)) {
        Write-Warning "$sc tidak ada di sumber maupun tujuan - fitur terkait tidak akan jalan."
    }
}

# Register ulang native messaging host DARI exe produksi, supaya manifest + registry
# menunjuk ke AppData (bukan target\release yang bisa kena cargo clean).
Write-Host "== register native host =="
& (Join-Path $dst "adm-bridge.exe") register $ExtensionId
if ($LASTEXITCODE -ne 0) { throw "register gagal (exit $LASTEXITCODE)" }

Write-Host ""
Write-Host "Selesai. Restart browser bila sebelumnya bridge sempat tidak terdaftar."
