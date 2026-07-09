//! Autostart via `HKCU\...\Run` (plan §3, §12). Toggle dari tray.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SAM_FLAGS, REG_SZ,
};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: PCWSTR = w!("ADM");

fn open(access: REG_SAM_FLAGS) -> Option<HKEY> {
    let mut hkey = HKEY::default();
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, Some(0), access, &mut hkey) };
    if rc == ERROR_SUCCESS {
        Some(hkey)
    } else {
        None
    }
}

/// Perintah yang diharapkan di value Run untuk exe saat ini.
fn expected_cmd() -> String {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("\"{exe}\" --tray")
}

/// Baca isi value Run "ADM" (None = tidak ada).
fn entry_value() -> Option<String> {
    let hkey = open(KEY_QUERY_VALUE)?;
    let mut size: u32 = 0;
    let rc = unsafe { RegQueryValueExW(hkey, VALUE_NAME, None, None, None, Some(&mut size)) };
    if rc != ERROR_SUCCESS || size == 0 {
        unsafe {
            let _ = RegCloseKey(hkey);
        }
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let rc = unsafe {
        RegQueryValueExW(hkey, VALUE_NAME, None, None, Some(buf.as_mut_ptr()), Some(&mut size))
    };
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    if rc != ERROR_SUCCESS {
        return None;
    }
    buf.truncate(size as usize);
    let wide: Vec<u16> = buf
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&c| c != 0)
        .collect();
    Some(String::from_utf16_lossy(&wide))
}

/// Apakah autostart aktif DAN menunjuk exe saat ini. Value yang ada tapi
/// menunjuk exe lama (mis. bekas `target\release` yang sudah dihapus)
/// dianggap TIDAK aktif agar ditulis ulang, bukan dibiarkan basi.
pub fn is_enabled() -> bool {
    entry_value().is_some_and(|v| v.eq_ignore_ascii_case(&expected_cmd()))
}

/// Samakan registry dengan setting saat startup: perbaiki entri basi
/// (path exe lama) dan hapus entri bila setting mati.
pub fn sync(want: bool) {
    match (want, entry_value()) {
        (true, v) if v.as_deref().map(str::to_ascii_lowercase)
            != Some(expected_cmd().to_ascii_lowercase()) => {
            set(true);
        }
        (false, Some(_)) => {
            set(false);
        }
        _ => {}
    }
}

/// Aktif/nonaktifkan autostart. Nilai = `"<exe>" --tray`.
pub fn set(enabled: bool) -> bool {
    let Some(hkey) = open(KEY_SET_VALUE) else {
        return false;
    };
    let ok = if enabled {
        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let cmd = format!("\"{exe}\" --tray");
        let mut wide: Vec<u16> = cmd.encode_utf16().collect();
        wide.push(0); // NUL terminator
        let bytes = unsafe {
            std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2)
        };
        let rc = unsafe { RegSetValueExW(hkey, VALUE_NAME, Some(0), REG_SZ, Some(bytes)) };
        rc == ERROR_SUCCESS
    } else {
        let rc = unsafe { RegDeleteValueW(hkey, VALUE_NAME) };
        // Sudah tidak ada juga dianggap sukses.
        rc == ERROR_SUCCESS || !is_enabled_after_close(hkey)
    };
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    ok
}

fn is_enabled_after_close(_hkey: HKEY) -> bool {
    is_enabled()
}

/// Toggle; kembalikan status baru.
pub fn toggle() -> bool {
    let new = !is_enabled();
    set(new);
    is_enabled()
}
