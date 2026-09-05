//! Draw.io XML core: parse, query, mutate, validate, serialize.
//!
//! See crate-level README for status. Phase 1 work proceeds by TDD.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

use std::collections::HashMap;
use std::io::BufRead;

use quick_xml::events::{BytesStart, Event};
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
    /// Parse an mxfile XML stream. Auto-detects compressed (base64+deflate)
    /// vs uncompressed diagram bodies.
    ///
    /// TDD scope (Phase 1, iteration 1): uncompressed diagrams only.
    pub fn parse(reader: impl BufRead) -> Result<Self, ParseError> {
        let mut r = Reader::from_reader(reader);
        r.config_mut().trim_text(true);

        let mut diagrams: Vec<Diagram> = Vec::new();
        let mut current: Option<DiagramBuilder> = None;
        let mut buf = Vec::new();

        loop {
            match r.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => match e.name().as_ref() {
                    b"diagram" => {
                        current = Some(DiagramBuilder::from_attrs(&e)?);
                    }
                    b"mxCell" => {
                        if let Some(b) = current.as_mut() {
                            b.begin_cell(parse_mxcell_attrs(&e)?);
                        }
                    }
                    b"mxGeometry" => {
                        if let Some(b) = current.as_mut() {
                            if let Some(cell) = b.current_cell.as_mut() {
                                cell.geometry = Some(parse_geometry_attrs(&e)?);
                            }
                        }
                    }
                    _ => {}
                },
                Ok(Event::Empty(e)) => match e.name().as_ref() {
                    b"mxCell" => {
                        if let Some(b) = current.as_mut() {
                            b.cells.push(parse_mxcell_attrs(&e)?);
                        }
                    }
                    b"mxGeometry" => {
                        // Self-closing <mxGeometry .../> inside a currently-open cell.
                        if let Some(b) = current.as_mut() {
                            if let Some(cell) = b.current_cell.as_mut() {
                                cell.geometry = Some(parse_geometry_attrs(&e)?);
                            }
                        }
                    }
                    _ => {}
                },
                Ok(Event::End(e)) => match e.name().as_ref() {
                    b"mxCell" => {
                        if let Some(b) = current.as_mut() {
                            if let Some(cell) = b.current_cell.take() {
                                b.cells.push(cell);
                            }
                        }
                    }
                    b"diagram" => {
                        if let Some(b) = current.take() {
                            diagrams.push(b.build()?);
                        }
                    }
                    _ => {}
                },
                Ok(Event::Eof) => break,
                Err(e) => return Err(ParseError::Xml(format!("{e:?} at pos {}", r.buffer_position()))),
                _ => {}
            }
            buf.clear();
        }

        Ok(MxFile { diagrams })
    }
}

#[derive(Default)]
struct DiagramBuilder {
    id: String,
    name: String,
    cells: Vec<Cell>,
    /// Holds a cell while its (possibly nested) geometry is being parsed.
    current_cell: Option<Cell>,
}

impl DiagramBuilder {
    fn from_attrs(e: &BytesStart<'_>) -> Result<Self, ParseError> {
        let mut b = Self::default();
        for attr in e.attributes().flatten() {
            match attr.key.as_ref() {
                b"id" => b.id = attr_value(&attr.value),
                b"name" => b.name = attr_value(&attr.value),
                _ => {}
            }
        }
        Ok(b)
    }

    fn begin_cell(&mut self, cell: Cell) {
        self.current_cell = Some(cell);
    }

    fn build(self) -> Result<Diagram, ParseError> {
        // Convention: cell id="0" is the synthetic root.
        let root_idx = self
            .cells
            .iter()
            .position(|c| c.id == "0")
            .ok_or_else(|| ParseError::Schema("missing root cell id=0".into()))?;

        // Group cells by parent id (skipping the root itself).
        let mut by_parent: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, c) in self.cells.iter().enumerate() {
            if i == root_idx {
                continue;
            }
            if let Some(p) = &c.parent {
                by_parent.entry(p.clone()).or_default().push(i);
            }
        }

        // Recursive subtree build.
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

        let mut root = self.cells[root_idx].clone();
        root.children = build_subtree(&self.cells, &by_parent, &root.id);

        Ok(Diagram {
            id: self.id,
            name: self.name,
            model: Some(MxGraphModel { root }),
        })
    }
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
    let mut g = Geometry { x: 0.0, y: 0.0, width: 0.0, height: 0.0 };
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
    s.parse::<f64>().map_err(|e| ParseError::Schema(e.to_string()))
}
