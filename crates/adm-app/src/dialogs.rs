//! Dialog "Add new download / Download File Info" (plan §9.10). WM3: field
//! inti (URL, Category, Save As) + Start/Cancel, modal. Refinement (remember
//! path, preview, ikon tipe) menyusul.

use adm_ipc::DownloadAddParams;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const IDOK: usize = 1;
const IDCANCEL: usize = 2;
const IDLATER: usize = 3;

static REGISTERED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);
static START_NOW: AtomicBool = AtomicBool::new(true);
static RESULT: Mutex<Option<DownloadAddParams>> = Mutex::new(None);
/// Hasil probe (ukuran + nama dari Content-Disposition) dikirim ke dialog via
/// lparam = `Box<ProbeOut>` (di-`from_raw` sekali di handler).
const WM_SIZE_RESULT: u32 = WM_APP + 10;
static SIZE_LABEL: Mutex<isize> = Mutex::new(0);
// HWND edit (sebagai isize) — dialog modal tunggal, jadi global aman.
static URL_EDIT: Mutex<isize> = Mutex::new(0);
static SAVE_EDIT: Mutex<isize> = Mutex::new(0);
/// Static SS_ICON penampil ikon tipe berkas (ala IDM) + HICON aktif (di-destroy
/// saat ganti/tutup; HICON dari SHGFI_ICON wajib `DestroyIcon`).
static ICON_CTRL: Mutex<isize> = Mutex::new(0);
static ICON_HANDLE: Mutex<isize> = Mutex::new(0);
/// Header titipan browser (referrer/UA/cookie) untuk probe ber-autentikasi —
/// agar nama & ukuran di dialog sama dgn yang nanti dipakai engine. Diisi saat
/// dialog dibuka, dikosongkan saat tutup.
static PROBE_HEADERS: Mutex<Option<adm_core::ReqHeaders>> = Mutex::new(None);

/// Payload hasil probe async yang dikirim balik ke dialog.
struct ProbeOut {
    total: u64,
    /// Nama dari `Content-Disposition` (sudah basename), bila ada.
    filename: Option<String>,
}

const CLASS: PCWSTR = w!("AdmAddDialog");

fn gui_font() -> HGDIOBJ {
    unsafe { GetStockObject(DEFAULT_GUI_FONT) }
}

unsafe fn set_font(hwnd: HWND) {
    SendMessageW(
        hwnd,
        WM_SETFONT,
        Some(WPARAM(gui_font().0 as usize)),
        Some(LPARAM(1)),
    );
}

#[allow(clippy::too_many_arguments)]
unsafe fn make_child(
    parent: HWND,
    class: PCWSTR,
    text: PCWSTR,
    style: WINDOW_STYLE,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: usize,
    instance: HINSTANCE,
) -> HWND {
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class,
        text,
        style | WS_CHILD | WS_VISIBLE,
        x,
        y,
        w,
        h,
        Some(parent),
        Some(HMENU(id as *mut core::ffi::c_void)),
        Some(instance),
        None,
    )
    .unwrap_or_default();
    set_font(hwnd);
    hwnd
}

fn read_text(slot: &Mutex<isize>) -> String {
    let h = *slot.lock().unwrap();
    if h == 0 {
        return String::new();
    }
    window_text(HWND(h as *mut core::ffi::c_void))
}

fn window_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

/// Basename (komponen terakhir setelah `\` atau `/`).
fn basename(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or(path).trim().to_string()
}

/// Ganti komponen nama berkas pada `full`, pertahankan direktorinya.
fn replace_basename(full: &str, new_name: &str) -> String {
    match full.rfind(['\\', '/']) {
        Some(i) => format!("{}{}", &full[..=i], new_name),
        None => new_name.to_string(),
    }
}

