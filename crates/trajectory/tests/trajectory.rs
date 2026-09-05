//! TDD #1: TrajectoryStore — record, list, filter, usage, export.

use drawio_agent_trajectory::{
    Event, TrajectoryEvent, TrajectoryEventKind, TrajectoryStore, UsageSummary,
};

fn store() -> TrajectoryStore {
    TrajectoryStore::new()
}

// ---------------------------------------------------------------------------
// Record + list
// ---------------------------------------------------------------------------

#[tokio::test]
async fn record_returns_event_with_monotonic_seq() {
    let s = store();
    let (id1, seq1) = s
        .record("a", TrajectoryEvent::LlmCallStarted { prompt_chars: 10, json_mode: false })
        .await;
    let (id2, seq2) = s
        .record("a", TrajectoryEvent::LlmCallStarted { prompt_chars: 20, json_mode: false })
        .await;
    assert_ne!(id1, id2, "each event gets a fresh UUID");
    assert_eq!(seq1, 0, "first event seq is 0");
    assert_eq!(seq2, 1, "second event seq is 1");
}

#[tokio::test]
async fn list_returns_events_in_arrival_order() {
    let s = store();
    s.record("a", TrajectoryEvent::LlmCallStarted { prompt_chars: 0, json_mode: false }).await;
    s.record("a", TrajectoryEvent::LlmCallCompleted {
        input_tokens: 10, output_tokens: 5, duration_ms: 100, finish_reason: None,
    }).await;
    s.record("a", TrajectoryEvent::RenderStarted { scale: 1.0 }).await;

    let events = s.list("a").await;
    assert_eq!(events.len(), 3);
    let kinds: Vec<TrajectoryEventKind> = events.iter().map(|e| e.kind.kind()).collect();
    assert_eq!(
        kinds,
        vec![
            TrajectoryEventKind::LlmCallStarted,
            TrajectoryEventKind::LlmCallCompleted,
            TrajectoryEventKind::RenderStarted,
        ]
    );
}

#[tokio::test]
async fn list_for_unknown_session_returns_empty() {
    let s = store();
    let events = s.list("nope").await;
    assert!(events.is_empty());
}

#[tokio::test]
async fn list_isolated_per_session() {
    let s = store();
    s.record("a", TrajectoryEvent::LlmCallStarted { prompt_chars: 0, json_mode: false }).await;
    s.record("b", TrajectoryEvent::RenderStarted { scale: 1.0 }).await;
    s.record("a", TrajectoryEvent::RenderCompleted { bytes: 100, duration_ms: 5 }).await;

    assert_eq!(s.list("a").await.len(), 2);
    assert_eq!(s.list("b").await.len(), 1);
    // Sequences are also per-session, not global.
    let a_events = s.list("a").await;
    assert_eq!(a_events[0].seq, 0);
    assert_eq!(a_events[1].seq, 1);
}

// ---------------------------------------------------------------------------
// list_by_kind
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_by_kind_filters_correctly() {
    let s = store();
    s.record("a", TrajectoryEvent::LlmCallStarted { prompt_chars: 0, json_mode: false }).await;
    s.record("a", TrajectoryEvent::LlmCallStarted { prompt_chars: 0, json_mode: true }).await;
    s.record("a", TrajectoryEvent::RenderStarted { scale: 1.0 }).await;
    s.record("a", TrajectoryEvent::LlmCallCompleted {
        input_tokens: 1, output_tokens: 1, duration_ms: 0, finish_reason: Some("stop".into()),
    }).await;

    let starts = s.list_by_kind("a", TrajectoryEventKind::LlmCallStarted).await;
    assert_eq!(starts.len(), 2);

    let completed = s.list_by_kind("a", TrajectoryEventKind::LlmCallCompleted).await;
    assert_eq!(completed.len(), 1);

    let renders = s.list_by_kind("a", TrajectoryEventKind::RenderStarted).await;
    assert_eq!(renders.len(), 1);

    let errors = s.list_by_kind("a", TrajectoryEventKind::Error).await;
    assert!(errors.is_empty());
}

// ---------------------------------------------------------------------------
// usage()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn usage_aggregates_token_counts_and_counts() {
    let s = store();
    s.record("a", TrajectoryEvent::LlmCallCompleted {
        input_tokens: 100, output_tokens: 50, duration_ms: 200, finish_reason: None,
    }).await;
    s.record("a", TrajectoryEvent::LlmCallCompleted {
        input_tokens: 30, output_tokens: 10, duration_ms: 100, finish_reason: None,
    }).await;
    s.record("a", TrajectoryEvent::RenderStarted { scale: 1.0 }).await;
    s.record("a", TrajectoryEvent::RenderCompleted { bytes: 1234, duration_ms: 50 }).await;
    s.record("a", TrajectoryEvent::Error { stage: "llm".into(), message: "boom".into() }).await;

    let u: UsageSummary = s.usage("a").await;
    assert_eq!(u.input_tokens, 130);
    assert_eq!(u.output_tokens, 60);
    assert_eq!(u.llm_calls, 2);
    assert_eq!(u.render_calls, 1, "RenderStarted counted as one render");
    assert_eq!(u.errors, 1);
    assert_eq!(u.total_tokens(), 190);
}

