//! Orkestrator unduhan: segmentasi multi-koneksi, positioned write, resume,
//! limiter, progres/ETA (plan §7).

use crate::error::{Error, Result};
use crate::limiter::Limiter;
use crate::sidecar::{self, SegRecord, Sidecar};
use crate::{hostcap, platform, probe};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, COOKIE, RANGE, REFERER};
use reqwest::Client;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Token pembatalan (pause/stop). Shared antar task.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<CancelInner>);

#[derive(Default)]
struct CancelInner {
    flag: AtomicBool,
    /// Membangunkan penunggu seketika. Tanpa ini, Stop baru terasa saat chunk
    /// berikutnya tiba — pada koneksi yang sudah mati itu berarti menunggu
    /// sampai `READ_TIMEOUT` (30 detik) sebelum baris berhenti.
    notify: tokio::sync::Notify,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.flag.load(Ordering::SeqCst)
    }
    /// Selesai begitu token dibatalkan (langsung bila sudah dibatalkan).
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            // Daftar DULU, baru periksa flag: dengan urutan sebaliknya, cancel
            // yang jatuh di antara periksa dan daftar akan terlewat dan
            // penunggu menggantung selamanya.
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// Permintaan unduhan.
#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub url: String,
    pub output: PathBuf,
    /// jumlah koneksi yang diinginkan (di-clamp ke [1, 64]).
    pub connections: usize,
    /// Abaikan verifikasi sertifikat TLS (server bersertifikat invalid).
    pub insecure: bool,
    /// Header titipan browser (untuk unduhan ber-autentikasi seperti Gmail).
    pub referrer: Option<String>,
    pub user_agent: Option<String>,
    pub cookies: Option<String>,
}

/// Snapshot progres untuk callback.
/// Progres satu segmen/koneksi (untuk bar segmen di GUI, §9.11).
#[derive(Debug, Clone, Copy)]
pub struct SegmentProgress {
    pub start: u64,
    /// inklusif.
    pub end: u64,
    pub downloaded: u64,
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub downloaded: u64,
    pub total: Option<u64>,
    /// kecepatan sesaat (byte/detik).
    pub speed_bps: u64,
    /// estimasi sisa waktu (detik); `None` bila tak terhitung.
    pub eta_secs: Option<u64>,
    pub connections: usize,
    /// snapshot progres per segmen (kosong untuk unduhan satu-koneksi non-resumable).
    pub segments: Vec<SegmentProgress>,
}

/// Hasil akhir.
#[derive(Debug, Clone)]
pub enum Outcome {
    Completed { bytes: u64 },
    Paused { downloaded: u64, total: Option<u64> },
}

pub type ProgressCb = Arc<dyn Fn(Progress) + Send + Sync>;

struct SegState {
    start: u64,
    end: u64, // inklusif
    downloaded: AtomicU64,
}

impl SegState {
    fn len(&self) -> u64 {
        self.end - self.start + 1
    }
    fn is_done(&self) -> bool {
        self.downloaded.load(Ordering::Relaxed) >= self.len()
    }
}

/// Header titipan browser untuk unduhan ber-autentikasi (referrer/UA/cookie).
#[derive(Default, Clone)]
pub struct ReqHeaders {
    pub referrer: Option<String>,
    pub user_agent: Option<String>,
    pub cookies: Option<String>,
}