/// Dialog "Add new download" (manual): URL + Category + Size. TANPA field nama
/// file — engine menamai otomatis (Content-Disposition → basename URL).
pub fn add_url_dialog(parent: HWND, default_url: &str) -> Option<(DownloadAddParams, bool)> {
    dialog_impl(parent, default_url, None, Path::new(""), false, adm_core::ReqHeaders::default())
}

/// Dialog "Download File Info" (download start dari browser): seperti di atas
/// PLUS field "Save As" dengan nama file terdeteksi otomatis. `headers` =
/// titipan browser agar probe nama/ukuran cocok dgn unduhan ber-autentikasi.
pub fn download_info_dialog(
    parent: HWND,
    default_url: &str,
    default_filename: Option<&str>,
    download_dir: &Path,
    headers: adm_core::ReqHeaders,
) -> Option<(DownloadAddParams, bool)> {
    dialog_impl(parent, default_url, default_filename, download_dir, true, headers)
}

/// Implementasi bersama. `with_filename` menentukan apakah field "Save As" ada.
fn dialog_impl(
    parent: HWND,
    default_url: &str,
    default_filename: Option<&str>,
    download_dir: &Path,
    with_filename: bool,
    headers: adm_core::ReqHeaders,
) -> Option<(DownloadAddParams, bool)> {
    unsafe {
        *PROBE_HEADERS.lock().unwrap() = Some(headers);
        let instance: HINSTANCE = GetModuleHandleW(None).ok()?.into();

        if !REGISTERED.swap(true, Ordering::SeqCst) {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(dlg_proc),
                hInstance: instance,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as *mut core::ffi::c_void),
                lpszClassName: CLASS,
                ..Default::default()
            };
            RegisterClassW(&wc);
        }

        DONE.store(false, Ordering::SeqCst);
        *RESULT.lock().unwrap() = None;
        *SAVE_EDIT.lock().unwrap() = 0; // 0 = tak ada field nama file (mode manual)
        *ICON_CTRL.lock().unwrap() = 0;
        *ICON_HANDLE.lock().unwrap() = 0;

        // Ukuran CLIENT diinginkan; window dibesarkan agar client = ini
        // (kontrol kanan tak kepotong / mepet).
        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        let client_h = if with_filename { 250 } else { 208 };
        let mut rcsz = RECT { left: 0, top: 0, right: 568, bottom: client_h };
        let _ = AdjustWindowRectEx(&mut rcsz, style, false, WS_EX_DLGMODALFRAME);
        let (dw, dh) = (rcsz.right - rcsz.left, rcsz.bottom - rcsz.top);

        // Pusatkan di AREA KERJA MONITOR (bukan rect parent — saat dipicu browser,
        // jendela utama bisa minimized/tersembunyi → koordinatnya di luar layar).
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let mon = MonitorFromWindow(parent, MONITOR_DEFAULTTONEAREST);
        let area = if GetMonitorInfoW(mon, &mut mi).as_bool() {
            mi.rcWork
        } else {
            RECT { left: 0, top: 0, right: 1920, bottom: 1080 }
        };
        let x = area.left + ((area.right - area.left) - dw) / 2;
        let y = area.top + ((area.bottom - area.top) - dh) / 2;

        let title = if with_filename { w!("Download File Info") } else { w!("Add new download") };
        let dlg = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            CLASS,
            title,
            style,
            x.max(0),
            y.max(0),
            dw,
            dh,
            Some(parent),
            None,
            Some(instance),
            None,
        )
        .ok()?;

        // Kontrol anak.
        let _ = make_child(dlg, w!("STATIC"), w!("URL:"), WINDOW_STYLE(0), 16, 18, 60, 18, 0, instance);
        let url = make_child(
            dlg, w!("EDIT"), PCWSTR::null(),
            WINDOW_STYLE((WS_BORDER.0 | WS_TABSTOP.0) | ES_AUTOHSCROLL as u32),
            84, 16, 400, 22, 100, instance,
        );
        *URL_EDIT.lock().unwrap() = url.0 as isize;
        if !default_url.is_empty() {
            let h = HSTRING::from(default_url);
            let _ = SetWindowTextW(url, PCWSTR(h.as_ptr()));
        }

        let _ = make_child(dlg, w!("STATIC"), w!("Category:"), WINDOW_STYLE(0), 16, 56, 60, 18, 0, instance);
        let combo = make_child(
            dlg, w!("COMBOBOX"), PCWSTR::null(),
            WINDOW_STYLE(WS_TABSTOP.0 | CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0),
            84, 54, 200, 200, 101, instance,
        );
        for c in ["General", "Compressed", "Documents", "Music", "Programs", "Video"] {
            let h = HSTRING::from(c);
            SendMessageW(combo, CB_ADDSTRING, Some(WPARAM(0)), Some(LPARAM(h.as_ptr() as isize)));
        }
        SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(0)), Some(LPARAM(0)));

        // Field "Save As" hanya untuk download start (browser).
        let size_y = if with_filename {
            let _ = make_child(dlg, w!("STATIC"), w!("Save As:"), WINDOW_STYLE(0), 16, 94, 60, 18, 0, instance);
            let save = make_child(
                dlg, w!("EDIT"), PCWSTR::null(),
                WINDOW_STYLE((WS_BORDER.0 | WS_TABSTOP.0) | ES_AUTOHSCROLL as u32),
                84, 92, 400, 22, 102, instance,
            );
            *SAVE_EDIT.lock().unwrap() = save.0 as isize;
            // Prefill: <download_dir>\<nama dari browser / tebakan url>
            let base = default_filename
                .map(|s| s.to_string())
                .unwrap_or_else(|| guess_filename(default_url));
            let initial = download_dir.join(&base);
            let h = HSTRING::from(initial.to_string_lossy().into_owned());
            let _ = SetWindowTextW(save, PCWSTR(h.as_ptr()));

            // Ikon tipe berkas (ala IDM) di kanan; SS_ICON|SS_CENTERIMAGE (0x203).
            let icon_ctrl = make_child(
                dlg, w!("STATIC"), PCWSTR::null(),
                WINDOW_STYLE(0x0000_0203), 500, 44, 48, 48, 0, instance,
            );
            *ICON_CTRL.lock().unwrap() = icon_ctrl.0 as isize;
            set_type_icon(&base);
            126
        } else {
            94 // tanpa Save As → Size naik ke baris itu
        };

        // Readout ukuran (di-probe async; juga saat URL diisi/diubah).
        let _ = make_child(dlg, w!("STATIC"), w!("Size:"), WINDOW_STYLE(0), 16, size_y, 50, 18, 0, instance);
        let size_lbl = make_child(dlg, w!("STATIC"), w!("\u{2026}"), WINDOW_STYLE(0), 70, size_y, 200, 18, 0, instance);
        *SIZE_LABEL.lock().unwrap() = size_lbl.0 as isize;
        if !default_url.is_empty() {
            spawn_size_probe(dlg, default_url);
        }

        let btn_y = size_y + 34;
        let _ = make_child(
            dlg, w!("BUTTON"), w!("Download Later"),
            WINDOW_STYLE(WS_TABSTOP.0 | BS_PUSHBUTTON as u32),
            120, btn_y, 120, 30, IDLATER, instance,
        );
        let _ = make_child(
            dlg, w!("BUTTON"), w!("Start Download"),
            WINDOW_STYLE(WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32),
            250, btn_y, 140, 30, IDOK, instance,
        );
        let _ = make_child(
            dlg, w!("BUTTON"), w!("Cancel"),
            WINDOW_STYLE(WS_TABSTOP.0 | BS_PUSHBUTTON as u32),
            400, btn_y, 100, 30, IDCANCEL, instance,
        );

        crate::dark::apply(dlg);
        let _ = EnableWindow(parent, false);
        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetForegroundWindow(dlg);

        // Loop modal.
        let mut msg = MSG::default();
        while !DONE.load(Ordering::SeqCst) && GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if !IsDialogMessageW(dlg, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        let _ = EnableWindow(parent, true);
        let _ = SetForegroundWindow(parent);
        if IsWindow(Some(dlg)).as_bool() {
            let _ = DestroyWindow(dlg);
        }

        *PROBE_HEADERS.lock().unwrap() = None;
        {
            let mut slot = ICON_HANDLE.lock().unwrap();
            if *slot != 0 {
                let _ = DestroyIcon(HICON(*slot as *mut core::ffi::c_void));
                *slot = 0;
            }
            *ICON_CTRL.lock().unwrap() = 0;
        }
        RESULT
            .lock()
            .unwrap()
            .take()
            .map(|p| (p, START_NOW.load(Ordering::SeqCst)))
    }
}

