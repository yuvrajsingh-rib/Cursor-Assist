import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { hydrateIcons, icon } from "./icons";

interface Selection {
  text: string;
  app: string;
  editable: boolean;
}

interface ActionView {
  id: string;
  name: string;
  group: string | null;
}

interface MenuData {
  selection: Selection | null;
  actions: ActionView[];
  recent: number;
}

interface RunStats {
  model: string;
  tokensPerSecond: number | null;
}

type Status = "running" | "done" | "stopped" | "failed";

/** A result kept in the core's in-memory history (see history.rs). */
interface Entry {
  id: number;
  actionId: string;
  title: string;
  instruction: string | null;
  app: string;
  createdMs: number;
  output: string;
  status: Status;
  error: string | null;
  model: string | null;
  tokensPerSecond: number | null;
  used: boolean;
  editable: boolean;
}

/** The result the result view is showing, and where Back goes from it. */
interface Shown {
  entry: Entry;
  replaceable: boolean;
  back: "menu" | "history";
}

const TASK_ACTION = "task";

/** Icons for the built-in actions and groups; custom actions get a generic one. */
const ACTION_ICONS: Record<string, string> = {
  refine: "wand",
  shorten: "shorten",
  grammar: "check",
  prompt: "prompt",
  "Change tone": "tone",
};

/** Room around the card for its shadow; must match `body.panel` padding in styles.css. */
const SHADOW_ROOM = 12;
/** Holding the mic longer than this means push-to-talk: releasing it stops recording. */
const HOLD_THRESHOLD_MS = 450;
const TASK_MAX_HEIGHT = 96;

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
const card = $("card");
const views = { menu: $("menu-view"), result: $("result-view"), history: $("history-view") };
const result = $("result");
const status = $("status");
const task = $<HTMLTextAreaElement>("task");
const taskForm = $<HTMLFormElement>("task-form");
const taskNote = $("task-note");
const taskRun = $<HTMLButtonElement>("task-run");
const mic = $<HTMLButtonElement>("mic");
const listening = $("listening");
const meterBars = [...$("meter").children] as HTMLElement[];
const replaceBtn = $<HTMLButtonElement>("replace");
const copyBtn = $<HTMLButtonElement>("copy");
const retryBtn = $<HTMLButtonElement>("retry");
const stopBtn = $<HTMLButtonElement>("stop");

let selection: Selection | null = null;
/** Actions whose model was already asked to wake since the menu opened. */
let prepared = new Set<string>();
let shown: Shown | null = null;
let lastId = 0;

type Dictation = "idle" | "recording" | "transcribing";
let dictation: Dictation = "idle";
let pressedAt = 0;
let timerHandle = 0;

hydrateIcons();

/** Sizes the window to the card; the panel only becomes visible once this has run. */
function fit() {
  invoke("fit_panel", { width: card.offsetWidth + SHADOW_ROOM * 2, height: card.offsetHeight + SHADOW_ROOM * 2 });
}
new ResizeObserver(fit).observe(card);

/** Run ids double as history ids, so they must be unique for the whole session. */
function nextId() {
  lastId = Math.max(lastId + 1, Date.now());
  return lastId;
}

/** Wakes the model behind an action as soon as the user shows interest in it. */
function prepare(actionId: string) {
  if (prepared.has(actionId)) return;
  prepared.add(actionId);
  invoke("prepare_action", { actionId });
}

function showView(view: keyof typeof views) {
  for (const [name, el] of Object.entries(views)) el.hidden = name !== view;
  if (view !== "result") {
    shown = null;
    invoke("set_viewing", { id: null });
  }
}

/** Plays the entrance animation and shows the panel once it has its final size. */
function present() {
  card.classList.remove("enter");
  void card.offsetWidth; // restart the animation
  card.classList.add("enter");
  fit();
}

// ---------- Menu ----------

async function openMenu() {
  prepared = new Set();
  setDictation("idle");
  task.value = "";
  autoGrow();
  note("");
  const data = await invoke<MenuData>("menu_data");
  selection = data.selection;
  renderActions(data.actions);
  $("recent-row").hidden = data.recent === 0;
  $("recent-count").textContent = String(data.recent);
  showView("menu");
  present();
}

