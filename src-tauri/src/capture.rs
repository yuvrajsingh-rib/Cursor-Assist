//! Reading the user's current selection from another application.
//!
//! 1. UI Automation `TextPattern.GetSelection()`: no side effects, works in most modern apps.
//! 2. Clipboard fallback: snapshot the clipboard, synthesize Ctrl+C, read the text, restore it.
//!
//! Writing back (Replace) goes through the clipboard too: put the result on it, synthesize Ctrl+V,
//! then restore what the user had there.

use std::thread::sleep;
use std::time::{Duration, Instant};

use windows::core::{w, Interface, Result, PWSTR};
use windows::Win32::Foundation::{CloseHandle, GlobalFree, HANDLE, HGLOBAL, HWND, POINT};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow, HWND_MESSAGE, WINDOW_EX_STYLE,
    WINDOW_STYLE,
};

use crate::hooks::INJECTED_MARKER;

const CF_UNICODETEXT: u32 = 13;

pub struct Uia {
    automation: IUIAutomation,
    walker: IUIAutomationTreeWalker,
}

impl Uia {
    /// Must be created on the thread that uses it (COM, multithreaded apartment).
    pub fn new() -> Result<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let automation: IUIAutomation = CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
            // A hung target app must not freeze us.
            if let Ok(a2) = automation.cast::<IUIAutomation2>() {
                let _ = a2.SetConnectionTimeout(1500);
                let _ = a2.SetTransactionTimeout(1500);
            }
            let walker = automation.ControlViewWalker()?;
            Ok(Self { automation, walker })
        }
    }

    pub fn focused(&self) -> Option<IUIAutomationElement> {
        unsafe { self.automation.GetFocusedElement().ok() }
    }

    pub fn is_password(element: &IUIAutomationElement) -> bool {
        unsafe { element.CurrentIsPassword().map(|b| b.as_bool()).unwrap_or(false) }
    }

    /// True only when the element positively reports itself read-only (e.g. a web page body).
    /// Elements without a ValuePattern (Word documents, many editors) count as editable.
    pub fn is_read_only(element: &IUIAutomationElement) -> bool {
        unsafe {
            element
                .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                .and_then(|p| p.CurrentIsReadOnly())
                .is_ok_and(|b| b.as_bool())
        }
    }

    /// Selected text of the focused element, looking a few ancestors up for a TextPattern
    /// (browsers and WebView2 apps often focus a node deep inside the document rather than the
    /// document itself). `None` means no TextPattern was found; `Some("")` means one was found
    /// with nothing selected.
    pub fn selected_text(&self, element: &IUIAutomationElement) -> Option<String> {
        self.ancestors(element).find_map(|el| {
            let pattern = unsafe { el.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) }.ok()?;
            read_selection(&pattern)
        })
    }

    /// Whether the element under `pt`, or one of its ancestors, is a text-like control.
    /// Web apps (Teams, new Outlook) show an arrow cursor over message text, so the cursor shape
    /// alone is not enough; their content lives inside a Document element.
    pub fn starts_in_text(&self, pt: POINT) -> bool {
        let Ok(element) = (unsafe { self.automation.ElementFromPoint(pt) }) else { return false };
        self.ancestors(&element)
            .any(|el| unsafe { el.CurrentControlType() }.is_ok_and(is_text_control))
    }

    /// The element itself followed by up to `MAX_ANCESTORS` parents.
    fn ancestors(&self, element: &IUIAutomationElement) -> impl Iterator<Item = IUIAutomationElement> + '_ {
        const MAX_ANCESTORS: usize = 20;
        std::iter::successors(Some(element.clone()), |el| unsafe { self.walker.GetParentElement(el).ok() })
            .take(MAX_ANCESTORS + 1)
    }
}

fn read_selection(pattern: &IUIAutomationTextPattern) -> Option<String> {
    unsafe {
        let ranges = pattern.GetSelection().ok()?;
        let mut out = String::new();
        for i in 0..ranges.Length().ok()? {
            if let Ok(text) = ranges.GetElement(i).and_then(|r| r.GetText(-1)) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&text.to_string());
            }
        }
        Some(out)
    }
}

/// Control types where a drag or double-click most likely selected text.
fn is_text_control(control_type: UIA_CONTROLTYPE_ID) -> bool {
    [UIA_EditControlTypeId, UIA_DocumentControlTypeId, UIA_TextControlTypeId].contains(&control_type)
}

pub struct ClipboardGrab {
    owner: HWND,
}

