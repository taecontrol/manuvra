//! The budget and replay ledger: actions, step mutations, fallbacks, and every minted replay key,
//! charged when a permit is minted and refunded only for an unused permit or a proven non-effect.

use super::{Candidate, Next, Permit, Policy, PolicyStop};
use crate::judgment::Operation;
use manuvra_chrome::Observation;
use manuvra_contract::Step;

impl Policy {
    pub(super) fn check_fallback_budget(&self, operation: Operation) -> Result<(), PolicyStop> {
        (operation.mutates() || self.fallbacks < 8)
            .then_some(())
            .ok_or(PolicyStop::Blocked("budget_exhausted"))
    }

    pub(super) fn fallback(&mut self, next: Next) -> Next {
        match self.check_fallback_budget(Operation::Wait) {
            Ok(()) => {
                self.charge(Operation::Wait);
                next
            }
            Err(stop) => Next::Stop(stop),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos", test))]
    pub(crate) fn release_unused(&mut self, permit: Box<Permit>) -> Candidate {
        let (candidate, _, replay_key, action_sequence) = permit.consume();
        debug_assert_eq!(action_sequence, u64::from(self.actions));
        let charged = self.replay.remove(&replay_key);
        debug_assert_eq!(charged, Some(candidate.operation));
        self.unsettled_keys.remove(&replay_key);
        self.actions = self.actions.saturating_sub(1);
        match charged.map(Operation::mutates) {
            Some(true) => self.step_mutations = self.step_mutations.saturating_sub(1),
            Some(false) => self.fallbacks = self.fallbacks.saturating_sub(1),
            None => {}
        }
        candidate
    }

    /// Releases the replay key of an operation proven not performed. Only a mutation is refunded;
    /// a fallback attempt stays counted, so rejected fallbacks remain bounded.
    #[cfg(any(target_os = "linux", target_os = "macos", test))]
    pub(crate) fn release_not_performed(&mut self, replay_key: &str) {
        self.unsettled_keys.remove(replay_key);
        if self
            .replay
            .remove(replay_key)
            .is_some_and(Operation::mutates)
        {
            self.step_mutations = self.step_mutations.saturating_sub(1);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos", test))]
    pub(crate) fn record_observed(&mut self, replay_key: &str) {
        self.unsettled_keys.remove(replay_key);
    }

    pub(super) fn authorize_context(
        &self,
        step: &Step,
        observation: &Observation,
    ) -> Result<(), PolicyStop> {
        if self.actions >= self.max_actions
            || self.step_mutations >= step.mutation_limit
            || self.active_elapsed() >= self.active_timeout
        {
            return Err(PolicyStop::Blocked("budget_exhausted"));
        }
        self.check_origin(observation)
    }

    pub(super) fn mint(&mut self, observation: &Observation, candidate: Candidate) -> Next {
        let Some(replay_key) = self.candidate_replay_key(observation, &candidate) else {
            return Next::Stop(PolicyStop::Uncertain("candidate_revalidation_failed"));
        };
        let key_identity = self.key_identity(observation, &candidate);
        if self.replay_forbidden(&replay_key, key_identity.as_deref()) {
            return Next::Stop(PolicyStop::Uncertain("replay_forbidden"));
        }
        self.replay.insert(replay_key.clone(), candidate.operation);
        if let Some(identity) = key_identity {
            self.unsettled_keys.insert(replay_key.clone(), identity);
        }
        self.actions += 1;
        self.charge(candidate.operation);
        Next::Mutate(Box::new(Permit {
            candidate,
            document_id: observation.document_id.clone(),
            replay_key,
            action_sequence: u64::from(self.actions),
        }))
    }

    fn replay_forbidden(&self, replay_key: &str, key_identity: Option<&str>) -> bool {
        self.replay.contains_key(replay_key)
            || key_identity.is_some_and(|identity| {
                self.unsettled_keys
                    .values()
                    .any(|pending| pending == identity)
            })
    }

    /// A mutation consumes the step limit and resets the fallback budget; a fallback consumes the
    /// fallback budget only.
    fn charge(&mut self, operation: Operation) {
        if operation.mutates() {
            self.step_mutations += 1;
            self.fallbacks = 0;
        } else {
            self.fallbacks += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::tests::support::*;
    use manuvra_contract::JobOptions;

    #[test]
    fn permit_is_single_owner_and_replay_survives_wait_remount_shape() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let first = decide_not_done(
            &mut policy,
            &step(),
            &observation("CLICK", "button"),
            &judgments("CLICK"),
            false,
        );
        assert!(matches!(first, Next::Mutate(_)));
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("CLICK", "button"),
                &judgments("WAIT"),
                false
            ),
            Next::Wait
        ));
        let mut remount = observation("CLICK", "button");
        remount.elements[0].node_id = 77;
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &remount, &judgments("CLICK"), false),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
    }

    #[test]
    fn proven_not_performed_remount_releases_replay_and_mutation_consumption() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let first = decide_not_done(
            &mut policy,
            &step(),
            &observation("CLICK", "button"),
            &judgments("CLICK"),
            false,
        );
        let Next::Mutate(permit) = first else {
            panic!("first permit");
        };
        let (_, _, replay_key, _) = permit.consume();
        policy.release_not_performed(&replay_key);
        assert_eq!(policy.step_mutations(), 0);
        let mut remounted = observation("CLICK", "button");
        remounted.elements[0].node_id = 77;
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &remounted, &judgments("CLICK"), false),
            Next::Mutate(_)
        ));
    }

    #[test]
    fn scroll_fallbacks_are_bounded_without_consuming_the_step_mutation_limit() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut page = observation("CLICK", "button");
        page.viewport.document_height = 2_000.0;
        for index in 0..8 {
            page.viewport.scroll_y = f64::from(index) * 100.0;
            assert!(matches!(
                decide_not_done(
                    &mut policy,
                    &step(),
                    &page,
                    &judgments("SCROLL_DOWN"),
                    false
                ),
                Next::Mutate(_)
            ));
            assert_eq!(policy.step_mutations(), 0);
        }
        page.viewport.scroll_y = 800.0;
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &page,
                &judgments("SCROLL_DOWN"),
                false
            ),
            Next::Stop(PolicyStop::Blocked("budget_exhausted"))
        ));
    }

    #[test]
    fn wait_fallbacks_are_bounded_and_never_mint_a_permit() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let page = observation("CLICK", "button");
        for _ in 0..8 {
            assert!(matches!(
                decide_not_done(&mut policy, &step(), &page, &judgments("WAIT"), false),
                Next::Wait
            ));
            assert_eq!(policy.step_mutations(), 0);
        }
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &page, &judgments("WAIT"), false),
            Next::Stop(PolicyStop::Blocked("budget_exhausted"))
        ));
    }

    #[test]
    fn hover_is_bounded_by_actions_time_step_mutations_and_fallbacks() {
        let no_actions = JobOptions {
            max_actions: Some(0),
            ..JobOptions::default()
        };
        let no_time = JobOptions {
            active_timeout_ms: Some(0),
            ..JobOptions::default()
        };
        for options in [no_actions, no_time] {
            let mut policy = Policy::new(&options, "http://example.test/");
            assert!(matches!(
                decide_not_done(
                    &mut policy,
                    &step(),
                    &hover_page(),
                    &hover(Some("R1_1")),
                    false
                ),
                Next::Stop(PolicyStop::Blocked("budget_exhausted"))
            ));
        }

        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        minted(decide_not_done(
            &mut policy,
            &one_mutation_step(),
            &hover_page(),
            &judgments("CLICK"),
            false,
        ));
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &one_mutation_step(),
                &hover_page(),
                &hover(Some("R1_1")),
                false
            ),
            Next::Stop(PolicyStop::Blocked("budget_exhausted"))
        ));

        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut page = hover_page();
        for index in 0..8 {
            page.hover_regions[1].name = format!("Rent {index}");
            minted(decide_not_done(
                &mut policy,
                &step(),
                &page,
                &hover(Some("R1_1")),
                false,
            ));
        }
        page.hover_regions[1].name = "Rent 8".into();
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &page, &hover(Some("R1_1")), false),
            Next::Stop(PolicyStop::Blocked("budget_exhausted"))
        ));
        assert_eq!(policy.step_mutations(), 0);
    }

    #[test]
    fn hover_then_click_fits_a_mutation_limit_of_one() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        minted(decide_not_done(
            &mut policy,
            &one_mutation_step(),
            &hover_page(),
            &hover(Some("R1_1")),
            false,
        ));
        assert_eq!(policy.step_mutations(), 0);
        minted(decide_not_done(
            &mut policy,
            &one_mutation_step(),
            &hover_page(),
            &judgments("CLICK"),
            false,
        ));
        assert_eq!(policy.step_mutations(), 1);
        assert_eq!(policy.fallbacks, 0);
        assert_eq!(policy.actions, 2);
    }

    #[test]
    fn not_performed_fallbacks_after_a_click_refund_no_mutation() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut page = hover_page();
        page.viewport.document_height = 2_000.0;
        minted(decide_not_done(
            &mut policy,
            &step(),
            &page,
            &judgments("CLICK"),
            false,
        ));
        assert_eq!(policy.step_mutations(), 1);
        for fallback in [hover(Some("R1_1")), judgments("SCROLL_DOWN")] {
            let (_, _, replay_key, _) = minted(decide_not_done(
                &mut policy,
                &step(),
                &page,
                &fallback,
                false,
            ))
            .consume();
            policy.release_not_performed(&replay_key);
            assert_eq!(policy.step_mutations(), 1);
            assert!(!policy.replay.contains_key(&replay_key));
        }
        assert_eq!(policy.fallbacks, 2);
        minted(decide_not_done(
            &mut policy,
            &step(),
            &page,
            &hover(Some("R1_1")),
            false,
        ));
    }

    #[test]
    fn key_presses_and_hovers_share_one_charge_ledger_but_keep_their_own_replay_scopes() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut page = hover_page();
        page.focus_anchor = focused("Save").focus_anchor;
        let escape = judgments("PRESS_KEY");
        minted(decide_not_done(
            &mut policy,
            &step(),
            &page,
            &hover(Some("R1_1")),
            false,
        ));
        assert_eq!((policy.step_mutations(), policy.fallbacks), (0, 1));
        let press = minted(decide_not_done(&mut policy, &step(), &page, &escape, false));
        assert_eq!((policy.step_mutations(), policy.fallbacks), (1, 0));
        let candidate = policy.release_unused(Box::new(press));
        assert_eq!(candidate.operation, Operation::PressKey);
        assert_eq!((policy.step_mutations(), policy.fallbacks), (0, 0));
        assert_eq!(policy.actions, 1);
        assert!(policy.unsettled_keys.is_empty());
        let press = minted(decide_not_done(&mut policy, &step(), &page, &escape, false));
        policy.release_not_performed(&press.replay_key);
        assert_eq!((policy.step_mutations(), policy.fallbacks), (0, 0));
        assert!(policy.unsettled_keys.is_empty());
        let press = minted(decide_not_done(&mut policy, &step(), &page, &escape, false));
        policy.record_observed(&press.replay_key);

        policy.begin_step();
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &page, &hover(Some("R1_1")), false),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
        minted(decide_not_done(&mut policy, &step(), &page, &escape, false));
        assert_eq!(policy.step_mutations(), 1);
    }

    #[test]
    fn unused_hover_permit_refunds_its_action_and_fallback_but_no_mutation() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        minted(decide_not_done(
            &mut policy,
            &step(),
            &hover_page(),
            &judgments("CLICK"),
            false,
        ));
        let permit = minted(decide_not_done(
            &mut policy,
            &step(),
            &hover_page(),
            &hover(Some("R2_1")),
            false,
        ));
        let candidate = policy.release_unused(Box::new(permit));
        assert_eq!(candidate.operation, Operation::Hover);
        assert_eq!(policy.step_mutations(), 1);
        assert_eq!(policy.actions, 1);
        assert_eq!(policy.fallbacks, 0);
        assert_eq!(policy.replay.len(), 1);
    }
}
