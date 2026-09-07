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
/// (id, (x, y, w, h)) 几何元组，layout 工具内部用。
type GeomEntry = (String, (f64, f64, f64, f64));


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
        r#"1. read   {"range": "120-156" | "cell:svc-a" | "120"}
          或 {"query": "order"}（按文本搜 cell，返回命中 cell 与 @行区间，
           最多 12 条——把用户说的概念映射到文件位置）
   返回文件中指定区间的原文（带行号）。改之前先读；范围尽量小
   （超长区间会被截断）。行号会随编辑漂移，优先用 cell:id。

3. edit   {"range": "120-156" | "cell:svc-a", "text": "<完整 XML 片段>"}
          或批量 {"ranges": [{"range": "...", "text": "..."}, ...]}
   把 range 覆盖的行整体替换为 text。text 必须是**完整自洽的 XML**：
   开闭标签齐全、属性完整（如 mxGeometry 要带 as="geometry"、
   mxCell 要带 parent/vertex），新增 cell 用新的唯一 id，连线要有
   source/target。只改目标 cell，其余必须字节不变——系统校验后回报
   added/changed/removed 清单，出现越界改动会被警告。
   **批量（ranges 数组）**：一次提交多个不重叠的区间（行号都按当前
   文件），全部通过才落盘、任一失败整体不动（全或无）。
   规则：需要改动 2 个及以上 cell 时**必须**用批量一次提交，禁止逐个
   cell 单独 edit（那会浪费大量轮次——已有实测反馈）。

4. draw   {"xml": "<mxfile>…</mxfile>"}
   整图重建（新画一张图或大改布局时用）。xml 必须是完整 mxfile。

5. check  {}
   确定性结构校验：XML 合法、id 唯一、parent/source/target 引用完整。
   edit/draw 之后建议调用。布局质量（重叠/交叉/对齐/箭头）不看这里——
   用 view 看图自己判断（你有语义理解：容器背景叠放、有向边箭头等
   由你按图意把握）。

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
   画布坐标与 xml 行区间没有 1:1 对应：定位用 read query，几何值用 read。

