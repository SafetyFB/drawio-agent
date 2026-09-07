//! Tools the model (and the human REPL) can invoke. Every mutation goes
//! through `XmlDoc::apply_edit` — line-range textual replacement + full
//! validation — so locality (untouched bytes stay untouched) is a property
//! of the mechanics, not a promise.

use std::sync::Arc;

use serde_json::Value;

use crate::xmlfile::{check_doc, lines_in, total_lines, CheckReport, EditReport, XmlDoc};

/// Result of executing one tool: free-form text fed back to the model,
/// optionally carrying an image (the `view` tool returns the screenshot so
/// the engine can send it to a vision-capable model as an image part).
#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    pub text: String,
    pub image_png: Option<Vec<u8>>,
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        Self { text: s.into(), image_png: None }
    }
    pub fn with_image(text: impl Into<String>, png: Vec<u8>) -> Self {
        Self { text: text.into(), image_png: Some(png) }
    }
    pub fn plain(&self) -> String {
        if self.image_png.is_some() {
            format!("{}（附渲染截图）", self.text)
        } else {
            self.text.clone()
        }
    }
}

/// Renderer backend shared across `view` calls (constructed lazily per call
/// for now — chromium launch is ~1s, fine for interactive use).
#[derive(Clone)]
pub struct Tools {
    pub render: bool,
    pub renderer: Option<Arc<drawio_agent_renderer::Renderer>>,
}

impl std::fmt::Debug for Tools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tools")
            .field("render", &self.render)
            .field("renderer", &self.renderer.is_some())
            .finish()
    }
}

/// Resolve a range argument that may be a number, `a-b`, `cell:id`, or a
/// full `@file:lines` token, against the doc.
fn resolve_arg(doc: &XmlDoc, spec: &str) -> Result<(usize, usize), String> {
    let spec = spec.trim().trim_start_matches('@');
    doc.resolve_range(spec)
        .map_err(|e| format!("无法解析范围 `{spec}`: {e}"))
}

fn numbered(text: &str, start: usize) -> String {
    let mut out = String::new();
    for (i, l) in text.lines().enumerate() {
        out.push_str(&format!("{:>5}| {}\n", start + i, l));
    }
    out
}

impl Tools {
    pub fn new(render: bool) -> Self {
        Self { render, renderer: None }
    }

    /// Test seam: inject a canned renderer (e.g. built on
    /// `drawio_agent_renderer::MockDriver`) so `view` works without chromium.
    pub fn with_renderer(renderer: drawio_agent_renderer::Renderer) -> Self {
        Self { render: true, renderer: Some(Arc::new(renderer)) }
    }

