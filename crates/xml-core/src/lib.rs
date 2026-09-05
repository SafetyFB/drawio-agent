//! Draw.io XML core: parse, query, mutate, validate, serialize.
//!
//! See crate-level README for status. Phase 1 work proceeds by TDD.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

use std::collections::HashMap;
use std::io::{Read, Write};

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::name::QName;
use quick_xml::reader::Reader;
use quick_xml::writer::Writer;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MxFile {
    pub diagrams: Vec<Diagram>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagram {
    pub id: String,
    pub name: String,
    pub model: Option<MxGraphModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MxGraphModel {
    pub root: Cell,
}

impl MxGraphModel {
    /// Find a cell by id via depth-first search.
    /// Returns `None` if no cell with that id exists.
    pub fn get(&self, id: &str) -> Option<&Cell> {
        find_in(&self.root, id)
    }

    /// Mutable counterpart of [`get`].
    pub fn get_mut(&mut self, id: &str) -> Option<&mut Cell> {
        find_in_mut(&mut self.root, id)
    }

    /// Extract a subgraph containing the requested cells and the edges
    /// that connect them. Cells adjacent to the selection via edges are
    /// returned in `context` (read-only on apply).
    ///
    /// Requested ids not found in the model are reported in `missing`.
    /// Duplicate ids are deduplicated.
    pub fn extract_subgraph(&self, ids: &[&str]) -> Subgraph {
        use std::collections::HashSet;

        let mut primary: Vec<Cell> = Vec::new();
        let mut missing: Vec<String> = Vec::new();
        let mut primary_set: HashSet<String> = HashSet::new();

        for id in ids {
            if let Some(cell) = self.get(id) {
                if primary_set.insert(cell.id.clone()) {
                    primary.push(cell.clone());
                }
            } else {
                missing.push((*id).to_string());
            }
        }

        // Walk the model collecting every edge, then partition by relation
        // to the primary set.
        let mut all_edges: Vec<&Cell> = Vec::new();
        collect_edges(&self.root, &mut all_edges);

        let mut edges: Vec<Cell> = Vec::new();
        let mut context: Vec<Cell> = Vec::new();
        let mut context_set: HashSet<String> = HashSet::new();

        for edge in &all_edges {
            let src = edge.source.as_deref();
            let tgt = edge.target.as_deref();

            let src_in = src.map_or(false, |id| primary_set.contains(id));
            let tgt_in = tgt.map_or(false, |id| primary_set.contains(id));

            if !src_in && !tgt_in {
                continue;
            }

            edges.push((*edge).clone());

            // Pull the non-primary endpoint into context (deduplicated).
            let other_id = if src_in { tgt } else { src };
            if let Some(id) = other_id {
                if !primary_set.contains(id) && context_set.insert(id.to_string()) {
                    if let Some(cell) = self.get(id) {
                        context.push(cell.clone());
                    }
                }
            }
        }

        Subgraph {
            primary,
            edges,
            context,
            missing,
        }
    }

    /// Apply a [`Subgraph`] back to the model.
    ///
    /// - `primary` and `edges` cells: replaced in-place if id exists;
    ///   inserted under their declared `parent` if id is new. Children
    ///   of an existing cell are preserved (the subgraph does not own them).
    /// - `context` cells: **ignored** — read-only reference for the LLM.
    /// - Cells not in the subgraph: untouched, attributes byte-for-byte
    ///   preserved.
    pub fn apply_subgraph(&mut self, sub: &Subgraph) -> ApplyResult {
        let mut updated: Vec<String> = Vec::new();
        let mut added: Vec<String> = Vec::new();

        for cell in sub.primary.iter().chain(sub.edges.iter()) {
            let id = cell.id.clone();
            if id.is_empty() {
                continue;
            }
            if self.get_mut(&id).is_some() {
                replace_cell_in_place(&mut self.root, &id, cell);
                updated.push(id);
            } else {
                insert_cell(&mut self.root, cell.clone());
                added.push(id);
            }
        }

        ApplyResult {
            updated,
            added,
            context_ignored: sub.context.len(),
        }
    }

    /// Validate structural integrity of the model. Collects **all** errors
    /// it can detect in a single pass rather than failing on the first.
    ///
    /// Checks:
    /// - The synthetic root cell exists with id `0`.
    /// - All cell ids are unique across the tree.
    /// - Every edge's `source` and `target` reference an existing cell.
    pub fn validate(&self) -> ValidationReport {
        use std::collections::HashMap;

        let mut errors: Vec<ValidationError> = Vec::new();

        // Pass 1: count occurrences of every id (also detects MissingRoot).
        let mut counts: HashMap<String, usize> = HashMap::new();
        count_ids(&self.root, &mut counts);

        let root_count = counts.get("0").copied().unwrap_or(0);
        if root_count == 0 {
            errors.push(ValidationError::MissingRoot);
        }
        for (id, n) in &counts {
            if *n > 1 {
                errors.push(ValidationError::DuplicateId(id.clone()));
            }
        }

        // Pass 2: every edge's endpoints must resolve.
        let mut edges: Vec<&Cell> = Vec::new();
        collect_edges(&self.root, &mut edges);
        for edge in &edges {
            if let Some(src) = &edge.source {
                if !counts.contains_key(src) {
                    errors.push(ValidationError::EdgeMissingSource {
                        edge_id: edge.id.clone(),
                        source: src.clone(),
                    });
                }
            }
            if let Some(tgt) = &edge.target {
                if !counts.contains_key(tgt) {
                    errors.push(ValidationError::EdgeMissingTarget {
                        edge_id: edge.id.clone(),
                        target: tgt.clone(),
                    });
                }
            }
        }

        ValidationReport { errors }
    }
}

/// Outcome of [`MxGraphModel::apply_subgraph`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyResult {
    /// Ids of existing cells that were replaced.
    pub updated: Vec<String>,
    /// Ids of cells newly inserted into the model.
    pub added: Vec<String>,
    /// Number of context cells that were deliberately skipped.
    pub context_ignored: usize,
}

/// One structural issue found by [`MxGraphModel::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// The model tree contains no cell with id `0` (synthetic root).
    MissingRoot,
    /// Two or more cells share the same id.
    DuplicateId(String),
    /// An edge references a `source` cell that does not exist.
    EdgeMissingSource { edge_id: String, source: String },
    /// An edge references a `target` cell that does not exist.
    EdgeMissingTarget { edge_id: String, target: String },
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingRoot => write!(f, "missing root cell id=0"),
            Self::DuplicateId(id) => write!(f, "duplicate cell id: {id}"),
            Self::EdgeMissingSource { edge_id, source } => {
                write!(f, "edge {edge_id} references missing source: {source}")
            }
            Self::EdgeMissingTarget { edge_id, target } => {
                write!(f, "edge {edge_id} references missing target: {target}")
            }
        }
    }
}

