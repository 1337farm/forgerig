//! Persistent branch-tree chat sessions with history, forking, undo/redo.
//!
//! A session is a *tree* of OpenAI-compat messages rooted at the system
//! message. The visible thread is the path from the root to the current
//! `head` node. Undo moves `head` back past the last user/assistant turn but
//! never deletes nodes: the undone limb stays in the tree (a "phantom copy"),
//! so redo and branch navigation can always walk back down/up. Nothing is
//! ever pruned except the trash-cap eviction of whole closed sessions.
//!
//! Sessions and closed tabs persist to a JSON file so they survive an Android
//! process kill or app force-close.

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
    pub archived: bool,
    /// True when the visible thread has an earlier user turn to undo to.
    pub can_undo: bool,
    /// Alternate limbs off the current head (children) plus redo depth.
    pub alt_count: usize,
    /// Team model: id of the parent session, if this is a sub-agent.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Sub-agent role within its team ("" for root sessions).
    #[serde(default)]
    pub role: String,
    /// Direct (non-archived) sub-agent count.
    #[serde(default)]
    pub child_count: usize,
}

/// One message node in a session tree.
#[derive(Clone, Serialize, Deserialize)]
pub struct MsgNode {
    pub id: u64,
    pub parent: Option<u64>,
    /// Full OpenAI-compat message object (role/content/tool_calls/...).
    pub message: Value,
    pub children: Vec<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SessionTree {
    pub nodes: HashMap<u64, MsgNode>,
    pub root: u64,
    pub head: u64,
    pub next: u64,
    /// Heads abandoned by undo, newest last. Cleared by any new append
    /// (a new message from a rewound head forks a fresh limb).
    pub redo: Vec<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub tree: SessionTree,
    pub archived: bool,
    pub created_ms: u64,
    /// Team model: parent session id for user-driven sub-agents.
    /// `None` = root session (a team unto itself). Serde defaults keep
    /// pre-team on-disk state (v2) parsing without a migration.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Sub-agent role within its team, e.g. "backend", "qa".
    #[serde(default)]
    pub role: String,
    /// High-level objective this session (or sub-agent) pursues.
    #[serde(default)]
    pub goal: String,
}

impl Session {
    /// Isolated guest workspace for this session's file work.
    /// Each team member gets its own directory under the shared garden root,
    /// so parallel sub-agents never cross-contaminate; the shared orchestrator
    /// merges results back explicitly (see `merge_child`). The single
    /// implementation lives in the gatekeeper so path policy and session
    /// layout can never drift apart.
    pub fn workspace(&self) -> String {
        crate::gatekeeper::session_workspace(&self.id)
    }
}

impl Session {
    /// Visible thread: messages on the path root -> head.
    pub fn thread(&self) -> Vec<Value> {
        let mut ids = Vec::new();
        let mut cur = Some(self.tree.head);
        while let Some(id) = cur {
            ids.push(id);
            cur = self.tree.nodes.get(&id).and_then(|n| n.parent);
        }
        ids.reverse();
        ids.iter()
            .filter_map(|id| self.tree.nodes.get(id))
            .map(|n| n.message.clone())
            .collect()
    }

