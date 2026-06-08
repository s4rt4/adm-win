//! Scheduler (plan §9.15): jalankan/hentikan antrian otomatis pada jam & hari
//! tertentu. State global + thread pemeriksa waktu lokal tiap 20 detik +
//! jendela pengaturan modal.

use crate::engine::EngineHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone)]
pub struct Schedule {
    pub enabled: bool,
    pub start: (u8, u8),
    pub stop: (u8, u8),
    /// index 0=Minggu .. 6=Sabtu (cocok dgn SYSTEMTIME.wDayOfWeek).
    pub days: [bool; 7],
}

static SCHEDULE: Mutex<Schedule> = Mutex::new(Schedule {
    enabled: false,
    start: (9, 0),
    stop: (18, 0),
    days: [true, true, true, true, true, true, true],
});

pub fn get() -> Schedule {
    SCHEDULE.lock().unwrap().clone()
}

fn set(s: Schedule) {
    *SCHEDULE.lock().unwrap() = s;
}

fn local_now() -> (u16, u32) {
    let st = unsafe { GetLocalTime() };
    (st.wDayOfWeek, st.wHour as u32 * 60 + st.wMinute as u32)
}

/// Thread pemicu: cek tiap 20 detik, edge-trigger start/stop queue.
pub fn start(engine: EngineHandle) {
    std::thread::Builder::new()
        .name("adm-scheduler".into())
        .spawn(move || {
            let mut was_active = false;
            loop {
                std::thread::sleep(Duration::from_secs(20));
                let s = get();
                if !s.enabled {
                    was_active = false;
                    continue;
                }
                let (dow, now) = local_now();
                let active = if s.days[dow as usize] {
                    let start = s.start.0 as u32 * 60 + s.start.1 as u32;
                    let stop = s.stop.0 as u32 * 60 + s.stop.1 as u32;
                    if start <= stop {
                        now >= start && now < stop
                    } else {
                        now >= start || now < stop // melewati tengah malam
                    }
                } else {
                    false
                };
                if active && !was_active {
                    engine.start_queue();
                } else if !active && was_active {
                    engine.stop_queue();
                }
                was_active = active;
            }
        })
        .expect("spawn scheduler");
}

// ============================ Dialog ============================

const CLASS: PCWSTR = w!("AdmSchedulerDialog");
static REGISTERED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);
static SAVED: AtomicBool = AtomicBool::new(false);

const ID_ENABLE: usize = 1;
const ID_START_H: usize = 2;
const ID_START_M: usize = 3;
const ID_STOP_H: usize = 4;
const ID_STOP_M: usize = 5;
const ID_DAY0: usize = 10; // .. ID_DAY0+6
const ID_OK: usize = 20;
const ID_CANCEL: usize = 21;

// Handle kontrol (dialog modal tunggal → global aman).
static CTRL: Mutex<[isize; 12]> = Mutex::new([0; 12]);
// indeks: 0 enable, 1 start_h, 2 start_m, 3 stop_h, 4 stop_m, 5..=11 day0..6

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

fn set_int(h: HWND, v: u32) {
    let s = HSTRING::from(v.to_string());
    unsafe {
        let _ = SetWindowTextW(h, PCWSTR(s.as_ptr()));
    }
}
unsafe fn get_int(h: HWND) -> u32 {
    let len = GetWindowTextLengthW(h);
    if len <= 0 {
        return 0;
    }
    let mut buf = vec![0u16; len as usize + 1];
    let n = GetWindowTextW(h, &mut buf);
    String::from_utf16_lossy(&buf[..n as usize]).trim().parse().unwrap_or(0)
}
fn set_check(h: HWND, on: bool) {
    unsafe {
        SendMessageW(h, BM_SETCHECK, Some(WPARAM(if on { 1 } else { 0 })), Some(LPARAM(0)));
    }
}
fn get_check(h: HWND) -> bool {
    unsafe { SendMessageW(h, BM_GETCHECK, Some(WPARAM(0)), Some(LPARAM(0))).0 == 1 }
}

