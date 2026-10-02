use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const FOCUS_OBJECT_GROUP: &str = "manuvra-focus";

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
    pub focus_anchor: Option<FocusAnchor>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scroll_regions: Vec<ScrollRegion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay: Option<Overlay>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub scroll_regions_truncated: bool,
    /// Visible containers whose controls are hidden by opacity until hovered, in document order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hover_regions: Vec<HoverRegion>,
    /// More hover regions existed than the snapshot lists. This is not a coverage gap.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hover_regions_truncated: bool,
    /// CSSOM hover rules could not be read. Detection-only; not a coverage gap.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hover_rules_unreadable: bool,
}

/// A visible, vertically overflowing region. Node identities are browser-internal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScrollRegion {
    pub node_id: u64,
    pub name: String,
    pub overlay: Option<String>,
    #[serde(default)]
    pub overlay_node_id: Option<u64>,
    pub parent_node_id: Option<u64>,
    pub can_scroll_up: bool,
    pub can_scroll_down: bool,
    pub scroll_top: f64,
    pub scroll_height: f64,
    pub client_height: f64,
    pub rect: Rect,
}

/// The topmost routing scope; the browser identity stays internal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overlay {
    pub node_id: u64,
    pub name: String,
}

/// Dispatch-free identity used by action evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollTarget {
    pub name: String,
    pub overlay: Option<String>,
    pub document: bool,
}

impl ScrollRegion {
    pub fn target(&self) -> ScrollTarget {
        ScrollTarget {
            name: self.name.clone(),
            overlay: self.overlay.clone(),
            document: false,
        }
    }
}

/// A visible container holding controls that are hidden by opacity until the pointer is over it.
/// Hidden controls are reveal choices; policy hovers before judging the visible click.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoverRegion {
    pub index: u64,
    pub name: String,
    pub reveals_on_hover: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reveal_roles: Vec<String>,
    /// Hidden controls aligned with their names and roles. Internal dispatch identity.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reveal_node_ids: Vec<u64>,
    /// The first hidden control, retained for region observation compatibility.
    pub node_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusAnchor {
    pub node_id: u64,
    pub context: String,
    pub role: String,
    pub name: String,
    pub in_dialog: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    pub covered: bool,
    #[serde(default)]
    pub surface: Option<FocusSurface>,
    #[serde(default)]
    pub active_descendant: Option<ActiveDescendant>,
    #[serde(default)]
    pub expanded: Option<bool>,
    #[serde(default)]
    pub selected: Option<bool>,
    #[serde(default)]
    pub checked: Option<bool>,
    #[serde(default)]
    pub position: Option<u32>,
}

impl FocusAnchor {
    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        self.same_element(other) && self.same_state(other)
    }

    fn same_element(&self, other: &Self) -> bool {
        (
            self.node_id,
            &self.context,
            &self.role,
            &self.name,
            &self.in_dialog,
            &self.container,
            self.covered,
            self.surface,
            self.position,
        ) == (
            other.node_id,
            &other.context,
            &other.role,
            &other.name,
            &other.in_dialog,
            &other.container,
            other.covered,
            other.surface,
            other.position,
        )
    }

    fn same_state(&self, other: &Self) -> bool {
        (
            self.descendant_identity(),
            self.expanded,
            self.selected,
            self.checked,
        ) == (
            other.descendant_identity(),
            other.expanded,
            other.selected,
            other.checked,
        )
    }

    fn descendant_identity(&self) -> Option<(&str, &str, &str)> {
        self.active_descendant
            .as_ref()
            .map(|item| (item.id.as_str(), item.role.as_str(), item.name.as_str()))
    }
}

/// The page-side probe only sees roots created through `attachShadow`, so a
/// declarative closed root is found by asking CDP about the focused host.
pub(crate) fn mark_closed_shadow_focus<E>(
    observation: &mut Observation,
    mut command: impl FnMut(&str, Value) -> Result<Value, E>,
) -> Result<(), E> {
    let Some(anchor) = observation
        .focus_anchor
        .as_mut()
        .filter(|anchor| anchor.covered)
    else {
        return Ok(());
    };
    let object = command(
        "Runtime.evaluate",
        json!({
            "expression": format!("window.__manuvra?.nodes?.get({})", anchor.node_id),
            "objectGroup": FOCUS_OBJECT_GROUP
        }),
    )?;
    let Some(object_id) = object.pointer("/result/objectId").and_then(Value::as_str) else {
        return Ok(());
    };
    let described = command(
        "DOM.describeNode",
        json!({"objectId": object_id, "pierce": true, "depth": 1}),
    );
    let _ = command(
        "Runtime.releaseObjectGroup",
        json!({"objectGroup": FOCUS_OBJECT_GROUP}),
    );
    if hosts_closed_shadow_root(&described?) {
        anchor.covered = false;
        anchor.surface = Some(FocusSurface::ClosedShadowRoot);
    }
    Ok(())
}

