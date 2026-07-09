//! Menu YouTube (yt-dlp + ffmpeg). Deteksi biner, dialog opsi (URL + mode +
//! resolusi), lalu jalankan yt-dlp sebagai proses anak — progres di-parse dan
//! di-emit sebagai `EngineEvent` biasa sehingga unduhan tampil sebagai baris
//! ListView normal (progress, Stop, dialog Complete semuanya ter-reuse).
//!
//! Biner dicari: path di Options → folder aplikasi → PATH. Bila salah satu tak
//! ada, menu YouTube di-disable (lihat `available`/`missing_tooltip`).

use crate::engine::{EngineEvent, EngineHandle};
use crate::settings;
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

/// `CREATE_NO_WINDOW` — jalankan yt-dlp tanpa jendela konsol berkedip.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ============================ Job object (anti-yatim) ============================

/// Job object `KILL_ON_JOB_CLOSE` per proses yt-dlp: seluruh pohon proses
/// (yt-dlp + ffmpeg anaknya) mati bersama saat di-`terminate` (Stop) ATAU saat
/// handle tertutup — termasuk saat ADM keluar mendadak. Tanpa ini, `kill()`
/// hanya mematikan yt-dlp (ffmpeg yatim terus menulis & mengunci file), dan
/// keluar dari ADM meninggalkan unduhan yt-dlp berjalan tanpa UI.
pub(crate) struct KillTree(HANDLE);

// HANDLE job object aman dibagikan antar-thread (Terminate/Close thread-safe).
unsafe impl Send for KillTree {}
unsafe impl Sync for KillTree {}

impl KillTree {
    pub(crate) fn new() -> Option<Self> {
        use windows::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        unsafe {
            let job = CreateJobObjectW(None, PCWSTR::null()).ok()?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let _ = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            Some(KillTree(job))
        }
    }

    /// Masukkan proses anak ke job (anak-anaknya otomatis ikut).
    pub(crate) fn assign(&self, child: &std::process::Child) {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::System::JobObjects::AssignProcessToJobObject;
        unsafe {
            let _ = AssignProcessToJobObject(self.0, HANDLE(child.as_raw_handle()));
        }
    }

    /// Bunuh seluruh pohon proses dalam job.
    pub(crate) fn terminate(&self) {
        use windows::Win32::System::JobObjects::TerminateJobObject;
        unsafe {
            let _ = TerminateJobObject(self.0, 1);
        }
    }
}

impl Drop for KillTree {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Jalankan `cmd` sampai selesai dengan proses terdaftar di job object —
/// pengganti `Command::output()` agar probe/fetch tak yatim saat ADM keluar.
fn output_with_job(mut cmd: Command) -> std::io::Result<std::process::Output> {
    let child = cmd.spawn()?;
    let tree = KillTree::new();
    if let Some(t) = &tree {
        t.assign(&child);
    }
    child.wait_with_output()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Video + audio (digabung ffmpeg).
    Video,
    /// Audio saja (diekstrak ke mp3).
    AudioOnly,
    /// Video tanpa audio.
    VideoOnly,
    /// Seluruh playlist / channel.
    Playlist,
}

/// Permintaan unduhan YouTube dari dialog.
#[derive(Clone)]
pub struct YtRequest {
    pub url: String,
    pub mode: Mode,
    /// Batas tinggi video (px); `None` = kualitas terbaik. Tak relevan utk audio.
    pub height: Option<u32>,
}

// ============================ Deteksi biner ============================

fn app_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(|p| p.to_path_buf())
}

/// Cari `exe` di setiap direktori pada PATH.
fn find_in_path(exe: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(exe))
        .find(|p| p.is_file())
}

/// Resolusi biner: override Options (bisa berupa file exe atau folder-nya) →
/// folder aplikasi → PATH. `None` bila override diset tapi tak valid.
fn resolve(config: Option<String>, exe: &str) -> Option<PathBuf> {
    if let Some(c) = config.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        let p = PathBuf::from(&c);
        if p.is_file() {
            return Some(p);
        }
        let inp = p.join(exe);
        return inp.is_file().then_some(inp);
    }
    app_dir()
        .map(|d| d.join(exe))
        .filter(|p| p.is_file())
        .or_else(|| find_in_path(exe))
}

pub fn ytdlp_path() -> Option<PathBuf> {
    resolve(settings::get().youtube_ytdlp, "yt-dlp.exe")
}

pub fn ffmpeg_path() -> Option<PathBuf> {
    resolve(settings::get().youtube_ffmpeg, "ffmpeg.exe")
}

/// True bila yt-dlp DAN ffmpeg tersedia.
pub fn available() -> bool {
    ytdlp_path().is_some() && ffmpeg_path().is_some()
}

/// Teks tooltip saat menu di-disable (menyebut biner yang hilang).
pub fn missing_tooltip() -> String {
    let mut missing = Vec::new();
    if ytdlp_path().is_none() {
        missing.push("yt-dlp");
    }
    if ffmpeg_path().is_none() {
        missing.push("ffmpeg");
    }
    if missing.is_empty() {
        String::new()
    } else {
        format!(
            "{} tidak ditemukan. Pasang di PATH atau atur lokasinya di Options.",
            missing.join(" & ")
        )
    }
}

