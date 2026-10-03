//! Text sources and completeness for legacy defaults and the strict painted opt-in.
use super::{matching_count, scope_complete, scoped_text};
use manuvra_chrome::{Observation, TextInventory};
use manuvra_contract::{
    AssertionScope, TextAssertion, TextChannel, TextCheckEvidence, TextFailure, VerdictResult,
};

pub(super) fn evaluate(assertion: TextAssertion, observation: &Observation) -> TextCheckEvidence {
    let searched_channels = channels(&assertion);
    let mut check = TextCheckEvidence {
        assertion,
        result: VerdictResult::Unresolved,
        searched_channels,
        matched_channel: None,
        reason: None,
    };
    match search(&check.assertion, observation) {
        Ok((matched, complete)) => {
            check.matched_channel = matched;
            if matched.is_none() && !complete {
                check.reason = Some(TextFailure::IncompleteCoverage);
            } else {
                let presence = matches!(check.assertion, TextAssertion::Visible(_));
                check.result = if matched.is_some() == presence {
                    VerdictResult::Satisfied
                } else {
                    VerdictResult::NotSatisfied
                };
            }
        }
        Err(reason) => check.reason = Some(reason),
    }
    check
}

fn channels(assertion: &TextAssertion) -> Vec<TextChannel> {
    if assertion.include_aria_hidden() {
        vec![TextChannel::Accessible, TextChannel::PaintedAriaHidden]
    } else {
        vec![legacy_channel(assertion.scope())]
    }
}

fn legacy_channel(scope: Option<&AssertionScope>) -> TextChannel {
    match scope {
        Some(AssertionScope::Dialog(_)) => TextChannel::DialogText,
        _ => TextChannel::Accessible,
    }
}

fn search(
    assertion: &TextAssertion,
    observation: &Observation,
) -> Result<(Option<TextChannel>, bool), TextFailure> {
    if !unique_scope(observation, assertion.scope()) {
        return Err(TextFailure::AmbiguousOrMissingScope);
    }
    if !assertion.include_aria_hidden() {
        let text = scoped_text(observation, assertion.scope())
            .ok_or(TextFailure::AmbiguousOrMissingScope)?;
        let channel = legacy_channel(assertion.scope());
        return Ok((
            text.contains(assertion.text()).then_some(channel),
            scope_complete(observation, assertion.scope()),
        ));
    }
    let inventory =
        inventory(observation, assertion.scope()).ok_or(TextFailure::IncompleteCoverage)?;
    let matched = if inventory.accessible.contains(assertion.text()) {
        Some(TextChannel::Accessible)
    } else if inventory.painted_aria_hidden.contains(assertion.text()) {
        Some(TextChannel::PaintedAriaHidden)
    } else {
        None
    };
    Ok((
        matched,
        inventory.complete && painted_coverage_complete(observation),
    ))
}

fn unique_scope(observation: &Observation, scope: Option<&AssertionScope>) -> bool {
    match scope {
        Some(AssertionScope::Dialog(wanted)) => {
            matching_count(&observation.dialogs, &wanted.dialog) == 1
        }
        _ => true,
    }
}

fn inventory<'a>(
    observation: &'a Observation,
    scope: Option<&AssertionScope>,
) -> Option<&'a TextInventory> {
    let painted = observation.painted_text.as_ref()?;
    match scope {
        Some(AssertionScope::Dialog(wanted)) => {
            let mut matches = painted
                .dialogs
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case(&wanted.dialog));
            let (_, inventory) = matches.next()?;
            matches.next().is_none().then_some(inventory)
        }
        _ => Some(&painted.viewport),
    }
}

fn legacy_text_truncation(gap: &str) -> bool {
    matches!(
        gap,
        "visible_text_truncated" | "covered_text_truncated" | "dialog_text_truncated"
    )
}

fn painted_coverage_complete(observation: &Observation) -> bool {
    let coverage = &observation.coverage;
    let legacy_truncation = coverage.gaps.iter().any(|gap| legacy_text_truncation(gap));
    (coverage.viewport_complete || legacy_truncation) && traversed(observation)
}

fn traversed(observation: &Observation) -> bool {
    let coverage = &observation.coverage;
    coverage.open_shadow_roots
        && coverage.slots
        && coverage.same_origin_frames
        && coverage.gaps.iter().all(|gap| legacy_text_truncation(gap))
}
