//! Sidecar `.adm` — state resume tahan-crash (plan §7).
//!
//! Disimpan di sebelah file output (`<file>.adm`). Berisi URL, total, validator
//! (ETag/Last-Modified), dan progres per-segmen. Ditulis atomik (tmp + rename).

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

/// Path sidecar untuk file output tertentu.
pub fn path_for(output: &Path) -> PathBuf {
    let mut s = output.as_os_str().to_os_string();
    s.push(".adm");
    PathBuf::from(s)
}

/// Muat sidecar bila ada & valid JSON.
pub fn load(path: &Path) -> Option<Sidecar> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Tulis sidecar secara atomik.
pub fn save(path: &Path, sc: &Sidecar) -> std::io::Result<()> {
    let tmp = {
        let mut s = path.as_os_str().to_os_string();
        s.push(".tmp");
        PathBuf::from(s)
    };
    let data = serde_json::to_vec(sc).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, &data)?;
    std::fs::rename(&tmp, path)?; // MoveFileEx REPLACE_EXISTING di Windows
    Ok(())
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
