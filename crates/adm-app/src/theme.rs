//! Resolusi tema (plan §12). System dibaca dari registry Windows.

use windows::core::{w, PCSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
    REG_VALUE_TYPE,
};

/// Apakah Windows sedang memakai tema gelap (AppsUseLightTheme == 0).
#[allow(dead_code)] // dipakai lagi saat dark mode dirombak
pub fn system_is_dark() -> bool {
    unsafe {
        let mut hkey = HKEY::default();
        let opened = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            Some(0),
            KEY_QUERY_VALUE,
            &mut hkey,
        );
        if opened != ERROR_SUCCESS {
            return false;
        }
        let mut data: u32 = 1;
        let mut size = 4u32;
        let mut ty = REG_VALUE_TYPE(0);
        let rc = RegQueryValueExW(
            hkey,
            w!("AppsUseLightTheme"),
            None,
            Some(&mut ty),
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(hkey);
        rc == ERROR_SUCCESS && data == 0
    }
}

/// Aktifkan/non-aktifkan mode gelap untuk popup menu (best-effort, uxtheme).
/// Memakai ordinal tak-terdokumentasi 135 (SetPreferredAppMode) & 136
/// (FlushMenuThemes) yang lazim dipakai aplikasi dark-mode Win32. Aman gagal.
pub fn set_dark_menus(dark: bool) {
    unsafe {
        let Ok(h) = LoadLibraryW(w!("uxtheme.dll")) else {
            return;
        };
        if let Some(p) = GetProcAddress(h, PCSTR(135 as *const u8)) {
            // SetPreferredAppMode(mode): 0=Default,1=AllowDark,2=ForceDark,3=ForceLight
            let f: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(p);
            f(if dark { 2 } else { 3 });
        }
        if let Some(p) = GetProcAddress(h, PCSTR(136 as *const u8)) {
            // FlushMenuThemes()
            let f: unsafe extern "system" fn() = std::mem::transmute(p);
            f();
        }
    }
}

/// Tema efektif. **Dark mode dinonaktifkan sementara** (rendering bermasalah —
/// akan dirombak nanti); selalu Light agar UI stabil.
pub fn effective_dark(_theme_setting: u8) -> bool {
    false
}
