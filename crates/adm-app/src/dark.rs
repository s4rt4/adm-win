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
const SEL_BG: (u8, u8, u8) = (58, 63, 75); // sorotan item combo/list terpilih
const PROGRESS_GREEN: (u8, u8, u8) = (6, 176, 37); // hijau progress bar Windows (klasik)

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

/// Brush latar gelap (cache proses) — untuk handler `WM_CTLCOLOR*` manual di
/// luar [`ctlcolor`] (mis. label empty-state di window utama).
///
/// # Safety
/// Panggil dari UI thread.
pub unsafe fn bg_brush() -> HBRUSH {
    cached_brush(&BG_BRUSH, BG)
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
///
/// # Safety
/// `hwnd` harus window valid; panggil dari thread pemilik window (UI thread).
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

/// Terapkan ULANG tema (gelap ATAU terang) ke dialog yang sudah terbuka —
/// dipakai saat user mengganti tema selagi dialog modeless (progress/playlist)
/// tampil. Berbeda dengan [`apply`] yang satu arah (no-op saat terang), fungsi
/// ini juga MENGEMBALIKAN kontrol ke tampilan terang, lalu memaksa repaint
/// penuh (tanpa ini hanya kontrol yang kebetulan repaint yang berubah warna —
/// dialog jadi belang gelap/terang).
///
/// # Safety
/// `hwnd` harus window valid; panggil dari thread pemilik window (UI thread).
pub unsafe fn retheme(hwnd: HWND) {
    let dark = is_dark();
    let flag = windows::core::BOOL(dark as i32);
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &flag as *const _ as *const core::ffi::c_void,
        std::mem::size_of::<windows::core::BOOL>() as u32,
    );
    let _ = EnumChildWindows(Some(hwnd), Some(retheme_child), LPARAM(dark as isize));
    let _ = RedrawWindow(
        Some(hwnd),
        None,
        None,
        RDW_ERASE | RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_FRAME,
    );
}

unsafe extern "system" fn retheme_child(child: HWND, lp: LPARAM) -> BOOL {
    if lp.0 != 0 {
        return theme_child(child, LPARAM(0));
    }
    // Kembali ke terang: tema standar + reset warna list/tree ke default sistem.
    let _ = SetWindowTheme(child, w!("Explorer"), PCWSTR::null());
    let mut cls = [0u16; 64];
    let n = GetClassNameW(child, &mut cls);
    let name = String::from_utf16_lossy(&cls[..n as usize]);
    let bg = GetSysColor(COLOR_WINDOW) as isize;
    let txt = GetSysColor(COLOR_WINDOWTEXT) as isize;
    if name.eq_ignore_ascii_case("SysListView32") {
        SendMessageW(child, LVM_SETBKCOLOR, Some(WPARAM(0)), Some(LPARAM(bg)));
        SendMessageW(child, LVM_SETTEXTBKCOLOR, Some(WPARAM(0)), Some(LPARAM(bg)));
        SendMessageW(child, LVM_SETTEXTCOLOR, Some(WPARAM(0)), Some(LPARAM(txt)));
        // Subclass header (bila terpasang) self-gate via is_dark() → cukup repaint.
        let hdr = SendMessageW(child, LVM_GETHEADER, Some(WPARAM(0)), Some(LPARAM(0)));
        if hdr.0 != 0 {
            let _ = InvalidateRect(Some(HWND(hdr.0 as *mut core::ffi::c_void)), None, true);
        }
    } else if name.eq_ignore_ascii_case("SysTreeView32") {
        // -1 = CLR_DEFAULT (kembalikan ke warna bawaan).
        SendMessageW(child, TVM_SETBKCOLOR, Some(WPARAM(0)), Some(LPARAM(-1)));
        SendMessageW(child, TVM_SETTEXTCOLOR, Some(WPARAM(0)), Some(LPARAM(-1)));
    }
    BOOL(1)
}

