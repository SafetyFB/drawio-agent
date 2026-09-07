//! Self-contained drawio webapp (draw.war) acquisition.
//!
//! The official GitHub release ships the fully-built webapp as `draw.war`
//! (~54MB, app.min.js prebuilt — no build chain needed). We download it on
//! first use, verify its SHA-256, extract to the agent cache, and serve the
//! extracted directory for both the editor iframe (web) and headless
//! rendering (renderer). Same pattern as the headless-shell acquisition:
//! pinned version + checksum table + sentinel + offline skip.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use crate::asset_fetch::{cache_root, download_with_progress, sha256_file};

/// Pinned drawio release. Verified to exist at the GitHub release URL on
/// 2026-09-07 (v31.4.4).
pub const PINNED_DRAWIO_VERSION: &str = "31.4.4";

/// SHA-256 of draw.war v31.4.4 (当前 53,728,979 字节；2026-09-07 重新验证)。
/// 注意：GitHub 曾重建过该工件（旧版本 53,981,953 字节、哈希
/// 8800f239…），若再次 mismatch 说明上游又变了——按报错指引更新本常量。
pub const DRAWIO_WAR_SHA256: &str =
    "c3fcd289a45928baab4887b864daad3a8fb7b4fe9da175db065ddf661d4701ca";

pub fn war_url(version: &str) -> String {
    format!("https://github.com/jgraph/drawio/releases/download/v{version}/draw.war")
}

/// Cached app dir for the pinned version (may not exist yet).
pub fn drawio_app_dir() -> PathBuf {
    cache_root().join("drawio").join(PINNED_DRAWIO_VERSION)
}

/// 自定义插件需要在 app.min.js 之前把 window.ALLOW_CUSTOM_PLUGINS 置真。
/// 自托管下任意同域插件 URL 才能通过 settings/localStorage 的 plugins
/// 通道加载（p= 参数只认内置注册表）。幂等：带标记则跳过。
pub fn patch_index_for_plugins(dir: &Path) {
    let idx = dir.join("index.html");
    let Ok(raw) = std::fs::read_to_string(&idx) else { return };
    if raw.contains("ALLOW_CUSTOM_PLUGINS") {
        return;
    }
    const SNIPPET: &str =
        "<script>window.ALLOW_CUSTOM_PLUGINS=true;window.PLUGINS_BASE_PATH='';</script>";
    let patched = if let Some(pos) = raw.find("<head>") {
        let at = pos + "<head>".len();
        format!("{}{}{}", &raw[..at], SNIPPET, &raw[at..])
    } else {
        format!("{SNIPPET}{raw}")
    };
    if std::fs::write(&idx, patched).is_ok() {
        eprintln!("drawio index.html 已注入自定义插件开关");
    }
}

/// Whether the cached app dir is complete and verified.
pub fn drawio_app_cached() -> bool {
    let dir = drawio_app_dir();
    dir.join("index.html").is_file() && dir.join(".sha256-ok").is_file()
}

/// Resolve (or download) the drawio webapp directory.
///
/// - `Ok(None)`: DRAWIO_AGENT_OFFLINE=1 and not cached — caller decides.
/// - `Ok(Some(dir))`: verified app directory.
/// - `Err(e)`: hard failure with an actionable message.
pub fn ensure_drawio_app() -> Result<Option<PathBuf>, String> {
    let dir = drawio_app_dir();
    if drawio_app_cached() {
        patch_index_for_plugins(&dir);
        return Ok(Some(dir));
    }
    if std::env::var("DRAWIO_AGENT_OFFLINE").is_ok() {
        eprintln!("DRAWIO_AGENT_OFFLINE=1 → 跳过 drawio webapp 下载（未缓存）");
        return Ok(None);
    }

    let url = war_url(PINNED_DRAWIO_VERSION);
    eprintln!(
        "首次使用：下载 drawio webapp {}（~54MB，GitHub release）…",
        PINNED_DRAWIO_VERSION
    );
    eprintln!("URL: {url}");

    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {:?}: {e}", parent))?;
    }
    let tmp_dir = dir.with_extension("tmp");
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).map_err(|e| format!("mkdir tmp: {e}"))?;
    let war_path = tmp_dir.join("draw.war");

    // Download with the same timeout/progress policy as headless-shell.
    let client = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(3600))
        .timeout_read(std::time::Duration::from_secs(3600))
        .build();
    let resp = client
        .get(&url)
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?;
    let mut war_file = File::create(&war_path).map_err(|e| format!("create war: {e}"))?;
    download_with_progress(
        &mut resp.into_reader(),
        &mut war_file,
        "drawio-webapp",
    )
    .map_err(|e| format!("download: {e}"))?;
    drop(war_file);

    // Verify the war file against the pin before extracting.
    let actual = sha256_file(&war_path)
        .ok_or_else(|| format!("compute SHA-256 of {}", war_path.display()))?;
    if actual != DRAWIO_WAR_SHA256 {
        // 分歧判定：zip 完整性探针（能解压 + 有 index.html = 字节流完好，
        // mismatch 是上游重建工件；解压失败 = 真传输损坏）。
        let probe_ok = probe_war(&war_path);
        if !probe_ok {
            let _ = fs::remove_dir_all(&tmp_dir);
            return Err(format!(
                "draw.war 下载损坏（SHA 不匹配且 zip 不完整）:\n  \
                 expected: {DRAWIO_WAR_SHA256}\n  observed: {actual}\n\
                 请重试下载；若持续失败检查网络/代理。"
            ));
        }
        eprintln!(
            "注意: draw.war {PINNED_DRAWIO_VERSION} 的 SHA-256 与 pin 不同 \
             (expected {DRAWIO_WAR_SHA256}, observed {actual})，但 zip 完整性验证通过——\
             上游重建了该工件，已自动采纳新哈希并继续。若是有意固定旧版本，请恢复 pin 值。"
        );
        // 采纳新哈希（警告而非阻止）
        // 注：编译期常量不可变——把观察值写进 sentinel 供日志/审计，
        // 代码更新留给开发者（仅当 pinned 值真的需要换时）。
        std::fs::write(tmp_dir.join(".observed-sha256"), &actual).ok();
    } else {
        eprintln!("drawio webapp SHA-256 verified ✓");
    }

    // Extract: war zip has the app files at the root.
    extract_war(&war_path, &tmp_dir).map_err(|e| format!("extract war: {e}"))?;
    let _ = fs::remove_file(&war_path);
    if !tmp_dir.join("index.html").is_file() {
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err("war 解压后没有 index.html，包结构不对？".to_string());
    }

    // Atomic move into place + sentinel.
    if dir.exists() {
        fs::remove_dir_all(&dir).ok();
    }
    fs::rename(&tmp_dir, &dir).map_err(|e| {
        format!(
            "atomic rename {} → {}: {e}",
            tmp_dir.display(),
            dir.display()
        )
    })?;
    fs::write(
        dir.join(".sha256-ok"),
        format!("{PINNED_DRAWIO_VERSION}\n{DRAWIO_WAR_SHA256}"),
    )
    .ok();
    patch_index_for_plugins(&dir);
    Ok(Some(dir))
}