/// Aggregated validation outcome for a model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationReport {
    pub errors: Vec<ValidationError>,
}

impl ValidationReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

impl std::fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_ok() {
            return write!(f, "validation passed");
        }
        for e in &self.errors {
            writeln!(f, "- {e}")?;
        }
        Ok(())
    }
}

fn replace_cell_in_place(root: &mut Cell, id: &str, replacement: &Cell) {
    fn walk(node: &mut Cell, id: &str, replacement: &Cell) -> bool {
        if node.id == id {
            // Preserve children — they are not part of the subgraph scope.
            let children = std::mem::take(&mut node.children);
            *node = replacement.clone();
            node.children = children;
            return true;
        }
        for child in node.children.iter_mut() {
            if walk(child, id, replacement) {
                return true;
            }
        }
        false
    }
    walk(root, id, replacement);
}

fn insert_cell(root: &mut Cell, cell: Cell) {
    let parent_id = cell.parent.clone().unwrap_or_else(|| "1".to_string());
    if try_insert_under(root, &parent_id, cell.clone()) {
        return;
    }
    // Parent missing → fall back to root layer (id="1") so we don't lose
    // the cell entirely. Validation can flag this in a later pass.
    let _ = try_insert_under(root, "1", cell);
}

fn try_insert_under(root: &mut Cell, parent_id: &str, cell: Cell) -> bool {
    if root.id == parent_id {
        root.children.push(cell);
        return true;
    }
    for child in root.children.iter_mut() {
        if try_insert_under(child, parent_id, cell.clone()) {
            return true;
        }
    }
    false
}

