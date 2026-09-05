//! TDD tests for SessionUsage — per-session token aggregation and budget guard.

use drawio_agent_llm_client::{SessionUsage, Usage};

#[test]
fn new_session_starts_empty() {
    let s = SessionUsage::new();
    assert_eq!(s.total_input(), 0);
    assert_eq!(s.total_output(), 0);
    assert_eq!(s.total_tokens(), 0);
    assert!((s.total_cost - 0.0).abs() < 1e-9);
    assert!(!s.exceeds_budget());
    assert!(s.budget_remaining().is_none());
}

#[test]
fn record_accumulates_by_model() {
    let mut s = SessionUsage::new();
    s.record(
        "glm-4-flash",
        "generate",
        &Usage {
            input_tokens: 100,
            output_tokens: 50,
        },
        0.001,
    );
    s.record(
        "glm-4-flash",
        "generate",
        &Usage {
            input_tokens: 200,
            output_tokens: 80,
        },
        0.002,
    );
    s.record(
        "qwen-vl-plus",
        "review",
        &Usage {
            input_tokens: 500,
            output_tokens: 100,
        },
        0.05,
    );

    let glm = s.by_model.get("glm-4-flash").expect("glm-4-flash recorded");
    assert_eq!(glm.input_tokens, 300);
    assert_eq!(glm.output_tokens, 130);

    let qwen = s.by_model.get("qwen-vl-plus").expect("qwen-vl-plus recorded");
    assert_eq!(qwen.input_tokens, 500);
    assert_eq!(qwen.output_tokens, 100);
}

#[test]
fn record_accumulates_by_purpose() {
    let mut s = SessionUsage::new();
    s.record(
        "glm-4-flash",
        "generate",
        &Usage {
            input_tokens: 100,
            output_tokens: 50,
        },
        0.001,
    );
    s.record(
        "qwen-vl-plus",
        "review",
        &Usage {
            input_tokens: 500,
            output_tokens: 100,
        },
        0.05,
    );
    s.record(
        "glm-4-flash",
        "review",
        &Usage {
            input_tokens: 200,
            output_tokens: 30,
        },
        0.02,
    );

    let generate = s.by_purpose.get("generate").expect("generate recorded");
    assert_eq!(generate.input_tokens, 100);
    assert_eq!(generate.output_tokens, 50);

    let review = s.by_purpose.get("review").expect("review recorded");
    assert_eq!(review.input_tokens, 700);
    assert_eq!(review.output_tokens, 130);
}

#[test]
fn record_accumulates_total_cost() {
    let mut s = SessionUsage::new();
    s.record(
        "m",
        "p",
        &Usage {
            input_tokens: 1,
            output_tokens: 1,
        },
        0.001,
    );
    s.record(
        "m",
        "p",
        &Usage {
            input_tokens: 1,
            output_tokens: 1,
        },
        0.0042,
    );
    s.record(
        "m",
        "p",
        &Usage {
            input_tokens: 1,
            output_tokens: 1,
        },
        0.01,
    );
    assert!((s.total_cost - 0.0152).abs() < 1e-9, "got {}", s.total_cost);
}

#[test]
fn total_tokens_sums_input_and_output() {
    let mut s = SessionUsage::new();
    s.record(
        "m",
        "p",
        &Usage {
            input_tokens: 800,
            output_tokens: 400,
        },
        0.0,
    );
    s.record(
        "m",
        "p",
        &Usage {
            input_tokens: 200,
            output_tokens: 100,
        },
        0.0,
    );
    assert_eq!(s.total_input(), 1000);
    assert_eq!(s.total_output(), 500);
    assert_eq!(s.total_tokens(), 1500);
}

#[test]
fn without_budget_exceeds_budget_is_false() {
    let mut s = SessionUsage::new();
    for _ in 0..10 {
        s.record(
            "m",
            "p",
            &Usage {
                input_tokens: 1000,
                output_tokens: 1000,
            },
            100.0,
        );
    }
    assert!(!s.exceeds_budget(), "no budget => never over budget");
    assert!(s.budget_remaining().is_none());
}

#[test]
fn with_budget_exceeds_when_total_cost_at_or_above_threshold() {
    let mut s = SessionUsage::with_budget(0.05);
    assert!(!s.exceeds_budget());
    s.record("m", "p", &Usage::default(), 0.04);
    assert!(
        !s.exceeds_budget(),
        "0.04 < 0.05 budget should not exceed"
    );
    s.record("m", "p", &Usage::default(), 0.01);
    assert!(
        s.exceeds_budget(),
        "0.05 >= 0.05 budget must trigger"
    );
}

#[test]
fn budget_remaining_decreases_with_cost() {
    let mut s = SessionUsage::with_budget(1.0);
    assert_eq!(s.budget_remaining(), Some(1.0));
    s.record("m", "p", &Usage::default(), 0.3);
    assert_eq!(s.budget_remaining(), Some(0.7));
    s.record("m", "p", &Usage::default(), 0.7);
    assert_eq!(s.budget_remaining(), Some(0.0));
}

#[test]
fn budget_remaining_can_go_negative() {
    let mut s = SessionUsage::with_budget(1.0);
    s.record("m", "p", &Usage::default(), 1.5);
    assert_eq!(s.budget_remaining(), Some(-0.5));
    assert!(s.exceeds_budget());
}

#[test]
fn by_model_and_by_purpose_keys_match_input_strings() {
    // Keys should be exactly what the caller passed (case-sensitive, no
    // normalization).
    let mut s = SessionUsage::new();
    s.record("GLM-4-Flash", "Generate", &Usage::default(), 0.0);
    assert!(s.by_model.contains_key("GLM-4-Flash"));
    assert!(!s.by_model.contains_key("glm-4-flash"));
    assert!(s.by_purpose.contains_key("Generate"));
}

#[test]
fn empty_record_calls_dont_corrupt_state() {
    let mut s = SessionUsage::new();
    s.record("m", "p", &Usage::default(), 0.0);
    // Recording zero usage and zero cost is a no-op against totals.
    assert_eq!(s.total_input(), 0);
    assert_eq!(s.total_output(), 0);
    assert_eq!(s.total_cost, 0.0);
}
