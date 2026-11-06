//! Local model hosting and streaming chat completions.
//!
//! Managed models run in their own `llama-server` processes, tuned to stay out of the way:
//! - on the GPU (default) the CPU stays free while generating; on CPU the weights are
//!   memory-mapped, so they live in the OS file cache and Windows can reclaim them under pressure;
//! - a small, quantized KV cache and no prompt cache instead of the server defaults (~17 GB);
//! - below-normal priority and a few threads, so other apps stay responsive;
//! - weights are unloaded after `sleep_after_secs` idle and reloaded on the next request.
//! Servers are started through `process::spawn`, so they never outlive CursorAssist.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

use crate::config::{self, LlmSettings, ModelSettings};
use crate::process;

/// How long a request waits for a server that is still starting or reloading its weights.
const STARTUP_GRACE: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunStats {
    pub model: String,
    pub tokens_per_second: Option<f64>,
}

pub struct LlmHost {
    settings: LlmSettings,
    server_exe: PathBuf,
    log_dir: PathBuf,
    client: reqwest::Client,
    /// Required by our managed servers on every request; see `process::random_key`.
    api_key: String,
    children: Mutex<HashMap<String, Child>>,
    waking: Mutex<HashSet<String>>,
}

impl LlmHost {
    /// `bundled_exe` is used unless settings.json names another llama-server.
    pub fn new(settings: LlmSettings, bundled_exe: PathBuf, log_dir: PathBuf) -> Self {
        Self {
            server_exe: settings.server_exe.clone().unwrap_or(bundled_exe),
            settings,
            log_dir,
            client: process::local_http_client(),
            api_key: process::random_key(),
            children: Mutex::new(HashMap::new()),
            waking: Mutex::new(HashSet::new()),
        }
    }

    fn model(&self, id: &str) -> Result<&ModelSettings, String> {
        let model = self
            .settings
            .models
            .iter()
            .find(|m| m.id == id)
            .ok_or_else(|| format!("no model '{id}' in settings.json"))?;
        if !config::is_local_url(&model.base_url()) {
            return Err(format!("model '{id}': only servers on this computer are allowed (baseUrl)"));
        }
        Ok(model)
    }

    /// Starts the managed servers for the given model ids; they load, then fall asleep until first
    /// used. Other models start on demand when an action first needs them.
    pub fn start(&self, ids: &HashSet<String>) {
        for model in self.settings.models.iter().filter(|m| m.is_managed() && ids.contains(&m.id)) {
            if let Err(e) = self.ensure_running(model) {
                eprintln!("[llm] {e}");
            }
        }
    }

    /// (Re)starts a managed server if it is not running. External endpoints are left alone.
    fn ensure_running(&self, model: &ModelSettings) -> Result<(), String> {
        let Some(path) = model.path.as_ref().filter(|_| model.is_managed()) else { return Ok(()) };
        let mut children = self.children.lock().unwrap();
        if let Some(child) = children.get_mut(&model.id) {
            if matches!(child.try_wait(), Ok(None)) {
                return Ok(());
            }
            eprintln!("[llm] '{}' server exited, restarting", model.id);
        }

        let exe = &self.server_exe;
        if !exe.exists() {
            return Err(format!("llama-server not found at {}", exe.display()));
        }
        if !path.exists() {
            return Err(format!("model file for '{}' not found at {}", model.id, path.display()));
        }
        let _ = std::fs::create_dir_all(&self.log_dir);
        let log = File::create(self.log_dir.join(format!("llama-{}.log", model.id))).map_err(|e| e.to_string())?;

        let s = &self.settings;
        // CPU: memory-map the weights so they stay reclaimable file cache.
        // GPU: offload every layer; llama.cpp picks the load mode the device supports.
        let placement: &[&str] =
            if model.gpu { &["--n-gpu-layers", "99"] } else { &["--n-gpu-layers", "0", "--load-mode", "mmap"] };
        let mut command = Command::new(exe);
        command
            .arg("-m")
            .arg(path)
            .args(["--host", "127.0.0.1", "--port", &model.port.to_string(), "--parallel", "1"])
            .args(placement)
            .args(["--ctx-size", &s.context_size.to_string(), "--cache-type-k", "q8_0", "--cache-type-v", "q8_0"])
            .args(["--flash-attn", "on", "--cache-ram", "0", "--no-warmup", "--poll", "0"])
            .args(["--threads", &s.threads.to_string(), "--reasoning", "off"])
            .args(["--sleep-idle-seconds", &s.sleep_after_secs.to_string()])
            // Only the chat API: no browser UI or monitoring endpoints to attack.
            .args(["--no-webui", "--no-slots"])
            // Passed through the environment so it doesn't show up in the process command line.
            .env("LLAMA_API_KEY", &self.api_key);
        let child = process::spawn(command, log)
            .map_err(|e| format!("could not start llama-server for '{}': {e}", model.id))?;

        let device = if model.gpu { "GPU" } else { "CPU" };
        eprintln!("[llm] started '{}' on {device}, port {} (pid {})", model.id, model.port, child.id());
        children.insert(model.id.clone(), child);
        Ok(())
    }

