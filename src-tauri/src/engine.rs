//! The engine thread: receives hook events and UI commands, captures the selection, drives the
//! overlays, and writes results back. It is the only thread that touches the clipboard.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO};

use crate::capture::{self, ClipboardGrab, Uia};
use crate::history::Status;
use crate::hooks::{cursor_pos, Gesture, HookEvent};
use crate::overlay::{self, Overlay};
use crate::AppState;

/// Selections longer than this are ignored (a small local model can't do much with them anyway).
const MAX_CHARS: usize = 8000;

/// Logical size of the bubble window: the dot sits in its centre, and the rest is the invisible
/// "approach" area that makes it grow.
const BUBBLE_SIZE: (i32, i32) = (40, 40);

/// Never capture anything in these apps: password managers and Windows sign-in/credential prompts.
const BLOCKED_APPS: &[&str] = &[
    "keepass.exe",
    "keepassxc.exe",
    "1password.exe",
    "bitwarden.exe",
    "credentialuibroker.exe",
    "lockapp.exe",
    "logonui.exe",
    "consent.exe",
];

/// Never send Ctrl+C to these apps: in a console it interrupts the running program.
const NO_CLIPBOARD_APPS: &[&str] =
    &["windowsterminal.exe", "cmd.exe", "powershell.exe", "pwsh.exe", "conhost.exe", "openconsole.exe", "mstsc.exe"];

pub enum Event {
    Hook(HookEvent),
    /// Put history entry N's result on the clipboard and close the panel.
    Copy(u64),
    /// Paste history entry N's result over the selection it was made from and close the panel.
    Replace(u64),
    /// The panel's ✕ or Esc.
    ClosePanel,
    /// Generation for history entry N ended (done, failed or stopped).
    RunFinished(u64),
    /// Open the recent-results list (tray menu or Ctrl+Alt+H).
    ShowHistory,
}

/// How long the dot stays as a "your result is here" marker after the answer is finished.
const MARKER_TTL: Duration = Duration::from_secs(120);