/// Batas percobaan satu segmen sebelum unduhan dinyatakan gagal. Gangguan
/// sesaat (koneksi di-reset host, TLS putus, body terpotong) adalah penyebab
/// kegagalan paling umum pada berkas besar atau host ber-rate-limit; tanpa
/// retry satu hiccup di salah satu koneksi menggagalkan SELURUH unduhan
/// padahal link-nya masih sehat (persis kasus "Resume langsung jalan lagi").
const MAX_ATTEMPTS: u32 = 5;
/// Percobaan jalur satu-koneksi non-resumable — tiap ulangan mulai dari nol,
/// jadi sengaja lebih sedikit daripada segmen yang bisa melanjutkan.
const SINGLE_ATTEMPTS: u32 = 3;
/// Percobaan probe awal (deteksi ukuran & dukungan Range).
const PROBE_ATTEMPTS: u32 = 3;
/// Berapa banyak retry dalam SATU unduhan yang dianggap "host ini menolak
/// koneksi paralel sebanyak itu" — batas koneksi host lalu diturunkan.
const RETRY_PENALTY_THRESHOLD: u32 = 3;
/// Jeda dasar backoff eksponensial: 2s, 4s, 8s, 16s.
const RETRY_BASE_DELAY: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Status HTTP yang pantas dicoba ulang (sibuk/sementara), bukan penolakan
/// permanen. 401/403/404/410 sengaja TIDAK di sini: itu link yang benar-benar
/// kedaluwarsa — user harus cepat diarahkan ke Refresh Link, bukan menunggu
/// lima percobaan sia-sia.
fn retryable_status(code: u16) -> bool {
    matches!(code, 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

/// Rantai pesan error (termasuk seluruh `source()`) dalam huruf kecil.
fn chain_lower(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(x) = src {
        s.push(' ');
        s.push_str(&x.to_string());
        src = x.source();
    }
    s.to_ascii_lowercase()
}

/// Masalah sertifikat: mengulanginya lima kali tidak akan membuat sertifikat
/// jadi sah — hanya menunda dialog "terima risiko" setengah menit. Perlakukan
/// sebagai permanen agar user langsung ditawari pilihan.
fn is_cert_error(e: &Error) -> bool {
    let m = chain_lower(e);
    m.contains("certificate")
        || m.contains("unknownissuer")
        || m.contains("notvalidfor")
        || m.contains("certexpired")
        || m.contains("badsignature")
}

/// Apakah error ini gangguan sesaat yang layak dicoba ulang.
fn is_transient(e: &Error) -> bool {
    if is_cert_error(e) {
        return false;
    }
    match e {
        // Error reqwest tanpa status = level transport (timeout, connect
        // refused, koneksi di-reset, body/decode terputus) — hampir selalu
        // sesaat. Dengan status → ikuti tabel di atas.
        Error::Http(re) => re.status().map(|s| retryable_status(s.as_u16())).unwrap_or(true),
        Error::BadStatus(code) => retryable_status(*code),
        Error::Truncated { .. } => true,
        Error::RangeIgnored(_)
        | Error::UnknownSize
        | Error::Io(_)
        | Error::Json(_)
        | Error::Other(_) => false,
    }
}

/// Server menolak koneksi ini (401/403) — di probe berarti link mati, tapi di
/// tengah unduhan multi-koneksi sering berarti "koneksi paralel kebanyakan".
fn is_rejection(e: &Error) -> bool {
    let code = match e {
        Error::Http(re) => re.status().map(|s| s.as_u16()),
        Error::BadStatus(c) => Some(*c),
        _ => None,
    };
    matches!(code, Some(401 | 403))
}

/// Koordinasi antar segmen satu unduhan: berapa yang sedang memegang koneksi,
/// dan sinyal tiap kali salah satunya keluar (selesai atau gagal).
///
/// Host seperti pixeldrain menjawab 403 — bukan 429 — untuk koneksi paralel
/// yang melebihi jatah per-IP, padahal link-nya sehat (probe barusan lolos,
/// segmen lain masih mengalir). Menganggapnya "link kedaluwarsa" menggagalkan
/// seluruh unduhan; Resume lalu langsung tuntas karena tinggal satu-dua segmen
/// yang tersisa. Segmen yang ditolak sebaiknya mengalah: tunggu sampai
/// koneksi lain lepas, lalu coba lagi.
struct Crew {
    active: AtomicUsize,
    exited: tokio::sync::Notify,
}

impl Crew {
    fn new(n: usize) -> Self {
        Self {
            active: AtomicUsize::new(n),
            exited: tokio::sync::Notify::new(),
        }
    }

    /// Segmen ini selesai/gagal — bangunkan yang sedang mengalah.
    fn leave(&self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.exited.notify_waiters();
    }

    /// Mengalah setelah ditolak: lepas slot, tunggu segmen lain keluar, ambil
    /// slot lagi. `false` bila tak ada segmen lain yang aktif — penolakan itu
    /// berarti link memang ditolak, bukan soal jatah koneksi.
    async fn yield_slot(&self, cancel: &CancelToken) -> bool {
        let exited = self.exited.notified();
        tokio::pin!(exited);
        // Daftar sebagai penunggu SEBELUM melepas slot, agar keluarnya segmen
        // lain di antara dua langkah ini tidak terlewat.
        exited.as_mut().enable();
        if self.active.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.active.fetch_add(1, Ordering::SeqCst);
            return false;
        }
        tokio::select! {
            _ = exited => {}
            _ = cancel.cancelled() => {}
        }
        self.active.fetch_add(1, Ordering::SeqCst);
        true
    }
}

/// Tidur `d`, tapi bangun seketika bila user menekan Pause/Stop selama backoff.
async fn sleep_cancellable(d: Duration, cancel: &CancelToken) {
    tokio::select! {
        _ = tokio::time::sleep(d) => {}
        _ = cancel.cancelled() => {}
    }
}

/// Probe dengan retry — kegagalan pertama sering cuma hiccup TCP/TLS, dan
/// kalau ia lolos ke pemanggil unduhan gagal sebelum satu byte pun terkirim.
async fn probe_with_retry(client: &Client, url: &str, cancel: &CancelToken) -> Result<probe::Probe> {
    let mut attempt = 1u32;
    loop {
        match probe::probe(client, url).await {
            Ok(p) => return Ok(p),
            Err(e) => {
                if cancel.is_cancelled() || attempt >= PROBE_ATTEMPTS || !is_transient(&e) {
                    return Err(e);
                }
                sleep_cancellable(RETRY_BASE_DELAY * attempt, cancel).await;
                attempt += 1;
            }
        }
    }
}

fn build_client(insecure: bool, h: &ReqHeaders) -> Result<Client> {
    let ua = h
        .user_agent
        .clone()
        .unwrap_or_else(|| concat!("ADM/", env!("CARGO_PKG_VERSION")).to_string());
    let mut b = Client::builder()
        .user_agent(ua)
        // Jangan simpan koneksi idle (probe singkat tak meninggalkan keep-alive
        // yang menggantung; tiap segmen pakai koneksi sendiri).
        .pool_max_idle_per_host(0)
        // Tanpa timeout (default reqwest) koneksi yang "mati diam" — host
        // berhenti mengirim tanpa menutup socket — menggantung selamanya.
        // Dengan timeout ia jadi error transien yang langsung dicoba ulang.
        // Catatan: read_timeout hanya berjalan saat body sedang di-poll, jadi
        // jeda akibat limiter kecepatan tidak ikut terhitung.
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT);
    let mut headers = HeaderMap::new();
    if let Some(r) = &h.referrer {
        if let Ok(v) = HeaderValue::from_str(r) {
            headers.insert(REFERER, v);
        }
    }
    if let Some(c) = &h.cookies {
        if let Ok(v) = HeaderValue::from_str(c) {
            headers.insert(COOKIE, v);
        }
    }
    if !headers.is_empty() {
        b = b.default_headers(headers);
    }
    if insecure {
        // User memilih "terima risiko": jangan verifikasi sertifikat TLS.
        b = b.danger_accept_invalid_certs(true);
    }
    Ok(b.build()?)
}

