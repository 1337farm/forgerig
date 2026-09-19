//! In-memory chat sessions with history, forking, and deletion.
//!
//! A session's `messages` vector is the OpenAI-compat thread (system message
//! seeded at creation, then user / assistant / tool messages in order), so it
//! can be replayed verbatim to the model and reused as the cache for resuming,
//! forking, or branching a conversation. Tool round-trips stay in the thread
//! (so context is complete) but are invisible to the UI, which renders only
//! user messages and assistant messages with text content.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub message_count: usize,
    pub created_ms: u64,
}

#[derive(Clone)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub messages: Vec<Value>,
    pub created_ms: u64,
}

pub struct SessionManager {
    sessions: Mutex<HashMap<String, Session>>,
    /// Soft-deleted sessions (closed tabs): restorable, capped.
    /// The u64 is a monotonic trash sequence (insertion order); wall-clock
    /// millis would tie under fast test loops and evict arbitrarily.
    trash: Mutex<HashMap<String, (Session, u64)>>,
    seq: Mutex<u64>,
    trash_seq: Mutex<u64>,
}

/// Cap on restorable closed tabs; oldest evicted first.
const TRASH_CAP: usize = 20;
/// Max stored title length (UI + RPC trim longer input).
const TITLE_MAX: usize = 60;

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            trash: Mutex::new(HashMap::new()),
            seq: Mutex::new(0),
            trash_seq: Mutex::new(0),
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
        Some(session.clone())
    }

    pub fn delete(&self, id: &str) -> bool {
        let mut lock = self.sessions.lock().unwrap();
        match lock.remove(id) {
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
        Some(session.clone())
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
        Some(forked)
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
}