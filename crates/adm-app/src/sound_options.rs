//! Sub-dialog Options → Sounds...: per event notifikasi ada checkbox aktif,
//! path file suara (kosong = bawaan), Browse, dan Test. OK langsung
//! menyimpan ke `settings` (terlepas dari OK/Cancel dialog Options induknya).

use crate::settings;
use crate::sound::Event;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::EM_SETCUEBANNER;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

const CLASS: PCWSTR = w!("AdmSoundsDialog");
static REGISTERED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

const N: usize = Event::ALL.len();
const ID_CHECK: usize = 100; // + indeks event
const ID_FILE: usize = 110;
const ID_BROWSE: usize = 120;
const ID_TEST: usize = 130;
const ID_OK: usize = 20;
const ID_CANCEL: usize = 21;

/// HWND checkbox & edit per event: [0..N) checkbox, [N..2N) edit.
static CTRL: Mutex<[isize; 2 * N]> = Mutex::new([0; 2 * N]);
/// Nilai yang dibaca handler OK sebelum DestroyWindow (enabled, file).
static PENDING: Mutex<Option<Vec<(bool, String)>>> = Mutex::new(None);

fn ctrl(i: usize) -> HWND {
    HWND(CTRL.lock().unwrap()[i] as *mut core::ffi::c_void)
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
    SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(GetStockObject(DEFAULT_GUI_FONT).0 as usize)), Some(LPARAM(1)));
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

pub fn show(parent: HWND) {
    unsafe {
        let instance: HINSTANCE = match GetModuleHandleW(None) {
            Ok(h) => h.into(),
            Err(_) => return,
        };
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
        *PENDING.lock().unwrap() = None;

        const M: i32 = 20;
        const ROW: i32 = 64;
        let rows_bottom = 16 + ROW * N as i32;

        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        let mut rc = RECT { left: 0, top: 0, right: 460, bottom: rows_bottom + 78 };
        let _ = AdjustWindowRectEx(&mut rc, style, false, WS_EX_DLGMODALFRAME);
        let (dw, dh) = (rc.right - rc.left, rc.bottom - rc.top);
        let mut pr = RECT::default();
        let _ = GetWindowRect(parent, &mut pr);
        let x = (pr.left + ((pr.right - pr.left) - dw) / 2).max(0);
        let y = (pr.top + ((pr.bottom - pr.top) - dh) / 2).max(0);

        let Ok(dlg) = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            CLASS,
            w!("Sounds"),
            style,
            x, y, dw, dh,
            Some(parent),
            None,
            Some(instance),
            None,
        ) else {
            return;
        };

        let cfg = settings::get();
        let cue = w!("(built-in sound)");
        for (i, ev) in Event::ALL.iter().enumerate() {
            let p = ev.pref(&cfg);
            let y0 = 16 + ROW * i as i32;
            let label = HSTRING::from(ev.label());
            let c = mk(dlg, w!("BUTTON"), PCWSTR(label.as_ptr()), WINDOW_STYLE(WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32), M, y0, 300, 20, ID_CHECK + i);
            SendMessageW(c, BM_SETCHECK, Some(WPARAM(p.enabled as usize)), Some(LPARAM(0)));
            let e = mk(dlg, w!("EDIT"), PCWSTR::null(), WINDOW_STYLE(WS_BORDER.0 | WS_TABSTOP.0 | ES_AUTOHSCROLL as u32), M, y0 + 24, 250, 24, ID_FILE + i);
            set_text(e, p.file.as_deref().unwrap_or(""));
            SendMessageW(e, EM_SETCUEBANNER, Some(WPARAM(0)), Some(LPARAM(cue.as_ptr() as isize)));
            let _ = mk(dlg, w!("BUTTON"), w!("Browse..."), WINDOW_STYLE(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), M + 256, y0 + 23, 80, 26, ID_BROWSE + i);
            let _ = mk(dlg, w!("BUTTON"), w!("Test"), WINDOW_STYLE(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), M + 342, y0 + 23, 78, 26, ID_TEST + i);
            let mut h = CTRL.lock().unwrap();
            h[i] = c.0 as isize;
            h[N + i] = e.0 as isize;
        }

        let _ = mk(dlg, w!("STATIC"), w!("WAV or MP3. Leave the path empty to use the built-in sound."), WINDOW_STYLE(0), M, rows_bottom, 420, 16, 0);
        let _ = mk(dlg, w!("BUTTON"), w!("OK"), WINDOW_STYLE(WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32), 264, rows_bottom + 32, 84, 30, ID_OK);
        let _ = mk(dlg, w!("BUTTON"), w!("Cancel"), WINDOW_STYLE(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), 356, rows_bottom + 32, 84, 30, ID_CANCEL);

        crate::dark::apply(dlg);
        let _ = EnableWindow(parent, false);
        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetForegroundWindow(dlg);

        let mut msg = MSG::default();
        let _modal = crate::state::ModalGuard::new();
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
        if IsWindow(Some(dlg)).as_bool() {
            let _ = DestroyWindow(dlg);
        }

        if let Some(vals) = PENDING.lock().unwrap().take() {
            settings::update(|s| {
                for (ev, (enabled, file)) in Event::ALL.iter().zip(vals) {
                    let p = ev.pref_mut(s);
                    p.enabled = enabled;
                    p.file = if file.is_empty() { None } else { Some(file) };
                }
            });
        }
    }
}

extern "system" fn proc_(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let id = wparam.0 & 0xFFFF;
                match id {
                    ID_OK => {
                        let vals = (0..N)
                            .map(|i| {
                                let on = SendMessageW(ctrl(i), BM_GETCHECK, Some(WPARAM(0)), Some(LPARAM(0))).0 == 1;
                                (on, get_text(ctrl(N + i)).trim().to_string())
                            })
                            .collect();
                        *PENDING.lock().unwrap() = Some(vals);
                        DONE.store(true, Ordering::SeqCst);
                        let _ = DestroyWindow(hwnd);
                    }
                    ID_CANCEL => {
                        DONE.store(true, Ordering::SeqCst);
                        let _ = DestroyWindow(hwnd);
                    }
                    _ if (ID_BROWSE..ID_BROWSE + N).contains(&id) => {
                        let i = id - ID_BROWSE;
                        if let Some(p) = crate::tasks::pick_sound(hwnd, "Pilih file suara") {
                            set_text(ctrl(N + i), &p.to_string_lossy());
                        }
                    }
                    _ if (ID_TEST..ID_TEST + N).contains(&id) => {
                        let i = id - ID_TEST;
                        crate::sound::preview(Event::ALL[i], Some(&get_text(ctrl(N + i))));
                    }
                    _ => {}
                }
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
                DONE.store(true, Ordering::SeqCst);
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
