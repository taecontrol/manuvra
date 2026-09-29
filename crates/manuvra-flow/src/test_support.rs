//! Builders shared by the unit tests of more than one module.

use manuvra_chrome::HoverRegion;

/// A listed hover region whose only hidden control is `Actions for <name>`.
pub(crate) fn hover_region(index: u64, name: &str, node_id: u64) -> HoverRegion {
    HoverRegion {
        index,
        name: name.into(),
        reveals_on_hover: vec![format!("Actions for {name}")],
        node_id,
    }
}
