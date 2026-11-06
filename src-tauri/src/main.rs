#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod autostart;
mod capture;
mod commands;
mod config;
mod engine;
mod history;
mod hooks;
mod llm;
mod overlay;
mod process;
mod stt;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use tauri::async_runtime::JoinHandle;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::Manager;

use audio::Recording;
use config::{ActionsFile, Settings};
use engine::{Event, Selection};
use history::History;
use llm::LlmHost;
use stt::SttHost;

pub struct AppState {
    pub current: Mutex<Option<Selection>>,
    pub engine: Sender<Event>,
    pub llm: Arc<LlmHost>,
    pub stt: Arc<SttHost>,
    pub config_dir: PathBuf,
    /// The generation in flight and the history entry it writes into.
    pub run: Mutex<Option<(u64, JoinHandle<()>)>>,
    pub recording: Mutex<Option<Recording>>,
    pub history: History,
    /// The history entry the panel is showing, if any (so a dismissed panel can leave a way back).
    pub viewing: Mutex<Option<u64>>,
}

impl AppState {
    /// Stops the generation in flight, if any, keeping what it wrote so far. Dropping the HTTP
    /// stream makes llama-server stop.
    pub fn cancel_run(&self) {
        if let Some((id, task)) = self.run.lock().unwrap().take() {
            task.abort();
            self.history.update(id, |e| {
                if e.status == history::Status::Running {
                    e.status = history::Status::Stopped;
                }
            });
        }
    }

    /// Discards a dictation in progress, if any.
    pub fn cancel_dictation(&self) {
        if let Some(recording) = self.recording.lock().unwrap().take() {
            recording.cancel();
        }
    }
}

/// Returns false if CursorAssist is already running in this session (e.g. started at login and
/// then again from the Start menu). Two instances would show two bubbles and fight over ports.
fn claim_single_instance() -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    unsafe {
        // Never closed: the mutex is held for the life of the process.
        let created = CreateMutexW(None, true, w!("Local\\CursorAssist.SingleInstance"));
        created.is_ok() && GetLastError() != ERROR_ALREADY_EXISTS
    }
}

fn main() {
    if !claim_single_instance() {
        eprintln!("CursorAssist is already running");
        return;
    }
    let (tx, rx) = mpsc::channel();
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            commands::menu_data,
            commands::run_action,
            commands::prepare_action,
            commands::history_list,
            commands::history_entry,
            commands::set_viewing,
            commands::cancel_run,
            commands::apply_result,
            commands::close_panel,
            commands::focus_panel,
            commands::fit_panel,
            commands::prepare_dictation,
            commands::start_dictation,
            commands::stop_dictation,
            commands::cancel_dictation,
        ])
        .setup(move |app| {
            let config_dir = app.path().app_config_dir()?;
            let settings: Settings = config::load_or_init(&config_dir, "settings.json");
            // Write it back so settings added in newer versions show up in the file.
            let _ = config::save(&config_dir, "settings.json", &settings);
            let actions: ActionsFile = config::load_or_init(&config_dir, "actions.json");
            eprintln!("[config] {}", config_dir.display());

            // Release builds only: a dev build would register target\debug\cursorassist.exe.
            if !cfg!(debug_assertions) {
                if let Err(e) = autostart::set(settings.launch_at_login) {
                    eprintln!("[autostart] {e}");
                }
            }

            let log_dir = app.path().app_log_dir()?;
            // Helper servers ship with CursorAssist (see bundle.resources in tauri.conf.json).
            let bin = app.path().resource_dir()?.join("bin");
            let llm = Arc::new(LlmHost::new(settings.llm.clone(), bin.join("llama-server.exe"), log_dir.clone()));
            // Only models some action uses; an unused model would just sit in memory.
            llm.start(&actions.actions.iter().map(|a| a.model.clone()).collect());
            let stt = SttHost::new(settings.stt.clone(), bin.join("parakeet-server.exe"), log_dir);

            app.manage(AppState {
                current: Mutex::new(None),
                engine: tx.clone(),
                llm,
                stt,
                config_dir: config_dir.clone(),
                run: Mutex::new(None),
                recording: Mutex::new(None),
                history: History::default(),
                viewing: Mutex::new(None),
            });
            overlay::init(app.handle())?;
            build_tray(app, &settings, config_dir)?;
            engine::spawn(app.handle().clone(), rx);
            hooks::spawn(tx);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running CursorAssist");
}

fn build_tray(app: &tauri::App, settings: &Settings, config_dir: PathBuf) -> tauri::Result<()> {
    let pause = CheckMenuItem::with_id(app, "pause", "Pause", true, false, None::<&str>)?;
    let login = CheckMenuItem::with_id(
        app,
        "login",
        "Start with Windows",
        !cfg!(debug_assertions),
        settings.launch_at_login,
        None::<&str>,
    )?;
    let recent = MenuItem::with_id(app, "recent", "Recent results\tCtrl+Alt+H", true, None::<&str>)?;
    let open_config = MenuItem::with_id(app, "config", "Open settings folder", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit CursorAssist", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let separator2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&recent, &separator2, &pause, &login, &open_config, &separator, &quit])?;

    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("CursorAssist")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "recent" => {
                let _ = app.state::<AppState>().engine.send(Event::ShowHistory);
            }
            "pause" => {
                let paused = pause.is_checked().unwrap_or(false);
                hooks::PAUSED.store(paused, Ordering::Relaxed);
                if paused {
                    overlay::hide_all();
                }
            }
            "login" => {
                let enabled = login.is_checked().unwrap_or(false);
                let mut settings: Settings = config::load_or_init(&config_dir, "settings.json");
                settings.launch_at_login = enabled;
                if let Err(e) = config::save(&config_dir, "settings.json", &settings) {
                    eprintln!("[config] {e}");
                }
                if let Err(e) = autostart::set(enabled) {
                    eprintln!("[autostart] {e}");
                }
            }
            "config" => {
                let _ = std::process::Command::new("explorer").arg(&config_dir).spawn();
            }
            "quit" => app.exit(0),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}
