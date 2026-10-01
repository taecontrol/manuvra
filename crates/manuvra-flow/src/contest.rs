//! Request-level contention owns the scope of container display, click wording, and target gating.
use manuvra_chrome::{Element, Observation};

pub(crate) fn contested(observation: &Observation) -> bool {
    !observation.hover_regions.is_empty() || observation.elements.iter().any(|e| e.shares_name)
}

pub(crate) fn container_for<'a>(
    element: &'a Element,
    observation: &Observation,
) -> Option<&'a str> {
    (contested(observation) && element.shares_name)
        .then_some(element.container.as_deref())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::hover_region;

    #[test]
    fn contention_depends_on_regions_or_rendered_twins_and_display_only_on_twins() {
        for regions in [false, true] {
            for twins in [false, true] {
                let mut page: Observation = serde_json::from_value(serde_json::json!({
                    "document_id":"d","url":"http://example.test/","route":"/","title":"Ready","focused":null,
                    "viewport":{"width":1120,"height":780,"scroll_x":0,"scroll_y":0,"document_height":780},
                    "elements":[{"index":1,"node_id":7,"role":"button","name":"Edit","value":"","input_type":null,"checked":null,"selected":null,"expanded":null,"disabled":false,"in_dialog":null,"rect":{"x":0,"y":0,"width":10,"height":10}}]
                })).unwrap();
                let control = &mut page.elements[0];
                control.container = Some("Alpha".into());
                control.shares_name = twins;

                if regions {
                    page.hover_regions.push(hover_region(1, "Bravo", 8));
                }
                assert_eq!(contested(&page), regions || twins);
                assert_eq!(
                    container_for(&page.elements[0], &page),
                    twins.then_some("Alpha")
                );
            }
        }
    }
}
