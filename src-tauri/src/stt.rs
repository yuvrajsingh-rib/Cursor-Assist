//! Speech-to-text with a managed Parakeet server (whisper.cpp `/inference` protocol).
//!
//! The server holds ~1.3 GB while running and has no idle sleep of its own, so it is started on
//! demand (when the user reaches for the task box or mic) and stopped after `stop_after_secs` idle.
//! It has no authentication; it listens on 127.0.0.1 only, and only while in use. It sends no CORS
//! headers, so web pages can't read anything back from it.

use std::fs::File;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::{self, SttSettings};
use crate::process;

/// How long a transcription waits for a server that is still loading its model.
const STARTUP_GRACE: Duration = Duration::from_secs(45);

pub struct SttHost {
    settings: SttSettings,
    server_exe: PathBuf,
    log_dir: PathBuf,
    client: reqwest::Client,
    child: Mutex<Option<Child>>,
    last_used: Mutex<Instant>,
}

impl SttHost {
    /// `bundled_exe` is used unless settings.json names another speech server.
    pub fn new(settings: SttSettings, bundled_exe: PathBuf, log_dir: PathBuf) -> Arc<Self> {
        let host = Arc::new(Self {
            server_exe: settings.server_exe.clone().unwrap_or(bundled_exe),
            settings,
            log_dir,
            client: process::local_http_client(),
            child: Mutex::new(None),
            last_used: Mutex::new(Instant::now()),
        });
        let reaper = Arc::downgrade(&host);
        thread::Builder::new()
            .name("stt-reaper".into())
            .spawn(move || loop {
                thread::sleep(Duration::from_secs(10));
                let Some(host) = reaper.upgrade() else { return };
                host.stop_if_idle();
            })
            .expect("spawn stt reaper");
        host
    }

    fn url(&self) -> String {
        self.settings.endpoint.clone().unwrap_or_else(|| format!("http://127.0.0.1:{}/inference", self.settings.port))
    }

    fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }

    /// Starts the server if it isn't running, so it has loaded by the time audio arrives.
    pub fn prepare(&self) {
        self.touch();
        if let Err(e) = self.ensure_running() {
            eprintln!("[stt] {e}");
        }
    }

    fn ensure_running(&self) -> Result<(), String> {
        if self.settings.endpoint.is_some() {
            return Ok(());
        }
        let mut child = self.child.lock().unwrap();
        if let Some(running) = child.as_mut() {
            if matches!(running.try_wait(), Ok(None)) {
                return Ok(());
            }
        }
        let s = &self.settings;
        if !self.server_exe.exists() {
            return Err(format!("speech server not found at {}", self.server_exe.display()));
        }
        if !s.model.exists() {
            return Err(format!("speech model not found at {}", s.model.display()));
        }
        let _ = std::fs::create_dir_all(&self.log_dir);
        let log = File::create(self.log_dir.join("stt.log")).map_err(|e| e.to_string())?;
        let mut command = Command::new(&self.server_exe);
        command
            .arg("-m")
            .arg(&s.model)
            .args(["--host", "127.0.0.1", "--port", &s.port.to_string(), "--threads", &s.threads.to_string()]);
        if !s.gpu {
            command.arg("--no-gpu");
        }
        let started = process::spawn(command, log).map_err(|e| format!("could not start the speech server: {e}"))?;
        eprintln!("[stt] started on {} (pid {})", if s.gpu { "GPU" } else { "CPU" }, started.id());
        *child = Some(started);
        Ok(())
    }

    fn stop_if_idle(&self) {
        let idle = self.last_used.lock().unwrap().elapsed();
        if idle < Duration::from_secs(self.settings.stop_after_secs as u64) {
            return;
        }
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("[stt] stopped after {}s idle", idle.as_secs());
        }
    }

    /// Transcribes a 16 kHz mono WAV.
    pub async fn transcribe(&self, wav: Vec<u8>) -> Result<String, String> {
        self.touch();
        // Dev builds keep the last recording for diagnosing recognition problems; release builds
        // never write the user's voice to disk.
        if cfg!(debug_assertions) {
            let _ = std::fs::write(self.log_dir.join("last-dictation.wav"), &wav);
        }
        let url = self.url();
        if !config::is_local_url(&url) {
            return Err("Speech endpoint must be on this computer (stt.endpoint in settings.json)".into());
        }
        self.ensure_running()?;
        let started = Instant::now();
        let response = loop {
            let form = reqwest::multipart::Form::new().part(
                "file",
                reqwest::multipart::Part::bytes(wav.clone()).file_name("dictation.wav").mime_str("audio/wav").unwrap(),
            );
            match self.client.post(&url).multipart(form).send().await {
                Ok(r) if r.status().is_success() => break r,
                Ok(r) => return Err(format!("speech server error {}: {}", r.status(), r.text().await.unwrap_or_default())),
                // Still loading the model: the port isn't open yet.
                Err(e) if e.is_connect() && started.elapsed() < STARTUP_GRACE => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(e) => return Err(format!("speech server unreachable: {e}")),
            }
        };
        let body: Value = response.json().await.map_err(|e| e.to_string())?;
        self.touch();
        let text = body["text"].as_str().unwrap_or_default().trim().to_string();
        eprintln!("[stt] {} chars in {:.1}s", text.chars().count(), started.elapsed().as_secs_f32());
        if text.is_empty() {
            return Err("Didn't catch that. Try again.".into());
        }
        Ok(text)
    }
}
