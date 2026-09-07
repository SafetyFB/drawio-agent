//! 共享缓存根目录解析（build.rs 经 #[path] 引入，lib 直接引用）。

use std::path::{Path, PathBuf};

/// Shared agent cache root: DRAWIO_AGENT_CACHE_DIR > XDG_CACHE_HOME >
/// macOS ~/Library/Caches > windows LOCALAPPDATA > ~/.cache > cwd fallback.
pub fn cache_root() -> PathBuf {
    if let Ok(p) = std::env::var("DRAWIO_AGENT_CACHE_DIR") {
        return PathBuf::from(p);
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("drawio-agent");
    }
    if cfg!(target_os = "macos") {
        if let Ok(home) = std::env::var("HOME") {
            return Path::new(&home).join("Library/Caches/drawio-agent");
        }
    }
    if cfg!(target_os = "windows") {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            return Path::new(&local).join("drawio-agent");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return Path::new(&home).join(".cache/drawio-agent");
    }
    PathBuf::from(".drawio-agent-cache")
}
