//! Manajer engine in-process (plan §4) + antrian (§9.4/§10, WM6).
//!
//! Tiap unduhan = task tokio dengan `CancelToken`. Antrian menahan unduhan
//! "Download Later" dan menjalankannya hingga `max` konkuren; saat satu selesai,
//! slot terisi item berikutnya (`pump`).

use crate::category::Category;
use adm_core::{download, CancelToken, DownloadRequest, Limiter, Outcome, Progress, ProgressCb};
use adm_ipc::DownloadAddParams;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;

/// Gabungkan pesan error + seluruh rantai `source()` (mis. agar detail TLS
/// "invalid peer certificate: UnknownIssuer" ikut, bukan hanya pesan dangkal
/// reqwest "error sending request") — dipakai GUI untuk mendeteksi jenis error.
fn error_chain<E: std::error::Error>(e: &E) -> String {
    let mut msg = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        let part = s.to_string();
        if !msg.contains(&part) {
            msg.push_str(": ");
            msg.push_str(&part);
        }
        src = s.source();
    }
    msg
}

#[derive(Debug, Clone)]
pub enum EngineEvent {
    Queued { id: u64, url: String, output: PathBuf },
    Started { id: u64, url: String, output: PathBuf },
    /// Nama berkas dikoreksi (mis. dari Content-Disposition) setelah mulai.
    Renamed { id: u64, output: PathBuf },
    Progress {
        id: u64,
        downloaded: u64,
        total: Option<u64>,
        speed_bps: u64,
        segments: Vec<(u64, u64, u64)>,
    },
    Completed { id: u64, bytes: u64 },
    Paused { id: u64, downloaded: u64 },
    Failed { id: u64, error: String },
}

pub type EventSink = Arc<dyn Fn(EngineEvent) + Send + Sync>;

/// Unduhan aktif: id → (token cancel, limiter per-unduhan, generasi sesi).
/// Generasi membedakan sesi lama vs baru untuk id yang sama: tanpa itu, task
/// lama yang sedang mati (cancel → start ulang) menghapus entri milik sesi
/// baru, membuat Stop/limiter tak mempan pada unduhan yang justru berjalan.
type ActiveMap = HashMap<u64, (CancelToken, Arc<Limiter>, u64)>;

struct QueueState {
    running: bool,
    max: usize,
    pending: VecDeque<(u64, DownloadAddParams)>,
    running_ids: HashSet<u64>,
}

#[derive(Clone)]
pub struct EngineHandle {
    handle: Handle,
    download_dir: Arc<Mutex<PathBuf>>,
    sink: EventSink,
    active: Arc<Mutex<ActiveMap>>,
    next_id: Arc<AtomicU64>,
    /// Penomor generasi sesi unduhan (lihat [`ActiveMap`]).
    next_gen: Arc<AtomicU64>,
    queue: Arc<Mutex<QueueState>>,
    /// Limiter global (dibagi semua unduhan); live-adjustable.
    global_limiter: Arc<Limiter>,
}

impl EngineHandle {
    pub fn new(handle: Handle, download_dir: PathBuf, sink: EventSink) -> Self {
        Self {
            handle,
            download_dir: Arc::new(Mutex::new(download_dir)),
            sink,
            active: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            next_gen: Arc::new(AtomicU64::new(1)),
            queue: Arc::new(Mutex::new(QueueState {
                running: false,
                max: 1,
                pending: VecDeque::new(),
                running_ids: HashSet::new(),
            })),
            global_limiter: Arc::new(Limiter::unlimited()),
        }
    }

    /// Batas kecepatan global (byte/detik; `0` = tanpa batas). Live.
    pub fn set_global_limit(&self, bps: u64) {
        self.global_limiter.set_rate(bps);
    }

    /// Batas kecepatan per-unduhan (byte/detik; `0` = tanpa batas). Live.
    pub fn set_limit(&self, id: u64, bps: u64) {
        if let Some((_, lim, _)) = self.active.lock().unwrap().get(&id) {
            lim.set_rate(bps);
        }
    }

    pub fn download_dir(&self) -> PathBuf {
        self.download_dir.lock().unwrap().clone()
    }

    /// Handle runtime tokio (untuk spawn probe dari UI thread, mis. ukuran file).
    pub fn runtime(&self) -> Handle {
        self.handle.clone()
    }

