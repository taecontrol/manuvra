//! Request-level contention owns the scope of container display, click wording, and target gating.
use manuvra_chrome::{Element, Observation};

pub(crate) fn contested(observation: &Observation) -> bool {
    !observation.hover_regions.is_empty()
        || observation.elements.iter().any(|e| e.shares_name)
        || singleton_context(observation)
}

pub(crate) fn container_for<'a>(
    element: &'a Element,
    observation: &Observation,
) -> Option<&'a str> {
    (contested(observation)
        && (element.shares_name || (singleton_context(observation) && contextual_click(element))))
    .then_some(element.container.as_deref())
    .flatten()
}

/// A sole click target can be the wrong virtualized item despite having no rendered twin.
pub(crate) fn singleton_context(observation: &Observation) -> bool {
    if !observation.hover_regions.is_empty() {
        return false;
    }
    let mut clicks = observation
        .elements
        .iter()
        .filter(|e| e.operations.iter().any(|op| op == "CLICK"));
    clicks.next().is_some_and(contextual_click) && clicks.next().is_none()
}

fn contextual_click(element: &Element) -> bool {
    element.container.is_some() && element.operations.iter().any(|op| op == "CLICK")
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::hover_region;

    #[test]
    fn contention_and_display_include_unique_contained_clicks_but_leave_fields_alone() {
        for regions in [false, true] {
            for twins in [false, true] {
                for click in [false, true] {
                    let mut page: Observation = serde_json::from_value(serde_json::json!({
                    "document_id":"d","url":"http://example.test/","route":"/","title":"Ready","focused":null,
                    "viewport":{"width":1120,"height":780,"scroll_x":0,"scroll_y":0,"document_height":780},
                    "elements":[{"index":1,"node_id":7,"role":"button","name":"Edit","value":"","input_type":null,"checked":null,"selected":null,"expanded":null,"disabled":false,"in_dialog":null,"rect":{"x":0,"y":0,"width":10,"height":10}}]
                })).unwrap();
                    let control = &mut page.elements[0];
                    control.container = Some("Alpha".into());
                    control.shares_name = twins;
                    control.operations = vec![if click { "CLICK" } else { "TYPE_TEXT" }.into()];

                    if regions {
                        page.hover_regions.push(hover_region(1, "Bravo", 8));
                    }
                    assert_eq!(contested(&page), regions || twins || click);
                    assert_eq!(
                        container_for(&page.elements[0], &page),
                        (twins || (click && !regions)).then_some("Alpha")
                    );
                }
            }
        }
    }
    #[test]
    fn singleton_context_requires_a_container_and_exactly_one_click_choice() {
        let original: Observation = serde_json::from_value(serde_json::json!({
            "document_id":"d","url":"http://example.test/","route":"/","title":"Ready","focused":null,
            "viewport":{"width":1120,"height":780,"scroll_x":0,"scroll_y":0,"document_height":780},
            "elements":[{"index":1,"node_id":7,"role":"button","name":"Edit","operations":["CLICK"],"value":"","input_type":null,"checked":null,"selected":null,"expanded":null,"disabled":false,"in_dialog":null,"rect":{"x":0,"y":0,"width":10,"height":10}}]
        })).unwrap();
        assert!(!singleton_context(&original));
        assert!(!contested(&original));
        let mut one = original.clone();
        one.elements[0].container = Some("Alpha".into());
        assert!(singleton_context(&one));
        assert_eq!(container_for(&one.elements[0], &one), Some("Alpha"));
        let mut many = one.clone();
        let mut second = many.elements[0].clone();
        second.index = 2;
        second.node_id = 8;
        second.name = "Delete".into();
        many.elements.push(second);
        assert!(!singleton_context(&many));
        assert!(!contested(&many));
        assert_eq!(container_for(&many.elements[0], &many), None);
    }
}
