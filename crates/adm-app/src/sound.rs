//! Suara notifikasi (download selesai / antrean selesai / gagal / ditangkap
//! dari browser). Tiap event punya suara bawaan yang di-embed ke exe (tak ada
//! file sidecar yang bisa hilang) dan bisa diganti file milik user lewat
//! Options → Sounds.... WAV diputar via `PlaySoundW`, format lain (MP3 dll.)
//! via MCI — keduanya bawaan Windows, tanpa dependency tambahan.

use crate::settings::{self, SoundPref};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_MEMORY, SND_NODEFAULT};
use windows::Win32::Media::Multimedia::mciSendStringW;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Complete,
    QueueDone,
    Failed,
    Captured,
}

impl Event {
    pub const ALL: [Event; 4] = [Event::Complete, Event::QueueDone, Event::Failed, Event::Captured];

    pub fn label(self) -> &'static str {
        match self {
            Event::Complete => "Download complete",
            Event::QueueDone => "All downloads finished",
            Event::Failed => "Download failed",
            Event::Captured => "Download captured from browser",
        }
    }

    /// WAV bawaan: mono 22 kHz 16-bit (dikonversi dari sounds/ mixkit) supaya
    /// tambahan ukuran exe tetap kecil.
    fn builtin(self) -> &'static [u8] {
        match self {
            Event::Complete => include_bytes!("../assets/sounds/complete.wav"),
            Event::QueueDone => include_bytes!("../assets/sounds/queue-done.wav"),
            Event::Failed => include_bytes!("../assets/sounds/failed.wav"),
            Event::Captured => include_bytes!("../assets/sounds/captured.wav"),
        }
    }

    pub fn pref(self, s: &settings::Settings) -> &SoundPref {
        match self {
            Event::Complete => &s.sounds.complete,
            Event::QueueDone => &s.sounds.queue_done,
            Event::Failed => &s.sounds.failed,
            Event::Captured => &s.sounds.captured,
        }
    }

    pub fn pref_mut(self, s: &mut settings::Settings) -> &mut SoundPref {
        match self {
            Event::Complete => &mut s.sounds.complete,
            Event::QueueDone => &mut s.sounds.queue_done,
            Event::Failed => &mut s.sounds.failed,
            Event::Captured => &mut s.sounds.captured,
        }
    }
}

/// Putar suara event bila diaktifkan. Mengembalikan `true` bila suara diputar
/// — pemanggil lalu membungkam suara bawaan Windows (balon / MessageBox) agar
/// tidak bunyi dobel.
pub fn play(ev: Event) -> bool {
    let cfg = settings::get();
    let p = ev.pref(&cfg);
    if !p.enabled {
        return false;
    }
    preview(ev, p.file.as_deref());
    true
}

/// Putar tanpa memeriksa `enabled` (tombol Test). `file` kosong/None, atau
/// file yang sudah tidak ada/gagal diputar → suara bawaan, supaya notifikasi
/// tidak diam-diam hilang gara-gara file custom terhapus.
pub fn preview(ev: Event, file: Option<&str>) {
    if let Some(f) = file.map(str::trim).filter(|f| !f.is_empty()) {
        if Path::new(f).is_file() && play_file(f) {
            return;
        }
    }
    play_builtin(ev);
}

fn play_builtin(ev: Event) {
    stop_mci();
    let data = ev.builtin();
    // SND_MEMORY + SND_ASYNC aman: datanya 'static, hidup sampai proses keluar.
    unsafe {
        let _ = PlaySoundW(PCWSTR(data.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

fn play_file(path: &str) -> bool {
    let is_wav = Path::new(path)
        .extension()
        .map(|e| e.eq_ignore_ascii_case("wav"))
        .unwrap_or(false);
    if is_wav {
        stop_mci();
        let h = HSTRING::from(path);
        return unsafe { PlaySoundW(PCWSTR(h.as_ptr()), None, SND_FILENAME | SND_ASYNC | SND_NODEFAULT).as_bool() };
    }
    // MCI tak bisa meng-escape tanda kutip di path; Windows juga melarang `"`
    // di nama file, jadi cukup ditolak.
    if path.contains('"') {
        return false;
    }
    unsafe {
        // Hentikan WAV yang mungkin masih berbunyi agar tak tumpang tindih.
        let _ = PlaySoundW(PCWSTR::null(), None, SND_ASYNC);
    }
    stop_mci();
    mci(&format!("open \"{path}\" type mpegvideo alias {MCI_ALIAS}")) && mci(&format!("play {MCI_ALIAS}"))
}

const MCI_ALIAS: &str = "adm_notify";

fn stop_mci() {
    let _ = mci(&format!("close {MCI_ALIAS}"));
}

fn mci(cmd: &str) -> bool {
    let h = HSTRING::from(cmd);
    unsafe { mciSendStringW(PCWSTR(h.as_ptr()), None, None) == 0 }
}

/// Unduhan yang selesai sejak antrean terakhir kosong — membedakan "satu
/// unduhan selesai" dari "seluruh antrean (≥2 unduhan) selesai".
static DONE_SINCE_IDLE: AtomicUsize = AtomicUsize::new(0);

/// Satu suara untuk satu batch notifikasi (bukan per file, supaya banyak
/// unduhan yang selesai bersamaan tidak berbunyi beruntun). Prioritas: gagal,
/// lalu antrean selesai, lalu selesai. Event yang dimatikan jatuh ke event
/// berikutnya. Mengembalikan event yang benar-benar diputar.
pub fn notify_batch(completed: usize, failed: usize, idle: bool) -> Option<Event> {
    let done = DONE_SINCE_IDLE.fetch_add(completed, Ordering::SeqCst) + completed;
    if idle {
        DONE_SINCE_IDLE.store(0, Ordering::SeqCst);
    }
    if failed > 0 && play(Event::Failed) {
        return Some(Event::Failed);
    }
    if completed == 0 {
        return None;
    }
    if idle && done >= 2 && play(Event::QueueDone) {
        return Some(Event::QueueDone);
    }
    play(Event::Complete).then_some(Event::Complete)
}