// ============================ Penyusunan argumen ============================

/// Set env & flag proses yt-dlp: sisipkan folder biner ke PATH (agar `deno`
/// sebagai JS runtime — mencegah throttling YouTube — serta ffmpeg/ffprobe
/// ditemukan) dan jalankan tanpa jendela konsol.
fn tool_env(cmd: &mut Command, ytdlp: &std::path::Path) {
    if let Some(dir) = ytdlp.parent() {
        let mut p = dir.as_os_str().to_os_string();
        if let Some(existing) = std::env::var_os("PATH") {
            p.push(";");
            p.push(existing);
        }
        cmd.env("PATH", p);
    }
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// Filter tinggi `[height<=H]` bila diminta, selain itu kosong.
fn height_filter(h: Option<u32>) -> String {
    h.map(|v| format!("[height<={v}]")).unwrap_or_default()
}

/// Argumen pemilihan format sesuai mode (tanpa URL/output/progress).
fn format_args(req: &YtRequest) -> Vec<String> {
    let hf = height_filter(req.height);
    match req.mode {
        Mode::AudioOnly => vec![
            "-x".into(),
            "--audio-format".into(),
            "mp3".into(),
            "--audio-quality".into(),
            "0".into(),
        ],
        Mode::VideoOnly => vec!["-f".into(), format!("bv{hf}/bv*{hf}")],
        Mode::Video | Mode::Playlist => vec![
            "-f".into(),
            format!("bv*{hf}+ba/b{hf}"),
            "--merge-output-format".into(),
            "mp4".into(),
        ],
    }
}

/// Argumen bersama (format + template output + playlist flag).
fn common_args(req: &YtRequest, dir: &std::path::Path) -> Vec<String> {
    let mut a = format_args(req);
    a.push("-o".into());
    // yt-dlp menafsirkan `%` di path sebagai field template → escape jadi `%%`
    // (folder unduhan seperti `D:\100%stuff` merusak template tanpa ini).
    let dir_esc = dir.to_string_lossy().replace('%', "%%");
    let dir_esc = dir_esc.trim_end_matches(['\\', '/']);
    a.push(format!("{dir_esc}\\%(title)s.%(ext)s"));
    a.push(if req.mode == Mode::Playlist {
        "--yes-playlist".into()
    } else {
        "--no-playlist".into()
    });
    a
}

// ============================ Menjalankan ============================

/// Nama placeholder awal (sebelum yt-dlp melapor judul asli).
fn hint_name(mode: Mode) -> String {
    match mode {
        Mode::Playlist => "YouTube playlist".into(),
        Mode::AudioOnly => "YouTube audio".into(),
        _ => "YouTube video".into(),
    }
}

/// Tag mode untuk dipersist di store (Resume perlu tahu cara me-restart).
pub fn mode_tag(m: Mode) -> &'static str {
    match m {
        Mode::Video => "video",
        Mode::AudioOnly => "audio",
        Mode::VideoOnly => "video_only",
        Mode::Playlist => "playlist",
    }
}

pub fn mode_from_tag(s: &str) -> Option<Mode> {
    Some(match s {
        "video" => Mode::Video,
        "audio" => Mode::AudioOnly,
        "video_only" => Mode::VideoOnly,
        "playlist" => Mode::Playlist,
        _ => return None,
    })
}

/// Mulai unduhan YouTube; kembalikan id baris (untuk buka dialog progres).
/// Mengembalikan `None` bila biner tak tersedia.
pub fn start(engine: &EngineHandle, req: YtRequest) -> Option<u64> {
    let id = engine.alloc_id();
    start_as(engine, id, req)
}

/// Mulai/lanjutkan unduhan YouTube memakai id baris yang SUDAH ada — dipakai
/// Resume/Redownload agar baris YouTube di-restart lewat yt-dlp, bukan engine
/// HTTP (yang hanya akan mengunduh HTML halaman watch). yt-dlp sendiri
/// meneruskan file `.part` yang ada (perilaku continue default).
pub fn start_as(engine: &EngineHandle, id: u64, req: YtRequest) -> Option<u64> {
    let ytdlp = ytdlp_path()?;
    let ffmpeg = ffmpeg_path()?;
    let dir = engine.download_dir();
    let (token, gen) = engine.register(id);
    // Baris instan. Untuk Resume/Redownload, pertahankan output baris yang
    // sudah ada — placeholder hanya untuk baris baru. Bila probe judul gagal
    // (offline/yt-dlp error), jangan tinggalkan baris menunjuk "Video.mp4"
    // yang tak pernah ada (Open folder/duplikat/hapus-file jadi salah sasaran).
    let existing = crate::store::get(id)
        .map(|r| r.output)
        .filter(|p| p.file_name().is_some());
    engine.emit(EngineEvent::Started {
        id,
        url: req.url.clone(),
        output: existing.unwrap_or_else(|| dir.join(hint_name(req.mode))),
    });
    // Baris sudah ada di store (sink Started sinkron) → tandai sbg YouTube.
    if req.mode != Mode::Playlist {
        crate::store::set_youtube(id, mode_tag(req.mode).to_string(), req.height);
    }
    let eng = engine.clone();
    if std::thread::Builder::new()
        .name(format!("ytdlp-{id}"))
        .spawn(move || run_job(eng, id, gen, token, ytdlp, ffmpeg, dir, req))
        .is_err()
    {
        // Tanpa worker, baris akan macet "Downloading" selamanya.
        if engine.unregister(id, gen) {
            engine.emit(EngineEvent::Failed { id, error: "Gagal memulai thread yt-dlp".into() });
        }
    }
    Some(id)
}