/// Verifikasi `len` byte pertama di `output` cocok dengan byte awal dari `url`.
/// `Some(true)` cocok, `Some(false)` JELAS berbeda, `None` tak bisa diperiksa
/// (jaringan gagal) — pemanggil hanya membuang parsial bila `Some(false)`.
async fn verify_prefix(client: &Client, url: &str, output: &std::path::Path, len: u64) -> Option<bool> {
    use std::io::Read;
    let range = format!("bytes=0-{}", len - 1);
    let resp = client.get(url).header(RANGE, range).send().await.ok()?.error_for_status().ok()?;
    // Baca streaming maksimal `len` byte: server yang mengabaikan Range membalas
    // 200 dengan body PENUH — jangan tarik file multi-GB ke RAM.
    let mut body: Vec<u8> = Vec::with_capacity(len as usize);
    let mut stream = resp.bytes_stream();
    while (body.len() as u64) < len {
        let Some(item) = stream.next().await else { break };
        let chunk = item.ok()?;
        let room = len as usize - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
    let n = (len as usize).min(body.len());
    if n == 0 {
        return None;
    }
    let mut disk = vec![0u8; n];
    let mut f = std::fs::File::open(output).ok()?;
    f.read_exact(&mut disk).ok()?;
    if disk.as_slice() != &body[..n] {
        return Some(false); // jelas beda — meski perbandingan parsial
    }
    if n < len as usize {
        return None; // cocok tapi datanya kurang: belum bisa dipastikan
    }
    Some(true)
}

/// Probe ringan satu URL (client default tanpa header titipan).
pub async fn probe_url(url: &str) -> Result<probe::Probe> {
    probe_url_with(url, &ReqHeaders::default(), false).await
}

/// Probe dengan header titipan browser (referrer/UA/cookie) + opsi insecure —
/// agar unduhan ber-autentikasi (Gmail dll) memberi ukuran & Content-Disposition.
pub async fn probe_url_with(url: &str, h: &ReqHeaders, insecure: bool) -> Result<probe::Probe> {
    let client = build_client(insecure, h)?;
    probe::probe(&client, url).await
}

/// Unduh isi sebuah halaman sebagai teks (dipakai site grabber).
pub async fn fetch_text(url: &str) -> Result<String> {
    let client = build_client(false, &ReqHeaders::default())?;
    let resp = client.get(url).send().await?.error_for_status()?;
    Ok(resp.text().await?)
}

/// Jalankan unduhan (resume otomatis bila sidecar cocok). Blokir sampai
/// selesai, paused (cancel), atau error.
pub async fn download(
    req: DownloadRequest,
    cancel: CancelToken,
    on_progress: Option<ProgressCb>,
    per_limiter: Arc<Limiter>,
    global_limiter: Arc<Limiter>,
) -> Result<Outcome> {
    let headers = ReqHeaders {
        referrer: req.referrer.clone(),
        user_agent: req.user_agent.clone(),
        cookies: req.cookies.clone(),
    };
    let client = build_client(req.insecure, &headers)?;
    let pr = probe_with_retry(&client, &req.url, &cancel).await?;
    sidecar::migrate_legacy(&req.output); // lokasi lama `<file>.adm` → folder state
    let sidecar_path = sidecar::path_for(&req.output);

    // Jalur non-resumable: ukuran tak diketahui atau Range tak didukung.
    if !pr.resumable {
        // Sidecar lama (dari percobaan resumable sebelumnya) jadi basi begitu
        // file ditulis ulang dari nol — buang agar resume berikutnya tak korup.
        sidecar::remove(&sidecar_path);
        return download_single(&client, &req, cancel, on_progress, pr.total, per_limiter, global_limiter).await;
    }

    let total = pr.total.ok_or(Error::UnknownSize)?;
    let wanted = req.connections.clamp(1, 64);
    // Host yang sebelumnya memutus koneksi berlebih dipakai dengan segmen lebih
    // sedikit (lihat modul `hostcap`); setelan user tetap jadi batas atas.
    let host = hostcap::host_of(&req.url);
    let conns = match &host {
        Some(h) => hostcap::effective(h, wanted),
        None => wanted,
    };

    // Resume bila sidecar cocok; selain itu rencana segar. Sidecar hanya
    // dipercaya bila (a) file output masih ada dengan ukuran pre-alokasi penuh
    // — state kini terpisah dari file, jadi file bisa dihapus user tanpa state
    // ikut hilang; resume buta = file penuh nol dilaporkan Completed — dan
    // (b) rekaman segmennya utuh menutup [0, total) tanpa celah/tumpang-tindih.
    let output_len = std::fs::metadata(&req.output).map(|m| m.len()).unwrap_or(0);
    let loaded = sidecar::load(&sidecar_path);
    let reuse = output_len == total
        && loaded
            .as_ref()
            .is_some_and(|sc| sc.is_compatible(&req.url, &pr) && sc.segments_valid(total));
    let url_changed = loaded.as_ref().is_some_and(|sc| sc.url != req.url);
    let mut segments: Vec<Arc<SegState>> = if reuse {
        loaded
            .as_ref()
            .unwrap()
            .segments
            .iter()
            .map(|r| {
                Arc::new(SegState {
                    start: r.start,
                    end: r.end,
                    // saturating: sidecar bisa rusak (end<start) — jangan panik.
                    downloaded: AtomicU64::new(r.downloaded.min(r.end.saturating_sub(r.start) + 1)),
                })
            })
            .collect()
    } else {
        plan_segments(total, conns)
    };

    // Refresh Link (URL berubah, cocok-by-size): verifikasi byte awal di disk
    // cocok dengan sumber baru. Bila JELAS berbeda → buang parsial & unduh ulang
    // dari awal (cegah korup saat link baru menunjuk file beda berukuran sama).
    if reuse && url_changed {
        let prefix = segments
            .iter()
            .find(|s| s.start == 0)
            .map(|s| s.downloaded.load(Ordering::Relaxed))
            .unwrap_or(0);
        let check = prefix.min(64 * 1024);
        if check > 0 && verify_prefix(&client, &req.url, &req.output, check).await == Some(false) {
            segments = plan_segments(total, conns);
        }
    }

    platform::preallocate(&req.output, total)?;

    let downloaded0: u64 = segments
        .iter()
        .map(|s| s.downloaded.load(Ordering::Relaxed))
        .sum();
    let global = Arc::new(AtomicU64::new(downloaded0));

    // Tulis sidecar awal (agar crash sebelum flush pertama tetap resumable).
    write_sidecar(&sidecar_path, &req, &pr, total, &segments);

    // Reporter + flusher sidecar berkala.
    let reporter_stop = Arc::new(AtomicBool::new(false));
    let reporter = spawn_reporter(
        reporter_stop.clone(),
        global.clone(),
        Arc::new(segments.to_vec()),
        sidecar_path.clone(),
        req.clone(),
        pr.clone(),
        total,
        on_progress.clone(),
    );

    // Task per segmen. `retries` menghitung berapa kali koneksi harus diulang
    // — sinyal untuk menurunkan/menaikkan batas koneksi host.
    let retries = Arc::new(AtomicU32::new(0));
    let rejected = Arc::new(AtomicU32::new(0));
    let crew = Arc::new(Crew::new(segments.len()));
    let mut handles = Vec::with_capacity(segments.len());
    for seg in &segments {
        let h = tokio::spawn(run_segment(
            client.clone(),
            req.url.clone(),
            seg.clone(),
            req.output.clone(),
            per_limiter.clone(),
            global_limiter.clone(),
            cancel.clone(),
            global.clone(),
            retries.clone(),
            rejected.clone(),
            crew.clone(),
        ));
        handles.push(h);
    }

    let mut first_err: Option<Error> = None;
    for h in handles {
        match h.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                first_err.get_or_insert(e);
            }
            Err(join) => {
                first_err.get_or_insert(Error::Other(format!("task panik: {join}")));
            }
        }
    }

    // Hentikan reporter & flush state terakhir.
    reporter_stop.store(true, Ordering::SeqCst);
    let _ = reporter.await;
    write_sidecar(&sidecar_path, &req, &pr, total, &segments);

    // Umpan balik batas koneksi adaptif: sesi yang dibatalkan user tidak
    // dihitung (retry-nya bukan cerminan perilaku host).
    if let Some(h) = &host {
        if !cancel.is_cancelled() {
            let n = retries.load(Ordering::Relaxed);
            let used = segments.len();
            let fatal_transient = first_err.as_ref().is_some_and(is_transient);
            // Satu penolakan saja sudah bukti jelas batas koneksi host terlampaui.
            let was_rejected = rejected.load(Ordering::Relaxed) > 0;
            if used > 1 && (fatal_transient || was_rejected || n >= RETRY_PENALTY_THRESHOLD) {
                hostcap::penalize(h, used);
            } else if first_err.is_none() && n == 0 {
                hostcap::reward(h, wanted);
            }
        }
    }

    if cancel.is_cancelled() {
        let dl = global.load(Ordering::Relaxed);
        return Ok(Outcome::Paused {
            downloaded: dl,
            total: Some(total),
        });
    }
    if let Some(e) = first_err {
        // Probe bilang Range didukung, tapi saat segmen non-awal diminta server
        // membalas body penuh. Satu-satunya cara aman: unduh sekuensial satu
        // koneksi dari awal (jalur ini menulis ulang berkas dari nol).
        if matches!(e, Error::RangeIgnored(_)) && segments.len() > 1 {
            sidecar::remove(&sidecar_path);
            return download_single(
                &client,
                &req,
                cancel,
                on_progress,
                Some(total),
                per_limiter,
                global_limiter,
            )
            .await;
        }
        return Err(e);
    }

    let complete = segments.iter().all(|s| s.is_done());
    if complete {
        sidecar::remove(&sidecar_path);
        Ok(Outcome::Completed { bytes: total })
    } else {
        // Tidak cancel, tak ada error, tapi belum penuh: anggap paused (resumable).
        Ok(Outcome::Paused {
            downloaded: global.load(Ordering::Relaxed),
            total: Some(total),
        })
    }
}

