//! Resolver link "halaman" host berbagi berkas → URL berkas langsung.
//!
//! WeTransfer: `https://we.tl/t-XXXX` / `https://wetransfer.com/downloads/<id>/<hash>`
//! adalah halaman web, bukan berkas. URL berkas didapat dari
//! `POST /api/v4/transfers/<id>/download` → `direct_link` (token ±4 jam).
//! Karena token kedaluwarsa, resolusi dilakukan ulang setiap kali unduhan
//! dimulai/di-Resume; baris tetap menyimpan link halaman aslinya.
//!
//! Tombol Download di situs memicu `https://wetransfer.com/download/local-bartender?id=<base64 JSON>`
//! — URL yang hanya dilayani service worker browser (server membalas 404).
//! JSON-nya memuat `transferId`, `secret` (= security hash) dan `intent`, jadi
//! bisa diterjemahkan lewat API yang sama.

use crate::download::{build_client, ReqHeaders};
use crate::error::{Error, Result};
use url::Url;

/// Kembalikan URL berkas langsung bila `url` adalah link halaman host yang
/// dikenali; selain itu kembalikan `url` apa adanya (tanpa request jaringan).
pub async fn resolve_url(url: &str, h: &ReqHeaders, insecure: bool) -> Result<String> {
    let Ok(parsed) = Url::parse(url) else { return Ok(url.to_string()) };
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    if is_wetransfer(&host) {
        if let Some(t) = bartender_target(&parsed) {
            return wetransfer_direct(&t, h, insecure).await;
        }
    }
    if is_wetransfer_short(&host) || (is_wetransfer(&host) && wetransfer_parts(&parsed).is_some()) {
        return resolve_wetransfer(&parsed, h, insecure).await;
    }
    Ok(url.to_string())
}

fn is_wetransfer_short(host: &str) -> bool {
    host == "we.tl"
}

fn is_wetransfer(host: &str) -> bool {
    host == "wetransfer.com" || host.ends_with(".wetransfer.com")
}

/// `(transfer_id, recipient_id, security_hash)` dari path
/// `[/<locale>]/downloads/<id>[/<recipient>]/<hash>`.
fn wetransfer_parts(u: &Url) -> Option<(String, Option<String>, String)> {
    let segs: Vec<&str> = u.path_segments()?.filter(|s| !s.is_empty()).collect();
    let i = segs.iter().position(|s| *s == "downloads")?;
    match &segs[i + 1..] {
        [id, hash] => Some((id.to_string(), None, hash.to_string())),
        [id, rid, hash] => Some((id.to_string(), Some(rid.to_string()), hash.to_string())),
        _ => None,
    }
}

/// Target API WeTransfer: id transfer, security hash, recipient & intent.
#[derive(Debug, PartialEq)]
struct WtTarget {
    id: String,
    hash: String,
    recipient: Option<String>,
    intent: String,
    file_ids: Option<serde_json::Value>,
}

/// Payload `id` dari `/download/local-bartender?id=<base64 JSON>`.
fn bartender_target(u: &Url) -> Option<WtTarget> {
    if !u.path().trim_end_matches('/').ends_with("/download/local-bartender") {
        return None;
    }
    let raw = u.query_pairs().find(|(k, _)| k == "id")?.1.into_owned();
    let json = base64_decode(&raw)?;
    let v: serde_json::Value = serde_json::from_slice(&json).ok()?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    Some(WtTarget {
        id: s("transferId")?,
        hash: s("secret").or_else(|| s("securityHash"))?,
        recipient: s("recipientId"),
        intent: s("intent").unwrap_or_else(|| "entire_transfer".into()),
        file_ids: v.get("fileIds").or_else(|| v.get("file_ids")).cloned(),
    })
}

/// Base64 standar/url-safe, padding opsional.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

async fn resolve_wetransfer(u: &Url, h: &ReqHeaders, insecure: bool) -> Result<String> {
    let client = build_client(insecure, h)?;
    let mut page = u.clone();
    if is_wetransfer_short(page.host_str().unwrap_or("")) {
        // Short link → 302 ke halaman downloads; ikuti redirect-nya.
        let resp = client.get(page.as_str()).send().await?;
        page = resp.url().clone();
    }
    let (id, recipient, hash) = wetransfer_parts(&page)
        .ok_or_else(|| Error::Other(format!("Link WeTransfer tidak dikenali: {page}")))?;
    let t = WtTarget { id, hash, recipient, intent: "entire_transfer".into(), file_ids: None };
    wetransfer_direct_with(&client, &t).await
}

async fn wetransfer_direct(t: &WtTarget, h: &ReqHeaders, insecure: bool) -> Result<String> {
    let client = build_client(insecure, h)?;
    wetransfer_direct_with(&client, t).await
}

async fn wetransfer_direct_with(client: &reqwest::Client, t: &WtTarget) -> Result<String> {
    let mut body = serde_json::json!({ "intent": t.intent, "security_hash": t.hash });
    if let Some(r) = &t.recipient {
        body["recipient_id"] = serde_json::Value::String(r.clone());
    }
    if let Some(f) = &t.file_ids {
        body["file_ids"] = f.clone();
    }
    let api = format!("https://wetransfer.com/api/v4/transfers/{}/download", t.id);
    let resp = client
        .post(&api)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .body(body.to_string())
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    if let Some(link) = v.get("direct_link").and_then(|x| x.as_str()) {
        return Ok(link.to_string());
    }
    let msg = v.get("message").and_then(|x| x.as_str()).unwrap_or("tanpa direct_link");
    Err(Error::Other(format!(
        "WeTransfer menolak (HTTP {}): {msg} — transfer mungkin sudah kedaluwarsa/dihapus",
        status.as_u16()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(s: &str) -> Option<(String, Option<String>, String)> {
        wetransfer_parts(&Url::parse(s).unwrap())
    }

    #[test]
    fn parses_download_paths() {
        assert_eq!(
            parts("https://wetransfer.com/downloads/abc123/3b8f36?t_exp=1"),
            Some(("abc123".into(), None, "3b8f36".into()))
        );
        assert_eq!(
            parts("https://wetransfer.com/en/downloads/abc/rcp/hash"),
            Some(("abc".into(), Some("rcp".into()), "hash".into()))
        );
        assert_eq!(parts("https://wetransfer.com/"), None);
        assert_eq!(parts("https://wetransfer.com/downloads/onlyid"), None);
    }

    #[test]
    fn parses_local_bartender() {
        // {"transferId":"abc","secret":"3b8f36","intent":"entire_transfer"}
        let u = Url::parse(
            "https://wetransfer.com/download/local-bartender?id=eyJ0cmFuc2ZlcklkIjoiYWJjIiwic2VjcmV0IjoiM2I4ZjM2IiwiaW50ZW50IjoiZW50aXJlX3RyYW5zZmVyIn0%3D",
        )
        .unwrap();
        let t = bartender_target(&u).unwrap();
        assert_eq!((t.id.as_str(), t.hash.as_str(), t.intent.as_str()), ("abc", "3b8f36", "entire_transfer"));
        assert!(bartender_target(&Url::parse("https://wetransfer.com/download/x?id=e30=").unwrap()).is_none());
    }

    #[tokio::test]
    async fn non_wetransfer_untouched() {
        let u = "https://example.com/downloads/a/b";
        assert_eq!(resolve_url(u, &ReqHeaders::default(), false).await.unwrap(), u);
    }
}
