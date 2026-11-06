# CursorAssist — Scope & Plan

A lightweight Windows tray app. When you highlight text in **any** application, a small bubble appears near the cursor. Clicking it opens a menu of AI actions (Refine, Shorten, Fix grammar, Change tone, custom actions, or a spoken instruction). Everything runs on **local models**. Nothing leaves the machine.

## 1. Decisions so far

| Area | Decision |
|---|---|
| Reach | System-wide on Windows (any app) |
| Trigger | Small bubble near the cursor when a selection is detected |
| Output | Preview popup with **Copy / Replace / Retry** |
| LLM | Any **OpenAI-compatible local server** (Ollama, LM Studio, llama.cpp server…) |
| Voice ("narrate") | Mic button → local speech-to-text → the spoken text runs as a custom instruction on the selection |
| LLM model | **Qwen3-4B-Instruct-2507** (Q4_K_M GGUF), served by LM Studio or `llama-server` |
| Speech-to-text | **Parakeet TDT 0.6B v3** (ggml q8_0) via the existing `parakeet-server.exe`, which the app can launch as a managed sidecar. Any whisper-server `/inference` or OpenAI `/v1/audio/transcriptions` endpoint can be set as an override |
| Stack | **Tauri 2**: Rust core + plain TypeScript + CSS UI (no framework) |
| Hardware | Intel integrated GPU, no discrete GPU, so the defaults are tuned for CPU or Vulkan |
| Custom actions | Settings UI backed by an editable `actions.json` (hot-reloaded) |
| Privacy | No trigger in password fields, app blocklist, pause toggle, opt-in local history |

## 2. In scope (v1)

### 2.1 Selection capture
- Detect the **end of a selection** with low-level mouse and keyboard hooks:
  - mouse up after a drag
  - double-click or triple-click
  - Shift+arrow / Ctrl+A keyboard selections, after a short debounce
- Read the selected text in two steps:
  1. **UI Automation** `TextPattern.GetSelection()` first. This doesn't touch the clipboard and works in Word, Edge/Chrome, Notepad and WPF apps.
  2. **Clipboard fallback** if that fails: save the clipboard, send Ctrl+C, read the text, then restore the clipboard.
- Ignore selections that are empty, whitespace-only, or very long (configurable limit, e.g. 8k chars).

### 2.2 Bubble
- A small always-on-top window that **doesn't take focus** (`WS_EX_NOACTIVATE`), shown just below or right of the selection end. Keeping focus in the source app is essential for Replace to work.
- It auto-hides after N seconds, on Esc, or when you click or type elsewhere.
- Correct placement on multi-monitor and mixed-DPI setups.

### 2.3 Action menu
- Built-in actions: **Refine, Shorten, Fix grammar/spelling, Change tone →** (Formal, Casual, Friendly).
- User-defined custom actions (see §2.6).
- A **🎤 mic button** for spoken instructions.
- A free-text box ("Ask anything about this text…") as the typed equivalent of the mic.
- Optional per-action hotkeys, e.g. select text and press Ctrl+Alt+R to Refine.

### 2.4 Preview popup
- Shows the result **streaming** as it's generated.
- Buttons:
  - **Copy**
  - **Replace**: refocus the source window, paste over the selection, then restore the clipboard
  - **Retry**
  - **Edit prompt and re-run**
- An optional diff view (original vs result) for Fix grammar.
- Shows which model was used and how long it took.

### 2.5 Voice instruction
- Push-to-talk (hold the button) or click to toggle recording, with a level meter.
- Record 16 kHz mono PCM16 WAV in Rust (`cpal`).
- Transcribe with **Parakeet TDT 0.6B v3**. Ollama and LM Studio don't serve speech-to-text models, so Parakeet runs through whisper.cpp's Parakeet support:
  - **Managed sidecar (default)**: settings point at `parakeet-server.exe` and the model file. The app starts it on demand on `127.0.0.1:8178`, waits for `GET /` to return `ok`, and stops it on quit. To the user it behaves like an embedded model.
  - **External endpoint (override)**: a URL plus protocol, either `whisper-server` (`POST /inference`, multipart `file`, reply `{"text"}`) or OpenAI (`POST /v1/audio/transcriptions`).
  - Later option: link whisper.cpp's Parakeet API into the Rust app through FFI and drop the sidecar.