/// Segmentasi statis: bagi `total` ke `conns` rentang kontigu hampir sama.
fn plan_segments(total: u64, conns: usize) -> Vec<Arc<SegState>> {
    if total == 0 {
        return Vec::new(); // file kosong: tanpa segmen (langsung complete)
    }
    // ≥1 byte per segmen: file lebih kecil dari jumlah koneksi membuat
    // base = 0 dan `start + base - 1` underflow.
    let conns = (conns.max(1) as u64).min(total);
    let base = total / conns;
    let mut segs = Vec::new();
    let mut start = 0u64;
    for i in 0..conns {
        if start >= total {
            break;
        }
        let mut end = start + base - 1;
        if i == conns - 1 {
            end = total - 1; // segmen terakhir menyapu sisa
        }
        segs.push(Arc::new(SegState {
            start,
            end,
            downloaded: AtomicU64::new(0),
        }));
        start = end + 1;
    }
    segs
}

/// Satu segmen, dengan retry. Tiap percobaan menghitung ulang titik mulai dari
/// `seg.downloaded`, jadi byte yang sudah tertulis tidak diunduh ulang — retry
/// di sini setara "Resume otomatis" untuk satu koneksi saja.
#[allow(clippy::too_many_arguments)]
async fn run_segment(
    client: Client,
    url: String,
    seg: Arc<SegState>,
    output: PathBuf,
    per_limiter: Arc<Limiter>,
    global_limiter: Arc<Limiter>,
    cancel: CancelToken,
    global: Arc<AtomicU64>,
    retries: Arc<AtomicU32>,
    rejected: Arc<AtomicU32>,
    crew: Arc<Crew>,
) -> Result<()> {
    let res = run_segment_retrying(
        &client,
        &url,
        &seg,
        &output,
        &per_limiter,
        &global_limiter,
        &cancel,
        &global,
        &retries,
        &rejected,
        &crew,
    )
    .await;
    crew.leave();
    res
}

