//! TDD tests for the runtime settings API:
//! GET/PUT /api/config + POST /api/config/test (connection test).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drawio_agent_llm_client::LlmProvider;
use drawio_agent_renderer::MockDriver;
use drawio_agent_server::{
    build_app_state_with_renderer, build_router, AppState, LlmSettings, ServerConfig,
};
use serde_json::Value;
use tower::ServiceExt;

/// Every test writes into a throwaway temp dir so the real
/// ~/.drawio-agent/config.json is never touched.
fn temp_config_path() -> std::path::PathBuf {
    std::env::temp_dir()
        .join(format!("drawio-agent-cfg-{}", uuid::Uuid::new_v4()))
        .join("config.json")
}

async fn mock_state_with_config_path(config_path: Option<std::path::PathBuf>) -> Arc<AppState> {
    let config = ServerConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        static_dir: None,
        config_path,
        llm: Some(LlmSettings {
            kind: drawio_agent_server::LlmKind::Mock,
            ..Default::default()
        }),
    };
    build_app_state_with_renderer(&config, Some(Arc::new(MockDriver::new())))
        .await
        .map(Arc::new)
        .unwrap()
}

fn router(state: Arc<AppState>) -> axum::Router {
    build_router((*state).clone())
}

async fn get_config(app: axum::Router) -> Value {
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn get_config_reports_defaults_and_fixed_renderer() {
    // GET-only test: no PUT happens, so None is safe (nothing is written).
    let state = mock_state_with_config_path(None).await;
    let app = router(state);
    let cfg = get_config(app).await;
    assert_eq!(cfg["llm"]["kind"], "mock");
    assert_eq!(cfg["renderer"], "chromium", "renderer is fixed");
    assert_eq!(cfg["config_file"], Value::Null);
}

#[tokio::test]
async fn put_config_hot_swaps_the_provider() {
    let state = mock_state_with_config_path(Some(temp_config_path())).await;
    let app = router(state.clone());

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "kind": "openai-compat",
                        "base_url": "https://example.test/v1/",
                        "api_key": "sk-abcdef1234567890",
                        "model": "my-vision-model",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // GET reflects the new settings (key masked).
    let cfg = get_config(app.clone()).await;
    assert_eq!(cfg["llm"]["kind"], "openai-compat");
    assert_eq!(cfg["llm"]["base_url"], "https://example.test/v1/");
    assert_eq!(cfg["llm"]["model"], "my-vision-model");
    let masked = cfg["llm"]["api_key_masked"].as_str().unwrap();
    assert!(!masked.contains("abcdef"), "key must never leak in full: {masked}");
    assert!(masked.contains("sk-"), "masked key keeps prefix");

    // The live provider was swapped without a restart.
    let active = state.llm.snapshot().await;
    assert_eq!(active.name(), "my-vision-model");
}

#[tokio::test]
async fn put_config_validates_openai_compat_fields() {
    let state = mock_state_with_config_path(Some(temp_config_path())).await;
    let app = router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "kind": "openai-compat",
                        "base_url": "",
                        "model": "",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_config_with_empty_key_keeps_existing_secret() {
    let state = mock_state_with_config_path(Some(temp_config_path())).await;
    let app = router(state.clone());

    let put = |body: Value| {
        app.clone().oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
    };

    let resp = put(serde_json::json!({
        "kind": "openai-compat",
        "base_url": "https://example.test/v1",
        "api_key": "sk-secret-value-123",
        "model": "m1",
    }))
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Second save without a key must NOT wipe the stored secret.
    let resp = put(serde_json::json!({
        "kind": "openai-compat",
        "base_url": "https://example.test/v1",
        "api_key": "",
        "model": "m2",
    }))
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let stored = state
        .llm_settings
        .read()
        .unwrap()
        .api_key
        .clone();
    assert_eq!(stored, "sk-secret-value-123");
}

#[tokio::test]
async fn put_config_persists_to_the_config_file() {
    let dir = std::env::temp_dir().join(format!("drawio-agent-cfg-{}", uuid::Uuid::new_v4()));
    let path = dir.join("config.json");
    let state = mock_state_with_config_path(Some(path.clone())).await;
    let app = router(state);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "kind": "openai-compat",
                        "base_url": "https://persist.test/v1",
                        "api_key": "sk-persist-12345678",
                        "model": "persist-model",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // File exists and round-trips through load_config_file.
    let loaded = drawio_agent_server::load_config_file(&path).expect("config file written");
    assert_eq!(loaded.base_url, "https://persist.test/v1");
    assert_eq!(loaded.model, "persist-model");
    assert_eq!(loaded.api_key, "sk-persist-12345678");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn connection_test_handles_validation_without_network() {
    let state = mock_state_with_config_path(Some(temp_config_path())).await;
    let app = router(state);

    // Mock kind: no network involved.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/test")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"kind": "mock"})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["ok"], true, "mock test must succeed offline: {v}");

    // Openai-compat without required fields: rejected before any request.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/test")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "kind": "openai-compat",
                        "base_url": "",
                        "model": "",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().contains("required"));
}

/// Silence unused-import warnings if trait methods are not called here.
#[allow(dead_code)]
fn _keep(_p: Arc<dyn LlmProvider>) {}

#[tokio::test]
async fn switching_to_mock_keeps_saved_credentials() {
    // Regression: saving mock used to wipe base_url/api_key/model, which
    // silently broke switching back to the real provider.
    let state = mock_state_with_config_path(Some(temp_config_path())).await;
    let app = router(state.clone());

    let save = |body: Value| {
        app.clone().oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
    };

    let resp = save(serde_json::json!({
        "kind": "openai-compat",
        "base_url": "https://real.test/v1",
        "api_key": "sk-real-key-123456",
        "model": "glm-4.6v",
    }))
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Flip to mock…
    let resp = save(serde_json::json!({"kind": "mock"})).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // …credentials must survive.
    let stored = state.llm_settings.read().unwrap().clone();
    assert_eq!(stored.kind, drawio_agent_server::LlmKind::Mock);
    assert_eq!(stored.base_url, "https://real.test/v1");
    assert_eq!(stored.api_key, "sk-real-key-123456");
    assert_eq!(stored.model, "glm-4.6v");
}

#[tokio::test]
async fn unconfigured_provider_fails_fast_with_settings_hint() {
    // No file, no env, no override -> every call errors with a clear hint
    // (never a silent mock fallback).
    let settings = LlmSettings {
        kind: drawio_agent_server::LlmKind::Unconfigured,
        ..Default::default()
    };
    let provider = drawio_agent_server::build_provider(&settings);
    let err = provider
        .generate_xml(drawio_agent_llm_client::GenerateRequest {
            user_prompt: "anything".into(),
            ..Default::default()
        })
        .await
        .expect_err("unconfigured provider must error");
    assert!(
        err.to_string().contains("未配置"),
        "error must point at the settings UI: {err}"
    );
}

#[tokio::test]
async fn effective_settings_never_defaults_to_mock() {
    // Regression: with nothing configured the resolver must report
    // Unconfigured — silently running on the stub made users think their
    // prompts were being processed by a real model.
    let settings = drawio_agent_server::effective_settings(None);
    // NOTE: this test runs in an env that MAY carry DRAWIO_AGENT_LLM_*;
    // only assert the no-fallback invariant when no env is present.
    if std::env::var("DRAWIO_AGENT_LLM_PROVIDER").is_err() {
        assert_eq!(settings.kind, drawio_agent_server::LlmKind::Unconfigured);
    } else {
        assert_ne!(settings.kind, drawio_agent_server::LlmKind::Mock);
    }
}
