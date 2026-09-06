//! In-memory session storage for the Draw.io Agent server.
//!
//! Phase 4 skeleton: HashMap<SessionId, SessionData> behind RwLock,
//! plus a separate EventBus for WebSocket pub/sub. Persistence (SQLite,
//! etc.) is intentionally out of scope.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex, RwLock};
use uuid::Uuid;

/// Stable session identifier (UUIDv4 string).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lightweight timestamp helper (we don't pull chrono for a Phase 4 stub).
fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Crude ISO-ish format; good enough for a stub.
    format!("1970-01-01T00:00:00Z+{secs}s")
}

/// Convert a `SystemTime` to milliseconds since the Unix epoch (i64).
/// Pre-epoch times coerce to 0 so the wire format stays a JSON number.
fn system_time_ms(t: &std::time::SystemTime) -> i64 {
    use std::time::UNIX_EPOCH;
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Metadata returned by `GET /api/sessions/:id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub created_at: String,
    pub version_count: usize,
    pub current_version: Option<Uuid>,
}

/// Per-version metadata in the history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionMeta {
    pub version_id: Uuid,
    pub created_at: String,
    /// Free-form tag for what produced this version ("generate" / "patch").
    pub kind: String,
    /// Optional human-readable summary (truncated prompt or instruction).
    pub summary: Option<String>,
}

/// The full session record.
#[derive(Debug, Clone)]
pub struct SessionData {
    pub meta: SessionMeta,
    pub versions: Vec<VersionEntry>,
    /// Unix milliseconds at creation (used to order the session list).
    pub created_at: u64,
    /// Persistent conversation memory (R2): one entry per user request
    /// (role="user") and per completed turn (role="agent", a short
    /// summary). Replayed into later runs as background context so the
    /// model remembers earlier asks and outcomes across runSend calls.
    pub conversation: Vec<ConversationEntry>,
}

/// One entry of the session's persistent conversation memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationEntry {
    /// `"user"` (a request) or `"agent"` (a completed-turn summary).
    pub role: String,
    /// Short human/LLM-readable text: what was asked / what was done.
    pub text: String,
    /// Which endpoint produced this entry ("generate" | "patch" |
    /// "agent-loop").
    pub kind: String,
    /// Version created by this turn, if any.
    pub version_id: Option<Uuid>,
    /// Unix milliseconds.
    pub at_ms: u64,
}

impl ConversationEntry {
    pub fn new(
        role: &str,
        kind: &str,
        text: impl Into<String>,
        version_id: Option<Uuid>,
    ) -> Self {
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            role: role.to_string(),
            kind: kind.to_string(),
            text: text.into(),
            version_id,
            at_ms,
        }
    }
}

/// One past (or current) version: the XML plus metadata.
#[derive(Debug, Clone)]
pub struct VersionEntry {
    pub meta: VersionMeta,
    pub xml: String,
}

/// Events broadcast over the WebSocket.
///
/// Serialization is hand-rolled: `VersionCreated` / `Error` keep the
/// original flat shape (`{"type":"version_created",...}`), while the
/// `Trajectory` payload rides under a stable `event` key carrying the
/// internally-tagged `TrajectoryEvent`:
/// `{"type":"trajectory","event":{"kind":"llm_call_started",...}}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", content = "event", rename_all = "snake_case")]
pub enum WsEvent {
    /// A new version was added.
    VersionCreated {
        session_id: SessionId,
        version_id: Uuid,
        kind: String,
    },
    /// An error occurred during a long-running operation.
    Error {
        session_id: SessionId,
        message: String,
    },
    /// A trajectory event recorded by an action handler. Boxed so the
    /// enum stays small despite the heap-allocated payload.
    Trajectory(Box<drawio_agent_trajectory::Event>),
}

impl Serialize for WsEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        match self {
            WsEvent::VersionCreated {
                session_id,
                version_id,
                kind,
            } => {
                let mut s = serializer.serialize_struct("WsEvent", 4)?;
                s.serialize_field("type", "version_created")?;
                s.serialize_field("session_id", session_id)?;
                s.serialize_field("version_id", version_id)?;
                s.serialize_field("kind", kind)?;
                s.end()
            }
            WsEvent::Error { session_id, message } => {
                let mut s = serializer.serialize_struct("WsEvent", 3)?;
                s.serialize_field("type", "error")?;
                s.serialize_field("session_id", session_id)?;
                s.serialize_field("message", message)?;
                s.end()
            }
            WsEvent::Trajectory(event) => {
                let mut s = serializer.serialize_struct("WsEvent", 4)?;
                s.serialize_field("type", "trajectory")?;
                s.serialize_field("seq", &event.seq)?;
                s.serialize_field("at_ms", &system_time_ms(&event.at))?;
                s.serialize_field("event", &event.kind)?;
                s.end()
            }
        }
    }
}