function renderActions(actions: ActionView[]) {
  const container = $("actions");
  container.replaceChildren();
  const groups = new Map<string, HTMLElement>();
  for (const action of actions) {
    if (!action.group) {
      const row = document.createElement("button");
      row.className = "row";
      row.setAttribute("role", "menuitem");
      row.innerHTML = `${icon(ACTION_ICONS[action.id] ?? "bolt")}<span></span>`;
      row.querySelector("span")!.textContent = action.name;
      row.addEventListener("pointerenter", () => prepare(action.id));
      row.addEventListener("click", () => run(action.id));
      container.append(row);
      continue;
    }
    let chips = groups.get(action.group);
    if (!chips) {
      const row = document.createElement("div");
      row.className = "row group";
      row.innerHTML = `${icon(ACTION_ICONS[action.group] ?? "bolt")}<span></span><div class="chips"></div>`;
      row.querySelector("span")!.textContent = action.group.replace(/^Change /, "");
      chips = row.querySelector<HTMLElement>(".chips")!;
      // Entering the row is enough of a hint: its chips usually share one model.
      row.addEventListener("pointerenter", () => prepare(action.id));
      container.append(row);
      groups.set(action.group, chips);
    }
    const chip = document.createElement("button");
    chip.className = "chip";
    chip.setAttribute("role", "menuitem");
    chip.textContent = action.name;
    chip.addEventListener("pointerenter", () => prepare(action.id));
    chip.addEventListener("click", () => run(action.id));
    chips.append(chip);
  }
}

// ---------- Results ----------

/**
 * Starts an action. With `fromEntry` it reruns on that result's original selection (Retry);
 * otherwise on the current selection. The result streams into the history, so it survives the
 * panel being closed.
 */
async function run(actionId: string, instruction?: string, fromEntry?: Entry) {
  const id = nextId();
  const placeholder: Entry = {
    id,
    actionId,
    title: "",
    instruction: instruction ?? null,
    app: fromEntry?.app ?? selection?.app ?? "",
    createdMs: Date.now(),
    output: "",
    status: "running",
    error: null,
    model: null,
    tokensPerSecond: null,
    used: false,
    editable: fromEntry?.editable ?? selection?.editable ?? true,
  };
  const back = shown?.back ?? (fromEntry ? "history" : "menu");
  const replaceable = fromEntry ? (shown?.replaceable ?? false) : placeholder.editable;
  showEntry(placeholder, replaceable, back);
  try {
    await invoke("run_action", { runId: id, actionId, instruction: instruction ?? null, fromEntry: fromEntry?.id ?? null });
    const entry = await invoke<Entry | null>("history_entry", { id });
    // The core names the result; keep whatever has streamed in meanwhile.
    if (entry && shown?.entry.id === id) $("result-title").textContent = entry.title;
  } catch (e) {
    if (shown?.entry.id === id) {
      shown.entry.status = "failed";
      shown.entry.error = String(e);
      renderStatus();
    }
  }
}

/** Shows a result (live, reopened from the dot, or from Recent results). */
function showEntry(entry: Entry, replaceable: boolean, back: Shown["back"]) {
  shown = { entry, replaceable, back };
  $("result-title").textContent = entry.title;
  result.textContent = entry.output.trimStart();
  showView("result");
  shown = { entry, replaceable, back }; // showView clears it for other views
  invoke("set_viewing", { id: entry.id });
  renderStatus();
}

function renderStatus() {
  if (!shown) return;
  const { entry, replaceable } = shown;
  const running = entry.status === "running";
  const hasText = entry.output.trim().length > 0;
  result.classList.toggle("waiting", running && !hasText);
  result.classList.toggle("streaming", running && hasText);
  status.className = "status";
  switch (entry.status) {
    case "running":
      status.textContent = hasText ? "Writing" : "Thinking";
      status.classList.add("pending");
      break;
    case "done": {
      const speed = entry.tokensPerSecond ? ` · ${entry.tokensPerSecond.toFixed(0)} tok/s` : "";
      status.textContent = hasText ? `${entry.model ?? ""}${speed}` : "No answer. Try again.";
      if (!hasText) status.classList.add("bad");
      break;
    }
    case "stopped":
      status.textContent = "Stopped";
      break;
    case "failed":
      status.textContent = entry.error ?? "Something went wrong";
      status.classList.add("bad");
      break;
  }
  stopBtn.hidden = !running;
  retryBtn.hidden = running;
  copyBtn.disabled = running || !hasText;
  replaceBtn.hidden = !replaceable;
  replaceBtn.disabled = running || !hasText;
  // When the result can't be pasted back, Copy is the main action.
  copyBtn.classList.toggle("primary", !replaceable);
}