unsafe extern "system" fn theme_child(child: HWND, _: LPARAM) -> BOOL {
    let mut cls = [0u16; 64];
    let n = GetClassNameW(child, &mut cls);
    let name = String::from_utf16_lossy(&cls[..n as usize]);
    // Progress bar: PP_FILL versi DarkMode digambar PUTIH (bukan hijau) →
    // matikan theme dan warnai klasik: trough gelap + bar hijau.
    if name.eq_ignore_ascii_case("msctls_progress32") {
        let _ = SetWindowTheme(child, w!(""), w!(""));
        SendMessageW(child, PBM_SETBKCOLOR, Some(WPARAM(0)), Some(LPARAM(rgb(EDIT_BG).0 as isize)));
        SendMessageW(child, PBM_SETBARCOLOR, Some(WPARAM(0)), Some(LPARAM(rgb(PROGRESS_GREEN).0 as isize)));
        return BOOL(1);
    }
    // SegmentBar (dialog progres) mengambil hijau dari tema "PROGRESS" milik
    // window-nya saat paint; biarkan tema default agar tak dapat versi DarkMode
    // (fill putih). Warna gelap trough-nya diatur sendiri di paint_segbar.
    if name.eq_ignore_ascii_case("AdmSegmentBar") {
        return BOOL(1);
    }
    // DarkMode_Explorer membuat tombol/edit/scrollbar/combo dirender gelap di Win10+.
    let _ = SetWindowTheme(child, w!("DarkMode_Explorer"), PCWSTR::null());
    // ListView/TreeView tak menghormati WM_CTLCOLOR → warnai langsung via pesan.
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
    } else if name.eq_ignore_ascii_case("ComboBox") {
        // Tombol dropdown ▾ combobox tetap putih walau DarkMode_Explorer → subclass
        // untuk meng-overpaint tombol gelap. (Permukaan sudah owner-draw dark.)
        install_combo(child);
    } else if name.eq_ignore_ascii_case("SysTabControl32") {
        // Tema DarkMode tak mencakup kelas Tab: pane tetap abu terang → label
        // di atasnya tampak kotak-kotak. Subclass menggambar tab gelap penuh.
        install_tab(child);
    }
    BOOL(1)
}

// ID subclass tab control (unik terhadap subclass lain di modul ini).
const TAB_SUBCLASS_ID: usize = 0x00AD_0003;

/// Subclass tab control agar digambar gelap penuh (baris tab + pane). Proc
/// menggating sendiri via `is_dark()` — saat tema terang kembali ke gambar
/// default, jadi aman dipasang permanen dan dipanggil berulang.
///
/// # Safety
/// `tab` harus handle tab control (`SysTabControl32`) valid.
pub unsafe fn install_tab(tab: HWND) {
    let _ = SetWindowSubclass(tab, Some(tab_subclass), TAB_SUBCLASS_ID, 0);
}

