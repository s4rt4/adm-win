//! Test integrasi engine (kriteria WM1): multi-koneksi + checksum, resume
//! setelah cancel (mensimulasikan stop/crash), fallback non-Range, retry
//! otomatis saat host memutus body di tengah, gagal-cepat untuk status
//! permanen (403) yang memang berarti link kedaluwarsa, dan mengalah (bukan
//! gagal) saat host menolak koneksi paralel berlebih dengan 403.

use adm_core::{download, CancelToken, DownloadRequest, Limiter, Outcome};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use tiny_http::{Header, Response, Server, StatusCode};

const ETAG: &str = "\"adm-test-v1\"";

/// Sisa jatah "pemutusan" yang disuntikkan server untuk path `flaky`: tiap
/// permintaan Range yang kena jatah ini hanya dikirim separuh lalu koneksi
/// ditutup — meniru host yang memutus transfer di tengah jalan.
static FLAKY_LEFT: AtomicUsize = AtomicUsize::new(0);

/// Alamat loopback khusus test flaky (lihat `start_server_at`).
const FLAKY_HOST: &str = "127.0.0.2";

/// Koneksi Range besar yang sedang dilayani path `limited`, dan batasnya.
static LIMITED_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
const LIMITED_MAX: usize = 2;
const LIMITED_HOST: &str = "127.0.0.3";

fn unlimited() -> Arc<Limiter> {
    Arc::new(Limiter::unlimited())
}

fn make_payload(n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| {
            let x = (i as u64).wrapping_mul(2_654_435_761) ^ ((i as u64) >> 3);
            (x & 0xff) as u8
        })
        .collect()
}