// ---------- Recent results ----------

function timeAgo(ms: number) {
  const s = Math.round((Date.now() - ms) / 1000);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  return `${Math.floor(s / 3600)} h ago`;
}

async function openHistory() {
  const entries = await invoke<Entry[]>("history_list");
  const list = $("history-list");
  list.replaceChildren();
  if (!entries.length) {
    const empty = document.createElement("p");
    empty.className = "history-empty";
    empty.textContent = "Nothing yet. Results from this session appear here.";
    list.append(empty);
  }
  for (const entry of entries) {
    const item = document.createElement("button");
    item.className = "history-item";
    const head = document.createElement("span");
    head.className = "history-head";
    const title = document.createElement("span");
    title.className = "history-name";
    title.textContent = entry.title;
    const when = document.createElement("span");
    when.className = "history-when";
    when.textContent = entry.status === "running" ? "writing…" : timeAgo(entry.createdMs);
    head.append(title, when);
    const preview = document.createElement("span");
    preview.className = "history-preview";
    preview.textContent =
      entry.status === "failed" ? entry.error ?? "Failed" : entry.output.trim() || (entry.status === "stopped" ? "Stopped" : "…");
    const meta = document.createElement("span");
    meta.className = "history-meta";
    meta.textContent = `${entry.app}${entry.used ? " · used" : ""}`;
    item.append(head, preview, meta);
    // From the list, the original selection is long gone, so only Copy is offered.
    item.addEventListener("click", () => showEntry(entry, false, "history"));
    list.append(item);
  }
  showView("history");
  present();
}

async function openEntry(id: number) {
  const entry = await invoke<Entry | null>("history_entry", { id });
  if (!entry) return openHistory();
  // Reopened from the dot: the source text is where it was, so Replace still makes sense.
  showEntry(entry, entry.editable, "history");
  present();
}

// ---------- Task box ----------

function autoGrow() {
  task.style.height = "auto";
  task.style.height = `${Math.min(task.scrollHeight, TASK_MAX_HEIGHT)}px`;
  task.style.overflowY = task.scrollHeight > TASK_MAX_HEIGHT ? "auto" : "hidden";
  taskRun.hidden = dictation !== "idle" || !task.value.trim();
}

function note(message: string, bad = false) {
  taskNote.textContent = message;
  taskNote.hidden = !message;
  taskNote.classList.toggle("bad", bad);
}

function submitTask() {
  const instruction = task.value.trim();
  if (instruction && dictation === "idle") run(TASK_ACTION, instruction);
}

/**
 * The panel never takes focus on its own (so clicking it keeps focus in the user's app); ask for
 * it when the user wants to type, then put the caret back in the box once the window has it.
 */
async function focusTask() {
  prepare(TASK_ACTION);
  task.focus();
  await invoke("focus_panel");
  task.focus();
}

// ---------- Dictation ----------