/// A scope extracted from a model for selection-based editing.
///
/// `primary` and `edges` are mutable on apply; `context` is read-only
/// context for the LLM and must not be modified on apply.
#[derive(Debug, Clone, Default)]
pub struct Subgraph {
    pub primary: Vec<Cell>,
    pub edges: Vec<Cell>,
    pub context: Vec<Cell>,
    pub missing: Vec<String>,
}

fn find_in<'a>(cell: &'a Cell, id: &str) -> Option<&'a Cell> {
    if cell.id == id {
        return Some(cell);
    }
    for child in &cell.children {
        if let Some(found) = find_in(child, id) {
            return Some(found);
        }
    }
    None
}

fn find_in_mut<'a>(cell: &'a mut Cell, id: &str) -> Option<&'a mut Cell> {
    if cell.id == id {
        return Some(cell);
    }
    for child in &mut cell.children {
        if let Some(found) = find_in_mut(child, id) {
            return Some(found);
        }
    }
    None
}

fn collect_edges<'a>(cell: &'a Cell, out: &mut Vec<&'a Cell>) {
    if cell.edge {
        out.push(cell);
    }
    for child in &cell.children {
        collect_edges(child, out);
    }
}

fn count_ids(cell: &Cell, counts: &mut std::collections::HashMap<String, usize>) {
    *counts.entry(cell.id.clone()).or_insert(0) += 1;
    for child in &cell.children {
        count_ids(child, counts);
    }
}

// ---------------------------------------------------------------------------
// Serializer
// ---------------------------------------------------------------------------

fn xml_ser_err(e: quick_xml::Error) -> SerializeError {
    SerializeError::Xml(format!("{e:?}"))
}

fn write_diagram_uncompressed<W: Write>(
    writer: &mut Writer<W>,
    diagram: &Diagram,
) -> Result<(), SerializeError> {
    let mut elem = BytesStart::new("diagram");
    elem.push_attribute(("id", diagram.id.as_str()));
    elem.push_attribute(("name", diagram.name.as_str()));
    writer.write_event(Event::Start(elem)).map_err(xml_ser_err)?;

    if let Some(model) = &diagram.model {
        write_model(writer, model)?;
    }

    writer
        .write_event(Event::End(BytesEnd::new("diagram")))
        .map_err(xml_ser_err)?;
    Ok(())
}

fn write_diagram_compressed<W: Write>(
    writer: &mut Writer<W>,
    diagram: &Diagram,
) -> Result<(), SerializeError> {
    let mut elem = BytesStart::new("diagram");
    elem.push_attribute(("id", diagram.id.as_str()));
    elem.push_attribute(("name", diagram.name.as_str()));
    writer.write_event(Event::Start(elem)).map_err(xml_ser_err)?;

    if let Some(model) = &diagram.model {
        // Serialize the mxGraphModel to a string, then compress with raw
        // deflate + base64 to match Draw.io's on-disk format.
        let inner = serialize_model_to_string(model)?;
        let compressed = miniz_oxide::deflate::compress_to_vec(inner.as_bytes(), 6);
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&compressed);
        writer
            .write_event(Event::Text(BytesText::new(&encoded)))
            .map_err(xml_ser_err)?;
    }

    writer
        .write_event(Event::End(BytesEnd::new("diagram")))
        .map_err(xml_ser_err)?;
    Ok(())
}

fn serialize_model_to_string(model: &MxGraphModel) -> Result<String, SerializeError> {
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    write_model(&mut writer, model)?;
    String::from_utf8(writer.into_inner()).map_err(SerializeError::Utf8)
}

