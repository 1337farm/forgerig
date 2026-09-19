//! Persistent chat sessions with history, forking, deletion, and undo.
//!
//! A session's `messages` vector is the OpenAI-compat thread (system message
//! seeded at creation, then user / assistant / tool messages in order), so it
//! can be replayed verbatim to the model and reused as the cache for resuming,
//! forking, or branching a conversation. Tool round-trips stay in the thread
//! (so context is complete) but are invisible to the UI, which renders only
//! user messages and assistant messages with text content.
//!
//! Sessions and closed tabs are persisted to a JSON file so they survive an
//! Android process kill or app force-close. The file is stored in the app's
//! private files directory (or an explicit `FORGERIG_SESSIONS_FILE` path).

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub message_count: usize,
    pub created_ms: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub messages: Vec<Value>,
    pub created_ms: u64,
}

#[derive(Serialize, Deserialize)]
struct PersistedState {
    sessions: Vec<Session>,
    trash: Vec<(Session, u64)>,
    seq: u64,
    trash_seq: u64,
}

pub struct SessionManager {
    sessions: Mutex<HashMap<String, Session>>,
    /// Soft-deleted sessions (closed tabs): restorable, capped.
    /// The u64 is a monotonic trash sequence (insertion order); wall-clock
    /// millis would tie under fast test loops and evict arbitrarily.
    trash: Mutex<HashMap<String, (Session, u64)>>,
    seq: Mutex<u64>,
    trash_seq: Mutex<u64>,
    persist_path: Option<PathBuf>,
}

/// Cap on restorable closed tabs; oldest evicted first.
const TRASH_CAP: usize = 20;
/// Max stored title length (UI + RPC trim longer input).
const TITLE_MAX: usize = 60;

impl SessionManager {
    pub fn new() -> Self {
        Self::with_persist_path(None)
    }

    /// Create a manager that loads persisted sessions from `path` on startup.
    pub fn with_persist_path(path: Option<PathBuf>) -> Self {
        let mut sessions = HashMap::new();
        let mut trash = HashMap::new();
        let mut seq = 0;
        let mut trash_seq = 0;

        if let Some(path) = &path {
            if let Ok(mut file) = File::open(path) {
                let mut text = String::new();
                if file.read_to_string(&mut text).is_ok() {
                    if let Ok(state) = serde_json::from_str::<PersistedState>(&text) {
                        for session in state.sessions {
                            sessions.insert(session.id.clone(), session);
                        }
                        for (session, order) in state.trash {
                            trash.insert(session.id.clone(), (session, order));
                        }
                        seq = state.seq;
                        trash_seq = state.trash_seq;
                    }
                }
            }
        }

        Self {
            sessions: Mutex::new(sessions),
            trash: Mutex::new(trash),
            seq: Mutex::new(seq),
            trash_seq: Mutex::new(trash_seq),
            persist_path: path,
        }
    }

