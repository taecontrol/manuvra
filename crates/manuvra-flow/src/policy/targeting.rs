//! Targeting: the element, hover region, and provided value the provider's answers select on an
//! observation, and the native select option a `SELECT` needs.

use super::{Candidate, Policy, PolicyStop};
use crate::judgment::{ChoiceJudgment, Judgments, Operation, selected_operation};
use manuvra_chrome::{Element, HoverRegion, Observation, SelectOption};

pub(super) fn selected_target<'a>(
    observation: &'a Observation,
    judgments: &Judgments,
    operation: Operation,
) -> Result<&'a Element, PolicyStop> {
    selected_element(observation, judgments, operation)
        .ok_or(PolicyStop::Blocked("provider_invalid_response"))
}

pub(super) fn selected_element<'a>(
    observation: &'a Observation,
    judgments: &Judgments,
    operation: Operation,
) -> Option<&'a Element> {
    let answer: &ChoiceJudgment = match operation {
        Operation::Click => &judgments.click_target,
        Operation::TypeText => &judgments.type_target,
        Operation::Select => &judgments.select_target,
        _ => return None,
    };
    parse_target(observation, &answer.choice)
}

/// A click choice resolves to a visible element or to one hidden control to reveal.
#[derive(Debug)]
pub(crate) enum ClickChoice<'a> {
    Element(&'a Element),
    Reveal {
        region: &'a HoverRegion,
        offset: usize,
    },
}

pub(crate) fn reveal_key(region: u64, offset: usize) -> String {
    format!("R{region}_{}", offset + 1)
}

pub(crate) fn click_choice<'a>(observation: &'a Observation, key: &str) -> Option<ClickChoice<'a>> {
    if let Some(element) = parse_target(observation, key) {
        return Some(ClickChoice::Element(element));
    }
    reveal_choice(observation, key)
}

fn reveal_choice<'a>(observation: &'a Observation, key: &str) -> Option<ClickChoice<'a>> {
    let (index, offset) = parse_reveal_key(key)?;
    let region = observation
        .hover_regions
        .iter()
        .find(|region| region.index == index)?;
    if reveal_key(index, offset) != key
        || offset >= region.reveals_on_hover.len()
        || offset >= region.reveal_roles.len()
        || offset >= region.reveal_node_ids.len()
    {
        return None;
    }
    Some(ClickChoice::Reveal { region, offset })
}

fn parse_reveal_key(key: &str) -> Option<(u64, usize)> {
    let (region, ordinal) = key.strip_prefix('R')?.split_once('_')?;
    let index = region.parse::<u64>().ok()?;
    let offset = ordinal.parse::<usize>().ok()?.checked_sub(1)?;
    Some((index, offset))
}

pub(super) fn selected_reveal<'a>(
    observation: &'a Observation,
    judgments: &Judgments,
) -> Option<(&'a HoverRegion, usize)> {
    match click_choice(observation, &judgments.click_target.choice)? {
        ClickChoice::Reveal { region, offset } => Some((region, offset)),
        ClickChoice::Element(_) => None,
    }
}

pub(super) fn dispatched_operation(
    observation: &Observation,
    judgments: &Judgments,
) -> Result<Operation, PolicyStop> {
    let operation = selected_operation(judgments)
        .map_err(|_| PolicyStop::Blocked("provider_invalid_response"))?;
    if operation != Operation::Click {
        return Ok(operation);
    }
    match click_choice(observation, &judgments.click_target.choice) {
        Some(ClickChoice::Element(_)) => Ok(Operation::Click),
        Some(ClickChoice::Reveal { .. }) => Ok(Operation::Hover),
        None => Err(PolicyStop::Blocked("provider_invalid_response")),
    }
}

pub(super) fn selected_value(
    judgments: &Judgments,
    operation: Operation,
) -> Result<Option<String>, PolicyStop> {
    if !matches!(operation, Operation::TypeText | Operation::Select) {
        return Ok(None);
    }
    if judgments.type_value.choice == "NONE_FITS" {
        return Err(PolicyStop::Blocked("value_not_provided"));
    }
    Ok(Some(judgments.type_value.choice.clone()))
}

/// The enabled option of a native select whose value or label is the expected text.
pub(crate) fn offered_select_option<'a>(
    target: &'a Element,
    expected: &str,
) -> Option<&'a SelectOption> {
    target
        .select_options
        .iter()
        .find(|option| !option.disabled && (option.value == expected || option.label == expected))
}

fn parse_target<'a>(observation: &'a Observation, selected: &str) -> Option<&'a Element> {
    let index = selected.parse::<u64>().ok()?;
    observation
        .elements
        .iter()
        .find(|element| element.index == index)
}

