//! Recent results, so closing the panel (or clicking outside it) never loses an answer.
//!
//! Kept in memory only: never written to disk, and gone when CursorAssist quits.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::engine::Selection;

/// How many results to keep; the oldest drops off first.
const LIMIT: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Running,
    Done,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: u64,
    pub action_id: String,
    pub title: String,
    pub instruction: Option<String>,
    pub app: String,
    pub created_ms: u64,
    pub output: String,
    pub status: Status,
    pub error: Option<String>,
    pub model: Option<String>,
    pub tokens_per_second: Option<f64>,
    /// Copied or pasted already, so there is nothing left to come back for.
    pub used: bool,
    /// Whether the source text can be replaced (false for read-only text like web pages).
    pub editable: bool,
    /// The selection it ran on, kept for Retry and Replace.
    #[serde(skip)]
    pub selection: Selection,
}

impl Entry {
    pub fn new(id: u64, action_id: &str, title: String, instruction: Option<String>, selection: Selection) -> Self {
        let created_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        Self {
            id,
            action_id: action_id.to_string(),
            title,
            instruction,
            app: selection.app.clone(),
            created_ms,
            output: String::new(),
            status: Status::Running,
            error: None,
            model: None,
            tokens_per_second: None,
            used: false,
            editable: selection.editable,
            selection,
        }
    }
}

#[derive(Default)]
pub struct History {
    entries: Mutex<VecDeque<Entry>>,
}

impl History {
    pub fn push(&self, entry: Entry) {
        let mut entries = self.entries.lock().unwrap();
        entries.push_front(entry);
        entries.truncate(LIMIT);
    }

    /// Applies `change` to entry `id`, if it is still kept.
    pub fn update(&self, id: u64, change: impl FnOnce(&mut Entry)) {
        if let Some(entry) = self.entries.lock().unwrap().iter_mut().find(|e| e.id == id) {
            change(entry);
        }
    }

    pub fn get(&self, id: u64) -> Option<Entry> {
        self.entries.lock().unwrap().iter().find(|e| e.id == id).cloned()
    }

    /// Newest first.
    pub fn list(&self) -> Vec<Entry> {
        self.entries.lock().unwrap().iter().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::POINT;

    use super::*;
    use crate::hooks::Gesture;

    fn entry(id: u64) -> Entry {
        let selection = Selection {
            text: "some text".into(),
            method: "uia",
            app: "Notepad.exe".into(),
            gesture: Gesture::Drag,
            capture_ms: 1,
            editable: true,
            source: 0,
            anchor: POINT::default(),
        };
        Entry::new(id, "refine", "Refine".into(), None, selection)
    }

    #[test]
    fn keeps_newest_first_and_drops_the_oldest_past_the_limit() {
        let history = History::default();
        for id in 1..=(LIMIT as u64 + 5) {
            history.push(entry(id));
        }
        let ids: Vec<u64> = history.list().iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), LIMIT);
        assert_eq!(ids.first(), Some(&(LIMIT as u64 + 5)));
        assert!(history.get(1).is_none(), "oldest entries are dropped");
    }

    #[test]
    fn streams_into_an_entry_and_ignores_unknown_ids() {
        let history = History::default();
        history.push(entry(7));
        history.update(7, |e| e.output.push_str("Hello"));
        history.update(7, |e| e.output.push_str(" world"));
        history.update(99, |e| e.output.push_str("lost"));
        let kept = history.get(7).unwrap();
        assert_eq!(kept.output, "Hello world");
        assert_eq!(kept.status, Status::Running);
        assert!(kept.editable);
    }
}
