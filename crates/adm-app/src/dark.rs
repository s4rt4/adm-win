//! Helper tema gelap bersama untuk dialog (One Dark Pro). Win32 klasik tak
//! punya dark mode otomatis: latar & teks kontrol diwarnai lewat `WM_CTLCOLOR*`,
//! latar dialog lewat `WM_ERASEBKGND`, title bar lewat DWM, dan anak (tombol/
//! edit/scrollbar) ditema `DarkMode_Explorer`. Dipakai oleh semua proc dialog
//! agar konsisten dengan window utama. Aman saat tema terang (semua jadi no-op).

use std::sync::Mutex;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// Palet One Dark Pro (selaras `gui.rs`).
const BG: (u8, u8, u8) = (40, 44, 52); // #282C34 latar dialog
const EDIT_BG: (u8, u8, u8) = (30, 33, 39); // #1E2127 field input (sedikit lebih gelap)
const TEXT: (u8, u8, u8) = (171, 178, 191); // #ABB2BF teks

// Brush di-cache (hidup selama proses; sengaja tak dilepas — dipakai berulang).
static BG_BRUSH: Mutex<isize> = Mutex::new(0);
static EDIT_BRUSH: Mutex<isize> = Mutex::new(0);

fn rgb(c: (u8, u8, u8)) -> COLORREF {
    COLORREF((c.0 as u32) | ((c.1 as u32) << 8) | ((c.2 as u32) << 16))
}

/// Apakah tema gelap aktif (mengikuti setting global).
pub fn is_dark() -> bool {
    crate::theme::effective_dark(crate::settings::get().theme)
}

unsafe fn cached_brush(slot: &Mutex<isize>, color: (u8, u8, u8)) -> HBRUSH {
    let mut g = slot.lock().unwrap_or_else(|e| e.into_inner());
    if *g == 0 {
        *g = CreateSolidBrush(rgb(color)).0 as isize;
    }
    HBRUSH(*g as *mut core::ffi::c_void)
}

/// Terapkan dark ke window dialog + semua anaknya. Panggil SETELAH semua kontrol
/// anak dibuat (mis. tepat sebelum `ShowWindow`). No-op bila tema terang.
pub unsafe fn apply(hwnd: HWND) {
    if !is_dark() {
        return;
    }
    let flag = windows::core::BOOL(1);
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &flag as *const _ as *const core::ffi::c_void,
        std::mem::size_of::<windows::core::BOOL>() as u32,
    );
    let _ = EnumChildWindows(Some(hwnd), Some(theme_child), LPARAM(0));
    let _ = InvalidateRect(Some(hwnd), None, true);
}

unsafe extern "system" fn theme_child(child: HWND, _: LPARAM) -> BOOL {
    // DarkMode_Explorer membuat tombol/edit/scrollbar/combo dirender gelap di Win10+.
    let _ = SetWindowTheme(child, w!("DarkMode_Explorer"), PCWSTR::null());
    // ListView/TreeView tak menghormati WM_CTLCOLOR → warnai langsung via pesan.
    let mut cls = [0u16; 64];
    let n = GetClassNameW(child, &mut cls);
    let name = String::from_utf16_lossy(&cls[..n as usize]);
    let bg = rgb(BG).0 as isize;
    let txt = rgb(TEXT).0 as isize;
    if name.eq_ignore_ascii_case("SysListView32") {
        SendMessageW(child, LVM_SETBKCOLOR, Some(WPARAM(0)), Some(LPARAM(bg)));
        SendMessageW(child, LVM_SETTEXTBKCOLOR, Some(WPARAM(0)), Some(LPARAM(bg)));
        SendMessageW(child, LVM_SETTEXTCOLOR, Some(WPARAM(0)), Some(LPARAM(txt)));
    } else if name.eq_ignore_ascii_case("SysTreeView32") {
        SendMessageW(child, TVM_SETBKCOLOR, Some(WPARAM(0)), Some(LPARAM(bg)));
        SendMessageW(child, TVM_SETTEXTCOLOR, Some(WPARAM(0)), Some(LPARAM(txt)));
    }
    BOOL(1)
}

/// Tangani `WM_CTLCOLOR*` (STATIC/EDIT/BTN/LISTBOX/DLG/SCROLLBAR). Kembalikan
/// `Some(brush)` sbg LRESULT bila dark aktif; `None` → biarkan proc pakai default.
/// `wparam` = HDC kontrol.
pub unsafe fn ctlcolor(msg: u32, wparam: WPARAM) -> Option<LRESULT> {
    if !is_dark() {
        return None;
    }
    let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
    SetTextColor(hdc, rgb(TEXT));
    let brush = if msg == WM_CTLCOLOREDIT || msg == WM_CTLCOLORLISTBOX {
        // Field input: latar opaque sedikit lebih gelap agar terlihat sbg kotak.
        SetBkColor(hdc, rgb(EDIT_BG));
        cached_brush(&EDIT_BRUSH, EDIT_BG)
    } else {
        // Label/tombol/dialog: menyatu dengan latar dialog, teks transparan.
        SetBkMode(hdc, TRANSPARENT);
        SetBkColor(hdc, rgb(BG));
        cached_brush(&BG_BRUSH, BG)
    };
    Some(LRESULT(brush.0 as isize))
}

/// Isi latar dialog dengan warna gelap saat `WM_ERASEBKGND`. Kembalikan
/// `Some(LRESULT(1))` bila ditangani (dark), `None` bila terang.
pub unsafe fn erasebkgnd(hwnd: HWND, wparam: WPARAM) -> Option<LRESULT> {
    if !is_dark() {
        return None;
    }
    let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    FillRect(hdc, &rc, cached_brush(&BG_BRUSH, BG));
    Some(LRESULT(1))
}