#[tokio::test]
async fn usage_for_unknown_session_is_zero() {
    let s = store();
    let u = s.usage("nope").await;
    assert_eq!(u, UsageSummary::empty());
}

#[tokio::test]
async fn usage_add_helper_accumulates() {
    let mut a = UsageSummary::empty();
    a.add_llm_usage(10, 5);
    a.add_llm_usage(20, 10);
    a.add_render_call();
    a.add_error();

    let mut b = UsageSummary::empty();
    b.add_llm_usage(3, 1);

    a.add(&b);
    assert_eq!(a.input_tokens, 33);
    assert_eq!(a.output_tokens, 16);
    assert_eq!(a.llm_calls, 3);
    assert_eq!(a.render_calls, 1);
    assert_eq!(a.errors, 1);
}

// ---------------------------------------------------------------------------
// export_json + round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn export_json_is_valid_json_with_all_events() {
    let s = store();
    s.record("a", TrajectoryEvent::LlmCallStarted { prompt_chars: 42, json_mode: false }).await;
    s.record("a", TrajectoryEvent::LlmCallCompleted {
        input_tokens: 7, output_tokens: 3, duration_ms: 99, finish_reason: Some("stop".into()),
    }).await;

    let json = s.export_json("a").await.unwrap();
    let parsed: Vec<Event> = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].seq, 0);
    assert_eq!(parsed[1].seq, 1);
    // Round-trip preserves the LlmCallStarted payload exactly.
    match &parsed[0].kind {
        TrajectoryEvent::LlmCallStarted { prompt_chars, json_mode } => {
            assert_eq!(*prompt_chars, 42);
            assert!(!*json_mode);
        }
        other => panic!("expected LlmCallStarted, got {other:?}"),
    }
}

#[tokio::test]
async fn export_json_for_unknown_session_is_empty_array() {
    let s = store();
    let json = s.export_json("nope").await.unwrap();
    assert_eq!(json, "[]");
}

// ---------------------------------------------------------------------------
// clear() + total_event_count
// ---------------------------------------------------------------------------

#[tokio::test]
async fn clear_removes_only_one_session() {
    let s = store();
    s.record("a", TrajectoryEvent::LlmCallStarted { prompt_chars: 0, json_mode: false }).await;
    s.record("b", TrajectoryEvent::RenderStarted { scale: 1.0 }).await;

    s.clear("a").await;
    assert!(s.list("a").await.is_empty());
    assert_eq!(s.list("b").await.len(), 1);
}

#[tokio::test]
async fn total_event_count_sums_across_sessions() {
    let s = store();
    s.record("a", TrajectoryEvent::LlmCallStarted { prompt_chars: 0, json_mode: false }).await;
    s.record("a", TrajectoryEvent::LlmCallStarted { prompt_chars: 0, json_mode: false }).await;
    s.record("b", TrajectoryEvent::RenderStarted { scale: 1.0 }).await;

    assert_eq!(s.total_event_count().await, 3);
}

// ---------------------------------------------------------------------------
// TrajectoryEventKind::kind() mapping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn trajectory_event_kind_matches_variant() {
    let cases: Vec<(TrajectoryEvent, TrajectoryEventKind)> = vec![
        (
            TrajectoryEvent::LlmCallStarted { prompt_chars: 0, json_mode: false },
            TrajectoryEventKind::LlmCallStarted,
        ),
        (
            TrajectoryEvent::LlmCallCompleted {
                input_tokens: 0, output_tokens: 0, duration_ms: 0, finish_reason: None,
            },
            TrajectoryEventKind::LlmCallCompleted,
        ),
        (
            TrajectoryEvent::RenderStarted { scale: 1.0 },
            TrajectoryEventKind::RenderStarted,
        ),
        (
            TrajectoryEvent::RenderCompleted { bytes: 0, duration_ms: 0 },
            TrajectoryEventKind::RenderCompleted,
        ),
        (
            TrajectoryEvent::Error { stage: "x".into(), message: "y".into() },
            TrajectoryEventKind::Error,
        ),
        (
            TrajectoryEvent::StateTransition { from: None, to: "review".into() },
            TrajectoryEventKind::StateTransition,
        ),
    ];
    for (event, expected) in cases {
        assert_eq!(event.kind(), expected);
    }
}

// ---------------------------------------------------------------------------
// Concurrency: store is shared-safe
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_records_keep_unique_seqs_and_ids() {
    let s = store();
    let mut handles = vec![];
    for _ in 0..50 {
        let s = s.clone();
        handles.push(tokio::spawn(async move {
            s.record("a", TrajectoryEvent::LlmCallStarted { prompt_chars: 0, json_mode: false })
                .await
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let events = s.list("a").await;
    assert_eq!(events.len(), 50);
    // Seq values are a permutation of 0..50.
    let mut seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    seqs.sort();
    assert_eq!(seqs, (0..50_u64).collect::<Vec<_>>());
    // All event ids are unique.
    let mut ids: Vec<uuid::Uuid> = events.iter().map(|e| e.id).collect();
    let len_before = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), len_before, "all event ids must be unique");
}