#[allow(clippy::too_many_arguments)]
fn run_job(
    eng: EngineHandle,
    id: u64,
    gen: u64,
    token: adm_core::CancelToken,
    ytdlp: PathBuf,
    ffmpeg: PathBuf,
    dir: PathBuf,
    req: YtRequest,
) {
    // Pra-probe nama berkas final (hanya video tunggal) agar baris menampilkan
    // judul asli lebih awal, sebelum unduhan dimulai.
    let mut known: Option<PathBuf> = None;
    if req.mode != Mode::Playlist && !token.is_cancelled() {
        if let Some(path) = probe_filename(&ytdlp, &req, &dir) {
            eng.emit(EngineEvent::Renamed { id, output: path.clone() });
            known = Some(path);
        }
    }

    let outcome = run_download(&eng, id, &token, &ytdlp, &ffmpeg, &dir, &req, known);
    // Sesi yang sudah digantikan (Resume di-klik lagi) tak boleh emit event
    // terminal — statusnya akan menimpa sesi baru yang sedang jalan.
    if eng.unregister(id, gen) {
        match outcome {
            Ok(Done::Completed { bytes }) => eng.emit(EngineEvent::Completed { id, bytes }),
            Ok(Done::Cancelled { downloaded }) => eng.emit(EngineEvent::Paused { id, downloaded }),
            Ok(Done::Failed { error }) | Err(error) => eng.emit(EngineEvent::Failed { id, error }),
        }
    }
}

/// Jalankan `yt-dlp --print filename --no-download` untuk mengetahui nama berkas
/// final tanpa mengunduh. Kembalikan path pada baris keluaran pertama.
fn probe_filename(ytdlp: &std::path::Path, req: &YtRequest, dir: &std::path::Path) -> Option<PathBuf> {
    let mut cmd = Command::new(ytdlp);
    cmd.args(common_args(req, dir));
    cmd.args(["--print", "filename", "--no-download", "--no-warnings", "--quiet"]);
    cmd.arg("--"); // akhir opsi: URL/ID yang diawali '-' tak boleh diparse sbg flag
    cmd.arg(&req.url);
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    tool_env(&mut cmd, ytdlp);
    let out = output_with_job(cmd).ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(PathBuf::from(line))
}

enum Done {
    Completed { bytes: u64 },
    Cancelled { downloaded: u64 },
    Failed { error: String },
}

/// Susun Command yt-dlp untuk mengunduh `req` ke `dir` (dgn progress-template).
/// CATATAN: JANGAN tambahkan `--print` — itu memaksa mode quiet yang menekan
/// output progress-template. Nama berkas final diambil dari baris log normal
/// (`[Merger]`/`[ExtractAudio]`/`Destination`) via `parse_final_name`.
pub(crate) fn build_dl_command(
    ytdlp: &std::path::Path,
    ffmpeg: &std::path::Path,
    dir: &std::path::Path,
    req: &YtRequest,
) -> Command {
    let mut cmd = Command::new(ytdlp);
    cmd.args(common_args(req, dir));
    cmd.arg("--ffmpeg-location").arg(ffmpeg);
    cmd.args(["--newline", "--no-color", "--no-warnings"]);
    cmd.arg("--progress-template").arg(
        "download:ADMPROG %(progress.downloaded_bytes)s %(progress.total_bytes)s \
         %(progress.total_bytes_estimate)s %(progress.speed)s",
    );
    cmd.arg("--"); // akhir opsi: URL/ID yang diawali '-' tak boleh diparse sbg flag
    cmd.arg(&req.url);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    tool_env(&mut cmd, ytdlp);
    cmd
}

/// Hasil akhir `stream_ytdlp`.
pub(crate) struct Streamed {
    pub last_downloaded: u64,
    pub final_path: Option<PathBuf>,
    pub cancelled: bool,
    pub success: bool,
    pub error: String,
}