function setDictation(next: Dictation) {
  dictation = next;
  taskForm.classList.toggle("recording", next === "recording");
  taskForm.classList.toggle("transcribing", next === "transcribing");
  listening.hidden = next === "idle";
  task.hidden = next !== "idle";
  $("listening-label").textContent = next === "transcribing" ? "Transcribing" : "Listening";
  mic.innerHTML = icon(next === "recording" ? "stop" : "mic");
  mic.title = next === "recording" ? "Stop and transcribe" : "Dictate: click to start and stop, or hold to talk";
  mic.disabled = next === "transcribing";
  clearInterval(timerHandle);
  if (next === "recording") {
    const started = performance.now();
    $("timer").textContent = "0:00";
    timerHandle = window.setInterval(() => {
      const s = Math.floor((performance.now() - started) / 1000);
      $("timer").textContent = `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
    }, 250);
  }
  if (next !== "recording") meterBars.forEach((bar) => (bar.style.transform = ""));
  autoGrow();
}

async function startDictation() {
  note("");
  prepare(TASK_ACTION);
  try {
    await invoke("start_dictation");
    setDictation("recording");
  } catch (e) {
    note(String(e), true);
  }
}

async function stopDictation() {
  setDictation("transcribing");
  try {
    const text = await invoke<string>("stop_dictation");
    setDictation("idle");
    task.value = task.value.trim() ? `${task.value.trim()} ${text}` : text;
    autoGrow();
    note("Edit if needed, then press Enter.");
    await focusTask();
    task.setSelectionRange(task.value.length, task.value.length);
  } catch (e) {
    setDictation("idle");
    note(String(e), true);
  }
}

// ---------- Events ----------

const win = getCurrentWebviewWindow();
win.listen("menu-open", () => openMenu());
win.listen("history-open", () => openHistory());
win.listen<number>("entry-open", ({ payload }) => openEntry(payload));
win.listen<{ runId: number; text: string }>("llm-delta", ({ payload }) => {
  if (shown?.entry.id !== payload.runId) return;
  const entry = shown.entry;
  const first = !entry.output;
  entry.output += payload.text;
  result.textContent = entry.output.trimStart();
  result.scrollTop = result.scrollHeight;
  if (first) renderStatus();
});
win.listen<{ runId: number; stats: RunStats }>("llm-done", ({ payload }) => {
  if (shown?.entry.id !== payload.runId) return;
  Object.assign(shown.entry, { status: "done", model: payload.stats.model, tokensPerSecond: payload.stats.tokensPerSecond });
  renderStatus();
});
win.listen<{ runId: number; message: string }>("llm-error", ({ payload }) => {
  if (shown?.entry.id !== payload.runId) return;
  Object.assign(shown.entry, { status: "failed", error: payload.message });
  renderStatus();
});
win.listen<number>("mic-level", ({ payload }) => {
  // Middle bars react most, like a voice meter.
  const shape = [0.55, 0.85, 1, 0.85, 0.55];
  meterBars.forEach((bar, i) => {
    const jitter = 0.75 + Math.random() * 0.5;
    bar.style.transform = `scaleY(${Math.max(0.15, Math.min(1, payload * shape[i] * jitter))})`;
  });
});

taskForm.addEventListener("submit", (e) => {
  e.preventDefault();
  submitTask();
});
// Anywhere in the task row except its buttons focuses the box.
taskForm.addEventListener("mousedown", (e) => {
  if (dictation === "idle" && !(e.target as HTMLElement).closest("button")) focusTask();
});
task.addEventListener("input", () => {
  autoGrow();
  note("");
});
task.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    submitTask();
  }
});

mic.addEventListener("pointerenter", () => {
  invoke("prepare_dictation");
  prepare(TASK_ACTION);
});
mic.addEventListener("pointerdown", (e) => {
  e.preventDefault();
  if (dictation === "recording") {
    stopDictation();
  } else if (dictation === "idle") {
    pressedAt = performance.now();
    startDictation();
  }
});
mic.addEventListener("pointerup", () => {
  if (dictation === "recording" && performance.now() - pressedAt > HOLD_THRESHOLD_MS) stopDictation();
});

$("recent-row").addEventListener("click", () => openHistory());
document.querySelectorAll(".close").forEach((b) => b.addEventListener("click", () => invoke("close_panel")));
$("back").addEventListener("click", () => {
  // The result keeps streaming into Recent results; Stop is the way to cancel it.
  if (shown?.back === "menu" && selection) {
    showView("menu");
  } else {
    openHistory();
  }
});
stopBtn.addEventListener("click", () => {
  invoke("cancel_run");
  if (shown) {
    shown.entry.status = "stopped";
    renderStatus();
  }
});
retryBtn.addEventListener("click", () => {
  if (shown) run(shown.entry.actionId, shown.entry.instruction ?? undefined, shown.entry);
});
copyBtn.addEventListener("click", () => shown && invoke("apply_result", { id: shown.entry.id, replace: false }));
replaceBtn.addEventListener("click", () => shown && invoke("apply_result", { id: shown.entry.id, replace: true }));
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") return;
  if (dictation === "recording") {
    invoke("cancel_dictation");
    setDictation("idle");
  } else {
    invoke("close_panel");
  }
});
