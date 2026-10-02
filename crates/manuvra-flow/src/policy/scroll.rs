//! Deterministic scroll routing; browser facts describe regions, policy chooses one.
use crate::judgment::Operation;
use manuvra_chrome::{Observation, ScrollRegion};

#[derive(Debug, PartialEq)]
pub(crate) enum ScrollRoute<'a> {
    Window,
    Region(&'a ScrollRegion),
    None,
    Tied,
}

pub(crate) fn route(observation: &Observation, operation: Operation) -> ScrollRoute<'_> {
    if window_moves(observation, operation) {
        return ScrollRoute::Window;
    }
    if observation.scroll_regions_truncated {
        return ScrollRoute::Tied;
    }
    let movable: Vec<_> = observation
        .scroll_regions
        .iter()
        .filter(|r| region_moves(r, operation))
        .collect();
    let mut outermost = movable
        .iter()
        .copied()
        .filter(|r| !has_movable_ancestor(observation, r, &movable));
    match (outermost.next(), outermost.next()) {
        (Some(region), None) => ScrollRoute::Region(region),
        (None, _) => ScrollRoute::None,
        _ => ScrollRoute::Tied,
    }
}

fn window_moves(observation: &Observation, operation: Operation) -> bool {
    match operation {
        Operation::ScrollUp => observation.viewport.scroll_y > 0.0,
        Operation::ScrollDown => {
            observation.viewport.scroll_y + f64::from(observation.viewport.height)
                < observation.viewport.document_height
        }
        _ => false,
    }
}

fn region_moves(region: &ScrollRegion, operation: Operation) -> bool {
    match operation {
        Operation::ScrollUp => region.can_scroll_up,
        Operation::ScrollDown => region.can_scroll_down,
        _ => false,
    }
}

fn has_movable_ancestor(
    observation: &Observation,
    region: &ScrollRegion,
    movable: &[&ScrollRegion],
) -> bool {
    let mut parent = region.parent_node_id;
    for _ in 0..observation.scroll_regions.len() {
        let Some(id) = parent else { return false };
        if movable.iter().any(|r| r.node_id == id) {
            return true;
        }
        parent = observation
            .scroll_regions
            .iter()
            .find(|r| r.node_id == id)
            .and_then(|r| r.parent_node_id);
    }
    // A malformed ancestor cycle cannot authorize a region.
    parent.is_some()
}

pub(crate) fn positions(observation: &Observation, region: &ScrollRegion) -> serde_json::Value {
    let mut values = Vec::new();
    let mut current = Some(region);
    for _ in 0..observation.scroll_regions.len() {
        let Some(r) = current else { break };
        values
            .push(serde_json::json!({"name":r.name,"overlay":r.overlay,"scroll_top":r.scroll_top}));
        current = r
            .parent_node_id
            .and_then(|id| observation.scroll_regions.iter().find(|r| r.node_id == id));
    }
    values.push(serde_json::json!({"document":true,"scroll_top":observation.viewport.scroll_y}));
    values.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::tests::support::observation;
    use manuvra_chrome::Rect;
    fn region(id: u64, parent: Option<u64>) -> ScrollRegion {
        ScrollRegion {
            node_id: id,
            parent_node_id: parent,
            name: format!("Region {id}"),
            overlay: None,
            can_scroll_up: false,
            can_scroll_down: true,
            scroll_top: 0.0,
            scroll_height: 1000.0,
            client_height: 300.0,
            rect: Rect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 300.0,
            },
        }
    }
    #[test]
    fn document_routes_window_then_one_outermost_region_and_stops_ties() {
        let mut page = observation("CLICK", "button");
        assert_eq!(route(&page, Operation::ScrollDown), ScrollRoute::None);
        page.scroll_regions = vec![region(1, None), region(2, Some(1))];
        assert_eq!(
            route(&page, Operation::ScrollDown),
            ScrollRoute::Region(&page.scroll_regions[0])
        );
        page.viewport.document_height = 1000.0;
        page.scroll_regions_truncated = true;
        assert_eq!(route(&page, Operation::ScrollDown), ScrollRoute::Window);
        page.viewport.scroll_y = 900.0;
        assert_eq!(route(&page, Operation::ScrollDown), ScrollRoute::Tied);
        page.scroll_regions_truncated = false;
        page.scroll_regions[0].can_scroll_down = false;
        assert_eq!(
            route(&page, Operation::ScrollDown),
            ScrollRoute::Region(&page.scroll_regions[1])
        );
        page.scroll_regions.push(region(3, None));
        assert_eq!(route(&page, Operation::ScrollDown), ScrollRoute::Tied);
        page.scroll_regions.clear();
        assert_eq!(route(&page, Operation::ScrollDown), ScrollRoute::None);
        assert_eq!(route(&page, Operation::ScrollUp), ScrollRoute::Window);
    }
}