    /// Wakes one model with a 1-token request so its weights are loaded by the time the user
    /// commits to an action. Health checks don't wake a sleeping llama-server; a completion does.
    /// Concurrent requests for the same model collapse into one.
    pub async fn wake(&self, model_id: &str) {
        if !self.waking.lock().unwrap().insert(model_id.to_string()) {
            return;
        }
        let started = Instant::now();
        match self.stream(model_id, "", "hi", 0.0, 1, |_| {}).await {
            Ok(_) => eprintln!("[llm] '{model_id}' ready ({:.1}s)", started.elapsed().as_secs_f32()),
            Err(e) => eprintln!("[llm] could not wake '{model_id}': {e}"),
        }
        self.waking.lock().unwrap().remove(model_id);
    }

    /// Streams a chat completion, calling `on_delta` with each piece of visible text.
    pub async fn stream(
        &self,
        model_id: &str,
        system: &str,
        user: &str,
        temperature: f32,
        max_tokens: u32,
        mut on_delta: impl FnMut(&str),
    ) -> Result<RunStats, String> {
        let model = self.model(model_id)?.clone();
        self.ensure_running(&model)?;

        let mut messages = Vec::new();
        if !system.is_empty() {
            messages.push(json!({ "role": "system", "content": system }));
        }
        messages.push(json!({ "role": "user", "content": user }));
        let body = json!({
            "model": model.name.clone().unwrap_or_else(|| model.id.clone()),
            "messages": messages,
            "stream": true,
            "temperature": temperature,
            "max_tokens": max_tokens,
        });
        let url = format!("{}/chat/completions", model.base_url());

        // The server may still be starting (connection refused) or reloading weights (503).
        let started = Instant::now();
        let mut response = loop {
            let mut request = self.client.post(&url).json(&body);
            if model.is_managed() {
                request = request.bearer_auth(&self.api_key);
            }
            let retry = match request.send().await {
                Ok(r) if r.status().is_success() => break r,
                Ok(r) if r.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE => "model is loading".to_string(),
                Ok(r) => {
                    let status = r.status();
                    return Err(format!("{status}: {}", r.text().await.unwrap_or_default()));
                }
                Err(e) if e.is_connect() => format!("cannot reach {url}"),
                Err(e) => return Err(e.to_string()),
            };
            if started.elapsed() > STARTUP_GRACE {
                return Err(retry);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        };

        let mut stats = RunStats { model: model.id.clone(), tokens_per_second: None };
        let mut filter = ThinkFilter::default();
        let mut pending: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            pending.extend_from_slice(&chunk);
            // Only decode complete lines, so multi-byte characters split across chunks stay intact.
            while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = pending.drain(..=end).collect();
                let line = String::from_utf8_lossy(&line);
                let Some(data) = line.trim().strip_prefix("data:").map(str::trim) else { continue };
                if data == "[DONE]" {
                    break;
                }
                let Ok(event) = serde_json::from_str::<Value>(data) else { continue };
                if let Some(delta) = event["choices"][0]["delta"]["content"].as_str() {
                    let visible = filter.push(delta);
                    if !visible.is_empty() {
                        on_delta(&visible);
                    }
                }
                if let Some(tps) = event["timings"]["predicted_per_second"].as_f64() {
                    stats.tokens_per_second = Some(tps);
                }
            }
        }
        let rest = filter.finish();
        if !rest.is_empty() {
            on_delta(&rest);
        }
        Ok(stats)
    }
}

/// Drops a leading `<think>…</think>` block, in case a reasoning model is configured.
#[derive(Default)]
struct ThinkFilter {
    buffer: String,
    state: ThinkState,
}

#[derive(Default, PartialEq)]
enum ThinkState {
    #[default]
    Start,
    Thinking,
    Passing,
}

impl ThinkFilter {
    const OPEN: &'static str = "<think>";
    const CLOSE: &'static str = "</think>";

    fn push(&mut self, delta: &str) -> String {
        if self.state == ThinkState::Passing {
            return delta.to_string();
        }
        self.buffer.push_str(delta);
        if self.state == ThinkState::Start {
            let head = self.buffer.trim_start();
            if let Some(after) = head.strip_prefix(Self::OPEN) {
                self.buffer = after.to_string();
                self.state = ThinkState::Thinking;
            } else if Self::OPEN.starts_with(head) {
                return String::new(); // could still turn into "<think>"
            } else {
                self.state = ThinkState::Passing;
                return std::mem::take(&mut self.buffer);
            }
        }
        match self.buffer.find(Self::CLOSE) {
            Some(i) => {
                let rest = self.buffer[i + Self::CLOSE.len()..].trim_start().to_string();
                self.buffer.clear();
                self.state = ThinkState::Passing;
                rest
            }
            None => String::new(),
        }
    }

    fn finish(&mut self) -> String {
        match self.state {
            ThinkState::Start => std::mem::take(&mut self.buffer),
            _ => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ThinkFilter;

    fn run(chunks: &[&str]) -> String {
        let mut f = ThinkFilter::default();
        let mut out: String = chunks.iter().map(|c| f.push(c)).collect();
        out.push_str(&f.finish());
        out
    }

    #[test]
    fn passes_plain_text_through() {
        assert_eq!(run(&["Hello", " world"]), "Hello world");
    }

    #[test]
    fn strips_think_block_split_across_chunks() {
        assert_eq!(run(&["<thi", "nk>plan", "ning</th", "ink>\n\nAnswer", " here"]), "Answer here");
    }

    #[test]
    fn keeps_text_that_only_starts_like_a_tag() {
        assert_eq!(run(&["<", "b>bold"]), "<b>bold");
    }

    #[test]
    fn flushes_short_output_that_never_resolved() {
        assert_eq!(run(&["<th"]), "<th");
    }
}
