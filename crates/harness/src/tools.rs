//! Tools the model (and the human REPL) can invoke. Every mutation goes
//! through `XmlDoc::apply_edit` — line-range textual replacement + full
//! validation — so locality (untouched bytes stay untouched) is a property
//! of the mechanics, not a promise.

use std::sync::Arc;

use serde_json::Value;

use crate::xmlfile::{check_doc, lines_in, total_lines, CheckReport, EditReport, XmlDoc, XmlError};

/// Result of executing one tool: free-form text fed back to the model,
/// optionally carrying an image (the `view` tool returns the screenshot so
/// the engine can send it to a vision-capable model as an image part).
/// (id, (x, y, w, h)) 几何元组，layout 工具内部用。
type GeomEntry = (String, (f64, f64, f64, f64));

// Tool metadata is defined in tools_meta.rs (shared with build.rs)
include!("tools_meta.rs");

/// 模型可调用的全部工具名（从 TOOL_META 派生，保证同步）。
pub const TOOL_NAMES: [&str; 6] = [
    TOOL_META[0].0,
    TOOL_META[1].0,
    TOOL_META[2].0,
    TOOL_META[3].0,
    TOOL_META[4].0,
    TOOL_META[5].0,
];

/// move 单项语义：相对偏移（dx/dy），或绝对定位（x/y，None = 该轴
/// 保持不变）。模型想「放到 (400,200)」就直接写绝对值，不必算 delta。
#[derive(Debug, Clone, Copy)]
enum MoveSpec {
    Delta(f64, f64),
    Place(Option<f64>, Option<f64>),
}

/// 坐标显示：整数不带小数，其余两位（与 set_geometry_line 的写盘格式一致）。
fn fmt_coord(v: f64) -> String {
    if (v - v.round()).abs() < 1e-9 {
        format!("{}", v.round() as i64)
    } else {
        format!("{v:.2}")
    }
}


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
    /// 上次成功 view 的 (文件内容 + 参数) hash：同一 Tools 生命周期内
    /// 文件未变且参数相同 → 不重渲染、不重发图（截图占上下文，模型
    /// 反复刷同一状态是常见浪费）。人工 `open` 路径不缓存（用户显式
    /// 要求渲染并落盘打开）。
    last_view: Option<u64>,
    /// 同组目标 layout 微调计数，键为「op:排序后目标 ids」，自上次成功
    /// edit/draw 起算。E2E 实测：模型为消交叉对同一组节点 ±40px 反复
    /// nudge（参数每次都变，turn_loop 的同参守卫抓不到），7 连发烧尽
    /// 预算。第 3 次拒绝并给策略建议。
    nudge_counts: std::collections::HashMap<String, u32>,
    /// 同组连续 edit 守卫：连续触达同一批 cell 的第 3 次 edit 被拒绝。
    /// E2E 实测：模型给边加锚点后 view 闭环微调，e9/e10 连续 edit 3+ 次
    /// 烧尽 30 轮（每次参数都不同，其他守卫抓不到）。draw 重画后重置。
    last_edit_key: Option<String>,
    edit_streak: u32,
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

/// 单次 read（所有模式合计）回给模型的正文行数上限：控制上下文膨胀，
/// 超出截断并给出续读路径。
const MAX_READ_LINES: usize = 150;

impl Tools {
    pub fn new(render: bool) -> Self {
        Self {
            render,
            renderer: None,
            last_view: None,
            nudge_counts: Default::default(),
            last_edit_key: None,
            edit_streak: 0,
        }
    }

