//! LLM configuration: `~/.drawio-agent/config.json` (or
//! `DRAWIO_AGENT_CONFIG_FILE`) + env-var fallback, runtime hot-swap for the
//! web settings panel. Same file shape as the pre-refactor app so an
//! existing `~/.drawio-agent/config.json` keeps working.
//!
//! Precedence (mirrors the old server): a saved config file wins; env vars
//! (`DRAWIO_LLM_BASE_URL` / `DRAWIO_LLM_MODEL` / `DRAWIO_LLM_API_KEY`) are
//! the default only when no config file exists yet. Nothing found = clean
//! "unconfigured" state that fails fast with guidance — never a silent stub.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad config json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Thinking mode for LLM calls. `Default` leaves the model's own setting
/// alone; `NoThink` requests `thinking: {"type": "disabled"}` (GLM 4.6+).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingMode {
    #[default]
    Default,
    NoThink,
}

/// The user-facing LLM settings (what the UI edits and what persists).
///
/// Old config files (base_url/api_key/model only) keep loading: every new
/// field has a serde default.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LlmSettings {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub model: String,
    /// Context window in tokens. Used to guard prompt size before a call.
    #[serde(default)]
    pub context_length: Option<u64>,
    /// Fast path: disable provider-internal reasoning.
    #[serde(default)]
    pub thinking: ThinkingMode,
    /// Price in ¥ per 1M input tokens (0 = 未计价，只统计 token)。
    #[serde(default)]
    pub price_input_per_m: f64,
    /// Price in ¥ per 1M output tokens.
    #[serde(default)]
    pub price_output_per_m: f64,
    /// Optional session spend cap in ¥; the agent stops when reached.
    #[serde(default)]
    pub budget_yuan: Option<f64>,
}

/// Well-known model prices (¥/1M tokens, 2025 官方公开价，仅作填表便利;
/// 实际以账单为准，可手动修改). Preset only when the user asks.
pub fn preset_prices(model: &str) -> Option<(f64, f64)> {
    let m = model.to_lowercase();
    let p = if m.contains("glm-4.6") || m.contains("glm-4.5") {
        (5.0, 15.0)
    } else if m.contains("glm-4-flash") {
        (0.0, 0.0)
    } else if m.contains("glm-4") || m.contains("glm-4v") {
        (0.1, 0.1)
    } else if m.contains("deepseek-chat") {
        (2.0, 8.0)
    } else if m.contains("deepseek-reasoner") {
        (4.0, 16.0)
    } else if m.contains("gpt-4o") {
        (17.0, 68.0)
    } else if m.contains("gpt-4o-mini") {
        (1.1, 4.4)
    } else if m.contains("qwen") && m.contains("vl") {
        (2.0, 6.0)
    } else {
        return None;
    };
    Some(p)
}

/// ¥ cost of a usage at the given settings (0 price = 0 cost, token 照常统计).
pub fn usage_cost(input_tokens: u64, output_tokens: u64, s: &LlmSettings) -> f64 {
    input_tokens as f64 / 1e6 * s.price_input_per_m
        + output_tokens as f64 / 1e6 * s.price_output_per_m
}

/// On-disk shape: `{ "llm": { "kind": "openai-compat", ... } }`. The old
/// `kind` field is tolerated (unknown fields are ignored by serde).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub llm: LlmSettings,
}

impl LlmSettings {
    /// Masked key for display only; never leaks the real secret.
    pub fn api_key_masked(&self) -> String {
        mask_secret(&self.api_key)
    }

    pub fn configured(&self) -> bool {
        !self.base_url.is_empty() && !self.model.is_empty()
    }
}

pub fn mask_secret(secret: &str) -> String {
    if secret.is_empty() {
        return String::new();
    }
    if secret.len() <= 8 {
        return "***".to_string();
    }
    format!("{}…{}", &secret[..4], &secret[secret.len() - 4..])
}

/// Resolve the config file path: `DRAWIO_AGENT_CONFIG_FILE`, else
/// `~/.drawio-agent/config.json`.
pub fn config_file_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DRAWIO_AGENT_CONFIG_FILE") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".drawio-agent").join("config.json"))
}

pub fn load_config_file(path: &Path) -> Option<LlmSettings> {
    let raw = std::fs::read_to_string(path).ok()?;
    let parsed: ConfigFile = serde_json::from_str(&raw).ok()?;
    Some(parsed.llm)
}

pub fn save_config_file(path: &Path, llm: &LlmSettings) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| "config path has no parent".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create config dir: {e}"))?;
    let json = serde_json::to_string_pretty(&ConfigFile { llm: llm.clone() })
        .map_err(|e| format!("serialize config: {e}"))?;
    std::fs::write(path, json).map_err(|e| format!("write config file: {e}"))
}

