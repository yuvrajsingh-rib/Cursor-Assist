//! "Start with Windows" via the per-user Run key (no admin rights needed).

use windows::core::{w, PCWSTR};
use windows::Win32::System::Registry::{RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const VALUE_NAME: PCWSTR = w!("CursorAssist");

pub fn set(enabled: bool) -> Result<(), String> {
    unsafe {
        if enabled {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let command: Vec<u16> =
                format!("\"{}\"", exe.display()).encode_utf16().chain(std::iter::once(0)).collect();
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                VALUE_NAME,
                REG_SZ.0,
                Some(command.as_ptr().cast()),
                (command.len() * 2) as u32,
            )
            .ok()
            .map_err(|e| e.to_string())
        } else {
            let result = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE_NAME);
            // Already absent is fine.
            if result.is_err() && result != windows::Win32::Foundation::ERROR_FILE_NOT_FOUND {
                return Err(format!("{result:?}"));
            }
            Ok(())
        }
    }
}