impl Policy {
    /// A `SELECT` names a caller-provided value that its observed target offers as an enabled
    /// option; dispatch needs that option, so a page without it stops before a permit is minted.
    pub(super) fn check_select_value(
        &self,
        observation: &Observation,
        candidate: &Candidate,
    ) -> Result<(), PolicyStop> {
        if candidate.operation != Operation::Select {
            return Ok(());
        }
        let expected = candidate
            .value_name
            .as_ref()
            .and_then(|name| self.provided_values.get(name))
            .ok_or(PolicyStop::Blocked("value_not_provided"))?;
        candidate
            .target_index
            .and_then(|index| observation.elements.iter().find(|item| item.index == index))
            .and_then(|target| offered_select_option(target, expected))
            .map(|_| ())
            .ok_or(PolicyStop::Uncertain("select_option_unavailable"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Next;
    use crate::policy::tests::support::*;
    use manuvra_contract::JobOptions;

    #[test]
    fn native_select_is_authorized_only_for_an_offered_caller_value() {
        let offered = native_select(&[("Savings", "savings-id", false)]);
        for value in ["Savings", "savings-id"] {
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/")
                .with_provided_values(&provided(value));
            assert!(
                matches!(
                    decide_not_done(&mut policy, &step(), &offered, &judgments("SELECT"), false),
                    Next::Mutate(_)
                ),
                "{value}"
            );
        }
        for page in [
            native_select(&[("Checking", "checking-id", false)]),
            native_select(&[("Savings", "savings-id", true)]),
            observation("SELECT", "combobox"),
        ] {
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/")
                .with_provided_values(&provided("Savings"));
            assert!(matches!(
                decide_not_done(&mut policy, &step(), &page, &judgments("SELECT"), false),
                Next::Stop(PolicyStop::Uncertain("select_option_unavailable"))
            ));
            assert_eq!(policy.actions, 0);
        }
        let mut unknown = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(&mut unknown, &step(), &offered, &judgments("SELECT"), false),
            Next::Stop(PolicyStop::Blocked("value_not_provided"))
        ));
    }

    #[test]
    fn recorded_money_click_only_combobox_and_visible_option_remain_supported() {
        let mut control = observation("CLICK", "combobox");
        control.elements[0].name = "Currency or asset".into();
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &control, &judgments("CLICK"), false),
            Next::Mutate(_)
        ));
        let mut option = observation("CLICK", "option");
        option.elements[0].name = "+ New currency or asset…".into();
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &option, &judgments("CLICK"), false),
            Next::Mutate(_)
        ));
    }

    #[test]
    fn missing_or_unlisted_hover_target_is_an_invalid_provider_response() {
        for target in [
            None,
            Some("R9_1"),
            Some("R1_0"),
            Some("R1_2"),
            Some("R01_1"),
            Some("R1"),
            Some("R1_1x"),
            Some("999"),
        ] {
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
            assert!(
                matches!(
                    decide_not_done(&mut policy, &step(), &hover_page(), &hover(target), false),
                    Next::Stop(PolicyStop::Blocked("provider_invalid_response"))
                ),
                "{target:?}"
            );
            assert_eq!(policy.actions, 0);
        }
    }
    #[test]
    fn reveal_keys_round_trip_to_the_chosen_control_and_bare_hover_is_rejected() {
        let mut page = hover_page();
        page.hover_regions[0].reveals_on_hover.push("Delete".into());
        page.hover_regions[0].reveal_roles.push("button".into());
        page.hover_regions[0].reveal_node_ids.push(43);
        for (offset, name) in [(0, "Actions for Groceries"), (1, "Delete")] {
            let key = reveal_key(1, offset);
            let Some(ClickChoice::Reveal {
                region,
                offset: actual,
            }) = click_choice(&page, &key)
            else {
                panic!("reveal {key}");
            };
            assert_eq!(actual, offset);
            assert_eq!(region.reveals_on_hover[actual], name);
            let mut answer = hover(Some(&key));
            answer.click_target.confidence = 0.01;
            answer.operation.confidence = 0.60;
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
            assert_eq!(
                minted(decide_not_done(&mut policy, &step(), &page, &answer, false)).operation(),
                Operation::Hover
            );
            assert_eq!(
                policy.caller_candidate(&page, &answer).unwrap().operation,
                Operation::Hover
            );
        }
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let raw = judgments("HOVER");
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &page, &raw, false),
            Next::Stop(PolicyStop::Blocked("provider_invalid_response"))
        ));
        assert_eq!(
            policy.caller_candidate(&page, &raw),
            Err(PolicyStop::Blocked("provider_invalid_response"))
        );
        page.hover_regions[0].reveal_node_ids.clear();
        assert!(click_choice(&page, "R1_1").is_none());
    }
    #[test]
    fn reveal_resolution_requires_each_aligned_vector_to_cover_the_choice() {
        for missing in ["name", "role", "node"] {
            let mut page = hover_page();
            match missing {
                "name" => page.hover_regions[0].reveals_on_hover.clear(),
                "role" => page.hover_regions[0].reveal_roles.clear(),
                _ => page.hover_regions[0].reveal_node_ids.clear(),
            }
            assert!(click_choice(&page, "R1_1").is_none(), "{missing}");
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
            assert!(matches!(
                decide_not_done(&mut policy, &step(), &page, &hover(Some("R1_1")), false),
                Next::Stop(PolicyStop::Blocked("provider_invalid_response"))
            ));
            assert_eq!(
                policy.caller_candidate(&page, &hover(Some("R1_1"))).err(),
                Some(PolicyStop::Blocked("provider_invalid_response"))
            );
            assert_eq!(policy.actions, 0);
        }
    }
}