fn hosts_closed_shadow_root(described: &Value) -> bool {
    described
        .pointer("/node/shadowRoots")
        .and_then(Value::as_array)
        .is_some_and(|roots| roots.iter().any(|root| root["shadowRootType"] == "closed"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveDescendant {
    pub id: String,
    pub role: String,
    pub name: String,
    pub selected: Option<bool>,
    pub checked: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusSurface {
    CrossOriginFrame,
    ClosedShadowRoot,
    Canvas,
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shares_name: bool,
    pub in_dialog: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
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

    fn observed(covered: bool) -> Observation {
        serde_json::from_value(json!({
            "document_id":"d","url":"http://example.test/","route":"/","title":"x","focused":null,
            "focus_anchor":{"node_id":7,"context":"main","role":"interactive","name":"",
                "in_dialog":null,"covered":covered},
            "viewport":{"width":10,"height":10,"scroll_x":0,"scroll_y":0,"document_height":10}
        }))
        .unwrap()
    }

    #[test]
    fn closed_shadow_focus_asks_cdp_only_about_a_covered_anchor() {
        let mut uncovered = observed(false);
        let mut calls = Vec::new();
        mark_closed_shadow_focus(&mut uncovered, |method, _| {
            calls.push(method.to_owned());
            Ok::<_, ()>(Value::Null)
        })
        .unwrap();
        assert!(calls.is_empty());
        assert_eq!(uncovered.focus_anchor.unwrap().surface, None);

        let mut covered = observed(true);
        let result = mark_closed_shadow_focus(&mut covered, |method, params| {
            calls.push(method.to_owned());
            match method {
                "Runtime.evaluate" => {
                    assert_eq!(params["objectGroup"], FOCUS_OBJECT_GROUP);
                    Ok(json!({"result":{"objectId":"focus-7"}}))
                }
                "DOM.describeNode" => Err("detached"),
                _ => Ok(Value::Null),
            }
        });
        assert_eq!(result, Err("detached"));
        assert_eq!(
            calls,
            [
                "Runtime.evaluate",
                "DOM.describeNode",
                "Runtime.releaseObjectGroup"
            ]
        );
        assert!(covered.focus_anchor.unwrap().covered);
    }

    #[test]
    fn declarative_closed_root_uncovers_the_focus_anchor() {
        for (roots, covered) in [
            (json!([{"shadowRootType":"closed"}]), false),
            (json!([{"shadowRootType":"open"}]), true),
            (json!([]), true),
        ] {
            let mut observation = observed(true);
            mark_closed_shadow_focus(&mut observation, |method, _| {
                Ok::<_, ()>(match method {
                    "Runtime.evaluate" => json!({"result":{"objectId":"focus-7"}}),
                    _ => json!({"node":{"shadowRoots":roots}}),
                })
            })
            .unwrap();
            let anchor = observation.focus_anchor.unwrap();
            assert_eq!(anchor.covered, covered, "{roots}");
            assert_eq!(
                anchor.surface,
                (!covered).then_some(FocusSurface::ClosedShadowRoot)
            );
        }
    }

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
        assert!(!observation.hover_rules_unreadable);
        let serialized = serde_json::to_value(&observation).unwrap();
        assert!(serialized.get("hover_regions").is_none());
        assert!(serialized.get("hover_regions_truncated").is_none());
        assert!(serialized.get("hover_rules_unreadable").is_none());
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
                reveal_roles: Vec::new(),
                reveal_node_ids: Vec::new(),
            }]
        );
        assert!(observation.hover_regions_truncated);
        assert!(observation.coverage.viewport_complete);
    }
    #[test]
    fn legacy_populated_controls_focus_and_regions_omit_new_default_fields() {
        let mut legacy = snapshot();
        legacy["elements"] = json!([{"index":1,"node_id":7,"role":"button","name":"Edit","value":"","input_type":null,"checked":null,"selected":null,"expanded":null,"disabled":false,"in_dialog":null,"rect":{"x":0,"y":0,"width":10,"height":10}}]);
        legacy["focus_anchor"] = json!({"node_id":7,"context":"main","role":"button","name":"Edit","in_dialog":null,"covered":true});
        legacy["hover_regions"] =
            json!([{"index":1,"name":"Alpha","reveals_on_hover":["Edit"],"node_id":7}]);
        let observed: Observation = serde_json::from_value(legacy).unwrap();
        let serialized = serde_json::to_value(&observed).unwrap();
        for field in ["container", "shares_name"] {
            assert!(serialized["elements"][0].get(field).is_none(), "{field}");
        }
        assert!(serialized["focus_anchor"].get("container").is_none());
        for field in ["reveal_roles", "reveal_node_ids"] {
            assert!(
                serialized["hover_regions"][0].get(field).is_none(),
                "{field}"
            );
        }
    }
}
