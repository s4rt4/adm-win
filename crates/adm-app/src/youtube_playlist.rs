//! Playlist/channel YouTube: dialog checklist (pilih video), manajer unduhan
//! per-item (berurutan; tiap video subproses yt-dlp sendiri → bisa Resume/Delete
//! per video), dan jendela progres khusus (1 video 1 baris). Di list utama tampil
//! sebagai SATU baris agregat "Playlist: X (done/total)".

use crate::engine::{EngineEvent, EngineHandle};
use crate::youtube::{self, Mode, PlEntry, YtRequest};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, GetFocus, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::*;

// (label resolusi, tinggi) untuk combo playlist.
const RESOLUTIONS: [(&str, Option<u32>); 7] = [
    ("Best available", None),
    ("2160p (4K)", Some(2160)),
    ("1440p (2K)", Some(1440)),
    ("1080p (Full HD)", Some(1080)),
    ("720p (HD)", Some(720)),
    ("480p", Some(480)),
    ("360p", Some(360)),
];

// ============================ Model & manajer ============================

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum ItemStatus {
    Pending,
    Downloading,
    Done,
    Error,
    Stopped,
    Removed,
}

impl ItemStatus {
    fn label(self) -> &'static str {
        match self {
            ItemStatus::Pending => "Pending",
            ItemStatus::Downloading => "Downloading",
            ItemStatus::Done => "Done",
            ItemStatus::Error => "Error",
            ItemStatus::Stopped => "Stopped",
            ItemStatus::Removed => "Removed",
        }
    }
}

struct Item {
    url: String,
    title: String,
    status: ItemStatus,
    error: String,
    downloaded: u64,
    total: Option<u64>,
    speed: u64,
    output: Option<PathBuf>,
    cancel: adm_core::CancelToken,
}

/// Permintaan unduhan playlist dari dialog.
pub struct PlaylistJob {
    pub name: String,
    pub folder: PathBuf,
    pub height: Option<u32>,
    pub entries: Vec<PlEntry>,
}

struct Manager {
    id: u64,
    name: String,
    folder: PathBuf,
    height: Option<u32>,
    ytdlp: PathBuf,
    ffmpeg: PathBuf,
    engine: EngineHandle,
    items: Mutex<Vec<Item>>,
    /// Jeda (resumable): worker & proses berjalan berhenti; Resume mengosongkan.
    paused: AtomicBool,
    worker_active: AtomicBool,
}

static MANAGERS: Mutex<Vec<Arc<Manager>>> = Mutex::new(Vec::new());

fn manager_of(id: u64) -> Option<Arc<Manager>> {
    MANAGERS.lock().unwrap().iter().find(|m| m.id == id).cloned()
}

/// True bila `id` adalah baris agregat playlist (untuk routing di gui).
pub fn is_playlist(id: u64) -> bool {
    MANAGERS.lock().unwrap().iter().any(|m| m.id == id)
}

/// Jeda unduhan playlist `id` (Stop). Item Pending/Downloading → Stopped
/// (terlihat jelas). Resumable via jendela progres.
pub fn pause(id: u64) {
    if let Some(m) = manager_of(id) {
        m.paused.store(true, Ordering::SeqCst);
        {
            let mut items = m.items.lock().unwrap();
            for it in items.iter_mut() {
                match it.status {
                    ItemStatus::Downloading => {
                        it.cancel.cancel(); // bunuh proses; run_item menandai Stopped
                        it.status = ItemStatus::Stopped;
                        it.speed = 0;
                    }
                    ItemStatus::Pending => it.status = ItemStatus::Stopped,
                    _ => {}
                }
            }
        }
        save_state();
    }
}

/// Jeda semua playlist aktif (dipakai "Stop All").
pub fn pause_all() {
    let ids: Vec<u64> = MANAGERS.lock().unwrap().iter().map(|m| m.id).collect();
    for id in ids {
        pause(id);
    }
}

/// Hentikan & buang manajer (dipanggil saat baris agregat dihapus dari list).
pub fn remove(id: u64) {
    pause(id);
    MANAGERS.lock().unwrap().retain(|m| m.id != id);
    save_state();
}

// ========================= Persistensi state playlist =========================
//
// Manajer playlist hanya hidup di memori (MANAGERS). Tanpa persistensi, setelah
// app ditutup baris agregat di list utama kehilangan identitas playlist-nya:
// `is_playlist` jadi false, sehingga Resume salah rute ke dialog unduhan tunggal
// dan mencoba mengunduh URL palsu "Playlist: X". State ditulis ke
// %APPDATA%\ADM\playlists.json dan dipulihkan saat startup oleh `restore`.

#[derive(Serialize, Deserialize)]
struct SavedItem {
    url: String,
    title: String,
    status: ItemStatus,
    #[serde(default)]
    error: String,
    #[serde(default)]
    downloaded: u64,
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    output: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct SavedPlaylist {
    id: u64,
    name: String,
    folder: PathBuf,
    height: Option<u32>,
    items: Vec<SavedItem>,
}

fn state_file() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("ADM").join("playlists.json")
}

/// Serialisasi penulisan berkas (bisa dipicu worker playlist & UI bersamaan).
static SAVE_LOCK: Mutex<()> = Mutex::new(());

/// Tulis state semua manajer ke disk (atomik: tmp + rename). Dipanggil hanya
/// pada transisi status (mulai/selesai item, pause, resume, delete) — jarang,
/// jadi sinkron aman; progres per-byte sengaja tidak memicu tulis.
///
/// PENTING: jangan panggil selagi memegang `Manager::items` — fungsi ini
/// mengunci MANAGERS lalu items tiap manajer.
fn save_state() {
    let snapshot: Vec<SavedPlaylist> = {
        let mgrs = MANAGERS.lock().unwrap();
        mgrs.iter()
            .map(|m| SavedPlaylist {
                id: m.id,
                name: m.name.clone(),
                folder: m.folder.clone(),
                height: m.height,
                items: m
                    .items
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|it| SavedItem {
                        url: it.url.clone(),
                        title: it.title.clone(),
                        // Proses yt-dlp mati saat app ditutup → item yang sedang
                        // jalan dipersist sebagai Stopped (resumable), bukan
                        // Downloading yang menyesatkan setelah restart.
                        status: if it.status == ItemStatus::Downloading {
                            ItemStatus::Stopped
                        } else {
                            it.status
                        },
                        error: it.error.clone(),
                        downloaded: it.downloaded,
                        total: it.total,
                        output: it.output.clone(),
                    })
                    .collect(),
            })
            .collect()
    };
    let _guard = SAVE_LOCK.lock().unwrap();
    let file = state_file();
    if snapshot.is_empty() {
        let _ = std::fs::remove_file(&file);
        return;
    }
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_vec_pretty(&snapshot) {
        let tmp = file.with_extension("json.tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, &file);
        }
    }
}

