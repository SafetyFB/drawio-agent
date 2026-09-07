//! The one artifact: a canonical pretty-printed mxfile kept on disk.
//!
//! Raw .drawio files are usually compressed and single-line, which makes
//! line numbers useless and diffs unreadable. [`XmlDoc`] is a loader +
//! editor that:
//!
//! - expands compressed `<diagram>` payloads (base64 + raw deflate, the
//!   format drawio writes),
//! - re-formats to a canonical layout (one element per line, stable
//!   attribute order, byte-safe entity escaping),
//! - keeps a span index (`cell id -> line range`) rebuilt after every edit,
//!   which powers `@file:lines` style references and locality reporting,
//! - applies line-range edits textually: untouched bytes stay untouched.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine;
use flate2::read::DeflateDecoder;
use quick_xml::Reader;
use quick_xml::events::Event;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum XmlError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("xml parse error: {0}")]
    Xml(String),
    #[error("compressed diagram payload decode failed: {0}")]
    BlobDecode(String),
    #[error("bad range `{0}` (expect e.g. `120`, `120-156`, or `cell:abc`)")]
    BadRange(String),
}

// ---------------------------------------------------------------------------
// Lightweight tree (kept private; only used to re-emit canonical text)
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Node {
    Elem {
        tag: String,
        attrs: Vec<(String, String)>,
        children: Vec<Node>,
        /// True when the source used `<tag/>`; irrelevant once children exist,
        /// but preserved so empty elements keep self-closing form.
        self_closing: bool,
    },
    /// Escaped content, unescaped at parse time (re-escaped on emit).
    Text(String),
    /// Raw CDATA content (no `<![CDATA[` / `]]>` markers).
    CData(String),
    /// Raw comment content (no `<!--` / `-->` markers).
    Comment(String),
    /// Raw PI content (no `<?` / `?>` markers).
    Pi(String),
    /// XML declaration — normalized on emit (drawio files are UTF-8).
    Decl,
}

/// Parse raw XML into a node tree. All events are read up front (owned),
/// then a recursive builder walks them — no peek/pushback gymnastics.
/// Whitespace-only text nodes are dropped (canonical layout owns all
/// whitespace). Attribute values and text are unescaped here, so emitting
/// code can escape with a single policy and roundtrips are exact.
fn parse_nodes(xml: &str) -> Result<Vec<Node>, XmlError> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut events: Vec<quick_xml::events::Event<'static>> = Vec::new();
    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(ev) => events.push(ev.into_owned()),
            Err(e) => return Err(XmlError::Xml(format!("{e}"))),
        }
    }
    let mut i = 0usize;
    let mut nodes = Vec::new();
    while i < events.len() {
        if let Some(n) = build_node(&events, &mut i)? {
            nodes.push(n);
        }
    }
    Ok(nodes)
}

/// Build one node starting at `events[i]`, advancing `i` past it (including
/// any children and the matching end tag). Returns `None` for content that
/// canonical layout drops (whitespace-only text).
fn build_node(
    events: &[quick_xml::events::Event<'static>],
    i: &mut usize,
) -> Result<Option<Node>, XmlError> {
    let ev = &events[*i];
    match ev {
        Event::Start(e) => {
            let mut node = elem_node(e)?;
            *i += 1;
            // Children until the matching </tag>.
            let open = String::from_utf8_lossy(e.name().as_ref()).into_owned();
            while *i < events.len() {
                match &events[*i] {
                    Event::End(e2) => {
                        let close = String::from_utf8_lossy(e2.name().as_ref()).into_owned();
                        if close != open {
                            return Err(XmlError::Xml(format!(
                                "mismatched end tag `</{close}>` (open `<{open}>`)"
                            )));
                        }
                        *i += 1;
                        break;
                    }
                    _ => {
                        if let Some(child) = build_node(events, i)? {
                            if let Node::Elem {
                                children, ..
                            } = &mut node
                            {
                                children.push(child);
                            }
                        }
                    }
                }
            }
            Ok(Some(node))
        }
        Event::Empty(e) => {
            *i += 1;
            let mut node = elem_node(e)?;
            if let Node::Elem { self_closing, .. } = &mut node {
                *self_closing = true;
            }
            Ok(Some(node))
        }
        Event::Text(t) => {
            *i += 1;
            let raw = String::from_utf8_lossy(t.as_ref());
            let text = quick_xml::escape::unescape(&raw)
                .map_err(|e| XmlError::Xml(format!("bad entity: {e}")))?
                .into_owned();
            if text.trim().is_empty() {
                // Whitespace-only: canonical layout owns all whitespace.
                Ok(None)
            } else {
                Ok(Some(Node::Text(text)))
            }
        }
        Event::CData(c) => {
            *i += 1;
            Ok(Some(Node::CData(
                String::from_utf8_lossy(c.as_ref()).into_owned(),
            )))
        }
        Event::Comment(c) => {
            *i += 1;
            Ok(Some(Node::Comment(
                String::from_utf8_lossy(c.as_ref()).into_owned(),
            )))
        }
        Event::Decl(_) => {
            // Quick-xml hands us the decl INCLUDING the `xml ` target
            // prefix in as_ref; re-emitting it verbatim inside `<?xml …?>`
            // would stack another `xml ` on every save cycle (observed:
            // `<?xml xml xml xml version="1.0"?>` after four loads). The
            // declaration is just a marker here; emit a canonical one.
            *i += 1;
            Ok(Some(Node::Decl))
        }
        Event::PI(p) => {
            *i += 1;
            Ok(Some(Node::Pi(String::from_utf8_lossy(p.as_ref()).into_owned())))
        }
        Event::DocType(_) => {
            // Dropped: doc-types don't occur in drawio files and would break
            // the one-element-per-line layout.
            *i += 1;
            Ok(None)
        }
        other => Err(XmlError::Xml(format!(
            "unexpected event inside element tree: {other:?}"
        ))),
    }
}

/// 形状名 → 2018-viewer 支持的裸名（核心 mxShape + 2018 drawio 扩展）。
/// 现代 drawio 的 `mxgraph.basic.X` 命名空间在本 bundle 里不存在，会渲染成
/// 矩形；基础形状一律归一化为核心裸名。
fn basic_shape_alias(name: &str) -> Option<&'static str> {
    Some(match name {
        "ellipse" => "ellipse",
        "rectangle" | "rect" => "rectangle",
        "rounded" => "rounded",
        "rhombus" => "rhombus",
        "triangle" => "triangle",
        "hexagon" => "hexagon",
        "cylinder" => "cylinder",
        "actor" => "actor",
        "cloud" => "cloud",
        "swimlane" => "swimlane",
        "process" => "process",
        "step" => "step",
        "document" => "document",
        "note" => "note",
        "parallelogram" => "parallelogram",
        "trapezoid" => "trapezoid",
        "cube" => "cube",
        "delay" => "delay",
        "tee" => "tee",
        "cross" => "cross",
        "xor" => "xor",
        "or" => "or",
        "plus" => "plus",
        "tape" => "tape",
        "waypoint" => "waypoint",
        "link" => "link",
        "card" => "card",
        "folder" => "folder",
        "message" => "message",
        "datastore" => "datastore",
        "doubleEllipse" => "doubleEllipse",
        _ => return None,
    })
}

