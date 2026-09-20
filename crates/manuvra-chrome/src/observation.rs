use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub document_id: String,
    pub url: String,
    pub route: String,
    pub title: String,
    #[serde(default)]
    pub dialogs: Vec<String>,
    pub focused: Option<u64>,
    #[serde(default)]
    pub visible_text: String,
    #[serde(default)]
    pub covered_text: String,
    #[serde(default)]
    pub dialog_texts: BTreeMap<String, String>,
    #[serde(default)]
    pub elements: Vec<Element>,
    pub viewport: ViewportState,
    #[serde(default)]
    pub coverage: Coverage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Element {
    pub index: u64,
    pub node_id: u64,
    #[serde(default = "main_context")]
    pub context: String,
    pub role: String,
    pub name: String,
    pub input_type: Option<String>,
    pub value: String,
    pub checked: Option<bool>,
    pub selected: Option<bool>,
    pub expanded: Option<bool>,
    pub disabled: bool,
    pub in_dialog: Option<String>,
    #[serde(default)]
    pub operations: Vec<String>,
    #[serde(default)]
    pub select_options: Vec<SelectOption>,
    pub rect: Rect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectOption {
    pub node_id: u64,
    pub label: String,
    pub value: String,
    pub disabled: bool,
    pub selected: bool,
}

fn main_context() -> String {
    "main".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewportState {
    pub width: u32,
    pub height: u32,
    pub scroll_x: f64,
    pub scroll_y: f64,
    pub document_height: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    pub viewport_complete: bool,
    pub open_shadow_roots: bool,
    pub slots: bool,
    pub same_origin_frames: bool,
    #[serde(default)]
    pub gaps: Vec<String>,
}

impl Default for Coverage {
    fn default() -> Self {
        Self {
            viewport_complete: true,
            open_shadow_roots: true,
            slots: true,
            same_origin_frames: true,
            gaps: Vec::new(),
        }
    }
}