    /// Test seam: inject a canned renderer (e.g. built on
    /// `drawio_agent_renderer::MockDriver`) so `view` works without chromium.
    pub fn with_renderer(renderer: drawio_agent_renderer::Renderer) -> Self {
        Self {
            render: true,
            renderer: Some(Arc::new(renderer)),
            last_view: None,
            nudge_counts: Default::default(),
            last_edit_key: None,
            edit_streak: 0,
        }
    }

/// Tool docs embedded in the system prompt.
    pub fn tool_specs() -> &'static str {
        // Generated from TOOL_META at compile time via a const function.
        // This ensures TOOL_NAMES and tool_specs are always in sync.
        const SPECS: &str = {
            // We use a const fn to build the string at compile time
            // The actual generation is done by the macro below
            include_str!(concat!(env!("OUT_DIR"), "/tool_specs.txt"))
        };
        SPECS
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
            "layout" => self.layout(doc, args).await,
            "view" => self.view(doc, args).await,
            other => Err(format!(
                "未知工具 `{other}`。可用: {}",
                TOOL_NAMES.join(" ")
            )),
        }
    }

    fn read(&self, doc: &XmlDoc, args: &Value) -> Result<ToolOutput, String> {
        // 查询模式：read {"query": "..."} 按文本搜索 cell（旧 locate 合并）
        if let Some(q) = args.get("query").and_then(Value::as_str) {
            return self.locate(doc, q);
        }
        // 概览模式：read {"outline": true} 全图每实体一行（大图先概览再精读）
        if args.get("outline").and_then(Value::as_bool) == Some(true) {
            let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
            return self.outline(doc, offset);
        }
        // 批量模式：read {"cells": ["svc-a", "e3", "40-80"]} 一次读多个
        if let Some(cells) = args.get("cells").and_then(Value::as_array) {
            return self.read_cells(doc, cells);
        }
        let spec = args
            .get("range")
            .and_then(Value::as_str)
            .ok_or_else(|| "read 需要参数 range / cells / outline / query".to_string())?;
        let total_lines = total_lines(doc.canonical());
        let (a, b) = resolve_arg(doc, spec).or_else(|e| {
            // read 允许范围超出文件末尾：截到最后一行为止（edit 仍严格拒绝）
            crate::xmlfile::parse_range_loose(spec, total_lines).map_err(|_| e)
        })?;
        let lines = lines_in(doc.canonical(), a, b);
        let total = lines.lines().count();
        let body: String = lines
            .lines()
            .take(MAX_READ_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        let note = if total > MAX_READ_LINES {
            let shown_end = a + MAX_READ_LINES - 1;
            let next_start = a + MAX_READ_LINES;
            format!(
                "\n…（区间共 {total} 行，已截断：显示 {a}-{shown_end}；续读用 range \"{next_start}-{b}\"）"
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

    /// 批量读：一次拿多个 cell/区间，每项都是 range 解析语法（裸 id、
    /// `cell:id`、`a-b`、`file:a-b` 均可）。读操作无副作用，单项失败不拖累
    /// 其他项——错误行内报告，成功的照常返回（与 edit 的全或无相反，刻意如此）。
    fn read_cells(&self, doc: &XmlDoc, cells: &[Value]) -> Result<ToolOutput, String> {
        if cells.is_empty() {
            return Err("cells 不能为空".to_string());
        }
        let mut out = String::new();
        let mut budget = MAX_READ_LINES;
        let mut errors: Vec<String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        for item in cells {
            let Some(spec) = item.as_str() else {
                errors.push(format!("{}: 数组项需是字符串 range 语法", item));
                continue;
            };
            match resolve_arg(doc, spec) {
                Ok((a, b)) => {
                    if budget == 0 {
                        skipped.push(spec.to_string());
                        continue;
                    }
                    let slice = lines_in(doc.canonical(), a, b);
                    let taken: Vec<&str> = slice.lines().take(budget).collect();
                    budget -= taken.len();
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&format!(
                        "@{}:{} 内容如下:\n{}",
                        file_stem(doc),
                        range_str(a, b),
                        numbered(&taken.join("\n"), a)
                    ));
                }
                Err(e) => errors.push(format!("{spec}: {e}")),
            }
        }
        if !skipped.is_empty() {
            out.push_str(&format!(
                "\n…已达单次 {MAX_READ_LINES} 行上限，未读取: {}（请分次读取）",
                skipped.join(", ")
            ));
        }
        if !errors.is_empty() {
            out.push_str(&format!("\n未解析: {}", errors.join("；")));
        }
        Ok(ToolOutput::text(out))
    }

    /// 全图概览：每个实体一行「行区间 | id | 类型 | 标签」，先看概览再精读。
    /// 跳过结构锚点 0/1；非叶子元素标注（容器），其 span 覆盖全部子元素。
    /// 每次最多 {MAX_READ_LINES} 行，`offset` 按实体个数分页。
    fn outline(&self, doc: &XmlDoc, offset: usize) -> Result<ToolOutput, String> {
        let text = doc.canonical();
        let is_leaf = |c: &crate::xmlfile::CellSpan| {
            !doc.cells().iter().any(|o| {
                o.id != c.id && o.start_line > c.start_line && o.start_line <= c.end_line
            })
        };
        let mut rows: Vec<String> = Vec::new();
        let mut n_vertex = 0usize;
        let mut n_edge = 0usize;
        for c in doc.cells() {
            if c.id == "0" || c.id == "1" {
                continue;
            }
        let slice = lines_in(text, c.start_line, c.end_line);
        // 属性只看首行（本元素的开始标签）——容器若扫整段 slice
        // 会混入子元素的 vertex/edge 属性。
        let open_line = slice.lines().next().unwrap_or_default();
        let edge = open_line.contains("edge=\"1\"");
        let vertex = open_line.contains("vertex=\"1\"");
            let value = attr_value(&slice, "value").unwrap_or_default();
            let kind: &str = if edge {
                "edge"
            } else if vertex {
                "vertex"
            } else {
                &c.tag
            };
            let detail = if edge {
                let s = attr_value(&slice, "source").unwrap_or_else(|| "?".into());
                let t = attr_value(&slice, "target").unwrap_or_else(|| "?".into());
                let lbl = if value.is_empty() {
                    String::new()
                } else {
                    format!(" “{}”", truncate(&value, 20))
                };
                format!("{s}→{t}{lbl}")
            } else if vertex {
                if value.is_empty() {
                    let style = attr_value(&slice, "style").unwrap_or_default();
                    if style.is_empty() {
                        "—".to_string()
                    } else {
                        format!("style≈{}", truncate(&style, 30))
                    }
                } else {
                    truncate(&value, 40)
                }
            } else if value.is_empty() {
                attr_value(&slice, "name").unwrap_or_else(|| "—".into())
            } else {
                truncate(&value, 40)
            };
            let container = if is_leaf(c) { "" } else { " （容器）" };
            rows.push(format!(
                "{:>5}-{:<5} {:<14} {:<7} {}{}",
                c.start_line,
                c.end_line,
                truncate(&c.id, 14),
                kind,
                detail,
                container
            ));
            if edge {
                n_edge += 1;
            } else if vertex {
                n_vertex += 1;
            }
        }
        let total = rows.len();
        let mut out = format!(
            "全图概览：{} vertex · {} edge · 共 {total} 个实体（行区间 | id | 类型 | 标签；\
             容器的行区间覆盖其全部子元素）\n",
            n_vertex, n_edge
        );
        let shown: Vec<&String> = rows.iter().skip(offset).take(MAX_READ_LINES).collect();
        if shown.is_empty() {
            out.push_str(&format!("（offset {offset} 超出范围，共 {total} 个实体）"));
            return Ok(ToolOutput::text(out));
        }
        out.push_str(&shown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n"));
        let end = offset + shown.len();
        if end < total {
            out.push_str(&format!(
                "\n…（还有 {} 个未显示；续读加 \"offset\": {end}）",
                total - end
            ));
        }
        Ok(ToolOutput::text(out))
    }

    /// 几何级工具：align（对齐/等距）与 move（平移）。只动 mxGeometry，
    /// 不碰文本/样式/连线（那些走 edit）。整批计算 → 一次 apply_edits 落盘。
    /// 同组目标微调守卫：自上次成功 edit/draw 起，对同一组 cell 的第 3 次
    /// layout 调用拒绝（前两次放行，参数不同也算——±40px 换参数绕不过）。
    /// 局部平移解决不了交叉/重叠，模型需要「加拐点 / 重排 / 接受残余」
    /// 的策略提示而不是继续烧轮次（E2E 实测 7 连发）。
    /// 按需拉起渲染器（view/route 共用）：route 是常被首先调用的工具，
    /// 不能等 view 先跑一次才具备 libavoid 能力。
    async fn ensure_renderer(
        &mut self,
    ) -> Result<Arc<drawio_agent_renderer::Renderer>, String> {
        if let Some(r) = &self.renderer {
            return Ok(r.clone());
        }
        let driver = drawio_agent_renderer::HeadlessChromiumDriver::launch().await;
        let driver = driver.map_err(|e| {
            format!("chromium 启动失败: {e}（可设 DRAWIO_AGENT_CHROMIUM_PATH 指定路径）")
        })?;
        let r = Arc::new(drawio_agent_renderer::Renderer::new(Arc::new(driver)));
        self.renderer = Some(r.clone());
        Ok(r)
    }

    fn check_nudge(&mut self, op: &str, ids: &[String]) -> Result<(), String> {
        let mut sorted = ids.to_vec();
        sorted.sort();
        sorted.dedup();
        let cnt = self
            .nudge_counts
            .entry(format!("{op}:{}", sorted.join(",")))
            .or_insert(0);
        if *cnt >= 2 {
            return Err(format!(
                "同一组节点（{}）已连续 layout {} 次仍未收敛——小步平移解决不了问题，继续只会烧轮次。换一种做法：\
                 1) 用 check 看警告明细，用 edit 给边加拐点（<Array as=\"points\"> 放 mxGeometry 内）或改锚点比例；\
                 2) 重排节点顺序（edit 调整或 draw 重画）；\
                 3) 接受残余警告并总结收尾。残余轻微交叉是可接受的。",
                sorted.join(","),
                *cnt
            ));
        }
        *cnt += 1;
        Ok(())
    }

    async fn layout(&mut self, doc: &mut XmlDoc, args: &Value) -> Result<ToolOutput, String> {
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        // libavoid 失败回退时给模型/用户的可见原因（此前只进 stderr，
        // web 会话里完全不可见，用户无从知道为何等了很久又没走 libavoid）
        let mut fallback_note: Option<String> = None;
        // 参与本批移动/对齐的 cell：成功后回报结果坐标，闭环不用重读。
        let mut moved_ids: Vec<String> = Vec::new();
        if let Some(m) = args.get("move") {
            // 三种形态：统一偏移 {"ids":[...], "dx":n, "dy":n}
            //         逐项相对 [{"id":..,"dx":..,"dy":..}, ...]
            //         逐项绝对 [{"id":..,"x":..,"y":..}, ...]（x/y 给哪个改哪个）
            let items: Vec<(String, MoveSpec)> = if let Some(arr) = m.as_array() {
                arr.iter()
                    .map(|v| {
                        let id = v
                            .get("id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| "move 数组项需要 id".to_string())?;
                        let has_delta = v.get("dx").is_some() || v.get("dy").is_some();
                        let has_place = v.get("x").is_some() || v.get("y").is_some();
                        let spec = match (has_delta, has_place) {
                            (true, true) => {
                                return Err("move 单项不能同时给 dx/dy 与 x/y".to_string())
                            }
                            (true, false) => MoveSpec::Delta(
                                v.get("dx").and_then(Value::as_f64).unwrap_or(0.0),
                                v.get("dy").and_then(Value::as_f64).unwrap_or(0.0),
                            ),
                            (false, true) => MoveSpec::Place(
                                v.get("x").and_then(Value::as_f64),
                                v.get("y").and_then(Value::as_f64),
                            ),
                            (false, false) => {
                                return Err(
                                    "move 数组项需要 dx/dy（相对偏移）或 x/y（绝对定位）"
                                        .to_string(),
                                )
                            }
                        };
                        Ok((id.to_string(), spec))
                    })
                    .collect::<Result<Vec<_>, String>>()?
            } else {
                let ids: Vec<String> = m
                    .get("ids")
                    .and_then(Value::as_array)
                    .ok_or_else(|| "move 需要 ids 数组或 [{id,dx,dy},...] 数组".to_string())?
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
                let dx = m.get("dx").and_then(Value::as_f64).unwrap_or(0.0);
                let dy = m.get("dy").and_then(Value::as_f64).unwrap_or(0.0);
                ids.into_iter()
                    .map(|id| (id, MoveSpec::Delta(dx, dy)))
                    .collect()
            };
            if items.is_empty() {
                return Err("move 不能为空".to_string());
            }
            let ids_for_key: Vec<String> = items.iter().map(|(id, _)| id.clone()).collect();
            self.check_nudge("move", &ids_for_key)?;
            for (id, spec) in &items {
                let (x, y, w, h) = match doc.geometry_of(id) {
                    Some(g) => g,
                    None => {
                        // 存在但几何不可读 → 多半是边（无绝对坐标），给
                        // 可执行的替代方案而不是含混的「不可读」
                        let is_edge = doc.id_to_cell(id).is_some_and(|c| {
                            let slice =
                                lines_in(doc.canonical(), c.start_line, c.end_line);
                            attr_value(&slice, "edge").as_deref() == Some("1")
                        });
                        if is_edge {
                            return Err(format!(
                                "`{id}` 是边（edge），没有绝对几何，不能参与 layout。\
                                 调整边路由请用 edit：改锚点（exitX/entryY 全给 0..1 比例）\
                                 或加拐点 <Array as=\"points\"> 放 mxGeometry 内"
                            ));
                        }
                        return Err(format!("cell `{id}` 不存在或其几何不可读"));
                    }
                };
                let (nx, ny) = match spec {
                    MoveSpec::Delta(dx, dy) => (x + dx, y + dy),
                    MoveSpec::Place(px, py) => (px.unwrap_or(x), py.unwrap_or(y)),
                };
                if let Some((ln, line)) = doc
                    .set_geometry_line(id, nx, ny, w, h)
                    .map_err(|e| format!("{e}"))?
                {
                    edits.push((ln, ln, line));
                }
                moved_ids.push(id.clone());
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
            self.check_nudge("align", &ids)?;
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
                    sorted.sort_by(|a, b| a.1 .0.partial_cmp(&b.1 .0).expect("f64 comparable"));
                } else {
                    sorted.sort_by(|a, b| a.1 .1.partial_cmp(&b.1 .1).expect("f64 comparable"));
                }
                let total_len: f64 = if horizontal {
                    sorted.iter().map(|(_, g)| g.2).sum()
                } else {
                    sorted.iter().map(|(_, g)| g.3).sum()
                };
                let span = if horizontal {
                    sorted.last().expect("sorted non-empty").1 .0 + sorted.last().unwrap().1 .2 - sorted.first().unwrap().1 .0
                } else {
                    sorted.last().expect("sorted non-empty").1 .1 + sorted.last().unwrap().1 .3 - sorted.first().unwrap().1 .1
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
                moved_ids.push(id.clone());
            }
        } else if let Some(r) = args.get("route") {
            // 正交化路由。首选 libavoid（drawio 内置 WASM 避障路由器，
            // 与编辑器「布局-正交布线」同一实现，走 headless 渲染页面
            // 求解）；渲染器不可用/失败时回退到确定性候选路径路由。
            // ids 先校验（存在且为边）；libavoid 全图求解，ids 仅用于
            // 让模型表达意图与先期报错。
            // ids 可选：缺省 = 全图所有边（libavoid 本就是全图求解）；
            // 提供时先校验（存在且为边），拼错立即报错而不是静默忽略。
            let ids: Vec<String> = r
                .get("ids")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            if !ids.is_empty() {
                for id in &ids {
                    let bad = match doc.id_to_cell(id) {
                        None => Some("不存在".to_string()),
                        Some(span) => {
                            let slice =
                                lines_in(doc.canonical(), span.start_line, span.end_line);
                            let is_edge = slice
                                .lines()
                                .find(|l| l.contains("<mxCell"))
                                .and_then(|l| attr_value(l, "edge"))
                                .as_deref()
                                == Some("1");
                            (!is_edge).then(|| "不是边（edge）".to_string())
                        }
                    };
                    if let Some(reason) = bad {
                        return Err(format!("`{id}` {reason}，route 只作用于边"));
                    }
                }
            }
            if self.render {
                if let Ok(renderer) = self.ensure_renderer().await {
                    match renderer.reroute(doc.canonical()).await {
                        Ok(new_xml) => {
                            let (start, end) = (1usize, total_lines(doc.canonical()));
                            return match doc.apply_edit(start, end, &new_xml) {
                                Ok(report) if report.noop => Ok(ToolOutput::text(
                                    "no-op：libavoid 布线后无变化，布局已是正交且无遮挡。",
                                )),
                                Ok(report) => {
                                    doc.save().map_err(|e| format!("保存失败: {e}"))?;
                                    self.edit_streak = 0;
                                    self.last_edit_key = None;
                                    self.nudge_counts.clear();
                                    Ok(ToolOutput::text(format!(
                                        "已应用 libavoid 避障布线并保存。{}",
                                        report_summary(&report)
                                    )))
                                }
                                Err(e) => Err(format!(
                                    "libavoid 布线结果被拒绝（文件未改动）: {e}\n{}",
                                    reject_hint(&e)
                                )),
                            };
                        }
                        Err(e) => {
                            eprintln!("libavoid reroute 不可用，回退确定性路由: {e}");
                            fallback_note = Some(format!(
                                "（libavoid 不可用，已回退确定性路由：{e}）"
                            ));
                        }
                    }
                }
            }
            // 确定性兜底：候选路径逐段与节点矩形求交（无浏览器时可用）。
            let (geom, _page) = crate::metrics::parse_geom(doc.canonical())
                .map_err(|e| format!("布局分析失败: {e}"))?;
            let rect_of = |id: &str| -> Option<(f64, f64, f64, f64)> {
                geom
                    .iter()
                    .find(|c| c.id == id && !c.is_edge && c.w > 0.0)
                    .map(|c| (c.x, c.y, c.w, c.h))
            };
            // 障碍 = 除源/目标及其祖先链外的**叶子**顶点。泳道等容器
            // 不入障碍（跨泳道走线合法；否则容器矩形挡死一切候选路径，
            // 实测跨泳道边全部无解 → route 永远 no-op）。
            let containers: std::collections::HashSet<&str> = geom
                .iter()
                .filter(|c| !c.is_edge)
                .map(|c| c.parent.as_str())
                .filter(|p| *p != "0" && *p != "1")
                .collect();
            let obstacles_for = |src: &str, dst: &str| -> Vec<(f64, f64, f64, f64)> {
                geom
                    .iter()
                    .filter(|c| !c.is_edge && c.w > 0.0 && c.id != src && c.id != dst)
                    .filter(|c| !containers.contains(c.id.as_str()))
                    .filter(|c| {
                        !crate::metrics::is_ancestor(&c.id, src, &geom)
                            && !crate::metrics::is_ancestor(&c.id, dst, &geom)
                    })
                    .map(|c| (c.x, c.y, c.w, c.h))
                    .collect()
            };
            // 兜底缺省同样作用于全图
            let ids = if ids.is_empty() {
                geom.iter()
                    .filter(|c| c.is_edge)
                    .map(|c| c.id.clone())
                    .collect()
            } else {
                ids
            };
            for id in &ids {
                let span = doc
                    .id_to_cell(id)
                    .ok_or_else(|| format!("cell `{id}` 不存在"))?;
                let slice = lines_in(doc.canonical(), span.start_line, span.end_line);
                let cell_line = slice
                    .lines()
                    .find(|l| l.contains("<mxCell"))
                    .ok_or_else(|| format!("cell `{id}` 无法解析"))?;
                if attr_value(cell_line, "edge").as_deref() != Some("1") {
                    return Err(format!("`{id}` 不是边（edge），route 只作用于边"));
                }
                // 样式正交化（幂等）
                let is_ortho = cell_line.contains("edgeStyle=orthogonalEdgeStyle");
                if !is_ortho {
                    let new_line = if cell_line.contains("style=\"") {
                        cell_line.replacen(
                            "style=\"",
                            "style=\"edgeStyle=orthogonalEdgeStyle;",
                            1,
                        )
                    } else {
                        cell_line.replacen(
                            " edge=\"1\"",
                            " style=\"edgeStyle=orthogonalEdgeStyle;\" edge=\"1\"",
                            1,
                        )
                    };
                    let ln = span.start_line
                        + slice.lines().position(|l| l == cell_line).unwrap_or(0);
                    edits.push((ln, ln, new_line));
                    moved_ids.push(id.clone());
                }
                // 避障拐点：已有拐点的边尊重手动路由不动；两端必须是
                // 可读几何的顶点；mxGeometry 已有子元素时不安全重写，跳过
                let edge_geom = geom.iter().find(|c| c.id == *id);
                let already_routed = edge_geom.is_some_and(|e| !e.points.is_empty());
                let geo_line = slice
                    .lines()
                    .find(|l| l.contains("<mxGeometry"))
                    .unwrap_or("");
                if !already_routed {
                    if let (Some(e), Some(src), Some(dst)) = (
                        edge_geom,
                        e_src_tgt(edge_geom, 0).and_then(|s| rect_of(&s)),
                        e_src_tgt(edge_geom, 1).and_then(|d| rect_of(&d)),
                    ) {
                        let (s_id, d_id) = (
                            e.source.clone().unwrap_or_default(),
                            e.target.clone().unwrap_or_default(),
                        );
                        let bends = route_waypoints(
                            src,
                            dst,
                            &obstacles_for(&s_id, &d_id),
                            !is_ortho, // 本次被正交化的边必须钉死拐点
                        );
                        if !bends.is_empty()
                            && geo_line.trim_end().ends_with("/>")
                        {
                            let indent =
                                geo_line.len() - geo_line.trim_start().len();
                            let pad = |n: usize| " ".repeat(n);
                            let mut repl = format!(
                                "{}<mxGeometry relative=\"1\" as=\"geometry\">",
                                pad(indent)
                            );
                            repl.push_str(&format!(
                                "\n{}<Array as=\"points\">",
                                pad(indent + 1)
                            ));
                            for (x, y) in &bends {
                                repl.push_str(&format!(
                                    "\n{}<mxPoint x=\"{}\" y=\"{}\"/>",
                                    pad(indent + 2),
                                    fmt_coord(*x),
                                    fmt_coord(*y)
                                ));
                            }
                            repl.push_str(&format!("\n{}</Array>", pad(indent + 1)));
                            repl.push_str(&format!(
                                "\n{}</mxGeometry>",
                                pad(indent)
                            ));
                            let ln = span.start_line
                                + slice
                                    .lines()
                                    .position(|l| l.contains("<mxGeometry"))
                                    .unwrap_or(0);
                            edits.push((ln, ln, repl));
                        }
                    }
                }
            }
        } else {
            return Err("layout 需要 move / align / route 参数".to_string());
        }
        if edits.is_empty() {
            // 回显目标 cell 的当前坐标：把「目标==现状」从抽象结论变成
            // 可对照的数据——模型没有别的途径观测坐标，抽象纠错对弱模型
            // 无效，E2E 实测会复读同一 no-op 调用直到预算烧尽。
            let mut cur = Vec::new();
            for id in moved_ids.iter().take(8) {
                if let Some((x, y, _, _)) = doc.geometry_of(id) {
                    cur.push(format!("{}=({},{})", id, fmt_coord(x), fmt_coord(y)));
                }
            }
            let geo = if cur.is_empty() {
                String::new()
            } else {
                format!(" 当前坐标: {}", cur.join(" "))
            };
            return Ok(ToolOutput::text(format!(
                "no-op：目标坐标与现状一致，几何未变。{geo}\
                 若仍想移动，请给出与当前不同的坐标；若布局已满意，请继续下一步。"
            )));
        }
        match doc.apply_edits(&edits) {
            Ok(report) if report.noop => Ok(ToolOutput::text(
                "no-op：改动后的内容与现状一致，文件未变。若仍想修改，请给出与当前不同的\
                 内容；若已满意，请继续下一步（用 {\"reply\": …, \"done\": true} 收尾）。",
            )),
            Ok(report) => {
                doc.save().map_err(|e| format!("保存失败: {e}"))?;
                // 结果坐标直接回报：模型不用再 read 确认（省一轮）。
                let coords: Vec<String> = moved_ids
                    .iter()
                    .filter(|id| report.changed.iter().any(|c| c == *id))
                    .filter_map(|id| {
                        doc.geometry_of(id)
                            .map(|(x, y, _, _)| format!("{}→({},{})", id, fmt_coord(x), fmt_coord(y)))
                    })
                    .collect();
                let note = if coords.is_empty() {
                    String::new()
                } else {
                    format!(" 新坐标: {}", coords.join(" "))
                };
                Ok(ToolOutput::text(format!(
                    "布局已应用并保存。{}{}{}",
                    report_summary(&report),
                    note,
                    fallback_note.as_deref().unwrap_or("")
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
            !doc.cells().iter().any(|o| {
                o.id != c.id && o.start_line > c.start_line && o.start_line <= c.end_line
            })
        };
        let mut hits: Vec<String> = Vec::new();
        for c in doc.cells().iter().filter(|c| is_leaf(c)) {
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
                // 几何坐标一并返回：模型规划 layout/对齐时必需，缺了会
                // 诱发反复 query 找坐标的空转（实测 30 轮烧尽的根因）。
                // 只在 mxGeometry 行上取值——attr_value 是朴素子串匹配，
                // 在整块文本上取 `x` 会命中 vertex="1" 里的 x="1"。
                let geo_line = slice.lines().find(|l| l.contains("<mxGeometry")).unwrap_or("");
                let geo = match (
                    attr_value(geo_line, "x").and_then(|v| v.parse::<f64>().ok()),
                    attr_value(geo_line, "y").and_then(|v| v.parse::<f64>().ok()),
                    attr_value(geo_line, "width").and_then(|v| v.parse::<f64>().ok()),
                    attr_value(geo_line, "height").and_then(|v| v.parse::<f64>().ok()),
                ) {
                    (Some(x), Some(y), Some(w), Some(h)) if c.tag == "mxCell" => {
                        format!(" geo={x},{y} {w}x{h}")
                    }
                    _ => String::new(),
                };
                let mut s = format!(
                    "cell `{}` @{}-{}  value={:?}{}{}{}",
                    c.id,
                    c.start_line,
                    c.end_line,
                    truncate(&value, 40),
                    if style.is_empty() { String::new() } else { format!(" style={:?}", truncate(&style, 30)) },
                    geo,
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
                doc.cells().len()
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
            let xml = wrap_bare_graph_model(xml);
            let (start, end) = (1usize, total_lines(doc.canonical()));
            return match doc.apply_edit(start, end, &xml) {
                Ok(report) if report.noop => {
                    Ok(ToolOutput::text(
                        "no-op：替换后的内容与现状一致，文件未变。若仍想修改，请给出与当前不同的\
                         内容；若已满意，请继续下一步（用 {\"reply\": …, \"done\": true} 收尾）。",
                    ))
                }
                Ok(report) => {
                    doc.save().map_err(|e| format!("保存失败: {e}"))?;
                    // draw = 整图重画，新内容：同组 edit/layout 计数全部重置
                    self.edit_streak = 0;
                    self.last_edit_key = None;
                    self.nudge_counts.clear();
                    Ok(ToolOutput::text(format!(
                        "编辑已应用并保存。{}",
                        report_summary(&report)
                    )))
                }
                Err(e) => Err(format!("draw 被拒绝（文件未改动）: {e}\n{}", reject_hint(&e))),
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

        // 同组连续 edit 守卫（放宽版）：连续触达同一批 cell 的**成功** edit
        // 计数，第 4 次拒绝。失败的 edit（解析错误→修正重试同区间）不计数，
        // 属于合法恢复。锚点/样式的视觉微调循环每次参数都不同，只有「触达
        // 同一批 cell」这个特征稳定；拒绝后状态保留（不重置），持续复读
        // 会被 turn_loop 的无进展连击终止。
        let touched: Vec<String> = {
            let mut ids: Vec<String> = doc
                .cells()
                .iter()
                .filter(|c| {
                    batch
                        .iter()
                        .any(|&(s, e, _)| !(c.end_line < s || c.start_line > e))
                })
                .map(|c| c.id.clone())
                .collect();
            ids.sort();
            ids.dedup();
            ids
        };
        let key = touched.join(",");
        let same_as_last = self.last_edit_key.as_deref() == Some(key.as_str());
        if same_as_last && self.edit_streak >= 3 {
            return Err(format!(
                "同一组 cell（{}）已连续成功 edit {} 次——别再对同一处做第 4 次微调，继续只会烧轮次。换一种做法：\
                 1) 接受现状，用 reply 收尾（残余视觉瑕疵是可接受的）；\
                 2) 对边路由问题改锚点组合（exitX/entryY 全部给 0..1 比例）或加拐点 <Array as=\"points\">；\
                 3) 该区域实在不满意就 draw 重画。注意：本次调用未执行。",
                touched.join(","),
                self.edit_streak
            ));
        }

        // 全或无：内存内一次应用 + 单次校验，通过才原子落盘
        match doc.apply_edits(&batch) {
            Ok(report) if report.noop => Ok(ToolOutput::text(
                "no-op：替换后的内容与现状一致，文件未变。若仍想修改，请给出与当前不同的\
                 内容；若已满意，请继续下一步（用 {\"reply\": …, \"done\": true} 收尾）。",
            )),
            Ok(report) => {
                doc.save().map_err(|e| format!("保存失败: {e}"))?;
                // 成功的非 no-op edit 才推进同组计数（失败的 edit 是合法恢复）
                if same_as_last {
                    self.edit_streak += 1;
                } else {
                    self.edit_streak = 1;
                    self.last_edit_key = Some(key);
                }
                // 内容变了：同组微调计数重置（edit 后重新布局是合法的新策略）
                self.nudge_counts.clear();
                Ok(ToolOutput::text(format!(
                    "编辑已应用并保存。{}",
                    report_summary(&report)
                )))
            }
            Err(e) => Err(format!(
                "编辑被拒绝（文件未改动）: {e}\n{}",
                reject_hint(&e)
            )),
        }
    }

    pub fn check(&self, doc: &XmlDoc) -> Result<ToolOutput, String> {
        let mut text = match check_doc(doc.canonical()) {
            Ok(r) => r.summarize(),
            Err(e) => return Err(format!("校验失败: {e}")),
        };
        // 布局 lint 摘要：确定性几何分析（重叠/交叉/溢出/越界/分支平行），
        // 只附加 warning 段——error 类的断引用上面结构校验已报过。
        // analyze 失败时静默跳过（结构校验已覆盖可解析性）。
        if let Ok(rep) = crate::metrics::analyze(doc.canonical()) {
            text.push('\n');
            text.push_str(&crate::metrics::lint_summary_text(&rep));
        }
        Ok(ToolOutput::text(text))
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
        // 同内容缓存（仅模型路径；人工 open 显式要求渲染）：文件与参数都
        // 未变时直接回文本，不重渲染、不重发图——上一张截图还在模型
        // 上下文里，重发只是重复 token。判定在 chromium 懒启动之前。
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        doc.canonical().hash(&mut hasher);
        opts.annotate.hash(&mut hasher);
        opts.focus.hash(&mut hasher);
        let view_hash = hasher.finish();
        if !open && self.last_view == Some(view_hash) {
            return Ok(ToolOutput::text(
                "文件自上次 view 以来未变化：截图与上下文里的上一张相同，无需重复查看。\
                 要看局部细节可加 focus 参数；改动后再 view 会得到新图。",
            ));
        }
        let renderer = self.ensure_renderer().await?;
        match renderer.render(doc.canonical(), &opts).await {
            Ok(png) => {
                if !open {
                    self.last_view = Some(view_hash);
                }
                // 截图 + 确定性 lint 同屏：模型视觉对重叠/交叉不可靠，
                // 用一行确定性结论兜底（详见 metrics::lint_one_liner）。
                let lint = crate::metrics::analyze(doc.canonical())
                    .map(|r| crate::metrics::lint_one_liner(&r))
                    .unwrap_or_default();
                let mut text = if open {
                    let stem = file_stem(doc).replace(".xml", "").replace(".drawio", "");
                    let png_path = doc
                        .path
                        .parent()
                        .unwrap_or(std::path::Path::new("."))
                        .join(format!("{stem}.png"));
                    if std::fs::write(&png_path, &png).is_ok() {
                        open_with_system_viewer(&png_path);
                        format!("已渲染并打开 {} ({}, cells={})", png_path.display(), png.len(), doc.cells().len())
                    } else {
                        format!("渲染成功 ({} bytes, cells={})", png.len(), doc.cells().len())
                    }
                } else {
                    format!("渲染成功 ({} bytes, cells={})", png.len(), doc.cells().len())
                };
                if !lint.is_empty() {
                    text.push('\n');
                    text.push_str(&lint);
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

/// route 的确定性避障拐点计算：候选路径（直连 → L 形 → Z 形 → 绕侧
/// 通道）逐段与障碍矩形（全部非源/目标的顶点，含泳道容器，外扩安全
/// 边距）求交，选第一条无碰撞路径的拐点；全部被堵则返回空——交由
/// drawio 渲染器的正交路由兜底（渲染器无真避障）。
fn route_waypoints(
    src: (f64, f64, f64, f64),
    dst: (f64, f64, f64, f64),
    obstacles: &[(f64, f64, f64, f64)],
    allow_direct: bool,
) -> Vec<(f64, f64)> {
    const MARGIN: f64 = 15.0;
    let sc = (src.0 + src.2 / 2.0, src.1 + src.3 / 2.0);
    let tc = (dst.0 + dst.2 / 2.0, dst.1 + dst.3 / 2.0);
    let inflated: Vec<(f64, f64, f64, f64)> = obstacles
        .iter()
        .map(|&(x, y, w, h)| (x - MARGIN, y - MARGIN, w + 2.0 * MARGIN, h + 2.0 * MARGIN))
        .collect();
    let seg_clear = |p: (f64, f64), q: (f64, f64)| {
        !inflated
            .iter()
            .any(|&r| crate::metrics::seg_rect_intersect(p, q, r))
    };
    let path_ok = |bends: &[(f64, f64)]| -> bool {
        let mut pts = vec![sc];
        pts.extend(bends.iter().copied());
        pts.push(tc);
        pts.windows(2).all(|w| seg_clear(w[0], w[1]))
    };
    let mx = (sc.0 + tc.0) / 2.0;
    let my = (sc.1 + tc.1) / 2.0;
    let (min_x, max_x) = inflated
        .iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), r| {
            (lo.min(r.0), hi.max(r.0 + r.2))
        });
    let (min_y, max_y) = inflated
        .iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), r| {
            (lo.min(r.1), hi.max(r.1 + r.3))
        });
    let mut candidates: Vec<Vec<(f64, f64)>> = vec![
        vec![(sc.0, tc.1)],                               // L：先竖后横
        vec![(tc.0, sc.1)],                               // L：先横后竖
        vec![(sc.0, my), (tc.0, my)],                     // Z：经水平中线
        vec![(mx, sc.1), (mx, tc.1)],                     // Z：经垂直中线
        vec![(sc.0, min_y - 60.0), (tc.0, min_y - 60.0)], // 绕上方
        vec![(sc.0, max_y + 60.0), (tc.0, max_y + 60.0)], // 绕下方
        vec![(max_x + 50.0, sc.1), (max_x + 50.0, tc.1)], // 绕右侧通道
        vec![(min_x - 50.0, sc.1), (min_x - 50.0, tc.1)], // 绕左侧通道
    ];
    // 直连候选仅在两种情况下有效：普通直线边（渲染器会按直线画）；
    // 或端点轴对齐（正交渲染也是直线）。正交边且两端错位时渲染器会
    // 自选拐弯（可能拐进节点）——必须显式钉死拐点，不能留空。
    if allow_direct || sc.0 == tc.0 || sc.1 == tc.1 {
        candidates.insert(0, vec![]);
    }
    candidates
        .into_iter()
        .find(|b| path_ok(b))
        .unwrap_or_default()
}

/// 从 GeomCell 提取 source/target id（idx=0 取 source，1 取 target）。
fn e_src_tgt(e: Option<&crate::metrics::GeomCell>, idx: usize) -> Option<String> {
    let e = e?;
    match idx {
        0 => e.source.clone(),
        _ => e.target.clone(),
    }
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

/// draw 的宽容输入：mxfile/diagram 两层包裹是纯样板、对语义无贡献，
/// 而模型经常只给 mxGraphModel。根元素恰为 mxGraphModel 时自动补全外壳；
/// 其余形态原样交给 canonicalize 校验。
fn wrap_bare_graph_model(xml: &str) -> String {
    let t = xml.trim();
    if t.starts_with("<mxGraphModel") && t.ends_with("</mxGraphModel>") {
        format!(
            "<mxfile host=\"app.diagrams.net\"><diagram id=\"page-1\" name=\"Page-1\">{t}</diagram></mxfile>"
        )
    } else {
        xml.to_string()
    }
}

/// 编辑被拒绝时的教学提示：解析失败（标签不配对是最常见死因）给整文件
/// 骨架；其余（断引用等）给单 cell 正确形态——错误体本身已指明具体问题。
fn reject_hint(e: &XmlError) -> &'static str {
    match e {
        XmlError::Xml(_) | XmlError::BlobDecode(_) => concat!(
            "XML 解析失败：开闭标签必须逐层配对。整文件四层结构：\n",
            "<mxfile><diagram id=\"d\" name=\"Page-1\"><mxGraphModel><root>…</root></mxGraphModel></diagram></mxfile>\n",
            "常见死因：漏写 </mxGraphModel> 或 </diagram>；逐层数一遍闭合。"
        ),
        _ => concat!(
            "正确形态示例（单 cell 自洽 XML，含完整属性）：\n",
            "<mxCell id=\"新id\" value=\"标签\" vertex=\"1\" parent=\"1\">",
            "<mxGeometry x=\"40\" y=\"60\" width=\"120\" height=\"60\" as=\"geometry\"/></mxCell>\n",
            "连线需 vertex→edge：source/target=已有 cell id、父级 parent=\"1\"。"
        ),
    }
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
    async fn view_skips_render_when_doc_unchanged() {
        let mock = drawio_agent_renderer::MockDriver::new();
        let renderer =
            drawio_agent_renderer::Renderer::new(std::sync::Arc::new(mock.clone()));
        let mut tools = Tools::with_renderer(renderer);
        let d = doc();
        // 第一次 view：正常渲染。
        let out1 = tools.view(&d, &serde_json::json!({})).await.unwrap();
        assert!(out1.image_png.is_some(), "首次 view 应带图");
        assert_eq!(mock.calls().len(), 1);
        // 同内容重复 view：只回文本，不重渲染、不重发图。
        let out2 = tools.view(&d, &serde_json::json!({})).await.unwrap();
        assert!(out2.image_png.is_none(), "未变化时不应重发图");
        assert!(out2.text.contains("未变化"), "{}", out2.text);
        assert_eq!(mock.calls().len(), 1, "不应重复渲染");
        // 参数不同（annotate）→ 重新渲染。
        let out3 = tools.view(&d, &serde_json::json!({ "annotate": true })).await.unwrap();
        assert!(out3.image_png.is_some());
        assert_eq!(mock.calls().len(), 2);
        // 文件内容变化 → 重新渲染。
        let d2 = two_cell_doc();
        let out4 = tools.view(&d2, &serde_json::json!({})).await.unwrap();
        assert!(out4.image_png.is_some());
        assert_eq!(mock.calls().len(), 3);
    }

    #[tokio::test]
    async fn check_reports_structural_and_layout_lint() {
        // 重叠的两个节点：check 应同时含结构摘要与布局 lint 警告段。
        let xml = r#"<mxfile host="app.diagrams.net"><diagram id="d1"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="a" value="A" vertex="1" parent="1"><mxGeometry x="40" y="60" width="160" height="60" as="geometry"/></mxCell><mxCell id="b" value="B" vertex="1" parent="1"><mxGeometry x="100" y="70" width="160" height="60" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;
        let mut d = XmlDoc::from_text(xml).unwrap();
        let mut t = Tools::new(false);
        let out = t.run(&mut d, "check", &serde_json::json!({})).await.unwrap();
        assert!(out.text.contains("issues=0"), "结构应无问题: {}", out.text);
        assert!(out.text.contains("布局 lint"), "{}", out.text);
        assert!(
            out.text.contains("[warning:overlap] a, b"),
            "应列出重叠警告对: {}",
            out.text
        );
        // 干净布局：lint 段无警告条目。
        let mut clean = two_cell_doc();
        let out2 = t.run(&mut clean, "check", &serde_json::json!({})).await.unwrap();
        assert!(out2.text.contains("未触发重叠/交叉/溢出/越界"), "{}", out2.text);
        assert!(!out2.text.contains("[warning:"), "{}", out2.text);
        // 断引用仍由结构校验报告（error 不在 lint 段重复）。
        let broken = r#"<mxfile><diagram id="d1"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="e1" edge="1" parent="1" source="ghost" target="a"><mxGeometry relative="1" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;
        let mut bd = XmlDoc::from_text(broken).unwrap();
        let out3 = t.run(&mut bd, "check", &serde_json::json!({})).await.unwrap();
        assert!(out3.text.contains("missing source cell `ghost`"), "{}", out3.text);
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
    async fn layout_move_place_absolute_coordinates() {
        let mut d = two_cell_doc();
        let mut t = Tools::new(false);
        let out = t
            .run(
                &mut d,
                "layout",
                &serde_json::json!({
                    "move": [{"id": "svc-a", "x": 400, "y": 200}]
                }),
            )
            .await
            .unwrap();
        let ga = d.geometry_of("svc-a").unwrap();
        assert_eq!((ga.0, ga.1), (400.0, 200.0), "绝对定位直达");
        // 报告带结果坐标，闭环不用重读
        assert!(out.text.contains("svc-a→(400,200)"), "{}", out.text);
    }

    #[tokio::test]
    async fn layout_move_place_partial_axis_keeps_other() {
        let mut d = two_cell_doc();
        let mut t = Tools::new(false);
        t.run(&mut d, "layout", &serde_json::json!({
            "move": [{"id": "svc-a", "x": 500}]
        })).await.unwrap();
        let ga = d.geometry_of("svc-a").unwrap();
        assert_eq!(ga.0, 500.0);
        assert_eq!(ga.1, 60.0, "未给的轴保持不变");
    }

    #[tokio::test]
    async fn layout_move_mixed_forms_and_rejects_conflict() {
        let mut d = two_cell_doc();
        let mut t = Tools::new(false);
        // 绝对与相对可混用
        t.run(&mut d, "layout", &serde_json::json!({
            "move": [
                {"id": "svc-a", "x": 300, "y": 100},
                {"id": "svc-b", "dx": 40, "dy": 0}
            ]
        })).await.unwrap();
        assert_eq!(d.geometry_of("svc-a").unwrap().0, 300.0);
        assert_eq!(d.geometry_of("svc-b").unwrap().0, 300.0);
        // 同一项同时给 dx 与 x → 报错且文件不动
        let before = d.canonical().to_string();
        let err = t.run(&mut d, "layout", &serde_json::json!({
            "move": [{"id": "svc-a", "x": 1, "dx": 2}]
        })).await.unwrap_err();
        assert!(err.contains("同时"), "{err}");
        assert_eq!(d.canonical(), before);
        // 什么都没给 → 明确报错
        let err2 = t.run(&mut d, "layout", &serde_json::json!({
            "move": [{"id": "svc-a"}]
        })).await.unwrap_err();
        assert!(err2.contains("dx/dy"), "{err2}");
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
    async fn layout_move_array_supports_per_cell_offsets() {
        let mut d = two_cell_doc();
        let mut t = Tools::new(false);
        let out = t.run(&mut d, "layout", &serde_json::json!({
            "move": [
                {"id": "svc-a", "dx": 10, "dy": 5},
                {"id": "svc-b", "dx": -30, "dy": 0}
            ]
        })).await.unwrap();
        assert!(out.text.contains("changed=[svc-a, svc-b]"), "{}", out.text);
        let ga = d.geometry_of("svc-a").unwrap();
        let gb = d.geometry_of("svc-b").unwrap();
        assert_eq!((ga.0, ga.1), (50.0, 65.0), "a 按自己的偏移");
        assert_eq!((gb.0, gb.1), (230.0, 60.0), "b 按自己的偏移");
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
        t.run(&mut d, "layout", &serde_json::json!({
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
    async fn draw_accepts_bare_mxgraphmodel_and_wraps() {
        let mut d = doc();
        let mut t = Tools::new(false);
        let xml = r#"<mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="n1" value="Bare" vertex="1" parent="1"><mxGeometry x="0" y="0" width="100" height="50" as="geometry"/></mxCell></root></mxGraphModel>"#;
        let out = t.run(&mut d, "draw", &serde_json::json!({ "xml": xml })).await.unwrap();
        assert!(out.text.contains("added=[n1]"), "{}", out.text);
        assert!(
            d.canonical().starts_with("<mxfile"),
            "裸 mxGraphModel 应被自动包上 mxfile 外壳: {}",
            &d.canonical()[..80]
        );
        assert!(d.canonical().contains("diagram id=\"page-1\""));
    }

    #[tokio::test]
    async fn draw_parse_error_teaches_skeleton() {
        let mut d = doc();
        let before = d.canonical().to_string();
        let mut t = Tools::new(false);
        // 漏写 </mxGraphModel>——模型最常见的标签配对死因（线上实测）
        let xml = r#"<mxfile><diagram id="d2"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/></root></diagram></mxfile>"#;
        let err = t
            .run(&mut d, "draw", &serde_json::json!({ "xml": xml }))
            .await
            .unwrap_err();
        assert!(err.contains("</mxGraphModel>"), "拒绝信息应教四层骨架: {err}");
        assert_eq!(d.canonical(), before);
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
    async fn view_appends_layout_lint_one_liner() {
        // 两个重叠的盒子：渲染 mock 不做几何分析，lint 必须补上确定性结论
        let xml = r#"<mxfile host="x"><diagram id="d"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="a" value="A" vertex="1" parent="1"><mxGeometry x="40" y="60" width="160" height="60" as="geometry"/></mxCell><mxCell id="b" value="B" vertex="1" parent="1"><mxGeometry x="120" y="80" width="160" height="60" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;
        let d = XmlDoc::from_text(xml).unwrap();
        let mock = drawio_agent_renderer::MockDriver::new().with_bytes(vec![0x89, b'P', b'N', b'G']);
        let renderer = drawio_agent_renderer::Renderer::new(std::sync::Arc::new(mock));
        let mut tools = Tools::with_renderer(renderer);
        let out = tools.view(&d, &serde_json::json!({})).await.unwrap();
        assert!(out.text.contains("布局 lint：1 条警告（重叠 1"), "{}", out.text);
        assert!(out.text.contains("不要为清零反复微调"), "{}", out.text);
    }

    #[tokio::test]
    async fn layout_nudge_guard_rejects_third_same_group_call() {
        let mut d = two_cell_doc();
        let mut t = Tools::new(false);
        // 同组 [svc-a] 两次放行（参数每次不同也没用），第 3 次拒绝并给策略
        for x in [100.0, 140.0] {
            t.run(&mut d, "layout", &serde_json::json!({
                "move": [{"id": "svc-a", "x": x, "y": 60}]
            })).await.unwrap();
        }
        let err = t
            .run(&mut d, "layout", &serde_json::json!({
                "move": [{"id": "svc-a", "x": 180.0, "y": 60}]
            }))
            .await
            .unwrap_err();
        assert!(err.contains("仍未收敛"), "{err}");
        assert!(err.contains("拐点"), "拒绝信息应给替代策略: {err}");
        // 不同组不受影响
        t.run(&mut d, "layout", &serde_json::json!({
            "move": [{"id": "svc-b", "x": 400, "y": 60}]
        })).await.unwrap();
        // 成功 edit 后计数重置：同组可再次 layout
        let edit_env = serde_json::json!({
            "range": "cell:svc-a",
            "text": r#"<mxCell id="svc-a" value="A2" vertex="1" parent="1"><mxGeometry x="100" y="60" width="160" height="60" as="geometry"/></mxCell>"#
        });
        t.run(&mut d, "edit", &edit_env).await.unwrap();
        t.run(&mut d, "layout", &serde_json::json!({
            "move": [{"id": "svc-a", "x": 60, "y": 60}]
        })).await.unwrap();
    }

    #[tokio::test]
    async fn edit_same_group_guard_rejects_fourth() {
        let mut d = two_cell_doc();
        let mut t = Tools::new(false);
        let mk = |v: String| {
            serde_json::json!({
                "range": "cell:svc-a",
                "text": format!(r#"<mxCell id="svc-a" value="{v}" vertex="1" parent="1"><mxGeometry x="40" y="60" width="160" height="60" as="geometry"/></mxCell>"#)
            })
        };
        // 连续 3 次同组成功 edit 放行
        for v in ["A1", "A2", "A3"] {
            t.run(&mut d, "edit", &mk(v.into())).await.unwrap();
        }
        let err = t.run(&mut d, "edit", &mk("A4".into())).await.unwrap_err();
        assert!(err.contains("连续成功 edit 3 次"), "{err}");
        assert!(err.contains("本次调用未执行"), "{err}");
        // 换另一组：放行且重置同组计数
        t.run(&mut d, "edit", &serde_json::json!({
            "range": "cell:svc-b",
            "text": r#"<mxCell id="svc-b" value="B1" vertex="1" parent="1"><mxGeometry x="260" y="60" width="160" height="60" as="geometry"/></mxCell>"#
        })).await.unwrap();
        // 回到 svc-a：新的一轮，放行
        t.run(&mut d, "edit", &mk("A5".into())).await.unwrap();
    }

    #[tokio::test]
    async fn failed_edits_do_not_count_toward_streak() {
        let mut d = two_cell_doc();
        let mut t = Tools::new(false);
        // 解析失败（ill-formed）不计数：之后同组 3 次成功仍放行
        for _ in 0..2 {
            assert!(t.run(&mut d, "edit", &serde_json::json!({
                "range": "cell:svc-a", "text": "<mxCell>"
            })).await.is_err());
        }
        let mk = |v: String| {
            serde_json::json!({
                "range": "cell:svc-a",
                "text": format!(r#"<mxCell id="svc-a" value="{v}" vertex="1" parent="1"><mxGeometry x="40" y="60" width="160" height="60" as="geometry"/></mxCell>"#)
            })
        };
        for v in ["A1", "A2", "A3"] {
            t.run(&mut d, "edit", &mk(v.into())).await.unwrap();
        }
        assert!(t.run(&mut d, "edit", &mk("A4".into())).await.is_err());
    }

    #[tokio::test]
    async fn layout_on_edge_gets_specific_error() {
        let xml = r#"<mxfile><diagram id="d"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="a" value="A" vertex="1" parent="1"><mxGeometry x="40" y="60" width="100" height="50" as="geometry"/></mxCell><mxCell id="b" value="B" vertex="1" parent="1"><mxGeometry x="240" y="60" width="100" height="50" as="geometry"/></mxCell><mxCell id="e1" style="edgeStyle=orthogonalEdgeStyle" edge="1" parent="1" source="a" target="b"><mxGeometry relative="1" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;
        let mut d = XmlDoc::from_text(xml).unwrap();
        let mut t = Tools::new(false);
        let err = t
            .run(&mut d, "layout", &serde_json::json!({
                "move": [{"id": "e1", "dx": 10, "dy": 0}]
            }))
            .await
            .unwrap_err();
        assert!(err.contains("边（edge）"), "{err}");
        assert!(err.contains("拐点"), "错误应给可执行替代方案: {err}");
    }

    #[tokio::test]
    async fn layout_route_sets_orthogonal_and_avoids_obstacles() {
        // 直连路径上有障碍 → 必须给出绕行拐点（绕上/绕下），且样式正交化
        let xml = r#"<mxfile><diagram id="d"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="a" value="A" vertex="1" parent="1"><mxGeometry x="0" y="0" width="100" height="50" as="geometry"/></mxCell><mxCell id="b" value="B" vertex="1" parent="1"><mxGeometry x="400" y="0" width="100" height="50" as="geometry"/></mxCell><mxCell id="obs" value="挡路" vertex="1" parent="1"><mxGeometry x="150" y="10" width="100" height="30" as="geometry"/></mxCell><mxCell id="e" style="endArrow=classic" edge="1" parent="1" source="a" target="b"><mxGeometry relative="1" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;
        let mut d = XmlDoc::from_text(xml).unwrap();
        let mut t = Tools::new(false);
        let out = t
            .run(&mut d, "layout", &serde_json::json!({ "route": { "ids": ["e"] } }))
            .await
            .unwrap();
        let text = d.canonical();
        assert!(text.contains("edgeStyle=orthogonalEdgeStyle"), "{out:?}");
        assert!(text.contains("<Array as=\"points\""), "应有避障拐点: {out:?}");
        // 拐点必须在障碍矩形（y 10..40）之外：绕上 y=-65 或绕下 y=115
        let has_detour = text.contains("y=\"-65\"") || text.contains("y=\"115\"");
        assert!(has_detour, "拐点应绕开障碍: {text}");
    }

    #[tokio::test]
    async fn layout_route_without_ids_targets_all_edges() {
        let xml = r#"<mxfile><diagram id="d"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="a" value="A" vertex="1" parent="1"><mxGeometry x="0" y="0" width="100" height="50" as="geometry"/></mxCell><mxCell id="b" value="B" vertex="1" parent="1"><mxGeometry x="400" y="0" width="100" height="50" as="geometry"/></mxCell><mxCell id="e1" edge="1" parent="1" source="a" target="b"><mxGeometry relative="1" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;
        let mut d = XmlDoc::from_text(xml).unwrap();
        let mut t = Tools::new(false);
        let out = t.run(&mut d, "layout", &serde_json::json!({ "route": {} })).await.unwrap();
        let text = d.canonical();
        assert!(text.contains("edgeStyle=orthogonalEdgeStyle"), "{}", out.text);
        // 校验路径：拼错的 id 立即报错
        let err = t.run(&mut d, "layout", &serde_json::json!({ "route": { "ids": ["ghost"] } })).await.unwrap_err();
        assert!(err.contains("不存在"), "{err}");
        let err2 = t.run(&mut d, "layout", &serde_json::json!({ "route": { "ids": ["a"] } })).await.unwrap_err();
        assert!(err2.contains("不是边"), "{err2}");
    }

    #[tokio::test]
    async fn layout_route_clear_path_skips_waypoints() {
        // 无障碍直连：只正交化样式，不加多余拐点
        let xml = r#"<mxfile><diagram id="d"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="a" value="A" vertex="1" parent="1"><mxGeometry x="0" y="0" width="100" height="50" as="geometry"/></mxCell><mxCell id="b" value="B" vertex="1" parent="1"><mxGeometry x="400" y="0" width="100" height="50" as="geometry"/></mxCell><mxCell id="e" edge="1" parent="1" source="a" target="b"><mxGeometry relative="1" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;
        let mut d = XmlDoc::from_text(xml).unwrap();
        let mut t = Tools::new(false);
        t.run(&mut d, "layout", &serde_json::json!({ "route": { "ids": ["e"] } }))
            .await
            .unwrap();
        let text = d.canonical();
        assert!(text.contains("edgeStyle=orthogonalEdgeStyle"));
        assert!(!text.contains("<Array as=\"points\""), "直连无障碍不应加拐点: {text}");
        // 幂等：再跑一次 → no-op
        let out = t
            .run(&mut d, "layout", &serde_json::json!({ "route": { "ids": ["e"] } }))
            .await
            .unwrap();
        assert!(out.text.contains("no-op"), "{}", out.text);
    }



    #[tokio::test]
    async fn locate_finds_by_value() {
        let mut d = doc();
        let mut t = Tools::new(false);
        let out = t.run(&mut d, "read", &serde_json::json!({"query": "order"})).await.unwrap();
        assert!(out.text.contains("svc-a"), "{}", out.text);
        assert!(out.text.contains("@"), "{}", out.text);
        // geo 坐标必须返回：模型规划 layout 依赖它，缺了会诱发 query 空转
        assert!(out.text.contains("geo=40,60 160x60"), "{}", out.text);
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
    async fn read_batch_cells_returns_each_span() {
        let mut d = two_cell_doc();
        let mut t = Tools::new(false);
        let out = t
            .run(&mut d, "read", &serde_json::json!({
                "cells": ["svc-a", "cell:svc-b"]
            }))
            .await
            .unwrap();
        // 两个 cell 的区间各自带 @ 头与行号正文（文件名无关断言）
        let a = d.id_to_cell("svc-a").unwrap();
        let b = d.id_to_cell("svc-b").unwrap();
        assert!(out.text.contains(&format!(":{} 内容如下:", range_str(a.start_line, a.end_line))), "{}", out.text);
        assert!(out.text.contains(&format!(":{} 内容如下:", range_str(b.start_line, b.end_line))), "{}", out.text);
        assert!(out.text.contains("value=\"A\"") && out.text.contains("value=\"B\""), "{}", out.text);
        // 行号是绝对行号（与 edit 共享语义）
        assert!(out.text.contains(&format!("{:>5}|", a.start_line)), "{}", out.text);
    }

    #[tokio::test]
    async fn read_batch_cells_reports_unresolved_inline() {
        let mut d = two_cell_doc();
        let mut t = Tools::new(false);
        let out = t
            .run(&mut d, "read", &serde_json::json!({
                "cells": ["svc-a", "ghost", "42"]
            }))
            .await
            .unwrap();
        // 读操作无副作用：成功项照常返回，失败项行内报告
        assert!(out.text.contains("svc-a"), "{}", out.text);
        assert!(out.text.contains("未解析"), "{}", out.text);
        assert!(out.text.contains("ghost") && out.text.contains("42"), "{}", out.text);
    }

    #[tokio::test]
    async fn read_batch_cells_rejects_empty() {
        let mut d = two_cell_doc();
        let mut t = Tools::new(false);
        let out = t.run(&mut d, "read", &serde_json::json!({ "cells": [] })).await;
        assert!(out.is_err());
    }

    #[tokio::test]
    async fn read_outline_lists_entities_one_per_line() {
        let mut d = doc(); // svc-a vertex
        let mut t = Tools::new(false);
        // 补一条边，验证 edge 行的 src→tgt 形态
        let e = r#"<mxCell id="e1" value="是" edge="1" parent="1" source="svc-a" target="svc-a"><mxGeometry relative="1" as="geometry"/></mxCell>"#;
        let total = crate::xmlfile::total_lines(d.canonical());
        d.apply_edit(total, total, e).unwrap();
        let out = t.run(&mut d, "read", &serde_json::json!({"outline": true})).await.unwrap();
        assert!(out.text.contains("1 vertex"), "{}", out.text);
        assert!(out.text.contains("1 edge"), "{}", out.text);
        // vertex 行有 id 与标签；edge 行有 src→tgt
        assert!(out.text.contains("svc-a"), "{}", out.text);
        assert!(out.text.contains("Order Service"), "{}", out.text);
        assert!(out.text.contains("svc-a→svc-a"), "{}", out.text);
        // diagram 是容器且被标注；锚点 0/1 不出现（首个实体行是 d1）
        assert!(out.text.contains("（容器）"), "{}", out.text);
        let first_row = out.text.lines().nth(1).unwrap();
        assert!(
            first_row.trim_start().starts_with("2-"),
            "首个实体行应是 d1 容器（跳过锚点 0/1）: {first_row}"
        );
        // offset 分页：跳过第一个实体后数量减少
        let out2 = t.run(&mut d, "read", &serde_json::json!({"outline": true, "offset": 1})).await.unwrap();
        let first_rows = out.text.lines().count();
        let second_rows = out2.text.lines().count();
        assert_eq!(first_rows - second_rows, 1, "offset=1 应少显示一行");
    }

    #[tokio::test]
    async fn read_truncation_hint_names_next_range() {
        let mut xml = String::from("<mxfile><diagram id=\"d\"><mxGraphModel><root><mxCell id=\"0\"/>");
        for i in 1..=210 {
            xml.push_str(&format!(r#"<mxCell id="n{i}" value="x" vertex="1" parent="1"><mxGeometry x="0" y="0" width="10" height="10" as="geometry"/></mxCell>"#));
        }
        xml.push_str("</root></mxGraphModel></diagram></mxfile>");
        let mut d = XmlDoc::from_text(&xml).unwrap();
        let mut t = Tools::new(false);
        let out = t.run(&mut d, "read", &serde_json::json!({"range": "1-999"})).await.unwrap();
        assert!(out.text.contains("已截断"), "{}", &out.text[out.text.len().saturating_sub(120)..]);
        assert!(out.text.contains("续读用 range \"151-"), "应给出续读路径: {}", &out.text[out.text.len().saturating_sub(160)..]);
        assert!(out.text.lines().count() <= 160, "{}", out.text.lines().count());
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