/// Pulihkan manajer playlist dari disk saat startup. Baris agregatnya sendiri
/// sudah dimuat oleh `store::load`; ini mengembalikan identitas playlist-nya
/// agar Resume membuka jendela playlist dan melanjutkan video yang belum
/// selesai. Semua manajer dipulihkan dalam keadaan dijeda — user yang memulai.
pub fn restore(engine: &EngineHandle) {
    let Ok(bytes) = std::fs::read(state_file()) else { return };
    let Ok(saved) = serde_json::from_slice::<Vec<SavedPlaylist>>(&bytes) else { return };
    // Biner bisa saja absen (sidecar hilang); manajer tetap dipulihkan supaya
    // routing Resume benar — kegagalan menjalankan yt-dlp tampil per item.
    let ytdlp = youtube::ytdlp_path().unwrap_or_default();
    let ffmpeg = youtube::ffmpeg_path().unwrap_or_default();
    let mut restored = false;
    {
        let mut mgrs = MANAGERS.lock().unwrap();
        for sp in saved {
            // Baris agregat sudah tak ada di daftar (dihapus user) → buang.
            if crate::store::get(sp.id).is_none() || mgrs.iter().any(|m| m.id == sp.id) {
                continue;
            }
            let items: Vec<Item> = sp
                .items
                .into_iter()
                .map(|s| Item {
                    url: s.url,
                    title: s.title,
                    status: if s.status == ItemStatus::Downloading {
                        ItemStatus::Stopped
                    } else {
                        s.status
                    },
                    error: s.error,
                    downloaded: s.downloaded,
                    total: s.total,
                    speed: 0,
                    output: s.output,
                    cancel: adm_core::CancelToken::new(),
                })
                .collect();
            mgrs.push(Arc::new(Manager {
                id: sp.id,
                name: sp.name,
                folder: sp.folder,
                height: sp.height,
                ytdlp: ytdlp.clone(),
                ffmpeg: ffmpeg.clone(),
                engine: engine.clone(),
                items: Mutex::new(items),
                paused: AtomicBool::new(true),
                worker_active: AtomicBool::new(false),
            }));
            restored = true;
        }
    }
    // Buang entri basi (baris yang sudah dihapus user) dari berkas; bila tak ada
    // satu pun yang tersisa, `save_state` menghapus berkasnya.
    save_state();
    if restored {
        // Selaraskan teks "(done/total)" baris agregat dengan item yang ada.
        let mgrs: Vec<Arc<Manager>> = MANAGERS.lock().unwrap().clone();
        for m in &mgrs {
            let (done, total) = {
                let items = m.items.lock().unwrap();
                let active: Vec<&Item> =
                    items.iter().filter(|it| it.status != ItemStatus::Removed).collect();
                (
                    active.iter().filter(|it| it.status == ItemStatus::Done).count(),
                    active.len(),
                )
            };
            crate::store::set_name(m.id, &format!("Playlist: {} ({done}/{total})", m.name));
        }
    }
}

/// Mulai unduhan playlist; kembalikan id baris agregat. `None` bila biner absen.
pub fn start(engine: &EngineHandle, job: PlaylistJob) -> Option<u64> {
    let ytdlp = youtube::ytdlp_path()?;
    let ffmpeg = youtube::ffmpeg_path()?;
    let _ = std::fs::create_dir_all(&job.folder);
    let id = engine.alloc_id();
    let items: Vec<Item> = job
        .entries
        .iter()
        .map(|e| Item {
            url: e.url.clone(),
            title: e.title.clone(),
            status: ItemStatus::Pending,
            error: String::new(),
            downloaded: 0,
            total: None,
            speed: 0,
            output: None,
            cancel: adm_core::CancelToken::new(),
        })
        .collect();
    let total = items.len();
    let mgr = Arc::new(Manager {
        id,
        name: job.name.clone(),
        folder: job.folder.clone(),
        height: job.height,
        ytdlp,
        ffmpeg,
        engine: engine.clone(),
        items: Mutex::new(items),
        paused: AtomicBool::new(false),
        worker_active: AtomicBool::new(false),
    });
    MANAGERS.lock().unwrap().push(mgr.clone());

    // Baris agregat di list utama.
    engine.emit(EngineEvent::Started {
        id,
        url: format!("Playlist: {}", job.name),
        output: job.folder.join(&job.name),
    });
    crate::store::set_name(id, &format!("Playlist: {} (0/{total})", job.name));
    crate::state::post_to_ui(crate::state::WM_PROGRESS);

    save_state();
    ensure_worker(&mgr);
    Some(id)
}

/// Pastikan worker berjalan (idempoten). Loop luar menangani balapan Resume yang
/// menambah Pending tepat saat worker hendak berhenti: flag dilepas, lalu dicek
/// ulang; hanya satu worker aktif pada satu waktu, dan `finalize` sekali di akhir.
fn ensure_worker(mgr: &Arc<Manager>) {
    if mgr.worker_active.swap(true, Ordering::SeqCst) {
        return;
    }
    let mgr = mgr.clone();
    let _ = std::thread::Builder::new()
        .name(format!("pl-{}", mgr.id))
        .spawn(move || {
            loop {
                run_pending(&mgr);
                mgr.worker_active.store(false, Ordering::SeqCst);
                let more = !mgr.paused.load(Ordering::SeqCst)
                    && mgr.items.lock().unwrap().iter().any(|it| it.status == ItemStatus::Pending);
                if !more {
                    break; // benar-benar habis → kita yang finalize
                }
                if mgr.worker_active.swap(true, Ordering::SeqCst) {
                    return; // worker lain sudah mengambil alih → biar dia yang finalize
                }
            }
            finalize(&mgr);
        });
}

/// Proses semua item Pending berurutan hingga habis atau dijeda.
fn run_pending(mgr: &Arc<Manager>) {
    loop {
        if mgr.paused.load(Ordering::SeqCst) {
            break;
        }
        let idx = mgr
            .items
            .lock()
            .unwrap()
            .iter()
            .position(|it| it.status == ItemStatus::Pending);
        let Some(idx) = idx else { break };
        run_item(mgr, idx);
        update_aggregate(mgr);
    }
}

