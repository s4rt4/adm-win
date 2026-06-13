<p align="center">
  <img src="banner.png" alt="Alpha Download Manager" width="100%">
</p>

<h1 align="center">Alpha Download Manager (ADM)</h1>

<p align="center">
  A fast, native Windows download manager written from scratch in Rust — an IDM-style experience without the bloat.
</p>

<p align="center">
  <img src="https://img.shields.io/badge/License-MIT-green.svg" alt="License: MIT">
  <img src="https://img.shields.io/badge/platform-Windows-0078D6" alt="Platform: Windows">
  <img src="https://img.shields.io/badge/built%20with-Rust-orange?logo=rust" alt="Built with Rust">
  <img src="https://img.shields.io/badge/status-unstable%20(0.1.0)-red" alt="Status: unstable">
</p>

---

**ADM** is a lightweight download accelerator for Windows. The UI is pure Win32
(no Electron, no web runtime), the engine is a from-scratch multi-connection
downloader, and the whole thing ships as a small, single executable with the
logo embedded.

> ⚠️ **Status: early & unstable (v0.1.0).** Expect bugs and breaking changes.

## ✨ Features

- **Multi-connection downloads** — segmented transfers for higher throughput, with
  a colorful per-connection progress view.
- **Resume that survives anything** — crash/stop-safe via a `.adm` sidecar; pick up
  exactly where you left off.
- **Refresh Link** — when a link expires (file hosts, etc.), paste a fresh URL into the
  existing download and it **continues from the current offset** instead of restarting.
- **Browser integration** — Chrome/Edge extension captures downloads and forwards the
  **cookies, referrer, and User-Agent**, so authenticated downloads (e.g. Gmail
  attachments) work too.
- **Video grabber (progressive)** — a floating "Download with ADM" panel detects
  progressive `mp4/webm/flv/mp3…` media playing on a page.
- **Queue, scheduler & speed limiter** — global and per-download bandwidth caps,
  start/stop times, and concurrent-download limits.
- **Categories** — auto-sorts downloads (Compressed / Documents / Music / Programs /
  Video) with a sidebar to filter, plus Unfinished / Finished / Queues views.
- **Batch & lists** — add many URLs at once (with `[1-10]` wildcard expansion), grab
  from the clipboard, run a **site grabber** over a page, and import/export URL lists.
- **Resilient networking** — friendly **download-failed** prompt, and an opt-in
  **"download anyway"** for servers with invalid TLS certificates.
- **Persistent** — your download list is restored across restarts.
- **Tiny & native** — Win32 GUI, tray icon, small release binary (`opt-level=z`, LTO,
  stripped), embedded app icon.

## 📦 Architecture

A Cargo workspace of focused crates:

| Crate | Role |
|-------|------|
| [`adm-core`](crates/adm-core) | Portable download engine: segmentation, resume, probing, limiter, site grabber. |
| [`adm-ipc`](crates/adm-ipc)   | JSON message types shared between app and bridge. |
| [`adm-app`](crates/adm-app)   | The Windows app: Win32 GUI + tray + in-process engine + named-pipe server. |
| [`adm-bridge`](crates/adm-bridge) | Native-messaging host (browser extension ↔ app over stdio/pipe). |
| [`extension/`](extension)     | MV3 browser extension (Chrome/Edge). |

## 🛠️ Build from source

**Requirements:** Windows + [Rust](https://rustup.rs) (stable, MSVC toolchain).

```sh
git clone https://github.com/s4rt4/adm-win
cd adm-win
cargo build --release
```

The app is at `target/release/adm-app.exe` (the native host is `adm-bridge.exe`).

Regenerate icon assets only if you change the SVGs:

```sh
cargo run --manifest-path tools/icongen/Cargo.toml --release
```

## 🌐 Browser integration

1. Build the workspace (above).
2. In `chrome://extensions` (or `edge://extensions`), enable **Developer mode** →
   **Load unpacked** → select the `extension/` folder.
3. Copy the **Extension ID** shown, then register the native host:
   ```sh
   target\release\adm-bridge.exe register <EXTENSION_ID>
   ```
4. Click a download in the browser, or right-click a link → **Download with ADM**.

To remove: `adm-bridge.exe unregister`. See [`extension/README.md`](extension/README.md)
and [`packaging/README.md`](packaging/README.md) for details and release packaging.

## 🚧 Scope & limitations

- **Windows only.**
- **No torrents.**
- The video grabber handles **progressive media only** — adaptive streaming
  (HLS `.m3u8` / DASH `.mpd`) and DRM-protected video are **not** supported.
- UI strings are English; the i18n mechanism exists but translations aren't filled in.

## 📄 License

Licensed under the [MIT License](LICENSE) © 2026 s4rt4.
