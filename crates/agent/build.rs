//! Emit the git short hash for build identification (debugging aid:
//! the web UI shows it, so a stale-binary report is instantly visible).
//! Also generates tool_specs.txt from TOOL_META (single source of truth).

use std::fs;
use std::path::Path;

fn main() {
    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_HASH={hash}");
    println!("cargo:rerun-if-changed=.git/HEAD");

    // Generate tool_specs.txt from tools_meta.rs
    generate_tool_specs();
    println!("cargo:rerun-if-changed=src/tools_meta.rs");
}

fn generate_tool_specs() {
    let meta_path = Path::new("src/tools_meta.rs");
    let content = fs::read_to_string(meta_path).expect("read tools_meta.rs");

    // Parse TOOL_META array entries - they span multiple lines
    // Each entry: ( "name", r#"multi-line desc"# ),
    // We track paren depth across lines to find complete entries
    let mut specs = String::new();
    let mut i = 0;
    let mut in_meta = false;
    let mut paren_depth = 0;
    let mut current_entry = String::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("pub const TOOL_META:") {
            in_meta = true;
            continue;
        }
        if in_meta {
            if trimmed.starts_with(']') || trimmed.starts_with("};") {
                break;
            }
            // Track parentheses
            for ch in line.chars() {
                if ch == '(' {
                    paren_depth += 1;
                } else if ch == ')' {
                    paren_depth -= 1;
                }
            }
            current_entry.push_str(line);
            current_entry.push('\n');

            // When we close a top-level paren, we have a complete entry
            if paren_depth == 0 && !current_entry.trim().is_empty() && current_entry.contains('"') {
                if let Some(name) = extract_name(&current_entry) {
                    if let Some(desc) = extract_desc(&current_entry) {
                        i += 1;
                        specs.push_str(&format!("{}. {}   {}\n\n", i, name, desc));
                    }
                }
                current_entry.clear();
            }
        }
    }

    // Add the closing protocol line
    specs.push_str(r#"任务完成用输出协议的结束信封 {"reply": "<给用户的总结>", "done": true}
（reply 会直接展示给用户）。"#);

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let out_path = Path::new(&out_dir).join("tool_specs.txt");
    fs::write(&out_path, specs).expect("write tool_specs.txt");
}

fn extract_name(entry: &str) -> Option<String> {
    let mut found_first = false;
    let mut name_start = 0;
    for (idx, ch) in entry.char_indices() {
        if ch == '"' {
            if !found_first {
                found_first = true;
                name_start = idx + 1;
            } else {
                return Some(entry[name_start..idx].to_string());
            }
        }
    }
    None
}

fn extract_desc(entry: &str) -> Option<String> {
    if let Some(start) = entry.find("r#\"") {
        let content = &entry[start + 3..];
        if let Some(end) = content.find("\"#") {
            return Some(content[..end].to_string());
        }
    }
    None
}