fn run_item(mgr: &Arc<Manager>, idx: usize) {
    // Set Downloading + token segar.
    let (url, cancel) = {
        let mut items = mgr.items.lock().unwrap();
        let it = &mut items[idx];
        if it.status == ItemStatus::Removed {
            return;
        }
        it.status = ItemStatus::Downloading;
        it.cancel = adm_core::CancelToken::new();
        it.downloaded = 0;
        it.total = None;
        it.speed = 0;
        it.error.clear();
        (it.url.clone(), it.cancel.clone())
    };
    update_aggregate(mgr);

    let req = YtRequest { url, mode: Mode::Video, height: mgr.height };
    let cmd = youtube::build_dl_command(&mgr.ytdlp, &mgr.ffmpeg, &mgr.folder, &req);
    let paused = &mgr.paused;
    let cancel_ref = &cancel;
    let s = youtube::stream_ytdlp(
        cmd,
        &|| cancel_ref.is_cancelled() || paused.load(Ordering::SeqCst),
        &mut |dl, total, speed| {
            let mut items = mgr.items.lock().unwrap();
            if let Some(it) = items.get_mut(idx) {
                it.downloaded = dl;
                if total.is_some() {
                    it.total = total;
                }
                it.speed = speed;
            }
        },
        &mut |path| {
            let mut items = mgr.items.lock().unwrap();
            if let Some(it) = items.get_mut(idx) {
                it.output = Some(path.to_path_buf());
            }
        },
        None,
    );

    let mut items = mgr.items.lock().unwrap();
    if let Some(it) = items.get_mut(idx) {
        it.speed = 0;
        if it.status == ItemStatus::Removed {
            // Dihapus di tengah jalan → biarkan.
        } else if s.cancelled {
            // Pause/Delete → Stopped (resumable). Delete sudah menandai Removed.
            // Bila user keburu me-Resume (status sudah Pending lagi) saat proses
            // lama masih dimatikan, JANGAN timpa — item harus tetap diproses.
            if it.status == ItemStatus::Downloading {
                it.status = ItemStatus::Stopped;
            }
        } else if s.success {
            it.status = ItemStatus::Done;
            if let Some(p) = &s.final_path {
                it.output = Some(p.clone());
                if let Ok(m) = std::fs::metadata(p) {
                    it.total = Some(m.len());
                    it.downloaded = m.len();
                }
            }
        } else {
            it.status = ItemStatus::Error;
            it.error = s.error;
        }
    }
    drop(items);
    save_state();
}

/// Ringkas status ke baris agregat list utama.
fn update_aggregate(mgr: &Arc<Manager>) {
    let (done, total, speed) = {
        let items = mgr.items.lock().unwrap();
        let active: Vec<&Item> = items.iter().filter(|it| it.status != ItemStatus::Removed).collect();
        let done = active.iter().filter(|it| it.status == ItemStatus::Done).count();
        let speed = active
            .iter()
            .filter(|it| it.status == ItemStatus::Downloading)
            .map(|it| it.speed)
            .sum();
        (done, active.len(), speed)
    };
    crate::store::set_name(mgr.id, &format!("Playlist: {} ({done}/{total})", mgr.name));
    mgr.engine.emit(EngineEvent::Progress {
        id: mgr.id,
        downloaded: done as u64,
        total: Some(total as u64),
        speed_bps: speed,
        segments: Vec::new(),
    });
}

fn finalize(mgr: &Arc<Manager>) {
    let (all_done, any_error, has_pending, bytes) = {
        let items = mgr.items.lock().unwrap();
        let active: Vec<&Item> = items.iter().filter(|it| it.status != ItemStatus::Removed).collect();
        let all_done = !active.is_empty() && active.iter().all(|it| it.status == ItemStatus::Done);
        let any_error = active.iter().any(|it| it.status == ItemStatus::Error);
        // Item Stopped (user resume sebagian) = masih ada pekerjaan tersisa —
        // tanpa ini baris agregat keliru dilaporkan Completed.
        let has_pending = active
            .iter()
            .any(|it| matches!(it.status, ItemStatus::Pending | ItemStatus::Stopped));
        let bytes = active.iter().filter_map(|it| it.total).sum::<u64>();
        (all_done, any_error, has_pending, bytes)
    };
    if mgr.paused.load(Ordering::SeqCst) || has_pending {
        // Dijeda (masih ada yang belum) → baris "Stopped", resumable.
        let done = mgr.items.lock().unwrap().iter().filter(|it| it.status == ItemStatus::Done).count();
        mgr.engine.emit(EngineEvent::Paused { id: mgr.id, downloaded: done as u64 });
    } else if all_done {
        mgr.engine.emit(EngineEvent::Completed { id: mgr.id, bytes });
    } else if any_error {
        mgr.engine.emit(EngineEvent::Failed {
            id: mgr.id,
            error: "Sebagian video gagal — buka jendela playlist untuk Resume/Delete.".into(),
        });
    } else {
        // Semua Removed → tandai selesai (kosong).
        mgr.engine.emit(EngineEvent::Completed { id: mgr.id, bytes: 0 });
    }
    save_state();
    crate::state::post_to_ui(crate::state::WM_PROGRESS);
}

// ---- Aksi dari jendela progres ----

/// Resume item terpilih (Error/Pending → Pending) lalu jalankan worker.
fn resume_items(mgr: &Arc<Manager>, item_indices: &[usize]) {
    {
        let mut items = mgr.items.lock().unwrap();
        for &i in item_indices {
            if let Some(it) = items.get_mut(i) {
                if matches!(it.status, ItemStatus::Error | ItemStatus::Stopped | ItemStatus::Pending) {
                    it.status = ItemStatus::Pending;
                    it.error.clear();
                }
            }
        }
    }
    mgr.paused.store(false, Ordering::SeqCst);
    save_state();
    ensure_worker(mgr);
}

/// Resume SEMUA item yang belum selesai (dipakai dari list utama).
/// Pending ikut disertakan: setelah restart manajer dipulihkan dalam keadaan
/// dijeda tanpa worker, jadi item Pending pun perlu dibangunkan.
/// Mengembalikan false bila `id` bukan baris playlist.
pub fn resume_all(id: u64) -> bool {
    let Some(mgr) = manager_of(id) else { return false };
    let all: Vec<usize> = mgr
        .items
        .lock()
        .unwrap()
        .iter()
        .enumerate()
        .filter(|(_, it)| {
            matches!(
                it.status,
                ItemStatus::Error | ItemStatus::Stopped | ItemStatus::Pending
            )
        })
        .map(|(i, _)| i)
        .collect();
    resume_items(&mgr, &all);
    true
}

/// Tandai item terpilih Removed (batalkan bila sedang jalan).
fn delete_items(mgr: &Arc<Manager>, item_indices: &[usize]) {
    {
        let mut items = mgr.items.lock().unwrap();
        for &i in item_indices {
            if let Some(it) = items.get_mut(i) {
                it.status = ItemStatus::Removed;
                it.cancel.cancel();
            }
        }
    }
    save_state();
}

// ============================ Dialog checklist ============================

const D_CLASS: PCWSTR = w!("AdmPlaylistDialog");
static D_REG: AtomicBool = AtomicBool::new(false);
static D_DONE: AtomicBool = AtomicBool::new(false);
static D_SAVED: AtomicBool = AtomicBool::new(false);
static D_CTRL: Mutex<[isize; 4]> = Mutex::new([0; 4]); // 0 url, 1 list, 2 save, 3 res
static D_ENTRIES: Mutex<Vec<PlEntry>> = Mutex::new(Vec::new());
static D_BASE: Mutex<String> = Mutex::new(String::new());
static D_NAME: Mutex<String> = Mutex::new(String::new());

const DID_URL: usize = 1;
const DID_CHECK: usize = 2;
const DID_LIST: usize = 3;
const DID_SAVE: usize = 4;
const DID_BROWSE: usize = 5;
const DID_RES: usize = 6;
const DID_ALL: usize = 7;
const DID_NONE: usize = 8;
const DID_OK: usize = 20;
const DID_CANCEL: usize = 21;
const WM_PL_META: u32 = WM_APP + 31;