#[allow(clippy::too_many_arguments)]
async fn run_segment_retrying(
    client: &Client,
    url: &str,
    seg: &SegState,
    output: &std::path::Path,
    per_limiter: &Limiter,
    global_limiter: &Limiter,
    cancel: &CancelToken,
    global: &AtomicU64,
    retries: &AtomicU32,
    rejected: &AtomicU32,
    crew: &Crew,
) -> Result<()> {
    let mut attempt = 1u32;
    while attempt <= MAX_ATTEMPTS {
        match run_segment_once(client, url, seg, output, per_limiter, global_limiter, cancel, global).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                // Pause/Stop bukan kegagalan: jangan retry, jangan lapor error.
                if cancel.is_cancelled() {
                    return Ok(());
                }
                // Ditolak saat segmen lain masih jalan: jatah koneksi host
                // habis. Tunggu giliran — tak memakan jatah percobaan, karena
                // tiap tunggu butuh segmen lain keluar (jumlahnya terbatas).
                if is_rejection(&e) {
                    if !crew.yield_slot(cancel).await {
                        return Err(e);
                    }
                    rejected.fetch_add(1, Ordering::Relaxed);
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    continue;
                }
                if attempt == MAX_ATTEMPTS || !is_transient(&e) {
                    return Err(e);
                }
                retries.fetch_add(1, Ordering::Relaxed);
                sleep_cancellable(RETRY_BASE_DELAY * (1u32 << (attempt - 1)), cancel).await;
                if cancel.is_cancelled() {
                    return Ok(());
                }
                attempt += 1;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_segment_once(
    client: &Client,
    url: &str,
    seg: &SegState,
    output: &std::path::Path,
    per_limiter: &Limiter,
    global_limiter: &Limiter,
    cancel: &CancelToken,
    global: &AtomicU64,
) -> Result<()> {
    let begin = seg.start + seg.downloaded.load(Ordering::Relaxed);
    if begin > seg.end {
        return Ok(()); // sudah selesai
    }

    let range = format!("bytes={}-{}", begin, seg.end);
    let resp = client
        .get(url)
        .header(RANGE, range)
        .send()
        .await?
        .error_for_status()?;

    // Segmen non-awal WAJIB dapat 206 Partial Content. Bila server mengabaikan
    // Range dan membalas 200 (body penuh dari byte 0), menulisnya di offset
    // segmen akan MERUSAK berkas — gagalkan dengan jelas, jangan korup.
    // Pemanggil menangkap error ini dan jatuh ke jalur satu-koneksi.
    if begin > 0 && resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(Error::RangeIgnored(resp.status().as_u16()));
    }

    let file = platform::open_writer(output)?;
    let mut offset = begin;
    let mut stream = resp.bytes_stream();

    loop {
        // `biased`: cancel selalu dicek lebih dulu agar Stop tidak kalah cepat
        // dari data yang masih mengalir deras.
        let item = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(()),
            it = stream.next() => it,
        };
        let Some(item) = item else { break };
        let chunk = item?;
        // Jangan melampaui akhir segmen (server bisa abaikan batas Range).
        let allowed = (seg.end + 1 - offset) as usize;
        let data: &[u8] = if chunk.len() > allowed {
            &chunk[..allowed]
        } else {
            &chunk
        };
        if data.is_empty() {
            break;
        }
        per_limiter.acquire(data.len()).await;
        global_limiter.acquire(data.len()).await;
        platform::write_at(&file, data, offset)?;
        let n = data.len() as u64;
        offset += n;
        seg.downloaded.fetch_add(n, Ordering::Relaxed);
        global.fetch_add(n, Ordering::Relaxed);
        if offset > seg.end {
            break;
        }
    }
    // Stream habis sebelum segmen penuh. Body chunked yang ditutup "rapi" di
    // tengah jalan tidak dianggap error oleh hyper — tanpa cek ini segmen
    // dilaporkan sukses, unduhan berakhir "Stopped" di 90-an persen, dan tidak
    // ada yang mencoba ulang. Laporkan transien agar retry mengambil alih.
    if !cancel.is_cancelled() && offset <= seg.end {
        return Err(Error::Truncated {
            got: offset - seg.start,
            want: seg.len(),
        });
    }
    Ok(())
}

