//! Caller revalidation: an offered candidate receives caller authority only while a fresh
//! observation still shows the same target, on a supported surface, within budget and origin.

use super::surface::key_surface;
use super::{Candidate, Next, Permit, Policy, PolicyStop};
use crate::judgment::Operation;
use manuvra_chrome::{Element, Observation};
use manuvra_contract::Step;

impl Policy {
    #[cfg(any(target_os = "linux", target_os = "macos", test))]
    pub(crate) fn authorize_caller(
        &mut self,
        step: &Step,
        observation: &Observation,
        candidate: &Candidate,
    ) -> Result<Permit, PolicyStop> {
        self.authorize_context(step, observation)?;
        self.check_fallback_budget(candidate.operation)?;
        let current = self.revalidated_caller_candidate(observation, candidate)?;
        match self.mint(observation, current) {
            Next::Mutate(permit) => Ok(*permit),
            Next::Stop(stop) => Err(stop),
            _ => unreachable!("caller authorization only mints or stops"),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos", test))]
    fn revalidated_caller_candidate(
        &self,
        observation: &Observation,
        candidate: &Candidate,
    ) -> Result<Candidate, PolicyStop> {
        let current = revalidate_caller_candidate(observation, candidate)?;
        self.check_select_value(observation, &current)
            .map(|()| current)
    }
}

/// The offered candidate, when the fresh observation still shows the same target: for a key
/// press the same document and focus anchor on a supported surface, for a `HOVER` the same region
/// (document, hidden control, name, and reveals), otherwise the same element.
fn revalidate_caller_candidate(
    observation: &Observation,
    candidate: &Candidate,
) -> Result<Candidate, PolicyStop> {
    let revalidated = match candidate.operation {
        Operation::PressKey => return revalidate_key_candidate(observation, candidate),
        Operation::Hover => candidate
            .hover_region(observation)
            .map(|_| candidate.clone()),
        _ => revalidated_element_candidate(observation, candidate),
    };
    revalidated.ok_or(PolicyStop::Uncertain("candidate_revalidation_failed"))
}

fn revalidate_key_candidate(
    observation: &Observation,
    candidate: &Candidate,
) -> Result<Candidate, PolicyStop> {
    if observation.document_id != candidate.target_identity.document_id
        || observation.focus_anchor != candidate.focus_anchor
    {
        return Err(PolicyStop::Uncertain("candidate_revalidation_failed"));
    }
    if let Some(surface) = key_surface(observation, candidate.key) {
        return Err(PolicyStop::UnsupportedSurface(surface));
    }
    Ok(candidate.clone())
}

fn revalidated_element_candidate(
    observation: &Observation,
    candidate: &Candidate,
) -> Option<Candidate> {
    let target = candidate
        .target_index
        .and_then(|index| observation.elements.iter().find(|item| item.index == index))
        .filter(|target| candidate_matches(observation, target, candidate))?;
    let mut current = candidate.clone();
    current.target_name = Some(target.name.clone());
    Some(current)
}

fn candidate_matches(observation: &Observation, target: &Element, candidate: &Candidate) -> bool {
    candidate_identity_matches(observation, target, candidate)
        && candidate_semantics_match(target, candidate)
        && candidate_operation_supported(target, candidate.operation)
}

fn candidate_identity_matches(
    observation: &Observation,
    target: &Element,
    candidate: &Candidate,
) -> bool {
    observation.document_id == candidate.target_identity.document_id
        && Some(target.node_id) == candidate.target_identity.node_id
}

fn candidate_semantics_match(target: &Element, candidate: &Candidate) -> bool {
    Some(target.name.as_str()) == candidate.target_name.as_deref()
        && Some(target.role.as_str()) == candidate.target_role.as_deref()
        && target.in_dialog == candidate.target_dialog
        && target.container == candidate.target_container
        && target.input_type == candidate.target_input_type
}

fn candidate_operation_supported(target: &Element, operation: Operation) -> bool {
    let expected = match operation {
        Operation::Click => "CLICK",
        Operation::TypeText => "TYPE_TEXT",
        Operation::Select => "SELECT",
        _ => return false,
    };
    target.operations.iter().any(|item| item == expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::tests::support::*;
    use manuvra_chrome::{FocusSurface, Key};
    use manuvra_contract::JobOptions;

    #[test]
    fn caller_key_candidate_revalidates_document_and_focus_anchor() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let candidate = policy
            .caller_candidate(&focused("A"), &judgments("PRESS_KEY"))
            .unwrap();
        assert_eq!(candidate.key, Some(Key::Escape));
        assert!(matches!(
            policy.authorize_caller(&step(), &focused("B"), &candidate),
            Err(PolicyStop::Uncertain("candidate_revalidation_failed"))
        ));
        let mut replaced = focused("A");
        replaced.document_id = "replacement".into();
        assert!(matches!(
            policy.authorize_caller(&step(), &replaced, &candidate),
            Err(PolicyStop::Uncertain("candidate_revalidation_failed"))
        ));
        let mut lost_focus = focused("A");
        lost_focus.focus_anchor = None;
        assert!(matches!(
            policy.authorize_caller(&step(), &lost_focus, &candidate),
            Err(PolicyStop::Uncertain("candidate_revalidation_failed"))
        ));
        assert_eq!(policy.actions, 0);
        assert!(
            policy
                .authorize_caller(&step(), &focused("A"), &candidate)
                .is_ok()
        );
    }

    #[test]
    fn caller_key_candidate_on_an_unchanged_canvas_anchor_is_an_unsupported_surface() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut canvas = focused("Drawing");
        canvas.focus_anchor.as_mut().unwrap().surface = Some(FocusSurface::Canvas);
        let candidate = policy
            .caller_candidate(&canvas, &judgments("PRESS_KEY"))
            .unwrap();
        assert_eq!(
            policy.authorize_caller(&step(), &canvas, &candidate).err(),
            Some(PolicyStop::UnsupportedSurface("canvas_control"))
        );
        assert_eq!(policy.actions, 0);
    }

    #[test]
    fn caller_authority_revalidates_identity_semantics_origin_budget_and_replay() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let original = observation("CLICK", "button");
        let candidate = policy
            .caller_candidate(&original, &judgments("CLICK"))
            .unwrap();

        let mut remounted = original.clone();
        remounted.elements[0].node_id = 10;
        assert!(matches!(
            policy.authorize_caller(&step(), &remounted, &candidate),
            Err(PolicyStop::Uncertain("candidate_revalidation_failed"))
        ));
        let mut renamed = original.clone();
        renamed.elements[0].name = "Delete".into();
        assert!(matches!(
            policy.authorize_caller(&step(), &renamed, &candidate),
            Err(PolicyStop::Uncertain("candidate_revalidation_failed"))
        ));
        let mut recycled = original.clone();
        recycled.elements[0].container = Some("Different row".into());
        assert_eq!(
            policy
                .authorize_caller(&step(), &recycled, &candidate)
                .err(),
            Some(PolicyStop::Uncertain("candidate_revalidation_failed"))
        );
        let permit = policy
            .authorize_caller(&step(), &original, &candidate)
            .expect("unchanged candidate receives caller authority");
        drop(permit);
        assert!(matches!(
            policy.authorize_caller(&step(), &original, &candidate),
            Err(PolicyStop::Uncertain("replay_forbidden"))
        ));
    }