fn d_ctrl(i: usize) -> HWND {
    HWND(D_CTRL.lock().unwrap()[i] as *mut core::ffi::c_void)
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

unsafe fn lv_add_col(lv: HWND, i: i32, text: &str, cx: i32) {
    let mut wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let mut col = LVCOLUMNW {
        mask: LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM,
        cx,
        pszText: PWSTR(wide.as_mut_ptr()),
        iSubItem: i,
        ..Default::default()
    };
    SendMessageW(lv, LVM_INSERTCOLUMNW, Some(WPARAM(i as usize)), Some(LPARAM(&mut col as *mut _ as isize)));
}

unsafe fn lv_set(lv: HWND, item: i32, sub: i32, text: &str) {
    let mut wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let mut lvi = LVITEMW { iSubItem: sub, pszText: PWSTR(wide.as_mut_ptr()), ..Default::default() };
    SendMessageW(lv, LVM_SETITEMTEXTW, Some(WPARAM(item as usize)), Some(LPARAM(&mut lvi as *mut _ as isize)));
}

/// Tampilkan dialog playlist; kembalikan job bila user menekan Download.
pub fn show_dialog(parent: HWND) -> Option<PlaylistJob> {
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None).ok()?.into();
        if !D_REG.swap(true, Ordering::SeqCst) {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(d_proc),
                hInstance: instance,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as *mut core::ffi::c_void),
                lpszClassName: D_CLASS,
                ..Default::default()
            };
            RegisterClassW(&wc);
        }
        D_DONE.store(false, Ordering::SeqCst);
        D_SAVED.store(false, Ordering::SeqCst);
        *D_ENTRIES.lock().unwrap() = Vec::new();
        *D_NAME.lock().unwrap() = String::new();
        let base = crate::gui::engine().map(|e| e.download_dir()).unwrap_or_default();
        *D_BASE.lock().unwrap() = base.to_string_lossy().into_owned();

        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        let mut rc = RECT { left: 0, top: 0, right: 600, bottom: 470 };
        let _ = AdjustWindowRectEx(&mut rc, style, false, WS_EX_DLGMODALFRAME);
        let (dw, dh) = (rc.right - rc.left, rc.bottom - rc.top);
        let mut pr = RECT::default();
        let _ = GetWindowRect(parent, &mut pr);
        let x = (pr.left + ((pr.right - pr.left) - dw) / 2).max(0);
        let y = (pr.top + ((pr.bottom - pr.top) - dh) / 2).max(0);

        let dlg = CreateWindowExW(
            WS_EX_DLGMODALFRAME, D_CLASS, w!("Download playlist / channel"), style,
            x, y, dw, dh, Some(parent), None, Some(instance), None,
        );
        let Ok(dlg) = dlg else { return None };

        const M: i32 = 16;
        let _ = mk(dlg, w!("STATIC"), w!("Playlist / channel URL:"), WINDOW_STYLE(0), M, 12, 280, 16, 0);
        let u = mk(dlg, w!("EDIT"), PCWSTR::null(), WINDOW_STYLE(WS_BORDER.0 | WS_TABSTOP.0 | ES_AUTOHSCROLL as u32), M, 32, 470, 24, DID_URL);
        let _ = mk(dlg, w!("BUTTON"), w!("Check"), WINDOW_STYLE(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), M + 476, 31, 92, 26, DID_CHECK);
        youtube::set_cue(u, "Paste a YouTube playlist / channel URL");

        // Daftar video (checkbox).
        let list = mk(
            dlg, w!("SysListView32"), PCWSTR::null(),
            WINDOW_STYLE(LVS_REPORT | LVS_NOSORTHEADER | WS_BORDER.0 | WS_TABSTOP.0),
            M, 66, 568, 250, DID_LIST,
        );
        SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, Some(WPARAM(0)),
            Some(LPARAM((LVS_EX_CHECKBOXES | LVS_EX_FULLROWSELECT) as isize)));
        // Kolom "#" harus muat checkbox (~16px) + nomor 2–3 digit, jadi lebih lebar.
        lv_add_col(list, 0, "#", 56);
        lv_add_col(list, 1, "Title", 434);
        lv_add_col(list, 2, "Duration", 72);

        let _ = mk(dlg, w!("BUTTON"), w!("Select all"), WINDOW_STYLE(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), M, 322, 90, 24, DID_ALL);
        let _ = mk(dlg, w!("BUTTON"), w!("Select none"), WINDOW_STYLE(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), M + 96, 322, 90, 24, DID_NONE);

        // Resolusi.
        let _ = mk(dlg, w!("STATIC"), w!("Resolution:"), WINDOW_STYLE(0), M + 300, 326, 70, 16, 0);
        let res = mk(dlg, w!("COMBOBOX"), PCWSTR::null(), WINDOW_STYLE(WS_TABSTOP.0 | CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0 | crate::dark::combo_style()), M + 372, 324, 180, 200, DID_RES);
        for (label, _) in RESOLUTIONS {
            let h = HSTRING::from(label);
            SendMessageW(res, CB_ADDSTRING, Some(WPARAM(0)), Some(LPARAM(h.as_ptr() as isize)));
        }
        SendMessageW(res, CB_SETCURSEL, Some(WPARAM(0)), Some(LPARAM(0)));

        // Save to.
        let _ = mk(dlg, w!("STATIC"), w!("Save to (a new folder is created):"), WINDOW_STYLE(0), M, 356, 400, 16, 0);
        let save = mk(dlg, w!("EDIT"), PCWSTR::null(), WINDOW_STYLE(WS_BORDER.0 | WS_TABSTOP.0 | ES_AUTOHSCROLL as u32), M, 376, 486, 24, DID_SAVE);
        set_text(save, &base.to_string_lossy());
        let _ = mk(dlg, w!("BUTTON"), w!("Browse..."), WINDOW_STYLE(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), M + 492, 375, 76, 26, DID_BROWSE);

        // Tombol.
        let _ = mk(dlg, w!("BUTTON"), w!("Download"), WINDOW_STYLE(WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32), 388, 420, 96, 30, DID_OK);
        let _ = mk(dlg, w!("BUTTON"), w!("Cancel"), WINDOW_STYLE(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), 490, 420, 94, 30, DID_CANCEL);

        *D_CTRL.lock().unwrap() = [u.0 as isize, list.0 as isize, save.0 as isize, res.0 as isize];

        crate::dark::apply(dlg);
        let _ = EnableWindow(parent, false);
        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetForegroundWindow(dlg);

        let _modal = crate::state::ModalGuard::new();
        let mut msg = MSG::default();
        while !D_DONE.load(Ordering::SeqCst) {
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

        let result = if D_SAVED.load(Ordering::SeqCst) {
            let entries = D_ENTRIES.lock().unwrap();
            // Item yang ter-check.
            let checked: Vec<PlEntry> = (0..entries.len())
                .filter(|&i| lv_checked(d_ctrl(1), i as i32))
                .filter_map(|i| entries.get(i).cloned())
                .collect();
            let folder = PathBuf::from(get_text(d_ctrl(2)).trim());
            let ri = SendMessageW(d_ctrl(3), CB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0 as usize;
            let height = RESOLUTIONS.get(ri).and_then(|r| r.1);
            let name = D_NAME.lock().unwrap().clone();
            if checked.is_empty() || folder.as_os_str().is_empty() {
                None
            } else {
                Some(PlaylistJob { name, folder, height, entries: checked })
            }
        } else {
            None
        };
        if IsWindow(Some(dlg)).as_bool() {
            let _ = DestroyWindow(dlg);
        }
        result
    }
}

