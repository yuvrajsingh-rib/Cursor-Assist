//! Low-level mouse and keyboard hooks that turn raw input into "a selection gesture just ended".
//!
//! Hook callbacks run on a dedicated thread and must return quickly (Windows silently unhooks slow
//! low-level hooks), so they only classify the input and forward a `HookEvent` to the engine thread.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;

use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::engine::Event;
use crate::overlay;

/// Tags input we synthesize ourselves (e.g. Ctrl+C for the clipboard fallback) so the hooks ignore it.
pub const INJECTED_MARKER: usize = 0xCA55_1570;

/// When set, selection gestures are not reported (tray "Pause").
pub static PAUSED: AtomicBool = AtomicBool::new(false);

static TX: OnceLock<Sender<Event>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Gesture {
    Drag,
    MultiClick,
    Keyboard,
}

#[derive(Debug, Clone, Copy)]
pub enum HookEvent {
    /// A gesture that usually selects text just finished.
    SelectionEnded { at: POINT, start: POINT, gesture: Gesture, ibeam: bool },
    /// A mouse button went down outside the bubble (used to dismiss overlays).
    PointerDown { at: POINT },
    /// A non-modifier key was pressed (used to dismiss overlays).
    KeyDown,
    /// The bubble was clicked. The click itself is swallowed so the source app keeps focus.
    BubbleClicked,
    /// Ctrl+Alt+H: open Recent results.
    HistoryHotkey,
}

#[derive(Default)]
struct Tracker {
    down_at: POINT,
    down_ibeam: bool,
    last_down_at: POINT,
    last_down_time: u32,
    clicks: u32,
    swallow_up: bool,
    swallow_history_up: bool,
    shift_nav: bool,
    ctrl_a: bool,
}

/// Ctrl+Alt+H opens Recent results.
const HISTORY_KEY: VIRTUAL_KEY = VK_H;

thread_local! {
    static TRACKER: RefCell<Tracker> = RefCell::new(Tracker::default());
}

fn send(event: HookEvent) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(Event::Hook(event));
    }
}

fn paused() -> bool {
    PAUSED.load(Ordering::Relaxed)
}

pub fn spawn(tx: Sender<Event>) {
    let _ = TX.set(tx);
    std::thread::Builder::new()
        .name("hooks".into())
        .spawn(|| unsafe {
            let module = GetModuleHandleW(None).ok().map(|m| HINSTANCE(m.0));
            let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0);
            let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0);
            if let Err(e) = mouse.as_ref().and(keyboard.as_ref()) {
                eprintln!("[hooks] failed to install input hooks: {e}");
                return;
            }
            // Low-level hooks are delivered through this thread's message queue.
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        })
        .expect("spawn hook thread");
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        if info.dwExtraInfo != INJECTED_MARKER && on_mouse(wparam.0 as u32, info.pt, info.time) {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        if info.dwExtraInfo != INJECTED_MARKER && on_key(wparam.0 as u32, VIRTUAL_KEY(info.vkCode as u16)) {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Returns true when the event must be swallowed (a click on the bubble).
fn on_mouse(msg: u32, pt: POINT, time: u32) -> bool {
    TRACKER.with_borrow_mut(|t| match msg {
        WM_LBUTTONDOWN => {
            if overlay::bubble_contains(pt) {
                t.swallow_up = true;
                send(HookEvent::BubbleClicked);
                return true;
            }
            let (cx, cy, max_gap) = unsafe {
                (GetSystemMetrics(SM_CXDOUBLECLK), GetSystemMetrics(SM_CYDOUBLECLK), GetDoubleClickTime())
            };
            let repeat = time.wrapping_sub(t.last_down_time) <= max_gap
                && (pt.x - t.last_down_at.x).abs() <= cx / 2
                && (pt.y - t.last_down_at.y).abs() <= cy / 2;
            t.clicks = if repeat { t.clicks + 1 } else { 1 };
            t.last_down_time = time;
            t.last_down_at = pt;
            t.down_at = pt;
            t.down_ibeam = cursor_is_ibeam();
            send(HookEvent::PointerDown { at: pt });
            false
        }
        WM_LBUTTONUP => {
            if std::mem::take(&mut t.swallow_up) {
                return true;
            }
            let moved = (pt.x - t.down_at.x).abs().max((pt.y - t.down_at.y).abs());
            let threshold = unsafe { GetSystemMetrics(SM_CXDRAG) }.max(4);
            let gesture = if moved > threshold {
                Some(Gesture::Drag)
            } else if t.clicks >= 2 {
                Some(Gesture::MultiClick)
            } else {
                None
            };
            if let (Some(gesture), false) = (gesture, paused()) {
                send(HookEvent::SelectionEnded { at: pt, start: t.down_at, gesture, ibeam: t.down_ibeam });
            }
            false
        }
        WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
            send(HookEvent::PointerDown { at: pt });
            false
        }
        _ => false,
    })
}

/// Returns true when the key must be swallowed (our Ctrl+Alt+H shortcut).
fn on_key(msg: u32, vk: VIRTUAL_KEY) -> bool {
    let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    TRACKER.with_borrow_mut(|t| {
        if vk == HISTORY_KEY && (down || t.swallow_history_up) {
            if down && key_held(VK_CONTROL) && key_held(VK_MENU) && !key_held(VK_SHIFT) {
                // Repeats while held are swallowed too, but open the list only once.
                if !std::mem::replace(&mut t.swallow_history_up, true) {
                    send(HookEvent::HistoryHotkey);
                }
                return true;
            }
            if !down {
                t.swallow_history_up = false;
                return true;
            }
        }
        if down {
            if is_modifier(vk) {
                return false;
            }
            if key_held(VK_SHIFT) && is_navigation(vk) {
                t.shift_nav = true;
            } else if vk == VK_A && key_held(VK_CONTROL) {
                t.ctrl_a = true;
            }
            send(HookEvent::KeyDown);
            return false;
        }
        // A keyboard selection is complete once the modifier that drove it is released.
        let ended = match vk {
            VK_SHIFT | VK_LSHIFT | VK_RSHIFT => std::mem::take(&mut t.shift_nav),
            VK_CONTROL | VK_LCONTROL | VK_RCONTROL => std::mem::take(&mut t.ctrl_a),
            _ => false,
        };
        if ended && !paused() {
            let at = cursor_pos();
            send(HookEvent::SelectionEnded { at, start: at, gesture: Gesture::Keyboard, ibeam: true });
        }
        false
    })
}

fn is_modifier(vk: VIRTUAL_KEY) -> bool {
    matches!(
        vk,
        VK_SHIFT | VK_LSHIFT | VK_RSHIFT | VK_CONTROL | VK_LCONTROL | VK_RCONTROL | VK_MENU | VK_LMENU
            | VK_RMENU | VK_LWIN | VK_RWIN
    )
}

fn is_navigation(vk: VIRTUAL_KEY) -> bool {
    matches!(vk, VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN | VK_HOME | VK_END | VK_PRIOR | VK_NEXT)
}

fn key_held(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(vk.0 as i32) < 0 }
}

fn cursor_is_ibeam() -> bool {
    unsafe {
        let mut info = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
        GetCursorInfo(&mut info).is_ok()
            && LoadCursorW(None, IDC_IBEAM).map(|c| c == info.hCursor).unwrap_or(false)
    }
}

pub fn cursor_pos() -> POINT {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    pt
}