/// The bubble left behind when the panel is dismissed with a result still running or unused.
struct Marker {
    entry: u64,
    /// None while the result is still being written.
    expires: Option<Instant>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Selection {
    pub text: String,
    pub method: &'static str,
    pub app: String,
    pub gesture: Gesture,
    pub capture_ms: u64,
    /// False when the source reports itself read-only (Replace is disabled then).
    pub editable: bool,
    #[serde(skip)]
    pub source: isize,
    #[serde(skip)]
    pub anchor: POINT,
}

impl Selection {
    fn source_hwnd(&self) -> HWND {
        HWND(self.source as *mut _)
    }
}

pub fn spawn(app: AppHandle, events: Receiver<Event>) {
    thread::Builder::new()
        .name("engine".into())
        .spawn(move || {
            let uia = Uia::new().map_err(|e| eprintln!("[engine] UI Automation unavailable: {e}")).ok();
            let clipboard = ClipboardGrab::new().map_err(|e| eprintln!("[engine] clipboard unavailable: {e}")).ok();
            let own_pid = std::process::id();
            let state = app.state::<AppState>();
            let mut marker: Option<Marker> = None;

            loop {
                let event = match events.recv_timeout(Duration::from_millis(500)) {
                    Ok(event) => Some(event),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                if marker.as_ref().and_then(|m| m.expires).is_some_and(|t| Instant::now() > t) {
                    marker = None;
                    overlay::hide(Overlay::Bubble);
                }
                let Some(event) = event else { continue };
                match event {
                    Event::Hook(HookEvent::PointerDown { at }) => {
                        if !overlay::any_contains(at) {
                            leave(&app, &state, &mut marker, false);
                        }
                    }
                    Event::Hook(HookEvent::KeyDown) => {
                        if foreground_pid() != own_pid {
                            leave(&app, &state, &mut marker, false);
                        }
                    }
                    Event::Hook(HookEvent::SelectionEnded { at, start, gesture, ibeam }) => {
                        // Give the source app a moment to finalize the selection.
                        thread::sleep(Duration::from_millis(60));
                        match capture_selection(uia.as_ref(), clipboard.as_ref(), own_pid, at, start, gesture, ibeam) {
                            Ok(selection) => {
                                eprintln!(
                                    "[engine] {} chars from {} via {} ({:?}, {} ms)",
                                    selection.text.chars().count(),
                                    selection.app,
                                    selection.method,
                                    selection.gesture,
                                    selection.capture_ms
                                );
                                // A new selection takes the dot over from any result marker; the
                                // result itself stays in Recent results.
                                marker = None;
                                set_bubble_mode(&app, "");
                                // Start the 40px window just past the pointer: under it, the dot
                                // would grow at once and the user's next click would open the menu.
                                overlay::show_near(Overlay::Bubble, selection.anchor, (6, 8), Some(BUBBLE_SIZE));
                                *state.current.lock().unwrap() = Some(selection);
                            }
                            Err(reason) => {
                                let app = capture::process_name(unsafe { GetForegroundWindow() });
                                eprintln!("[engine] skipped in {app} ({gesture:?}, ibeam={ibeam}): {reason}");
                            }
                        }
                    }
                    Event::Hook(HookEvent::BubbleClicked) => match marker.take() {
                        Some(m) => {
                            set_bubble_mode(&app, "");
                            open_entry(&app, m.entry);
                        }
                        None => open_panel(&app, &state),
                    },
                    Event::Hook(HookEvent::HistoryHotkey) | Event::ShowHistory => {
                        *state.viewing.lock().unwrap() = None;
                        overlay::prepare_panel(cursor_pos());
                        let _ = app.emit_to("panel", "history-open", ());
                    }
                    Event::RunFinished(id) => {
                        if let Some(m) = marker.as_mut().filter(|m| m.entry == id) {
                            m.expires = Some(Instant::now() + MARKER_TTL);
                            set_bubble_mode(&app, "ready");
                        }
                    }
                    Event::Copy(id) => {
                        let Some(entry) = state.history.get(id) else { continue };
                        overlay::hide(Overlay::Panel);
                        if let Some(clipboard) = &clipboard {
                            clipboard.set_text(entry.output.trim(), false);
                        }
                        state.history.update(id, |e| e.used = true);
                        return_focus(&state, own_pid);
                        leave(&app, &state, &mut marker, true);
                    }
                    Event::Replace(id) => {
                        let Some(entry) = state.history.get(id) else { continue };
                        overlay::hide(Overlay::Panel);
                        state.history.update(id, |e| e.used = true);
                        leave(&app, &state, &mut marker, true);
                        let selection = entry.selection;
                        if !capture::focus_window(selection.source_hwnd()) {
                            eprintln!("[engine] replace aborted: could not refocus {}", selection.app);
                            continue;
                        }
                        let pasted = clipboard.as_ref().is_some_and(|c| c.paste_text(entry.output.trim()));
                        eprintln!("[engine] replaced selection in {}: {pasted}", selection.app);
                    }
                    Event::ClosePanel => {
                        // Refocus first: it needs the source window recorded with the selection.
                        return_focus(&state, own_pid);
                        leave(&app, &state, &mut marker, true);
                    }
                }
            }
        })
        .expect("spawn engine thread");
}

/// The user moved on from the panel (clicked outside, typed elsewhere, or closed it). Hides it,
/// stops any dictation and forgets the selected text: nothing selected stays in memory once the
/// user has moved on. Generation is *not* stopped: the answer keeps streaming into Recent results.
///
/// If the panel was showing a result that is still being written, or that hasn't been copied or
/// pasted, the dot stays where the panel was as a way back to it (`explicit` closes only keep it
/// while the answer is still being written).
fn leave(app: &AppHandle, state: &AppState, marker: &mut Option<Marker>, explicit: bool) {
    let panel = overlay::rect(Overlay::Panel);
    let viewing = state.viewing.lock().unwrap().take();
    overlay::hide(Overlay::Panel);
    state.cancel_dictation();
    state.current.lock().unwrap().take();

    let entry = viewing.and_then(|id| state.history.get(id));
    if let (Some(rect), Some(entry)) = (panel, entry) {
        let running = entry.status == Status::Running;
        if running || (!explicit && !entry.used && entry.status == Status::Done) {
            set_bubble_mode(app, if running { "running" } else { "ready" });
            overlay::show_near(Overlay::Bubble, POINT { x: rect.left, y: rect.top }, (0, 0), Some(BUBBLE_SIZE));
            *marker = Some(Marker { entry: entry.id, expires: (!running).then(|| Instant::now() + MARKER_TTL) });
            return;
        }
    }
    if marker.is_none() {
        overlay::hide(Overlay::Bubble);
    }
}

/// Switches the bubble between the plain dot (""), "running" and "ready". The bubble page has no
/// IPC access, so the core sets it directly.
fn set_bubble_mode(app: &AppHandle, mode: &'static str) {
    if let Some(bubble) = app.get_webview_window("bubble") {
        let _ = bubble.eval(format!("document.body.dataset.mode = '{mode}'"));
    }
}

/// Reopens the panel on history entry `id`, where the bubble is.
fn open_entry(app: &AppHandle, id: u64) {
    let anchor = overlay::rect(Overlay::Bubble).map(|r| POINT { x: r.left, y: r.bottom }).unwrap_or_else(cursor_pos);
    overlay::hide(Overlay::Bubble);
    overlay::prepare_panel(anchor);
    let _ = app.emit_to("panel", "entry-open", id);
}

/// If the panel took focus (the user typed into it), hand focus back to the source app.
fn return_focus(state: &AppState, own_pid: u32) {
    if foreground_pid() != own_pid {
        return;
    }
    if let Some(selection) = state.current.lock().unwrap().as_ref() {
        capture::focus_window(selection.source_hwnd());
    }
}

fn open_panel(app: &AppHandle, state: &AppState) {
    if state.current.lock().unwrap().is_none() {
        return;
    }
    let bubble = overlay::rect(Overlay::Bubble);
    let anchor = bubble.map(|r| POINT { x: r.left, y: r.bottom }).unwrap_or_else(cursor_pos);
    overlay::hide(Overlay::Bubble);
    // The page renders the menu, then calls `fit_panel` to size and show itself. Models are woken
    // individually as the user hovers actions (see `commands::prepare_action`).
    overlay::prepare_panel(anchor);
    let _ = app.emit_to("panel", "menu-open", ());
}

fn capture_selection(
    uia: Option<&Uia>,
    clipboard: Option<&ClipboardGrab>,
    own_pid: u32,
    at: POINT,
    start: POINT,
    gesture: Gesture,
    ibeam: bool,
) -> Result<Selection, String> {
    let started = Instant::now();
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_invalid() {
        return Err("no foreground window".into());
    }
    if capture::window_pid(foreground) == own_pid {
        return Err("selection is inside CursorAssist".into());
    }
    let app = capture::process_name(foreground);
    let app_lower = app.to_lowercase();
    if BLOCKED_APPS.contains(&app_lower.as_str()) {
        return Err(format!("{app} is blocklisted"));
    }

    let focused = uia.and_then(Uia::focused);
    if focused.as_ref().is_some_and(Uia::is_password) {
        return Err("focused element is a password field".into());
    }
    let editable = !focused.as_ref().is_some_and(Uia::is_read_only);

    let mut method = "uia";
    let mut text = match (uia, &focused) {
        (Some(uia), Some(el)) => uia.selected_text(el).unwrap_or_default(),
        _ => String::new(),
    };

    if text.trim().is_empty() {
        // Only fall back to Ctrl+C when the gesture really looked like a text selection,
        // otherwise every window drag would poke the target app.
        let texty = ibeam || gesture == Gesture::Keyboard || uia.is_some_and(|u| u.starts_in_text(start));
        if !texty {
            return Err("gesture did not start on text".into());
        }
        if NO_CLIPBOARD_APPS.contains(&app_lower.as_str()) {
            return Err(format!("no UIA selection and clipboard fallback is disabled for {app}"));
        }
        text = clipboard.and_then(ClipboardGrab::copy_selection).ok_or("nothing selected")?;
        method = "clipboard";
    }

    if text.trim().is_empty() {
        return Err("selection is empty".into());
    }
    if text.chars().count() > MAX_CHARS {
        return Err(format!("selection longer than {MAX_CHARS} chars"));
    }

    let anchor = if gesture == Gesture::Keyboard { caret_pos(foreground).unwrap_or_else(cursor_pos) } else { at };
    Ok(Selection {
        text,
        method,
        app,
        gesture,
        capture_ms: started.elapsed().as_millis() as u64,
        editable,
        source: foreground.0 as isize,
        anchor,
    })
}

/// Bottom of the system caret, for apps that still use one (Notepad, classic edit controls).
fn caret_pos(foreground: HWND) -> Option<POINT> {
    unsafe {
        let thread = GetWindowThreadProcessId(foreground, None);
        let mut info = GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        GetGUIThreadInfo(thread, &mut info).ok()?;
        if info.hwndCaret.is_invalid() {
            return None;
        }
        let mut pt = POINT { x: info.rcCaret.left, y: info.rcCaret.bottom };
        ClientToScreen(info.hwndCaret, &mut pt).as_bool().then_some(pt)
    }
}

fn foreground_pid() -> u32 {
    capture::window_pid(unsafe { GetForegroundWindow() })
}