- The transcript is shown and can be edited, then runs as `{{instruction}}` on the selection.

### 2.6 Custom actions — `actions.json`
```json
{
  "id": "jira-ticket",
  "name": "Make Jira ticket",
  "icon": "ticket",
  "hotkey": "Ctrl+Alt+J",
  "system": "You rewrite text. Output only the result.",
  "prompt": "Turn the following into a Jira ticket with Title, Description and Acceptance Criteria:\n\n{{selection}}",
  "temperature": 0.3,
  "model": null
}
```
- Template variables: `{{selection}}`, `{{instruction}}` (voice or typed), `{{app}}` (source process name), `{{language}}`.
- The built-in actions are defined in the same file, so they can be edited or hidden too.
- Optional per-action model override, e.g. a small fast model for grammar and a bigger one for refine.

### 2.7 LLM connection
- Settings: base URL, model name, optional API key, timeout. Presets:
  - LM Studio: `http://localhost:1234/v1`
  - llama-server: `http://localhost:8080/v1`
  - Ollama: `http://localhost:11434/v1`
- Uses `POST /chat/completions` with `stream: true`.
- **Qwen3 handling**: the 2507 Instruct build has no thinking mode, but any `<think>…</think>` block is stripped defensively in case a thinking-mode Qwen3 is configured.
- Optional managed `llama-server.exe` sidecar, same pattern as speech-to-text, so the whole tool can run without LM Studio open.
- A **Test connection** button, plus a "list models" dropdown via `GET /models`.
- Clear error states: server not running, model not pulled, timeout.

### 2.8 Privacy & control
- **Password fields**: never trigger when the UIA element has `IsPassword = true`.
- **App blocklist** by process name, editable in settings. Sensible defaults: password managers, `mstsc`, `cmd`/terminals are optional.
- **Pause**: from the tray menu, a global hotkey, or "pause for 1 hour".
- **History** is opt-in and stored in SQLite under `%APPDATA%`. It has a retention limit (count or days), "Clear history", and searchable reuse.