/// Jalankan `cmd` (sudah lengkap progress-template), alirkan progres & nama via
/// callback, dan hormati pembatalan. Blocking — panggil di thread khusus.
/// `is_cancelled` dipolling untuk membunuh proses; `on_progress(dl,total,speed)`
/// & `on_name(path)` dipanggil per pembaruan.
pub(crate) fn stream_ytdlp(
    mut cmd: Command,
    is_cancelled: &(dyn Fn() -> bool + Sync),
    on_progress: &mut dyn FnMut(u64, Option<u64>, u64),
    on_name: &mut dyn FnMut(&std::path::Path),
    initial: Option<PathBuf>,
) -> Streamed {
    let spawned = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Streamed {
                last_downloaded: 0,
                final_path: initial,
                cancelled: false,
                success: false,
                error: format!("Gagal menjalankan yt-dlp: {e}"),
            }
        }
    };
    let mut child = spawned;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // Satu job per unduhan: Stop membunuh seluruh pohon (yt-dlp + ffmpeg),
    // dan keluar dari ADM (handle job tertutup) tak meninggalkan proses yatim.
    let tree = KillTree::new();
    if let Some(t) = &tree {
        t.assign(&child);
    }
    let child = Mutex::new(child);

    let done_flag = AtomicBool::new(false);
    let killed = AtomicBool::new(false);
    let err_buf = Mutex::new(String::new());
    let mut last_downloaded = 0u64;
    let mut final_path: Option<PathBuf> = initial;

    // `scope` menjamin semua thread join sebelum fungsi kembali → aman meminjam
    // is_cancelled / callback / child tanpa Arc atau transmute lifetime.
    std::thread::scope(|s| {
        // Pemantau pembatalan: bunuh proses saat is_cancelled() true.
        s.spawn(|| loop {
            if is_cancelled() {
                killed.store(true, Ordering::SeqCst);
                // Bunuh SELURUH pohon via job — kill() saja meninggalkan ffmpeg
                // yatim yang terus mengunci file output.
                if let Some(t) = &tree {
                    t.terminate();
                }
                let _ = child.lock().unwrap().kill();
                break;
            }
            if done_flag.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        });

        // Kuras stderr (simpan baris terakhir untuk pesan error).
        if let Some(stderr) = stderr {
            s.spawn(|| {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let line = line.trim().to_string();
                    if !line.is_empty() {
                        *err_buf.lock().unwrap() = line;
                    }
                }
            });
        }

        // Baca stdout di thread ini.
        if let Some(stdout) = stdout {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(rest) = line.strip_prefix("ADMPROG ") {
                    if let Some((downloaded, total, speed)) = parse_progress(rest) {
                        last_downloaded = downloaded;
                        on_progress(downloaded, total, speed);
                    }
                } else if let Some(path) = parse_final_name(&line) {
                    if final_path.as_deref() != Some(path.as_path()) {
                        on_name(&path);
                        final_path = Some(path);
                    }
                }
            }
        }
        done_flag.store(true, Ordering::SeqCst);
    });

    let status = child.lock().unwrap().wait();
    let cancelled = killed.load(Ordering::SeqCst);
    let success = !cancelled && matches!(status, Ok(s) if s.success());
    let error = if success || cancelled {
        String::new()
    } else {
        let e = err_buf.lock().unwrap().clone();
        if e.is_empty() { "yt-dlp gagal (kode error non-nol).".into() } else { e }
    };
    Streamed { last_downloaded, final_path, cancelled, success, error }
}

#[allow(clippy::too_many_arguments)]
fn run_download(
    eng: &EngineHandle,
    id: u64,
    token: &adm_core::CancelToken,
    ytdlp: &std::path::Path,
    ffmpeg: &std::path::Path,
    dir: &std::path::Path,
    req: &YtRequest,
    initial: Option<PathBuf>,
) -> Result<Done, String> {
    let cmd = build_dl_command(ytdlp, ffmpeg, dir, req);
    let s = stream_ytdlp(
        cmd,
        &|| token.is_cancelled(),
        &mut |downloaded, total, speed| {
            let end = total.unwrap_or(downloaded);
            eng.emit(EngineEvent::Progress {
                id,
                downloaded,
                total,
                speed_bps: speed,
                segments: vec![(0, end, downloaded)],
            });
        },
        &mut |path| eng.emit(EngineEvent::Renamed { id, output: path.to_path_buf() }),
        initial,
    );
    if s.cancelled {
        return Ok(Done::Cancelled { downloaded: s.last_downloaded });
    }
    if s.success {
        let bytes = s
            .final_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(s.last_downloaded);
        Ok(Done::Completed { bytes })
    } else {
        Ok(Done::Failed { error: s.error })
    }
}

/// Ambil path berkas final dari baris log yt-dlp normal. Prioritas hasil
/// pasca-proses (`[Merger]` / `[ExtractAudio]`) di atas `Destination` mentah;
/// nama antara pra-merge (mis. `judul.f396.mp4`) diabaikan.
pub(crate) fn parse_final_name(line: &str) -> Option<PathBuf> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("[Merger] Merging formats into ") {
        let p = rest.trim().trim_matches('"');
        return (!p.is_empty()).then(|| PathBuf::from(p));
    }
    if let Some(rest) = line.strip_prefix("[ExtractAudio] Destination: ") {
        let p = rest.trim();
        return (!p.is_empty()).then(|| PathBuf::from(p));
    }
    if let Some(rest) = line.strip_prefix("[download] Destination: ") {
        let p = rest.trim();
        return (!p.is_empty() && !is_intermediate(p)).then(|| PathBuf::from(p));
    }
    if let Some(rest) = line.strip_prefix("[download] ") {
        if let Some(idx) = rest.find(" has already been downloaded") {
            let p = rest[..idx].trim();
            return (!p.is_empty()).then(|| PathBuf::from(p));
        }
    }
    None
}