/// Effective settings at a point in time: config file first, env fallback.
pub fn effective_settings() -> Option<LlmSettings> {
    let from_file = config_file_path().and_then(|p| load_config_file(&p));
    if let Some(s) = from_file {
        if s.configured() {
            return Some(s);
        }
    }
    let base_url = std::env::var("DRAWIO_LLM_BASE_URL").ok();
    let model = std::env::var("DRAWIO_LLM_MODEL").ok();
    match (base_url, model) {
        (Some(base_url), Some(model)) if !base_url.is_empty() && !model.is_empty() => {
            Some(LlmSettings {
                base_url,
                model,
                api_key: std::env::var("DRAWIO_LLM_API_KEY").unwrap_or_default(),
                ..Default::default()
            })
        }
        _ => None,
    }
}

/// Where the effective config came from (for the settings panel display).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    None,
    File,
    Env,
}

pub fn effective_source() -> ConfigSource {
    if let Some(p) = config_file_path() {
        if let Some(s) = load_config_file(&p) {
            if s.configured() {
                return ConfigSource::File;
            }
        }
    }
    let base = std::env::var("DRAWIO_LLM_BASE_URL").unwrap_or_default();
    let model = std::env::var("DRAWIO_LLM_MODEL").unwrap_or_default();
    if !base.is_empty() && !model.is_empty() {
        ConfigSource::Env
    } else {
        ConfigSource::None
    }
}

/// Guidance shown whenever an LLM call is attempted while unconfigured.
pub const UNCONFIGURED_MSG: &str = "LLM 未配置：在网页右上角 ⚙ 里填写并保存，或运行 \
    `drawio-harness config set --base-url … --model …`，或用环境变量 \
    DRAWIO_LLM_BASE_URL / DRAWIO_LLM_MODEL / DRAWIO_LLM_API_KEY";

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpfile(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "drawio-harness-cfg-{tag}-{}.json",
            std::process::id()
        ))
    }

    #[test]
    fn save_load_roundtrip_and_mask() {
        let p = tmpfile("roundtrip");
        let _ = std::fs::remove_file(&p);
        let s = LlmSettings {
            base_url: "https://x.example/v1".into(),
            model: "m-1".into(),
            api_key: "abcdefgh-12345678".into(),
            ..Default::default()
        };
        save_config_file(&p, &s).unwrap();
        let loaded = load_config_file(&p).unwrap();
        assert_eq!(loaded, s);
        assert_eq!(s.api_key_masked(), "abcd…5678");
        assert_eq!(mask_secret("short"), "***");
        assert_eq!(mask_secret(""), "");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn old_shape_with_kind_field_still_loads() {
        let p = tmpfile("oldshape");
        let _ = std::fs::remove_file(&p);
        std::fs::write(
            &p,
            r#"{"llm":{"kind":"openai-compat","base_url":"https://b/v1","api_key":"k","model":"glm-x"}}"#,
        )
        .unwrap();
        let s = load_config_file(&p).unwrap();
        assert_eq!(s.model, "glm-x");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn env_fallback_and_file_wins() {
        // (env access is global — keep this sequential inside one test)
        std::env::set_var("DRAWIO_LLM_BASE_URL", "https://env/v1");
        std::env::set_var("DRAWIO_LLM_MODEL", "env-model");
        std::env::set_var("DRAWIO_LLM_API_KEY", "");
        // no file (override path to a nonexistent one)
        std::env::set_var("DRAWIO_AGENT_CONFIG_FILE", tmpfile("nonexistent"));
        let e = effective_settings().expect("env fallback should configure");
        assert_eq!(e.base_url, "https://env/v1");
        assert_eq!(effective_source(), ConfigSource::Env);

        // file wins over env
        let p = tmpfile("filewins");
        std::env::set_var("DRAWIO_AGENT_CONFIG_FILE", &p);
        save_config_file(
            &p,
            &LlmSettings {
                base_url: "https://file/v1".into(),
                model: "file-model".into(),
                api_key: "".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let f = effective_settings().expect("file should win");
        assert_eq!(f.base_url, "https://file/v1");
        assert_eq!(effective_source(), ConfigSource::File);

        // cleanup
        let _ = std::fs::remove_file(&p);
        std::env::remove_var("DRAWIO_AGENT_CONFIG_FILE");
        std::env::remove_var("DRAWIO_LLM_BASE_URL");
        std::env::remove_var("DRAWIO_LLM_MODEL");
    }
}