### 2.9 App shell
- Tray icon with Pause, Settings, History and Quit.
- Option to start with Windows.
- Settings stored as JSON in `%APPDATA%\CursorAssist\`.
- Light and dark themes that follow Windows. Theme tokens are in CSS so they're easy to restyle.
- Installer: NSIS or MSI via the Tauri bundler. Target installer size is **< 15 MB**; speech models are downloaded separately.

## 3. Out of scope for v1 (candidates for later)
- macOS / Linux
- Cloud LLM providers. The OpenAI-compatible config could point at one, but it's not designed or tested for that.
- Multi-turn chat about a selection
- OCR / image selections
- Text-to-speech read-aloud
- Sharing action packs across a team
- Auto-update

## 4. Known risks & constraints

| Risk | Impact | Mitigation |
|---|---|---|
| Not every app exposes selection via UI Automation (some Electron apps, Java, games, Citrix/RDP) | Text not captured | Clipboard fallback, and a per-app "force clipboard mode" option |
| Clipboard fallback can clobber the user's clipboard or trigger clipboard managers | Annoyance or data loss | Save and restore all clipboard formats; keep the window short; mark our writes so managers can ignore them |
| **UIPI**: a non-elevated app can't hook or send input to elevated (admin) windows | No bubble in admin apps | Document it; optional "run elevated" setting |
| The bubble steals focus | Replace pastes into the wrong place | No-activate window, and remember the source HWND and caret before acting |
| False triggers, e.g. drag-to-move files or selecting while reading | Noise | Minimum selection length, debounce, only trigger on text-capable elements, per-app blocklist |
| A 4B model on CPU or iGPU runs at roughly 8–15 tokens/s | Long rewrites feel slow | Streaming output, keep the model warm, cap the selection length, concise prompts |
| Parakeet sidecar cold start (loading the model takes a few seconds) | First voice command lags | Start the sidecar when the mic button is first hovered, or keep it warm while the app runs (setting) |

## 5. Non-functional targets
- Idle RAM **< 60 MB**; idle CPU about 0%.
- The bubble appears **< 150 ms** after the selection ends.
- First streamed token **< 2 s** for a typical paragraph with Qwen3-4B Q4 on this machine (model warm).
- Voice: transcript **< 1.5 s** after release for a 5 s clip (Parakeet warm).
- Zero network calls except to the configured local endpoints (all `127.0.0.1`).

## 6. Architecture sketch

```
┌──────────────────────── Rust core (Tauri) ────────────────────────┐
│ hooks/       WH_MOUSE_LL + WH_KEYBOARD_LL → selection-end events  │
│ capture/     UIA TextPattern → clipboard fallback; password check │
│ guard/       blocklist, pause state, length limits                │
│ inject/      refocus HWND + clipboard paste (Replace)             │
│ llm/         OpenAI-compatible client, SSE streaming              │
│ stt/         cpal mic → WAV → parakeet-server sidecar | endpoint  │
│ sidecar/     start/health-check/stop parakeet-server, llama-server│
│ actions/     actions.json load / validate / hot-reload / render   │
│ store/       settings.json, SQLite history (opt-in)               │
│ tray/        tray menu, global hotkeys, autostart                 │
└──────────────── events / commands (Tauri IPC) ────────────────────┘
┌──────────────── Web UI (TS + plain CSS or Svelte) ────────────────┐
│ bubble.html   menu.html   preview.html   settings.html   history  │
└───────────────────────────────────────────────────────────────────┘
```
Windows are created hidden at startup and shown or hidden on demand, so they appear instantly.

## 7. Delivery plan (risk-first)

| Phase | Goal | Done when |
|---|---|---|
| **0. Spike** | Prove the hardest part: system-wide capture, plus a bubble that doesn't steal focus | Selecting text in Notepad, Edge, Word, Outlook and Teams shows the bubble with the correct text; focus stays in the source app |
| **1. Core loop** | Built-in actions, LLM streaming, preview with Copy/Replace/Retry | Refine/Shorten/Grammar/Tone work end to end against Ollama; Replace works in the five apps above |
| **2. Customisation** | `actions.json`, settings UI, custom actions, per-action hotkeys and models | A user adds a "Make Jira ticket" action in the UI and it shows up in the menu without a restart |
| **3. Voice** | Mic, embedded whisper, endpoint override, editable transcript | Speak "turn this into bullet points" and it runs on the selection |
| **4. Privacy & polish** | Password detection, blocklist, pause, history, tray, autostart, installer, themes | Installer under 15 MB; the guardrails are verified by a manual test checklist |

### Phase 0 results (2026-10-07) ✅

| App | Capture method | Typical latency | Focus kept on bubble click |
|---|---|---|---|
| Notepad | UI Automation | 10–14 ms | ✅ |
| Edge | UI Automation (one case needed the clipboard fallback) | 8–28 ms (126 ms via clipboard) | ✅ |
| Word | UI Automation | 90–210 ms | ✅ |
| Teams (new) | UI Automation | 25–29 ms | ✅ |
| Outlook (new, `olk.exe`) | Clipboard fallback | ~185 ms | ✅ |

What we learned:
- WebView2 apps (Teams, new Outlook) put the TextPattern on an ancestor more than 6 levels above the focused node. The ancestor walk was raised to 20 levels.
- Teams shows an arrow cursor over message text, so the cursor shape can't gate the clipboard fallback. The gate now also accepts a drag that starts inside an Edit, Document or Text element or one of its ancestors.
- Swallowing the bubble click in the low-level mouse hook, together with `WS_EX_NOACTIVATE`, keeps focus in the source app everywhere we tested.
- Follow-up: Word's UI Automation is sometimes slow (~200 ms). New Outlook still needs the clipboard; look into its UIA tree in Phase 1.

### Phases 1–4 status (2026-10-22)
- **Done:** built-in actions with streaming and Copy/Replace/Retry; per-action models in `actions.json`; Task mode with Parakeet dictation; ghost-dot bubble with RGB glow; privacy/security hardening (below); tray (Pause, Start with Windows, settings folder); per-user NSIS installer.
- **Changed from plan:** both models run on the iGPU (frees the CPU; not faster on shared memory). Quick actions (Grammar, Shorten, Tone) use **Gemma 4 E2B** (~9 tok/s, edit quality on par with Qwen3-4B). Refine and Task use **Qwen3-4B** (~6 tok/s, more polished open-ended writing). Qwen3-1.7B was tried and dropped for quality.
- **Recent results (2026-10-23):** generation keeps going when the panel is dismissed. The dot stays as a marker (spinner while writing, ✓ when done, 2 minutes) and reopens the result with Replace still available. The last 20 results are under the menu's Recent row, the tray and Ctrl+Alt+H. They are kept in memory only.
- **Not yet:** settings UI (settings are JSON files), per-action hotkeys.

## 8. Privacy & security (as built)

**Nothing leaves the computer**
- All model and speech traffic goes to helper servers on `127.0.0.1`. Any `baseUrl` or `endpoint` in settings that isn't loopback (`127.0.0.1`, `localhost`, `::1`) is refused, and the HTTP client ignores system and corporate proxy settings.
- No telemetry, updater or model downloads. The WebView runs with SmartScreen, background networking, component updates, sync, pings and crash reporting disabled.
- Strict CSP: the overlay pages load only bundled files and can only talk to the Rust core over IPC.

**Local attack surface**
- Model servers require a random 256-bit key, generated at each launch, held only in memory and passed via environment variable. This means other programs, and web pages in a browser (llama-server allows any origin), can't use them. Their web UI and `/slots` endpoints are turned off.
- The Parakeet server has no authentication. It listens on 127.0.0.1 only, runs only during dictation plus 2 minutes, and sends no CORS headers, so pages can't read results.
- The bubble window has no IPC permissions; the panel can only listen to events.
- Helper servers run at below-normal priority in a kill-on-close job object, so they never outlive CursorAssist. Only one CursorAssist instance can run per session.

**What is kept, and where**
- Selected text lives in memory only until the bubble or panel is dismissed, or the result is copied or pasted.
- The clipboard is restored after capture or Replace, and those temporary writes are excluded from Windows clipboard history.
- Release builds never write audio to disk. Logs (`%LOCALAPPDATA%\com.rib.cursorassist\logs`) contain server status and timings, never text.
- Capture is skipped in password fields, password managers and Windows sign-in or credential prompts. The input hooks only track mouse buttons and modifier or navigation keys; nothing typed is stored.

## 9. Resolved questions
1. App name: **CursorAssist**.
2. UI: **plain TypeScript + CSS** (lightest).
3. Read-only text (web pages, PDFs): the bubble **does** appear, with Replace disabled.
4. Phase 0 acceptance apps: **Notepad, Edge, Word, Outlook, Teams**.
5. Hardware: Intel integrated GPU, so the defaults are Qwen3-4B Q4 and Parakeet 0.6B q8_0.

## 10. Local environment (as found)
- Node 25.5, npm 11.8, MSVC Build Tools (VS 18), WebView2 154: ✅
- **Rust toolchain: missing.** Needs `rustup` before Phase 0.
- Ollama 0.35 is running, but only `llama3.2:3b` is pulled.
- LM Studio: `models\local\Qwen3-4B` exists but is **empty**, and the server isn't running.
- Existing binaries and models in `Downloads\voice-scratchpad-app\`:
  - `ai\bin\llama-server.exe`
  - `ai\models\Qwen3-4B-Instruct-2507-Q4_K_M.gguf`
  - `whisper\bin\parakeet-server.exe`
  - `whisper\models\ggml-parakeet-tdt-0.6b-v3-q8_0.bin`