    pub fn set_download_dir(&self, dir: PathBuf) {
        *self.download_dir.lock().unwrap() = dir;
    }

    pub fn active_count(&self) -> usize {
        self.active.lock().unwrap().len()
    }

    /// Pastikan id berikutnya minimal `min_next` (dipakai setelah memulihkan
    /// daftar unduhan dari disk agar id baru tak bentrok dengan yang dipulihkan).
    pub fn reserve_ids(&self, min_next: u64) {
        self.next_id.fetch_max(min_next, Ordering::SeqCst);
    }

    /// Batas unduhan antrian yang berjalan bersamaan.
    pub fn set_queue_max(&self, max: usize) {
        self.queue.lock().unwrap().max = max.max(1);
        self.pump(); // batas naik saat antrian jalan → isi slot baru sekarang
    }

    pub fn cancel(&self, id: u64) {
        if let Some((t, _, _)) = self.active.lock().unwrap().get(&id) {
            t.cancel();
        }
        // Item yang masih menunggu di antrian ikut dicabut — tanpa ini, item
        // yang dihapus dari daftar "bangkit lagi" saat pump berikutnya.
        let was_pending = {
            let mut q = self.queue.lock().unwrap();
            let before = q.pending.len();
            q.pending.retain(|(i, _)| *i != id);
            q.pending.len() != before
        };
        if was_pending && !self.active.lock().unwrap().contains_key(&id) {
            (self.sink)(EngineEvent::Paused { id, downloaded: 0 });
        }
    }

    pub fn cancel_all(&self) {
        for (t, _, _) in self.active.lock().unwrap().values() {
            t.cancel();
        }
    }