/// 归一化 mxCell 的 style 属性，使形状写法与本仓库 vendored 的 2018 版
/// mxGraph viewer 兼容（它只认 `shape=<裸名>`）：
/// - 裸首键简写：`ellipse;whiteSpace=wrap;…` → `shape=ellipse;…`
/// - 命名空间：`shape=mxgraph.basic.ellipse` → `shape=ellipse`
/// - 其余条目（含 mxgraph.er.* / bpmn.* 等 bundle 支持的扩展名）原样保留。
fn normalize_style(style: &str) -> String {
    let parts: Vec<&str> = style
        .split(';')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        return style.to_string();
    }
    let mut out: Vec<String> = parts.iter().map(|p| p.to_string()).collect();
    // 1) shape=mxgraph.basic.X -> shape=X
    for part in out.iter_mut() {
        if let Some(rest) = part.strip_prefix("shape=mxgraph.basic.") {
            if let Some(alias) = basic_shape_alias(rest) {
                *part = format!("shape={alias}");
            }
        }
    }
    // 2) 裸首键是已知形状名 -> shape=<名>
    if !out[0].contains('=') {
        if let Some(alias) = basic_shape_alias(out[0].as_str()) {
            out[0] = format!("shape={alias}");
        }
    }
    let mut joined = out.join(";");
    // 保留原样的尾分号（drawio 惯例）
    if style.ends_with(';') && !joined.ends_with(';') {
        joined.push(';');
    }
    joined
}

/// Node from a Start/Empty event, with unescaped attribute values.
fn elem_node(e: &quick_xml::events::BytesStart<'_>) -> Result<Node, XmlError> {
    let tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    let mut attrs = Vec::new();
    for a in e.attributes() {
        let a = a.map_err(|e| XmlError::Xml(format!("bad attribute: {e}")))?;
        let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
        let mut value = a
            .unescape_value()
            .map_err(|e| XmlError::Xml(format!("bad entity in `{key}`: {e}")))?
            .into_owned();
        // mxCell 的 style 属性做 shape 归一化（viewer 兼容）
        if tag == "mxCell" && key == "style" {
            value = normalize_style(&value);
        }
        attrs.push((key, value));
    }
    Ok(Node::Elem {
        tag,
        attrs,
        children: Vec::new(),
        self_closing: false,
    })
}

// ---------------------------------------------------------------------------
// Escape / emit
// ---------------------------------------------------------------------------

/// Escape for attribute values: `& < > "` plus control characters that XML
/// would otherwise normalize away (`\n \r \t` -> `&#10; &#13; &#9;`). This
/// matches what drawio writes, so multi-line labels roundtrip byte-exactly.
fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            c => out.push(c),
        }
    }
    out
}

/// Escape for text content (same policy as attributes minus the quote, but
/// quoting quotes is harmless; keep identical for predictability).
fn escape_text(s: &str) -> String {
    escape_attr(s).replace("&quot;", "\"").replace("&apos;", "'")
}

struct Emitter {
    out: String,
}

