//! User-editable configuration: `settings.json` and `actions.json` in the app config directory
//! (`%APPDATA%\com.rib.cursorassist`). Missing files are created with defaults on first run.

use std::fs;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub launch_at_login: bool,
    pub llm: LlmSettings,
    pub stt: SttSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self { launch_at_login: true, llm: LlmSettings::default(), stt: SttSettings::default() }
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default())
}

/// Speech-to-text for dictated tasks. The managed server speaks whisper.cpp's `/inference`
/// protocol (multipart `file` = 16 kHz mono WAV, reply `{"text": ...}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SttSettings {
    /// Speech server executable; unset uses the copy bundled with CursorAssist.
    pub server_exe: Option<PathBuf>,
    pub model: PathBuf,
    pub port: u16,
    pub gpu: bool,
    pub threads: u32,
    /// The server holds ~1.3 GB while running, so it is stopped after this many idle seconds and
    /// started again on demand (~5 s).
    pub stop_after_secs: u32,
    /// Use an already-running whisper-server compatible endpoint on this machine instead, e.g.
    /// `http://127.0.0.1:8178/inference`. Non-local addresses are refused.
    pub endpoint: Option<String>,
}

impl Default for SttSettings {
    fn default() -> Self {
        let home = home();
        Self {
            server_exe: None,
            model: home.join(r"models\speech\parakeet\ggml-parakeet-tdt-0.6b-v3-q8_0.bin"),
            port: 8183,
            gpu: true,
            threads: 4,
            stop_after_secs: 120,
            endpoint: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LlmSettings {
    /// llama-server executable for managed models; unset uses the copy bundled with CursorAssist.
    pub server_exe: Option<PathBuf>,
    /// CPU threads per model server; kept low so other apps stay responsive.
    pub threads: u32,
    pub context_size: u32,
    /// A model's weights are unloaded after this many idle seconds and reloaded on the next request.
    pub sleep_after_secs: u32,
    pub models: Vec<ModelSettings>,
}

impl Default for LlmSettings {
    fn default() -> Self {
        let home = home();
        let llm = home.join(r"models\llm");
        Self {
            server_exe: None,
            threads: 4,
            context_size: 4096,
            // GPU weights are fixed memory while awake, so release them soon after use.
            sleep_after_secs: 120,
            // Benchmarked on a Core Ultra 7 165U iGPU: Gemma 4 E2B writes ~9 tok/s with edit quality
            // on par with Qwen3-4B (~6 tok/s), which stays more polished for open-ended writing.
            models: vec![
                ModelSettings::managed(
                    "fast",
                    llm.join(r"google\gemma-4-E2B-it-qat-q4_0-gguf\gemma-4-E2B_q4_0-it.gguf"),
                    8181,
                ),
                ModelSettings::managed(
                    "quality",
                    llm.join(r"unsloth\Qwen3-4B-Instruct-2507-GGUF\Qwen3-4B-Instruct-2507-Q4_K_M.gguf"),
                    8182,
                ),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSettings {
    pub id: String,
    /// GGUF file served by a managed llama-server. Leave unset when `base_url` is used.
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub port: u16,
    /// OpenAI-compatible server on this machine (LM Studio, Ollama, ...) used instead of a managed
    /// one. Non-local addresses are refused: text never leaves the computer.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Model name sent with each request; external servers need it to pick the model.
    #[serde(default)]
    pub name: Option<String>,
    /// Run on the GPU (Vulkan). On an integrated GPU this isn't faster, but it frees the CPU;
    /// the cost is that the weights become fixed (non-reclaimable) memory while the model is awake.
    #[serde(default)]
    pub gpu: bool,
}

impl ModelSettings {
    fn managed(id: &str, path: PathBuf, port: u16) -> Self {
        Self { id: id.into(), path: Some(path), port, base_url: None, name: None, gpu: true }
    }

    pub fn is_managed(&self) -> bool {
        self.base_url.is_none() && self.path.is_some()
    }

    pub fn base_url(&self) -> String {
        self.base_url.clone().unwrap_or_else(|| format!("http://127.0.0.1:{}/v1", self.port))
    }
}

/// True only for plain-HTTP URLs on this machine. Everything CursorAssist talks to must pass this,
/// so selected text and audio can never be sent off the computer by a settings change.
pub fn is_local_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return false; // userinfo tricks like http://127.0.0.1@evil.example
    }
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    matches!(host.to_ascii_lowercase().as_str(), "127.0.0.1" | "localhost" | "::1")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Action {
    pub id: String,
    pub name: String,
    /// Actions sharing a group are shown together as one row, e.g. "Change tone".
    #[serde(default)]
    pub group: Option<String>,
    /// Overrides the default system prompt.
    #[serde(default)]
    pub system: Option<String>,
    /// Template with `{{selection}}`, `{{instruction}}` and `{{app}}` placeholders.
    pub prompt: String,
    /// Id of the model in settings.json that runs this action.
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Hidden actions are not listed in the menu (e.g. `task`, used by the task box).
    #[serde(default)]
    pub hidden: bool,
}

fn default_model() -> String {
    "quality".into()
}

fn default_temperature() -> f32 {
    0.3
}

fn default_max_tokens() -> u32 {
    1024
}

/// The action run by the task box (typed or dictated instructions).
pub const TASK_ACTION: &str = "task";

const TASK_SYSTEM: &str = "You are a capable assistant. The user gives you a piece of text and a task to do \
with it. Do the task. Reply with only the result, ready to use: no preamble, no explanations, no quotes \
around it.";

pub const DEFAULT_SYSTEM: &str = "You are a writing assistant. Apply the instruction to the user's text. \
Reply with only the resulting text: no preamble, no explanations, no quotes around it. \
Keep the original language unless asked otherwise.";

const PROMPT_SYSTEM: &str = "You turn rough notes and requests into prompts for an AI assistant. \
Reply with only the prompt, ready to paste: no preamble, no explanations, no quotes around it.";

const PROMPT_TEMPLATE: &str = "Turn the following text into a clear, well-structured prompt for an AI assistant \
such as ChatGPT or Claude. Make the goal explicit, include the relevant context and constraints from the text, \
and say what form the answer should take. Keep every fact from the text and do not invent new requirements. \
Address the assistant directly.\n\n{{selection}}";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionsFile {
    pub actions: Vec<Action>,
}

impl Default for ActionsFile {
    fn default() -> Self {
        let action = |id: &str, name: &str, group: Option<&str>, model: &str, prompt: &str| Action {
            id: id.into(),
            name: name.into(),
            group: group.map(Into::into),
            system: None,
            prompt: prompt.into(),
            model: model.into(),
            temperature: default_temperature(),
            max_tokens: default_max_tokens(),
            hidden: false,
        };
        let tone = |id: &str, name: &str, tone: &str| {
            action(
                id,
                name,
                Some("Change tone"),
                "fast",
                &format!("Rewrite the following text in a {tone} tone. Keep the meaning.\n\n{{{{selection}}}}"),
            )
        };
        Self {
            actions: vec![
                action(
                    "refine",
                    "Refine",
                    None,
                    "quality",
                    "Refine the following text so it reads clearly and professionally. \
                     Keep the meaning and roughly the same length.\n\n{{selection}}",
                ),
                action(
                    "shorten",
                    "Shorten",
                    None,
                    "fast",
                    "Shorten the following text to about half its length, keeping the key facts.\n\n{{selection}}",
                ),
                action(
                    "grammar",
                    "Fix grammar & spelling",
                    None,
                    "fast",
                    "Fix only spelling, grammar and punctuation in the following text. \
                     Do not change the wording or tone otherwise.\n\n{{selection}}",
                ),
                Action {
                    system: Some(PROMPT_SYSTEM.into()),
                    ..action("prompt", "Turn into a prompt", None, "quality", PROMPT_TEMPLATE)
                },
                tone("tone-formal", "Formal", "formal, professional"),
                tone("tone-casual", "Casual", "casual, relaxed"),
                tone("tone-friendly", "Friendly", "warm, friendly"),
                Action {
                    hidden: true,
                    system: Some(TASK_SYSTEM.into()),
                    ..action(
                        TASK_ACTION,
                        "Task",
                        None,
                        "quality",
                        "Task: {{instruction}}\n\nText:\n\"\"\"\n{{selection}}\n\"\"\"",
                    )
                },
            ],
        }
    }
}

pub fn render(template: &str, selection: &str, instruction: &str, app: &str) -> String {
    template.replace("{{selection}}", selection).replace("{{instruction}}", instruction).replace("{{app}}", app)
}

/// Reads `name` from `dir`, creating it with defaults when missing. A file that fails to parse is
/// left untouched (so the user's edits are not lost) and defaults are used for this run.
pub fn load_or_init<T: Default + Serialize + DeserializeOwned>(dir: &Path, name: &str) -> T {
    let path = dir.join(name);
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            eprintln!("[config] {} is invalid, using defaults: {e}", path.display());
            T::default()
        }),
        Err(_) => {
            let value = T::default();
            if let Err(e) = save(dir, name, &value) {
                eprintln!("[config] could not write {}: {e}", path.display());
            }
            value
        }
    }
}

pub fn save<T: Serialize>(dir: &Path, name: &str, value: &T) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join(name), serde_json::to_string_pretty(value)?)
}

#[cfg(test)]
mod tests {
    use super::is_local_url;

    #[test]
    fn accepts_loopback_addresses() {
        assert!(is_local_url("http://127.0.0.1:8181/v1"));
        assert!(is_local_url("http://localhost:1234/v1"));
        assert!(is_local_url("http://LOCALHOST/v1"));
        assert!(is_local_url("http://[::1]:11434/v1"));
    }

    #[test]
    fn refuses_anything_that_could_leave_the_machine() {
        assert!(!is_local_url("https://api.example.com/v1"));
        assert!(!is_local_url("http://192.168.1.20:1234/v1"));
        assert!(!is_local_url("http://127.0.0.1.evil.example/v1"));
        assert!(!is_local_url("http://localhost.evil.example/v1"));
        assert!(!is_local_url("http://127.0.0.1@evil.example/v1"));
        assert!(!is_local_url("http://user:pw@localhost/v1"));
        assert!(!is_local_url("ftp://127.0.0.1/"));
    }
}
