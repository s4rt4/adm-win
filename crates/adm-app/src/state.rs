//! State bersama UI <-> engine + pesan kustom untuk marshalling ke UI thread.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// Klik/aksi tray (uCallbackMessage).
pub const WM_TRAY: u32 = WM_APP + 1;
/// Progres/daftar unduhan berubah → UI refresh.
pub const WM_PROGRESS: u32 = WM_APP + 2;
/// Instance kedua minta jendela dimunculkan.
pub const WM_ACTIVATE_APP: u32 = WM_APP + 3;
/// Browser/bridge menitip unduhan → tampilkan dialog "Download File Info".
/// lparam = `*mut DownloadAddParams` (Box mentah, diambil-alih UI thread).
pub const WM_ADD_DOWNLOAD: u32 = WM_APP + 4;

pub static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
pub static MENUSTRIP_HWND: AtomicIsize = AtomicIsize::new(0);
pub static TOOLBAR_HWND: AtomicIsize = AtomicIsize::new(0);
pub static TREE_HWND: AtomicIsize = AtomicIsize::new(0);
pub static LIST_HWND: AtomicIsize = AtomicIsize::new(0);
pub static STATUS_HWND: AtomicIsize = AtomicIsize::new(0);

/// Sidebar kategori terlihat (View ▸ Hide categories).
pub static SIDEBAR_VISIBLE: AtomicBool = AtomicBool::new(true);
/// Toolbar terlihat (View ▸ Toolbar).
pub static TOOLBAR_VISIBLE: AtomicBool = AtomicBool::new(true);
/// Ikon tray aktif (View ▸ ADM tray icon).
pub static TRAY_VISIBLE: AtomicBool = AtomicBool::new(true);

fn to_hwnd(v: isize) -> Option<HWND> {
    if v == 0 {
        None
    } else {
        Some(HWND(v as *mut core::ffi::c_void))
    }
}

pub fn store_hwnd(slot: &AtomicIsize, hwnd: HWND) {
    slot.store(hwnd.0 as isize, Ordering::SeqCst);
}

pub fn load_hwnd(slot: &AtomicIsize) -> Option<HWND> {
    to_hwnd(slot.load(Ordering::SeqCst))
}

/// Kirim pesan kustom ke UI thread (aman dari thread mana pun).
pub fn post_to_ui(msg: u32) {
    if let Some(hwnd) = load_hwnd(&MAIN_HWND) {
        unsafe {
            let _ = PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(0));
        }
    }
}

/// Kedalaman dialog modal yang sedang memompa message-loop sendiri.
/// `refresh_ui` menunda popup completion/failed selama > 0 — popup yang muncul
/// DI DALAM loop modal dialog lain merusak modality (EnableWindow(parent, true)
/// tanpa syarat) dan bisa meng-clobber statics dialog.
static MODAL_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// Guard RAII: naikkan kedalaman modal selama hidup; saat drop, turunkan dan
/// "senggol" UI (WM_PROGRESS) agar popup yang tertunda segera ditampilkan.
pub struct ModalGuard;

impl ModalGuard {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        MODAL_DEPTH.fetch_add(1, Ordering::SeqCst);
        ModalGuard
    }
}

impl Drop for ModalGuard {
    fn drop(&mut self) {
        MODAL_DEPTH.fetch_sub(1, Ordering::SeqCst);
        post_to_ui(WM_PROGRESS);
    }
}

pub fn in_modal() -> bool {
    MODAL_DEPTH.load(Ordering::SeqCst) > 0
}
