//! Public serialize types returned across the IPC boundary for graph /
//! tag / link queries. Kept in one place so sub-modules don't have to
//! cross-import structs.

use serde::Serialize;

#[derive(Debug, Serialize, Clone)]
pub struct BacklinkEntry {
    pub title: String,
    pub path: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct NoteLinkEntry {
    pub target: String,
    pub link_text: String,
}

#[derive(Debug, Serialize, Clone, specta::Type)]
pub struct NoteIndexEntry {
    pub id: String,
    pub title: String,
    pub path: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct GraphStats {
    pub notes: usize,
    pub links: usize,
    pub tags: usize,
}

#[derive(Debug, Serialize, Clone)]
pub struct GraphNode {
    pub id: String,
    pub title: String,
    pub path: String,
    /// Kind of node — "note" or "tag". Frontend uses this for styling
    /// (tag nodes render green, smaller, click-disabled).
    pub kind: String,
    pub link_count: usize,
}

#[derive(Debug, Serialize, Clone)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct FullGraph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}