/// Jalur satu-koneksi tanpa resume, dengan retry. Karena server tak mendukung
/// Range, tiap percobaan terpaksa mulai dari nol — tetap jauh lebih baik
/// daripada menyerah pada hiccup pertama.
#[allow(clippy::too_many_arguments)]
async fn download_single(
    client: &Client,
    req: &DownloadRequest,
    cancel: CancelToken,
    on_progress: Option<ProgressCb>,
    total: Option<u64>,
    per_limiter: Arc<Limiter>,
    global_limiter: Arc<Limiter>,
) -> Result<Outcome> {
    for attempt in 1..=SINGLE_ATTEMPTS {
        match download_single_once(
            client,
            req,
            &cancel,
            &on_progress,
            total,
            &per_limiter,
            &global_limiter,
        )
        .await
        {
            Ok(o) => return Ok(o),
            Err(e) => {
                if cancel.is_cancelled() {
                    return Ok(Outcome::Paused { downloaded: 0, total });
                }
                if attempt == SINGLE_ATTEMPTS || !is_transient(&e) {
                    return Err(e);
                }
                sleep_cancellable(RETRY_BASE_DELAY * attempt, &cancel).await;
                if cancel.is_cancelled() {
                    return Ok(Outcome::Paused { downloaded: 0, total });
                }
            }
        }
    }
    Ok(Outcome::Paused { downloaded: 0, total })
}