/// True bila nama berkas adalah stream antara pra-merge (`*.f<digit>.<ext>`).
fn is_intermediate(path: &str) -> bool {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    let parts: Vec<&str> = name.split('.').collect();
    parts.len() >= 3
        && parts[parts.len() - 2]
            .strip_prefix('f')
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// Parse baris "downloaded total estimate speed" (nilai bisa "NA").
/// Kembalikan (downloaded, total, speed_bps).
pub(crate) fn parse_progress(s: &str) -> Option<(u64, Option<u64>, u64)> {
    // Beberapa extractor melaporkan byte sebagai float ("12345.0") — terima
    // int maupun float agar progres tak macet di 0.
    fn num(v: &str) -> Option<u64> {
        v.parse::<u64>().ok().or_else(|| v.parse::<f64>().ok().map(|f| f as u64))
    }
    let mut it = s.split_whitespace();
    let downloaded = num(it.next()?)?;
    let total = it.next().and_then(num);
    let estimate = it.next().and_then(num);
    let speed = it.next().and_then(num).unwrap_or(0);
    Some((downloaded, total.or(estimate).filter(|&t| t > 0), speed))
}

// ============================ Metadata (yt-dlp -J) ============================

#[derive(serde::Deserialize)]
struct YtFmtJson {
    height: Option<u32>,
    vcodec: Option<String>,
    acodec: Option<String>,
    filesize: Option<u64>,
    filesize_approx: Option<u64>,
    tbr: Option<f64>,
}

#[derive(serde::Deserialize)]
struct YtInfoJson {
    title: Option<String>,
    duration: Option<f64>,
    formats: Option<Vec<YtFmtJson>>,
}

/// Format ringkas (sesudah normalisasi) untuk estimasi ukuran.
struct FmtLite {
    height: Option<u32>,
    has_video: bool,
    has_audio: bool,
    size: Option<u64>,
}

/// Metadata video tunggal hasil Check.
pub struct VideoMeta {
    pub title: String,
    formats: Vec<FmtLite>,
}

fn codec_present(c: &Option<String>) -> bool {
    matches!(c, Some(s) if !s.is_empty() && s != "none")
}

/// Ambil metadata video tunggal via `yt-dlp -J` (blocking; panggil di thread).
pub fn fetch_video_meta(ytdlp: &std::path::Path, url: &str) -> Result<VideoMeta, String> {
    let mut cmd = Command::new(ytdlp);
    cmd.args(["-J", "--no-playlist", "--no-warnings"]);
    cmd.arg("--");
    cmd.arg(url);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    tool_env(&mut cmd, ytdlp);
    let out = output_with_job(cmd).map_err(|e| format!("Gagal menjalankan yt-dlp: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("yt-dlp gagal");
        return Err(line.trim().to_string());
    }
    let info: YtInfoJson =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("JSON tak terbaca: {e}"))?;
    let dur = info.duration.unwrap_or(0.0);
    let formats = info
        .formats
        .unwrap_or_default()
        .into_iter()
        .map(|f| {
            let size = f.filesize.or(f.filesize_approx).or_else(|| {
                // Estimasi dari bitrate total: byte ≈ tbr(kbps) * 125 * durasi(s).
                f.tbr.filter(|_| dur > 0.0).map(|t| (t * 125.0 * dur) as u64)
            });
            FmtLite {
                height: f.height,
                has_video: codec_present(&f.vcodec),
                has_audio: codec_present(&f.acodec),
                size,
            }
        })
        .collect();
    Ok(VideoMeta {
        title: info.title.unwrap_or_else(|| "video".into()),
        formats,
    })
}

/// Estimasi ukuran unduhan untuk (mode, batas tinggi). `None` bila tak diketahui.
pub fn size_for(meta: &VideoMeta, mode: Mode, cap: Option<u32>) -> Option<u64> {
    let f = &meta.formats;
    let fits = |h: Option<u32>| cap.is_none_or(|c| h.unwrap_or(0) <= c);
    let audio = f
        .iter()
        .filter(|x| x.has_audio && !x.has_video)
        .filter_map(|x| x.size)
        .max();
    let best_vo = f
        .iter()
        .filter(|x| x.has_video && !x.has_audio && fits(x.height))
        .max_by_key(|x| (x.height.unwrap_or(0), x.size.unwrap_or(0)))
        .and_then(|x| x.size);
    let best_prog = f
        .iter()
        .filter(|x| x.has_video && x.has_audio && fits(x.height))
        .max_by_key(|x| (x.height.unwrap_or(0), x.size.unwrap_or(0)))
        .and_then(|x| x.size);
    match mode {
        Mode::AudioOnly => audio,
        Mode::VideoOnly => best_vo.or(best_prog),
        Mode::Video | Mode::Playlist => match (best_vo, audio) {
            (Some(v), Some(a)) => Some(v + a),
            _ => best_prog,
        },
    }
}

// ---- Playlist (flat) ----

#[derive(serde::Deserialize)]
struct FlatEntry {
    id: Option<String>,
    title: Option<String>,
    url: Option<String>,
    duration: Option<f64>,
}

#[derive(serde::Deserialize)]
struct FlatPlaylist {
    title: Option<String>,
    entries: Option<Vec<FlatEntry>>,
}

/// Satu video dalam playlist/channel.
#[derive(Clone)]
pub struct PlEntry {
    pub url: String,
    pub title: String,
    pub duration: Option<u32>,
}

/// Metadata playlist/channel hasil Check.
pub struct PlaylistMeta {
    pub name: String,
    pub entries: Vec<PlEntry>,
}

/// Ambil daftar video playlist/channel via `yt-dlp --flat-playlist -J` (cepat,
/// 1 permintaan; tanpa probe tiap video). Blocking — panggil di thread.
pub fn fetch_playlist(ytdlp: &std::path::Path, url: &str) -> Result<PlaylistMeta, String> {
    let mut cmd = Command::new(ytdlp);
    cmd.args(["-J", "--flat-playlist", "--no-warnings"]);
    cmd.arg("--");
    cmd.arg(url);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    tool_env(&mut cmd, ytdlp);
    let out = output_with_job(cmd).map_err(|e| format!("Gagal menjalankan yt-dlp: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("yt-dlp gagal");
        return Err(line.trim().to_string());
    }
    let pl: FlatPlaylist =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("JSON tak terbaca: {e}"))?;
    let entries = pl
        .entries
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| {
            let url = match e.url {
                Some(u) if u.starts_with("http") => u,
                _ => format!("https://www.youtube.com/watch?v={}", e.id.as_deref()?),
            };
            Some(PlEntry {
                url,
                title: e.title.unwrap_or_else(|| "video".into()),
                duration: e.duration.map(|d| d as u32),
            })
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return Err("Tidak ada video pada tautan ini.".into());
    }
    Ok(PlaylistMeta {
        name: pl.title.unwrap_or_else(|| "playlist".into()),
        entries,
    })
}

/// Format durasi detik → "m:ss" atau "h:mm:ss".
pub fn fmt_duration(secs: u32) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Format ukuran singkat (mis. "45.2 MB").
pub fn fmt_mb(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GB", b / (1024.0 * 1024.0 * 1024.0))
    } else if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else {
        format!("{:.0} KB", b / 1024.0)
    }
}

