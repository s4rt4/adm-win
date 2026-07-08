//! Sidecar `.adm` — state resume tahan-crash (plan §7).
//!
//! Berisi URL, total, validator (ETag/Last-Modified), dan progres per-segmen.
//!
//! Disimpan di folder state aplikasi (`%LOCALAPPDATA%\ADM\state\`), BUKAN di
//! sebelah file output: sidecar di-flush tiap 500 ms selama unduhan, dan
//! operasi file berulang di folder tujuan membuat Explorer me-refresh view
//! terus-menerus (folder "berkedip", kotak rename in-place ke-reset). Lokasi
//! lama (`<file>.adm` di sebelah output) tetap dikenali lewat migrasi.

use crate::probe::Probe;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sidecar {
    pub url: String,
    pub total: u64,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
    pub segments: Vec<SegRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegRecord {
    pub start: u64,
    /// inklusif.
    pub end: u64,
    pub downloaded: u64,
}

/// Path sidecar untuk file output tertentu: `<state_dir>/<stem>-<hash>.adm`.
/// Hash FNV-1a 64-bit dari path absolut output (stabil antar-proses) menjaga
/// keunikan; stem disertakan agar mudah dikenali saat debugging.
pub fn path_for(output: &Path) -> PathBuf {
    let Some(dir) = state_dir() else {
        return legacy_path_for(output); // fallback: di sebelah output
    };
    let stem = output
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem: String = stem.chars().take(80).collect();
    dir.join(format!("{stem}-{:016x}.adm", fnv1a64(output)))
}

/// Folder state aplikasi (`%LOCALAPPDATA%\ADM\state`). None bila env tak ada
/// (mis. non-Windows) — pemanggil jatuh ke lokasi legacy di sebelah output.
fn state_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("ADM").join("state"))
}

/// Path sidecar lokasi LAMA (`<file>.adm` di sebelah output) — dipakai untuk
/// migrasi dan bersih-bersih.
pub fn legacy_path_for(output: &Path) -> PathBuf {
    let mut s = output.as_os_str().to_os_string();
    s.push(".adm");
    PathBuf::from(s)
}

/// FNV-1a 64-bit atas path (lossy UTF-8) — stabil antar-proses/versi Rust.
fn fnv1a64(p: &Path) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in p.to_string_lossy().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Migrasi sidecar dari lokasi lama (sebelah output) ke folder state, bila
/// yang baru belum ada. Copy+hapus (bukan rename) agar aman lintas-volume.
pub fn migrate_legacy(output: &Path) {
    let new = path_for(output);
    let old = legacy_path_for(output);
    if new == old || new.exists() || !old.exists() {
        return;
    }
    if let Some(parent) = new.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::copy(&old, &new).is_ok() {
        let _ = std::fs::remove_file(&old);
    }
}

/// Hapus sidecar untuk `output` — lokasi baru maupun legacy.
pub fn remove_for(output: &Path) {
    let _ = std::fs::remove_file(path_for(output));
    let _ = std::fs::remove_file(legacy_path_for(output));
}

/// Pindahkan state sidecar mengikuti perpindahan/rename file output.
pub fn rename_for(old_output: &Path, new_output: &Path) {
    migrate_legacy(old_output);
    let from = path_for(old_output);
    let to = path_for(new_output);
    if from != to && from.exists() {
        let _ = std::fs::rename(&from, &to);
    }
}

/// Muat sidecar bila ada & valid JSON.
pub fn load(path: &Path) -> Option<Sidecar> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Tulis sidecar di tempat (truncate + write), TANPA pola tmp+rename.
///
/// Sengaja tidak atomik: save dipanggil tiap 500 ms selama unduhan, dan
/// create+rename memicu event direktori yang membuat Explorer me-refresh
/// seluruh folder tujuan terus-menerus (isi folder "berkedip"). Menulis di
/// tempat hanya memicu event modifikasi file ini saja. Risiko crash tepat di
/// tengah tulis menghasilkan JSON tak valid → `load` mengembalikan None →
/// unduhan mulai dari awal (degradasi anggun, bukan korupsi).
pub fn save(path: &Path, sc: &Sidecar) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let data = serde_json::to_vec(sc).map_err(std::io::Error::other)?;
    std::fs::write(path, &data)
}

pub fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
}

impl Sidecar {
    /// Apakah sidecar masih cocok dengan kondisi server saat ini (resume aman).
    ///
    /// Kunci kecocokan adalah **ukuran total** (sinyal identitas file). Bila URL
    /// sama, validator (ETag/Last-Modified) ditegakkan ketat. Bila URL berbeda
    /// — kasus "Refresh Link": link kedaluwarsa diganti link segar untuk file
    /// yang sama — validator TIDAK diwajibkan, karena link yang diregenerasi
    /// sering mengubah ETag/Last-Modified meski byte-nya identik. Resume tetap
    /// dari offset selama ukuran cocok.
    pub fn is_compatible(&self, url: &str, probe: &Probe) -> bool {
        if Some(self.total) != probe.total {
            return false;
        }
        if self.url == url {
            if let (Some(a), Some(b)) = (&self.etag, &probe.etag) {
                if a != b {
                    return false;
                }
            }
            if let (Some(a), Some(b)) = (&self.last_modified, &probe.last_modified) {
                if a != b {
                    return false;
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sidecar(url: &str, total: u64, etag: Option<&str>) -> Sidecar {
        Sidecar {
            url: url.into(),
            total,
            etag: etag.map(|s| s.into()),
            last_modified: None,
            segments: vec![],
        }
    }

    fn probe(total: u64, etag: Option<&str>) -> Probe {
        Probe {
            total: Some(total),
            resumable: true,
            etag: etag.map(|s| s.into()),
            last_modified: None,
            suggested_filename: None,
        }
    }

    #[test]
    fn same_url_same_size_compatible() {
        let sc = sidecar("http://a/x", 1000, Some("v1"));
        assert!(sc.is_compatible("http://a/x", &probe(1000, Some("v1"))));
    }

    #[test]
    fn same_url_etag_changed_incompatible() {
        let sc = sidecar("http://a/x", 1000, Some("v1"));
        assert!(!sc.is_compatible("http://a/x", &probe(1000, Some("v2"))));
    }

    #[test]
    fn refresh_link_new_url_same_size_compatible() {
        // Kasus Refresh Link: URL beda, ukuran sama, ETag beda → tetap resume.
        let sc = sidecar("http://a/old?token=expired", 1000, Some("v1"));
        assert!(sc.is_compatible("http://a/fresh?token=new", &probe(1000, Some("v2"))));
    }

    #[test]
    fn refresh_link_size_mismatch_incompatible() {
        // Ukuran beda → kemungkinan file beda → jangan resume (mulai awal).
        let sc = sidecar("http://a/old", 1000, None);
        assert!(!sc.is_compatible("http://a/fresh", &probe(2048, None)));
    }
}