/// Apakah item ke-`i` ter-check di ListView.
unsafe fn lv_checked(lv: HWND, i: i32) -> bool {
    let st = SendMessageW(lv, LVM_GETITEMSTATE, Some(WPARAM(i as usize)), Some(LPARAM(LVIS_STATEIMAGEMASK.0 as isize))).0 as u32;
    (st >> 12) == 2
}

unsafe fn lv_set_check(lv: HWND, i: i32, checked: bool) {
    let state = ((if checked { 2u32 } else { 1u32 }) << 12) & LVIS_STATEIMAGEMASK.0;
    let mut lvi = LVITEMW { stateMask: LVIS_STATEIMAGEMASK, state: LIST_VIEW_ITEM_STATE_FLAGS(state), ..Default::default() };
    SendMessageW(lv, LVM_SETITEMSTATE, Some(WPARAM(i as usize)), Some(LPARAM(&mut lvi as *mut _ as isize)));
}

unsafe fn do_check(hwnd: HWND) {
    let url = get_text(d_ctrl(0)).trim().to_string();
    if url.is_empty() {
        return;
    }
    let Some(ytdlp) = youtube::ytdlp_path() else { return };
    let hwnd_isize = hwnd.0 as isize;
    let _ = std::thread::Builder::new().name("pl-check".into()).spawn(move || {
        let result = youtube::fetch_playlist(&ytdlp, &url);
        let payload = Box::into_raw(Box::new(result));
        let hwnd = HWND(hwnd_isize as *mut core::ffi::c_void);
        if PostMessageW(Some(hwnd), WM_PL_META, WPARAM(0), LPARAM(payload as isize)).is_err() {
            drop(Box::from_raw(payload));
        }
    });
}

/// Isi ListView dari metadata hasil Check.
unsafe fn populate(meta: youtube::PlaylistMeta) {
    let list = d_ctrl(1);
    SendMessageW(list, LVM_DELETEALLITEMS, Some(WPARAM(0)), Some(LPARAM(0)));
    for (i, e) in meta.entries.iter().enumerate() {
        let mut num: Vec<u16> = format!("{}", i + 1).encode_utf16().chain(std::iter::once(0)).collect();
        let mut lvi = LVITEMW { mask: LVIF_TEXT, iItem: i as i32, pszText: PWSTR(num.as_mut_ptr()), ..Default::default() };
        SendMessageW(list, LVM_INSERTITEMW, Some(WPARAM(0)), Some(LPARAM(&mut lvi as *mut _ as isize)));
        lv_set(list, i as i32, 1, &e.title);
        lv_set(list, i as i32, 2, &e.duration.map(youtube::fmt_duration).unwrap_or_default());
        lv_set_check(list, i as i32, true);
    }
    // Folder default = base / nama playlist (folder baru).
    let base = PathBuf::from(D_BASE.lock().unwrap().clone());
    let folder = base.join(crate::engine::sanitize(&meta.name));
    set_text(d_ctrl(2), &folder.to_string_lossy());
    *D_NAME.lock().unwrap() = meta.name.clone();
    *D_ENTRIES.lock().unwrap() = meta.entries;
}