// ============================ Dialog opsi ============================

const CLASS: PCWSTR = w!("AdmYouTubeDialog");
static REGISTERED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);
static SAVED: AtomicBool = AtomicBool::new(false);

const ID_URL: usize = 1;
const ID_MODE: usize = 2;
const ID_RES: usize = 3;
const ID_CHECK: usize = 4;
const ID_OK: usize = 20;
const ID_CANCEL: usize = 21;

// Hasil Check dikirim balik ke dialog (lparam = Box<Result<VideoMeta,String>>).
const WM_YT_META: u32 = WM_APP + 21;

// 0 = URL edit, 1 = Mode combo, 2 = Resolution combo, 3 = Title label, 4 = Size label.
static CTRL: Mutex<[isize; 5]> = Mutex::new([0; 5]);
/// Metadata hasil Check terakhir (dialog modal tunggal → global aman).
static CHECKED: Mutex<Option<VideoMeta>> = Mutex::new(None);

/// (label mode, Mode) — indeks combo = indeks array ini.
const MODES: [(&str, Mode); 3] = [
    ("Video (with audio)", Mode::Video),
    ("Audio only (mp3)", Mode::AudioOnly),
    ("Video only (no audio)", Mode::VideoOnly),
];

/// (label resolusi, tinggi) — indeks combo = indeks array ini.
const RESOLUTIONS: [(&str, Option<u32>); 7] = [
    ("Best available", None),
    ("2160p (4K)", Some(2160)),
    ("1440p (2K)", Some(1440)),
    ("1080p (Full HD)", Some(1080)),
    ("720p (HD)", Some(720)),
    ("480p", Some(480)),
    ("360p", Some(360)),
];

fn set_ctrl(i: usize, h: HWND) {
    CTRL.lock().unwrap()[i] = h.0 as isize;
}
fn ctrl(i: usize) -> HWND {
    HWND(CTRL.lock().unwrap()[i] as *mut core::ffi::c_void)
}

unsafe fn gui_font() -> HGDIOBJ {
    GetStockObject(DEFAULT_GUI_FONT)
}

#[allow(clippy::too_many_arguments)]
unsafe fn mk(parent: HWND, class: PCWSTR, text: PCWSTR, style: WINDOW_STYLE, x: i32, y: i32, w: i32, h: i32, id: usize) -> HWND {
    let instance: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class,
        text,
        style | WS_CHILD | WS_VISIBLE,
        x, y, w, h,
        Some(parent),
        Some(HMENU(id as *mut core::ffi::c_void)),
        Some(instance),
        None,
    )
    .unwrap_or_default();
    SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(gui_font().0 as usize)), Some(LPARAM(1)));
    hwnd
}

fn set_text(h: HWND, s: &str) {
    let hs = HSTRING::from(s);
    unsafe {
        let _ = SetWindowTextW(h, PCWSTR(hs.as_ptr()));
    }
}
unsafe fn get_text(h: HWND) -> String {
    let len = GetWindowTextLengthW(h);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let n = GetWindowTextW(h, &mut buf);
    String::from_utf16_lossy(&buf[..n as usize])
}

unsafe fn combo_add(combo: HWND, s: &str) {
    let hs = HSTRING::from(s);
    SendMessageW(combo, CB_ADDSTRING, Some(WPARAM(0)), Some(LPARAM(hs.as_ptr() as isize)));
}

/// Placeholder abu-abu pada EDIT (hilang sendiri saat diketik). `wparam=1` =
/// tetap terlihat meski field fokus, sampai user mengetik.
pub(crate) fn set_cue(edit: HWND, text: &str) {
    const EM_SETCUEBANNER: u32 = 0x1501;
    let h = HSTRING::from(text);
    unsafe {
        SendMessageW(edit, EM_SETCUEBANNER, Some(WPARAM(1)), Some(LPARAM(h.as_ptr() as isize)));
    }
}

