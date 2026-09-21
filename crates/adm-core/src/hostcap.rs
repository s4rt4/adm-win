//! Batas koneksi adaptif per-host.
//!
//! Sebagian host (CDN file/streaming) membatasi koneksi paralel per IP dan
//! memutus sepihak koneksi yang berlebih. Akibatnya unduhan dengan 8 segmen
//! terus-menerus kena putus — retry menutupinya, tapi lambat dan boros.
//!
//! Modul ini mencatat "pengalaman" per-host selama proses hidup: unduhan yang
//! banyak di-retry menurunkan batas host itu (8 → 4 → 2 → 1), sedangkan
//! unduhan yang mulus tanpa satu pun retry menaikkannya kembali selangkah ke
//! arah angka yang diminta user. Tidak dipersist: kondisi jaringan berubah,
//! dan setelan user di Options tetap jadi batas atas.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// Batas terendah yang boleh dijatuhkan otomatis (di bawah ini unduhan jadi
/// sekuensial dan kehilangan seluruh keuntungan multi-koneksi).
const FLOOR: usize = 1;

static CAPS: LazyLock<Mutex<HashMap<String, usize>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Host dari URL (tanpa port) — kunci pencatatan. `None` bila URL tak terparse.
pub fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
}

/// Jumlah koneksi yang benar-benar dipakai untuk `host`: `wanted` (setelan
/// user) dibatasi oleh pengalaman buruk sebelumnya.
pub fn effective(host: &str, wanted: usize) -> usize {
    let caps = CAPS.lock().unwrap();
    match caps.get(host) {
        Some(&cap) => wanted.min(cap).max(FLOOR),
        None => wanted,
    }
}

/// Host ini menolak/memutus berulang kali pada `used` koneksi — separuhkan
/// batasnya untuk unduhan berikutnya. Kembalikan batas baru.
pub fn penalize(host: &str, used: usize) -> usize {
    let mut caps = CAPS.lock().unwrap();
    let cur = caps.get(host).copied().unwrap_or(used).min(used);
    let next = (cur / 2).max(FLOOR);
    caps.insert(host.to_string(), next);
    next
}

/// Unduhan mulus tanpa retry — naikkan batas selangkah ke arah `wanted`.
/// Entri dibuang begitu batas mencapai `wanted` lagi (kembali normal).
pub fn reward(host: &str, wanted: usize) {
    let mut caps = CAPS.lock().unwrap();
    let Some(&cur) = caps.get(host) else { return };
    let next = (cur * 2).min(wanted.max(FLOOR));
    if next >= wanted {
        caps.remove(host);
    } else {
        caps.insert(host.to_string(), next);
    }
}

/// Batas yang sedang berlaku untuk `host` (`None` = belum pernah diturunkan).
pub fn current(host: &str) -> Option<usize> {
    CAPS.lock().unwrap().get(host).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_parsing() {
        assert_eq!(host_of("https://Cdn.Example.com:8443/a/b.mkv"), Some("cdn.example.com".into()));
        assert_eq!(host_of("bukan-url"), None);
    }

    #[test]
    fn turun_lalu_pulih() {
        let h = "turun.example";
        assert_eq!(effective(h, 8), 8, "host baru pakai setelan user apa adanya");

        assert_eq!(penalize(h, 8), 4);
        assert_eq!(effective(h, 8), 4);
        assert_eq!(penalize(h, 4), 2);
        assert_eq!(penalize(h, 2), 1);
        // Tidak pernah jatuh ke 0 walau terus dihukum.
        assert_eq!(penalize(h, 1), 1);
        assert_eq!(effective(h, 8), 1);

        // Pemulihan bertahap, lalu entri hilang saat kembali ke angka user.
        reward(h, 8);
        assert_eq!(effective(h, 8), 2);
        reward(h, 8);
        assert_eq!(effective(h, 8), 4);
        reward(h, 8);
        assert_eq!(current(h), None);
        assert_eq!(effective(h, 8), 8);
    }

    #[test]
    fn setelan_user_tetap_batas_atas() {
        let h = "atas.example";
        penalize(h, 8); // cap 4
        assert_eq!(effective(h, 2), 2, "user minta 2 → tetap 2, bukan naik ke cap");
    }
}