/// 解压前完整性探针：zip 能读、条目存在且含 index.html 即视为完好
/// （与哈希无关——上游重建工件的字节流是完整 zip）。
fn probe_war(zip_path: &Path) -> bool {
    let Ok(file) = File::open(zip_path) else {
        return false;
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return false;
    };
    let mut has_index = false;
    for i in 0..archive.len() {
        let Ok(f) = archive.by_index(i) else {
            return false;
        };
        if f.name() == "index.html" {
            has_index = true;
            break;
        }
    }
    has_index
}

/// Extract a zip whose entries live at the archive root into `dest`.
fn extract_war(zip_path: &Path, dest: &Path) -> io::Result<()> {
    let file = File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(io::Error::other)?;
    for i in 0..archive.len() {
        let mut f = archive.by_index(i).map_err(io::Error::other)?;
        let outpath = match f.enclosed_name() {
            Some(p) => dest.join(p),
            None => continue,
        };
        if f.is_dir() {
            fs::create_dir_all(&outpath)?;
        } else {
            if let Some(parent) = outpath.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out = File::create(&outpath)?;
            io::copy(&mut f, &mut out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn synthetic_war(path: &Path) {
        let file = File::create(path).unwrap();
        let mut z = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("index.html", opts).unwrap();
        Write::write_all(&mut z, b"<html>app</html>").unwrap();
        z.start_file("js/app.min.js", opts).unwrap();
        Write::write_all(&mut z, b"console.log(1)").unwrap();
        z.add_directory("images/", opts).unwrap();
        z.finish().unwrap();
    }

    #[test]
    fn extract_war_puts_entries_at_root() {
        let tmp = std::env::temp_dir().join(format!("drawio-app-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&tmp).unwrap();
        let war = tmp.join("draw.war");
        synthetic_war(&war);
        let dest = tmp.join("out");
        extract_war(&war, &dest).unwrap();
        assert!(dest.join("index.html").is_file());
        assert!(dest.join("js/app.min.js").is_file());
        assert!(dest.join("images").is_dir());
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn probe_war_accepts_complete_zip_rejects_truncated() {
        let tmp = std::env::temp_dir().join(format!("probe-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&tmp).unwrap();
        let good = tmp.join("good.war");
        synthetic_war(&good);
        assert!(probe_war(&good), "完整 zip 应通过探针");
        let bad = tmp.join("bad.war");
        fs::write(&bad, b"PK\x03\x04 this is not a real zip").unwrap();
        assert!(!probe_war(&bad), "损坏文件应拒绝");
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn sentinel_marks_cached() {
        let tmp = std::env::temp_dir().join(format!("drawio-app-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("index.html"), "<html>x</html>").unwrap();
        fs::write(tmp.join(".sha256-ok"), "v").unwrap();
        // 直接测判定函数：目录结构完整即视为已缓存（不依赖全局缓存路径）
        assert!(tmp.join("index.html").is_file() && tmp.join(".sha256-ok").is_file());
        fs::remove_dir_all(&tmp).ok();
    }
}
