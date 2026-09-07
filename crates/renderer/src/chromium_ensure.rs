//! Runtime acquisition of the pinned `chrome-headless-shell` binary.
//!
//! The build no longer downloads anything: `build.rs` only emits the cache
//! path. The binary is fetched on first actual use (view/export), unless a
//! system Chrome/Chromium/Edge is already available — in that case the
//! bundled binary is never downloaded at all.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use crate::asset_fetch::{cache_root, download_with_progress, sha256_file};
use crate::checksum;

/// Pinned chrome-for-testing `chrome-headless-shell` version. Verified to
/// exist at the storage.googleapis.com CDN URL on 2026-09-05.
pub const PINNED_CHROMIUM_VERSION: &str = "131.0.6778.85";

/// (os, arch) → CDN platform slug (runtime variant).
fn platform_slug() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "x86_64") => Some("mac-x64"),
        ("macos", "aarch64") => Some("mac-arm64"),
        ("linux", "x86_64") => Some("linux64"),
        ("windows", "x86_64") => Some("win64"),
        _ => None,
    }
}

fn binary_filename() -> &'static str {
    if cfg!(windows) {
        "chrome-headless-shell.exe"
    } else {
        "chrome-headless-shell"
    }
}

fn zip_url(version: &str, platform: &str) -> String {
    format!(
        "https://storage.googleapis.com/chrome-for-testing-public/{version}/{platform}/chrome-headless-shell-{platform}.zip"
    )
}

/// Cache path of the bundled binary (may not exist yet). Mirrors the path
/// build.rs emits into `BUNDLED_CHROMIUM_PATH`.
pub fn bundled_bin() -> Option<PathBuf> {
    let platform = platform_slug()?;
    Some(
        cache_root()
            .join("chrome-headless-shell")
            .join(PINNED_CHROMIUM_VERSION)
            .join(platform)
            .join(format!("chrome-headless-shell-{platform}"))
            .join(binary_filename()),
    )
}

fn bundled_cached() -> bool {
    match bundled_bin() {
        Some(bin) => {
            bin.is_file()
                && bin
                    .parent()
                    .and_then(|p| p.parent())
                    .map(|d| d.join(".sha256-ok").is_file())
                    .unwrap_or(false)
        }
        None => false,
    }
}

/// Download + verify + extract the pinned headless-shell into the cache.
/// Returns the binary path.
fn download_chromium() -> Result<PathBuf, String> {
    let platform = platform_slug()
        .ok_or_else(|| "unsupported target for chrome-headless-shell".to_string())?;
    let bin = bundled_bin().ok_or_else(|| "unsupported target".to_string())?;
    let target_dir = bin
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| "bad cache layout".to_string())?
        .to_path_buf();
    let url = zip_url(PINNED_CHROMIUM_VERSION, platform);
    eprintln!(
        "首次渲染：下载 chrome-headless-shell {PINNED_CHROMIUM_VERSION}（~90MB）…"
    );
    eprintln!("URL: {url}");

    fs::create_dir_all(&target_dir).map_err(|e| format!("mkdir {target_dir:?}: {e}"))?;
    let tmp_dir = target_dir.with_extension("tmp");
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).map_err(|e| format!("mkdir tmp: {e}"))?;
    let zip_path = tmp_dir.join("headless-shell.zip");

    let client = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(3600))
        .timeout_read(std::time::Duration::from_secs(3600))
        .build();
    let resp = client
        .get(&url)
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?;
    let mut zip_file = File::create(&zip_path).map_err(|e| format!("create zip: {e}"))?;
    download_with_progress(
        &mut resp.into_reader(),
        &mut zip_file,
        "chrome-headless-shell",
    )
    .map_err(|e| format!("download: {e}"))?;
    drop(zip_file);

    // Extract (zip top folder becomes the platform dir inside the version dir).
    let zip_file = File::open(&zip_path).map_err(|e| format!("open zip: {e}"))?;
    let mut archive = zip::ZipArchive::new(zip_file).map_err(|e| format!("read zip: {e}"))?;
    for i in 0..archive.len() {
        let mut f = archive.by_index(i).map_err(|e| format!("zip entry {i}: {e}"))?;
        let outpath = match f.enclosed_name() {
            Some(p) => tmp_dir.join(p),
            None => continue,
        };
        if f.is_dir() {
            fs::create_dir_all(&outpath).map_err(|e| format!("mkdir zip entry: {e}"))?;
        } else {
            if let Some(parent) = outpath.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("mkdir parent: {e}"))?;
            }
            let mut out = File::create(&outpath).map_err(|e| format!("create zip entry: {e}"))?;
            std::io::copy(&mut f, &mut out).map_err(|e| format!("write zip entry: {e}"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Some(mode) = f.unix_mode() {
                    fs::set_permissions(&outpath, fs::Permissions::from_mode(mode)).ok();
                }
            }
        }
    }
    drop(archive);
    let _ = fs::remove_file(&zip_path);

    let extracted_bin = tmp_dir
        .join(format!("chrome-headless-shell-{platform}"))
        .join(binary_filename());
    if !extracted_bin.exists() {
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err(format!(
            "expected binary not found at {} after extract",
            extracted_bin.display()
        ));
    }

    let hash = sha256_file(&extracted_bin)
        .ok_or_else(|| format!("compute SHA-256 of {}", extracted_bin.display()))?;
    if let Err(e) = checksum::verify_checksum(platform, &hash) {
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err(e);
    }

    // macOS Gatekeeper: strip the quarantine xattr.
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("xattr")
            .args(["-d", "-r", "com.apple.quarantine"])
            .arg(&extracted_bin)
            .output();
    }

    if target_dir.exists() {
        fs::remove_dir_all(&target_dir).ok();
    }
    fs::rename(&tmp_dir, &target_dir).map_err(|e| {
        format!(
            "atomic rename {} → {}: {e}",
            tmp_dir.display(),
            target_dir.display()
        )
    })?;
    fs::write(target_dir.join(".sha256-ok"), PINNED_CHROMIUM_VERSION).ok();
    Ok(bin)
}

