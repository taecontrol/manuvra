//! Unique-owner resolution and foreground comparisons. Browser identities never leave here.
use super::{DoneResult, viewport_complete};
use manuvra_chrome::{ColorObservation, ColorScope, ColorScopeKind, Observation};
use manuvra_contract::{
    ColorChannel, ColorCheck, ColorCheckEvidence, ColorComparator, ColorFailure, ColorTarget,
    ColorTargetEvidence, VerdictResult,
};
use std::collections::BTreeMap;

pub(super) fn evaluate(check: &ColorCheck, observation: &Observation) -> ColorCheckEvidence {
    let target = resolve(check.target(), observation);
    let reference = check.reference().map(|wanted| resolve(wanted, observation));
    let failures = [
        target.reason,
        reference.as_ref().and_then(|item| item.reason),
    ];
    let reason = failures
        .into_iter()
        .flatten()
        .min_by_key(|reason| !absent(*reason));
    let result = match reason {
        Some(reason) if absent(reason) => VerdictResult::NotSatisfied,
        Some(_) => VerdictResult::Unresolved,
        None => compare(
            check,
            target.rgba,
            reference.as_ref().and_then(|item| item.rgba),
        ),
    };
    ColorCheckEvidence {
        target,
        reference,
        result,
        reason,
        comparator: comparator(check),
        equals: match check {
            ColorCheck::Equals(check) => Some(check.equals.clone()),
            _ => None,
        },
        tolerance: check.tolerance(),
    }
}

fn comparator(check: &ColorCheck) -> ColorComparator {
    match check {
        ColorCheck::Equals(_) => ColorComparator::Equals,
        ColorCheck::SameAs(_) => ColorComparator::SameAs,
        ColorCheck::DifferentFrom(_) => ColorComparator::DifferentFrom,
    }
}

fn absent(reason: ColorFailure) -> bool {
    matches!(reason, ColorFailure::Missing | ColorFailure::Transparent)
}

fn compare(
    check: &ColorCheck,
    target: Option<[u8; 4]>,
    reference: Option<[u8; 4]>,
) -> VerdictResult {
    let reference = match check {
        ColorCheck::Equals(check) => check.rgba(),
        _ => reference,
    };
    let (Some(target), Some(reference)) = (target, reference) else {
        return VerdictResult::Unresolved;
    };
    let equal = target
        .into_iter()
        .zip(reference)
        .all(|(left, right)| left.abs_diff(right) <= check.tolerance());
    let satisfied = if matches!(check, ColorCheck::DifferentFrom(_)) {
        !equal
    } else {
        equal
    };
    if satisfied {
        VerdictResult::Satisfied
    } else {
        VerdictResult::NotSatisfied
    }
}

pub(super) fn outcome(result: VerdictResult) -> DoneResult {
    match result {
        VerdictResult::Satisfied => DoneResult::Satisfied,
        VerdictResult::NotSatisfied => DoneResult::NotSatisfied,
        VerdictResult::Unresolved | VerdictResult::NotRun => DoneResult::Unknown,
    }
}

fn resolve(target: &ColorTarget, observation: &Observation) -> ColorTargetEvidence {
    let mut evidence = ColorTargetEvidence {
        selector: target.clone(),
        channel: None,
        raw: None,
        rgba: None,
        reason: None,
    };
    match owner(target, observation) {
        Ok(owner) => {
            evidence.channel = Some(match owner.channel {
                manuvra_chrome::ColorChannel::Accessible => ColorChannel::Accessible,
                manuvra_chrome::ColorChannel::PaintedAriaHidden => ColorChannel::PaintedAriaHidden,
            });
            evidence.raw = Some(owner.color.raw.clone());
            evidence.rgba = owner.color.rgba;
            evidence.reason = color_failure(owner);
        }
        Err(reason) => evidence.reason = Some(reason),
    }
    if !observation.colors_complete || !viewport_complete(observation) {
        evidence.reason = Some(ColorFailure::IncompleteCoverage);
    }
    evidence
}

fn color_failure(owner: &ColorObservation) -> Option<ColorFailure> {
    if !owner.paint_complete {
        return Some(ColorFailure::IncompleteCoverage);
    }
    match owner.color.rgba {
        None => Some(ColorFailure::UnsupportedColor),
        Some([_, _, _, 0]) => Some(ColorFailure::Transparent),
        Some(_) => None,
    }
}

fn owner<'a>(
    target: &ColorTarget,
    observation: &'a Observation,
) -> Result<&'a ColorObservation, ColorFailure> {
    let dialog = scope(observation, target.dialog(), ColorScopeKind::Dialog, None)?;
    let container = scope(
        observation,
        target.container(),
        ColorScopeKind::Container,
        dialog,
    )?;
    let matches = observation
        .colors
        .iter()
        .filter(|owner| {
            matches_target(target, owner)
                && in_scope(owner.dialog_node_id, dialog)
                && in_scope(owner.container_node_id, container)
        })
        .map(|owner| ((owner.context.as_str(), owner.node_id), owner))
        .collect::<BTreeMap<_, _>>();
    let eligible = matches
        .values()
        .copied()
        .filter(|owner| !matches!(owner.color.rgba, Some([_, _, _, 0])))
        .collect::<Vec<_>>();
    match eligible.as_slice() {
        [owner] => Ok(owner),
        [] if matches.len() == 1 => Ok(matches.values().next().expect("one owner")),
        [] if !matches.is_empty() => Err(ColorFailure::Transparent),
        [] => Err(ColorFailure::Missing),
        _ => Err(ColorFailure::AmbiguousOwner),
    }
}

fn matches_target(target: &ColorTarget, owner: &ColorObservation) -> bool {
    match target {
        ColorTarget::Text(target) => owner
            .text
            .as_deref()
            .is_some_and(|text| text == target.normalized_text()),
        ColorTarget::Name(target) => {
            owner
                .name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(&target.name))
                && target.role.as_ref().is_none_or(|role| {
                    owner
                        .role
                        .as_deref()
                        .is_some_and(|actual| actual.eq_ignore_ascii_case(role))
                })
        }
    }
}

fn in_scope(node_id: Option<u64>, scope: Option<&ColorScope>) -> bool {
    // The browser allocates node IDs across contexts; an ancestor can be outside a shadow root.
    scope.is_none_or(|scope| node_id == Some(scope.node_id))
}

fn scope<'a>(
    observation: &'a Observation,
    wanted: Option<&str>,
    kind: ColorScopeKind,
    dialog: Option<&ColorScope>,
) -> Result<Option<&'a ColorScope>, ColorFailure> {
    let Some(wanted) = wanted else {
        return Ok(None);
    };
    let matches = observation
        .color_scopes
        .iter()
        .filter(|scope| {
            scope.kind == kind
                && scope.name.eq_ignore_ascii_case(wanted)
                && in_scope(scope.dialog_node_id, dialog)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [scope] => Ok(Some(scope)),
        [] => Err(ColorFailure::Missing),
        _ => Err(ColorFailure::AmbiguousScope),
    }
}
