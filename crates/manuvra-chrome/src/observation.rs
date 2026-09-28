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
    /// Visible containers whose controls are hidden by opacity until hovered, in document order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hover_regions: Vec<HoverRegion>,
    /// More hover regions existed than the snapshot lists. This is not a coverage gap.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hover_regions_truncated: bool,
}

/// A visible container holding controls that are hidden by opacity until the pointer is over it.
/// Its hidden controls are never candidates; hovering the region reveals them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoverRegion {
    pub index: u64,
    pub name: String,
    pub reveals_on_hover: Vec<String>,
    /// The first hidden control, whose center is the hover point. Internal to dispatch; never
    /// persisted or published.
    pub node_id: u64,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot() -> serde_json::Value {
        json!({
            "document_id":"d",
            "url":"http://127.0.0.1/",
            "route":"/",
            "title":"Plan",
            "focused":null,
            "viewport":{"width":1120,"height":780,"scroll_x":0.0,"scroll_y":0.0,"document_height":780.0}
        })
    }

    #[test]
    fn observation_without_hover_fields_has_no_regions() {
        let observation: Observation = serde_json::from_value(snapshot()).unwrap();

        assert!(observation.hover_regions.is_empty());
        assert!(!observation.hover_regions_truncated);
        let serialized = serde_json::to_value(&observation).unwrap();
        assert!(serialized.get("hover_regions").is_none());
        assert!(serialized.get("hover_regions_truncated").is_none());
    }

    #[test]
    fn observation_reads_hover_regions_and_the_cap_flag() {
        let mut value = snapshot();
        value["hover_regions"] = json!([
            {"index":1,"name":"Groceries","reveals_on_hover":["Actions for Groceries"],"node_id":42}
        ]);
        value["hover_regions_truncated"] = json!(true);

        let observation: Observation = serde_json::from_value(value).unwrap();

        assert_eq!(
            observation.hover_regions,
            [HoverRegion {
                index: 1,
                name: "Groceries".into(),
                reveals_on_hover: vec!["Actions for Groceries".into()],
                node_id: 42,
            }]
        );
        assert!(observation.hover_regions_truncated);
        assert!(observation.coverage.viewport_complete);
    }
}
