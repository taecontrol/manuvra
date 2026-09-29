//! Targeting: the element, hover region, and provided value the provider's answers select on an
//! observation, and the native select option a `SELECT` needs.

use super::{Candidate, Policy, PolicyStop};
use crate::judgment::{ChoiceJudgment, Judgments, Operation, hover_region_key};
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

/// The listed hover region named by the `hover_target` answer, if any.
pub(super) fn selected_hover_region<'a>(
    observation: &'a Observation,
    judgments: &Judgments,
) -> Option<&'a HoverRegion> {
    let choice = &judgments.hover_target.as_ref()?.choice;
    observation
        .hover_regions
        .iter()
        .find(|region| hover_region_key(region) == *choice)
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
        for target in [None, Some("R9"), Some("1"), Some("2")] {
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
}