extern "system" fn d_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let id = wparam.0 & 0xFFFF;
                match id {
                    DID_CHECK => do_check(hwnd),
                    DID_BROWSE => {
                        if let Some(p) = crate::tasks::pick_folder(hwnd, "Pilih folder simpan") {
                            set_text(d_ctrl(2), &p.to_string_lossy());
                        }
                    }
                    DID_ALL | DID_NONE => {
                        let n = SendMessageW(d_ctrl(1), LVM_GETITEMCOUNT, Some(WPARAM(0)), Some(LPARAM(0))).0;
                        for i in 0..n {
                            lv_set_check(d_ctrl(1), i as i32, id == DID_ALL);
                        }
                    }
                    DID_OK => {
                        D_SAVED.store(true, Ordering::SeqCst);
                        D_DONE.store(true, Ordering::SeqCst);
                    }
                    DID_CANCEL => D_DONE.store(true, Ordering::SeqCst),
                    _ => {}
                }
                LRESULT(0)
            }
            m if m == WM_PL_META => {
                if lparam.0 != 0 {
                    let result = *Box::from_raw(lparam.0 as *mut Result<youtube::PlaylistMeta, String>);
                    match result {
                        Ok(meta) => populate(meta),
                        Err(e) => {
                            let msg = HSTRING::from(format!("Check gagal: {e}"));
                            MessageBoxW(Some(hwnd), PCWSTR(msg.as_ptr()), w!("YouTube"), MB_ICONWARNING);
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
                D_DONE.store(true, Ordering::SeqCst);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

// ============================ Jendela progres ============================

const W_CLASS: PCWSTR = w!("AdmPlaylistProgress");
static W_REG: AtomicBool = AtomicBool::new(false);
static W_OPEN: Mutex<Vec<(u64, isize)>> = Mutex::new(Vec::new());
const W_TIMER: usize = 1;

// ID kontrol sengaja mulai dari 100: `IsDialogMessageW` mengubah Enter/Esc jadi
// WM_COMMAND IDOK(1)/IDCANCEL(2), jadi ID kontrol tak boleh bertabrakan dengan
// keduanya (dulu WID_LIST=1 dan WID_RESUME=2 → Esc = "Resume selected").
const WID_LIST: usize = 101;
const WID_RESUME: usize = 102;
const WID_DELETE: usize = 103;
const WID_STOP: usize = 104;
const WID_CLOSE: usize = 105;

struct WinData {
    id: u64,
    /// Manajer di-cache: custom-draw dipanggil per SEL, dan `manager_of` di sana
    /// berarti kunci MANAGERS + clone Arc ratusan kali tiap repaint.
    mgr: Arc<Manager>,
    list: HWND,
    bar: HWND,
    lbl: HWND,
    /// jumlah baris tampil terakhir (untuk deteksi perlu rebuild).
    shown: usize,
    /// Teks sel terakhir yang ditulis per baris (Title, Status, Speed, Progress)
    /// — `LVM_SETITEMTEXT` selalu meng-invalidate subitem walau teksnya sama,
    /// jadi tanpa cache ini seluruh list repaint tiap tick timer.
    cells: Vec<[String; 4]>,
    /// Nilai terakhir bar + label agregat (hindari repaint sia-sia).
    last_bar: usize,
    last_lbl: String,
    /// Status enable terakhir [Resume, Delete, Stop] — tombol yang mati
    /// memberi tahu user KENAPA klik tak berefek (mis. belum ada seleksi).
    last_btn: [bool; 3],
}

unsafe fn win_data(hwnd: HWND) -> Option<&'static mut WinData> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WinData;
    if p.is_null() { None } else { Some(&mut *p) }
}

/// Buka (atau fokuskan) jendela progres playlist untuk `id`.
pub fn open_window(parent: HWND, id: u64) {
    unsafe {
        if let Some((_, h)) = W_OPEN.lock().unwrap().iter().find(|(i, _)| *i == id).copied() {
            let hwnd = HWND(h as *mut core::ffi::c_void);
            if IsWindow(Some(hwnd)).as_bool() {
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetForegroundWindow(hwnd);
                return;
            }
        }
        let Some(mgr) = manager_of(id) else { return };
        let instance: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
        if !W_REG.swap(true, Ordering::SeqCst) {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(w_proc),
                hInstance: instance,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as *mut core::ffi::c_void),
                lpszClassName: W_CLASS,
                ..Default::default()
            };
            RegisterClassW(&wc);
        }

        let title = HSTRING::from(format!("Playlist: {}", mgr.name));
        // WS_CLIPCHILDREN: tanpa ini latar (WM_ERASEBKGND, apalagi dark mode)
        // menimpa tombol tiap repaint → tombol berkedip & terasa tak merespons.
        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX | WS_CLIPCHILDREN;
        let mut rc = RECT { left: 0, top: 0, right: 640, bottom: 440 };
        let _ = AdjustWindowRectEx(&mut rc, style, false, WINDOW_EX_STYLE::default());
        let (dw, dh) = (rc.right - rc.left, rc.bottom - rc.top);
        let mut pr = RECT::default();
        let _ = GetWindowRect(parent, &mut pr);
        let x = (pr.left + 40).max(0);
        let y = (pr.top + 40).max(0);
        let dlg = CreateWindowExW(
            WINDOW_EX_STYLE::default(), W_CLASS, PCWSTR(title.as_ptr()), style,
            x, y, dw, dh, Some(parent), None, Some(instance), None,
        );
        let Ok(dlg) = dlg else { return };

        let list = mk(
            dlg, w!("SysListView32"), PCWSTR::null(),
            WINDOW_STYLE(LVS_REPORT | LVS_NOSORTHEADER | WS_BORDER.0 | WS_TABSTOP.0),
            10, 10, 604, 320, WID_LIST,
        );
        // DOUBLEBUFFER: cegah flicker putih saat sel di-update tiap tick timer.
        SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, Some(WPARAM(0)),
            Some(LPARAM((LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize)));
        lv_add_col(list, 0, "#", 34);
        lv_add_col(list, 1, "Title", 300);
        lv_add_col(list, 2, "Status", 80);
        lv_add_col(list, 3, "Speed", 88);
        lv_add_col(list, 4, "Progress", 100);

        let bar = mk(dlg, w!("msctls_progress32"), PCWSTR::null(), WINDOW_STYLE(0), 10, 340, 500, 18, 0);
        SendMessageW(bar, PBM_SETRANGE32, Some(WPARAM(0)), Some(LPARAM(1000)));
        let lbl = mk(dlg, w!("STATIC"), w!(""), WINDOW_STYLE(0), 520, 342, 94, 16, 0);

        let _ = mk(dlg, w!("BUTTON"), w!("Resume"), WINDOW_STYLE(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), 10, 372, 130, 30, WID_RESUME);
        let _ = mk(dlg, w!("BUTTON"), w!("Delete selected"), WINDOW_STYLE(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), 148, 372, 130, 30, WID_DELETE);
        let _ = mk(dlg, w!("BUTTON"), w!("Stop all"), WINDOW_STYLE(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), 286, 372, 100, 30, WID_STOP);
        let _ = mk(dlg, w!("BUTTON"), w!("Close"), WINDOW_STYLE(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), 514, 372, 100, 30, WID_CLOSE);

        let data = Box::new(WinData {
            id,
            mgr,
            list,
            bar,
            lbl,
            shown: usize::MAX,
            cells: Vec::new(),
            last_bar: usize::MAX,
            last_lbl: String::new(),
            last_btn: [true; 3],
        });
        SetWindowLongPtrW(dlg, GWLP_USERDATA, Box::into_raw(data) as isize);
        W_OPEN.lock().unwrap().push((id, dlg.0 as isize));

        w_layout(dlg);
        w_refresh(dlg);
        SetTimer(Some(dlg), W_TIMER, 500, None);
        crate::dark::apply(dlg);
        let _ = ShowWindow(dlg, SW_SHOW);
        // Tanpa fokus awal, keyboard tak mengenai kontrol mana pun: Tab/Enter/
        // spasi tak melakukan apa-apa sampai user mengklik sesuatu.
        let _ = SetFocus(Some(list));
    }
}

/// Saring pesan milik jendela progres playlist dari message loop utama.
/// Mengembalikan true bila pesan sudah ditangani di sini — pemanggil HARUS
/// melewati akselerator global.
///
/// Dua hal diperbaiki sekaligus:
/// * Jendela ini modeless dan bukan dialog, jadi tanpa `IsDialogMessageW` tak
///   ada navigasi Tab, tak ada Enter/Esc, dan tombol hanya bereaksi ke mouse.
/// * Tabel akselerator window utama (Del=Remove, Ctrl+N/F, F3, Ctrl+Q) berlaku
///   untuk SELURUH pesan thread. Menekan Del di sini dulu malah menghapus baris
///   di list utama, bukan item playlist yang dipilih.
pub fn pre_translate(msg: &MSG) -> bool {
    unsafe {
        if msg.hwnd.is_invalid() {
            return false;
        }
        let root = GetAncestor(msg.hwnd, GA_ROOT);
        let mine = W_OPEN.lock().unwrap().iter().any(|(_, h)| *h == root.0 as isize);
        if !mine {
            return false;
        }
        if !IsDialogMessageW(root, msg).as_bool() {
            let _ = TranslateMessage(msg);
            DispatchMessageW(msg);
        }
        true
    }
}

/// Terapkan ulang tema ke semua jendela progres playlist yang terbuka
/// (dipanggil saat user mengganti tema selagi jendela tampil).
pub fn retheme_open() {
    let hwnds: Vec<isize> = W_OPEN.lock().unwrap().iter().map(|(_, h)| *h).collect();
    for h in hwnds {
        unsafe {
            let hwnd = HWND(h as *mut core::ffi::c_void);
            if IsWindow(Some(hwnd)).as_bool() {
                crate::dark::retheme(hwnd);
            }
        }
    }
}

/// Indeks item (dalam Vec manajer) untuk baris ListView non-Removed.
fn visible_map(mgr: &Arc<Manager>) -> Vec<usize> {
    mgr.items
        .lock()
        .unwrap()
        .iter()
        .enumerate()
        .filter(|(_, it)| it.status != ItemStatus::Removed)
        .map(|(i, _)| i)
        .collect()
}

/// Reflow kontrol mengikuti ukuran window: list mengisi area atas, bar+label
/// tepat di atas baris tombol, tombol menempel di tepi bawah, Title menyerap
/// sisa lebar. Dipanggil sekali saat buka + tiap `WM_SIZE`.
unsafe fn w_layout(hwnd: HWND) {
    let Some(d) = win_data(hwnd) else { return };
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let (cw, ch) = (rc.right, rc.bottom);
    const M: i32 = 10;
    const BH: i32 = 30; // tinggi baris tombol
    const BARH: i32 = 18; // tinggi bar agregat

    let btn_y = ch - M - BH;
    let bar_y = btn_y - 12 - BARH;
    let list_h = (bar_y - M - M).max(60);
    let _ = MoveWindow(d.list, M, M, (cw - 2 * M).max(80), list_h, true);

    // Bar agregat + label "done/total" (label rata kanan).
    const LBL_W: i32 = 94;
    let lbl_x = cw - M - LBL_W;
    let bar_w = (lbl_x - M - M).max(60);
    let _ = MoveWindow(d.bar, M, bar_y, bar_w, BARH, true);
    let _ = MoveWindow(d.lbl, lbl_x, bar_y + 1, LBL_W, 16, true);

    // Tombol: tiga di kiri, Close menempel kanan.
    let btn = |id: usize| GetDlgItem(Some(hwnd), id as i32).unwrap_or_default();
    let _ = MoveWindow(btn(WID_RESUME), M, btn_y, 130, BH, true);
    let _ = MoveWindow(btn(WID_DELETE), M + 138, btn_y, 130, BH, true);
    let _ = MoveWindow(btn(WID_STOP), M + 276, btn_y, 100, BH, true);
    let _ = MoveWindow(btn(WID_CLOSE), cw - M - 100, btn_y, 100, BH, true);

    // Title menyerap sisa lebar (kolom lain tetap: #, Status, Speed, Progress).
    let others = 34 + 80 + 88 + 100;
    let title_w = ((cw - 2 * M) - others - 24).max(120); // 24 ~ border + scrollbar
    SendMessageW(d.list, LVM_SETCOLUMNWIDTH, Some(WPARAM(1)), Some(LPARAM(title_w as isize)));
}

unsafe fn w_refresh(hwnd: HWND) {
    let Some(d) = win_data(hwnd) else { return };
    // Manajer sudah dilepas (baris playlist dihapus dari list utama) → tutup.
    if !is_playlist(d.id) {
        let _ = DestroyWindow(hwnd);
        return;
    }
    let mgr = d.mgr.clone();
    let map = visible_map(&mgr);
    // Rebuild bila jumlah baris berubah (mis. ada yang dihapus).
    if map.len() != d.shown {
        SendMessageW(d.list, LVM_DELETEALLITEMS, Some(WPARAM(0)), Some(LPARAM(0)));
        for (row, &item_idx) in map.iter().enumerate() {
            let mut num: Vec<u16> = format!("{}", row + 1).encode_utf16().chain(std::iter::once(0)).collect();
            let mut lvi = LVITEMW {
                mask: LVIF_TEXT | LVIF_PARAM,
                iItem: row as i32,
                pszText: PWSTR(num.as_mut_ptr()),
                lParam: LPARAM(item_idx as isize),
                ..Default::default()
            };
            SendMessageW(d.list, LVM_INSERTITEMW, Some(WPARAM(0)), Some(LPARAM(&mut lvi as *mut _ as isize)));
        }
        d.shown = map.len();
        d.cells = vec![<[String; 4]>::default(); map.len()];
    }

    // Rakit seluruh teks sel SELAGI memegang lock, lalu lepaskan sebelum
    // menyentuh ListView: `pl_customdraw` juga mengunci `items`, jadi mengirim
    // pesan ke list sambil memegang lock itu adalah deadlock yang menunggu
    // terjadi (cukup satu repaint sinkron).
    let (rows, done, total, resumable, running) = {
        let items = mgr.items.lock().unwrap();
        let mut rows: Vec<[String; 4]> = Vec::with_capacity(map.len());
        let (mut done, mut resumable, mut running) = (0usize, false, false);
        for &item_idx in &map {
            let Some(it) = items.get(item_idx) else {
                rows.push(Default::default());
                continue;
            };
            // Kecepatan (hanya saat mengunduh).
            let speed = if it.status == ItemStatus::Downloading && it.speed > 0 {
                fmt_speed(it.speed)
            } else {
                String::new()
            };
            // Progress: teks fallback; untuk Downloading (total diketahui)
            // bar+persen digambar via custom-draw menimpa teks ini.
            let prog = match it.status {
                ItemStatus::Downloading => match it.total {
                    Some(t) if t > 0 => format!("{}%", (it.downloaded.saturating_mul(100) / t).min(100)),
                    _ => youtube::fmt_mb(it.downloaded),
                },
                ItemStatus::Done => it.total.map(youtube::fmt_mb).unwrap_or_default(),
                ItemStatus::Error => trunc(&it.error, 40),
                ItemStatus::Pending | ItemStatus::Stopped => "-".into(),
                ItemStatus::Removed => String::new(),
            };
            match it.status {
                ItemStatus::Done => done += 1,
                ItemStatus::Downloading => running = true,
                ItemStatus::Pending | ItemStatus::Stopped | ItemStatus::Error => resumable = true,
                ItemStatus::Removed => {}
            }
            rows.push([it.title.clone(), it.status.label().to_string(), speed, prog]);
        }
        let paused = mgr.paused.load(Ordering::SeqCst);
        (rows, done, map.len(), resumable, running || !paused)
    };

    // Tulis HANYA sel yang berubah. `LVM_SETITEMTEXT` selalu meng-invalidate
    // subitem-nya walau teks identik, jadi menulis semuanya tiap 500 ms berarti
    // seluruh list (plus custom-draw per sel) repaint dua kali per detik — pada
    // playlist panjang UI jadi berat dan tombol terasa lambat/tak merespons.
    for (row, next) in rows.iter().enumerate() {
        let Some(prev) = d.cells.get_mut(row) else { continue };
        for c in 0..4 {
            if prev[c] != next[c] {
                lv_set(d.list, row as i32, c as i32 + 1, &next[c]);
                prev[c].clone_from(&next[c]);
            }
        }
    }

    // Status tombol. Sebelumnya semuanya selalu aktif, jadi mengklik "Resume"/
    // "Delete" tanpa baris terpilih benar-benar tak melakukan apa pun — persis
    // terasa seperti tombol yang tidak merespons. Sekarang tombol yang tak bisa
    // dipakai tampak mati.
    let sel = SendMessageW(d.list, LVM_GETSELECTEDCOUNT, Some(WPARAM(0)), Some(LPARAM(0))).0;
    let want = [resumable, sel > 0, running];
    for (i, (&on, id)) in want
        .iter()
        .zip([WID_RESUME, WID_DELETE, WID_STOP])
        .enumerate()
    {
        if d.last_btn[i] != on {
            if let Ok(b) = GetDlgItem(Some(hwnd), id as i32) {
                // Menonaktifkan kontrol yang sedang fokus membuang fokus ke
                // "tak ke mana-mana" — keyboard mati total sampai user mengklik
                // lagi. Pindahkan dulu ke list.
                if !on && GetFocus() == b {
                    let _ = SetFocus(Some(d.list));
                }
                let _ = EnableWindow(b, on);
            }
            d.last_btn[i] = on;
        }
    }

    let permille = (done * 1000).checked_div(total).unwrap_or(0);
    if permille != d.last_bar {
        SendMessageW(d.bar, PBM_SETPOS, Some(WPARAM(permille)), Some(LPARAM(0)));
        d.last_bar = permille;
    }
    let lbl = format!("{done} / {total} done");
    if lbl != d.last_lbl {
        set_text(d.lbl, &lbl);
        d.last_lbl = lbl;
    }
}

const COL_PROGRESS: i32 = 4;

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

fn fmt_speed(bps: u64) -> String {
    let b = bps as f64;
    if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB/s", b / (1024.0 * 1024.0))
    } else {
        format!("{:.0} KB/s", b / 1024.0)
    }
}