    /// Tool docs embedded in the system prompt.
    pub fn tool_specs() -> &'static str {
        r#"1. read   {"range": "120-156" | "cell:svc-a" | "120"}}
   返回文件中指定区间的原文（带行号）。改之前先读；范围尽量小
   （超长区间会被截断）。行号会随编辑漂移，优先用 cell:id。

2. locate {"query": "order"}
   按文本/cell id 搜索，返回命中的 cell 与 @行区间（最多 12 条）。
   用来把用户说的概念（"订单服务那个框"）映射到文件位置。

3. edit   {"range": "120-156" | "cell:svc-a", "text": "<完整 XML 片段>"}
   把 range 覆盖的行整体替换为 text。text 必须是**完整自洽的 XML**：
   开闭标签齐全、属性完整（如 mxGeometry 要带 as="geometry"、
   mxCell 要带 parent/vertex），新增 cell 用新的唯一 id，连线要有
   source/target。只改目标 cell，其余必须字节不变——系统校验后回报
   added/changed/removed 清单，出现越界改动会被警告。

4. draw   {"xml": "<mxfile>…</mxfile>"}
   整图重建（新画一张图或大改布局时用）。xml 必须是完整 mxfile。

5. check  {}
   确定性校验：XML 结构、id 唯一、parent/source/target 引用完整。
   edit/draw 之后建议调用。

6. lint   {}
   确定性布局质量检查：重叠、连线交叉/穿框、标签溢出、越界、断引用。
   返回分级清单（error/warning + cell id）。edit/draw 之后建议调用，
   比纯眼睛可靠；warning 阈值化，不必强行全清（0 交叉但布局怪
   反而更差）。

7. view   {} 或 {"annotate": true} 或 {"focus": ["svc-a", "db"]}
   渲染当前文件为截图并作为图像消息发给你——你会真正看到这张图。
   - annotate=true：图上叠加红色小徽章标注 cell id（密集处自动避让），
     用于把视觉元素与 cell id 对应起来
   - focus=[id...]：只渲染这些 cell 的局部放大图（密集区域看细节用）
   检查：节点重叠、文字溢出框体、连线错位/穿框、箭头方向、布局失衡。
   看完再决定改哪里；不要连续重复调用（上一张图已经在你的上下文里）。
   画布坐标与 xml 行区间没有 1:1 对应：定位用 locate，几何值用 read。

7. 结束  {"reply": "<给用户的总结>", "done": true}
   任务完成时使用；reply 会直接展示给用户。"#
    }

    /// Run one tool. `name` comes straight from the model envelope; args are
    /// free-form JSON. Returns the tool result text (Err = tool failed; the
    /// error text goes back to the model as the result of a failed call).
    pub async fn run(
        &mut self,
        doc: &mut XmlDoc,
        name: &str,
        args: &Value,
    ) -> Result<ToolOutput, String> {
        match name {
            "read" => self.read(doc, args),
            "locate" => self.locate(doc, args),
            "edit" => self.edit(doc, args, false),
            "draw" => self.edit(doc, args, true),
            "check" => self.check(doc),
            "lint" => {
                let report = crate::metrics::analyze(doc.canonical())
                    .map_err(|e| format!("lint 失败: {e}"))?;
                Ok(ToolOutput::text(crate::metrics::lint_text(&report)))
            }
            "view" => self.view(doc, args).await,
            other => Err(format!("未知工具 `{other}`。可用: read locate edit draw check lint view")),
        }
    }

    fn read(&self, doc: &XmlDoc, args: &Value) -> Result<ToolOutput, String> {
        let spec = args
            .get("range")
            .and_then(Value::as_str)
            .ok_or_else(|| "read 需要参数 range".to_string())?;
        let total_lines = total_lines(doc.canonical());
        let (a, b) = resolve_arg(doc, spec).or_else(|e| {
            // read 允许范围超出文件末尾：截到最后一行为止（edit 仍严格拒绝）
            crate::xmlfile::parse_range_loose(spec, total_lines).map_err(|_| e)
        })?;
        const MAX_READ_LINES: usize = 150;
        let lines = lines_in(doc.canonical(), a, b);
        let total = lines.lines().count();
        let body: String = lines
            .lines()
            .take(MAX_READ_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        let note = if total > MAX_READ_LINES {
            format!(
                "\n…（区间共 {total} 行，已截断至前 {MAX_READ_LINES} 行；请缩小 range 分批读）"
            )
        } else {
            String::new()
        };
        Ok(ToolOutput::text(format!(
            "@{}:{} 内容如下:\n{}{}",
            file_stem(doc),
            range_str(a, b),
            numbered(&body, a),
            note
        )))
    }

    fn locate(&self, doc: &XmlDoc, args: &Value) -> Result<ToolOutput, String> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| "locate 需要参数 query".to_string())?;
        let q = query.to_lowercase();
        let text = doc.canonical();
        // 只报叶子 cell：容器（diagram 等）的 span 覆盖所有子元素，
        // 总会命中，属于噪音。
        let is_leaf = |c: &crate::xmlfile::CellSpan| {
            !doc.cells.iter().any(|o| {
                o.id != c.id && o.start_line > c.start_line && o.start_line <= c.end_line
            })
        };
        let mut hits: Vec<String> = Vec::new();
        for c in doc.cells.iter().filter(|c| is_leaf(c)) {
            let slice = lines_in(text, c.start_line, c.end_line);
            if slice.to_lowercase().contains(&q) {
                // find first matching line inside the cell for display
                let hit_line = slice
                    .lines()
                    .enumerate()
                    .find(|(_, l)| l.to_lowercase().contains(&q))
                    .map(|(i, l)| (c.start_line + i, l.to_string()));
                let value = attr_value(&slice, "value").unwrap_or_default();
                let style = attr_value(&slice, "style").unwrap_or_default();
                let mut s = format!(
                    "cell `{}` @{}-{}  value={:?}{}{}",
                    c.id,
                    c.start_line,
                    c.end_line,
                    truncate(&value, 40),
                    if style.is_empty() { String::new() } else { format!(" style={:?}", truncate(&style, 30)) },
                    if c.tag == "mxCell" { String::new() } else { format!(" tag={}", c.tag) }
                );
                if let Some((ln, l)) = hit_line {
                    s.push_str(&format!("\n  {:>5}| {}", ln, truncate(&l, 120)));
                }
                hits.push(s);
            }
        }
        const MAX_LOCATE_HITS: usize = 12;
        if hits.is_empty() {
            Ok(ToolOutput::text(format!(
                "没有找到包含 `{query}` 的 cell（共 {} 个 cell）",
                doc.cells.len()
            )))
        } else {
            let total = hits.len();
            let shown = if hits.len() > MAX_LOCATE_HITS {
                format!(
                    "命中 {total} 个 cell，显示前 {MAX_LOCATE_HITS} 条（请用更精确的查询缩小范围）:\n{}\n…",
                    hits[..MAX_LOCATE_HITS].join("\n")
                )
            } else {
                format!("命中 {} 个 cell:\n{}", hits.len(), hits.join("\n"))
            };
            Ok(ToolOutput::text(shown))
        }
    }

    fn edit(&mut self, doc: &mut XmlDoc, args: &Value, whole: bool) -> Result<ToolOutput, String> {
        let (range_spec, text) = if whole {
            let xml = args
                .get("xml")
                .and_then(Value::as_str)
                .ok_or_else(|| "draw 需要参数 xml".to_string())?;
            ("1-end".to_string(), xml.to_string())
        } else {
            let spec = args
                .get("range")
                .and_then(Value::as_str)
                .ok_or_else(|| "edit 需要参数 range".to_string())?;
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| "edit 需要参数 text".to_string())?;
            (spec.to_string(), text.to_string())
        };

        let (start, end) = if whole {
            (1usize, total_lines(doc.canonical()))
        } else {
            resolve_arg(doc, &range_spec)?
        };
        match doc.apply_edit(start, end, &text) {
            Ok(report) if report.noop => Ok(ToolOutput::text("no-op：内容与当前文件相同，未修改。")),
            Ok(report) => {
                doc.save()
                    .map_err(|e| format!("保存失败: {e}"))?;
                Ok(ToolOutput::text(format!(
                    "编辑已应用并保存。{}",
                    report_summary(&report)
                )))
            }
            Err(e) => Err(format!("编辑被拒绝（文件未改动）: {e}")),
        }
    }

    fn check(&self, doc: &XmlDoc) -> Result<ToolOutput, String> {
        match check_doc(doc.canonical()) {
            Ok(r) => Ok(ToolOutput::text(r.summarize())),
            Err(e) => Err(format!("校验失败: {e}")),
        }
    }

    /// Render current doc to PNG. `open=true` also opens it in the system
    /// viewer (human `/view`); model-driven calls pass `false` and instead
    /// get the PNG back as an image part.
    pub async fn view(&mut self, doc: &XmlDoc, args: &Value) -> Result<ToolOutput, String> {
        let open = args.get("open").and_then(Value::as_bool).unwrap_or(false);
        if !self.render {
            return Ok(ToolOutput::text(
                "渲染未启用（无 chromium）。请用 /view 在本地渲染查看。",
            ));
        }
        let renderer = match &self.renderer {
            Some(r) => r.clone(),
            None => {
                let driver = drawio_agent_renderer::HeadlessChromiumDriver::launch().await;
                let driver = match driver {
                    Ok(d) => d,
                    Err(e) => {
                        return Err(format!(
                            "chromium 启动失败: {e}（可设 DRAWIO_AGENT_CHROMIUM_PATH 指定路径）"
                        ))
                    }
                };
                let r = Arc::new(drawio_agent_renderer::Renderer::new(Arc::new(driver)));
                self.renderer = Some(r.clone());
                r
            }
        };
        let mut opts = drawio_agent_renderer::RenderOptions::default();
        opts.trace_dir = std::env::var("DRAWIO_RENDER_TRACE_DIR").ok();
        // 可选增强：annotate=id 徽章标注；focus=[cell ids] 局部裁剪放大
        if args.get("annotate").and_then(Value::as_bool) == Some(true) {
            opts.annotate = true;
        }
        if let Some(f) = args.get("focus").and_then(Value::as_array) {
            opts.focus = f
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
        }
        match renderer.render(doc.canonical(), &opts).await {
            Ok(png) => {
                let mut text = format!("渲染成功 ({} bytes, cells={})", png.len(), doc.cells.len());
                if open {
                    let stem = file_stem(doc).replace(".xml", "").replace(".drawio", "");
                    let png_path = doc
                        .path
                        .parent()
                        .unwrap_or(std::path::Path::new("."))
                        .join(format!("{stem}.png"));
                    if std::fs::write(&png_path, &png).is_ok() {
                        open_with_system_viewer(&png_path);
                        text = format!("已渲染并打开 {} ({}, cells={})", png_path.display(), png.len(), doc.cells.len());
                    }
                }
                Ok(ToolOutput::with_image(text, png))
            }
            Err(e) => Err(format!("渲染失败: {e}")),
        }
    }
}