    #[test]
    fn caller_select_authority_revalidates_the_offered_option() {
        let offered = native_select(&[("Savings", "savings-id", false)]);
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/")
            .with_provided_values(&provided("Savings"));
        let candidate = policy
            .caller_candidate(&offered, &judgments("SELECT"))
            .unwrap();
        let removed = native_select(&[("Checking", "checking-id", false)]);
        assert!(matches!(
            policy.authorize_caller(&step(), &removed, &candidate),
            Err(PolicyStop::Uncertain("select_option_unavailable"))
        ));
        assert_eq!(policy.actions, 0);
        assert!(
            policy
                .authorize_caller(&step(), &offered, &candidate)
                .is_ok()
        );
    }

    #[test]
    fn caller_hover_authority_revalidates_region_identity_budgets_origin_and_replay() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let candidate = policy
            .caller_candidate(&hover_page(), &hover(Some("R1")))
            .unwrap();
        let mut remounted = hover_page();
        remounted.hover_regions[0].node_id = 77;
        let mut renamed = hover_page();
        renamed.hover_regions[0].name = "Groceries and more".into();
        let mut revealing_other = hover_page();
        revealing_other.hover_regions[0].reveals_on_hover = vec!["Rename Groceries".into()];
        let mut navigated = hover_page();
        navigated.document_id = "other".into();
        let mut revealed = hover_page();
        revealed.hover_regions.remove(0);
        for changed in [remounted, renamed, revealing_other, navigated, revealed] {
            assert!(matches!(
                policy.authorize_caller(&one_mutation_step(), &changed, &candidate),
                Err(PolicyStop::Uncertain("candidate_revalidation_failed"))
            ));
        }
        assert_eq!(policy.actions, 0);

        let permit = policy
            .authorize_caller(&one_mutation_step(), &hover_page(), &candidate)
            .expect("unchanged region receives caller authority");
        assert_eq!(permit.operation(), Operation::Hover);
        assert_eq!(permit.consume().0.hover_target, candidate.hover_target);
        assert_eq!((policy.actions, policy.fallbacks), (1, 1));
        assert_eq!(policy.step_mutations(), 0);
        assert!(matches!(
            policy.authorize_caller(&one_mutation_step(), &hover_page(), &candidate),
            Err(PolicyStop::Uncertain("replay_forbidden"))
        ));
        minted(decide_not_done(
            &mut policy,
            &one_mutation_step(),
            &hover_page(),
            &judgments("CLICK"),
            false,
        ));

        let mut exhausted = Policy::new(&JobOptions::default(), "http://example.test/");
        exhausted.fallbacks = 8;
        assert!(matches!(
            exhausted.authorize_caller(&step(), &hover_page(), &candidate),
            Err(PolicyStop::Blocked("budget_exhausted"))
        ));
        let foreign = JobOptions {
            allowed_origins: Some(vec!["http://allowed.test".into()]),
            ..JobOptions::default()
        };
        let mut foreign = Policy::new(&foreign, "http://allowed.test/");
        assert!(matches!(
            foreign.authorize_caller(&step(), &hover_page(), &candidate),
            Err(PolicyStop::Blocked("origin_not_allowed"))
        ));
    }

    #[test]
    fn a_permit_for_a_changed_region_fails_revalidation() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let (candidate, ..) = minted(decide_not_done(
            &mut policy,
            &step(),
            &hover_page(),
            &hover(Some("R1")),
            false,
        ))
        .consume();
        let mut remounted = hover_page();
        remounted.hover_regions[0].node_id = 77;
        let mut renamed = hover_page();
        renamed.hover_regions[0].reveals_on_hover = vec!["Rename Groceries".into()];
        let mut navigated = hover_page();
        navigated.document_id = "other".into();
        for changed in [remounted, renamed, navigated] {
            assert_eq!(candidate.hover_region(&changed), None);
            assert!(matches!(
                policy.mint(&changed, candidate.clone()),
                Next::Stop(PolicyStop::Uncertain("candidate_revalidation_failed"))
            ));
        }
    }
}