/// Well-known system browser locations (any real Chrome-family binary works
/// as a CDP host with --headless).
pub fn system_browser() -> Option<PathBuf> {
    let candidates: &[&str] = &[
        // macOS
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        // Linux
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/microsoft-edge",
        "/usr/bin/brave-browser",
    ];
    for c in candidates {
        let p = Path::new(c);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    #[cfg(windows)]
    {
        let probes = [
            ("PROGRAMFILES", "Google/Chrome/Application/chrome.exe"),
            ("PROGRAMFILES(X86)", "Google/Chrome/Application/chrome.exe"),
            ("LOCALAPPDATA", "Google/Chrome/Application/chrome.exe"),
            ("PROGRAMFILES", "Microsoft/Edge/Application/msedge.exe"),
        ];
        for (var, rel) in probes {
            if let Ok(base) = std::env::var(var) {
                let p = Path::new(&base).join(rel);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    // PATH lookup for chrome-family binaries.
    for name in ["google-chrome", "chromium", "chromium-browser", "chrome"] {
        if let Some(p) = which(name) {
            return Some(p);
        }
    }
    None
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

/// Resolve a usable CDP browser binary:
///
/// 1. `DRAWIO_AGENT_CHROMIUM_PATH` (explicit override)
/// 2. bundled chrome-headless-shell, if already cached (deterministic pin)
/// 3. system Chrome/Chromium/Edge (no download needed)
/// 4. bundled chrome-headless-shell — download on first use
///
/// `Ok(None)` = offline and nothing available.
pub fn resolve_chromium() -> Result<Option<PathBuf>, String> {
    // 1. Explicit override.
    if let Ok(p) = std::env::var("DRAWIO_AGENT_CHROMIUM_PATH") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Ok(Some(pb));
        }
    }
    // 2. Cached bundled binary (pinned, deterministic).
    if bundled_cached() {
        return Ok(bundled_bin());
    }
    // 3. System browser — avoids the ~90MB download entirely.
    if let Some(p) = system_browser() {
        eprintln!("使用系统浏览器渲染: {}", p.display());
        return Ok(Some(p));
    }
    // 4. Download on first use.
    if std::env::var("DRAWIO_AGENT_OFFLINE").is_ok() {
        eprintln!("DRAWIO_AGENT_OFFLINE=1 → 跳过 chrome-headless-shell 下载（未缓存且无系统浏览器）");
        return Ok(None);
    }
    download_chromium().map(Some)
}

/// Ensure the bundled binary exists (download if needed); returns its path
/// or None when offline/unavailable. Used when callers explicitly want the
/// pinned binary rather than a system browser.
pub fn ensure_chromium() -> Result<Option<PathBuf>, String> {
    if bundled_cached() {
        return Ok(bundled_bin());
    }
    if std::env::var("DRAWIO_AGENT_OFFLINE").is_ok() {
        return Ok(None);
    }
    download_chromium().map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_slug_matches_host() {
        // Runtime slug must match the build-time emitted path layout for
        // the host platform (mac-arm64 in CI/dev; at least it must be Some
        // on supported targets).
        if cfg!(any(
            all(target_os = "macos", target_arch = "aarch64"),
            all(target_os = "macos", target_arch = "x86_64"),
            all(target_os = "linux", target_arch = "x86_64"),
            all(target_os = "windows", target_arch = "x86_64")
        )) {
            assert!(platform_slug().is_some());
        }
    }

    #[test]
    fn system_browser_checks_well_known_paths() {
        // 无系统 Chrome 的机器返回 None 是合法结果；不 panic 即可
        let _ = system_browser();
    }
}
