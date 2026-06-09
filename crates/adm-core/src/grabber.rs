//! Site grabber sederhana: ambil HTML sebuah halaman, lalu ekstrak tautan
//! berkas yang bisa diunduh (atribut `href`/`src`) sebagai URL absolut.

use crate::error::{Error, Result};
use std::collections::HashSet;
use url::Url;

/// Ekstensi berkas yang dianggap "bisa diunduh".
const DOWNLOADABLE: &[&str] = &[
    "zip", "rar", "7z", "gz", "tgz", "tar", "bz2", "xz", "iso", "exe", "msi", "apk", "dmg", "deb",
    "rpm", "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "mp3", "wav", "flac", "ogg", "m4a",
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "jpg", "jpeg", "png", "gif", "bmp", "webp",
    "svg", "txt", "csv", "json", "xml", "epub", "mobi",
];

/// Ambil halaman lalu kembalikan daftar tautan berkas (URL absolut, unik).
pub async fn grab_links(page_url: &str) -> Result<Vec<String>> {
    let base = Url::parse(page_url).map_err(|e| Error::Other(format!("URL tak valid: {e}")))?;
    let html = crate::download::fetch_text(page_url).await?;
    Ok(extract_links(&html, &base))
}

/// Ekstrak tautan berkas dari HTML relatif terhadap `base`. Murni & teruji.
pub fn extract_links(html: &str, base: &Url) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for attr in ["href", "src"] {
        let mut from = 0usize;
        while let Some(rel) = lower[from..].find(attr) {
            let i = from + rel;
            from = i + attr.len();
            // Setelah nama atribut: spasi opsional lalu '='.
            let after = html[from..].trim_start();
            let Some(after) = after.strip_prefix('=') else { continue };
            let after = after.trim_start();
            let (quote, body) = if let Some(b) = after.strip_prefix('"') {
                ('"', b)
            } else if let Some(b) = after.strip_prefix('\'') {
                ('\'', b)
            } else {
                continue;
            };
            let Some(end) = body.find(quote) else { continue };
            let val = &body[..end];
            if val.is_empty() {
                continue;
            }
            let Ok(abs) = base.join(val) else { continue };
            if !matches!(abs.scheme(), "http" | "https") {
                continue;
            }
            let path = abs.path().to_ascii_lowercase();
            let ext = path.rsplit('.').next().unwrap_or("");
            if path.contains('.') && DOWNLOADABLE.contains(&ext) {
                let s = abs.to_string();
                if seen.insert(s.clone()) {
                    out.push(s);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("http://site.test/dir/page.html").unwrap()
    }

    #[test]
    fn absolute_and_relative() {
        let html = r#"<a href="https://cdn.x/a.zip">a</a> <a href='/b.rar'>b</a> <a href="c.pdf">c</a>"#;
        assert_eq!(
            extract_links(html, &base()),
            vec![
                "https://cdn.x/a.zip".to_string(),
                "http://site.test/b.rar".to_string(),
                "http://site.test/dir/c.pdf".to_string(),
            ]
        );
    }

    #[test]
    fn skips_non_downloadable_and_dedups() {
        let html = r#"<a href="page2.html">x</a><a href="a.zip">1</a><a href="a.zip">2</a>"#;
        assert_eq!(extract_links(html, &base()), vec!["http://site.test/dir/a.zip"]);
    }

    #[test]
    fn picks_up_src_attr() {
        let html = r#"<img src="/img/logo.png"><script src="app.js"></script>"#;
        assert_eq!(extract_links(html, &base()), vec!["http://site.test/img/logo.png"]);
    }

    #[test]
    fn ignores_mailto_and_anchors() {
        let html = r##"<a href="mailto:x@y.z">m</a><a href="#top">t</a><a href="movie.mp4">v</a>"##;
        assert_eq!(extract_links(html, &base()), vec!["http://site.test/dir/movie.mp4"]);
    }
}