fn sha256(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

struct ServerGuard {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Server HTTP lokal yang melayani `payload` dengan dukungan Range + ETag.
/// Path mengandung "norange" => server abaikan Range & tak umumkan Accept-Ranges.
fn start_server(payload: Arc<Vec<u8>>) -> (String, ServerGuard) {
    start_server_at("127.0.0.1", payload)
}

/// Varian dengan alamat bind pilihan sendiri. Test yang menyentuh state global
/// per-host (`adm_core::hostcap`) memakai alamat loopback berbeda agar tidak
/// saling mengganggu dengan test lain yang jalan paralel di proses yang sama.
fn start_server_at(bind: &str, payload: Arc<Vec<u8>>) -> (String, ServerGuard) {
    let server = Arc::new(Server::http(format!("{bind}:0")).unwrap());
    let addr = server.server_addr().to_ip().unwrap();
    let base = format!("http://{}", addr);
    let stop = Arc::new(AtomicBool::new(false));

    let mut threads = Vec::new();
    for _ in 0..8 {
        let server = server.clone();
        let payload = payload.clone();
        let stop = stop.clone();
        threads.push(std::thread::spawn(move || loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            match server.recv_timeout(Duration::from_millis(100)) {
                Ok(Some(req)) => handle(req, &payload),
                Ok(None) => continue,
                Err(_) => break,
            }
        }));
    }

    (base, ServerGuard { stop, threads })
}

fn handle(req: tiny_http::Request, payload: &[u8]) {
    let total = payload.len();
    let no_range = req.url().contains("norange");

    // Link yang benar-benar ditolak server (kedaluwarsa): harus gagal cepat.
    if req.url().contains("forbidden") {
        let _ = req.respond(Response::from_data(Vec::new()).with_status_code(StatusCode(403)));
        return;
    }

    let range = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Range"))
        .map(|h| h.value.as_str().to_string());

    let etag_header = Header::from_bytes(&b"ETag"[..], ETAG.as_bytes()).unwrap();

    if no_range {
        let resp = Response::from_data(payload.to_vec()).with_header(etag_header);
        let _ = req.respond(resp);
        return;
    }

    match range.as_deref().and_then(parse_range) {
        Some((a, b_opt)) => {
            let mut b = b_opt.unwrap_or(total as u64 - 1).min(total as u64 - 1);
            let a = a.min(total as u64 - 1);
            // Potong body di tengah (hanya untuk permintaan besar, agar probe
            // `bytes=0-0` tetap utuh) selama jatah flaky masih ada.
            if req.url().contains("flaky")
                && b - a > 1024
                && FLAKY_LEFT
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok()
            {
                b = a + (b - a) / 2;
            }
            // Host berbatas koneksi (meniru pixeldrain): koneksi ke-3 dst.
            // ditolak 403 selama dua lainnya masih mengalir.
            let mut held = false;
            if req.url().contains("limited") && b - a > 1024 {
                if LIMITED_IN_FLIGHT.fetch_add(1, Ordering::SeqCst) >= LIMITED_MAX {
                    LIMITED_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                    let _ = req.respond(Response::from_data(Vec::new()).with_status_code(StatusCode(403)));
                    return;
                }
                held = true;
                std::thread::sleep(Duration::from_millis(150));
            }
            let slice = payload[a as usize..=b as usize].to_vec();
            let cr = format!("bytes {}-{}/{}", a, b, total);
            let resp = Response::from_data(slice)
                .with_status_code(StatusCode(206))
                .with_header(Header::from_bytes(&b"Content-Range"[..], cr.as_bytes()).unwrap())
                .with_header(Header::from_bytes(&b"Accept-Ranges"[..], &b"bytes"[..]).unwrap())
                .with_header(etag_header);
            let _ = req.respond(resp);
            if held {
                LIMITED_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
            }
        }
        None => {
            let resp = Response::from_data(payload.to_vec())
                .with_header(Header::from_bytes(&b"Accept-Ranges"[..], &b"bytes"[..]).unwrap())
                .with_header(etag_header);
            let _ = req.respond(resp);
        }
    }
}

fn parse_range(v: &str) -> Option<(u64, Option<u64>)> {
    let rest = v.trim().strip_prefix("bytes=")?;
    let (a, b) = rest.split_once('-')?;
    let a: u64 = a.trim().parse().ok()?;
    let b = if b.trim().is_empty() {
        None
    } else {
        Some(b.trim().parse().ok()?)
    };
    Some((a, b))
}

fn read_file(path: &std::path::Path) -> Vec<u8> {
    let mut f = std::fs::File::open(path).unwrap();
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).unwrap();
    buf
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_connection_checksum() {
    let payload = Arc::new(make_payload(2 * 1024 * 1024)); // 2 MiB
    let (base, _srv) = start_server(payload.clone());
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("file.bin");

    let req = DownloadRequest {
        url: format!("{base}/file.bin"),
        output: out.clone(),
        connections: 8,
        insecure: false,
        referrer: None,
        user_agent: None,
        cookies: None,
    };
    let outcome = download(req, CancelToken::new(), None, unlimited(), unlimited())
        .await
        .unwrap();
    assert!(matches!(outcome, Outcome::Completed { bytes } if bytes == payload.len() as u64));

    let got = read_file(&out);
    assert_eq!(got.len(), payload.len());
    assert_eq!(sha256(&got), sha256(&payload), "checksum harus cocok");

    // Sidecar harus terhapus setelah selesai.
    assert!(!adm_core_sidecar_exists(&out), "sidecar .adm harus hilang");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_after_cancel() {
    let payload = Arc::new(make_payload(2 * 1024 * 1024)); // 2 MiB
    let (base, _srv) = start_server(payload.clone());
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("file.bin");
    let url = format!("{base}/file.bin");

    // Percobaan 1: batasi 512 KiB/s lalu cancel di tengah jalan.
    let cancel = CancelToken::new();
    {
        let c = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(600)).await;
            c.cancel();
        });
    }
    let req1 = DownloadRequest {
        url: url.clone(),
        output: out.clone(),
        connections: 4,
        insecure: false,
        referrer: None,
        user_agent: None,
        cookies: None,
    };
    // batasi 512 KiB/s lewat per-limiter agar cancel sempat di tengah.
    let per = Arc::new(Limiter::new(512 * 1024));
    let o1 = download(req1, cancel, None, per, unlimited()).await.unwrap();
    let mid = match o1 {
        Outcome::Paused { downloaded, .. } => downloaded,
        Outcome::Completed { .. } => panic!("seharusnya ter-pause, bukan selesai"),
    };
    assert!(mid > 0 && (mid as usize) < payload.len(), "harus parsial: {mid}");
    assert!(adm_core_sidecar_exists(&out), "sidecar harus ada setelah pause");

    // Percobaan 2: lanjutkan tanpa batas/cancel — harus selesai & utuh.
    let req2 = DownloadRequest {
        url,
        output: out.clone(),
        connections: 4,
        insecure: false,
        referrer: None,
        user_agent: None,
        cookies: None,
    };
    let o2 = download(req2, CancelToken::new(), None, unlimited(), unlimited())
        .await
        .unwrap();
    assert!(matches!(o2, Outcome::Completed { .. }));

    let got = read_file(&out);
    assert_eq!(sha256(&got), sha256(&payload), "checksum setelah resume harus cocok");
    assert!(!adm_core_sidecar_exists(&out), "sidecar harus hilang setelah selesai");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fallback_no_range() {
    let payload = Arc::new(make_payload(512 * 1024)); // 512 KiB
    let (base, _srv) = start_server(payload.clone());
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("norange.bin");

    let req = DownloadRequest {
        url: format!("{base}/norange.bin"), // server abaikan Range
        output: out.clone(),
        connections: 8, // diminta 8, tapi engine harus fallback ke 1
        insecure: false,
        referrer: None,
        user_agent: None,
        cookies: None,
    };
    let outcome = download(req, CancelToken::new(), None, unlimited(), unlimited())
        .await
        .unwrap();
    assert!(matches!(outcome, Outcome::Completed { .. }));
    let got = read_file(&out);
    assert_eq!(sha256(&got), sha256(&payload));
}

