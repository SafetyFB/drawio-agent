//! TDD tests for the PriceBook — token-to-currency conversion.

use drawio_agent_llm_client::{PriceBook, PriceEntry, Usage};

fn approx_eq(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[test]
fn cost_known_model_returns_correct_amount() {
    let mut book = PriceBook::new();
    book.insert(
        "glm-4-flash".to_string(),
        PriceEntry {
            input_per_1k: 0.0001,
            output_per_1k: 0.0001,
            currency: "CNY".to_string(),
        },
    );

    let usage = Usage {
        input_tokens: 1000,
        output_tokens: 500,
    };
    // 1000/1000 * 0.0001 + 500/1000 * 0.0001 = 0.0001 + 0.00005 = 0.00015
    let cost = book.cost("glm-4-flash", &usage);
    assert!(
        approx_eq(cost, 0.00015),
        "expected 0.00015, got {cost}"
    );
}

#[test]
fn cost_unknown_model_returns_zero() {
    let book = PriceBook::new();
    let usage = Usage {
        input_tokens: 1000,
        output_tokens: 500,
    };
    let cost = book.cost("never-seen-model", &usage);
    assert_eq!(cost, 0.0, "unknown model must cost 0 (not panic)");
}

#[test]
fn cost_zero_usage_returns_zero() {
    let mut book = PriceBook::new();
    book.insert(
        "qwen-vl-plus".to_string(),
        PriceEntry {
            input_per_1k: 0.008,
            output_per_1k: 0.008,
            currency: "CNY".to_string(),
        },
    );
    let cost = book.cost("qwen-vl-plus", &Usage::default());
    assert_eq!(cost, 0.0);
}

#[test]
fn cost_formula_matches_input_plus_output() {
    let mut book = PriceBook::new();
    book.insert(
        "m".to_string(),
        PriceEntry {
            input_per_1k: 1.0,
            output_per_1k: 3.0,
            currency: "USD".to_string(),
        },
    );
    let usage = Usage {
        input_tokens: 2000,
        output_tokens: 1000,
    };
    // 2 * 1.0 + 1 * 3.0 = 5.0
    let cost = book.cost("m", &usage);
    assert!(approx_eq(cost, 5.0), "expected 5.0, got {cost}");
}

#[test]
fn insert_then_get_roundtrip() {
    let mut book = PriceBook::new();
    let entry = PriceEntry {
        input_per_1k: 0.5,
        output_per_1k: 1.5,
        currency: "USD".to_string(),
    };
    book.insert("model-x".to_string(), entry.clone());
    let got = book.get("model-x").expect("model must be retrievable");
    assert_eq!(got.input_per_1k, 0.5);
    assert_eq!(got.output_per_1k, 1.5);
    assert_eq!(got.currency, "USD");
}

#[test]
fn get_unknown_model_returns_none() {
    let book = PriceBook::new();
    assert!(book.get("nonexistent").is_none());
}

#[test]
fn default_price_book_includes_common_models() {
    let book = PriceBook::default_book();
    // Cover the providers we plan to use out of the box.
    assert!(
        book.get("glm-4-flash").is_some(),
        "default book must include glm-4-flash"
    );
    assert!(
        book.get("qwen-vl-plus").is_some(),
        "default book must include qwen-vl-plus"
    );
    assert!(
        book.get("gpt-4o").is_some(),
        "default book must include gpt-4o"
    );
}

#[test]
fn default_book_costs_use_realistic_per_kil_token_prices() {
    // Sanity: default prices produce non-zero costs for a realistic call.
    let book = PriceBook::default_book();
    let usage = Usage {
        input_tokens: 1000,
        output_tokens: 500,
    };
    let cost = book.cost("glm-4-flash", &usage);
    assert!(cost > 0.0, "default glm-4-flash cost must be > 0: {cost}");
    assert!(cost < 1.0, "default glm-4-flash cost must be reasonable: {cost}");
}

#[test]
fn price_entry_currency_field_is_preserved() {
    let mut book = PriceBook::new();
    book.insert(
        "m".to_string(),
        PriceEntry {
            input_per_1k: 1.0,
            output_per_1k: 2.0,
            currency: "EUR".to_string(),
        },
    );
    assert_eq!(book.get("m").unwrap().currency, "EUR");
}

#[test]
fn price_entry_default_is_zero_zero_with_usd() {
    let e = PriceEntry::default();
    assert_eq!(e.input_per_1k, 0.0);
    assert_eq!(e.output_per_1k, 0.0);
    assert_eq!(e.currency, "USD");
}
