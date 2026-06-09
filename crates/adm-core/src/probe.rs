//! Probe Range: deteksi ukuran, dukungan resume, dan validator (plan §7).

use crate::error::Result;
use reqwest::header::{ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_RANGE, ETAG, LAST_MODIFIED, RANGE};
use reqwest::Client;

#[derive(Debug, Clone)]
pub struct Probe {
    /// Total ukuran bila diketahui.
    pub total: Option<u64>,
    /// Server mendukung Range (resume + multi-koneksi).
    pub resumable: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Nama berkas dari header `Content-Disposition` (bila ada).
    pub suggested_filename: Option<String>,
}

/// Probe dengan `Range: bytes=0-0` — cara paling andal mendeteksi dukungan
/// Range sekaligus total (lewat `Content-Range: bytes 0-0/<total>`).
pub async fn probe(client: &Client, url: &str) -> Result<Probe> {
    let resp = client
        .get(url)
        .header(RANGE, "bytes=0-0")
        .send()
        .await?;

    let status = resp.status();
    let headers = resp.headers();

    let etag = header_str(headers, ETAG);
    let last_modified = header_str(headers, LAST_MODIFIED);
    let suggested_filename = header_str(headers, CONTENT_DISPOSITION)
        .as_deref()
        .and_then(parse_content_disposition);

    let (total, resumable) = if status.as_u16() == 206 {
        // Content-Range: bytes 0-0/12345
        let total = headers
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range_total);
        (total, true)
    } else if status.is_success() {
        // 200: tanpa dukungan Range (kecuali Accept-Ranges: bytes).
        let total = resp.content_length();
        let accept_ranges = headers
            .get(ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.eq_ignore_ascii_case("bytes"))
            .unwrap_or(false);
        (total, accept_ranges)
    } else {
        return Err(crate::error::Error::BadStatus(status.as_u16()));
    };

    Ok(Probe {
        total,
        // Resume aman hanya bila ukuran diketahui.
        resumable: resumable && total.is_some(),
        etag,
        last_modified,
        suggested_filename,
    })
}

/// Ambil nama berkas dari header `Content-Disposition`.
/// Dukung `filename*=UTF-8''nama%20.rar` (RFC 5987) & `filename="nama.rar"`.
fn parse_content_disposition(v: &str) -> Option<String> {
    // Prioritaskan filename* (ter-encode).
    if let Some(i) = v.to_ascii_lowercase().find("filename*=") {
        let rest = v[i + "filename*=".len()..].trim();
        // bentuk: charset'lang'pct-encoded
        let enc = rest.split(';').next().unwrap_or(rest).trim();
        let value = enc.rsplit('\'').next().unwrap_or(enc);
        let decoded = percent_decode(value);
        let name = basename(&decoded);
        if !name.is_empty() {
            return Some(name);
        }
    }
    if let Some(i) = v.to_ascii_lowercase().find("filename=") {
        let rest = v[i + "filename=".len()..].trim();
        let raw = rest.split(';').next().unwrap_or(rest).trim();
        let unq = raw.trim_matches('"').trim();
        let name = basename(unq);
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn basename(s: &str) -> String {
    s.rsplit(['/', '\\']).next().unwrap_or(s).trim().to_string()
}

fn header_str(headers: &reqwest::header::HeaderMap, name: reqwest::header::HeaderName) -> Option<String> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
}

fn parse_content_range_total(v: &str) -> Option<u64> {
    // "bytes 0-0/12345" -> 12345 ; "bytes 0-0/*" -> None
    let slash = v.rfind('/')?;
    let total = &v[slash + 1..];
    total.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::parse_content_disposition;

    #[test]
    fn content_disposition() {
        assert_eq!(
            parse_content_disposition("attachment; filename=\"my file.rar\""),
            Some("my file.rar".into())
        );
        assert_eq!(
            parse_content_disposition("inline; filename=archive.zip"),
            Some("archive.zip".into())
        );
        assert_eq!(
            parse_content_disposition("attachment; filename*=UTF-8''na%20me.7z"),
            Some("na me.7z".into())
        );
        assert_eq!(
            parse_content_disposition("attachment; filename=\"..\\\\evil\\\\x.exe\""),
            Some("x.exe".into())
        );
        assert_eq!(parse_content_disposition("attachment"), None);
    }
}