#[allow(clippy::too_many_arguments)]
async fn download_single_once(
    client: &Client,
    req: &DownloadRequest,
    cancel: &CancelToken,
    on_progress: &Option<ProgressCb>,
    total: Option<u64>,
    per_limiter: &Limiter,
    global_limiter: &Limiter,
) -> Result<Outcome> {
    use std::io::Write;

    let resp = client.get(&req.url).send().await?.error_for_status()?;
    if let Some(parent) = req.output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&req.output)?;

    let mut stream = resp.bytes_stream();
    let mut downloaded = 0u64;

    loop {
        let item = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(Outcome::Paused { downloaded, total }),
            it = stream.next() => it,
        };
        let Some(item) = item else { break };
        let chunk = item?;
        per_limiter.acquire(chunk.len()).await;
        global_limiter.acquire(chunk.len()).await;
        file.write_all(&chunk)?;
        downloaded += chunk.len() as u64;
        if let Some(cb) = on_progress {
            cb(Progress {
                downloaded,
                total,
                speed_bps: 0,
                eta_secs: None,
                connections: 1,
                segments: Vec::new(),
            });
        }
    }
    file.flush()?;
    // Body chunked yang diterminasi rapi sebelum tuntas tidak dianggap error
    // oleh hyper — bandingkan dengan total probe agar file terpotong tidak
    // dilaporkan Completed. (downloaded > total dibiarkan: body ter-decode
    // transfer/content-encoding bisa lebih besar dari Content-Length.)
    if let Some(t) = total {
        if downloaded < t {
            return Err(Error::Truncated {
                got: downloaded,
                want: t,
            });
        }
    }
    Ok(Outcome::Completed { bytes: downloaded })
}