/// Gambar bar progres + persen pada kolom Progress untuk item Downloading.
unsafe fn pl_customdraw(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let p = &*(lparam.0 as *const NMLVCUSTOMDRAW);
    let stage = p.nmcd.dwDrawStage.0;
    if stage == CDDS_PREPAINT.0 {
        return LRESULT(CDRF_NOTIFYITEMDRAW as isize);
    }
    if stage == CDDS_ITEMPREPAINT.0 {
        return LRESULT(CDRF_NOTIFYSUBITEMDRAW as isize);
    }
    let dodefault = LRESULT(CDRF_DODEFAULT as isize);
    if stage != (CDDS_ITEMPREPAINT.0 | CDDS_SUBITEM.0) || p.iSubItem != COL_PROGRESS {
        return dodefault;
    }
    let Some(d) = win_data(hwnd) else { return dodefault };
    let item_idx = p.nmcd.lItemlParam.0 as usize;
    let (dl, total, downloading) = {
        let items = d.mgr.items.lock().unwrap();
        match items.get(item_idx) {
            Some(it) => (it.downloaded, it.total, it.status == ItemStatus::Downloading),
            None => return dodefault,
        }
    };
    if !downloading {
        return dodefault;
    }
    let Some(total) = total.filter(|t| *t > 0) else { return dodefault };
    let pct = (dl.saturating_mul(100) / total).min(100) as i32;

    let idx = p.nmcd.dwItemSpec as i32;
    let mut r = RECT { left: 0, top: COL_PROGRESS, ..Default::default() };
    SendMessageW(d.list, LVM_GETSUBITEMRECT, Some(WPARAM(idx as usize)), Some(LPARAM(&mut r as *mut _ as isize)));
    let mut bar = RECT { left: r.left + 3, top: r.top + 2, right: r.right - 3, bottom: r.bottom - 2 };
    if bar.right <= bar.left || bar.bottom <= bar.top {
        return dodefault;
    }
    let dark = crate::dark::is_dark();
    let hdc = p.nmcd.hdc;
    let track = CreateSolidBrush(if dark { rgb(52, 58, 68) } else { rgb(228, 228, 228) });
    FillRect(hdc, &bar, track);
    let _ = DeleteObject(track.into());
    let w = bar.right - bar.left;
    let fill_rc = RECT { right: bar.left + w * pct / 100, ..bar };
    let green = CreateSolidBrush(if dark { rgb(152, 195, 121) } else { rgb(59, 160, 90) });
    FillRect(hdc, &fill_rc, green);
    let _ = DeleteObject(green.into());
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, if dark { rgb(220, 224, 232) } else { rgb(20, 20, 20) });
    let mut wide: Vec<u16> = format!("{pct}%").encode_utf16().collect();
    DrawTextW(hdc, &mut wide, &mut bar, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    LRESULT(CDRF_SKIPDEFAULT as isize)
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Indeks item terpilih (via lParam baris ListView).
unsafe fn selected_item_indices(lv: HWND) -> Vec<usize> {
    let n = SendMessageW(lv, LVM_GETITEMCOUNT, Some(WPARAM(0)), Some(LPARAM(0))).0;
    let mut out = Vec::new();
    for row in 0..n {
        let st = SendMessageW(lv, LVM_GETITEMSTATE, Some(WPARAM(row as usize)), Some(LPARAM(LVIS_SELECTED.0 as isize))).0 as u32;
        if st & LVIS_SELECTED.0 != 0 {
            let mut lvi = LVITEMW { mask: LVIF_PARAM, iItem: row as i32, ..Default::default() };
            if SendMessageW(lv, LVM_GETITEMW, Some(WPARAM(0)), Some(LPARAM(&mut lvi as *mut _ as isize))).0 != 0 {
                out.push(lvi.lParam.0 as usize);
            }
        }
    }
    out
}

extern "system" fn w_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                w_refresh(hwnd);
                LRESULT(0)
            }
            WM_SIZE => {
                w_layout(hwnd);
                LRESULT(0)
            }
            WM_GETMINMAXINFO => {
                let mmi = &mut *(lparam.0 as *mut MINMAXINFO);
                mmi.ptMinTrackSize.x = 460;
                mmi.ptMinTrackSize.y = 320;
                LRESULT(0)
            }
            WM_NOTIFY => {
                let hdr = &*(lparam.0 as *const NMHDR);
                if let Some(d) = win_data(hwnd) {
                    if hdr.hwndFrom == d.list && hdr.code == NM_CUSTOMDRAW {
                        return pl_customdraw(hwnd, lparam);
                    }
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = wparam.0 & 0xFFFF;
                // Esc (IsDialogMessageW → IDCANCEL) menutup; Enter (IDOK)
                // menjalankan aksi utama, sama seperti tombol Resume.
                if id == IDCANCEL.0 as usize || id == WID_CLOSE {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                let Some(d) = win_data(hwnd) else { return LRESULT(0) };
                let mgr = d.mgr.clone();
                match id {
                    WID_RESUME | 1 /* IDOK */ => {
                        let sel = selected_item_indices(d.list);
                        // Tanpa seleksi, Resume di jendela ini tak pernah
                        // melakukan apa pun — user mengira tombolnya rusak.
                        // Kosong = lanjutkan semua yang belum selesai.
                        if sel.is_empty() {
                            resume_all(d.id);
                        } else {
                            resume_items(&mgr, &sel);
                        }
                    }
                    WID_DELETE => {
                        let sel = selected_item_indices(d.list);
                        if !sel.is_empty() {
                            delete_items(&mgr, &sel);
                            d.shown = usize::MAX; // paksa rebuild
                        }
                    }
                    WID_STOP => pause(d.id),
                    _ => return LRESULT(0),
                }
                w_refresh(hwnd);
                LRESULT(0)
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
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                let _ = KillTimer(Some(hwnd), W_TIMER);
                W_OPEN.lock().unwrap().retain(|(_, h)| *h != hwnd.0 as isize);
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WinData;
                if !p.is_null() {
                    drop(Box::from_raw(p));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