impl Emitter {
    fn line(&mut self, indent: usize, s: &str) {
        for _ in 0..indent {
            self.out.push_str("  ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn elem(&mut self, indent: usize, n: &Node) {
        let Node::Elem { tag, attrs, children, self_closing } = n else {
            unreachable!()
        };
        let mut open = format!("<{tag}");
        for (k, v) in attrs {
            let _ = write!(open, " {k}=\"{}\"", escape_attr(v));
        }
        if children.is_empty() && *self_closing {
            self.line(indent, &format!("{open}/>"));
            return;
        }
        if children.is_empty() {
            // Source had `<a></a>`; normalize to self-closing for compactness.
            self.line(indent, &format!("{open}/>"));
            return;
        }
        self.line(indent, &format!("{open}>"));
        for c in children {
            self.node(indent + 1, c);
        }
        self.line(indent, &format!("</{tag}>"));
    }

    fn node(&mut self, indent: usize, n: &Node) {
        match n {
            Node::Elem { .. } => self.elem(indent, n),
            Node::Text(t) => self.line(indent, &escape_text(t)),
            Node::CData(raw) => self.line(indent, &format!("<![CDATA[{raw}]]>")),
            Node::Comment(raw) => self.line(indent, &format!("<!--{raw}-->")),
            Node::Decl => {
                if self.out.is_empty() {
                    self.out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
                }
            }
            Node::Pi(raw) => self.line(indent, &format!("<?{raw}?>")),
        }
    }
}

// ---------------------------------------------------------------------------
// Compressed diagram expansion (drawio on-disk format)
// ---------------------------------------------------------------------------

fn inflate_diagram_blob(blob: &str) -> Result<String, XmlError> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(blob.trim())
        .map_err(|e| XmlError::BlobDecode(format!("base64: {e}")))?;
    let mut dec = DeflateDecoder::new(raw.as_slice());
    let mut s = String::new();
    std::io::Read::read_to_string(&mut dec, &mut s)
        .map_err(|e| XmlError::BlobDecode(format!("deflate: {e}")))?;
    Ok(s)
}

/// Expand any `compressed="true"` `<diagram>` payloads in a parsed tree and
/// flip the attribute to `compressed="false"`.
fn expand_diagrams(nodes: &mut [Node]) -> Result<(), XmlError> {
    for n in nodes {
        match n {
            Node::Elem { tag, attrs, children, .. } if tag == "mxfile" => {
                for child in children {
                    expand_diagram(child)?;
                }
            }
            Node::Elem { tag, .. } if tag == "diagram" => expand_diagram(n)?,
            _ => {}
        }
    }
    Ok(())
}

fn expand_diagram(n: &mut Node) -> Result<(), XmlError> {
    let Node::Elem { attrs, children, .. } = n else { return Ok(()) };
    let compressed = attrs
        .iter()
        .any(|(k, v)| k == "compressed" && v == "true");
    if !compressed {
        return Ok(());
    }
    // Blob lives as the sole non-whitespace text child.
    let blob: Option<String> = match children.as_slice() {
        [Node::Text(t)] => Some(t.clone()),
        _ => None,
    };
    let Some(blob) = blob else {
        // Uncompressed-looking diagram that claims compression: keep as-is
        // but drop the lie so drawio doesn't try to decode real children.
        for (k, v) in attrs.iter_mut() {
            if k == "compressed" {
                *v = "false".into();
            }
        }
        return Ok(());
    };
    let inner = inflate_diagram_blob(&blob)?;
    if inner.trim().is_empty() {
        children.clear();
    } else {
        let parsed = parse_nodes(&inner).map_err(|e| {
            XmlError::BlobDecode(format!("expanded diagram is not valid XML: {e}"))
        })?;
        *children = parsed;
    }
    for (k, v) in attrs.iter_mut() {
        if k == "compressed" {
            *v = "false".into();
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Canonical text
// ---------------------------------------------------------------------------

/// Parse + re-emit in canonical layout. Idempotent: canonical input maps to
/// itself byte-for-byte (asserted in tests). Attribute order is preserved;
/// entity escaping is normalized to one canonical policy.
pub fn canonicalize(xml: &str) -> Result<String, XmlError> {
    let mut roots = parse_nodes(xml)?;
    expand_diagrams(&mut roots)?;
    let mut em = Emitter {
        out: String::new(),
    };
    for n in &roots {
        em.node(0, n);
    }
    Ok(em.out)
}

/// Is this file compressed drawio format (needs one canonicalize pass that
/// expands it), or already readable XML? `canonicalize` handles both, so this
/// is only a convenience for diagnostics.
pub fn looks_compressed(xml: &str) -> bool {
    xml.contains("compressed=\"true\"") || xml.contains("compressed='true'")
}

// ---------------------------------------------------------------------------
// Span index: cell id -> line range (1-based, inclusive)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellSpan {
    pub id: String,
    pub tag: String,
    /// 1-based inclusive line range of the whole element in canonical text.
    pub start_line: usize,
    pub end_line: usize,
    /// Byte offsets (for potential byte-level slicing).
    pub start_byte: usize,
    pub end_byte: usize,
}

pub fn line_index(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

fn line_of(starts: &[usize], byte: usize) -> usize {
    match starts.binary_search(&byte) {
        Ok(i) => i + 1,
        Err(i) => i, // Err(i) = insertion point; line is i (0-based) -> +1
    }
}

/// Rescan canonical text and index every element carrying an `id` attribute.
/// Also catches duplicate ids (drawio breaks on them).
pub fn index(text: &str) -> Result<Vec<CellSpan>, XmlError> {
    let starts = line_index(text);
    let mut reader = Reader::from_str(text);
    let mut buf = Vec::new();
    let mut stack: Vec<CellSpan> = Vec::new();
    let mut out: Vec<CellSpan> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();

    loop {
        buf.clear();
        let pos_before = reader.buffer_position() as usize;
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(ev) => {
                let pos_after = reader.buffer_position() as usize;
                match ev {
                    Event::Start(e) => {
                        let id = id_attr(&e);
                        let span = CellSpan {
                            id: id.unwrap_or_default(),
                            tag: String::from_utf8_lossy(e.name().as_ref()).into_owned(),
                            start_line: line_of(&starts, pos_before),
                            end_line: 0,
                            start_byte: pos_before,
                            end_byte: 0,
                        };
                        stack.push(span);
                    }
                    Event::Empty(e) => {
                        if let Some(id) = id_attr(&e) {
                            let cell = CellSpan {
                                id: id.clone(),
                                tag: String::from_utf8_lossy(e.name().as_ref()).into_owned(),
                                start_line: line_of(&starts, pos_before),
                                end_line: line_of(&starts, pos_after),
                                start_byte: pos_before,
                                end_byte: pos_after,
                            };
                            if let Some(prev) = seen.insert(id, out.len()) {
                                return Err(XmlError::Xml(format!(
                                    "duplicate id `{}` (elements at lines {} and {})",
                                    cell.id, out[prev].start_line, cell.start_line
                                )));
                            }
                            out.push(cell);
                        }
                    }
                    Event::End(_) => {
                        if let Some(mut span) = stack.pop() {
                            span.end_line = line_of(&starts, pos_after);
                            span.end_byte = pos_after;
                            if !span.id.is_empty() {
                                if let Some(prev) = seen.insert(span.id.clone(), out.len()) {
                                    return Err(XmlError::Xml(format!(
                                        "duplicate id `{}` (elements at lines {} and {})",
                                        span.id, out[prev].start_line, span.start_line
                                    )));
                                }
                                out.push(span);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Err(e) => return Err(XmlError::Xml(format!("{e}"))),
        }
    }
    out.sort_by_key(|c| c.start_line);
    Ok(out)
}

fn id_attr(e: &quick_xml::events::BytesStart<'_>) -> Option<String> {
    for a in e.attributes() {
        let Ok(a) = a else { continue };
        if a.key.as_ref() == b"id" {
            return a
                .unescape_value()
                .ok()
                .map(|v| v.into_owned());
        }
    }
    None
}

/// Extract the raw lines `[start..=end]` (1-based, inclusive).
pub fn lines_in(text: &str, start: usize, end: usize) -> String {
    text.lines()
        .skip(start.saturating_sub(1))
        .take(end.saturating_sub(start) + 1)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Replace lines `[start..=end]` (1-based, inclusive) with `replacement`.
pub fn replace_lines(text: &str, start: usize, end: usize, replacement: &str) -> String {
    let all: Vec<&str> = text.lines().collect();
    let mut out = String::new();
    for (i, l) in all.iter().enumerate() {
        let lineno = i + 1;
        if lineno == start {
            out.push_str(replacement);
            out.push('\n');
        }
        if lineno < start || lineno > end {
            out.push_str(l);
            out.push('\n');
        }
    }
    if start > all.len() {
        // Appending past the end: pad nothing, just append replacement once.
        out.push_str(replacement);
        out.push('\n');
    }
    out
}

pub fn total_lines(text: &str) -> usize {
    text.lines().count()
}

// ---------------------------------------------------------------------------
// The document
// ---------------------------------------------------------------------------

/// Result of an applied edit, reported back to the model so it knows exactly
/// what changed (locality feedback instead of protocol enforcement).
#[derive(Debug, Default, Clone)]
pub struct EditReport {
    pub noop: bool,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    /// Changed cells whose span does NOT touch the edited line range.
    pub off_range: Vec<String>,
    pub unchanged: usize,
}

/// Loaded, canonical document with its span index.
#[derive(Debug, Clone)]
pub struct XmlDoc {
    pub path: PathBuf,
    pub text: String,
    pub cells: Vec<CellSpan>,
    /// Rolling undo stack of previous canonical texts (most recent first).
    history: Vec<String>,
}

impl XmlDoc {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, XmlError> {
        let raw = std::fs::read_to_string(path.as_ref())?;
        let mut doc = Self::from_text(&raw)?;
        doc.path = path.as_ref().to_path_buf();
        Ok(doc)
    }

    /// 读 cell 的几何（模型坐标），无则 None。复用 metrics 的解析。
    pub fn geometry_of(&self, id: &str) -> Option<(f64, f64, f64, f64)> {
        let (cells, _) = crate::metrics::parse_geom(&self.text).ok()?;
        cells
            .iter()
            .find(|c| c.id == id && !c.is_edge)
            .map(|c| (c.x, c.y, c.w, c.h))
    }

    /// 把 cell 的 mxGeometry 行整体替换（x/y/width/height 不变则返回 Ok(None)）。
    /// 返回 (该行行号, 新行文本)——布局工具只替换这一行，不碰 cell 其余行。
    pub fn set_geometry_line(&self, id: &str, x: f64, y: f64, w: f64, h: f64) -> Result<Option<(usize, String)>, XmlError> {
        let span = self
            .cells
            .iter()
            .find(|c| c.id == id)
            .ok_or_else(|| XmlError::BadRange(format!("cell `{id}` 不存在")))?;
        let slice = lines_in(&self.text, span.start_line, span.end_line);
        // 找 mxGeometry 所在行
        let mut geo_line_no = None;
        for (i, l) in slice.lines().enumerate() {
            if l.contains("mxGeometry") {
                geo_line_no = Some(i);
                break;
            }
        }
        let Some(gi) = geo_line_no else {
            return Err(XmlError::BadRange(format!("cell `{id}` 没有 mxGeometry")));
        };
        let line = slice.lines().nth(gi).unwrap().to_string();
        let fmt = |v: f64| {
            if (v - v.round()).abs() < 1e-9 {
                format!("{}", v.round() as i64)
            } else {
                format!("{v:.2}")
            }
        };
        // 线内 x=/y=/width=/height= 逐个替换（手写区间替换，无正则依赖）
        let mut out_line = line.clone();
        for (attr, val) in [
            ("x", x),
            ("y", y),
            ("width", w),
            ("height", h),
        ] {
            // 简单手写：找到 attr=" 后到下一个 " 的区间
            let pat = format!("{attr}=\"");
            let Some(starti) = out_line.find(&pat) else {
                continue;
            };
            let vstart = starti + pat.len();
            let vend = out_line[vstart..].find('"').map(|i| vstart + i);
            if let Some(ve) = vend {
                out_line.replace_range(vstart..ve, &fmt(val));
            }
        }
        if out_line == line {
            return Ok(None);
        }
        Ok(Some((span.start_line + gi, out_line)))
    }

    pub fn save(&self) -> Result<(), XmlError> {
        // 原子落盘：临时文件 + rename（批量全或无的最后一环）
        // 唯一后缀：多线程测试/多会话并行时避免 tmp 路径相撞
        let nonce: u128 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = self.path.with_extension(format!("drawio.tmp.{nonce:016x}"));
        std::fs::write(&tmp, &self.text)?;
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(XmlError::from(e));
        }
        Ok(())
    }

    pub fn canonical(&self) -> &str {
        &self.text
    }

    /// Parse canonicalize+index in memory (no disk).
    pub fn from_text(text: &str) -> Result<Self, XmlError> {
        let text = canonicalize(text)?;
        let cells = index(&text)?;
        Ok(Self {
            path: PathBuf::from("(memory)"),
            text,
            cells,
            history: Vec::new(),
        })
    }

    /// Like [`Self::from_text`] but bound to a real file path, so `save()`
    /// writes where the caller expects (memory-only docs must never save
    /// into a stray `(memory)` file).
    pub fn from_text_at(text: &str, path: impl AsRef<Path>) -> Result<Self, XmlError> {
        let mut d = Self::from_text(text)?;
        d.path = path.as_ref().to_path_buf();
        Ok(d)
    }

    pub fn id_to_cell(&self, id: &str) -> Option<&CellSpan> {
        self.cells.iter().find(|c| c.id == id)
    }

    /// Resolve a user/model range spec. Accepted forms: `120`, `120-156`,
    /// `diagram.xml:120-156` (path prefix stripped), `cell:abc` or a bare
    /// cell id (via span index). Returns (start, end) line numbers.
    pub fn resolve_range(&self, spec: &str) -> Result<(usize, usize), XmlError> {
        let spec = spec.trim();
        if let Some(id) = spec.strip_prefix("cell:") {
            return self
                .id_to_cell(id)
                .map(|c| (c.start_line, c.end_line))
                .ok_or_else(|| XmlError::BadRange(format!("no cell with id `{id}`")));
        }
        let candidates: Vec<&str> = spec
            .rsplit_once(':')
            .map(|(_, rest)| vec![rest.trim(), spec])
            .unwrap_or_else(|| vec![spec]);
        let total = total_lines(&self.text);
        for c in candidates {
            if let Ok((a, b)) = parse_line_range(c) {
                if b <= total {
                    return Ok((a, b));
                }
                return Err(XmlError::BadRange(format!(
                    "{c} (file has {total} lines)"
                )));
            }
            if let Some(cell) = self.id_to_cell(c) {
                return Ok((cell.start_line, cell.end_line));
            }
        }
        Err(XmlError::BadRange(spec.into()))
    }

    /// Apply a textual edit over `[start..=end]` lines: validate, canonicalize,
    /// re-index, diff, then commit. Nothing is written to disk here; the caller
    /// saves (REPL saves after every applied edit).
    pub fn apply_edit(
        &mut self,
        start: usize,
        end: usize,
        replacement: &str,
    ) -> Result<EditReport, XmlError> {
        let total = total_lines(&self.text);
        if start < 1 || end < start || end > total {
            return Err(XmlError::BadRange(format!(
                "{start}-{end} (file has {total} lines)"
            )));
        }
        let before = content_map(&self.text, &self.cells);
        let candidate = replace_lines(&self.text, start, end, replacement);
        let canonical = canonicalize(&candidate)?;
        if canonical == self.text {
            return Ok(EditReport {
                noop: true,
                unchanged: before.len(),
                ..EditReport::default()
            });
        }
        let cells = index(&canonical)?;
        let after = content_map(&canonical, &cells);

        let mut report = EditReport {
            noop: false,
            ..EditReport::default()
        };
        let mut ids = BTreeSet::new();
        for id in before.keys() {
            ids.insert(id.clone());
        }
        for id in after.keys() {
            ids.insert(id.clone());
        }
        for id in &ids {
            match (before.get(id), after.get(id)) {
                (None, Some(_)) => report.added.push(id.clone()),
                (Some(_), None) => report.removed.push(id.clone()),
                (Some(a), Some(b)) => {
                    if a != b {
                        report.changed.push(id.clone());
                    } else {
                        report.unchanged += 1;
                    }
                }
                (None, None) => {}
            }
        }
        for id in &report.changed {
            let Some(span) = cells.iter().find(|c| &c.id == id) else {
                continue;
            };
            let touches = !(span.end_line < start || span.start_line > end);
            if !touches {
                report.off_range.push(id.clone());
            }
        }

        self.history.push(std::mem::replace(&mut self.text, canonical));
        if self.history.len() > 10 {
            self.history.remove(0);
        }
        self.cells = cells;
        Ok(report)
    }

    /// 批量行区间编辑（原始文件行号）：内存内一次应用全部并统一校验。
    ///
    /// - 每个范围都按**原始文件**行号解释（模型 read 到的行号）；
    ///   区间按 start 从大到小应用，行号不漂移
    /// - 区间重叠（含相接）视为非法：批量里模型必须给不相交区间
    /// - 全部应用后**一次** canonicalize/index/diff（含 off_range 用
    ///   整个批量的并集区间判定）——磁盘只在调用方 save 时落盘，
    ///   校验失败时磁盘分毫未动（全或无）
    pub fn apply_edits(
        &mut self,
        edits: &[(usize, usize, String)],
    ) -> Result<EditReport, XmlError> {
        let total = total_lines(&self.text);
        for (start, end, _) in edits {
            if *start < 1 || *end < *start || *end > total {
                return Err(XmlError::BadRange(format!(
                    "{start}-{end} (file has {total} lines)"
                )));
            }
        }
        // 重叠检测：按 start 升序排，后一个 start <= 前一个 end 即重叠
        let mut sorted: Vec<&(usize, usize, String)> = edits.iter().collect();
        sorted.sort_by_key(|(s, _, _)| *s);
        for w in sorted.windows(2) {
            if w[1].0 <= w[0].1 {
                return Err(XmlError::BadRange(format!(
                    "批量区间重叠或相接: {}-{} 与 {}-{}",
                    w[0].0, w[0].1, w[1].0, w[1].1
                )));
            }
        }
        let (lo, hi) = (
            sorted.first().map(|e| e.0).unwrap_or(1),
            sorted.last().map(|e| e.1).unwrap_or(0),
        );
        // 从大到小应用：先改后面的行，前面的行号不受影响
        let mut candidate = self.text.clone();
        let mut desc = sorted;
        desc.reverse();
        for (start, end, text) in desc {
            candidate = replace_lines(&candidate, *start, *end, text);
        }
        let canonical = canonicalize(&candidate)?;
        if canonical == self.text {
            return Ok(EditReport {
                noop: true,
                unchanged: total,
                ..EditReport::default()
            });
        }
        let cells = index(&canonical)?;
        let before = content_map(&self.text, &self.cells);
        let after = content_map(&canonical, &cells);
        let mut report = EditReport {
            noop: false,
            ..EditReport::default()
        };
        let mut ids = BTreeSet::new();
        for id in before.keys() {
            ids.insert(id.clone());
        }
        for id in after.keys() {
            ids.insert(id.clone());
        }
        for id in &ids {
            match (before.get(id), after.get(id)) {
                (None, Some(_)) => report.added.push(id.clone()),
                (Some(_), None) => report.removed.push(id.clone()),
                (Some(a), Some(b)) => {
                    if a != b {
                        report.changed.push(id.clone());
                    } else {
                        report.unchanged += 1;
                    }
                }
                (None, None) => {}
            }
        }
        for id in &report.changed {
            let Some(span) = cells.iter().find(|c| &c.id == id) else {
                continue;
            };
            let touches = !(span.end_line < lo || span.start_line > hi);
            if !touches {
                report.off_range.push(id.clone());
            }
        }
        self.history.push(std::mem::replace(&mut self.text, canonical));
        if self.history.len() > 10 {
            self.history.remove(0);
        }
        self.cells = cells;
        Ok(report)
    }

    pub fn undo(&mut self) -> Option<String> {
        let prev = self.history.pop()?;
        self.text = prev;
        self.cells = index(&self.text).ok()?;
        Some(self.text.clone())
    }

}

fn parse_line_range(spec: &str) -> Result<(usize, usize), XmlError> {
    let spec = spec.trim();
    let (a, b) = match spec.split_once('-') {
        Some((a, b)) => {
            let a: usize = a
                .trim()
                .parse()
                .map_err(|_| XmlError::BadRange(spec.into()))?;
            let b: usize = b
                .trim()
                .parse()
                .map_err(|_| XmlError::BadRange(spec.into()))?;
            if a == 0 || b < a {
                return Err(XmlError::BadRange(spec.into()));
            }
            (a, b)
        }
        None => {
            let a: usize = spec
                .parse()
                .map_err(|_| XmlError::BadRange(spec.into()))?;
            if a == 0 {
                return Err(XmlError::BadRange(spec.into()));
            }
            (a, a)
        }
    };
    Ok((a, b))
}

/// Loose range parse for read-only lookups: strips a path prefix, accepts
/// ranges that run past the end of the file (clamped), and rejects
/// zero/inverted ranges.
pub fn parse_range_loose(spec: &str, total_lines: usize) -> Result<(usize, usize), XmlError> {
    let spec = spec.trim().trim_start_matches('@');
    let cand: Vec<&str> = spec
        .rsplit_once(':')
        .map(|(_, rest)| vec![rest.trim(), spec])
        .unwrap_or_else(|| vec![spec]);
    for c in cand {
        if let Ok((a, b)) = parse_line_range(c) {
            if a < 1 || a > total_lines {
                return Err(XmlError::BadRange(format!("{c} (file has {total_lines} lines)")));
            }
            return Ok((a, b.min(total_lines)));
        }
    }
    Err(XmlError::BadRange(spec.into()))
}

/// id -> element line-slice content, used to diff versions by content.
///
/// Only *leaf* ids are reportable: an element whose span contains another
/// id'd element (e.g. `<diagram id="d1">` wrapping all cells) would always
/// look "changed" when any descendant changes. Containers are tracked by
/// the index (for @refs) but excluded from edit diffs.
fn content_map(text: &str, cells: &[CellSpan]) -> HashMap<String, String> {
    // A cell is a leaf if no other cell starts strictly inside its span.
    let is_leaf = |c: &CellSpan| {
        !cells.iter().any(|o| {
            o.id != c.id
                && o.start_line > c.start_line
                && o.start_line <= c.end_line
        })
    };
    let mut m = HashMap::new();
    for c in cells {
        if !is_leaf(c) {
            continue;
        }
        // Content only — line positions shift when anything above a cell is
        // inserted/deleted and must NOT count as a change.
        m.insert(
            c.id.clone(),
            lines_in(text, c.start_line, c.end_line),
        );
    }
    m
}

// ---------------------------------------------------------------------------
// Deterministic checks
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct CheckReport {
    pub cells: usize,
    pub edges: usize,
    pub issues: Vec<String>,
}

impl CheckReport {
    pub fn ok(&self) -> bool {
        self.issues.is_empty()
    }
    pub fn summarize(&self) -> String {
        let mut s = format!(
            "cells={} edges={} issues={}",
            self.cells,
            self.edges,
            self.issues.len()
        );
        for i in &self.issues {
            s.push_str(&format!("\n  ⚠️  {i}"));
        }
        if self.issues.is_empty() {
            s.push_str("\n  通过：id 唯一、结构完整");
        }
        s
    }
}

/// Deterministic sanity checks on the current document (no LLM involved):
/// parseability is implied by construction; this checks id uniqueness again,
/// edge endpoint references, and mxGraphModel/root structure.
pub fn check_doc(text: &str) -> Result<CheckReport, XmlError> {
    let cells = index(text)?;
    let mut report = CheckReport {
        cells: cells.len(),
        ..CheckReport::default()
    };
    let ids: BTreeSet<&str> = cells.iter().map(|c| c.id.as_str()).collect();
    for c in &cells {
        if c.tag == "mxCell" {
            // edges vs vertices by attribute presence: not parsed here, so use
            // a structural probe: count mxCell with edge attr later? cheap
            // textual probe is fine for diagnostics.
        }
    }
    // Edge endpoint references + parent references.
    let mut cell_tags: HashMap<String, &CellSpan> = HashMap::new();
    for c in &cells {
        cell_tags.insert(c.id.clone(), c);
    }
    let mut reader = Reader::from_str(text);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if e.name().as_ref() != b"mxCell" {
                    continue;
                }
                let mut attr_id = None;
                let mut parent = None;
                let mut source = None;
                let mut target = None;
                let mut edge = None;
                for a in e.attributes() {
                    let Ok(a) = a else { continue };
                    let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
                    let val = a
                        .unescape_value()
                        .map(|v| v.into_owned())
                        .unwrap_or_default();
                    match key.as_str() {
                        "id" => attr_id = Some(val),
                        "parent" => parent = Some(val),
                        "source" => source = Some(val),
                        "target" => target = Some(val),
                        "edge" => edge = Some(val),
                        _ => {}
                    }
                }
                let Some(id) = attr_id else { continue };
                if edge.as_deref() == Some("1") {
                    report.edges += 1;
                    for (k, v) in [("source", &source), ("target", &target)] {
                        if let Some(v) = v.as_deref() {
                            if !ids.contains(v) {
                                report.issues.push(format!(
                                    "edge `{id}` references missing {k} cell `{v}`"
                                ));
                            }
                        }
                    }
                }
                if let Some(p) = parent {
                    if !ids.contains(p.as_str()) {
                        report.issues.push(format!(
                            "cell `{id}` references missing parent `{p}`"
                        ));
                    }
                }
                let _ = cell_tags.get(id.as_str());
            }
            Ok(_) => {}
            Err(e) => return Err(XmlError::Xml(format!("{e}"))),
        }
    }
    // Structural: exactly one mxGraphModel with a root inside it.
    report
        .issues
        .sort();
    report.issues.dedup();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<mxfile host="app.diagrams.net"><diagram id="d1" name="Page-1"><mxGraphModel dx="700" dy="500" grid="1" gridSize="10"><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="svc-a" value="Order Service" style="rounded=1" vertex="1" parent="1"><mxGeometry x="40" y="60" width="160" height="60" as="geometry"/></mxCell><mxCell id="svc-b" value="Billing&#10;Service" vertex="1" parent="1"><mxGeometry x="240" y="60" width="160" height="60" as="geometry"/></mxCell><mxCell id="e1" style="edgeStyle=orthogonalEdgeStyle" edge="1" parent="1" source="svc-a" target="svc-b"><mxGeometry relative="1" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;

    #[test]
    fn canonical_layout_one_element_per_line() {
        let c = canonicalize(SAMPLE).unwrap();
        // Every markup open tag owns exactly one line (no lines with
        // multiple elements, no closing tag sharing a line).
        assert!(c.starts_with("<mxfile host=\"app.diagrams.net\">\n"));
        let n = total_lines(&c);
        let opens = c.matches('<').count();
        assert_eq!(c.lines().count(), opens, "one element open per line");
        assert!(n > 12, "expanded beyond one line");
    }

    #[test]
    fn decl_never_stacks_and_corrupted_decl_is_repaired() {
        // Valid decl stays valid and does not multiply across load-save
        // cycles (each cycle used to add one more `xml ` prefix).
        let c1 = canonicalize("<?xml version=\"1.0\" encoding=\"UTF-8\"?><mxfile><diagram id=\"d\"/></mxfile>").unwrap();
        assert!(c1.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"), "{c1}");
        let c2 = canonicalize(&c1).unwrap();
        assert_eq!(c1, c2, "second save must not alter the decl");
        // A file corrupted by the old bug gets repaired in one pass.
        let bad = "<?xml xml xml xml version=\"1.0\"?><mxfile><diagram id=\"d\"/></mxfile>";
        let fixed = canonicalize(bad).unwrap();
        assert!(fixed.starts_with("<?xml version=\"1.0\""), "{fixed}");
        assert!(!fixed.contains("xml xml xml"));
        // No decl in the input -> none added (byte-shape stays predictable).
        let none = canonicalize("<mxfile><diagram id=\"d\"/></mxfile>").unwrap();
        assert!(!none.starts_with("<?xml"));
    }

    #[test]
    fn canonical_is_idempotent() {
        let c1 = canonicalize(SAMPLE).unwrap();
        assert_eq!(canonicalize(&c1).unwrap(), c1);
    }

    #[test]
    fn style_shapes_normalized_for_2018_viewer() {
        assert_eq!(
            normalize_style("ellipse;whiteSpace=wrap;html=1;fillColor=#FFFF00;"),
            "shape=ellipse;whiteSpace=wrap;html=1;fillColor=#FFFF00;"
        );
        assert_eq!(
            normalize_style("shape=mxgraph.basic.ellipse;whiteSpace=wrap;html=1;"),
            "shape=ellipse;whiteSpace=wrap;html=1;"
        );
        assert_eq!(
            normalize_style("shape=mxgraph.basic.rect;perimeter=rectanglePerimeter;"),
            "shape=rectangle;perimeter=rectanglePerimeter;"
        );
        // 未建模的扩展名原样保留
        assert_eq!(
            normalize_style("shape=mxgraph.er.entity;html=1;"),
            "shape=mxgraph.er.entity;html=1;"
        );
        assert_eq!(
            normalize_style("shape=hexagon;perimeter=hexagonPerimeter2;"),
            "shape=hexagon;perimeter=hexagonPerimeter2;"
        );
        // 普通键不受影响
        assert_eq!(
            normalize_style("rounded=1;whiteSpace=wrap;html=1;"),
            "rounded=1;whiteSpace=wrap;html=1;"
        );
        // 空样式
        assert_eq!(normalize_style(""), "");
    }

    #[test]
    fn canonicalize_converts_modern_shape_forms() {
        let xml = r#"<mxfile><diagram id="d"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="c" value="" style="ellipse;whiteSpace=wrap;html=1;" vertex="1" parent="1"><mxGeometry x="0" y="0" width="40" height="40" as="geometry"/></mxCell><mxCell id="d2" value="" style="shape=mxgraph.basic.ellipse;whiteSpace=wrap;html=1;" vertex="1" parent="1"><mxGeometry x="60" y="0" width="40" height="40" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;
        let c = canonicalize(xml).unwrap();
        assert!(c.contains(r#"style="shape=ellipse;whiteSpace=wrap;html=1;""#), "{c}");
        assert!(!c.contains("mxgraph.basic"), "{c}");
        // 幂等
        assert_eq!(canonicalize(&c).unwrap(), c);
    }

    #[test]
    fn multiline_label_roundtrips_byte_exact() {
        let c1 = canonicalize(SAMPLE).unwrap();
        assert!(c1.contains("Billing&#10;Service"));
        assert!(!c1.contains("&amp;#10;"), "double-escaped entity");
        assert_eq!(canonicalize(&c1).unwrap(), c1);
    }

    #[test]
    fn attr_order_and_unknown_content_preserved() {
        let xml = r#"<mxfile host="h" agent="a"><diagram id="z"><mxGraphModel><root><mxCell id="1" value="a &amp; b &lt;c&gt; &quot;d&quot;" vertex="1"><mxGeometry x="1" y="2" width="3" height="4" as="geometry"><Array as="points"><mxPoint x="0" y="0"/></Array></mxGeometry></mxCell></root></mxGraphModel></diagram></mxfile>"#;
        let c1 = canonicalize(xml).unwrap();
        assert!(c1.contains("a &amp; b &lt;c&gt; &quot;d&quot;"));
        assert!(c1.contains("<Array as=\"points\">"));
        assert_eq!(canonicalize(&c1).unwrap(), c1);
    }

    #[test]
    fn compressed_diagram_expands() {
        let inner = r#"<mxGraphModel dx="700"><root><mxCell id="0"/><mxCell id="1" parent="0"/></root></mxGraphModel>"#;
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        use std::io::Write;
        enc.write_all(inner.as_bytes()).unwrap();
        let compressed = base64::engine::general_purpose::STANDARD.encode(enc.finish().unwrap());
        let file = format!(
            r#"<mxfile host="app.diagrams.net"><diagram id="d1" name="p" compressed="true">{compressed}</diagram></mxfile>"#
        );
        let c = canonicalize(&file).unwrap();
        assert!(c.contains("compressed=\"false\""));
        assert!(c.contains("<mxGraphModel dx=\"700\">"));
        assert!(c.contains("<mxCell id=\"0\"/>"));
        assert_eq!(canonicalize(&c).unwrap(), c);
    }

    #[test]
    fn index_finds_cell_line_ranges() {
        let c = canonicalize(SAMPLE).unwrap();
        let cells = index(&c).unwrap();
        let a = cells.iter().find(|x| x.id == "svc-a").unwrap();
        assert_eq!(a.tag, "mxCell");
        let got = lines_in(&c, a.start_line, a.end_line);
        assert!(got.contains("svc-a"));
        assert!(got.contains("mxGeometry"));
        assert!(!got.contains("svc-b"));
        assert!(a.end_line > a.start_line);
        // containers are indexed too (used by @refs)
        assert!(cells.iter().any(|x| x.id == "d1"));
    }

    #[test]
    fn replace_lines_edit_and_diff() {
        let mut doc = XmlDoc::from_text(SAMPLE).unwrap();
        let svc_a = doc.id_to_cell("svc-a").unwrap().clone();
        let new_text = r#"<mxCell id="svc-a" value="Payments Service" style="rounded=1" vertex="1" parent="1"><mxGeometry x="40" y="60" width="160" height="60" as="geometry"/></mxCell>"#;
        let rep = doc
            .apply_edit(svc_a.start_line, svc_a.end_line, new_text)
            .unwrap();
        assert!(!rep.noop);
        assert_eq!(rep.changed, vec!["svc-a"], "containers must not pollute diff");
        assert!(rep.off_range.is_empty());
        assert!(doc.text.contains("Payments Service"));
        // unchanged cells are byte-identical
        let b = doc.id_to_cell("svc-b").unwrap();
        let btxt = lines_in(&doc.text, b.start_line, b.end_line);
        assert!(btxt.contains("Billing&#10;Service"));
    }

    #[test]
    fn whole_file_replace_locality() {
        // Model "draw" = replace 1..end: diff must name exactly the cells
        // whose content changed, and nothing off-range.
        let mut doc = XmlDoc::from_text(SAMPLE).unwrap();
        let (start, end) = (1, total_lines(&doc.text));
        let new_text =
            lines_in(&doc.text, start, end).replace("Order Service", "Order Service v2");
        let rep = doc.apply_edit(start, end, &new_text).unwrap();
        assert_eq!(rep.changed, vec!["svc-a"]);
        assert!(rep.added.is_empty() && rep.removed.is_empty());
        assert!(rep.off_range.is_empty());
        assert_eq!(rep.unchanged, 4); // 0, 1, svc-b, e1 (d1 filtered: container)
    }

    #[test]
    fn delete_cell_reports_removed() {
        let mut doc = XmlDoc::from_text(SAMPLE).unwrap();
        let e1 = doc.id_to_cell("e1").unwrap().clone();
        let rep = doc.apply_edit(e1.start_line, e1.end_line, "").unwrap();
        assert_eq!(rep.removed, vec!["e1"]);
        assert!(rep.changed.is_empty());
        assert!(doc.id_to_cell("e1").is_none());
        // d1, 0, 1, svc-a, svc-b survive
        assert_eq!(doc.cells.len(), 5);
    }

    #[test]
    fn noop_edit_detected() {
        let mut doc = XmlDoc::from_text(SAMPLE).unwrap();
        let svc_a = doc.id_to_cell("svc-a").unwrap().clone();
        let same = lines_in(&doc.text, svc_a.start_line, svc_a.end_line);
        let rep = doc.apply_edit(svc_a.start_line, svc_a.end_line, &same).unwrap();
        assert!(rep.noop);
    }

    #[test]
    fn undo_restores() {
        let mut doc = XmlDoc::from_text(SAMPLE).unwrap();
        let before = doc.text.clone();
        let svc_a = doc.id_to_cell("svc-a").unwrap().clone();
        doc.apply_edit(
            svc_a.start_line,
            svc_a.end_line,
            r#"<mxCell id="svc-a" value="X" parent="1"/>"#,
        )
        .unwrap();
        assert!(doc.text.contains("X"));
        doc.undo();
        assert_eq!(doc.text, before);
    }

    #[test]
    fn resolve_range_specs() {
        let doc = XmlDoc::from_text(SAMPLE).unwrap();
        let a = doc.id_to_cell("svc-a").unwrap();
        assert_eq!(
            doc.resolve_range("cell:svc-a").unwrap(),
            (a.start_line, a.end_line)
        );
        assert_eq!(
            doc.resolve_range(&format!("{}", a.start_line)).unwrap(),
            (a.start_line, a.start_line)
        );
        assert_eq!(doc.resolve_range("diagram.xml:1-3").unwrap(), (1, 3));
        assert_eq!(doc.resolve_range("@diagram.xml:2").unwrap(), (2, 2));
        assert!(doc.resolve_range("cell:nope").is_err());
        assert!(doc.resolve_range("999999-1000000").is_err());
    }

    #[test]
    fn index_rejects_duplicate_ids() {
        let xml = r#"<mxfile><diagram id="d"><mxGraphModel><root><mxCell id="0"/><mxCell id="c" vertex="1" parent="0"/><mxCell id="c" vertex="1" parent="0"/></root></mxGraphModel></diagram></mxfile>"#;
        assert!(index(&canonicalize(xml).unwrap()).is_err());
    }

    #[test]
    fn check_reports_broken_refs() {
        let xml = r#"<mxfile><diagram id="d"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="c" vertex="1" parent="9"/><mxCell id="e" edge="1" parent="1" source="c" target="ghost"/></root></mxGraphModel></diagram></mxfile>"#;
        let report = check_doc(&canonicalize(xml).unwrap()).unwrap();
        assert!(report.issues.iter().any(|i| i.contains("parent `9`")));
        assert!(report.issues.iter().any(|i| i.contains("missing target")));
        assert!(!report.issues.iter().any(|i| i.contains("source")));
        assert_eq!(report.edges, 1);
        assert!(!report.ok());
    }
}