/// Resolusi hanya relevan untuk mode video; disable saat "Audio only".
unsafe fn sync_res_enable() {
    let mode_idx = SendMessageW(ctrl(1), CB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0 as usize;
    let is_audio = MODES.get(mode_idx).map(|m| m.1) == Some(Mode::AudioOnly);
    let _ = EnableWindow(ctrl(2), !is_audio);
}

/// Pilihan (mode, tinggi) saat ini dari combo.
unsafe fn current_selection() -> (Mode, Option<u32>) {
    let mi = SendMessageW(ctrl(1), CB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0 as usize;
    let ri = SendMessageW(ctrl(2), CB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0 as usize;
    (
        MODES.get(mi).map(|m| m.1).unwrap_or(Mode::Video),
        RESOLUTIONS.get(ri).and_then(|r| r.1),
    )
}

/// Perbarui label ukuran dari metadata hasil Check + pilihan saat ini.
unsafe fn update_size_label() {
    let guard = CHECKED.lock().unwrap();
    let text = match guard.as_ref() {
        None => "(click Check to estimate)".to_string(),
        Some(meta) => {
            let (mode, cap) = current_selection();
            match size_for(meta, mode, cap) {
                Some(b) => format!("~ {}", fmt_mb(b)),
                None => "size unknown".to_string(),
            }
        }
    };
    set_text(ctrl(4), &text);
}

/// Tampilkan dialog opsi YouTube; `initial` = mode terpilih dari item menu.
/// Kembalikan permintaan bila user menekan Download.
pub fn show_dialog(parent: HWND, initial: Mode) -> Option<YtRequest> {
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None).ok()?.into();
        if !REGISTERED.swap(true, Ordering::SeqCst) {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(proc_),
                hInstance: instance,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as *mut core::ffi::c_void),
                lpszClassName: CLASS,
                ..Default::default()
            };
            RegisterClassW(&wc);
        }
        DONE.store(false, Ordering::SeqCst);
        SAVED.store(false, Ordering::SeqCst);
        *CHECKED.lock().unwrap() = None;

        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        let mut rc = RECT { left: 0, top: 0, right: 480, bottom: 250 };
        let _ = AdjustWindowRectEx(&mut rc, style, false, WS_EX_DLGMODALFRAME);
        let (dw, dh) = (rc.right - rc.left, rc.bottom - rc.top);
        let mut pr = RECT::default();
        let _ = GetWindowRect(parent, &mut pr);
        let x = (pr.left + ((pr.right - pr.left) - dw) / 2).max(0);
        let y = (pr.top + ((pr.bottom - pr.top) - dh) / 2).max(0);

        let dlg = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            CLASS,
            w!("Download from YouTube"),
            style,
            x, y, dw, dh,
            Some(parent),
            None,
            Some(instance),
            None,
        );
        let Ok(dlg) = dlg else { return None };

        const M: i32 = 20;
        // URL + tombol Check.
        let _ = mk(dlg, w!("STATIC"), w!("Video URL:"), WINDOW_STYLE(0), M, 14, 260, 16, 0);
        let u = mk(dlg, w!("EDIT"), PCWSTR::null(), WINDOW_STYLE(WS_BORDER.0 | WS_TABSTOP.0 | ES_AUTOHSCROLL as u32), M, 34, 360, 24, ID_URL);
        set_ctrl(0, u);
        let _ = mk(dlg, w!("BUTTON"), w!("Check"), WINDOW_STYLE(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), M + 366, 33, 74, 26, ID_CHECK);
        set_cue(u, "Paste a YouTube video URL");

        // Judul (diisi setelah Check).
        let _ = mk(dlg, w!("STATIC"), w!("Title:"), WINDOW_STYLE(0), M, 68, 50, 16, 0);
        let title_lbl = mk(dlg, w!("STATIC"), w!("\u{2014}"), WINDOW_STYLE(0x0000_4000), 70, 68, 390, 16, 0); // SS_ENDELLIPSIS
        set_ctrl(3, title_lbl);

        // Mode.
        let _ = mk(dlg, w!("STATIC"), w!("Download:"), WINDOW_STYLE(0), M, 98, 70, 16, 0);
        let m = mk(dlg, w!("COMBOBOX"), PCWSTR::null(), WINDOW_STYLE(WS_TABSTOP.0 | CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0 | crate::dark::combo_style()), 96, 96, 250, 200, ID_MODE);
        for (label, _) in MODES {
            combo_add(m, label);
        }
        let sel = MODES.iter().position(|x| x.1 == initial).unwrap_or(0);
        SendMessageW(m, CB_SETCURSEL, Some(WPARAM(sel)), Some(LPARAM(0)));
        set_ctrl(1, m);

        // Resolusi.
        let _ = mk(dlg, w!("STATIC"), w!("Resolution:"), WINDOW_STYLE(0), M, 130, 70, 16, 0);
        let r = mk(dlg, w!("COMBOBOX"), PCWSTR::null(), WINDOW_STYLE(WS_TABSTOP.0 | CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0 | crate::dark::combo_style()), 96, 128, 180, 200, ID_RES);
        for (label, _) in RESOLUTIONS {
            combo_add(r, label);
        }
        SendMessageW(r, CB_SETCURSEL, Some(WPARAM(0)), Some(LPARAM(0)));
        set_ctrl(2, r);
        sync_res_enable();

        // Ukuran estimasi (diisi setelah Check).
        let _ = mk(dlg, w!("STATIC"), w!("Size:"), WINDOW_STYLE(0), M, 162, 50, 16, 0);
        let size_lbl = mk(dlg, w!("STATIC"), PCWSTR::null(), WINDOW_STYLE(0), 70, 162, 380, 16, 0);
        set_ctrl(4, size_lbl);
        update_size_label();

        // Tombol.
        let _ = mk(dlg, w!("BUTTON"), w!("Download"), WINDOW_STYLE(WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32), 284, 204, 92, 30, ID_OK);
        let _ = mk(dlg, w!("BUTTON"), w!("Cancel"), WINDOW_STYLE(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), 384, 204, 92, 30, ID_CANCEL);

        crate::dark::apply(dlg);
        let _ = EnableWindow(parent, false);
        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetForegroundWindow(dlg);
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(u));

        let _modal = crate::state::ModalGuard::new();
        let mut msg = MSG::default();
        while !DONE.load(Ordering::SeqCst) {
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                PostQuitMessage(0); // teruskan WM_QUIT ke loop luar, jangan ditelan
                break;
            }
            if !IsDialogMessageW(dlg, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        let _ = EnableWindow(parent, true);
        let _ = SetForegroundWindow(parent);

        let result = if SAVED.load(Ordering::SeqCst) {
            let url = get_text(ctrl(0)).trim().to_string();
            let mode_idx = SendMessageW(ctrl(1), CB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0 as usize;
            let res_idx = SendMessageW(ctrl(2), CB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0 as usize;
            let mode = MODES.get(mode_idx).map(|m| m.1).unwrap_or(Mode::Video);
            let height = RESOLUTIONS.get(res_idx).and_then(|r| r.1);
            (!url.is_empty()).then_some(YtRequest { url, mode, height })
        } else {
            None
        };
        if IsWindow(Some(dlg)).as_bool() {
            let _ = DestroyWindow(dlg);
        }
        result
    }
}

/// Jalankan Check: ambil metadata di thread, kirim balik via `WM_YT_META`.
unsafe fn do_check(hwnd: HWND) {
    let url = get_text(ctrl(0)).trim().to_string();
    if url.is_empty() {
        return;
    }
    let Some(ytdlp) = ytdlp_path() else { return };
    set_text(ctrl(3), "Checking\u{2026}");
    set_text(ctrl(4), "Checking\u{2026}");
    let hwnd_isize = hwnd.0 as isize;
    let _ = std::thread::Builder::new().name("yt-check".into()).spawn(move || {
        let result = fetch_video_meta(&ytdlp, &url);
        let payload = Box::into_raw(Box::new(result));
        let hwnd = HWND(hwnd_isize as *mut core::ffi::c_void);
        let posted = PostMessageW(Some(hwnd), WM_YT_META, WPARAM(0), LPARAM(payload as isize)).is_ok();
        if !posted {
            drop(Box::from_raw(payload));
        }
    });
}

extern "system" fn proc_(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let id = wparam.0 & 0xFFFF;
                let code = (wparam.0 >> 16) & 0xFFFF;
                if code == CBN_SELCHANGE as usize && (id == ID_MODE || id == ID_RES) {
                    if id == ID_MODE {
                        sync_res_enable();
                    }
                    update_size_label();
                    return LRESULT(0);
                }
                match id {
                    ID_CHECK => do_check(hwnd),
                    ID_OK => {
                        SAVED.store(true, Ordering::SeqCst);
                        DONE.store(true, Ordering::SeqCst);
                    }
                    ID_CANCEL => {
                        DONE.store(true, Ordering::SeqCst);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            m if m == WM_YT_META => {
                if lparam.0 != 0 {
                    let result = *Box::from_raw(lparam.0 as *mut Result<VideoMeta, String>);
                    match result {
                        Ok(meta) => {
                            set_text(ctrl(3), &meta.title);
                            *CHECKED.lock().unwrap() = Some(meta);
                            update_size_label();
                        }
                        Err(e) => {
                            set_text(ctrl(3), &format!("Check failed: {e}"));
                            *CHECKED.lock().unwrap() = None;
                            set_text(ctrl(4), "size unknown");
                        }
                    }
                }
                LRESULT(0)
            }
            WM_DRAWITEM => {
                if let Some(r) = crate::dark::draw_combobox(lparam) {
                    return r;
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORBTN | WM_CTLCOLORLISTBOX => {
                if let Some(r) = crate::dark::ctlcolor(msg, wparam) {
                    return r;
                }
                SetBkMode(HDC(wparam.0 as *mut _), TRANSPARENT);
                LRESULT(GetSysColorBrush(COLOR_BTNFACE).0 as isize)
            }
            WM_ERASEBKGND => {
                if let Some(r) = crate::dark::erasebkgnd(hwnd, wparam) {
                    return r;
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_CLOSE => {
                DONE.store(true, Ordering::SeqCst);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
