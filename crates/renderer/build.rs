//! Build-time download of a pinned `chrome-headless-shell` binary.
//!
//! Downloads from `storage.googleapis.com/chrome-for-testing-public`,
//! caches under `$XDG_CACHE_HOME/drawio-agent/chrome-headless-shell/`, and
//! emits `bundled_chromium.rs` into `OUT_DIR` with the resolved path.
//!
//! `DRAWIO_AGENT_OFFLINE=1` skips the download entirely (emits `None`).
//!
//! SHA-256 verification is enforced via `src/checksum.rs::CHECKSUMS`
//! (pulled in with `#[path]`); a mismatch hard-fails the build with both
//! hashes printed. Bump `PINNED_VERSION` and the `CHECKSUMS` table together.

use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::Digest;

/// Maximum total time we'll spend on a single download.
const DOWNLOAD_OVERALL_TIMEOUT: Duration = Duration::from_secs(1800); // 30 min
/// Log progress every N bytes or every N seconds, whichever comes first.
const PROGRESS_LOG_BYTES: u64 = 10 * 1024 * 1024; // 10 MB
const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// Stream-copy from `reader` to `writer` in 64KB chunks, logging progress to
/// cargo's warning stream and enforcing an overall wall-clock timeout.
///
/// Replaces the naive `io::copy` (which would silently die on ureq's
/// per-read timeout during slow CDN responses — see Phase 13 wrap-up).
fn download_with_progress<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
) -> io::Result<()> {
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    let start = Instant::now();
    let mut last_log = Instant::now();
    let mut next_log_threshold: u64 = PROGRESS_LOG_BYTES;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        total += n as u64;
        let elapsed = start.elapsed();
        if elapsed > DOWNLOAD_OVERALL_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{label}: overall timeout after {total} bytes ({:.1}s elapsed)",
                    elapsed.as_secs_f64()
                ),
            ));
        }
        if total >= next_log_threshold || last_log.elapsed() > PROGRESS_LOG_INTERVAL {
            eprintln!(
                "cargo:warning={label}: {} MB downloaded in {:.1}s",
                total / (1024 * 1024),
                elapsed.as_secs_f64(),
            );
            last_log = Instant::now();
            next_log_threshold = total.saturating_add(PROGRESS_LOG_BYTES);
        }
    }
    Ok(())
}

#[path = "src/checksum.rs"]
mod checksum;

/// Pinned chrome-for-testing `chrome-headless-shell` version. Verified to
/// exist at the storage.googleapis.com CDN URL on 2026-09-05.
const PINNED_VERSION: &str = "131.0.6778.85";

/// (os, arch) → CDN platform slug.
fn platform_slug() -> Option<&'static str> {
    let os = env::var("CARGO_CFG_TARGET_OS").ok()?;
    let arch = env::var("CARGO_CFG_TARGET_ARCH").ok()?;
    match (os.as_str(), arch.as_str()) {
        ("macos", "x86_64") => Some("mac-x64"),
        ("macos", "aarch64") => Some("mac-arm64"),
        ("linux", "x86_64") => Some("linux64"),
        ("windows", "x86_64") => Some("win64"),
        _ => None,
    }
}

fn zip_url(version: &str, platform: &str) -> String {
    format!(
        "https://storage.googleapis.com/chrome-for-testing-public/{version}/{platform}/chrome-headless-shell-{platform}.zip"
    )
}

fn cache_root() -> PathBuf {
    if let Ok(p) = env::var("DRAWIO_AGENT_CACHE_DIR") {
        return PathBuf::from(p);
    }
    if let Ok(xdg) = env::var("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("drawio-agent");
    }
    if cfg!(target_os = "macos") {
        if let Ok(home) = env::var("HOME") {
            return Path::new(&home).join("Library/Caches/drawio-agent");
        }
    }
    if cfg!(target_os = "windows") {
        if let Ok(local) = env::var("LOCALAPPDATA") {
            return Path::new(&local).join("drawio-agent");
        }
    }
    if let Ok(home) = env::var("HOME") {
        return Path::new(&home).join(".cache/drawio-agent");
    }
    PathBuf::from(".drawio-agent-cache")
}

fn binary_filename() -> &'static str {
    if cfg!(target_os = "windows") {
        "chrome-headless-shell.exe"
    } else {
        "chrome-headless-shell"
    }
}

