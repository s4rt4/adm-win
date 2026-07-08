# ADM

## Lokasi exe yang dipakai (user menyebutnya "unit test")

- Exe produksi yang dipakai sehari-hari ada di `C:\Users\Sarta\AppData\Local\ADM\adm-app.exe`.
- Shortcut Desktop "ADM" menunjuk ke lokasi AppData tersebut, BUKAN ke `target\release`.
- Sidecar wajib ada di folder yang sama dengan exe: `adm-bridge.exe`, `deno.exe`, `ffmpeg.exe`, `ffprobe.exe`, `yt-dlp.exe`, `com.adm.bridge.json`.

## Folder target

- Folder `target` aman di-`cargo clean` / dihapus kapan saja — jangan andalkan `target\release` sebagai lokasi exe yang dipakai.
- Setelah build rilis baru: jalankan `powershell -ExecutionPolicy Bypass -File tools\deploy.ps1` — menyalin `adm-app.exe` + `adm-bridge.exe` (dan sidecar jika berubah) ke `AppData\Local\ADM\`, lalu otomatis `register` ulang native messaging host dari exe AppData. Jangan salin manual tanpa register: registry native host pernah putus karena masih menunjuk manifest di `target\release` yang kena `cargo clean` (ekstensi mencegat download tapi tidak masuk ADM).