8. layout {"move": {"ids": [...], "dx": n, "dy": n}}
         或 {"align": {"ids": [...], "axis": "x"|"y", "mode": "left"|"right"|"center"|"top"|"bottom"|"middle"|"gap"}}
   几何级工具：批量平移或对齐/等距分布多个 cell。只动 mxGeometry，
   不碰文本/样式/连线（那些用 edit）。整批一次落盘，失败整体回滚。

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
            "edit" => self.edit(doc, args, false),
            "draw" => self.edit(doc, args, true),
            "check" => self.check(doc),
            "layout" => self.layout(doc, args),
            "view" => self.view(doc, args).await,
            other => Err(format!("未知工具 `{other}`。可用: read edit draw check view layout")),
        }
    }

    fn read(&self, doc: &XmlDoc, args: &Value) -> Result<ToolOutput, String> {
        // 查询模式：read {"query": "..."} 按文本搜索 cell（旧 locate 合并）
        if let Some(q) = args.get("query").and_then(Value::as_str) {
            return self.locate(doc, q);
        }
        let spec = args
            .get("range")
            .and_then(Value::as_str)
            .ok_or_else(|| "read 需要参数 range 或 query".to_string())?;
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

    /// 几何级工具：align（对齐/等距）与 move（平移）。只动 mxGeometry，
    /// 不碰文本/样式/连线（那些走 edit）。整批计算 → 一次 apply_edits 落盘。
    fn layout(&self, doc: &mut XmlDoc, args: &Value) -> Result<ToolOutput, String> {
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        if let Some(m) = args.get("move") {
            let ids: Vec<String> = m
                .get("ids")
                .and_then(Value::as_array)
                .ok_or_else(|| "move 需要 ids 数组".to_string())?
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            let dx = m.get("dx").and_then(Value::as_f64).unwrap_or(0.0);
            let dy = m.get("dy").and_then(Value::as_f64).unwrap_or(0.0);
            if ids.is_empty() {
                return Err("move 的 ids 不能为空".to_string());
            }
            for id in &ids {
                let (x, y, w, h) = doc
                    .geometry_of(id)
                    .ok_or_else(|| format!("cell `{id}` 不存在或其几何不可读"))?;
                if let Some((ln, line)) = doc
                    .set_geometry_line(id, x + dx, y + dy, w, h)
                    .map_err(|e| format!("{e}"))?
                {
                    edits.push((ln, ln, line));
                }
            }
        } else if let Some(a) = args.get("align") {
            let ids: Vec<String> = a
                .get("ids")
                .and_then(Value::as_array)
                .ok_or_else(|| "align 需要 ids 数组".to_string())?
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            let axis = a.get("axis").and_then(Value::as_str).unwrap_or("x");
            let mode = a.get("mode").and_then(Value::as_str).unwrap_or("left");
            if ids.len() < 2 {
                return Err("align 至少需要 2 个 cell".to_string());
            }
            let mut geoms: Vec<GeomEntry> = Vec::new();
            for id in &ids {
                let g = doc
                    .geometry_of(id)
                    .ok_or_else(|| format!("cell `{id}` 不存在或其几何不可读"))?;
                geoms.push((id.clone(), g));
            }
            // 计算目标值并生成几何行替换
            // axis=x: left(最小x) right(最大右缘) center(中心对齐) gap(水平等距)
            // axis=y: top(最小y) bottom(最大下缘) middle(中心对齐) gap(垂直等距)
            let mut targets: Vec<(String, f64, f64)> = Vec::new(); // id, x, y
            let geoms_ref = &geoms;
            if axis == "x" && mode == "left" {
                let t = geoms_ref.iter().map(|(_, g)| g.0).fold(f64::INFINITY, f64::min);
                targets = geoms_ref.iter().map(|(id, g)| (id.clone(), t, g.1)).collect();
            } else if axis == "x" && mode == "right" {
                let t = geoms_ref.iter().map(|(_, g)| g.0 + g.2).fold(f64::NEG_INFINITY, f64::max);
                targets = geoms_ref.iter().map(|(id, g)| (id.clone(), t - g.2, g.1)).collect();
            } else if axis == "x" && mode == "center" {
                let t = geoms_ref.iter().map(|(_, g)| g.0 + g.2 / 2.0).sum::<f64>() / geoms_ref.len() as f64;
                targets = geoms_ref.iter().map(|(id, g)| (id.clone(), t - g.2 / 2.0, g.1)).collect();
            } else if axis == "y" && mode == "top" {
                let t = geoms_ref.iter().map(|(_, g)| g.1).fold(f64::INFINITY, f64::min);
                targets = geoms_ref.iter().map(|(id, g)| (id.clone(), g.0, t)).collect();
            } else if axis == "y" && mode == "bottom" {
                let t = geoms_ref.iter().map(|(_, g)| g.1 + g.3).fold(f64::NEG_INFINITY, f64::max);
                targets = geoms_ref.iter().map(|(id, g)| (id.clone(), g.0, t - g.3)).collect();
            } else if axis == "y" && mode == "middle" {
                let t = geoms_ref.iter().map(|(_, g)| g.1 + g.3 / 2.0).sum::<f64>() / geoms_ref.len() as f64;
                targets = geoms_ref.iter().map(|(id, g)| (id.clone(), g.0, t - g.3 / 2.0)).collect();
            } else if mode == "gap" {
                // 等距分布：按轴排序后均匀摆放（间距 = 空隙相等）
                let horizontal = axis == "x";
                let mut sorted: Vec<&GeomEntry> = geoms_ref.iter().collect();
                if horizontal {
                    sorted.sort_by(|a, b| a.1 .0.partial_cmp(&b.1 .0).unwrap());
                } else {
                    sorted.sort_by(|a, b| a.1 .1.partial_cmp(&b.1 .1).unwrap());
                }
                let total_len: f64 = if horizontal {
                    sorted.iter().map(|(_, g)| g.2).sum()
                } else {
                    sorted.iter().map(|(_, g)| g.3).sum()
                };
                let span = if horizontal {
                    sorted.last().unwrap().1 .0 + sorted.last().unwrap().1 .2 - sorted.first().unwrap().1 .0
                } else {
                    sorted.last().unwrap().1 .1 + sorted.last().unwrap().1 .3 - sorted.first().unwrap().1 .1
                };
                let gap = ((span - total_len) / (sorted.len().saturating_sub(1) as f64)).max(0.0);
                let mut cursor = if horizontal { sorted[0].1 .0 } else { sorted[0].1 .1 };
                for (id, g) in &sorted {
                    if horizontal {
                        targets.push((id.clone(), cursor, g.1));
                        cursor += g.2 + gap;
                    } else {
                        targets.push((id.clone(), g.0, cursor));
                        cursor += g.3 + gap;
                    }
                }
            } else {
                return Err(
                    "align 支持: axis=x|y × mode=left|right|center|top|bottom|middle|gap（axis 与 mode 需匹配）                     示例: {\"align\": {\"ids\": [\"a\",\"b\",\"c\"], \"axis\": \"x\", \"mode\": \"left\"}}"
                        .to_string(),
                );
            }
            for (id, nx, ny) in &targets {
                let (_, _, w, h) = doc
                    .geometry_of(id)
                    .ok_or_else(|| format!("cell `{id}` 不存在"))?;
                if let Some((ln, line)) = doc
                    .set_geometry_line(id, *nx, *ny, w, h)
                    .map_err(|e| format!("{e}"))?
                {
                    edits.push((ln, ln, line));
                }
            }
        } else {
            return Err("layout 需要 move 或 align 参数".to_string());
        }
        if edits.is_empty() {
            return Ok(ToolOutput::text("no-op：几何无需调整。"));
        }
        match doc.apply_edits(&edits) {
            Ok(report) if report.noop => Ok(ToolOutput::text("no-op：内容与当前文件相同，未修改。")),
            Ok(report) => {
                doc.save().map_err(|e| format!("保存失败: {e}"))?;
                Ok(ToolOutput::text(format!(
                    "布局已应用并保存。{}",
                    report_summary(&report)
                )))
            }
            Err(e) => Err(format!("布局被拒绝（文件未改动）: {e}")),
        }
    }

    fn locate(&self, doc: &XmlDoc, query: &str) -> Result<ToolOutput, String> {
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
        if whole {
            let xml = args
                .get("xml")
                .and_then(Value::as_str)
                .ok_or_else(|| "draw 需要参数 xml".to_string())?;
            let (start, end) = (1usize, total_lines(doc.canonical()));
            return match doc.apply_edit(start, end, xml) {
                Ok(report) if report.noop => {
                    Ok(ToolOutput::text("no-op：内容与当前文件相同，未修改。"))
                }
                Ok(report) => {
                    doc.save().map_err(|e| format!("保存失败: {e}"))?;
                    Ok(ToolOutput::text(format!(
                        "编辑已应用并保存。{}",
                        report_summary(&report)
                    )))
                }
                Err(e) => Err(format!("编辑被拒绝（文件未改动）: {e}")),
            };
        }
        // 批量：ranges=[{range,text},...] 一次全做（全或无）；兼容旧单区间
        let batch: Vec<(usize, usize, String)> = if let Some(ranges) = args.get("ranges") {
            let arr = ranges
                .as_array()
                .ok_or_else(|| "ranges 需要是数组".to_string())?;
            if arr.is_empty() {
                return Err("ranges 不能为空".to_string());
            }
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                let spec = item
                    .get("range")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "批量项需要 range".to_string())?;
                let text = item
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "批量项需要 text".to_string())?;
                let (start, end) = resolve_arg(doc, spec)?;
                out.push((start, end, text.to_string()));
            }
            out
        } else {
            let spec = args
                .get("range")
                .and_then(Value::as_str)
                .ok_or_else(|| "edit 需要参数 range 或 ranges".to_string())?;
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| "edit 需要参数 text".to_string())?;
            let (start, end) = resolve_arg(doc, spec)?;
            vec![(start, end, text.to_string())]
        };

        // 全或无：内存内一次应用 + 单次校验，通过才原子落盘
        match doc.apply_edits(&batch) {
            Ok(report) if report.noop => Ok(ToolOutput::text("no-op：内容与当前文件相同，未修改。")),
            Ok(report) => {
                doc.save().map_err(|e| format!("保存失败: {e}"))?;
                Ok(ToolOutput::text(format!(
                    "编辑已应用并保存。{}",
                    report_summary(&report)
                )))
            }
            Err(e) => Err(format!(
                "编辑被拒绝（文件未改动）: {e}\n正确形态示例（单 cell 自洽 XML，含完整属性）：\n<mxCell id=\"新id\" value=\"标签\" vertex=\"1\" parent=\"1\"><mxGeometry x=\"40\" y=\"60\" width=\"120\" height=\"60\" as=\"geometry\"/></mxCell>\n连线需 vertex→edge：source/target=已有 cell id、父级 parent=\"1\"。"
            )),
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
        // 可选增强：annotate=id 徽章标注；focus=[cell ids] 局部裁剪放大
        let mut opts = drawio_agent_renderer::RenderOptions {
            trace_dir: std::env::var("DRAWIO_RENDER_TRACE_DIR").ok(),
            annotate: args.get("annotate").and_then(Value::as_bool) == Some(true),
            ..Default::default()
        };
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

    fn two_cell_doc() -> XmlDoc {
        let s = r#"<mxfile host="app.diagrams.net"><diagram id="d1"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="svc-a" value="A" vertex="1" parent="1"><mxGeometry x="40" y="60" width="160" height="60" as="geometry"/></mxCell><mxCell id="svc-b" value="B" vertex="1" parent="1"><mxGeometry x="260" y="60" width="160" height="60" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;
        XmlDoc::from_text(s).unwrap()
    }

    #[tokio::test]
    async fn batch_edit_applies_all_cells_atomically() {
        let mut d = two_cell_doc();
        let mut t = Tools::new(false);
        let l = d.id_to_cell("svc-a").unwrap();
        let r = d.id_to_cell("svc-b").unwrap();
        let before = d.canonical().to_string();
        let args = serde_json::json!({
            "ranges": [
                {"range": format!("{}-{}", l.start_line, l.end_line),
                 "text": r#"<mxCell id="svc-a" value="PA" vertex="1" parent="1"><mxGeometry x="40" y="60" width="200" height="60" as="geometry"/></mxCell>"#},
                {"range": format!("{}-{}", r.start_line, r.end_line),
                 "text": r#"<mxCell id="svc-b" value="PB" vertex="1" parent="1"><mxGeometry x="260" y="60" width="200" height="60" as="geometry"/></mxCell>"#}
            ]
        });
        let out = t.run(&mut d, "edit", &args).await.unwrap();
        assert!(out.text.contains("changed=[svc-a, svc-b]"), "{}", out.text);
        assert!(d.canonical().contains("PA") && d.canonical().contains("PB"));
        assert_ne!(d.canonical(), before);
    }

    #[tokio::test]
    async fn batch_edit_failure_keeps_file_untouched() {
        let mut d = two_cell_doc();
        let mut t = Tools::new(false);
        let l = d.id_to_cell("svc-a").unwrap();
        let r = d.id_to_cell("svc-b").unwrap();
        let before = d.canonical().to_string();
        let args = serde_json::json!({
            "ranges": [
                {"range": format!("{}-{}", l.start_line, l.end_line),
                 "text": r#"<mxCell id="svc-a" value="PA" vertex="1" parent="1"><mxGeometry x="40" y="60" width="200" height="60" as="geometry"/></mxCell>"#},
                {"range": format!("{}-{}", r.start_line, r.end_line),
                 "text": r#"<mxCell id="svc-a" value="DUP" vertex="1" parent="1"><mxGeometry x="260" y="60" width="200" height="60" as="geometry"/></mxCell>"#}
            ]
        });
        let out = t.run(&mut d, "edit", &args).await;
        assert!(out.is_err(), "应整体失败");
        assert_eq!(d.canonical(), before, "内存不应改动");
    }

    #[tokio::test]
    async fn batch_edit_rejects_overlapping_ranges() {
        let mut d = doc();
        let mut t = Tools::new(false);
        let l = d.id_to_cell("svc-a").unwrap();
        let before = d.canonical().to_string();
        let args = serde_json::json!({
            "ranges": [
                {"range": format!("{}-{}", l.start_line, l.end_line),
                 "text": r#"<mxCell id="svc-a" value="PA" vertex="1" parent="1"><mxGeometry x="40" y="60" width="200" height="60" as="geometry"/></mxCell>"#},
                {"range": format!("{}-{}", l.start_line, l.end_line),
                 "text": "x"}
            ]
        });
        let out = t.run(&mut d, "edit", &args).await;
        assert!(out.is_err());
        assert!(out.err().unwrap().contains("重叠"));
        assert_eq!(d.canonical(), before);
    }

    #[tokio::test]
    async fn layout_move_shifts_cells() {
        let mut d = two_cell_doc();
        let mut t = Tools::new(false);
        let out = t.run(&mut d, "layout", &serde_json::json!({
            "move": {"ids": ["svc-a", "svc-b"], "dx": 40, "dy": 10}
        })).await.unwrap();
        assert!(out.text.contains("changed=[svc-a, svc-b]"), "{}", out.text);
        let ga = d.geometry_of("svc-a").unwrap();
        let gb = d.geometry_of("svc-b").unwrap();
        assert_eq!((ga.0, ga.1), (80.0, 70.0));
        assert_eq!((gb.0, gb.1), (300.0, 70.0));
    }

    #[tokio::test]
    async fn layout_align_x_left_groups_columns() {
        let mut d = two_cell_doc();
        let mut t = Tools::new(false);
        // 再造一个 x 不同的 cell 验证 left 对齐
        let a = d.geometry_of("svc-a").unwrap();
        let out = t.run(&mut d, "layout", &serde_json::json!({
            "align": {"ids": ["svc-a", "svc-b"], "axis": "x", "mode": "left"}
        })).await.unwrap();
        assert!(out.text.contains("changed=[svc-b]"), "{}", out.text);
        assert_eq!(d.geometry_of("svc-b").unwrap().0, a.0);
    }

    #[tokio::test]
    async fn layout_align_gap_evenly_distributes() {
        let mut d = two_cell_doc();
        let mut t = Tools::new(false);
        // 竖向等距：两个 cell 上下排
        let out = t.run(&mut d, "layout", &serde_json::json!({
            "align": {"ids": ["svc-a", "svc-b"], "axis": "y", "mode": "gap"}
        })).await.unwrap();
        // 应让两者间隔相等（这里两 cell 同尺寸 → 上下紧贴排列）
        let ga = d.geometry_of("svc-a").unwrap();
        let gb = d.geometry_of("svc-b").unwrap();
        assert_eq!(ga.1, 60.0);
        assert_eq!(gb.1, 120.0, "b 应紧贴 a 下方（等距=0 空隙）");
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
        let out = t.run(&mut d, "read", &serde_json::json!({"query": "order"})).await.unwrap();
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
        let out = t.run(&mut d, "read", &serde_json::json!({"query": "common word"})).await.unwrap();
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
