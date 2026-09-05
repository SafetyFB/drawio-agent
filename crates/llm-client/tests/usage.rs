//! TDD tests for token usage types.

use drawio_agent_llm_client::Usage;

#[test]
fn usage_default_is_zero() {
    let u = Usage::default();
    assert_eq!(u.input_tokens, 0);
    assert_eq!(u.output_tokens, 0);
    assert_eq!(u.total(), 0);
}

#[test]
fn usage_total_sums_input_and_output() {
    let u = Usage {
        input_tokens: 1234,
        output_tokens: 567,
    };
    assert_eq!(u.total(), 1801);
}

#[test]
fn usage_add_accumulates_counts() {
    let mut a = Usage {
        input_tokens: 100,
        output_tokens: 50,
    };
    let b = Usage {
        input_tokens: 200,
        output_tokens: 80,
    };
    a.add(&b);
    assert_eq!(a.input_tokens, 300);
    assert_eq!(a.output_tokens, 130);
    assert_eq!(a.total(), 430);
}

#[test]
fn usage_add_is_commutative_in_total() {
    let a = Usage {
        input_tokens: 100,
        output_tokens: 50,
    };
    let b = Usage {
        input_tokens: 200,
        output_tokens: 80,
    };
    let mut x = a;
    x.add(&b);
    assert_eq!(x.total(), a.total() + b.total());
}

#[test]
fn usage_serializes_to_json_with_snake_case_fields() {
    let u = Usage {
        input_tokens: 10,
        output_tokens: 20,
    };
    let json = serde_json::to_string(&u).expect("serialize");
    assert!(json.contains("\"input_tokens\":10"), "got {json}");
    assert!(json.contains("\"output_tokens\":20"), "got {json}");
}

#[test]
fn usage_deserializes_from_json() {
    let json = r#"{"input_tokens":42,"output_tokens":7}"#;
    let u: Usage = serde_json::from_str(json).expect("deserialize");
    assert_eq!(u.input_tokens, 42);
    assert_eq!(u.output_tokens, 7);
}