/// Host yang memutus body di tengah jalan: engine harus mencoba ulang tiap
/// segmen dari byte terakhir yang tertulis, bukan menggagalkan seluruh unduhan
/// (gejala lama: "Download failed" padahal Resume langsung jalan lagi).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retries_truncated_body() {
    let payload = Arc::new(make_payload(1024 * 1024)); // 1 MiB
    // Loopback tersendiri: test ini sengaja "menghukum" host-nya (lihat akhir
    // test), jadi jangan dicampur dengan 127.0.0.1 milik test lain.
    let (base, _srv) = start_server_at(FLAKY_HOST, payload.clone());
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("flaky.bin");

    // Tiap segmen kena satu pemutusan; tanpa retry unduhan pasti gagal.
    FLAKY_LEFT.store(4, Ordering::SeqCst);

    let req = DownloadRequest {
        url: format!("{base}/flaky.bin"),
        output: out.clone(),
        connections: 4,
        insecure: false,
        referrer: None,
        user_agent: None,
        cookies: None,
    };
    let outcome = download(req, CancelToken::new(), None, unlimited(), unlimited())
        .await
        .unwrap();
    assert!(matches!(outcome, Outcome::Completed { .. }), "retry harus menuntaskan unduhan");
    assert_eq!(FLAKY_LEFT.load(Ordering::SeqCst), 0, "pemutusan harus benar-benar terjadi");

    let got = read_file(&out);
    assert_eq!(sha256(&got), sha256(&payload), "checksum setelah retry harus cocok");
    assert!(!adm_core_sidecar_exists(&out), "sidecar harus hilang setelah selesai");

    // Host yang memutus berulang kali harus diturunkan batas koneksinya untuk
    // unduhan berikutnya (4 segmen dengan 4 retry => separuh jadi 2).
    assert_eq!(
        adm_core::hostcap::current(FLAKY_HOST),
        Some(2),
        "batas koneksi host harus turun setelah banyak retry"
    );
    assert_eq!(adm_core::hostcap::effective(FLAKY_HOST, 8), 2);
}

/// Link yang ditolak permanen (403) bukan gangguan sesaat: engine harus
/// menyerah segera agar user cepat diarahkan ke Refresh Link, bukan menghabiskan
/// seluruh jatah retry + backoff.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permanent_status_fails_fast() {
    let payload = Arc::new(make_payload(64 * 1024));
    let (base, _srv) = start_server(payload);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("forbidden.bin");

    let req = DownloadRequest {
        url: format!("{base}/forbidden.bin"),
        output: out,
        connections: 4,
        insecure: false,
        referrer: None,
        user_agent: None,
        cookies: None,
    };
    let t0 = std::time::Instant::now();
    let res = download(req, CancelToken::new(), None, unlimited(), unlimited()).await;
    assert!(res.is_err(), "403 harus dilaporkan gagal");
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "403 tak boleh menunggu backoff retry: {:?}",
        t0.elapsed()
    );
}

/// 403 di tengah unduhan saat segmen lain masih jalan = jatah koneksi host
/// habis, bukan link mati: segmen yang ditolak harus menunggu giliran dan
/// unduhan tetap tuntas utuh, lalu batas koneksi host diturunkan.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_limit_403_waits_instead_of_failing() {
    let payload = Arc::new(make_payload(1024 * 1024));
    let (base, _srv) = start_server_at(LIMITED_HOST, payload.clone());
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("limited.bin");

    let req = DownloadRequest {
        url: format!("{base}/limited.bin"),
        output: out.clone(),
        connections: 8,
        insecure: false,
        referrer: None,
        user_agent: None,
        cookies: None,
    };
    let outcome = download(req, CancelToken::new(), None, unlimited(), unlimited())
        .await
        .expect("403 karena batas koneksi tak boleh menggagalkan unduhan");
    assert!(matches!(outcome, Outcome::Completed { .. }));
    assert_eq!(sha256(&read_file(&out)), sha256(&payload), "checksum harus cocok");
    assert_eq!(adm_core::hostcap::current(LIMITED_HOST), Some(4), "8 koneksi ditolak => separuh");
}

fn adm_core_sidecar_exists(output: &std::path::Path) -> bool {
    adm_core::sidecar::path_for(output).exists()
}
