//! In-memory trajectory store: per-session ordered event log + usage
//! aggregation. Persistence (SQLite, JSONL files) is out of scope for
//! Phase 5 — the public API is designed so a future persistent backend
//! can implement it without changing call sites.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::{Event, SessionKey, TrajectoryEvent, UsageSummary};

/// Thread-safe in-memory trajectory store. Cheap to clone (Arc-shared).
#[derive(Debug, Default, Clone)]
pub struct TrajectoryStore {
    inner: Arc<RwLock<HashMap<SessionKey, SessionLog>>>,
}

#[derive(Debug, Default)]
struct SessionLog {
    events: Vec<Event>,
    next_seq: AtomicU64,
}

impl TrajectoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an event for the given session and return its assigned
    /// monotonic sequence number.
    pub async fn record(
        &self,
        session_id: &str,
        kind: TrajectoryEvent,
    ) -> (uuid::Uuid, u64) {
        let id = uuid::Uuid::new_v4();
        let at = std::time::SystemTime::now();
        let mut guard = self.inner.write().await;
        let log = guard.entry(session_id.to_string()).or_default();
        let seq = log.next_seq.fetch_add(1, Ordering::SeqCst);
        log.events.push(Event {
            id,
            seq,
            at,
            session_id: session_id.to_string(),
            kind,
        });
        (id, seq)
    }

    /// Return all events for a session in arrival order (oldest first).
    pub async fn list(&self, session_id: &str) -> Vec<Event> {
        let guard = self.inner.read().await;
        guard
            .get(session_id)
            .map(|log| log.events.clone())
            .unwrap_or_default()
    }

    /// Return events filtered by kind.
    pub async fn list_by_kind(
        &self,
        session_id: &str,
        kind: crate::TrajectoryEventKind,
    ) -> Vec<Event> {
        self.list(session_id)
            .await
            .into_iter()
            .filter(|e| e.kind.kind() == kind)
            .collect()
    }

    /// Aggregate usage across all events in a session.
    pub async fn usage(&self, session_id: &str) -> UsageSummary {
        let mut total = UsageSummary::empty();
        for event in self.list(session_id).await {
            match &event.kind {
                TrajectoryEvent::LlmCallCompleted {
                    input_tokens,
                    output_tokens,
                    ..
                } => {
                    total.add_llm_usage(*input_tokens, *output_tokens);
                }
                TrajectoryEvent::RenderStarted { .. } => {
                    total.add_render_call();
                }
                TrajectoryEvent::Error { .. } => {
                    total.add_error();
                }
                _ => {}
            }
        }
        total
    }

    /// Serialize the session's trajectory to JSON. Uses the same serde
    /// representation as the in-memory events, so a round-trip preserves
    /// every field.
    pub async fn export_json(&self, session_id: &str) -> Result<String, serde_json::Error> {
        let events = self.list(session_id).await;
        serde_json::to_string_pretty(&events)
    }

    /// Drop all events for a session (e.g. after persistence flush).
    pub async fn clear(&self, session_id: &str) {
        self.inner.write().await.remove(session_id);
    }

    /// Count events across all sessions (for tests / metrics).
    pub async fn total_event_count(&self) -> usize {
        self.inner
            .read()
            .await
            .values()
            .map(|log| log.events.len())
            .sum()
    }
}