fn write_model<W: Write>(
    writer: &mut Writer<W>,
    model: &MxGraphModel,
) -> Result<(), SerializeError> {
    writer
        .write_event(Event::Start(BytesStart::new("mxGraphModel")))
        .map_err(xml_ser_err)?;
    writer
        .write_event(Event::Start(BytesStart::new("root")))
        .map_err(xml_ser_err)?;
    write_cell_recursive(writer, &model.root)?;
    writer
        .write_event(Event::End(BytesEnd::new("root")))
        .map_err(xml_ser_err)?;
    writer
        .write_event(Event::End(BytesEnd::new("mxGraphModel")))
        .map_err(xml_ser_err)?;
    Ok(())
}

fn write_cell_recursive<W: Write>(
    writer: &mut Writer<W>,
    cell: &Cell,
) -> Result<(), SerializeError> {
    write_cell(writer, cell)?;
    for child in &cell.children {
        write_cell_recursive(writer, child)?;
    }
    Ok(())
}

fn write_cell<W: Write>(
    writer: &mut Writer<W>,
    cell: &Cell,
) -> Result<(), SerializeError> {
    let mut elem = BytesStart::new("mxCell");
    elem.push_attribute(("id", cell.id.as_str()));
    if let Some(v) = &cell.value {
        elem.push_attribute(("value", v.as_str()));
    }
    if let Some(s) = &cell.style {
        elem.push_attribute(("style", s.as_str()));
    }
    if cell.vertex {
        elem.push_attribute(("vertex", "1"));
    }
    if cell.edge {
        elem.push_attribute(("edge", "1"));
    }
    if let Some(p) = &cell.parent {
        elem.push_attribute(("parent", p.as_str()));
    }
    if let Some(s) = &cell.source {
        elem.push_attribute(("source", s.as_str()));
    }
    if let Some(t) = &cell.target {
        elem.push_attribute(("target", t.as_str()));
    }

    if let Some(geom) = &cell.geometry {
        writer.write_event(Event::Start(elem)).map_err(xml_ser_err)?;
        write_geometry(writer, geom)?;
        writer
            .write_event(Event::End(BytesEnd::new("mxCell")))
            .map_err(xml_ser_err)?;
    } else {
        writer.write_event(Event::Empty(elem)).map_err(xml_ser_err)?;
    }
    Ok(())
}

fn write_geometry<W: Write>(
    writer: &mut Writer<W>,
    geom: &Geometry,
) -> Result<(), SerializeError> {
    let mut g = BytesStart::new("mxGeometry");
    g.push_attribute(("x", format_f64(geom.x).as_str()));
    g.push_attribute(("y", format_f64(geom.y).as_str()));
    g.push_attribute(("width", format_f64(geom.width).as_str()));
    g.push_attribute(("height", format_f64(geom.height).as_str()));
    g.push_attribute(("as", "geometry"));
    writer.write_event(Event::Empty(g)).map_err(xml_ser_err)?;
    Ok(())
}