    fn next_id(&self) -> String {
        let mut seq = self.seq.lock().unwrap();
        *seq += 1;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("s{seq}-{nanos}")
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Auto-title a session from its first user message.
    fn derive_title(content: &str) -> String {
        let mut title: String = content.chars().take(40).collect();
        if content.chars().count() > 40 {
            title.push('…');
        }
        title
    }

    fn persist(&self) {
        let Some(path) = &self.persist_path else {
            return;
        };
        let sessions = self.sessions.lock().unwrap();
        let trash = self.trash.lock().unwrap();
        let state = PersistedState {
            sessions: sessions.values().cloned().collect(),
            trash: trash.values().cloned().collect(),
            seq: *self.seq.lock().unwrap(),
            trash_seq: *self.trash_seq.lock().unwrap(),
        };
        drop(sessions);
        drop(trash);
        let text = match serde_json::to_string_pretty(&state) {
            Ok(text) => text,
            Err(_) => return,
        };
        let tmp = path.with_extension("tmp");
        let _ = fs::write(&tmp, text);
        let _ = fs::rename(&tmp, path);
    }

    /// Create a new session seeded with the system message.
    pub fn create(&self, system_message: Value) -> Session {
        let id = self.next_id();
        let session = Session {
            id: id.clone(),
            title: String::new(),
            messages: vec![system_message],
            created_ms: Self::now_ms(),
        };
        self.sessions.lock().unwrap().insert(id, session.clone());
        self.persist();
        session
    }

    pub fn get(&self, id: &str) -> Option<Session> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    pub fn exists(&self, id: &str) -> bool {
        self.sessions.lock().unwrap().contains_key(id)
    }

    pub fn list(&self) -> Vec<SessionSummary> {
        let mut v: Vec<SessionSummary> = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .map(|s| SessionSummary {
                id: s.id.clone(),
                title: s.title.clone(),
                message_count: s.messages.len(),
                created_ms: s.created_ms,
            })
            .collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.created_ms));
        v
    }

    /// Replace the whole message thread (written back after a chat extends it).
    pub fn set_messages(&self, id: &str, messages: Vec<Value>) -> Option<Session> {
        let mut lock = self.sessions.lock().unwrap();
        let session = lock.get_mut(id)?;
        session.messages = messages;
        if session.title.is_empty() {
            if let Some(content) = session
                .messages
                .iter()
                .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
            {
                session.title = Self::derive_title(content);
            }
        }
        let session = session.clone();
        drop(lock);
        self.persist();
        Some(session)
    }

    pub fn delete(&self, id: &str) -> bool {
        let removed = self.sessions.lock().unwrap().remove(id);
        match removed {
            Some(session) => {
                let mut trash_seq = self.trash_seq.lock().unwrap();
                *trash_seq += 1;
                let order = *trash_seq;
                drop(trash_seq);
                let mut trash = self.trash.lock().unwrap();
                trash.insert(id.to_string(), (session, order));
                while trash.len() > TRASH_CAP {
                    if let Some(oldest) = trash
                        .iter()
                        .min_by_key(|(_, (_, ts))| *ts)
                        .map(|(k, _)| k.clone())
                    {
                        trash.remove(&oldest);
                    } else {
                        break;
                    }
                }
                drop(trash);
                self.persist();
                true
            }
            None => false,
        }
    }

    /// Restore a soft-deleted (closed-tab) session back to the live list.
    pub fn restore(&self, id: &str) -> Option<Session> {
        let (session, _) = self.trash.lock().unwrap().remove(id)?;
        let mut lock = self.sessions.lock().unwrap();
        lock.insert(session.id.clone(), session.clone());
        drop(lock);
        self.persist();
        Some(session)
    }

    /// Closed tabs available for restore, newest first.
    pub fn trash_list(&self) -> Vec<SessionSummary> {
        let mut v: Vec<(SessionSummary, u64)> = self
            .trash
            .lock()
            .unwrap()
            .values()
            .map(|(s, ts)| {
                (
                    SessionSummary {
                        id: s.id.clone(),
                        title: s.title.clone(),
                        message_count: s.messages.len(),
                        created_ms: s.created_ms,
                    },
                    *ts,
                )
            })
            .collect();
        v.sort_by_key(|(_, ts)| std::cmp::Reverse(*ts));
        v.into_iter().map(|(s, _)| s).collect()
    }

    /// Rename a tab. A non-empty custom title also pins it: the auto-title
    /// in set_messages only fills blank titles, so renames stick.
    pub fn set_title(&self, id: &str, title: &str) -> Option<Session> {
        let trimmed: String = title.chars().take(TITLE_MAX).collect();
        let trimmed = trimmed.trim().to_string();
        if trimmed.is_empty() {
            return None;
        }
        let mut lock = self.sessions.lock().unwrap();
        let session = lock.get_mut(id)?;
        session.title = trimmed;
        let session = session.clone();
        drop(lock);
        self.persist();
        Some(session)
    }

    /// Fork: clone a session and keep only the thread up to and including
    /// message `index` (0-based, forked session keeps the same cache prefix).
    pub fn fork(&self, id: &str, index: usize) -> Option<Session> {
        let source = self.get(id)?;
        if source.messages.is_empty() {
            return None;
        }
        let keep = index.min(source.messages.len() - 1);
        let new_id = self.next_id();
        let forked = Session {
            id: new_id.clone(),
            title: format!("{} (fork)", source.title),
            messages: source.messages[..=keep].to_vec(),
            created_ms: Self::now_ms(),
        };
        self.sessions.lock().unwrap().insert(new_id, forked.clone());
        self.persist();
        Some(forked)
    }

    /// Undo the last completed user/assistant turn. Returns the user text so
    /// the client can put it back in the composer for editing.
    pub fn undo(&self, id: &str) -> Option<String> {
        let mut lock = self.sessions.lock().unwrap();
        let session = lock.get_mut(id)?;
        if session.messages.len() < 2 {
            return None;
        }
        let mut end = session.messages.len();
        while end > 0 {
            let role = session.messages[end - 1].get("role").and_then(|r| r.as_str());
            if role == Some("assistant") || role == Some("tool") {
                end -= 1;
            } else {
                break;
            }
        }
        if end == 0 || session.messages[end - 1].get("role").and_then(|r| r.as_str()) != Some("user") {
            return None;
        }
        let text = session.messages[end - 1]
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        session.messages.truncate(end - 1);
        drop(lock);
        self.persist();
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sys() -> Value {
        json!({ "role": "system", "content": "SYS" })
    }

    fn thread() -> Vec<Value> {
        vec![
            sys(),
            json!({"role":"user","content":"hello"}),
            json!({"role":"assistant","content":"hi"}),
            json!({"role":"user","content":"second question"}),
        ]
    }

    #[test]
    fn create_seeds_system_and_lists() {
        let m = SessionManager::new();
        let s = m.create(sys());
        assert_eq!(s.messages.len(), 1);
        assert!(m.exists(&s.id));
        assert_eq!(m.list().len(), 1);
    }

    #[test]
    fn fork_truncates_and_keeps_history() {
        let m = SessionManager::new();
        let s = m.create(sys());
        m.set_messages(&s.id, thread());
        // Fork from index 2 (the assistant "hi" reply): keep system + user + assistant.
        let f = m.fork(&s.id, 2).unwrap();
        assert_eq!(f.messages.len(), 3);
        assert_eq!(f.messages[1].get("content").unwrap(), "hello");
        assert_eq!(f.messages[2].get("content").unwrap(), "hi");
        assert_ne!(f.id, s.id);
        assert!(m.exists(&f.id));
    }

    #[test]
    fn auto_title_from_first_user_message() {
        let m = SessionManager::new();
        let s = m.create(sys());
        m.set_messages(&s.id, thread());
        let after = m.get(&s.id).unwrap();
        assert_eq!(after.title, "hello");
    }

    #[test]
    fn delete_removes_session() {
        let m = SessionManager::new();
        let s = m.create(sys());
        assert!(m.delete(&s.id));
        assert!(!m.exists(&s.id));
    }

    #[test]
    fn rename_pins_title_against_auto_title() {
        let m = SessionManager::new();
        let s = m.create(sys());
        m.set_title(&s.id, "Custom").unwrap();
        m.set_messages(&s.id, thread());
        assert_eq!(m.get(&s.id).unwrap().title, "Custom");
        assert!(m.set_title(&s.id, "   ").is_none());
    }

    #[test]
    fn delete_trashes_and_restore_brings_back() {
        let m = SessionManager::new();
        let s = m.create(sys());
        m.set_messages(&s.id, thread());
        assert!(m.delete(&s.id));
        assert_eq!(m.trash_list().len(), 1);
        let back = m.restore(&s.id).unwrap();
        assert_eq!(back.messages.len(), 4);
        assert!(m.exists(&s.id));
        assert!(m.trash_list().is_empty());
    }

    #[test]
    fn trash_evicts_oldest_beyond_cap() {
        let m = SessionManager::new();
        let mut first = String::new();
        for _ in 0..(TRASH_CAP + 5) {
            let s = m.create(sys());
            if first.is_empty() {
                first = s.id.clone();
            }
            m.delete(&s.id);
        }
        assert_eq!(m.trash_list().len(), TRASH_CAP);
        assert!(m.restore(&first).is_none());
    }

    #[test]
    fn undo_removes_last_turn_and_returns_user_text() {
        let m = SessionManager::new();
        let s = m.create(sys());
        m.set_messages(&s.id, thread());
        assert_eq!(m.undo(&s.id).unwrap(), "second question");
        let after = m.get(&s.id).unwrap();
        assert_eq!(after.messages.len(), 3);
        assert_eq!(after.messages[2].get("content").unwrap(), "hi");
    }

    #[test]
    fn undo_removes_user_and_assistant_pair() {
        let m = SessionManager::new();
        let s = m.create(sys());
        m.set_messages(&s.id, thread());
        assert_eq!(m.undo(&s.id).unwrap(), "second question");
        assert_eq!(m.undo(&s.id).unwrap(), "hello");
        let after = m.get(&s.id).unwrap();
        assert_eq!(after.messages.len(), 1);
        assert_eq!(after.messages[0].get("role").unwrap(), "system");
    }

    #[test]
    fn persistence_round_trips_live_and_trash_sessions() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("forgerig-sessions-{}.json", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let m = SessionManager::with_persist_path(Some(path.clone()));
        let s = m.create(sys());
        m.set_messages(&s.id, thread());
        assert!(m.delete(&s.id));
        drop(m);

        let m2 = SessionManager::with_persist_path(Some(path.clone()));
        assert!(!m2.exists(&s.id));
        assert_eq!(m2.trash_list().len(), 1);
        let back = m2.restore(&s.id).unwrap();
        assert_eq!(back.messages.len(), 4);
        let _ = std::fs::remove_file(path);
    }
}
