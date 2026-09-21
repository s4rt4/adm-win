//! Tipe error engine.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("server tidak mengembalikan ukuran (Content-Length/Content-Range)")]
    UnknownSize,

    #[error("status http tak terduga: {0}")]
    BadStatus(u16),

    #[error("server mengabaikan header Range (status {0}); unduhan multi-segmen dibatalkan")]
    RangeIgnored(u16),

    #[error("koneksi terputus: {got} dari {want} byte diterima")]
    Truncated { got: u64, want: u64 },

    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Kode status HTTP bila kegagalan ini berasal dari respons server.
    /// Dipakai lapisan GUI untuk membedakan "link ditolak permanen" dari
    /// "koneksi bermasalah" tanpa menebak-nebak dari teks pesan.
    pub fn status_code(&self) -> Option<u16> {
        match self {
            Error::Http(e) => e.status().map(|s| s.as_u16()),
            Error::BadStatus(c) | Error::RangeIgnored(c) => Some(*c),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