#[allow(clippy::too_many_arguments)]
fn spawn_reporter(
    stop: Arc<AtomicBool>,
    global: Arc<AtomicU64>,
    segments: Arc<Vec<Arc<SegState>>>,
    sidecar_path: PathBuf,
    req: DownloadRequest,
    pr: probe::Probe,
    total: u64,
    on_progress: Option<ProgressCb>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut prev = global.load(Ordering::Relaxed);
        let interval = Duration::from_millis(500);
        loop {
            tokio::time::sleep(interval).await;
            let cur = global.load(Ordering::Relaxed);
            let speed = ((cur.saturating_sub(prev)) as f64 / interval.as_secs_f64()) as u64;
            prev = cur;

            if let Some(cb) = &on_progress {
                let eta = total.saturating_sub(cur).checked_div(speed);
                let segs: Vec<SegmentProgress> = segments
                    .iter()
                    .map(|s| SegmentProgress {
                        start: s.start,
                        end: s.end,
                        downloaded: s.downloaded.load(Ordering::Relaxed),
                    })
                    .collect();
                cb(Progress {
                    downloaded: cur,
                    total: Some(total),
                    speed_bps: speed,
                    eta_secs: eta,
                    connections: segments.len(),
                    segments: segs,
                });
            }

            // Flush sidecar berkala (tahan-crash).
            write_sidecar(&sidecar_path, &req, &pr, total, &segments);

            if stop.load(Ordering::SeqCst) {
                break;
            }
        }
    })
}

fn write_sidecar(
    path: &std::path::Path,
    req: &DownloadRequest,
    pr: &probe::Probe,
    total: u64,
    segments: &[Arc<SegState>],
) {
    let sc = Sidecar {
        url: req.url.clone(),
        total,
        etag: pr.etag.clone(),
        last_modified: pr.last_modified.clone(),
        segments: segments
            .iter()
            .map(|s| SegRecord {
                start: s.start,
                end: s.end,
                downloaded: s.downloaded.load(Ordering::Relaxed),
            })
            .collect(),
    };
    let _ = sidecar::save(path, &sc);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_selesai_seketika_bila_sudah_dibatalkan() {
        let c = CancelToken::new();
        c.cancel();
        tokio::time::timeout(Duration::from_secs(1), c.cancelled())
            .await
            .expect("token yang sudah dibatalkan tak boleh menunggu");
    }

    /// Regresi: `notified()` harus didaftarkan SEBELUM flag diperiksa.
    /// Dengan urutan terbalik, cancel yang jatuh tepat di antara keduanya
    /// hilang dan penunggu menggantung selamanya.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_bangun_dari_task_lain() {
        let c = CancelToken::new();
        let c2 = c.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            c2.cancel();
        });
        tokio::time::timeout(Duration::from_secs(2), c.cancelled())
            .await
            .expect("penunggu harus dibangunkan oleh cancel()");
    }

    /// Stop saat backoff tidak boleh menunggu sisa jeda sampai habis.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backoff_putus_saat_cancel() {
        let c = CancelToken::new();
        let c2 = c.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            c2.cancel();
        });
        let t0 = std::time::Instant::now();
        sleep_cancellable(Duration::from_secs(30), &c).await;
        assert!(t0.elapsed() < Duration::from_secs(2), "backoff harus putus: {:?}", t0.elapsed());
    }

    #[test]
    fn klasifikasi_error() {
        // Sibuk/sementara → diulang.
        assert!(is_transient(&Error::BadStatus(503)));
        assert!(is_transient(&Error::BadStatus(429)));
        assert!(is_transient(&Error::Truncated { got: 1, want: 2 }));
        // Penolakan permanen → jangan buang waktu, user butuh link baru.
        assert!(!is_transient(&Error::BadStatus(403)));
        assert!(!is_transient(&Error::BadStatus(404)));
        assert!(!is_transient(&Error::RangeIgnored(200)));
        assert!(!is_transient(&Error::UnknownSize));
    }

    /// Regresi: retry membuat dialog "sertifikat tidak tepercaya" tertunda
    /// setengah menit. Masalah sertifikat tidak akan sembuh dengan diulang.
    #[test]
    fn error_sertifikat_tidak_diulang() {
        let e = Error::Other("invalid peer certificate: UnknownIssuer".into());
        assert!(is_cert_error(&e));
        assert!(!is_transient(&e));
    }
}
