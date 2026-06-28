//! Helper tema gelap bersama untuk dialog (One Dark Pro). Win32 klasik tak
//! punya dark mode otomatis: latar & teks kontrol diwarnai lewat `WM_CTLCOLOR*`,
//! latar dialog lewat `WM_ERASEBKGND`, title bar lewat DWM, dan anak (tombol/
//! edit/scrollbar) ditema `DarkMode_Explorer`. Dipakai oleh semua proc dialog
//! agar konsisten dengan window utama. Aman saat tema terang (semua jadi no-op).

use std::sync::Mutex;
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

// Palet One Dark Pro (selaras `gui.rs`).
const BG: (u8, u8, u8) = (40, 44, 52); // #282C34 latar dialog
const EDIT_BG: (u8, u8, u8) = (30, 33, 39); // #1E2127 field input (sedikit lebih gelap)
const TEXT: (u8, u8, u8) = (171, 178, 191); // #ABB2BF teks
const HEADER_BG: (u8, u8, u8) = (45, 50, 59); // #2D323B header tabel (sedikit > body)
const HEADER_SEP: (u8, u8, u8) = (60, 66, 76); // pemisah kolom header (halus)

// ID subclass header (arbitrer, unik per kelas subclass).
const HEADER_SUBCLASS_ID: usize = 0x00AD_0001;

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
        install_header(child);
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

/// Pasang subclass pada ListView agar header (SysHeader32) bisa di-custom-draw
/// gelap — notifikasi NM_CUSTOMDRAW header dikirim ke ListView, bukan ke window
/// utama. Aman dipanggil berulang (uIdSubclass sama = perbarui, tak menumpuk).
/// Proc menggating sendiri via `is_dark()`, jadi tetap benar saat tema terang.
pub unsafe fn install_header(lv: HWND) {
    let _ = SetWindowSubclass(lv, Some(lv_subclass), HEADER_SUBCLASS_ID, 0);
    // Paksa header repaint agar warna langsung ikut.
    let hdr = SendMessageW(lv, LVM_GETHEADER, Some(WPARAM(0)), Some(LPARAM(0)));
    if hdr.0 != 0 {
        let _ = InvalidateRect(Some(HWND(hdr.0 as *mut core::ffi::c_void)), None, true);
    }
}

unsafe extern "system" fn lv_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _ref: usize,
) -> LRESULT {
    if msg == WM_NOTIFY && is_dark() && lparam.0 != 0 {
        let nm = &*(lparam.0 as *const NMHDR);
        if nm.code == NM_CUSTOMDRAW {
            let mut cls = [0u16; 32];
            let n = GetClassNameW(nm.hwndFrom, &mut cls);
            if String::from_utf16_lossy(&cls[..n as usize]).eq_ignore_ascii_case("SysHeader32") {
                if let Some(r) = header_customdraw(lparam) {
                    return r;
                }
            }
        }
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

/// Custom-draw satu item header: latar gelap + teks terang + pemisah halus.
unsafe fn header_customdraw(lparam: LPARAM) -> Option<LRESULT> {
    let cd = &mut *(lparam.0 as *mut NMCUSTOMDRAW);
    let stage = cd.dwDrawStage;
    if stage == CDDS_PREPAINT {
        return Some(LRESULT(CDRF_NOTIFYITEMDRAW as isize));
    }
    if stage != CDDS_ITEMPREPAINT {
        return Some(LRESULT(CDRF_DODEFAULT as isize));
    }
    let hdc = cd.hdc;
    let rc = cd.rc;
    // Latar header.
    let bg = CreateSolidBrush(rgb(HEADER_BG));
    FillRect(hdc, &rc, bg);
    let _ = DeleteObject(bg.into());
    // Pemisah kolom tipis di tepi kanan.
    let sep = CreateSolidBrush(rgb(HEADER_SEP));
    let line = RECT { left: rc.right - 1, top: rc.top + 4, right: rc.right, bottom: rc.bottom - 4 };
    FillRect(hdc, &line, sep);
    let _ = DeleteObject(sep.into());
    // Teks + perataan dari item header.
    let mut buf = [0u16; 128];
    let mut item = HDITEMW {
        mask: HDI_TEXT | HDI_FORMAT,
        pszText: PWSTR(buf.as_mut_ptr()),
        cchTextMax: 128,
        ..Default::default()
    };
    let idx = cd.dwItemSpec as usize;
    SendMessageW(
        cd.hdr.hwndFrom,
        HDM_GETITEMW,
        Some(WPARAM(idx)),
        Some(LPARAM(&mut item as *mut _ as isize)),
    );
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let mut wide: Vec<u16> = buf[..len].to_vec();
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, rgb(TEXT));
    let mut tr = RECT { left: rc.left + 7, top: rc.top, right: rc.right - 6, bottom: rc.bottom };
    let align = if item.fmt.0 & HDF_RIGHT.0 != 0 {
        DT_RIGHT
    } else if item.fmt.0 & HDF_CENTER.0 != 0 {
        DT_CENTER
    } else {
        DT_LEFT
    };
    DrawTextW(hdc, &mut wide, &mut tr, align | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
    Some(LRESULT(CDRF_SKIPDEFAULT as isize))
}
