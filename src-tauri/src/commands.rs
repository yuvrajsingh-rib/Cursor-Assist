//! Commands invoked from the panel UI.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::audio::Recording;
use crate::config::{self, Action, ActionsFile, DEFAULT_SYSTEM, TASK_ACTION};
use crate::engine::{Event, Selection};
use crate::history::{Entry, Status};
use crate::llm::RunStats;
use crate::overlay::{self, Overlay};
use crate::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MenuData {
    selection: Option<Selection>,
    actions: Vec<ActionView>,
    /// How many recent results are kept, for the menu's "Recent" row.
    recent: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ActionView {
    id: String,
    name: String,
    group: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Delta {
    run_id: u64,
    text: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Done {
    run_id: u64,
    stats: RunStats,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Failed {
    run_id: u64,
    message: String,
}

fn load_actions(state: &AppState) -> ActionsFile {
    // Read on every use, so edits to actions.json apply without a restart.
    config::load_or_init(&state.config_dir, "actions.json")
}

#[tauri::command]
pub fn menu_data(state: State<AppState>) -> MenuData {
    let actions = load_actions(&state)
        .actions
        .into_iter()
        .filter(|a| !a.hidden)
        .map(|a| ActionView { id: a.id, name: a.name, group: a.group })
        .collect();
    MenuData { selection: state.current.lock().unwrap().clone(), actions, recent: state.history.len() }
}

fn title_for(action: &Action, instruction: &str) -> String {
    match &action.group {
        _ if action.id == TASK_ACTION => instruction.to_string(),
        Some(group) => format!("{}: {}", group.trim_start_matches("Change "), action.name),
        None => action.name.clone(),
    }
}

/// Starts streaming `action_id` over the current selection, or over the selection of history
/// entry `from_entry` (Retry from history). The panel picks `run_id` so it can match events that
/// arrive before this call returns. The result is recorded in the history as it streams, so it
/// survives the panel being closed; starting a run stops the previous one.
#[tauri::command]
pub fn run_action(
    app: AppHandle,
    state: State<AppState>,
    run_id: u64,
    action_id: String,
    instruction: Option<String>,
    from_entry: Option<u64>,
) -> Result<(), String> {
    let action = load_actions(&state)
        .actions
        .into_iter()
        .find(|a| a.id == action_id)
        .ok_or_else(|| format!("no action '{action_id}' in actions.json"))?;
    let selection = match from_entry {
        Some(id) => state.history.get(id).map(|e| e.selection).ok_or("that result is no longer kept")?,
        None => state.current.lock().unwrap().clone().ok_or("no text selected")?,
    };
    let instruction = instruction.unwrap_or_default();
    if action.prompt.contains("{{instruction}}") && instruction.trim().is_empty() {
        return Err("Type an instruction first".into());
    }

    let prompt = config::render(&action.prompt, &selection.text, instruction.trim(), &selection.app);
    let system = action.system.clone().unwrap_or_else(|| DEFAULT_SYSTEM.to_string());
    let llm = state.llm.clone();

    state.cancel_run();
    let title = title_for(&action, instruction.trim());
    let instruction = (!instruction.trim().is_empty()).then(|| instruction.trim().to_string());
    eprintln!("[llm] run {run_id}: '{}' on {} chars with '{}'", action.id, selection.text.chars().count(), action.model);
    state.history.push(Entry::new(run_id, &action.id, title, instruction, selection));

    let task = tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let started = std::time::Instant::now();
        let mut first_token = None;
        let result = llm
            .stream(&action.model, &system, &prompt, action.temperature, action.max_tokens, |text| {
                first_token.get_or_insert_with(|| started.elapsed());
                state.history.update(run_id, |e| e.output.push_str(text));
                let _ = app.emit_to("panel", "llm-delta", Delta { run_id, text: text.to_string() });
            })
            .await;
        let _ = match result {
            Ok(stats) => {
                eprintln!(
                    "[llm] run {run_id} done: first text after {:.1}s, total {:.1}s, {:.1} tok/s",
                    first_token.unwrap_or_default().as_secs_f32(),
                    started.elapsed().as_secs_f32(),
                    stats.tokens_per_second.unwrap_or_default()
                );
                state.history.update(run_id, |e| {
                    e.status = Status::Done;
                    e.model = Some(stats.model.clone());
                    e.tokens_per_second = stats.tokens_per_second;
                });
                app.emit_to("panel", "llm-done", Done { run_id, stats })
            }
            Err(message) => {
                eprintln!("[llm] run {run_id} failed: {message}");
                state.history.update(run_id, |e| {
                    e.status = Status::Failed;
                    e.error = Some(message.clone());
                });
                app.emit_to("panel", "llm-error", Failed { run_id, message })
            }
        };
        let _ = state.engine.send(Event::RunFinished(run_id));
    });
    *state.run.lock().unwrap() = Some((run_id, task));
    Ok(())
}

/// Recent results, newest first.
#[tauri::command]
pub fn history_list(state: State<AppState>) -> Vec<Entry> {
    state.history.list()
}

#[tauri::command]
pub fn history_entry(state: State<AppState>, id: u64) -> Option<Entry> {
    state.history.get(id)
}

/// Tells the core which result the panel is showing, so dismissing the panel can leave a marker
/// to come back to it.
#[tauri::command]
pub fn set_viewing(state: State<AppState>, id: Option<u64>) {
    *state.viewing.lock().unwrap() = id;
}

/// Wakes the model behind `action_id` when the user shows interest in it (hovering it, or focusing
/// the ask box), so only that model is loaded and it is usually ready by the click.
#[tauri::command]
pub fn prepare_action(state: State<AppState>, action_id: String) {
    let Some(action) = load_actions(&state).actions.into_iter().find(|a| a.id == action_id) else { return };
    let llm = state.llm.clone();
    tauri::async_runtime::spawn(async move { llm.wake(&action.model).await });
}

#[tauri::command]
pub fn cancel_run(state: State<AppState>) {
    state.cancel_run();
}

/// Copies, or pastes over the original selection, the result of history entry `id`. The text
/// comes from the history, not from the page.
#[tauri::command]
pub fn apply_result(state: State<AppState>, id: u64, replace: bool) {
    let _ = state.engine.send(if replace { Event::Replace(id) } else { Event::Copy(id) });
}

#[tauri::command]
pub fn close_panel(state: State<AppState>) {
    let _ = state.engine.send(Event::ClosePanel);
}

/// The panel is non-activating so clicks keep focus in the source app; typing into its text box
/// needs keyboard focus, so it asks for it explicitly.
#[tauri::command]
pub fn focus_panel(app: AppHandle) {
    overlay::activate(Overlay::Panel);
    // Activating the window isn't enough: WebView2 also has to move keyboard focus into the page.
    if let Some(panel) = app.get_webview_window("panel") {
        let _ = panel.set_focus();
    }
}

/// Sizes the panel window to the page's content (logical pixels) and shows it if it is open.
#[tauri::command]
pub fn fit_panel(width: f64, height: f64) {
    overlay::fit_panel(width, height);
}

/// Starts the speech server early (it takes a few seconds to load), when the user reaches for
/// the mic.
#[tauri::command]
pub fn prepare_dictation(state: State<AppState>) {
    let stt = state.stt.clone();
    std::thread::spawn(move || stt.prepare());
}

/// Starts recording the microphone; the panel receives `mic-level` events for its meter.
#[tauri::command]
pub fn start_dictation(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    let mut slot = state.recording.lock().unwrap();
    if slot.is_some() {
        return Ok(());
    }
    let stt = state.stt.clone();
    std::thread::spawn(move || stt.prepare());
    let recording = Recording::start(move |level| {
        let _ = app.emit_to("panel", "mic-level", level);
    })?;
    eprintln!("[stt] recording");
    *slot = Some(recording);
    Ok(())
}

/// Stops recording and returns the transcript.
#[tauri::command]
pub async fn stop_dictation(state: State<'_, AppState>) -> Result<String, String> {
    let recording = state.recording.lock().unwrap().take().ok_or("not recording")?;
    let wav = tauri::async_runtime::spawn_blocking(move || recording.stop()).await.map_err(|e| e.to_string())??;
    state.stt.transcribe(wav).await
}

#[tauri::command]
pub fn cancel_dictation(state: State<AppState>) {
    state.cancel_dictation();
}
