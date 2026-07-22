# ADM Browser Integration (WM5)

Extension MV3 + native messaging host (`com.adm.bridge`). Lihat plan §11.

## Komponen
- `extension/` — WebExtension MV3 (Chrome/Edge; Firefox perlu sedikit penyesuaian manifest).
- `adm-bridge.exe` — native messaging host (stdio ↔ Named Pipe ke `adm-app`).

## Cara pasang (development, Chrome/Edge)

1. **Build** workspace: `cargo build` (menghasilkan `adm-bridge.exe` & `adm-app.exe` di `target/debug`).
2. **Muat extension**: buka `chrome://extensions` (atau `edge://extensions`) → aktifkan *Developer mode* → *Load unpacked* → pilih folder `extension/`.
3. **Salin Extension ID** yang muncul (32 huruf).
4. **Daftarkan host** (tulis manifest + registry `HKCU`):
   ```
   target\debug\adm-bridge.exe register <EXTENSION_ID> [FIREFOX_ID]
   ```
5. Selesai. Coba klik sebuah unduhan di browser, atau klik kanan link → **Download with ADM**.
   - Bila `adm-app` belum jalan, bridge otomatis men-spawn-nya (`--tray`).
   - Toggle "tangkap unduhan otomatis" ada di popup ikon extension.

## Lepas
```
target\debug\adm-bridge.exe unregister
```

## Catatan
- Registry yang ditulis: `HKCU\Software\{Google\Chrome|Microsoft\Edge|Mozilla}\NativeMessagingHosts\com.adm.bridge` → path manifest.
- Manifest Chrome/Edge memakai `allowed_origins` (chrome-extension://ID/); Firefox memakai `allowed_extensions`.
- Di rilis, installer (WiX/Inno, §11.2) yang menulis registry + manifest; langkah `register` manual ini untuk development.
- Extension ID kini STABIL: `cjamijdkchdmdocnpdbjobcnmagdjcfe` (field `key` di `manifest.json`;
  private key di `secrets/adm-extension-key.pem`, di-gitignore — JANGAN hilang/bocor, dipakai lagi
  bila kelak publish ke Chrome Web Store agar ID tetap sama).
- Installer rilis: `tools\make-installer.ps1` → `dist\ADM-Setup-<versi>.exe` (Inno Setup 6).
  Installer menyalin app+sidecar+folder `extension\` ke `%LOCALAPPDATA%\ADM`, lalu otomatis
  `register` ID stabil di atas. Uninstall menjalankan `unregister`.