/// Resolve (or download) the bundled binary. Returns the binary path, or
/// `None` when offline / unsupported target.
fn ensure_bundled() -> Result<Option<PathBuf>, String> {
    // 1. OFFLINE → skip everything.
    if env::var("DRAWIO_AGENT_OFFLINE").is_ok() {
        eprintln!("cargo:warning=DRAWIO_AGENT_OFFLINE=1 → skipping chrome-headless-shell download");
        return Ok(None);
    }

    let platform = match platform_slug() {
        Some(p) => p,
        None => {
            eprintln!("cargo:warning=unsupported target; chrome-headless-shell not bundled");
            return Ok(None);
        }
    };
    let target_dir = cache_root()
        .join("chrome-headless-shell")
        .join(PINNED_VERSION)
        .join(platform);
    let sentinel = target_dir.join(".sha256-ok");
    let bin = target_dir
        .join(format!("chrome-headless-shell-{platform}"))
        .join(binary_filename());

    // 2. Cache hit.
    if bin.exists() && sentinel.exists() {
        eprintln!("cargo:warning=using cached chrome-headless-shell at {}", bin.display());
        return Ok(Some(bin));
    }

    // 3. Download.
    let url = zip_url(PINNED_VERSION, platform);
    eprintln!(
        "cargo:warning=downloading chrome-headless-shell {version} for {platform}",
        version = PINNED_VERSION,
        platform = platform
    );
    eprintln!("cargo:warning=URL: {url}");

    fs::create_dir_all(&target_dir).map_err(|e| format!("mkdir {target_dir:?}: {e}"))?;
    let tmp_dir = target_dir.with_extension("tmp");
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).map_err(|e| format!("mkdir tmp: {e}"))?;

    let zip_path = tmp_dir.join("headless-shell.zip");

    // HEAD probe first to fail fast with a clear message. Per-read timeout
    // is set very high so slow CDN responses don't trip ureq before our
    // own overall timeout fires (see `download_with_progress`).
    let client = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(3600))
        .timeout_read(Duration::from_secs(3600))
        .build();
    let head_resp = client.head(&url).call();
    if let Err(e) = head_resp {
        return Err(format!(
            "HEAD {url} failed: {e} — is the pinned version '{PINNED_VERSION}' correct?"
        ));
    }

    let resp = client
        .get(&url)
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?;
    let mut zip_file = File::create(&zip_path).map_err(|e| format!("create zip: {e}"))?;
    download_with_progress(&mut resp.into_reader(), &mut zip_file, "chrome-headless-shell")
        .map_err(|e| format!("download: {e}"))?;
    drop(zip_file);

    // 4. Extract. The zip's top-level folder is
    //    `chrome-headless-shell-{platform}/`, so extracting into `tmp_dir`
    //    directly yields `<tmp>/chrome-headless-shell-{platform}/...` which
    //    becomes the final `<cache>/<version>/<platform>/...` layout after
    //    the atomic rename below.
    let extract_root = tmp_dir.clone();
    fs::create_dir_all(&extract_root).map_err(|e| format!("mkdir extract: {e}"))?;
    let zip_file = File::open(&zip_path).map_err(|e| format!("open zip: {e}"))?;
    let mut archive = zip::ZipArchive::new(zip_file).map_err(|e| format!("read zip: {e}"))?;
    for i in 0..archive.len() {
        let mut f = archive.by_index(i).map_err(|e| format!("zip entry {i}: {e}"))?;
        let outpath = match f.enclosed_name() {
            Some(p) => extract_root.join(p),
            None => continue,
        };
        if f.is_dir() {
            fs::create_dir_all(&outpath).map_err(|e| format!("mkdir zip entry: {e}"))?;
        } else {
            if let Some(parent) = outpath.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("mkdir parent: {e}"))?;
            }
            let mut out = File::create(&outpath).map_err(|e| format!("create zip entry: {e}"))?;
            io::copy(&mut f, &mut out).map_err(|e| format!("write zip entry: {e}"))?;
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

    // 4b. Zip is ~90MB — don't keep a duplicate copy in the cache dir.
    let _ = fs::remove_file(&zip_path);

    // 5. Verify binary exists at the expected path.
    let extracted_bin = extract_root
        .join(format!("chrome-headless-shell-{platform}"))
        .join(binary_filename());
    if !extracted_bin.exists() {
        return Err(format!(
            "expected binary not found at {} after extract",
            extracted_bin.display()
        ));
    }

    // 6. Verify checksum (fails build on mismatch).
    let hash = compute_sha256(&extracted_bin)
        .ok_or_else(|| format!("compute SHA-256 of {}", extracted_bin.display()))?;
    if let Err(e) = checksum::verify_checksum(platform, &hash) {
        // Clean up the half-extracted tmp_dir before failing.
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err(e);
    }

    // 7. macOS Gatekeeper: strip the quarantine xattr.
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("xattr")
            .args(["-d", "-r", "com.apple.quarantine"])
            .arg(&extracted_bin)
            .output();
    }

    // 8. Atomic move into the final location.
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
    let final_bin = target_dir
        .join(format!("chrome-headless-shell-{platform}"))
        .join(binary_filename());

    // 9. Write sentinel after the successful move.
    fs::write(target_dir.join(".sha256-ok"), PINNED_VERSION).ok();

    Ok(Some(final_bin))
}

fn compute_sha256(p: &Path) -> Option<String> {
    let mut f = File::open(p).ok()?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

fn main() {
    // Re-run this script when any discovery-affecting env var changes, so
    // e.g. toggling DRAWIO_AGENT_OFFLINE on an incremental build actually
    // flips the emitted BUNDLED_CHROMIUM_PATH.
    println!("cargo:rerun-if-env-changed=DRAWIO_AGENT_OFFLINE");
    println!("cargo:rerun-if-env-changed=DRAWIO_AGENT_CACHE_DIR");
    println!("cargo:rerun-if-env-changed=DRAWIO_AGENT_CHROMIUM_PATH");
    println!("cargo:rerun-if-env-changed=XDG_CACHE_HOME");
    println!("cargo:rerun-if-env-changed=HOME");
    println!("cargo:rerun-if-changed=src/checksum.rs");

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let emitted = PathBuf::from(out_dir).join("bundled_chromium.rs");

    let path = match ensure_bundled() {
        Ok(p) => p,
        Err(e) => {
            // Hard-fail the build, but with a clear actionable message.
            panic!("drawio-agent build.rs failed: {e}");
        }
    };

    let version = PINNED_VERSION;
    let path_str = path.as_ref().map(|p| p.to_string_lossy().into_owned());

    let content = format!(
        "/// Auto-generated by build.rs. Do not edit.\n\
         /// Pinned chrome-for-testing chrome-headless-shell version.\n\
         pub const PINNED_CHROMIUM_VERSION: &str = {version:?};\n\
         /// Resolved path to the bundled binary, or None if unavailable\n\
         /// (unsupported target, DRAWIO_AGENT_OFFLINE=1, or download failed).\n\
         pub const BUNDLED_CHROMIUM_PATH: Option<&'static str> = {path_str:?};\n",
    );
    fs::write(&emitted, content).expect("write bundled_chromium.rs");
}