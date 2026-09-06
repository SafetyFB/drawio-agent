//! `@file`-style references: map mentions of ranges / cell ids / labels
//! into line-numbered context snippets, aider-style.
//!
//! The harness never hands the model a detached subgraph. Instead the
//! user's message (or a canvas selection) is turned into `@path:lines`
//! tokens that resolve — via the doc's span index — to *the exact lines of
//! the xml file* those cells live in. "改这一部分" becomes: here is the
//! part, identified by file position.

use crate::xmlfile::{XmlDoc, XmlError};

/// A resolved reference: where it pointed and the snippet shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRef {
    /// Canonical form, e.g. `@diagram.xml:120-156` or `@cell:svc-a`.
    pub canonical: String,
    pub start_line: usize,
    pub end_line: usize,
    pub snippet: String,
}

/// Scan `text` for reference tokens and resolve them against `doc`.
///
/// Token syntax (tokens end at whitespace / comma / CJK punctuation):
/// - `@cell:ID`            — a cell by id
/// - `@120` / `@120-156`   — raw line numbers
/// - `@diagram.xml:120-156`— @file style (any path prefix allowed)
///
/// Unknown tokens are returned as errors so the caller can tell the model
/// (or human) instead of silently dropping them.
pub fn resolve_refs(text: &str, doc: &XmlDoc) -> (Vec<ResolvedRef>, Vec<String>, String) {
    let mut resolved = Vec::new();
    let mut errors = Vec::new();
    let mut cleaned = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        cleaned.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        // Token = until whitespace, CJK punctuation, comma, or closing
        // bracket. Keep `:` and `-` and `.` and alphanumerics.
        let len = after
            .char_indices()
            .find(|(_, c)| {
                !(c.is_alphanumeric() || matches!(c, ':' | '-' | '.' | '_' | '/'))
            })
            .map(|(i, _)| i)
            .unwrap_or(after.len());
        let (tok, tail) = after.split_at(len);
        let spec = tok.trim_end_matches(['.', ',', ';', '，', '。']);
        let is_ref = spec.starts_with("cell:")
            || spec.contains(':')
            || spec.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
            || doc.id_to_cell(spec).is_some();
        if spec.is_empty() || !is_ref {
            // plain '@' in prose (prices, emails) — leave untouched
            cleaned.push('@');
            cleaned.push_str(tail);
            rest = "";
            continue;
        }
        match resolve_one(spec, doc) {
            Ok(r) => {
                cleaned.push_str(&r.canonical);
                resolved.push(r);
            }
            Err(e) => {
                cleaned.push_str(&format!("@({e})"));
                errors.push(format!("{spec}: {e}"));
            }
        }
        rest = tail;
    }
    cleaned.push_str(rest);

    let mut snippet = String::new();
    if !resolved.is_empty() {
        snippet.push_str("\n\n── 选中/引用区段（来自当前 xml 文件） ──\n");
        for (i, r) in resolved.iter().enumerate() {
            snippet.push_str(&format!(
                "[{}] {} (lines {}-{})\n",
                i + 1,
                r.canonical,
                r.start_line,
                r.end_line
            ));
            for (j, line) in r.snippet.lines().enumerate() {
                snippet.push_str(&format!(
                    "{:>5}| {}\n",
                    r.start_line + j,
                    line
                ));
            }
        }
    }
    (resolved, errors, snippet)
}

fn resolve_one(spec: &str, doc: &XmlDoc) -> Result<ResolvedRef, XmlError> {
    let (start, end) = doc.resolve_range(spec)?;
    let lines = crate::xmlfile::lines_in(&doc.canonical(), start, end);
    let canonical = if let Some(id) = spec.strip_prefix("cell:") {
        if doc.id_to_cell(id).is_some() {
            format!("@cell:{id}")
        } else {
            format!("@{}:{}", file_stem(doc), start_line_hint(start, end))
        }
    } else if spec.contains(":") {
        // keep the path the author wrote, but normalize the range part
        format!("@{}:{}", spec.split(':').next().unwrap_or("xml"), start_line_hint(start, end))
    } else {
        format!("@{}", start_line_hint(start, end))
    };
    Ok(ResolvedRef {
        canonical,
        start_line: start,
        end_line: end,
        snippet: lines,
    })
}

fn start_line_hint(start: usize, end: usize) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

fn file_stem(doc: &XmlDoc) -> String {
    doc.path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "diagram.xml".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xmlfile::XmlDoc;

    const SAMPLE: &str = r#"<mxfile host="app.diagrams.net"><diagram id="d1" name="Page-1"><mxGraphModel dx="700" dy="500"><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="svc-a" value="Order Service" vertex="1" parent="1"><mxGeometry x="40" y="60" width="160" height="60" as="geometry"/></mxCell><mxCell id="svc-b" value="Billing Service" vertex="1" parent="1"><mxGeometry x="240" y="60" width="160" height="60" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;

    fn doc() -> XmlDoc {
        XmlDoc::from_text(SAMPLE).unwrap()
    }

    #[test]
    fn cell_token_resolves_to_span() {
        let (refs, errs, _) = resolve_refs("把 @cell:svc-a 的颜色改掉", &doc());
        assert!(errs.is_empty());
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].canonical, "@cell:svc-a");
        assert!(refs[0].snippet.contains("Order Service"));
        assert!(refs[0].snippet.contains("mxGeometry"));
    }

    #[test]
    fn file_style_token_resolves() {
        let (refs, errs, _) = resolve_refs("看下 @diagram.xml:1-4 这一段", &doc());
        assert!(errs.is_empty());
        assert_eq!(refs[0].start_line, 1);
        assert_eq!(refs[0].end_line, 4);
    }

    #[test]
    fn snippet_carries_line_numbers() {
        let d = doc();
        let (refs, _, block) = resolve_refs("改 @cell:svc-b", &d);
        let b = d.id_to_cell("svc-b").unwrap();
        assert_eq!(refs[0].start_line, b.start_line);
        assert!(block.contains(&format!("{}|", b.start_line)));
        assert!(block.contains("Billing Service"));
    }

    #[test]
    fn unknown_token_reported_not_silent() {
        let (refs, errs, _) = resolve_refs("改 @cell:ghost 和 @cell:svc-a", &doc());
        assert_eq!(refs.len(), 1);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("ghost"));
    }

    #[test]
    fn refs_do_not_collide_with_emails_or_at_signs_in_text() {
        // '@' followed by a space or inside an email is not a reference:
        // no refs resolved, no errors, no context snippet injected.
        let (refs, errs, snippet) = resolve_refs("价格 @ 1 元, 邮箱 a@b.com", &doc());
        assert!(refs.is_empty());
        assert!(errs.is_empty());
        assert!(snippet.is_empty());
    }
}