/// Tampilkan dialog Scheduler modal.
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
        SAVED.store(false, Ordering::SeqCst);

        let mut pr = RECT::default();
        let _ = GetWindowRect(parent, &mut pr);
        let (dw, dh) = (380, 280);
        let x = (pr.left + ((pr.right - pr.left) - dw) / 2).max(0);
        let y = (pr.top + ((pr.bottom - pr.top) - dh) / 2).max(0);

        let dlg = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            CLASS,
            w!("Scheduler"),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            x, y, dw, dh,
            Some(parent),
            None,
            Some(instance),
            None,
        );
        let Ok(dlg) = dlg else { return };

        let s = get();
        let chk = mk(dlg, w!("BUTTON"), w!("Enable scheduler"), WINDOW_STYLE(BS_AUTOCHECKBOX as u32), 16, 14, 200, 20, ID_ENABLE);
        set_check(chk, s.enabled);
        set_ctrl(0, chk);

        let _ = mk(dlg, w!("STATIC"), w!("Start at (HH:MM):"), WINDOW_STYLE(0), 16, 48, 110, 18, 0);
        let sh = mk(dlg, w!("EDIT"), w!(""), WINDOW_STYLE(WS_BORDER.0 | ES_NUMBER as u32), 130, 46, 40, 22, ID_START_H);
        let sm = mk(dlg, w!("EDIT"), w!(""), WINDOW_STYLE(WS_BORDER.0 | ES_NUMBER as u32), 178, 46, 40, 22, ID_START_M);
        set_int(sh, s.start.0 as u32);
        set_int(sm, s.start.1 as u32);
        set_ctrl(1, sh);
        set_ctrl(2, sm);

        let _ = mk(dlg, w!("STATIC"), w!("Stop at (HH:MM):"), WINDOW_STYLE(0), 16, 78, 110, 18, 0);
        let eh = mk(dlg, w!("EDIT"), w!(""), WINDOW_STYLE(WS_BORDER.0 | ES_NUMBER as u32), 130, 76, 40, 22, ID_STOP_H);
        let em = mk(dlg, w!("EDIT"), w!(""), WINDOW_STYLE(WS_BORDER.0 | ES_NUMBER as u32), 178, 76, 40, 22, ID_STOP_M);
        set_int(eh, s.stop.0 as u32);
        set_int(em, s.stop.1 as u32);
        set_ctrl(3, eh);
        set_ctrl(4, em);

        let _ = mk(dlg, w!("STATIC"), w!("Days:"), WINDOW_STYLE(0), 16, 112, 60, 18, 0);
        let labels = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        for (i, lbl) in labels.iter().enumerate() {
            let hl = HSTRING::from(*lbl);
            let cx = 16 + (i as i32 % 4) * 88;
            let cy = 134 + (i as i32 / 4) * 26;
            let d = mk(dlg, w!("BUTTON"), PCWSTR(hl.as_ptr()), WINDOW_STYLE(BS_AUTOCHECKBOX as u32), cx, cy, 80, 20, ID_DAY0 + i);
            set_check(d, s.days[i]);
            set_ctrl(5 + i, d);
        }

        let _ = mk(dlg, w!("BUTTON"), w!("OK"), WINDOW_STYLE(WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32), 180, 210, 80, 28, ID_OK);
        let _ = mk(dlg, w!("BUTTON"), w!("Cancel"), WINDOW_STYLE(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), 272, 210, 80, 28, ID_CANCEL);

        let _ = EnableWindow(parent, false);
        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetForegroundWindow(dlg);

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

        if SAVED.load(Ordering::SeqCst) {
            let clamp_h = |v: u32| (v.min(23)) as u8;
            let clamp_m = |v: u32| (v.min(59)) as u8;
            let mut days = [false; 7];
            for (i, d) in days.iter_mut().enumerate() {
                *d = get_check(ctrl(5 + i));
            }
            set(Schedule {
                enabled: get_check(ctrl(0)),
                start: (clamp_h(get_int(ctrl(1))), clamp_m(get_int(ctrl(2)))),
                stop: (clamp_h(get_int(ctrl(3))), clamp_m(get_int(ctrl(4))),),
                days,
            });
        }
    }
}

extern "system" fn proc_(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                match wparam.0 & 0xFFFF {
                    ID_OK => {
                        SAVED.store(true, Ordering::SeqCst);
                        DONE.store(true, Ordering::SeqCst);
                        let _ = DestroyWindow(hwnd);
                    }
                    ID_CANCEL => {
                        DONE.store(true, Ordering::SeqCst);
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CTLCOLORSTATIC => {
                SetBkMode(HDC(wparam.0 as *mut _), TRANSPARENT);
                LRESULT(GetSysColorBrush(COLOR_BTNFACE).0 as isize)
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
