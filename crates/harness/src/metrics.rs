//! Deterministic geometric quality metrics for a drawio diagram.
//!
//! 分层设计（与调研计划一致）：
//! - **errors**：客观缺陷（引用断掉）——人类几乎必然判错
//! - **warnings**：强代理指标（重叠/交叉/标签溢出/越界）——需要阈值，
//!   报告中带 cell id 供修复
//! - **info**：弱代理（孤立节点、近似对齐组）——只报告不评判
//!
//! 全部从 XML 几何计算，不依赖 LLM。lint 工具与 `metrics` CLI 共用。

use quick_xml::events::Event;
use quick_xml::Reader;

#[derive(Debug, Clone, Default)]
pub struct GeomCell {
    pub id: String,
    pub parent: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub value: String,
    pub font_size: f64,
    pub is_edge: bool,
    pub source: Option<String>,
    pub target: Option<String>,
    /// 折线拐点（edge 专属，模型坐标）
    pub points: Vec<(f64, f64)>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Issue {
    pub severity: &'static str, // "error" | "warning" | "info"
    pub kind: &'static str,
    pub ids: Vec<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Stats {
    pub vertices: usize,
    pub edges: usize,
    pub errors: usize,
    pub overlaps: usize,
    pub crossings: usize,
    pub label_overflows: usize,
    pub out_of_bounds: usize,
    pub isolated: usize,
    pub near_aligned_pairs: usize,
    /// 同一源的分支节点未按流向平行摆放（来自人类校准反馈）
    pub branch_misaligned: usize,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Report {
    pub errors: Vec<Issue>,
    pub warnings: Vec<Issue>,
    pub info: Vec<Issue>,
    pub stats: Stats,
}

/// (cells, page 尺寸) 解析结果。
type ParseGeom = (Vec<GeomCell>, Option<(f64, f64)>);

/// 解析 canonical XML 中的 cell 几何（mxCell + mxGeometry + mxPoint）。
pub fn parse_geom(xml: &str) -> Result<ParseGeom, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut cells: Vec<GeomCell> = Vec::new();
    let mut cur: Option<GeomCell> = None;
    let mut in_array = false;
    let mut page: Option<(f64, f64)> = None;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                match e.name().as_ref() {
                    b"mxGraphModel" => {
                        let a = e.attributes();
                        let mut pw: Option<f64> = None;
                        let mut ph: Option<f64> = None;
                        for attr in a.flatten() {
                            let key = attr.key.as_ref();
                            let val = attr
                                .unescape_value()
                                .unwrap_or_default()
                                .into_owned();
                            if key == b"pageWidth" {
                                pw = val.parse().ok();
                            } else if key == b"pageHeight" {
                                ph = val.parse().ok();
                            }
                        }
                        if let (Some(w), Some(h)) = (pw, ph) {
                            page = Some((w, h));
                        }
                    }
                    b"mxCell" => {
                        let mut c = GeomCell::default();
                        for attr in e.attributes().flatten() {
                            let key = attr.key.as_ref();
                            let val = attr
                                .unescape_value()
                                .unwrap_or_default()
                                .into_owned();
                            match key {
                                b"id" => c.id = val,
                                b"parent" => c.parent = val,
                                b"value" => c.value = val,
                                b"source" => c.source = Some(val),
                                b"target" => c.target = Some(val),
                                b"edge" => c.is_edge = val == "1",
                                _ => {}
                            }
                        }
                        cur = Some(c);                        in_array = false;
                    }
                    b"mxGeometry" => {                        if let Some(c) = cur.as_mut() {
                            for attr in e.attributes().flatten() {
                                let key = attr.key.as_ref();
                                let val = attr
                                    .unescape_value()
                                    .unwrap_or_default()
                                    .into_owned();
                                let f = val.parse::<f64>().unwrap_or(0.0);
                                match key {
                                    b"x" => c.x = f,
                                    b"y" => c.y = f,
                                    b"width" => c.w = f,
                                    b"height" => c.h = f,
                                    _ => {}
                                }
                            }
                        }
                    }
                    b"Array" => in_array = true,
                    b"mxPoint" => {
                        if let (true, Some(c)) = (in_array, cur.as_mut()) {
                            let mut px = 0.0;
                            let mut py = 0.0;
                            for attr in e.attributes().flatten() {
                                let key = attr.key.as_ref();
                                let val = attr
                                    .unescape_value()
                                    .unwrap_or_default()
                                    .into_owned();
                                let f = val.parse::<f64>().unwrap_or(0.0);
                                match key {
                                    b"x" => px = f,
                                    b"y" => py = f,
                                    _ => {}
                                }
                            }
                            c.points.push((px, py));
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                match e.name().as_ref() {

                    b"Array" => in_array = false,
                    b"mxCell" => {
                        if let Some(mut c) = cur.take() {
                            // fontSize 从 style 里拿不到的话用默认 11——style
                            // 在 mxCell 的 style 属性；这里回退解析不了就默认。
                            c.font_size = 11.0;
                            if c.id != "0" && c.id != "1" {
                                cells.push(c);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => {
                // 自闭合标签（如 <mxGeometry …/>）是 Empty 事件而非 Start+End
                match e.name().as_ref() {
                    b"mxGeometry" => {
                        if let Some(c) = cur.as_mut() {
                            for attr in e.attributes().flatten() {
                                let key = attr.key.as_ref();
                                let val = attr
                                    .unescape_value()
                                    .unwrap_or_default()
                                    .into_owned();
                                let f = val.parse::<f64>().unwrap_or(0.0);
                                match key {
                                    b"x" => c.x = f,
                                    b"y" => c.y = f,
                                    b"width" => c.w = f,
                                    b"height" => c.h = f,
                                    _ => {}
                                }
                            }
                        }
                    }
                    b"mxPoint" => {
                        if let (true, Some(c)) = (in_array, cur.as_mut()) {
                            let mut px = 0.0;
                            let mut py = 0.0;
                            for attr in e.attributes().flatten() {
                                let key = attr.key.as_ref();
                                let val = attr
                                    .unescape_value()
                                    .unwrap_or_default()
                                    .into_owned();
                                let f = val.parse::<f64>().unwrap_or(0.0);
                                match key {
                                    b"x" => px = f,
                                    b"y" => py = f,
                                    _ => {}
                                }
                            }
                            c.points.push((px, py));
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(format!("xml parse error: {e}")),
        }
        buf.clear();
    }
    Ok((cells, page))
}

fn bbox_intersect(a: &GeomCell, b: &GeomCell) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

/// 线段相交判定；相交返回交点坐标，否则 None。
fn seg_intersect(
    (ax, ay): (f64, f64),
    (bx, by): (f64, f64),
    (cx, cy): (f64, f64),
    (dx, dy): (f64, f64),
) -> Option<(f64, f64)> {
    let cross = |(x1, y1): (f64, f64), (x2, y2): (f64, f64), (x3, y3): (f64, f64)| {
        (x2 - x1) * (y3 - y1) - (y2 - y1) * (x3 - x1)
    };
    let d1 = cross((ax, ay), (bx, by), (cx, cy));
    let d2 = cross((ax, ay), (bx, by), (dx, dy));
    let d3 = cross((cx, cy), (dx, dy), (ax, ay));
    let d4 = cross((cx, cy), (dx, dy), (bx, by));
    let proper = ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
        && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0));
    if !proper {
        return None;
    }
    // 交点：参数 t = 线1上点 = A + t(B-A)
    let denom = (ax - bx) * (cy - dy) - (ay - by) * (cx - dx);
    if denom.abs() < 1e-12 {
        return None;
    }
    let t = ((ax - cx) * (cy - dy) - (ay - cy) * (cx - dx)) / denom;
    Some((ax + t * (bx - ax), ay + t * (by - ay)))
}

/// 线段与矩形相交（Liang-Barsky），排除端点接触
fn seg_rect_intersect(
    (x1, y1): (f64, f64),
    (x2, y2): (f64, f64),
    r: (f64, f64, f64, f64),
) -> bool {
    let (rx, ry, rw, rh) = r;
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    let dx = x2 - x1;
    let dy = y2 - y1;
    for (p, q) in [
        (-dx, x1 - rx),
        (dx, rx + rw - x1),
        (-dy, y1 - ry),
        (dy, ry + rh - y1),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return false;
            }
        } else {
            let t = q / p;
            if p < 0.0 {
                if t > t1 {
                    return false;
                }
                if t > t0 {
                    t0 = t;
                }
            } else {
                if t < t0 {
                    return false;
                }
                if t < t1 {
                    t1 = t;
                }
            }
        }
    }
    // 相交部分长度 > 0.5 才算（端点接触不算）
    t0 < t1 && (t1 - t0) * dx.abs().max(dy.abs()) > 0.5
}

fn edge_segments(
    edge: &GeomCell,
    cells: &[GeomCell],
) -> Vec<((f64, f64), (f64, f64))> {
    let center = |id: &str| -> Option<(f64, f64)> {
        cells
            .iter()
            .find(|c| c.id == id)
            .map(|c| (c.x + c.w / 2.0, c.y + c.h / 2.0))
    };
    let mut pts: Vec<(f64, f64)> = Vec::new();
    if let Some(s) = edge.source.as_ref().and_then(|id| center(id)) {
        pts.push(s);
    }
    pts.extend(edge.points.iter().copied());
    if let Some(t) = edge.target.as_ref().and_then(|id| center(id)) {
        pts.push(t);
    }
    pts.windows(2)
        .map(|w| (w[0], w[1]))
        .collect()
}

/// 祖先链（用于跳过容器与子元素的"重叠"误报）
fn is_ancestor(a: &str, b: &str, cells: &[GeomCell]) -> bool {
    let mut cur = b;
    let mut guard = 0;
    while guard < 64 {
        match cells.iter().find(|c| c.id == cur) {
            Some(c) if c.parent == a => return true,
            Some(c) => cur = &c.parent,
            None => return false,
        }
        guard += 1;
    }
    false
}

/// 估算标签文字需要的宽度（CJK ≈ fontSize，拉丁 ≈ 0.55×fontSize）
fn est_label_width(value: &str, font_size: f64) -> f64 {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii() {
                0.55 * font_size
            } else {
                font_size
            }
        })
        .sum()
}

pub fn analyze(xml: &str) -> Result<Report, String> {
    let (cells, page) = parse_geom(xml)?;
    let mut report = Report::default();
    let vertices: Vec<&GeomCell> = cells.iter().filter(|c| !c.is_edge).collect();
    let edges: Vec<&GeomCell> = cells.iter().filter(|c| c.is_edge).collect();
    report.stats.vertices = vertices.len();
    report.stats.edges = edges.len();

    let ids: std::collections::HashSet<&str> = cells.iter().map(|c| c.id.as_str()).collect();

    // errors: 引用断掉
    for c in &cells {
        let mut broken = Vec::new();
        if c.parent != "0" && c.parent != "1" && !ids.contains(c.parent.as_str()) {
            broken.push(format!("parent={}", c.parent));
        }
        if let Some(s) = &c.source {
            if !ids.contains(s.as_str()) {
                broken.push(format!("source={s}"));
            }
        }
        if let Some(t) = &c.target {
            if !ids.contains(t.as_str()) {
                broken.push(format!("target={t}"));
            }
        }
        if !broken.is_empty() {
            report.errors.push(Issue {
                severity: "error",
                kind: "broken_ref",
                ids: vec![c.id.clone()],
                detail: broken.join(" "),
            });
        }
    }
    report.stats.errors = report.errors.len();

    // warnings: 重叠（跳过祖先-后代）
    for i in 0..vertices.len() {
        for j in (i + 1)..vertices.len() {
            let (a, b) = (vertices[i], vertices[j]);
            if a.w <= 0.0 || a.h <= 0.0 || b.w <= 0.0 || b.h <= 0.0 {
                continue;
            }
            if bbox_intersect(a, b)
                && !is_ancestor(&a.id, &b.id, &cells)
                && !is_ancestor(&b.id, &a.id, &cells)
            {
                report.warnings.push(Issue {
                    severity: "warning",
                    kind: "overlap",
                    ids: vec![a.id.clone(), b.id.clone()],
                    detail: format!(
                        "bbox 相交 ({:.0},{:.0} {:.0}x{:.0}) × ({:.0},{:.0} {:.0}x{:.0})",
                        a.x, a.y, a.w, a.h, b.x, b.y, b.w, b.h
                    ),
                });
            }
        }
    }
    report.stats.overlaps = report.warnings.len();

    // warnings: 交叉（edge×edge 与 edge×顶点矩形）
    type Seg = (String, Vec<((f64, f64), (f64, f64))>);
    let segs: Vec<Seg> = edges
        .iter()
        .map(|e| (e.id.clone(), edge_segments(e, &cells)))
        .collect();
    for i in 0..segs.len() {
        for j in (i + 1)..segs.len() {
            let (id_a, sa) = &segs[i];
            let (id_b, sb) = &segs[j];
            let mut hit: Option<(f64, f64)> = None;
            'outer: for p in sa {
                for q in sb {
                    if let Some(pt) = seg_intersect(p.0, p.1, q.0, q.1) {
                        hit = Some(pt);
                        break 'outer;
                    }
                }
            }
            if let Some((hx, hy)) = hit {
                report.warnings.push(Issue {
                    severity: "warning",
                    kind: "crossing",
                    ids: vec![id_a.clone(), id_b.clone()],
                    detail: format!("连线相交于 ({hx:.0},{hy:.0})"),
                });
            }
        }
        // edge × vertex 矩形（跳过自己的源/目标）
        let (id_e, se) = &segs[i];
        let edge = edges.iter().find(|e| e.id == *id_e).unwrap();
        for v in &vertices {
            if edge.source.as_deref() == Some(v.id.as_str())
                || edge.target.as_deref() == Some(v.id.as_str())
                || v.w <= 0.0
                || v.h <= 0.0
            {
                continue;
            }
            let rect = (v.x, v.y, v.w, v.h);
            if se.iter().any(|(p, q)| seg_rect_intersect(*p, *q, rect)) {
                report.warnings.push(Issue {
                    severity: "warning",
                    kind: "crossing",
                    ids: vec![id_e.clone(), v.id.clone()],
                    detail: format!(
                        "连线 {id_e} 穿过节点 {} 内部（区域 {:.0},{:.0} - {:.0},{:.0}）",
                        v.id,
                        v.x,
                        v.y,
                        v.x + v.w,
                        v.y + v.h
                    ),
                });
            }
        }
    }
    report.stats.crossings = report
        .warnings
        .iter()
        .filter(|w| w.kind == "crossing")
        .count();

    // warnings: 标签溢出（粗估）
    for v in &vertices {
        if v.w <= 0.0 || v.value.trim().is_empty() {
            continue;
        }
        let need = est_label_width(&v.value, v.font_size) + 8.0;
        if need > v.w {
            report.warnings.push(Issue {
                severity: "warning",
                kind: "label_overflow",
                ids: vec![v.id.clone()],
                detail: format!("估 {need:.0}px > 宽 {:.0}px", v.w),
            });
        }
    }
    report.stats.label_overflows = report
        .warnings
        .iter()
        .filter(|w| w.kind == "label_overflow")
        .count();

    // warnings: 越界（页面有界时）
    if let Some((pw, ph)) = page {
        for v in &vertices {
            if v.x < 0.0 || v.y < 0.0 || v.x + v.w > pw || v.y + v.h > ph {
                report.warnings.push(Issue {
                    severity: "warning",
                    kind: "out_of_bounds",
                    ids: vec![v.id.clone()],
                    detail: format!("页面 {pw:.0}x{ph:.0}，cell 超出",),
                });
            }
        }
    }
    report.stats.out_of_bounds = report
        .warnings
        .iter()
        .filter(|w| w.kind == "out_of_bounds")
        .count();

    // warnings: 分支平行性（人类校准反馈派生；窄启发式——只在
    // "决策节点 + 后继同侧 + 尺寸相似"的高置信组合下才告警，
    // 避免径向/瀑布/注脚等合法布局的假阳）
    {
        const TOL: f64 = 8.0;
        for s in &vertices {
            let targets: Vec<&GeomCell> = edges
                .iter()
                .filter(|e| e.source.as_deref() == Some(s.id.as_str()))
                .filter_map(|e| {
                    e.target
                        .as_ref()
                        .and_then(|t| vertices.iter().find(|v| v.id == *t))
                })
                .copied()
                .collect();
            if targets.len() < 2 {
                continue;
            }
            // 同侧约束：所有后继中心相对源中心在水平/垂直方向上符号一致
            let scx = s.x + s.w / 2.0;
            let scy = s.y + s.h / 2.0;
            let dxs: Vec<f64> = targets.iter().map(|t| t.x + t.w / 2.0 - scx).collect();
            let dys: Vec<f64> = targets.iter().map(|t| t.y + t.h / 2.0 - scy).collect();
            let same_side_x =
                dxs.iter().all(|d| *d > 0.0) || dxs.iter().all(|d| *d < 0.0);
            let same_side_y =
                dys.iter().all(|d| *d > 0.0) || dys.iter().all(|d| *d < 0.0);
            let vertical = same_side_y
                && dxs.iter().map(|d| d.abs()).sum::<f64>()
                    < dys.iter().map(|d| d.abs()).sum::<f64>();
            let horizontal = same_side_x && !vertical && dys.iter().map(|d| d.abs()).sum::<f64>()
                < dxs.iter().map(|d| d.abs()).sum::<f64>();
            if !vertical && !horizontal {
                continue; // 混合侧/径向/环形——不评判
            }
            // 尺寸相似约束：最大/最小面积比 ≤ 2.5（注脚类大小悬殊跳过）
            let areas: Vec<f64> = targets.iter().map(|t| t.w * t.h).collect();
            let (min_a, max_a) = areas
                .iter()
                .fold((f64::INFINITY, 0.0f64), |(mn, mx), a| (mn.min(*a), mx.max(*a)));
            if min_a <= 0.0 || max_a / min_a > 2.5 {
                continue;
            }
            for i in 0..targets.len() {
                for j in (i + 1)..targets.len() {
                    let (a, b) = (targets[i], targets[j]);
                    let ay = a.y + a.h / 2.0;
                    let by = b.y + b.h / 2.0;
                    let ax = a.x + a.w / 2.0;
                    let bx = b.x + b.w / 2.0;
                    let mis = if vertical {
                        (ay - by).abs() > TOL
                    } else {
                        (ax - bx).abs() > TOL
                    };
                    if mis {
                        report.warnings.push(Issue {
                            severity: "warning",
                            kind: "branch_not_parallel",
                            ids: vec![a.id.clone(), b.id.clone()],
                            detail: format!(
                                "同源 {} 的分支未按流向平行（{}流向应同 {}；非平行的有意摆法可忽略）",
                                s.id,
                                if vertical { "垂直" } else { "水平" },
                                if vertical { "y" } else { "x" }
                            ),
                        });
                    }
                }
            }
        }
        report.stats.branch_misaligned = report
            .warnings
            .iter()
            .filter(|w| w.kind == "branch_not_parallel")
            .count();
    }

    // info: 孤立节点（图里有连线时才提示）
    if report.stats.edges >= 2 {
        for v in &vertices {
            let connected = edges
                .iter()
                .any(|e| e.source.as_deref() == Some(v.id.as_str()) || e.target.as_deref() == Some(v.id.as_str()));
            if !connected {
                report.info.push(Issue {
                    severity: "info",
                    kind: "isolated",
                    ids: vec![v.id.clone()],
                    detail: "无任何连线（可能是有意）".into(),
                });
            }
        }
    }
    report.stats.isolated = report.info.iter().filter(|i| i.kind == "isolated").count();

    // info: 近似对齐对（中心 x/y 差 ≤ 2px）
    for i in 0..vertices.len() {
        for j in (i + 1)..vertices.len() {
            let (a, b) = (vertices[i], vertices[j]);
            let ax = a.x + a.w / 2.0;
            let ay = a.y + a.h / 2.0;
            let bx = b.x + b.w / 2.0;
            let by = b.y + b.h / 2.0;
            if (ax - bx).abs() <= 2.0 || (ay - by).abs() <= 2.0 {
                report.stats.near_aligned_pairs += 1;
            }
        }
    }

    Ok(report)
}

/// lint 工具用的紧凑人类可读文本。
pub fn lint_text(report: &Report) -> String {
    let mut out = format!(
        "lint 结果: {} errors, {} warnings（重叠 {} · 交叉 {} · 标签溢出 {} · 越界 {} · 分支未平行 {}）",
        report.stats.errors,
        report.warnings.len(),
        report.stats.overlaps,
        report.stats.crossings,
        report.stats.label_overflows,
        report.stats.out_of_bounds,
        report.stats.branch_misaligned
    );
    if report.errors.is_empty() && report.warnings.is_empty() {
        out.push_str("\n无硬缺陷与警告。");
        return out;
    }
    for e in &report.errors {
        out.push_str(&format!("\n[error:{}] {} —— {}", e.kind, e.ids.join(", "), e.detail));
    }
    // 截断：警告过多时只列前 5——全量清单会让模型陷入逐条清零循环
    const SHOW_MAX: usize = 5;
    for (wi, w) in report.warnings.iter().take(SHOW_MAX).enumerate() {
        out.push_str(&format!(
            "\n[warning:{}] {} —— {}",
            w.kind,
            w.ids.join(", "),
            w.detail
        ));
        if wi + 1 == SHOW_MAX && report.warnings.len() > SHOW_MAX {
            out.push_str(&format!(
                "\n…（另有 {} 条同类警告未列出——修最明显的即可，不要逐条清零）",
                report.warnings.len() - SHOW_MAX
            ));
        }
    }
    out.push_str("\n说明：结构错误必须修；布局警告修最明显的 1-2 处即可，残余轻微交叉/重叠可接受并在总结里说明——不要逐条清零（收益极低且烧轮次）。若警告过多，说明布局整体拥挤即可。");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(body: &str) -> String {
        format!("<mxfile><diagram id=\"d\"><mxGraphModel pageWidth=\"600\" pageHeight=\"400\"><root><mxCell id=\"0\"/><mxCell id=\"1\" parent=\"0\"/>{body}</root></mxGraphModel></diagram></mxfile>")
    }

    fn vertex(id: &str, x: f64, y: f64, w: f64, h: f64, value: &str) -> String {
        format!(
            "<mxCell id=\"{id}\" value=\"{value}\" vertex=\"1\" parent=\"1\"><mxGeometry x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" as=\"geometry\"/></mxCell>"
        )
    }

    #[test]
    fn detects_overlap() {
        let xml = doc(&format!(
            "{}{}",
            vertex("a", 0.0, 0.0, 100.0, 100.0, "A"),
            vertex("b", 50.0, 50.0, 100.0, 100.0, "B")
        ));
        let r = analyze(&xml).unwrap();
        assert_eq!(r.stats.overlaps, 1);
        assert_eq!(r.warnings[0].ids, vec!["a", "b"]);
    }

    #[test]
    fn skips_ancestor_overlap() {
        let xml = doc(&format!(
            "<mxCell id=\"g\" value=\"\" vertex=\"1\" parent=\"1\"><mxGeometry x=\"0\" y=\"0\" width=\"200\" height=\"200\" as=\"geometry\"/></mxCell>\
             <mxCell id=\"child\" value=\"c\" vertex=\"1\" parent=\"g\"><mxGeometry x=\"10\" y=\"10\" width=\"50\" height=\"50\" as=\"geometry\"/></mxCell>"
        ));
        let r = analyze(&xml).unwrap();
        assert_eq!(r.stats.overlaps, 0);
    }

    #[test]
    fn crossing_detail_includes_coordinates() {
        // X 形交叉：a→d 与 b→c，交点应在图中央附近
        let xml = doc(&format!(
            "{}{}{}{}{}",
            vertex("a", 0.0, 0.0, 80.0, 40.0, "A"),
            vertex("b", 200.0, 0.0, 80.0, 40.0, "B"),
            vertex("c", 0.0, 200.0, 80.0, 40.0, "C"),
            vertex("d", 200.0, 200.0, 80.0, 40.0, "D"),
            "<mxCell id=\"e1\" edge=\"1\" parent=\"1\" source=\"a\" target=\"d\"><mxGeometry relative=\"1\" as=\"geometry\"/></mxCell>\
             <mxCell id=\"e2\" edge=\"1\" parent=\"1\" source=\"b\" target=\"c\"><mxGeometry relative=\"1\" as=\"geometry\"/></mxCell>"
        ));
        let r = analyze(&xml).unwrap();
        let crossing = r.warnings.iter().find(|w| w.kind == "crossing").unwrap();
        assert!(crossing.detail.contains('('), "应含交点坐标: {}", crossing.detail);
    }

    #[test]
    fn edge_through_vertex_detail_reports_region() {
        let xml = doc(&format!(
            "{}{}{}{}",
            vertex("a", 0.0, 0.0, 40.0, 40.0, "A"),
            vertex("b", 300.0, 0.0, 40.0, 40.0, "B"),
            vertex("mid", 150.0, -10.0, 60.0, 60.0, "M"),
            "<mxCell id=\"e1\" edge=\"1\" parent=\"1\" source=\"a\" target=\"b\"><mxGeometry relative=\"1\" as=\"geometry\"/></mxCell>"
        ));
        let r = analyze(&xml).unwrap();
        let crossing = r
            .warnings
            .iter()
            .find(|w| w.kind == "crossing" && w.detail.contains("穿过节点"))
            .expect("穿节点类应存在");
        assert!(crossing.detail.contains("区域"), "{}", crossing.detail);
    }

    #[test]
    fn detects_edge_crossing_and_shared_endpoint_ok() {
        let xml = doc(&format!(
            "{}{}{}{}{}",
            vertex("a", 0.0, 0.0, 80.0, 40.0, "A"),
            vertex("b", 200.0, 0.0, 80.0, 40.0, "B"),
            vertex("c", 0.0, 100.0, 80.0, 40.0, "C"),
            vertex("d", 200.0, 100.0, 80.0, 40.0, "D"),
            "<mxCell id=\"e1\" edge=\"1\" parent=\"1\" source=\"a\" target=\"d\"><mxGeometry relative=\"1\" as=\"geometry\"/></mxCell>\
             <mxCell id=\"e2\" edge=\"1\" parent=\"1\" source=\"b\" target=\"c\"><mxGeometry relative=\"1\" as=\"geometry\"/></mxCell>"
        ));
        let r = analyze(&xml).unwrap();
        // e1 与 e2 交叉（X 形）；共享端点不计（本用例无共享）
        assert_eq!(r.stats.crossings, 1);
    }

    #[test]
    fn edge_through_vertex_counts_as_crossing() {
        let xml = doc(&format!(
            "{}{}{}{}",
            vertex("a", 0.0, 0.0, 40.0, 40.0, "A"),
            vertex("b", 300.0, 0.0, 40.0, 40.0, "B"),
            vertex("mid", 150.0, -10.0, 60.0, 60.0, "M"),
            "<mxCell id=\"e1\" edge=\"1\" parent=\"1\" source=\"a\" target=\"b\"><mxGeometry relative=\"1\" as=\"geometry\"/></mxCell>"
        ));
        let r = analyze(&xml).unwrap();
        assert!(r.stats.crossings >= 1);
    }

    #[test]
    fn label_overflow_estimate() {
        let xml = doc(&vertex("a", 0.0, 0.0, 30.0, 40.0, "很长很长的标签"));
        let r = analyze(&xml).unwrap();
        assert!(r.stats.label_overflows >= 1);
    }

    #[test]
    fn out_of_bounds() {
        let xml = doc(&vertex("a", 500.0, 0.0, 200.0, 40.0, "A"));
        let r = analyze(&xml).unwrap();
        assert_eq!(r.stats.out_of_bounds, 1);
    }

    #[test]
    fn branch_parallelism_flagged_for_stacked_siblings() {
        // 垂直流向：verify → success(y=360) / fail(y=460) 堆叠 → 应告警
        let xml = doc(&format!(
            "{}{}{}{}",
            vertex("verify", 390.0, 240.0, 120.0, 60.0, "验证"),
            vertex("success", 380.0, 360.0, 140.0, 50.0, "成功"),
            vertex("fail", 380.0, 460.0, 140.0, 50.0, "失败"),
            "<mxCell id=\"e3\" edge=\"1\" parent=\"1\" source=\"verify\" target=\"success\"><mxGeometry relative=\"1\" as=\"geometry\"/></mxCell>\
             <mxCell id=\"e4\" edge=\"1\" parent=\"1\" source=\"verify\" target=\"fail\"><mxGeometry relative=\"1\" as=\"geometry\"/></mxCell>"
        ));
        let r = analyze(&xml).unwrap();
        assert!(r.stats.branch_misaligned >= 1);
        assert!(r.warnings.iter().any(|w| w.kind == "branch_not_parallel"));
    }

    #[test]
    fn branch_parallelism_ok_for_side_by_side_siblings() {
        // 同 y 的平行分支 → 不告警
        let xml = doc(&format!(
            "{}{}{}{}",
            vertex("verify", 350.0, 240.0, 120.0, 60.0, "验证"),
            vertex("success", 300.0, 360.0, 140.0, 50.0, "成功"),
            vertex("fail", 480.0, 360.0, 140.0, 50.0, "失败"),
            "<mxCell id=\"e3\" edge=\"1\" parent=\"1\" source=\"verify\" target=\"success\"><mxGeometry relative=\"1\" as=\"geometry\"/></mxCell>\
             <mxCell id=\"e4\" edge=\"1\" parent=\"1\" source=\"verify\" target=\"fail\"><mxGeometry relative=\"1\" as=\"geometry\"/></mxCell>"
        ));
        let r = analyze(&xml).unwrap();
        assert_eq!(r.stats.branch_misaligned, 0);
    }

    #[test]
    fn mixed_side_branch_skipped() {
        // "是→右、否→下"式分叉：不同侧 → 不评判
        let xml = doc(&format!(
            "{}{}{}{}",
            vertex("v", 350.0, 240.0, 120.0, 60.0, "决策"),
            vertex("yes", 520.0, 240.0, 100.0, 50.0, "是"),
            vertex("no", 350.0, 380.0, 100.0, 50.0, "否"),
            "<mxCell id=\"e1\" edge=\"1\" parent=\"1\" source=\"v\" target=\"yes\"><mxGeometry relative=\"1\" as=\"geometry\"/></mxCell>\
             <mxCell id=\"e2\" edge=\"1\" parent=\"1\" source=\"v\" target=\"no\"><mxGeometry relative=\"1\" as=\"geometry\"/></mxCell>"
        ));
        let r = analyze(&xml).unwrap();
        assert_eq!(r.stats.branch_misaligned, 0);
    }

    #[test]
    fn broken_ref_is_error() {
        let xml = doc("<mxCell id=\"e\" edge=\"1\" parent=\"1\" source=\"ghost\" target=\"a\"><mxGeometry relative=\"1\" as=\"geometry\"/></mxCell>");
        let r = analyze(&xml).unwrap();
        assert_eq!(r.stats.errors, 1);
        assert_eq!(r.errors[0].kind, "broken_ref");
    }
}