impl ClipboardGrab {
    /// Creates a message-only window to own the clipboard while restoring it
    /// (`SetClipboardData` fails when the clipboard was opened without an owner).
    pub fn new() -> Result<Self> {
        let owner = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("CursorAssistClipboard"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )?
        };
        Ok(Self { owner })
    }

    /// Copies the selection with a synthesized Ctrl+C and puts the user's clipboard back afterwards.
    /// Returns `None` if nothing was copied or the clipboard could not be safely snapshotted.
    pub fn copy_selection(&self) -> Option<String> {
        let saved = self.snapshot()?;
        let before = unsafe { GetClipboardSequenceNumber() };
        send_ctrl_chord(VK_C);

        let deadline = Instant::now() + Duration::from_millis(400);
        while unsafe { GetClipboardSequenceNumber() } == before {
            if Instant::now() > deadline {
                return None;
            }
            sleep(Duration::from_millis(10));
        }
        // Let the source app finish writing all of its formats.
        sleep(Duration::from_millis(30));

        let text = self.read_text();
        self.restore(&saved);
        text
    }

    /// Puts `text` on the clipboard. `transient` keeps it out of clipboard history, for text that
    /// is only there for a moment (Replace).
    pub fn set_text(&self, text: &str, transient: bool) -> bool {
        if !self.open() {
            return false;
        }
        let bytes: Vec<u8> = text.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect();
        unsafe {
            let _ = EmptyClipboard();
            set_global(CF_UNICODETEXT, &bytes);
            if transient {
                exclude_from_history();
            }
            let _ = CloseClipboard();
        }
        true
    }

    /// Pastes `text` over the current selection of the foreground app, then restores the
    /// user's clipboard.
    pub fn paste_text(&self, text: &str) -> bool {
        let Some(saved) = self.snapshot() else { return false };
        if !self.set_text(text, true) {
            return false;
        }
        send_ctrl_chord(VK_V);
        // Apps read the clipboard asynchronously after Ctrl+V (web apps especially); restoring
        // too early would paste the old content.
        sleep(Duration::from_millis(500));
        self.restore(&saved);
        true
    }

    fn open(&self) -> bool {
        for _ in 0..10 {
            if unsafe { OpenClipboard(Some(self.owner)) }.is_ok() {
                return true;
            }
            sleep(Duration::from_millis(15));
        }
        false
    }

    fn snapshot(&self) -> Option<Vec<(u32, Vec<u8>)>> {
        if !self.open() {
            return None;
        }
        let mut saved = Vec::new();
        unsafe {
            let mut format = 0;
            loop {
                format = EnumClipboardFormats(format);
                if format == 0 {
                    break;
                }
                if is_handle_format(format) {
                    continue;
                }
                if let Ok(handle) = GetClipboardData(format) {
                    if let Some(bytes) = global_bytes(HGLOBAL(handle.0)) {
                        saved.push((format, bytes));
                    }
                }
            }
            let _ = CloseClipboard();
        }
        Some(saved)
    }

    fn read_text(&self) -> Option<String> {
        if !self.open() {
            return None;
        }
        let text = unsafe {
            GetClipboardData(CF_UNICODETEXT).ok().and_then(|h| global_bytes(HGLOBAL(h.0))).map(|bytes| {
                let wide: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
                String::from_utf16_lossy(&wide[..end])
            })
        };
        unsafe {
            let _ = CloseClipboard();
        }
        text
    }

    fn restore(&self, saved: &[(u32, Vec<u8>)]) {
        if !self.open() {
            eprintln!("[capture] could not reopen clipboard to restore it");
            return;
        }
        unsafe {
            let _ = EmptyClipboard();
            for (format, bytes) in saved {
                set_global(*format, bytes);
            }
            // Keep the restore itself out of clipboard history / clipboard managers.
            exclude_from_history();
            let _ = CloseClipboard();
        }
    }
}

/// Marks the clipboard content being written as not for clipboard history or clipboard managers.
/// Call with the clipboard open.
unsafe fn exclude_from_history() {
    set_global(RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing")), &[0; 4]);
}

/// Formats whose data is a GDI or private handle rather than global memory; Windows synthesizes
/// the common ones (CF_BITMAP from CF_DIB) so skipping them is safe.
fn is_handle_format(format: u32) -> bool {
    matches!(format, 2 | 3 | 9 | 14 | 0x80 | 0x82 | 0x83 | 0x8E) || (0x200..=0x3FF).contains(&format)
}

unsafe fn global_bytes(global: HGLOBAL) -> Option<Vec<u8>> {
    let size = GlobalSize(global);
    if size == 0 {
        return None;
    }
    let ptr = GlobalLock(global) as *const u8;
    if ptr.is_null() {
        return None;
    }
    let bytes = std::slice::from_raw_parts(ptr, size).to_vec();
    let _ = GlobalUnlock(global);
    Some(bytes)
}

unsafe fn set_global(format: u32, bytes: &[u8]) {
    let Ok(global) = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) else { return };
    let ptr = GlobalLock(global) as *mut u8;
    if ptr.is_null() {
        let _ = GlobalFree(Some(global));
        return;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
    let _ = GlobalUnlock(global);
    // On success the clipboard owns the memory.
    if SetClipboardData(format, Some(HANDLE(global.0))).is_err() {
        let _ = GlobalFree(Some(global));
    }
}

fn key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: INJECTED_MARKER,
            },
        },
    }
}

/// Synthesizes Ctrl+`key`, tagged so our own hooks ignore it.
fn send_ctrl_chord(key: VIRTUAL_KEY) {
    let inputs = [key_input(VK_CONTROL, false), key_input(key, false), key_input(key, true), key_input(VK_CONTROL, true)];
    unsafe {
        SendInput(&inputs, size_of::<INPUT>() as i32);
    }
}

/// Brings `hwnd` back to the foreground (e.g. the source app before Replace).
pub fn focus_window(hwnd: HWND) -> bool {
    unsafe {
        if GetForegroundWindow() == hwnd {
            return true;
        }
        let _ = SetForegroundWindow(hwnd);
        sleep(Duration::from_millis(80));
        if GetForegroundWindow() == hwnd {
            return true;
        }
        // Windows refuses foreground changes from a process that did not get the last input;
        // a synthetic Alt tap makes us that process.
        SendInput(&[key_input(VK_MENU, false), key_input(VK_MENU, true)], size_of::<INPUT>() as i32);
        let _ = SetForegroundWindow(hwnd);
        sleep(Duration::from_millis(80));
        GetForegroundWindow() == hwnd
    }
}

pub fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    pid
}

/// Executable file name (e.g. `OUTLOOK.EXE`) of the process owning `hwnd`.
pub fn process_name(hwnd: HWND) -> String {
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, window_pid(hwnd)) else {
            return String::new();
        };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(process);
        if !ok {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit('\\').next().unwrap_or_default().to_string()
    }
}