    /// Tambah & mulai segera; kembalikan id.
    pub fn add(&self, params: DownloadAddParams) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.start(id, params, false);
        id
    }

    // ---- Unduhan eksternal (mis. yt-dlp) --------------------------------
    // Engine tak tahu detail proses eksternal; ia hanya menyediakan primitif
    // agar unduhan tsb tampil sebagai baris list biasa & bisa di-Stop lewat
    // jalur cancel yang sama (Stop / Stop All). Runner-nya (youtube.rs)
    // memanggil `emit` untuk mengalirkan event dan `register`/`unregister`
    // untuk mendaftarkan token pembatalan.

    /// Alokasikan id unduhan baru (tanpa memulai apa pun).
    pub fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Emit event engine (dipakai runner eksternal untuk lapor progres/selesai).
    pub fn emit(&self, ev: EngineEvent) {
        (self.sink)(ev);
    }

    /// Daftarkan token pembatalan untuk unduhan eksternal `id`; `cancel(id)` /
    /// `cancel_all` / `stop_queue` akan men-set token ini (runner memantau &
    /// membunuh proses). Kembalikan token untuk dipantau runner.
    pub fn register(&self, id: u64) -> CancelToken {
        let token = CancelToken::new();
        let gen = self.next_gen.fetch_add(1, Ordering::SeqCst);
        self.active
            .lock()
            .unwrap()
            .insert(id, (token.clone(), Arc::new(Limiter::unlimited()), gen));
        token
    }

    /// Lepas pendaftaran unduhan eksternal (dipanggil runner saat selesai/gagal).
    pub fn unregister(&self, id: u64) {
        self.active.lock().unwrap().remove(&id);
    }

    /// Lanjutkan unduhan yang sudah ada (segera). `insecure` mengabaikan
    /// verifikasi sertifikat TLS; header titipan (referrer/UA/cookie) diberikan
    /// pemanggil dari baris store (dipersist), agar resume ber-auth tetap jalan
    /// termasuk setelah restart.
    #[allow(clippy::too_many_arguments)]
    pub fn resume(
        &self,
        id: u64,
        url: String,
        filename: String,
        insecure: bool,
        referrer: Option<String>,
        user_agent: Option<String>,
        cookies: Option<String>,
    ) {
        self.start(
            id,
            DownloadAddParams {
                url,
                filename: Some(filename),
                insecure,
                referrer,
                user_agent,
                cookies,
            },
            false,
        );
    }

    /// Masukkan kembali item Queued yang dipulihkan dari disk ke antrian engine
    /// (memakai id yang sudah ada; baris di store tak dibuat ulang). Tanpa ini,
    /// "Start queue" tak memproses item Queued setelah aplikasi di-restart.
    pub fn requeue(&self, id: u64, params: DownloadAddParams) {
        self.queue.lock().unwrap().pending.push_back((id, params));
        self.pump();
    }

    /// Tambahkan ke antrian ("Download Later"); jalan saat queue running & ada slot.
    pub fn enqueue(&self, params: DownloadAddParams) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let output = self.output_for(&params, id);
        (self.sink)(EngineEvent::Queued { id, url: params.url.clone(), output });
        self.queue.lock().unwrap().pending.push_back((id, params));
        self.pump();
        id
    }

    /// Mulai antrian (Start Queue).
    pub fn start_queue(&self) {
        self.queue.lock().unwrap().running = true;
        self.pump();
    }

    /// Hentikan antrian (Stop Queue): batalkan item antrian yang sedang jalan.
    pub fn stop_queue(&self) {
        let ids: Vec<u64> = {
            let mut q = self.queue.lock().unwrap();
            q.running = false;
            q.running_ids.iter().copied().collect()
        };
        for id in ids {
            self.cancel(id);
        }
    }

    /// Jalankan item pending hingga batas konkuren tercapai.
    fn pump(&self) {
        loop {
            let next = {
                let mut q = self.queue.lock().unwrap();
                if !q.running || q.running_ids.len() >= q.max {
                    break;
                }
                match q.pending.pop_front() {
                    Some((id, params)) => {
                        q.running_ids.insert(id);
                        Some((id, params))
                    }
                    None => None,
                }
            };
            match next {
                Some((id, params)) => self.start(id, params, true),
                None => break,
            }
        }
    }

    fn output_for(&self, params: &DownloadAddParams, id: u64) -> PathBuf {
        let filename = pick_filename(params, id);
        let mut dir = self.download_dir.lock().unwrap().clone();
        if let Some(folder) = Category::from_filename(&filename).folder() {
            dir.push(folder);
        }
        dir.join(filename)
    }

    fn start(&self, id: u64, params: DownloadAddParams, queued: bool) {
        let cancel = CancelToken::new();
        let per_limiter = Arc::new(Limiter::unlimited());
        let gen = self.next_gen.fetch_add(1, Ordering::SeqCst);
        {
            let mut a = self.active.lock().unwrap();
            // Sesi lama untuk id yang sama (mis. Resume diklik saat masih
            // Downloading, atau Redownload): batalkan — jangan biarkan dua
            // sesi menulis file & sidecar yang sama berbarengan.
            if let Some((old, _, _)) = a.insert(id, (cancel.clone(), per_limiter.clone(), gen)) {
                old.cancel();
            }
        }
        // Start manual mencabut item dari antrian pending — tanpa ini, pump
        // berikutnya memulai id yang sama untuk kedua kalinya.
        self.queue.lock().unwrap().pending.retain(|(i, _)| *i != id);

        // Baris instan dengan tebakan nama (agar list & dialog progres langsung
        // ada); dikoreksi setelah resolusi nama (Content-Disposition).
        let guess_output = self.output_for(&params, id);
        (self.sink)(EngineEvent::Started {
            id,
            url: params.url.clone(),
            output: guess_output,
        });

        let prog = self.sink.clone();
        let on_progress: ProgressCb = Arc::new(move |p: Progress| {
            let segments = p.segments.iter().map(|s| (s.start, s.end, s.downloaded)).collect();
            prog(EngineEvent::Progress {
                id,
                downloaded: p.downloaded,
                total: p.total,
                speed_bps: p.speed_bps,
                segments,
            });
        });

        let this = self.clone();
        let global_limiter = self.global_limiter.clone();
        self.handle.spawn(async move {
            // Tentukan nama berkas (Content-Disposition bila nama generik/absen).
            let name = this.resolve_filename(&params, id).await;
            let mut dir = this.download_dir.lock().unwrap().clone();
            if let Some(folder) = Category::from_filename(&name).folder() {
                dir.push(folder);
            }
            let output = dir.join(&name);

            // Koreksi nama baris (placeholder Started sudah diemit sinkron).
            (this.sink)(EngineEvent::Renamed { id, output: output.clone() });

            let req = DownloadRequest {
                url: params.url.clone(),
                output,
                connections: 8,
                insecure: params.insecure,
                referrer: params.referrer.clone(),
                user_agent: params.user_agent.clone(),
                cookies: params.cookies.clone(),
            };
            let res = download(req, cancel, Some(on_progress), per_limiter, global_limiter).await;
            {
                // Hapus entri hanya bila masih milik sesi ini — sesi baru bisa
                // sudah menggantikan entri id yang sama.
                let mut a = this.active.lock().unwrap();
                if a.get(&id).is_some_and(|(_, _, g)| *g == gen) {
                    a.remove(&id);
                }
            }
            // Emit event terminal DULU sebelum memulai item antrian berikutnya.
            let ev = match res {
                Ok(Outcome::Completed { bytes }) => EngineEvent::Completed { id, bytes },
                Ok(Outcome::Paused { downloaded, .. }) => EngineEvent::Paused { id, downloaded },
                Err(e) => EngineEvent::Failed { id, error: error_chain(&e) },
            };
            (this.sink)(ev);
            if queued {
                this.queue.lock().unwrap().running_ids.remove(&id);
                this.pump();
            }
        });
    }

    /// Nama berkas akhir. Prioritas: nama eksplisit non-generik dari pemanggil
    /// (browser/dialog) → `Content-Disposition` server → basename URL → fallback.
    async fn resolve_filename(&self, params: &DownloadAddParams, id: u64) -> String {
        let provided = params
            .filename
            .as_deref()
            .map(sanitize)
            .filter(|s| !s.is_empty());

        if let Some(p) = &provided {
            if !looks_generic(p) {
                return p.clone();
            }
        }
        let headers = adm_core::ReqHeaders {
            referrer: params.referrer.clone(),
            user_agent: params.user_agent.clone(),
            cookies: params.cookies.clone(),
        };
        if let Ok(pr) = adm_core::probe_url_with(&params.url, &headers, params.insecure).await {
            if let Some(cd) = pr.suggested_filename {
                let cd = sanitize(&cd);
                if !cd.is_empty() && !looks_generic(&cd) {
                    return cd;
                }
            }
        }
        provided
            .or_else(|| url_basename(&params.url))
            .unwrap_or_else(|| format!("download-{id}.bin"))
    }
}

