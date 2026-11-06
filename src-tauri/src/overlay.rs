//! The bubble and panel windows: made non-activating so showing or clicking them never takes focus
//! from the app the user is selecting in, and positioned in physical pixels via Win32.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;

use tauri::{AppHandle, Manager};

use crate::capture;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Debug, Clone, Copy)]
pub enum Overlay {
    Bubble,
    Panel,
}

static BUBBLE: AtomicIsize = AtomicIsize::new(0);
static PANEL: AtomicIsize = AtomicIsize::new(0);

impl Overlay {
    const ALL: [Overlay; 2] = [Overlay::Bubble, Overlay::Panel];

    fn label(self) -> &'static str {
        match self {
            Overlay::Bubble => "bubble",
            Overlay::Panel => "panel",
        }
    }

    fn slot(self) -> &'static AtomicIsize {
        match self {
            Overlay::Bubble => &BUBBLE,
            Overlay::Panel => &PANEL,
        }
    }

    fn hwnd(self) -> Option<HWND> {
        let raw = self.slot().load(Ordering::Relaxed);
        (raw != 0).then(|| HWND(raw as *mut _))
    }
}

pub fn init(app: &AppHandle) -> tauri::Result<()> {
    for overlay in Overlay::ALL {
        let window = app
            .get_webview_window(overlay.label())
            .unwrap_or_else(|| panic!("window '{}' missing from tauri.conf.json", overlay.label()));
        let raw = window.hwnd()?.0 as isize;
        overlay.slot().store(raw, Ordering::Relaxed);
        unsafe {
            let hwnd = HWND(raw as *mut _);
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let extra = (WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST).0 as isize;
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | extra);
            // Undecorated windows keep hidden caption/frame styles, which make Windows enforce a
            // minimum window width (~130px): the 40px bubble became 133px wide. Plain popups have
            // no minimum.
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            let frame = (WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX).0 as isize;
            SetWindowLongPtrW(hwnd, GWL_STYLE, (style & !frame) | WS_POPUP.0 as isize);
            let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED);
        }
    }
    Ok(())
}

/// Window rectangle in physical screen pixels, only while the overlay is visible.
pub fn rect(overlay: Overlay) -> Option<RECT> {
    let hwnd = overlay.hwnd()?;
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return None;
        }
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect).ok()?;
        Some(rect)
    }
}

fn contains(overlay: Overlay, pt: POINT) -> bool {
    rect(overlay).is_some_and(|r| pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom)
}

pub fn bubble_contains(pt: POINT) -> bool {
    contains(Overlay::Bubble, pt)
}

pub fn any_contains(pt: POINT) -> bool {
    Overlay::ALL.into_iter().any(|o| contains(o, pt))
}

/// Where the open panel hangs from, set when the bubble is clicked. `None` while the panel is closed,
/// so a late resize request from the page can't bring a dismissed panel back.
static PANEL_ANCHOR: Mutex<Option<(i32, i32)>> = Mutex::new(None);

/// Remembers where the panel should appear; the page shows it via [`fit_panel`] once it has
/// rendered, so it never flashes stale content or the wrong size.
pub fn prepare_panel(anchor: POINT) {
    *PANEL_ANCHOR.lock().unwrap() = Some((anchor.x, anchor.y));
}

/// Sizes the open panel to its content (logical pixels) and shows it at its anchor.
pub fn fit_panel(width: f64, height: f64) {
    let Some((x, y)) = *PANEL_ANCHOR.lock().unwrap() else { return };
    let size = (width.ceil() as i32, height.ceil() as i32);
    show_near(Overlay::Panel, POINT { x, y }, (0, 4), Some(size));
}

/// Shows the overlay below-right of `anchor`, offset by logical pixels, kept inside the monitor's
/// work area (flipped above the anchor when there is no room below). `size` (logical pixels)
/// resizes it; `None` keeps the current size. Never activates it.
pub fn show_near(overlay: Overlay, anchor: POINT, offset: (i32, i32), size: Option<(i32, i32)>) {
    let Some(hwnd) = overlay.hwnd() else { return };
    unsafe {
        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let scale = |v: i32| v * dpi / 96;
        let (w, h) = match size {
            Some((w, h)) => (scale(w), scale(h)),
            None => {
                let mut current = RECT::default();
                let _ = GetWindowRect(hwnd, &mut current);
                (current.right - current.left, current.bottom - current.top)
            }
        };

        let mut x = anchor.x + scale(offset.0);
        let mut y = anchor.y + scale(offset.1);

        let monitor = MonitorFromPoint(anchor, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let work = info.rcWork;
            if x + w > work.right {
                x = work.right - w;
            }
            if y + h > work.bottom {
                y = anchor.y - h - scale(offset.1);
            }
            x = x.max(work.left);
            y = y.max(work.top);
        }

        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
}

/// Gives the overlay keyboard focus (the panel's text box). Allowed because the user just clicked it.
/// Lets the overlay take keyboard focus (the panel's task box). Windows won't activate a
/// `WS_EX_NOACTIVATE` window, so that style is lifted until the overlay is hidden again.
pub fn activate(overlay: Overlay) {
    let Some(hwnd) = overlay.hwnd() else { return };
    set_no_activate(hwnd, false);
    capture::focus_window(hwnd);
}

fn set_no_activate(hwnd: HWND, on: bool) {
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let flag = WS_EX_NOACTIVATE.0 as isize;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, if on { ex | flag } else { ex & !flag });
    }
}

pub fn hide(overlay: Overlay) {
    if matches!(overlay, Overlay::Panel) {
        *PANEL_ANCHOR.lock().unwrap() = None;
    }
    if let Some(hwnd) = overlay.hwnd() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        // Next time it opens, clicking it must not take focus from the user's app again.
        set_no_activate(hwnd, true);
    }
}

pub fn hide_all() {
    Overlay::ALL.into_iter().for_each(hide);
}