fn open_with_system_viewer(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    #[cfg(not(unix))]
    {
        // `start` 的第二个参数是窗口标题；给目标路径加引号防空格路径断裂
        let quoted = format!("\"{}\"", path.display());
        let _ = std::process::Command::new("cmd").args(["/C", "start", ""]).arg(quoted).spawn();
    }
}

pub fn report_summary(r: &EditReport) -> String {
    if r.noop {
        return "no-op（无变化）".into();
    }
    let mut s = format!(
        "added={} removed={} changed={} unchanged={}",
        r.added.len(),
        r.removed.len(),
        r.changed.len(),
        r.unchanged
    );
    if !r.changed.is_empty() {
        s.push_str(&format!(" changed=[{}]", r.changed.join(", ")));
    }
    if !r.added.is_empty() {
        s.push_str(&format!(" added=[{}]", r.added.join(", ")));
    }
    if !r.removed.is_empty() {
        s.push_str(&format!(" removed=[{}]", r.removed.join(", ")));
    }
    if !r.off_range.is_empty() {
        s.push_str(&format!(
            " ⚠️ 改动越界（不在请求区间内）: [{}]",
            r.off_range.join(", ")
        ));
    }
    s
}

fn attr_value(slice: &str, key: &str) -> Option<String> {
    let marker = format!("{key}=\"");
    let i = slice.find(&marker)?;
    let rest = &slice[i + marker.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

fn file_stem(doc: &XmlDoc) -> String {
    doc.path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "diagram.xml".into())
}

fn range_str(a: usize, b: usize) -> String {
    if a == b {
        a.to_string()
    } else {
        format!("{a}-{b}")
    }
}

pub fn check_text(text: &str) -> Result<CheckReport, String> {
    check_doc(text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> XmlDoc {
        let s = r#"<mxfile host="app.diagrams.net"><diagram id="d1"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="svc-a" value="Order Service" vertex="1" parent="1"><mxGeometry x="40" y="60" width="160" height="60" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;
        XmlDoc::from_text(s).unwrap()
    }

    #[tokio::test]
    async fn edit_by_cell_id_applies_and_reports() {
        let mut d = doc();
        let mut t = Tools::new(false);
        let args = serde_json::json!({
            "range": "cell:svc-a",
            "text": r#"<mxCell id="svc-a" value="Payments" vertex="1" parent="1"><mxGeometry x="40" y="60" width="200" height="60" as="geometry"/></mxCell>"#
        });
        let out = t.run(&mut d, "edit", &args).await.unwrap();
        assert!(out.text.contains("changed=[svc-a]"), "{}", out.text);
        assert!(d.canonical().contains("Payments"));
        assert!(!d.canonical().contains("Order Service"));
    }

    #[tokio::test]
    async fn draw_replaces_whole_file() {
        let mut d = doc();
        let mut t = Tools::new(false);
        let xml = r#"<mxfile host="x"><diagram id="d2"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="n1" value="New" vertex="1" parent="1"><mxGeometry x="0" y="0" width="100" height="50" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;
        let out = t.run(&mut d, "draw", &serde_json::json!({ "xml": xml })).await.unwrap();
        assert!(out.text.contains("added=[n1]"), "{}", out.text);
        assert!(d.id_to_cell("svc-a").is_none());
    }

    #[tokio::test]
    async fn bad_edit_is_rejected_and_file_unchanged() {
        let mut d = doc();
        let before = d.canonical().to_string();
        let mut t = Tools::new(false);
        let out = t.run(&mut d, "edit", &serde_json::json!({"range": "cell:svc-a", "text": "<mxCell>"})).await;
        assert!(out.is_err());
        assert_eq!(d.canonical(), before);
    }

    #[tokio::test]
    async fn locate_finds_by_value() {
        let mut d = doc();
        let mut t = Tools::new(false);
        let out = t.run(&mut d, "locate", &serde_json::json!({"query": "order"})).await.unwrap();
        assert!(out.text.contains("svc-a"), "{}", out.text);
        assert!(out.text.contains("@"), "{}", out.text);
    }

    #[tokio::test]
    async fn read_truncates_oversized_ranges() {
        // 200+ cells: read 1-300 must truncate at 150 lines with a note.
        let mut xml = String::from("<mxfile><diagram id=\"d\"><mxGraphModel><root><mxCell id=\"0\"/>");
        for i in 1..=210 {
            xml.push_str(&format!(r#"<mxCell id="n{i}" value="x" vertex="1" parent="1"><mxGeometry x="0" y="0" width="10" height="10" as="geometry"/></mxCell>"#));
        }
        xml.push_str("</root></mxGraphModel></diagram></mxfile>");
        let mut d = XmlDoc::from_text(&xml).unwrap();
        let mut t = Tools::new(false);
        let out = t.run(&mut d, "read", &serde_json::json!({"range": "1-999"})).await.unwrap();
        assert!(out.text.contains("已截断"), "{}", &out.text[out.text.len().saturating_sub(120)..]);
        assert!(out.text.lines().count() <= 160, "{}", out.text.lines().count());
    }

    #[tokio::test]
    async fn locate_caps_hits_at_twelve() {
        let mut xml = String::from("<mxfile><diagram id=\"d\"><mxGraphModel><root><mxCell id=\"0\"/>");
        for i in 1..=30 {
            xml.push_str(&format!(r#"<mxCell id="n{i}" value="common word {i}" vertex="1" parent="1"><mxGeometry x="0" y="0" width="10" height="10" as="geometry"/></mxCell>"#));
        }
        xml.push_str("</root></mxGraphModel></diagram></mxfile>");
        let mut d = XmlDoc::from_text(&xml).unwrap();
        let mut t = Tools::new(false);
        let out = t.run(&mut d, "locate", &serde_json::json!({"query": "common word"})).await.unwrap();
        assert!(out.text.contains("命中 30 个 cell，显示前 12 条"), "{}", &out.text[..120]);
    }

    #[tokio::test]
    async fn read_returns_numbered_lines() {
        let mut d = doc();
        let mut t = Tools::new(false);
        let start_line = {
            let a = d.id_to_cell("svc-a").unwrap();
            a.start_line
        };
        let end_line = {
            let a = d.id_to_cell("svc-a").unwrap();
            a.end_line
        };
        let range = format!("{start_line}-{end_line}");
        let out = t
            .run(&mut d, "read", &serde_json::json!({"range": range}))
            .await
            .unwrap();
        assert!(out.text.contains("Order Service"), "{}", out.text);
        assert!(out.text.contains(&format!("{start_line}|")));
    }
}
