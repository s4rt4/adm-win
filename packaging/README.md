# Packaging & signing (WM7)

## 1. Build rilis
```
cargo build --release
```
Menghasilkan `target/release/adm-app.exe` & `adm-bridge.exe` (GUI tanpa console,
profil rilis kecil: opt-level=z, LTO, strip).

## 2. Regenerasi ikon (bila aset berubah)
```
cargo run --manifest-path tools/icongen/Cargo.toml --release
```
→ `crates/adm-app/assets/adm.ico` + `assets/icons/toolbar24.bin`.

## 3. Code signing (Authenticode) — **disarankan** (hindari SmartScreen, plan §15.5)
Butuh sertifikat code-signing (idealnya EV). Dengan SignTool (Windows SDK):
```
signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 ^
  /a target\release\adm-app.exe target\release\adm-bridge.exe
```
Tandatangani juga installer setelah dibuat (langkah 4).
> Tanpa sertifikat, binary tetap jalan tetapi SmartScreen mungkin memperingatkan.

## 4. Installer (Inno Setup)
Pasang Inno Setup, lalu:
```
ISCC packaging\inno\adm.iss
```
→ `target/installer/adm-setup-0.1.0.exe`. Tandatangani installer:
```
signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 /a target\installer\adm-setup-0.1.0.exe
```

## 5. Integrasi browser (per-mesin, setelah extension dipasang)
Native messaging host bergantung Extension ID:
```
"%ProgramFiles%\ADM\adm-bridge.exe" register <EXTENSION_ID> [FIREFOX_ID]
```
Lihat `extension/README.md`.

## Status (WM7)
- ✅ Build rilis, ikon, installer Inno, autostart opsional, shortcut.
- ⏳ **Signing**: butuh sertifikat milik kamu (perintah di atas siap pakai).
- ⏳ **MSIX** (opsional, plan §3) belum.
- ⏳ **i18n**: mekanisme tersedia (setting `language` + combo di Options),
  tapi tabel string terjemahan belum diisi — UI masih berbahasa Inggris.