/// Format f64 without trailing `.0` for whole numbers (Draw.io style),
/// preserving precision for fractional values.
fn format_f64(v: f64) -> String {
    if v.is_finite() && v == v.trunc() && v.abs() < 1e16 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cell {
    pub id: String,
    pub value: Option<String>,
    pub style: Option<String>,
    pub vertex: bool,
    pub edge: bool,
    pub parent: Option<String>,
    pub source: Option<String>,
    pub target: Option<String>,
    pub geometry: Option<Geometry>,
    pub children: Vec<Cell>,
}

impl Cell {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            value: None,
            style: None,
            vertex: false,
            edge: false,
            parent: None,
            source: None,
            target: None,
            geometry: None,
            children: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Geometry {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("xml: {0}")]
    Xml(String),
    #[error("decode (base64): {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("inflate: {0}")]
    Inflate(#[from] std::io::Error),
    #[error("schema: {0}")]
    Schema(String),
}

#[derive(Debug, Error)]
pub enum SerializeError {
    #[error("xml: {0}")]
    Xml(String),
    #[error("utf8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

impl MxFile {
    /// Parse an mxfile XML stream. Each `<diagram>` body is auto-decoded:
    /// - starts with `<`  -> uncompressed XML
    /// - otherwise       -> base64 + raw deflate (Draw.io's on-disk format)
    pub fn parse(mut reader: impl Read) -> Result<Self, ParseError> {
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(ParseError::Inflate)?;
        Self::parse_bytes(&bytes)
    }

    /// Parse from an in-memory byte slice. Useful when you already have
    /// the bytes (e.g. embedded assets, network responses).
    pub fn parse_bytes(bytes: &[u8]) -> Result<Self, ParseError> {
        let mut r = Reader::from_reader(bytes);
        r.config_mut().trim_text(true);

        let mut diagrams: Vec<Diagram> = Vec::new();
        let mut buf = Vec::new();

        loop {
            match r.read_event_into(&mut buf) {
                Ok(Event::Start(e)) if e.name().as_ref() == b"diagram" => {
                    let mut partial = PartialDiagram::from_attrs(&e)?;
                    let body = r
                        .read_text(QName(b"diagram"))
                        .map_err(|e| ParseError::Xml(format!("{e:?}")))?;
                    partial.body = body.into_owned();
                    diagrams.push(partial.build()?);
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    return Err(ParseError::Xml(format!(
                        "{e:?} at pos {}",
                        r.buffer_position()
                    )))
                }
                _ => {}
            }
            buf.clear();
        }

        Ok(MxFile { diagrams })
    }

    /// Serialize to uncompressed mxfile XML. Each diagram body is
    /// emitted as inline `<mxGraphModel>`.
    pub fn to_xml(&self) -> Result<String, SerializeError> {
        let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
        writer
            .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
            .map_err(xml_ser_err)?;
        writer
            .write_event(Event::Start(BytesStart::new("mxfile")))
            .map_err(xml_ser_err)?;

        for diagram in &self.diagrams {
            write_diagram_uncompressed(&mut writer, diagram)?;
        }

        writer
            .write_event(Event::End(BytesEnd::new("mxfile")))
            .map_err(xml_ser_err)?;
        String::from_utf8(writer.into_inner()).map_err(SerializeError::Utf8)
    }

    /// Serialize to mxfile XML with base64 + raw deflate compression on
    /// each diagram body (Draw.io's on-disk format).
    pub fn to_compressed_xml(&self) -> Result<String, SerializeError> {
        let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
        writer
            .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
            .map_err(xml_ser_err)?;
        writer
            .write_event(Event::Start(BytesStart::new("mxfile")))
            .map_err(xml_ser_err)?;

        for diagram in &self.diagrams {
            write_diagram_compressed(&mut writer, diagram)?;
        }

        writer
            .write_event(Event::End(BytesEnd::new("mxfile")))
            .map_err(xml_ser_err)?;
        String::from_utf8(writer.into_inner()).map_err(SerializeError::Utf8)
    }
}

#[derive(Default)]
struct PartialDiagram {
    id: String,
    name: String,
    body: String,
}

impl PartialDiagram {
    fn from_attrs(e: &BytesStart<'_>) -> Result<Self, ParseError> {
        let mut p = Self::default();
        for attr in e.attributes().flatten() {
            match attr.key.as_ref() {
                b"id" => p.id = attr_value(&attr.value),
                b"name" => p.name = attr_value(&attr.value),
                _ => {}
            }
        }
        Ok(p)
    }

    fn build(self) -> Result<Diagram, ParseError> {
        let trimmed = self.body.trim();
        let model_bytes = if trimmed.starts_with('<') {
            // Uncompressed: pass-through.
            trimmed.as_bytes().to_vec()
        } else {
            // Compressed: base64 + raw deflate (Draw.io's on-disk format).
            decode_compressed_body(trimmed)?
        };

        let model = parse_mxgraphmodel(&model_bytes)?;

        Ok(Diagram {
            id: self.id,
            name: self.name,
            model: Some(model),
        })
    }
}

fn decode_compressed_body(b64: &str) -> Result<Vec<u8>, ParseError> {
    use base64::Engine;
    let compressed = base64::engine::general_purpose::STANDARD.decode(b64)?;
    miniz_oxide::inflate::decompress_to_vec(&compressed).map_err(|e| {
        ParseError::Inflate(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("inflate: {e:?}"),
        ))
    })
}

fn parse_mxgraphmodel(xml: &[u8]) -> Result<MxGraphModel, ParseError> {
    let mut r = Reader::from_reader(xml);
    r.config_mut().trim_text(true);

    let mut cells: Vec<Cell> = Vec::new();
    let mut current_cell: Option<Cell> = None;
    let mut buf = Vec::new();

    loop {
        match r.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"mxCell" => {
                    current_cell = Some(parse_mxcell_attrs(&e)?);
                }
                b"mxGeometry" => {
                    if let Some(cell) = current_cell.as_mut() {
                        cell.geometry = Some(parse_geometry_attrs(&e)?);
                    }
                }
                _ => {}
            },
            Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"mxCell" => {
                    cells.push(parse_mxcell_attrs(&e)?);
                }
                b"mxGeometry" => {
                    if let Some(cell) = current_cell.as_mut() {
                        cell.geometry = Some(parse_geometry_attrs(&e)?);
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"mxCell" => {
                    if let Some(cell) = current_cell.take() {
                        cells.push(cell);
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(ParseError::Xml(format!(
                    "{e:?} at pos {}",
                    r.buffer_position()
                )))
            }
            _ => {}
        }
        buf.clear();
    }

    // Convention: cell id="0" is the synthetic root.
    let root_idx = cells
        .iter()
        .position(|c| c.id == "0")
        .ok_or_else(|| ParseError::Schema("missing root cell id=0".into()))?;

    let mut by_parent: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, c) in cells.iter().enumerate() {
        if i == root_idx {
            continue;
        }
        if let Some(p) = &c.parent {
            by_parent.entry(p.clone()).or_default().push(i);
        }
    }

    fn build_subtree(
        cells: &[Cell],
        by_parent: &HashMap<String, Vec<usize>>,
        parent_id: &str,
    ) -> Vec<Cell> {
        let Some(indices) = by_parent.get(parent_id) else {
            return Vec::new();
        };
        indices
            .iter()
            .map(|&i| {
                let mut cell = cells[i].clone();
                cell.children = build_subtree(cells, by_parent, &cell.id);
                cell
            })
            .collect()
    }

    let mut root = cells[root_idx].clone();
    root.children = build_subtree(&cells, &by_parent, &root.id);

    Ok(MxGraphModel { root })
}

fn parse_mxcell_attrs(e: &BytesStart<'_>) -> Result<Cell, ParseError> {
    let mut cell = Cell::new("");
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"id" => cell.id = attr_value(&attr.value),
            b"value" => cell.value = Some(attr_value(&attr.value)),
            b"style" => cell.style = Some(attr_value(&attr.value)),
            b"vertex" => cell.vertex = is_truthy(&attr.value),
            b"edge" => cell.edge = is_truthy(&attr.value),
            b"parent" => cell.parent = Some(attr_value(&attr.value)),
            b"source" => cell.source = Some(attr_value(&attr.value)),
            b"target" => cell.target = Some(attr_value(&attr.value)),
            _ => {}
        }
    }
    Ok(cell)
}

fn parse_geometry_attrs(e: &BytesStart<'_>) -> Result<Geometry, ParseError> {
    let mut g = Geometry {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
    };
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"x" => g.x = parse_f64(&attr.value)?,
            b"y" => g.y = parse_f64(&attr.value)?,
            b"width" => g.width = parse_f64(&attr.value)?,
            b"height" => g.height = parse_f64(&attr.value)?,
            _ => {}
        }
    }
    Ok(g)
}

fn attr_value(v: &[u8]) -> String {
    String::from_utf8_lossy(v).into_owned()
}

fn is_truthy(v: &[u8]) -> bool {
    matches!(v, b"1" | b"true")
}

fn parse_f64(v: &[u8]) -> Result<f64, ParseError> {
    let s = std::str::from_utf8(v).map_err(|e| ParseError::Schema(e.to_string()))?;
    s.parse::<f64>()
        .map_err(|e| ParseError::Schema(e.to_string()))
}