/// In-memory session storage. Cheap to clone (Arc-shared).
#[derive(Debug, Default, Clone)]
pub struct SessionStore {
    inner: Arc<RwLock<HashMap<SessionId, SessionData>>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new empty session and return its id.
    pub async fn create(&self) -> SessionId {
        let id = SessionId::new();
        let meta = SessionMeta {
            id: id.clone(),
            created_at: now_iso(),
            version_count: 0,
            current_version: None,
        };
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let data = SessionData {
            meta,
            versions: Vec::new(),
            created_at,
            conversation: Vec::new(),
        };
        self.inner.write().await.insert(id.clone(), data);
        id
    }

    /// Look up a session by id.
    pub async fn get(&self, id: &SessionId) -> Option<SessionData> {
        self.inner.read().await.get(id).cloned()
    }

    /// Check if a session exists.
    pub async fn contains(&self, id: &SessionId) -> bool {
        self.inner.read().await.contains_key(id)
    }

    /// Append a new version. Updates meta and sets it as current.
    pub async fn append_version(
        &self,
        id: &SessionId,
        kind: &str,
        summary: Option<String>,
        xml: &str,
    ) -> Option<Uuid> {
        let mut guard = self.inner.write().await;
        let data = guard.get_mut(id)?;
        let version_id = Uuid::new_v4();
        let entry = VersionEntry {
            meta: VersionMeta {
                version_id,
                created_at: now_iso(),
                kind: kind.to_string(),
                summary,
            },
            xml: xml.to_string(),
        };
        data.meta.version_count = data.versions.len() + 1;
        data.meta.current_version = Some(version_id);
        data.versions.push(entry);
        Some(version_id)
    }

    /// Snapshot of the session's current XML (the latest version).
    pub async fn current_xml(&self, id: &SessionId) -> Option<String> {
        let guard = self.inner.read().await;
        let data = guard.get(id)?;
        data.versions.last().map(|v| v.xml.clone())
    }

    /// Append a conversation entry (user ask or agent turn summary).
    pub async fn push_conversation(
        &self,
        id: &SessionId,
        entry: ConversationEntry,
    ) {
        let mut guard = self.inner.write().await;
        if let Some(data) = guard.get_mut(id) {
            data.conversation.push(entry);
        }
    }

    /// Snapshot of the session's conversation memory.
    pub async fn conversation(&self, id: &SessionId) -> Vec<ConversationEntry> {
        let guard = self.inner.read().await;
        guard
            .get(id)
            .map(|d| d.conversation.clone())
            .unwrap_or_default()
    }

    /// List all version metadata for a session, newest last.
    pub async fn versions(&self, id: &SessionId) -> Vec<VersionMeta> {
        let guard = self.inner.read().await;
        guard
            .get(id)
            .map(|d| d.versions.iter().map(|v| v.meta.clone()).collect())
            .unwrap_or_default()
    }

    /// List every session's data (caller sorts/orders as needed).
    pub async fn list_all(&self) -> Vec<SessionData> {
        let guard = self.inner.read().await;
        guard.values().cloned().collect()
    }
}

/// Per-session pub/sub bus for WebSocket events. Kept separate from
/// `SessionStore` so WS handlers don't need to hold a RwLock on the
/// session data just to subscribe.
#[derive(Clone, Default)]
pub struct EventBus {
    senders: Arc<Mutex<HashMap<SessionId, broadcast::Sender<WsEvent>>>>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus").finish_non_exhaustive()
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Idempotently create the channel for `id`. Subsequent calls return
    /// the existing sender so multiple action handlers can register
    /// independently.
    pub async fn get_or_create(&self, id: &SessionId) -> broadcast::Sender<WsEvent> {
        let mut senders = self.senders.lock().await;
        senders
            .entry(id.clone())
            .or_insert_with(|| {
                let (tx, _rx) = broadcast::channel(64);
                tx
            })
            .clone()
    }

    /// Subscribe to events for a session. Lazily creates the channel if
    /// it doesn't exist yet (so subscribers can register before any emit).
    /// Always returns `Some`.
    pub async fn subscribe(&self, id: &SessionId) -> Option<broadcast::Receiver<WsEvent>> {
        Some(self.get_or_create(id).await.subscribe())
    }

    /// Ensure the channel exists (idempotent) and publish an event to all
    /// current subscribers. `tx.send` returns `Err` if there are no
    /// receivers, which we swallow.
    pub async fn emit(&self, id: &SessionId, event: WsEvent) {
        let tx = self.get_or_create(id).await;
        let _ = tx.send(event);
    }
}