extern "system" fn dlg_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let id = wparam.0 & 0xFFFF;
                let code = (wparam.0 >> 16) & 0xFFFF;
                // URL diisi/diubah lalu kehilangan fokus → probe ukuran.
                if id == 100 && code == EN_KILLFOCUS as usize {
                    let url = read_text(&URL_EDIT);
                    let url = url.trim();
                    if url.starts_with("http://") || url.starts_with("https://") {
                        spawn_size_probe(hwnd, url);
                    }
                    return LRESULT(0);
                }
                // User mengetik nama di "Save As" → ikon tipe ikut berubah live.
                if id == 102 && code == EN_CHANGE as usize {
                    let base = basename(&read_text(&SAVE_EDIT));
                    set_type_icon(&base);
                    return LRESULT(0);
                }
                match id {
                    IDOK | IDLATER => {
                        let url = read_text(&URL_EDIT);
                        if !url.trim().is_empty() {
                            // filename hanya bila field "Save As" ada (mode browser);
                            // mode manual → None, engine menamai otomatis.
                            let filename = if *SAVE_EDIT.lock().unwrap() != 0 {
                                read_text(&SAVE_EDIT)
                                    .rsplit(['\\', '/'])
                                    .next()
                                    .filter(|s| !s.is_empty())
                                    .map(|s| s.to_string())
                            } else {
                                None
                            };
                            *RESULT.lock().unwrap() = Some(DownloadAddParams {
                                url: url.trim().to_string(),
                                filename,
                                ..Default::default()
                            });
                            START_NOW.store(id == IDOK, Ordering::SeqCst);
                        }
                        DONE.store(true, Ordering::SeqCst);
                        let _ = DestroyWindow(hwnd);
                        LRESULT(0)
                    }
                    IDCANCEL => {
                        DONE.store(true, Ordering::SeqCst);
                        let _ = DestroyWindow(hwnd);
                        LRESULT(0)
                    }
                    _ => DefWindowProcW(hwnd, msg, wparam, lparam),
                }
            }
            WM_SIZE_RESULT => {
                if lparam.0 == 0 {
                    return LRESULT(0);
                }
                let out = *Box::from_raw(lparam.0 as *mut ProbeOut);
                // Ukuran.
                let h = *SIZE_LABEL.lock().unwrap();
                if h != 0 {
                    let txt = if out.total > 0 { fmt_size(out.total) } else { "Unknown".to_string() };
                    let hs = HSTRING::from(txt);
                    let _ = SetWindowTextW(HWND(h as *mut core::ffi::c_void), PCWSTR(hs.as_ptr()));
                }
                // Nama dari Content-Disposition → perbarui "Save As" (pertahankan
                // folder). Hanya bila nama server valid & nama saat ini masih
                // generik — selaras dgn prioritas `engine::resolve_filename`
                // (nama eksplisit non-generik menang atas Content-Disposition),
                // sekaligus tak menimpa editan manual user.
                if let Some(name) = out.filename {
                    let name = crate::engine::sanitize(&name);
                    let se = *SAVE_EDIT.lock().unwrap();
                    if !name.is_empty() && !crate::engine::looks_generic(&name) && se != 0 {
                        let edit = HWND(se as *mut core::ffi::c_void);
                        let cur = window_text(edit);
                        let cur_base = basename(&cur);
                        if cur_base.is_empty() || crate::engine::looks_generic(&cur_base) {
                            let newpath = replace_basename(&cur, &name);
                            let hs = HSTRING::from(newpath);
                            let _ = SetWindowTextW(edit, PCWSTR(hs.as_ptr()));
                            set_type_icon(&name);
                        }
                    }
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                DONE.store(true, Ordering::SeqCst);
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORBTN | WM_CTLCOLORLISTBOX => {
                if let Some(r) = crate::dark::ctlcolor(msg, wparam) {
                    return r;
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_ERASEBKGND => {
                if let Some(r) = crate::dark::erasebkgnd(hwnd, wparam) {
                    return r;
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Probe ukuran + nama (Content-Disposition) async; hasil dikirim balik ke
/// dialog via WM_SIZE_RESULT (lparam = `Box<ProbeOut>`). Pakai header titipan
/// browser agar cocok dgn unduhan ber-autentikasi & sama dgn pilihan engine.
unsafe fn spawn_size_probe(dlg: HWND, url: &str) {
    if let Some(eng) = crate::gui::engine() {
        let url = url.to_string();
        let dlg_isize = dlg.0 as isize;
        let headers = PROBE_HEADERS.lock().unwrap().clone().unwrap_or_default();
        eng.runtime().spawn(async move {
            let probe = adm_core::probe_url_with(&url, &headers, false).await.ok();
            let total = probe.as_ref().and_then(|p| p.total).unwrap_or(0);
            let filename = probe.and_then(|p| p.suggested_filename);
            let payload = Box::into_raw(Box::new(ProbeOut { total, filename }));
            let dlg = HWND(dlg_isize as *mut core::ffi::c_void);
            let posted = PostMessageW(Some(dlg), WM_SIZE_RESULT, WPARAM(0), LPARAM(payload as isize)).is_ok();
            if !posted {
                // Dialog sudah tutup → reklamasi box agar tak bocor.
                drop(Box::from_raw(payload));
            }
        });
    }
}

fn fmt_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GB", b / (1024.0 * 1024.0 * 1024.0))
    } else if b >= 1024.0 * 1024.0 {
        format!("{:.2} MB", b / (1024.0 * 1024.0))
    } else if b >= 1024.0 {
        format!("{:.2} KB", b / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// HICON tipe berkas (besar, ~32px) dari shell via ekstensi nama — tak perlu
/// berkasnya ada. Pemanggil wajib `DestroyIcon`.
unsafe fn file_large_icon(name: &str) -> HICON {
    let h = HSTRING::from(name);
    let mut shfi = SHFILEINFOW::default();
    let attr = windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0x80); // NORMAL
    SHGetFileInfoW(
        PCWSTR(h.as_ptr()),
        attr,
        Some(&mut shfi),
        std::mem::size_of::<SHFILEINFOW>() as u32,
        SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
    );
    shfi.hIcon
}

/// Perbarui ikon tipe berkas pada dialog dari `name`; destroy ikon sebelumnya.
unsafe fn set_type_icon(name: &str) {
    let ctrl = *ICON_CTRL.lock().unwrap();
    if ctrl == 0 || name.is_empty() {
        return;
    }
    let icon = file_large_icon(name);
    if icon.is_invalid() {
        return;
    }
    let hctrl = HWND(ctrl as *mut core::ffi::c_void);
    SendMessageW(hctrl, STM_SETICON, Some(WPARAM(icon.0 as usize)), Some(LPARAM(0)));
    let mut slot = ICON_HANDLE.lock().unwrap();
    if *slot != 0 {
        let _ = DestroyIcon(HICON(*slot as *mut core::ffi::c_void));
    }
    *slot = icon.0 as isize;
}

fn guess_filename(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let seg = path
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("download.bin");
    adm_core::percent_decode(seg)
}
