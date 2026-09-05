//! Draw.io XML core: parse, query, mutate, validate, serialize.
//!
//! See crate-level README for status. Phase 1 work proceeds by TDD.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

use std::collections::HashMap;
use std::io::Read;

use quick_xml::events::{BytesStart, Event};
use quick_xml::name::QName;
use quick_xml::reader::Reader;
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