unsafe extern "system" fn tab_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _ref: usize,
) -> LRESULT {
    if is_dark() {
        match msg {
            WM_ERASEBKGND => return LRESULT(1),
            WM_PAINT => {
                paint_tab_dark(hwnd);
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

/// Gambar tab control gelap: latar & pane BG (menyatu dengan label yang diberi
/// brush BG via `ctlcolor`), item tab HEADER_BG, item terpilih menyatu dengan
/// pane, border pane halus HEADER_SEP.
unsafe fn paint_tab_dark(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    FillRect(hdc, &rc, cached_brush(&BG_BRUSH, BG));

    let font = SendMessageW(hwnd, WM_GETFONT, Some(WPARAM(0)), Some(LPARAM(0))).0;
    let oldf =
        (font != 0).then(|| SelectObject(hdc, HGDIOBJ(font as *mut core::ffi::c_void)));

    let count = SendMessageW(hwnd, TCM_GETITEMCOUNT, Some(WPARAM(0)), Some(LPARAM(0))).0;
    let sel = SendMessageW(hwnd, TCM_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0;

    // Tepi bawah baris tab = tepi atas pane.
    let mut pane_top = rc.top;
    let mut rects: Vec<RECT> = Vec::with_capacity(count.max(0) as usize);
    for i in 0..count {
        let mut ir = RECT::default();
        SendMessageW(
            hwnd,
            TCM_GETITEMRECT,
            Some(WPARAM(i as usize)),
            Some(LPARAM(&mut ir as *mut _ as isize)),
        );
        pane_top = pane_top.max(ir.bottom);
        rects.push(ir);
    }

    // Border pane 1px.
    let sep = CreateSolidBrush(rgb(HEADER_SEP));
    let pane = RECT { left: rc.left, top: pane_top, right: rc.right, bottom: rc.bottom };
    FrameRect(hdc, &pane, sep);

    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, rgb(TEXT));
    for (i, ir) in rects.iter().enumerate() {
        if i as isize == sel {
            // Item terpilih: isi BG hingga menutup border pane di bawahnya
            // (tampak menyatu dengan pane) + garis tepi kiri/atas/kanan.
            let r = RECT { left: ir.left, top: ir.top, right: ir.right, bottom: pane_top + 1 };
            FillRect(hdc, &r, cached_brush(&BG_BRUSH, BG));
            for e in [
                RECT { left: r.left, top: r.top, right: r.left + 1, bottom: pane_top },
                RECT { left: r.left, top: r.top, right: r.right, bottom: r.top + 1 },
                RECT { left: r.right - 1, top: r.top, right: r.right, bottom: pane_top },
            ] {
                FillRect(hdc, &e, sep);
            }
        } else {
            let b = CreateSolidBrush(rgb(HEADER_BG));
            FillRect(hdc, ir, b);
            let _ = DeleteObject(b.into());
        }
        let mut buf = [0u16; 96];
        let mut item = TCITEMW {
            mask: TCIF_TEXT,
            pszText: PWSTR(buf.as_mut_ptr()),
            cchTextMax: buf.len() as i32,
            ..Default::default()
        };
        SendMessageW(
            hwnd,
            TCM_GETITEMW,
            Some(WPARAM(i)),
            Some(LPARAM(&mut item as *mut _ as isize)),
        );
        let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
        let mut wide: Vec<u16> = buf[..len].to_vec();
        let mut tr = *ir;
        DrawTextW(hdc, &mut wide, &mut tr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }
    let _ = DeleteObject(sep.into());

    if let Some(of) = oldf {
        SelectObject(hdc, of);
    }
    let _ = EndPaint(hwnd, &ps);
}

/// Tangani `WM_CTLCOLOR*` (STATIC/EDIT/BTN/LISTBOX/DLG/SCROLLBAR). Kembalikan
/// `Some(brush)` sbg LRESULT bila dark aktif; `None` → biarkan proc pakai default.
/// `wparam` = HDC kontrol.
///
/// # Safety
/// `wparam.0` harus HDC valid (dari pesan `WM_CTLCOLOR*` yang sedang ditangani).
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

/// Style tambahan combobox agar bisa digambar gelap. Combobox `CBS_DROPDOWNLIST`
/// TAK mengirim `WM_CTLCOLOR*` untuk permukaannya (kotak selection) → tetap putih
/// walau anak sudah `DarkMode_Explorer`. Owner-draw membuat kita menggambarnya
/// sendiri. Kembalikan 0 saat tema terang (combobox normal, tanpa owner-draw).
///
/// Pakai saat MEMBUAT combobox: `... | dark::combo_style() ...`, lalu route
/// `WM_DRAWITEM` ke [`draw_combobox`].
pub fn combo_style() -> u32 {
    if is_dark() {
        (CBS_OWNERDRAWFIXED | CBS_HASSTRINGS) as u32
    } else {
        0
    }
}

/// Gambar permukaan + item combobox owner-draw secara gelap. Panggil dari
/// `WM_DRAWITEM`. Kembalikan `Some(LRESULT(1))` bila ditangani (dark + combobox),
/// `None` bila bukan (biar proc pakai default).
///
/// # Safety
/// `lparam` harus pointer `DRAWITEMSTRUCT` valid dari pesan `WM_DRAWITEM`.
pub unsafe fn draw_combobox(lparam: LPARAM) -> Option<LRESULT> {
    if lparam.0 == 0 {
        return None;
    }
    let dis = &*(lparam.0 as *const DRAWITEMSTRUCT);
    if dis.CtlType != ODT_COMBOBOX {
        return None;
    }
    // Combobox owner-draw dibuat saat tema gelap; bila user pindah ke terang
    // selagi dialog terbuka, tetap HARUS digambar (default proc tak menggambar
    // owner-draw → kotak kosong) — pakai warna sistem terang.
    let dark = is_dark();
    let hdc = dis.hDC;
    let rc = dis.rcItem;
    // Item di dropdown yang tersorot → latar sorotan; selain itu latar field.
    let selected = dis.itemState.0 & ODS_SELECTED.0 != 0;
    let (bg_col, txt_col) = if dark {
        (rgb(if selected { SEL_BG } else { EDIT_BG }), rgb(TEXT))
    } else if selected {
        (COLORREF(GetSysColor(COLOR_HIGHLIGHT)), COLORREF(GetSysColor(COLOR_HIGHLIGHTTEXT)))
    } else {
        (COLORREF(GetSysColor(COLOR_WINDOW)), COLORREF(GetSysColor(COLOR_WINDOWTEXT)))
    };
    let brush = CreateSolidBrush(bg_col);
    FillRect(hdc, &rc, brush);
    let _ = DeleteObject(brush.into());
    // itemID == -1 → combobox kosong (belum ada pilihan): cukup latar.
    if dis.itemID as i32 >= 0 {
        // Tanya panjang dulu — CB_GETLBTEXT menyalin SELURUH string tanpa
        // batas; buffer tetap = stack overflow untuk item panjang.
        let len = SendMessageW(
            dis.hwndItem,
            CB_GETLBTEXTLEN,
            Some(WPARAM(dis.itemID as usize)),
            Some(LPARAM(0)),
        )
        .0;
        let mut buf = vec![0u16; len.max(0) as usize + 1];
        SendMessageW(
            dis.hwndItem,
            CB_GETLBTEXT,
            Some(WPARAM(dis.itemID as usize)),
            Some(LPARAM(buf.as_mut_ptr() as isize)),
        );
        let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
        let mut wide: Vec<u16> = buf[..len].to_vec();
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, txt_col);
        let mut tr = RECT { left: rc.left + 5, ..rc };
        DrawTextW(hdc, &mut wide, &mut tr, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
    }
    Some(LRESULT(1))
}

// ID subclass combobox (unik terhadap header subclass).
const COMBO_SUBCLASS_ID: usize = 0x00AD_0002;

/// Subclass combobox agar tombol dropdown ▾ digambar gelap (theme bawaan
/// membiarkannya putih). Aman dipanggil berulang. Dipasang otomatis via
/// [`apply`] untuk tiap combobox anak.
///
/// # Safety
/// `combo` harus handle combobox (`ComboBox`) valid.
pub unsafe fn install_combo(combo: HWND) {
    let _ = SetWindowSubclass(combo, Some(combo_subclass), COMBO_SUBCLASS_ID, 0);
}

unsafe extern "system" fn combo_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _ref: usize,
) -> LRESULT {
    if msg == WM_PAINT && is_dark() {
        // Biarkan combobox menggambar normal (permukaan → WM_DRAWITEM gelap,
        // tombol → theme putih), lalu timpa tombolnya gelap.
        let ret = DefSubclassProc(hwnd, msg, wparam, lparam);
        paint_combo_button(hwnd);
        return ret;
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

/// Overpaint tombol dropdown combobox: kotak gelap + chevron ▾ terang.
unsafe fn paint_combo_button(hwnd: HWND) {
    let hdc = GetDC(Some(hwnd));
    if hdc.0.is_null() {
        return;
    }
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let bw = GetSystemMetrics(SM_CXVSCROLL).max(16);
    let btn = RECT { left: rc.right - bw, top: rc.top, right: rc.right, bottom: rc.bottom };
    let fill = CreateSolidBrush(rgb(EDIT_BG));
    FillRect(hdc, &btn, fill);
    let _ = DeleteObject(fill.into());
    // Chevron bawah (segitiga) di tengah tombol.
    let cx = (btn.left + btn.right) / 2;
    let cy = (btn.top + btn.bottom) / 2;
    let pts = [
        POINT { x: cx - 4, y: cy - 2 },
        POINT { x: cx + 4, y: cy - 2 },
        POINT { x: cx, y: cy + 3 },
    ];
    let brush = CreateSolidBrush(rgb(TEXT));
    let pen = CreatePen(PS_SOLID, 1, rgb(TEXT));
    let ob = SelectObject(hdc, brush.into());
    let op = SelectObject(hdc, pen.into());
    let _ = Polygon(hdc, &pts);
    SelectObject(hdc, ob);
    SelectObject(hdc, op);
    let _ = DeleteObject(brush.into());
    let _ = DeleteObject(pen.into());
    ReleaseDC(Some(hwnd), hdc);
}

/// Isi latar dialog dengan warna gelap saat `WM_ERASEBKGND`. Kembalikan
/// `Some(LRESULT(1))` bila ditangani (dark), `None` bila terang.
///
/// # Safety
/// `hwnd` harus window valid dan `wparam.0` HDC valid (dari `WM_ERASEBKGND`).
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
///
/// # Safety
/// `lv` harus handle ListView (`SysListView32`) valid.
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
        // Minta notifikasi per-item + POSTPAINT. Latar diisi di POSTPAINT (di
        // PREPAINT akan tertimpa cat default header → area kosong tetap putih).
        return Some(LRESULT((CDRF_NOTIFYITEMDRAW | CDRF_NOTIFYPOSTPAINT) as isize));
    }
    if stage == CDDS_POSTPAINT {
        // Isi area kosong di kanan kolom terakhir (di luar semua item) dgn warna
        // header → tak tertinggal putih. Cari tepi kanan terjauh dari item.
        let hdr = cd.hdr.hwndFrom;
        let count = SendMessageW(hdr, HDM_GETITEMCOUNT, Some(WPARAM(0)), Some(LPARAM(0))).0 as i32;
        let mut max_right = 0i32;
        for i in 0..count {
            let mut ir = RECT::default();
            SendMessageW(hdr, HDM_GETITEMRECT, Some(WPARAM(i as usize)),
                Some(LPARAM(&mut ir as *mut _ as isize)));
            max_right = max_right.max(ir.right);
        }
        if max_right < cd.rc.right {
            let filler = RECT { left: max_right, top: cd.rc.top, right: cd.rc.right, bottom: cd.rc.bottom };
            let bg = CreateSolidBrush(rgb(HEADER_BG));
            FillRect(cd.hdc, &filler, bg);
            let _ = DeleteObject(bg.into());
        }
        return Some(LRESULT(CDRF_DODEFAULT as isize));
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
    let idx = cd.dwItemSpec;
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