pub(crate) fn looks_generic(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "download.bin"
        || n == "download"
        || (n.starts_with("download-") && n.ends_with(".bin"))
        || !n.contains('.') // tanpa ekstensi
}

fn url_basename(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or("");
    path.rsplit('/')
        .next()
        .map(|s| sanitize(&adm_core::percent_decode(s)))
        .filter(|s| !s.is_empty() && s.contains('.'))
}

fn pick_filename(params: &DownloadAddParams, id: u64) -> String {
    if let Some(f) = &params.filename {
        let s = sanitize(f);
        if !s.is_empty() {
            return s;
        }
    }
    let path = params.url.split(['?', '#']).next().unwrap_or("");
    if let Some(seg) = path.rsplit('/').next() {
        let s = sanitize(&adm_core::percent_decode(seg));
        if !s.is_empty() {
            return s;
        }
    }
    format!("download-{id}.bin")
}

pub(crate) fn sanitize(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if "\\/:*?\"<>|".contains(c) || (c as u32) < 0x20 {
                '_'
            } else {
                c
            }
        })
        .collect();
    // Win32 memangkas titik/spasi di akhir nama secara diam-diam saat create —
    // nama di disk jadi beda dengan `row.output` (Open/hapus/cek-duplikat
    // meleset). Pangkas sendiri agar konsisten.
    while s.ends_with('.') || s.ends_with(' ') {
        s.pop();
    }
    // Nama device Windows (CON, PRN, AUX, NUL, COM1-9, LPT1-9) tak boleh jadi
    // stem nama berkas — open bisa gagal/menyasar device.
    let stem = s.split('.').next().unwrap_or("");
    let up = stem.to_ascii_uppercase();
    let reserved = matches!(up.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (up.len() == 4
            && (up.starts_with("COM") || up.starts_with("LPT"))
            && up.as_bytes()[3].is_ascii_digit());
    if reserved {
        s.insert(0, '_');
    }
    s
}
