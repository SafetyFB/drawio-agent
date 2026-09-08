//! Tests for chromium discovery ordering.

use drawio_agent_renderer::{find_chromium, PINNED_CHROMIUM_VERSION};

#[test]
fn pinned_version_is_exported_and_nonempty() {
    assert!(!PINNED_CHROMIUM_VERSION.is_empty());
    // Looks like "123.0.4567.89"
    assert!(
        PINNED_CHROMIUM_VERSION.split('.').count() == 4
            || PINNED_CHROMIUM_VERSION == "unknown"
    );
}

#[test]
fn env_var_overrides_bundled() {
    // Use a temp file as a sentinel; if find_chromium honors the env var,
    // it must return that exact path.
    let tmp = std::env::temp_dir().join(format!("drawio-agent-test-{}", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, b"fake").unwrap();
    std::env::set_var("DRAWIO_AGENT_CHROMIUM_PATH", &tmp);
    let found = find_chromium();
    std::env::remove_var("DRAWIO_AGENT_CHROMIUM_PATH");
    assert_eq!(found, Some(tmp), "env must win over bundled/system");
}

#[test]
fn find_chromium_returns_some_on_this_host() {
    // We just built the crate on this host; bundled should exist (unless
    // offline). If offline (CI without cache), this is allowed to be None.
    let found = find_chromium();
    if std::env::var("DRAWIO_AGENT_OFFLINE").is_ok() {
        // offline + no env → may be None; that's fine
        let _ = found;
    } else {
        assert!(found.is_some(), "expected bundled or system chrome on dev host");
    }
}