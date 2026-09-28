//! Pengaturan aplikasi yang persist (plan §9.16, §12). Disimpan sebagai JSON
//! di `%APPDATA%\ADM\settings.json`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// 0 = System, 1 = Light, 2 = Dark (plan §12).
    pub theme: u8,
    /// folder unduhan (None = default %USERPROFILE%\Downloads).
    pub download_dir: Option<String>,
    /// batas unduhan antrian bersamaan.
    pub queue_max: usize,
    /// batas kecepatan global (KB/s; 0 = tanpa batas).
    pub global_limit_kbps: u64,
    /// jumlah koneksi (segmen) per unduhan. Host tertentu membatasi koneksi
    /// paralel per IP dan memutus yang berlebih — turunkan bila sering gagal.
    pub connections: usize,
    /// jalankan saat login.
    pub autostart: bool,
    /// bahasa UI ("en" / "id").
    pub language: String,
    /// tampilkan dialog "Download complete" saat unduhan selesai (§9.14).
    pub show_complete_dialog: bool,
    /// path yt-dlp.exe (None = auto: folder aplikasi → PATH).
    pub youtube_ytdlp: Option<String>,
    /// path ffmpeg.exe (None = auto: folder aplikasi → PATH).
    pub youtube_ffmpeg: Option<String>,
    /// suara notifikasi per event (Options → Sounds...).
    pub sounds: Sounds,
}

/// Setelan suara satu event. `file` None/kosong = suara bawaan (embedded).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundPref {
    pub enabled: bool,
    pub file: Option<String>,
}

impl Default for SoundPref {
    fn default() -> Self {
        Self { enabled: true, file: None }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Sounds {
    pub complete: SoundPref,
    pub queue_done: SoundPref,
    pub failed: SoundPref,
    pub captured: SoundPref,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: 0,
            download_dir: None,
            queue_max: 1,
            global_limit_kbps: 0,
            connections: 8,
            autostart: false,
            language: "en".into(),
            show_complete_dialog: true,
            youtube_ytdlp: None,
            youtube_ffmpeg: None,
            sounds: Sounds::default(),
        }
    }
}

pub const THEME_SYSTEM: u8 = 0;
pub const THEME_LIGHT: u8 = 1;
pub const THEME_DARK: u8 = 2;

static CURRENT: Mutex<Option<Settings>> = Mutex::new(None);

fn dir() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("ADM")
}

fn file() -> PathBuf {
    dir().join("settings.json")
}

/// Muat dari disk (atau default), simpan di memori.
pub fn load() -> Settings {
    let s = std::fs::read(file())
        .ok()
        .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
        .unwrap_or_default();
    *CURRENT.lock().unwrap() = Some(s.clone());
    s
}

pub fn get() -> Settings {
    CURRENT.lock().unwrap().clone().unwrap_or_default()
}

/// Perbarui (via closure) lalu tulis ke disk.
pub fn update(f: impl FnOnce(&mut Settings)) {
    let mut guard = CURRENT.lock().unwrap();
    let mut s = guard.clone().unwrap_or_default();
    f(&mut s);
    *guard = Some(s.clone());
    // Tulis ke disk selagi memegang lock: dua update() bersamaan tak boleh
    // saling menimpa file dengan snapshot basi.
    save(&s);
    drop(guard);
}

fn save(s: &Settings) {
    let _ = std::fs::create_dir_all(dir());
    if let Ok(json) = serde_json::to_vec_pretty(s) {
        let tmp = file().with_extension("json.tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, file());
        }
    }
}