    fn autotitle(&mut self) {
        if !self.title.is_empty() {
            return;
        }
        if let Some(content) = self
            .thread()
            .iter()
            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
        {
            self.title = SessionManager::derive_title(content);
        }
    }
}

/// Child descriptor for branch navigation UIs.
#[derive(Clone, Serialize)]
pub struct BranchChild {
    pub id: u64,
    pub role: String,
    pub preview: String,
}

#[derive(Serialize, Deserialize)]
struct PersistedState {
    version: u32,
    sessions: Vec<Session>,
    trash: Vec<(Session, u64)>,
    seq: u64,
    trash_seq: u64,
}

/// Pre-tree (v1) on-disk shape, for migration.
#[derive(Deserialize)]
struct V1Session {
    id: String,
    title: String,
    messages: Vec<Value>,
    created_ms: u64,
}

#[derive(Deserialize)]
struct V1State {
    sessions: Vec<V1Session>,
    trash: Vec<(V1Session, u64)>,
    seq: u64,
    trash_seq: u64,
}

const STATE_VERSION: u32 = 2;

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
                        if state.version == STATE_VERSION {
                            for session in state.sessions {
                                sessions.insert(session.id.clone(), session);
                            }
                            for (session, order) in state.trash {
                                trash.insert(session.id.clone(), (session, order));
                            }
                            seq = state.seq;
                            trash_seq = state.trash_seq;
                        }
                    } else if let Ok(v1) = serde_json::from_str::<V1State>(&text) {
                        // Migrate linear v1 threads into single-limb trees.
                        for s in v1.sessions {
                            sessions.insert(s.id.clone(), Self::linear_tree(&s.id, &s.title, s.messages, s.created_ms));
                        }
                        for (s, order) in v1.trash {
                            let session = Self::linear_tree(&s.id, &s.title, s.messages, s.created_ms);
                            trash.insert(session.id.clone(), (session, order));
                        }
                        seq = v1.seq;
                        trash_seq = v1.trash_seq;
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

    fn linear_tree(_id: &str, title: &str, messages: Vec<Value>, created_ms: u64) -> Session {
        let mut nodes = HashMap::new();
        let mut parent = None;
        let mut next = 0;
        for message in messages {
            let id = next;
            next += 1;
            nodes.insert(
                id,
                MsgNode { id, parent, message, children: Vec::new() },
            );
            if let Some(p) = parent {
                if let Some(pn) = nodes.get_mut(&p) {
                    pn.children.push(id);
                }
            }
            parent = Some(id);
        }
        let head = parent.unwrap_or(0);
        Session {
            id: _id.to_string(),
            title: title.to_string(),
            tree: SessionTree { nodes, root: 0, head, next, redo: Vec::new() },
            archived: false,
            created_ms,
            parent_id: None,
            role: String::new(),
            goal: String::new(),
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
            version: STATE_VERSION,
            sessions: sessions.values().cloned().collect(),
            trash: trash.values().cloned().collect(),
            seq: *self.seq.lock().unwrap(),
            trash_seq: *self.trash_seq.lock().unwrap(),
        };
        drop(sessions);
        drop(trash);
        let text = match serde_json::to_string(&state) {
            Ok(text) => text,
            Err(_) => return,
        };
        let tmp = path.with_extension("tmp");
        let _ = fs::write(&tmp, text);
        let _ = fs::rename(&tmp, path);
    }

    fn summary_of(s: &Session) -> SessionSummary {
        let thread = s.thread();
        let can_undo = thread
            .iter()
            .skip(1)
            .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"));
        let alt_count = s
            .tree
            .nodes
            .get(&s.tree.head)
            .map(|n| n.children.len())
            .unwrap_or(0)
            + s.tree.redo.len();
        SessionSummary {
            id: s.id.clone(),
            title: s.title.clone(),
            message_count: thread.len(),
            created_ms: s.created_ms,
            archived: s.archived,
            can_undo,
            alt_count,
            parent_id: s.parent_id.clone(),
            role: s.role.clone(),
            // Filled in by list()/children() from the live map.
            child_count: 0,
        }
    }

    /// Count direct non-archived children per session id.
    fn child_counts(sessions: &HashMap<String, Session>) -> HashMap<String, usize> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for s in sessions.values() {
            if s.archived {
                continue;
            }
            if let Some(pid) = &s.parent_id {
                *counts.entry(pid.clone()).or_insert(0) += 1;
            }
        }
        counts
    }

    /// Append one message node under the current head. Any new append forks
    /// a fresh limb, so the redo stack is cleared.
    fn push_node(session: &mut Session, message: Value) -> u64 {
        let id = session.tree.next;
        session.tree.next += 1;
        let parent = session.tree.head;
        session.tree.nodes.insert(
            id,
            MsgNode { id, parent: Some(parent), message, children: Vec::new() },
        );
        if let Some(pn) = session.tree.nodes.get_mut(&parent) {
            pn.children.push(id);
        }
        session.tree.head = id;
        session.tree.redo.clear();
        id
    }

    /// Create a new session seeded with the system message.
    pub fn create(&self, system_message: Value) -> Session {
        let id = self.next_id();
        let mut nodes = HashMap::new();
        nodes.insert(
            0,
            MsgNode { id: 0, parent: None, message: system_message, children: Vec::new() },
        );
        let session = Session {
            id: id.clone(),
            title: String::new(),
            tree: SessionTree { nodes, root: 0, head: 0, next: 1, redo: Vec::new() },
            archived: false,
            created_ms: Self::now_ms(),
            parent_id: None,
            role: String::new(),
            goal: String::new(),
        };
        self.sessions.lock().unwrap().insert(id, session.clone());
        self.persist();
        session
    }

    /// Spawn a user-driven sub-agent under `parent_id` (the small-team model).
    ///
    /// The child gets a fresh thread (system message only), its own isolated
    /// workspace (`Session::workspace`), and a recorded role + goal. It reads
    /// nothing from the parent automatically — the shared orchestrator (or the
    /// user) copies in whatever context the member needs, and merges results
    /// back explicitly via `merge_child`. Returns None when the parent is
    /// unknown or the role is blank.
    pub fn spawn_subagent(&self, parent_id: &str, role: &str, goal: &str, system_message: Value) -> Option<Session> {
        let role: String = role.trim().chars().take(40).collect();
        if role.is_empty() {
            return None;
        }
        let goal: String = goal.trim().chars().take(500).collect();
        if !self.exists(parent_id) {
            return None;
        }
        let id = self.next_id();
        let mut nodes = HashMap::new();
        nodes.insert(
            0,
            MsgNode { id: 0, parent: None, message: system_message, children: Vec::new() },
        );
        let title = if goal.is_empty() {
            role.clone()
        } else {
            let short: String = goal.chars().take(30).collect();
            format!("{role}: {short}")
        };
        let session = Session {
            id: id.clone(),
            title,
            tree: SessionTree { nodes, root: 0, head: 0, next: 1, redo: Vec::new() },
            archived: false,
            created_ms: Self::now_ms(),
            parent_id: Some(parent_id.to_string()),
            role,
            goal,
        };
        self.sessions.lock().unwrap().insert(id, session.clone());
        self.persist();
        Some(session)
    }

    /// Direct non-archived sub-agents of `parent_id`, newest first.
    /// (Named `subagents` — `children` already means branch-tree children.)
    pub fn subagents(&self, parent_id: &str) -> Vec<SessionSummary> {
        let lock = self.sessions.lock().unwrap();
        let counts = Self::child_counts(&lock);
        let mut v: Vec<SessionSummary> = lock
            .values()
            .filter(|s| !s.archived && s.parent_id.as_deref() == Some(parent_id))
            .map(|s| {
                let mut summary = Self::summary_of(s);
                summary.child_count = counts.get(&s.id).copied().unwrap_or(0);
                summary
            })
            .collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.created_ms));
        v
    }

    /// Merge a finished sub-agent back into its parent team session.
    ///
    /// The child's visible thread (minus its system message) is appended to
    /// the parent's thread in order, so the parent's singular goal absorbs the
    /// member's work as ordinary turns. The child is then archived (kept for
    /// audit, hidden from lists). Returns the updated parent, or None when
    /// either side is unknown / the child has no parent.
    pub fn merge_child(&self, child_id: &str) -> Option<Session> {
        let child = self.get(child_id)?;
        if child.archived {
            return None;
        }
        let parent_id = child.parent_id.clone()?;
        let incoming: Vec<Value> = child.thread().into_iter().skip(1).collect();
        let mut lock = self.sessions.lock().unwrap();
        {
            let parent = lock.get_mut(&parent_id)?;
            for msg in incoming {
                Self::push_node(parent, msg);
            }
            parent.autotitle();
        }
        if let Some(child_mut) = lock.get_mut(child_id) {
            child_mut.archived = true;
        }
        let out = lock.get(&parent_id).cloned()?;
        drop(lock);
        self.persist();
        Some(out)
    }

    pub fn get(&self, id: &str) -> Option<Session> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    /// Visible thread (root -> head path) for a session.
    pub fn thread(&self, id: &str) -> Option<Vec<Value>> {
        self.sessions.lock().unwrap().get(id).map(|s| s.thread())
    }

    pub fn exists(&self, id: &str) -> bool {
        self.sessions.lock().unwrap().contains_key(id)
    }

    pub fn list(&self) -> Vec<SessionSummary> {
        let lock = self.sessions.lock().unwrap();
        let counts = Self::child_counts(&lock);
        let mut v: Vec<SessionSummary> = lock
            .values()
            .filter(|s| !s.archived)
            .map(|s| {
                let mut summary = Self::summary_of(s);
                summary.child_count = counts.get(&s.id).copied().unwrap_or(0);
                summary
            })
            .collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.created_ms));
        v
    }

    /// Extend the visible thread to `full`: the longest common prefix with
    /// the current head path is kept (nodes shared), the remainder is
    /// appended as new nodes. The system message (index 0) is resynced, not
    /// compared, since project memory can refresh it between turns.
    pub fn set_messages(&self, id: &str, full: Vec<Value>) -> Option<Session> {
        let mut lock = self.sessions.lock().unwrap();
        let session = lock.get_mut(id)?;
        let path = session.thread();
        let base = path.len().min(full.len());
        let mut i = 1;
        while i < base && path.get(i) == full.get(i) {
            i += 1;
        }
        if full.is_empty() {
            return None;
        }
        if let Some(root) = session.tree.nodes.get_mut(&session.tree.root) {
            root.message = full[0].clone();
        }
        for msg in full.iter().skip(i) {
            Self::push_node(session, msg.clone());
        }
        session.autotitle();
        let session = session.clone();
        drop(lock);
        self.persist();
        Some(session)
    }

    /// Append a single message to the visible thread (early user-message
    /// persist at send time, partial assistant text on stop, ...).
    pub fn append_message(&self, id: &str, message: Value) -> Option<Session> {
        let mut lock = self.sessions.lock().unwrap();
        let session = lock.get_mut(id)?;
        Self::push_node(session, message);
        session.autotitle();
        let session = session.clone();
        drop(lock);
        self.persist();
        Some(session)
    }

    /// Move `head` back past the last assistant/tool block and its user
    /// message. Nodes are kept (phantom limb) and the old head is pushed on
    /// the redo stack. Returns the removed user text for edit+retry.
    pub fn undo(&self, id: &str) -> Option<String> {
        let mut lock = self.sessions.lock().unwrap();
        let session = lock.get_mut(id)?;
        // Walk back over trailing assistant/tool nodes to the user node.
        let mut cur = session.tree.head;
        loop {
            let role = session
                .tree
                .nodes
                .get(&cur)
                .and_then(|n| n.message.get("role"))
                .and_then(|r| r.as_str());
            match role {
                Some("assistant") | Some("tool") => {
                    cur = session.tree.nodes.get(&cur)?.parent?;
                }
                _ => break,
            }
        }
        let user_node = session.tree.nodes.get(&cur)?;
        if user_node.message.get("role").and_then(|r| r.as_str()) != Some("user")
            || cur == session.tree.root
        {
            return None;
        }
        let text = user_node
            .message
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let new_head = user_node.parent?;
        session.tree.redo.push(session.tree.head);
        session.tree.head = new_head;
        drop(lock);
        self.persist();
        Some(text)
    }

    /// Walk back down to the most recently undone head, if it still exists.
    pub fn redo(&self, id: &str) -> Option<Vec<Value>> {
        let mut lock = self.sessions.lock().unwrap();
        let session = lock.get_mut(id)?;
        let target = session.tree.redo.pop()?;
        if !session.tree.nodes.contains_key(&target) {
            drop(lock);
            self.persist();
            return None;
        }
        session.tree.head = target;
        let thread = session.thread();
        drop(lock);
        self.persist();
        Some(thread)
    }

    /// Jump `head` to any node in the tree (branch navigation). The undone
    /// limbs stay intact; sending from a rewound head forks a new limb.
    pub fn goto(&self, id: &str, node: u64) -> Option<Vec<Value>> {
        let mut lock = self.sessions.lock().unwrap();
        let session = lock.get_mut(id)?;
        if !session.tree.nodes.contains_key(&node) {
            return None;
        }
        session.tree.head = node;
        let thread = session.thread();
        drop(lock);
        self.persist();
        Some(thread)
    }

    /// Children of the current head: alternate limbs to navigate to.
    pub fn children(&self, id: &str) -> Option<Vec<BranchChild>> {
        let lock = self.sessions.lock().unwrap();
        let session = lock.get(id)?;
        let head = session.tree.nodes.get(&session.tree.head)?;
        Some(
            head.children
                .iter()
                .filter_map(|cid| session.tree.nodes.get(cid))
                .map(|n| {
                    let role = n
                        .message
                        .get("role")
                        .and_then(|r| r.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let preview: String = n
                        .message
                        .get("content")
                        .and_then(|c| c.as_str())
                        .unwrap_or("(tool)")
                        .chars()
                        .take(80)
                        .collect();
                    BranchChild { id: n.id, role, preview }
                })
                .collect(),
        )
    }

    /// Navigation snapshot for the branch pager UI.
    pub fn nav(&self, id: &str) -> Option<Value> {
        let lock = self.sessions.lock().unwrap();
        let session = lock.get(id)?;
        let thread = session.thread();
        let can_undo = thread
            .iter()
            .skip(1)
            .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"));
        let children = self.children(id).unwrap_or_default();
        Some(serde_json::json!({
            "head": session.tree.head,
            "can_undo": can_undo,
            "can_redo": !session.tree.redo.is_empty(),
            "children": children,
        }))
    }

    /// Archive (hide) or unhide a live or closed session.
    pub fn set_archived(&self, id: &str, archived: bool) -> bool {
        {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(s) = sessions.get_mut(id) {
                s.archived = archived;
            } else {
                let mut trash = self.trash.lock().unwrap();
                match trash.get_mut(id) {
                    Some((s, _)) => {
                        s.archived = archived;
                    }
                    None => return false,
                }
            }
        }
        self.persist();
        true
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
            .map(|(s, ts)| (Self::summary_of(s), *ts))
            .collect();
        v.sort_by_key(|(_, ts)| std::cmp::Reverse(*ts));
        v.into_iter().map(|(s, _)| s).collect()
    }

    /// Rename a tab. A non-empty custom title also pins it: the auto-title
    /// only fills blank titles, so renames stick.
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

    /// Fork: clone the head-path prefix up to and including raw thread
    /// `index` into a new single-limb session.
    pub fn fork(&self, id: &str, index: usize) -> Option<Session> {
        let source = self.get(id)?;
        let path = source.thread();
        if path.is_empty() {
            return None;
        }
        let keep = index.min(path.len() - 1);
        let new_id = self.next_id();
        let forked = Self::linear_tree(&new_id, &format!("{} (fork)", source.title), path[..=keep].to_vec(), Self::now_ms());
        self.sessions.lock().unwrap().insert(new_id, forked.clone());
        self.persist();
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

    fn user(content: &str) -> Value {
        json!({"role":"user","content":content})
    }

    fn assistant(content: &str) -> Value {
        json!({"role":"assistant","content":content})
    }

    fn thread() -> Vec<Value> {
        vec![sys(), user("hello"), assistant("hi"), user("second question")]
    }

    fn live(m: &SessionManager, id: &str) -> Vec<Value> {
        m.thread(id).unwrap()
    }

    #[test]
    fn create_seeds_system_and_lists() {
        let m = SessionManager::new();
        let s = m.create(sys());
        assert_eq!(live(&m, &s.id).len(), 1);
        assert!(m.exists(&s.id));
        assert_eq!(m.list().len(), 1);
    }

    #[test]
    fn set_messages_appends_and_keeps_prefix_nodes() {
        let m = SessionManager::new();
        let s = m.create(sys());
        m.set_messages(&s.id, thread());
        let before = m.get(&s.id).unwrap();
        let head_before = before.tree.head;
        // Write back the same thread plus one assistant reply: only one node added.
        let mut ext = thread();
        ext.push(assistant("answer"));
        m.set_messages(&s.id, ext);
        let after = m.get(&s.id).unwrap();
        assert_eq!(after.thread().len(), 5);
        assert_eq!(after.tree.nodes.len(), 5);
        assert_ne!(after.tree.head, head_before);
    }

    #[test]
    fn fork_truncates_and_keeps_history() {
        let m = SessionManager::new();
        let s = m.create(sys());
        m.set_messages(&s.id, thread());
        // Fork from index 2 (the assistant "hi" reply): keep system + user + assistant.
        let f = m.fork(&s.id, 2).unwrap();
        assert_eq!(f.thread().len(), 3);
        assert_eq!(f.thread()[1].get("content").unwrap(), "hello");
        assert_eq!(f.thread()[2].get("content").unwrap(), "hi");
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
        assert_eq!(back.thread().len(), 4);
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
    fn undo_keeps_phantom_limb_for_redo() {
        let m = SessionManager::new();
        let s = m.create(sys());
        m.set_messages(&s.id, thread());
        assert_eq!(m.undo(&s.id).unwrap(), "second question");
        let after = m.get(&s.id).unwrap();
        assert_eq!(after.thread().len(), 3);
        assert_eq!(after.thread()[2].get("content").unwrap(), "hi");
        // Nothing pruned: the undone node still exists as a child of the head.
        assert_eq!(after.tree.nodes.len(), 4);
        // Redo walks back down to the phantom head.
        let redone = m.redo(&s.id).unwrap();
        assert_eq!(redone.len(), 4);
        assert_eq!(redone[3].get("content").unwrap(), "second question");
    }

    #[test]
    fn undo_twice_then_branch_keeps_both_limbs() {
        let m = SessionManager::new();
        let s = m.create(sys());
        m.set_messages(&s.id, thread());
        assert_eq!(m.undo(&s.id).unwrap(), "second question");
        assert_eq!(m.undo(&s.id).unwrap(), "hello");
        assert_eq!(live(&m, &s.id).len(), 1);
        // New message from the rewound root forks a fresh limb; old limbs stay.
        m.append_message(&s.id, user("other path"));
        let after = m.get(&s.id).unwrap();
        assert_eq!(after.thread().len(), 2);
        assert_eq!(after.thread()[1].get("content").unwrap(), "other path");
        assert!(after.tree.nodes.len() > 2);
        // Redo stack was cleared by the new append.
        assert!(m.redo(&s.id).is_none());
    }

    #[test]
    fn goto_navigates_to_any_node() {
        let m = SessionManager::new();
        let s = m.create(sys());
        m.set_messages(&s.id, thread());
        let full = m.get(&s.id).unwrap();
        let hello_id = full
            .tree
            .nodes
            .values()
            .find(|n| n.message.get("content").and_then(|c| c.as_str()) == Some("hello"))
            .unwrap()
            .id;
        let thread = m.goto(&s.id, hello_id).unwrap();
        assert_eq!(thread.len(), 2);
        let kids = m.children(&s.id).unwrap();
        assert!(kids.iter().any(|k| k.preview == "hi"));
    }

    #[test]
    fn archive_hides_from_lists_but_stays_restorable() {
        let m = SessionManager::new();
        let s = m.create(sys());
        m.set_messages(&s.id, thread());
        assert!(m.set_archived(&s.id, true));
        assert!(m.list().is_empty());
        assert!(m.delete(&s.id));
        let trash = m.trash_list();
        assert_eq!(trash.len(), 1);
        assert!(trash[0].archived);
        assert!(m.set_archived(&s.id, false));
        assert!(!m.trash_list()[0].archived);
        assert!(m.restore(&s.id).is_some());
    }

    #[test]
    fn spawn_links_parent_role_goal_and_workspace() {
        let m = SessionManager::new();
        let parent = m.create(sys());
        let child = m.spawn_subagent(&parent.id, "backend", "Build the API", sys()).unwrap();
        assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
        assert_eq!(child.role, "backend");
        assert_eq!(child.goal, "Build the API");
        assert!(child.workspace().starts_with("/root/workspace/"));
        assert!(child.workspace().contains(&child.id));
        // Parent lists the child; blank role or unknown parent refused.
        assert_eq!(m.subagents(&parent.id).len(), 1);
        assert_eq!(m.subagents(&parent.id)[0].role, "backend");
        assert!(m.spawn_subagent(&parent.id, "  ", "x", sys()).is_none());
        assert!(m.spawn_subagent("nope", "qa", "x", sys()).is_none());
    }

    #[test]
    fn merge_appends_child_thread_and_archives() {
        let m = SessionManager::new();
        let parent = m.create(sys());
        m.set_messages(&parent.id, thread());
        let child = m.spawn_subagent(&parent.id, "qa", "Verify", sys()).unwrap();
        m.set_messages(&child.id, vec![sys(), user("check this"), assistant("looks good")]);
        let merged = m.merge_child(&child.id).unwrap();
        // Parent gains the child's two non-system turns.
        let msgs = merged.thread();
        assert_eq!(msgs.len(), 4 + 2);
        assert_eq!(msgs[4].get("content").unwrap(), "check this");
        assert_eq!(msgs[5].get("content").unwrap(), "looks good");
        // Child archived (audit trail kept, hidden from lists/subagents).
        assert!(m.get(&child.id).unwrap().archived);
        assert!(m.subagents(&parent.id).is_empty());
        assert_eq!(m.list().iter().filter(|s| s.id == child.id).count(), 0);
        // Merging twice or merging a root is refused (no duplicates).
        assert!(m.merge_child(&child.id).is_none());
        assert!(m.merge_child(&parent.id).is_none());
        assert!(m.merge_child("nope").is_none());
    }

    #[test]
    fn team_fields_persist_across_reload() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "forgerig-team-{}.json",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let m = SessionManager::with_persist_path(Some(path.clone()));
        let parent = m.create(sys());
        let child = m.spawn_subagent(&parent.id, "devops", "Ship it", sys()).unwrap();
        drop(m);

        let m2 = SessionManager::with_persist_path(Some(path.clone()));
        let back = m2.get(&child.id).unwrap();
        assert_eq!(back.parent_id.as_deref(), Some(parent.id.as_str()));
        assert_eq!(back.role, "devops");
        assert_eq!(back.goal, "Ship it");
        assert_eq!(m2.subagents(&parent.id).len(), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn persistence_round_trips_tree_and_trash() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "forgerig-sessions-{}.json",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let m = SessionManager::with_persist_path(Some(path.clone()));
        let s = m.create(sys());
        m.set_messages(&s.id, thread());
        m.undo(&s.id).unwrap();
        assert!(m.delete(&s.id));
        drop(m);

        let m2 = SessionManager::with_persist_path(Some(path.clone()));
        assert!(!m2.exists(&s.id));
        assert_eq!(m2.trash_list().len(), 1);
        let back = m2.restore(&s.id).unwrap();
        // Restored at the rewound head; the phantom limb survived the reload.
        assert_eq!(back.thread().len(), 3);
        assert_eq!(back.tree.nodes.len(), 4);
        assert!(m2.redo(&s.id).is_some());
        let _ = std::fs::remove_file(path);
    }
}
