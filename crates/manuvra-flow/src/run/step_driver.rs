//! Autonomous step evaluation: observe, check the done condition, ask the provider, and act only
//! under a permit until the step completes, stops, or escalates.

use super::artifacts::{PendingEscalation, RunArtifacts, done_basis, record_step};
use super::capture::{Captured, DriveBrowser, capture_step, record_capture, redacted_value};
use super::escalation::{escalate, natural_noul};
use super::stops::{
    Stop, control_stop, policy_stop, provider_stop, step_detail, terminal_action_stop,
    value_not_provided,
};
use crate::evidence::Redactor;
use crate::verification::{DoneResult, check_done, check_natural_done};
use crate::{actions, judgment, policy, values::Values};
use manuvra_contract::{DoneCondition, Job, StepVerdict, VerdictResult};
use serde_json::json;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[allow(clippy::too_many_arguments)]
pub(super) fn evaluate_step(
    job: &Job,
    redactor: &Redactor,
    browser: &impl DriveBrowser,
    evaluator: &impl manuvra_jev::Evaluator,
    journal: &mut impl actions::ActionJournal,
    values: &Values<'_>,
    cancellation: &manuvra_chrome::InputCancellation,
    policy: &mut policy::Policy,
    index: usize,
    step: &manuvra_contract::Step,
    artifacts: &mut RunArtifacts,
    initial_mutations: u8,
    initial_observation_number: usize,
) -> Option<Stop> {
    StepDriver {
        job,
        redactor,
        browser,
        evaluator,
        journal,
        values,
        cancellation,
        policy,
        index,
        step,
        artifacts,
        observation_number: initial_observation_number,
        done_unknown_reobserved: false,
        operation_gate_reobserved: false,
        mutations: initial_mutations,
        awaiting_final_done_reobservation: false,
    }
    .drive()
}

struct StepDriver<'a> {
    job: &'a Job,
    redactor: &'a Redactor,
    browser: &'a dyn DriveBrowser,
    evaluator: &'a dyn manuvra_jev::Evaluator,
    journal: &'a mut dyn actions::ActionJournal,
    values: &'a Values<'a>,
    cancellation: &'a manuvra_chrome::InputCancellation,
    policy: &'a mut policy::Policy,
    index: usize,
    step: &'a manuvra_contract::Step,
    artifacts: &'a mut RunArtifacts,
    observation_number: usize,
    done_unknown_reobserved: bool,
    operation_gate_reobserved: bool,
    mutations: u8,
    awaiting_final_done_reobservation: bool,
}

impl StepDriver<'_> {
    fn drive(&mut self) -> Option<Stop> {
        loop {
            match self.drive_iteration() {
                StepProgress::Continue => {}
                StepProgress::Complete => return None,
                StepProgress::Stop(stop) => return Some(stop),
            }
        }
    }

    fn drive_iteration(&mut self) -> StepProgress {
        // Every earlier action has settled, so a cancelled run stops before observing again. The
        // hosted loop publishes the termination that cancelled it in place of this stop.
        if self.cancellation.is_cancelled() {
            return StepProgress::Stop(Stop::blocked(
                "run_cancelled",
                step_detail(self.redactor, self.step),
            ));
        }
        let captured = match self.capture_guarded() {
            Ok(value) => value,
            Err(stop) => return StepProgress::Stop(stop),
        };
        match &self.step.done_when {
            DoneCondition::Structured(assertions) => {
                let done = check_done(assertions, &captured.raw, &self.job.values);
                self.record(&captured, done);
                if let Err(stop) = self.policy.check_active() {
                    return StepProgress::Stop(policy_stop(stop, self.redactor, self.step));
                }
                self.done_progress(done, &captured)
            }
            DoneCondition::NaturalLanguage(condition) => {
                self.natural_done_progress(condition, &captured)
            }
        }
    }

    fn capture_guarded(&mut self) -> Result<Captured, Stop> {
        let captured = self.capture()?;
        self.policy
            .check_origin(&captured.raw)
            .map_err(|stop| policy_stop(stop, self.redactor, self.step))?;
        Ok(captured)
    }

    fn natural_done_progress(&mut self, condition: &str, captured: &Captured) -> StepProgress {
        let judgments = match self.natural_judgments(captured) {
            Ok(value) => value,
            Err(stop) => return StepProgress::Stop(stop),
        };
        let done = check_natural_done(condition, &captured.raw, judgments.step_done);
        self.record(captured, done);
        self.record_decision(&judgments);
        if done == DoneResult::NotSatisfied && self.mutations >= self.step.mutation_limit {
            return self.after_mutation_limit(done);
        }
        let next = self.policy.decide(
            self.step,
            &captured.raw,
            &judgments,
            done,
            self.done_unknown_reobserved,
            self.operation_gate_reobserved,
        );
        self.apply_next(done, captured, &judgments, next)
    }

    fn natural_judgments(&mut self, captured: &Captured) -> Result<judgment::Judgments, Stop> {
        if !captured.redaction_verified {
            self.record(captured, DoneResult::Unknown);
            return Err(Stop::blocked(
                "redaction_unverifiable",
                step_detail(self.redactor, self.step),
            ));
        }
        let deadline = self
            .policy
            .record_model_call()
            .map_err(|stop| policy_stop(stop, self.redactor, self.step))?;
        self.judge(captured, deadline)
    }

    fn after_mutation_limit(&mut self, done: DoneResult) -> StepProgress {
        if !self.awaiting_final_done_reobservation {
            self.awaiting_final_done_reobservation = true;
            self.pause_before_reobservation(Duration::from_millis(300));
            StepProgress::Continue
        } else {
            StepProgress::Stop(self.done_condition_failed(done))
        }
    }

    fn capture(&mut self) -> Result<Captured, Stop> {
        self.observation_number += 1;
        capture_step(
            self.browser,
            self.redactor,
            self.index + 1,
            self.observation_number,
        )
        .map_err(control_stop)
    }

    fn record(&mut self, captured: &Captured, done: DoneResult) {
        record_capture(
            self.artifacts,
            self.redactor,
            self.step,
            captured,
            if self.done_unknown_reobserved
                || self.operation_gate_reobserved
                || self.awaiting_final_done_reobservation
            {
                "reobservation"
            } else {
                "observation"
            },
            done,
        );
    }

    fn done_progress(&mut self, done: DoneResult, captured: &Captured) -> StepProgress {
        if !captured.redaction_verified {
            return StepProgress::Stop(Stop::blocked(
                "redaction_unverifiable",
                step_detail(self.redactor, self.step),
            ));
        }
        match done {
            DoneResult::Satisfied => {
                self.complete_step(done);
                StepProgress::Complete
            }
            DoneResult::Unknown => self.unknown_done(done, captured),
            DoneResult::NotSatisfied => self.not_done(done, captured),
        }
    }

    fn complete_step(&mut self, done: DoneResult) {
        self.artifacts.verdicts[self.index] = StepVerdict {
            id: self.redactor.redact_export_text(&self.step.id),
            result: VerdictResult::Satisfied,
            basis: Some(self.done_basis().into()),
        };
        self.record_step(done, true);
    }

    fn unknown_done(&mut self, done: DoneResult, captured: &Captured) -> StepProgress {
        if !self.done_unknown_reobserved {
            self.done_unknown_reobserved = true;
            self.pause_before_reobservation(Duration::from_millis(300));
            return StepProgress::Continue;
        }
        StepProgress::Stop(escalate(
            self.artifacts,
            self.redactor,
            self.index,
            self.step,
            done,
            None,
            PendingEscalation {
                done,
                noul: None,
                candidate: None,
                observation: captured.raw.clone(),
                ambiguous_mutation: false,
            },
            "done_unknown",
        ))
    }

    fn not_done(&mut self, done: DoneResult, captured: &Captured) -> StepProgress {
        if self.mutations >= self.step.mutation_limit {
            return self.after_mutation_limit(done);
        }
        let deadline = match self.policy.record_model_call() {
            Ok(deadline) => deadline,
            Err(stop) => return StepProgress::Stop(policy_stop(stop, self.redactor, self.step)),
        };
        let judgments = match self.judge(captured, deadline) {
            Ok(value) => value,
            Err(stop) => return StepProgress::Stop(stop),
        };
        self.record_decision(&judgments);
        self.apply_policy(done, captured, &judgments)
    }

    fn done_condition_failed(&mut self, done: DoneResult) -> Stop {
        self.artifacts.verdicts[self.index] = StepVerdict {
            id: self.redactor.redact_export_text(&self.step.id),
            result: VerdictResult::NotSatisfied,
            basis: Some(self.done_basis().into()),
        };
        self.record_step(done, true);
        Stop::failed(
            "done_condition_not_met",
            BTreeMap::from([
                (
                    "step_id".into(),
                    json!(self.redactor.redact_export_text(&self.step.id)),
                ),
                ("mutation_limit_consumed".into(), json!(true)),
            ]),
        )
    }

    fn judge(&self, captured: &Captured, deadline: Instant) -> Result<judgment::Judgments, Stop> {
        judgment::judge(
            self.evaluator,
            self.step,
            &captured.raw,
            &self.artifacts.trace,
            self.values,
            deadline,
        )
        .map_err(|error| provider_stop(&error, self.redactor, self.step))
    }

    fn record_decision(&mut self, judgments: &judgment::Judgments) {
        let name = format!("d_{:04}", self.artifacts.decisions.len() + 1);
        self.artifacts
            .decisions
            .push((name, redacted_value(judgments, self.redactor)));
    }

    fn apply_policy(
        &mut self,
        done: DoneResult,
        captured: &Captured,
        judgments: &judgment::Judgments,
    ) -> StepProgress {
        let next = self.policy.decide(
            self.step,
            &captured.raw,
            judgments,
            done,
            self.done_unknown_reobserved,
            self.operation_gate_reobserved,
        );
        self.apply_next(done, captured, judgments, next)
    }

    fn apply_next(
        &mut self,
        done: DoneResult,
        captured: &Captured,
        judgments: &judgment::Judgments,
        next: policy::Next,
    ) -> StepProgress {
        match next {
            policy::Next::Complete => {
                self.complete_step(done);
                StepProgress::Complete
            }
            policy::Next::ReobserveDone => {
                self.done_unknown_reobserved = true;
                self.pause_before_reobservation(Duration::from_millis(300));
                StepProgress::Continue
            }
            policy::Next::ReobserveOperation => {
                self.operation_gate_reobserved = true;
                self.pause_before_reobservation(Duration::from_millis(300));
                StepProgress::Continue
            }
            other => self.apply_after_reobserve(done, captured, judgments, other),
        }
    }

    fn apply_after_reobserve(
        &mut self,
        done: DoneResult,
        captured: &Captured,
        judgments: &judgment::Judgments,
        next: policy::Next,
    ) -> StepProgress {
        match next {
            policy::Next::Wait => {
                self.done_unknown_reobserved = false;
                self.operation_gate_reobserved = false;
                std::thread::sleep(Duration::from_millis(150));
                StepProgress::Continue
            }
            other => self.apply_terminal_next(done, captured, judgments, other),
        }
    }

    fn apply_terminal_next(
        &mut self,
        done: DoneResult,
        captured: &Captured,
        judgments: &judgment::Judgments,
        next: policy::Next,
    ) -> StepProgress {
        match next {
            policy::Next::Mutate(permit) if self.force_stop_before_first_mutation(&permit) => {
                let candidate = self.policy.release_unused(permit);
                StepProgress::Stop(escalate(
                    self.artifacts,
                    self.redactor,
                    self.index,
                    self.step,
                    done,
                    Some(judgments),
                    PendingEscalation {
                        done,
                        noul: natural_noul(self.step, judgments),
                        candidate: Some(candidate),
                        observation: captured.raw.clone(),
                        ambiguous_mutation: true,
                    },
                    "debug_forced_stop",
                ))
            }
            policy::Next::Mutate(permit) => self.mutate(done, captured, judgments, *permit),
            policy::Next::Stop(stop) => {
                StepProgress::Stop(self.policy_stop(done, captured, judgments, stop))
            }
            policy::Next::Complete
            | policy::Next::ReobserveDone
            | policy::Next::ReobserveOperation
            | policy::Next::Wait => {
                unreachable!("earlier next variants were handled")
            }
        }
    }

    /// The forced debug stop fires on the step's first mutating permit; scrolls and hovers before
    /// it are performed.
    fn force_stop_before_first_mutation(&self, permit: &policy::Permit) -> bool {
        permit.operation().mutates()
            && self.mutations == 0
            && self
                .job
                .options
                .debug
                .as_ref()
                .is_some_and(|debug| debug.force_stop_at_step == self.step.id)
    }

    fn mutate(
        &mut self,
        done: DoneResult,
        captured: &Captured,
        judgments: &judgment::Judgments,
        permit: policy::Permit,
    ) -> StepProgress {
        self.done_unknown_reobserved = false;
        self.operation_gate_reobserved = false;
        let journal_start = self.journal.entries().len();
        let result = actions::perform(
            permit,
            self.browser,
            &captured.raw,
            self.values,
            self.journal,
            self.cancellation,
        );
        self.artifacts
            .trace
            .extend(self.journal.entries()[journal_start..].iter().cloned());
        match result {
            Ok(fact) => {
                self.policy.record_observed(&fact.replay_key);
                if fact.operation.mutates() {
                    self.mutations += 1;
                }
                self.awaiting_final_done_reobservation = false;
                debug_assert_eq!(
                    self.artifacts
                        .trace
                        .last()
                        .and_then(|event| event.get("event")),
                    Some(&json!("action_fact"))
                );
                StepProgress::Continue
            }
            Err(actions::ActionStop::UncertainScroll(replay_key)) => {
                self.policy.record_uncertain_scroll(&replay_key);
                StepProgress::Continue
            }
            Err(actions::ActionStop::Reobserve(replay_key)) => {
                self.policy.release_not_performed(&replay_key);
                self.done_unknown_reobserved = false;
                self.operation_gate_reobserved = false;
                StepProgress::Continue
            }
            Err(stop) => StepProgress::Stop(self.action_stop(done, captured, judgments, stop)),
        }
    }

    fn action_stop(
        &mut self,
        done: DoneResult,
        captured: &Captured,
        judgments: &judgment::Judgments,
        stop: actions::ActionStop,
    ) -> Stop {
        match stop {
            actions::ActionStop::Uncertain => {
                self.escalate_action(done, captured, judgments, "action_outcome_uncertain", true)
            }
            actions::ActionStop::InvalidPermit => self.escalate_action(
                done,
                captured,
                judgments,
                "candidate_revalidation_failed",
                false,
            ),
            actions::ActionStop::Reobserve(_) | actions::ActionStop::UncertainScroll(_) => {
                unreachable!("not-performed actions reobserve before stop mapping")
            }
            other => terminal_action_stop(other, self.artifacts, self.redactor, self.step),
        }
    }

    /// An action whose outcome may have changed the page is escalated as an ambiguous mutation;
    /// a permit that could not be prepared was never journaled or dispatched.
    fn escalate_action(
        &mut self,
        done: DoneResult,
        captured: &Captured,
        judgments: &judgment::Judgments,
        reason: &'static str,
        ambiguous_mutation: bool,
    ) -> Stop {
        escalate(
            self.artifacts,
            self.redactor,
            self.index,
            self.step,
            done,
            Some(judgments),
            PendingEscalation {
                done,
                noul: natural_noul(self.step, judgments),
                candidate: None,
                observation: captured.raw.clone(),
                ambiguous_mutation,
            },
            reason,
        )
    }

    fn policy_stop(
        &mut self,
        done: DoneResult,
        captured: &Captured,
        judgments: &judgment::Judgments,
        stop: policy::PolicyStop,
    ) -> Stop {
        match stop {
            policy::PolicyStop::Uncertain(reason) => {
                let candidate = (reason == "operation_below_gate" || reason == "key_below_gate")
                    .then(|| self.policy.caller_candidate(&captured.raw, judgments).ok())
                    .flatten();
                escalate(
                    self.artifacts,
                    self.redactor,
                    self.index,
                    self.step,
                    done,
                    Some(judgments),
                    PendingEscalation {
                        done,
                        noul: natural_noul(self.step, judgments),
                        candidate,
                        observation: captured.raw.clone(),
                        ambiguous_mutation: false,
                    },
                    reason,
                )
            }
            policy::PolicyStop::Blocked("value_not_provided") => {
                value_not_provided(self.redactor, self.step, captured, judgments, self.values)
            }
            other => policy_stop(other, self.redactor, self.step),
        }
    }

    fn pause_before_reobservation(&mut self, delay: Duration) {
        std::thread::sleep(delay);
    }

    fn record_step(&mut self, done: DoneResult, redaction_verified: bool) {
        record_step(
            self.artifacts,
            self.redactor,
            self.index,
            self.step,
            done,
            self.mutations,
            redaction_verified,
            self.policy.active_ms(),
            self.done_basis(),
        );
    }

    fn done_basis(&self) -> &'static str {
        done_basis(self.step)
    }
}

enum StepProgress {
    Continue,
    Complete,
    Stop(Stop),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::machine::HostedMachine;
    use crate::run::stops::terminal_fields;
    use crate::run::tests::support::*;
    use manuvra_chrome::{BrowserError, Observation, PerformError, PerformFact, PreparedOperation};
    use manuvra_contract::RunState;
    use serde_json::Value;

    #[test]
    fn an_uncertain_scroll_followed_by_cancellation_ends_run_cancelled() {
        struct CancelAfterScroll(FakeBrowser);
        impl super::super::capture::BrowserPage for CancelAfterScroll {
            fn capture_redacted_page(
                &self,
                values: &[String],
            ) -> Result<manuvra_chrome::CapturedPage, manuvra_chrome::BrowserError> {
                self.0.capture_redacted_page(values)
            }
            fn observe_page(&self) -> Result<Observation, manuvra_chrome::BrowserError> {
                self.0.observe_page()
            }
        }
        impl actions::Performer for CancelAfterScroll {
            fn dispatch(
                &self,
                input: manuvra_chrome::PreparedInput,
                cancellation: &manuvra_chrome::InputCancellation,
            ) -> Result<manuvra_chrome::PerformFact, PerformError> {
                let result = actions::Performer::dispatch(&self.0, input, cancellation);
                cancellation.cancel();
                result
            }
        }
        let job = job("Finished");
        let mut page = observed("Ready");
        page.viewport.document_height = 2000.0;
        let browser = CancelAfterScroll(
            FakeBrowser::new([page])
                .always_dispatching(Err(PerformError::Uncertain("lost wheel".into()))),
        );
        let redactor = Redactor::for_job(&job).unwrap();
        let mut machine = crate::run::machine::HostedMachine::new(&job, &redactor);
        let cancellation = manuvra_chrome::InputCancellation::default();
        machine.drive(
            &browser,
            &ScriptedProvider::new([Turn::scroll_down()]),
            &mut MemoryJournal::default(),
            &cancellation,
            None,
            &ScriptedControl::default(),
        );
        assert_eq!(
            machine.artifacts.stop.as_ref().unwrap().code,
            "run_cancelled"
        );
        assert!(machine.artifacts.escalation.is_none());
        assert_eq!(browser.0.dispatched(), 1);
    }

    #[test]
    fn uncertain_scrolls_reobserve_with_new_keys_until_the_fallback_budget() {
        let mut job = mutation_job();
        job.steps[0].done_when =
            serde_json::from_value(json!([{ "text_visible":"Finished"}])).unwrap();
        let mut page = observed("Ready");
        page.viewport.document_height = 2000.0;
        let browser = FakeBrowser::new([page])
            .always_dispatching(Err(PerformError::Uncertain("lost wheel".into())));
        let provider = ScriptedProvider::new((0..9).map(|_| Turn::scroll_down()));
        let artifacts = driven(&job, &browser, &provider, &mut MemoryJournal::default());
        assert_eq!(artifacts.stop.as_ref().unwrap().code, "budget_exhausted");
        assert!(artifacts.escalation.is_none());
        assert_eq!(browser.dispatched(), 8);
        let keys: std::collections::BTreeSet<_> = artifacts
            .trace
            .iter()
            .filter(|e| e["event"] == "action_prepared")
            .map(|e| e["replay_key"].as_str().unwrap())
            .collect();
        assert_eq!(keys.len(), 8);
    }

    #[test]
    fn rejected_scrolls_reobserve_and_release_their_key_without_refunding_fallbacks() {
        let mut job = mutation_job();
        job.steps[0].done_when =
            serde_json::from_value(json!([{ "text_visible":"Finished"}])).unwrap();
        let mut page = observed("Ready");
        page.viewport.document_height = 2000.0;
        let browser = FakeBrowser::new([page])
            .always_dispatching(Err(PerformError::Rejected("stale region".into())));
        let provider = ScriptedProvider::new((0..9).map(|_| Turn::scroll_down()));
        let artifacts = driven(&job, &browser, &provider, &mut MemoryJournal::default());
        assert_eq!(artifacts.stop.as_ref().unwrap().code, "budget_exhausted");
        assert!(artifacts.escalation.is_none());
        assert_eq!(browser.dispatched(), 8);
    }

    #[test]
    fn classified_region_names_are_masked_in_escalation_recent_actions() {
        let mut job = mutation_job();
        job.steps[0].done_when =
            serde_json::from_value(json!([{ "text_visible":"Finished"}])).unwrap();
        job.values.get_mut("name").unwrap().secret = true;
        let mut page = observed("Ready");
        page.scroll_regions=serde_json::from_value(json!([{"node_id":9997,"name":"Wanted","overlay":null,"parent_node_id":null,"can_scroll_up":false,"can_scroll_down":true,"scroll_top":0,"scroll_height":1000,"client_height":300,"rect":{"x":0,"y":0,"width":200,"height":300}}])).unwrap();
        let browser =
            FakeBrowser::new([page]).always_dispatching(Ok(manuvra_chrome::PerformFact {
                scroll_readback: vec![manuvra_chrome::ScrollPosition {
                    name: Some("Wanted".into()),
                    overlay: None,
                    document: false,
                    before: 0.0,
                    after: 0.0,
                }],
                readback: None,
                readback_matches: None,
                suboperations: vec!["scroll_down".into()],
            }));
        let provider = ScriptedProvider::new([Turn::scroll_down(), Turn::scroll_down()]);
        let artifacts = driven(&job, &browser, &provider, &mut MemoryJournal::default());
        let escalation = &artifacts.escalations.last().unwrap().1;
        let recent = escalation["recent_actions"].to_string();
        assert!(recent.contains("scroll_target"));
        assert!(recent.contains("scroll_readback"));
        assert!(!recent.contains("Wanted"));
        assert!(!recent.contains("9997"));
    }

    #[test]
    fn autonomous_arrow_sequence_selects_observed_beta() {
        let job = key_job(
            "choose",
            "Press the arrow keys and Enter to choose Beta",
            json!([{"text_visible":"Selected: Beta"}]),
            3,
        );
        let mut before = key_observation("Choose item");
        before.focus_anchor.as_mut().unwrap().role = "combobox".into();
        let descendant = |id: &str, name: &str, selected: bool| {
            let mut page = before.clone();
            page.focus_anchor.as_mut().unwrap().active_descendant =
                Some(manuvra_chrome::ActiveDescendant {
                    id: id.into(),
                    role: "option".into(),
                    name: name.into(),
                    selected: Some(selected),
                    checked: None,
                });
            page
        };
        let alpha = descendant("alpha", "Alpha", false);
        let beta = descendant("beta", "Beta", false);
        let mut selected = descendant("beta", "Beta", true);
        selected.visible_text = "Selected: Beta".into();
        let browser = FakeBrowser::new([before.clone(), alpha, beta, selected])
            .always_dispatching(performed(&["key_down", "key_up"]));
        let provider = ScriptedProvider::new([
            Turn::key("ArrowDown"),
            Turn::key("ArrowDown"),
            Turn::key("Enter"),
        ]);

        let artifacts = driven(&job, &browser, &provider, &mut MemoryJournal::default());

        assert!(artifacts.stop.is_none());
        assert_eq!(artifacts.verdicts[0].result, VerdictResult::Satisfied);
        assert_eq!(provider.calls(), 3);
        let keys: Vec<_> = artifacts
            .trace
            .iter()
            .filter(|event| event["event"] == "action_prepared")
            .map(|event| event["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, ["ArrowDown", "ArrowDown", "Enter"]);
    }

    #[test]
    fn autonomous_escape_records_anchor_and_observes_restored_focus() {
        let job = key_job(
            "close",
            "Press Escape to close the breakdown popover",
            json!([{"dialog_closed":"Breakdown"}]),
            1,
        );
        let mut before = key_observation("First");
        before.dialogs.push("Breakdown".into());
        before.focus_anchor.as_mut().unwrap().in_dialog = Some("Breakdown".into());
        let browser = FakeBrowser::new([before, key_observation("Open breakdown")])
            .dispatching([performed(&["key_down", "key_up"])]);
        let provider = ScriptedProvider::new([Turn::key("Escape")]);

        let artifacts = driven(&job, &browser, &provider, &mut MemoryJournal::default());

        assert!(artifacts.stop.is_none());
        assert_eq!(artifacts.verdicts[0].result, VerdictResult::Satisfied);
        assert_eq!(provider.calls(), 1);
        let prepared = artifacts
            .trace
            .iter()
            .find(|value| value["event"] == "action_prepared")
            .unwrap();
        let fact = artifacts
            .trace
            .iter()
            .find(|value| value["event"] == "action_fact")
            .unwrap();
        assert_eq!(prepared["operation"], "PRESS_KEY");
        assert_eq!(prepared["key"], "Escape");
        assert_eq!(prepared["focus_anchor"]["name"], "First");
        assert_eq!(fact["fact"]["key"], "Escape");
        assert!(
            artifacts
                .observations
                .iter()
                .any(|(_, value, _)| value["focus_anchor"]["name"] == "Open breakdown")
        );
    }

    #[test]
    fn autonomous_tab_reaches_focus_condition_within_mutation_limit() {
        let job = key_job(
            "move",
            "Press Tab to focus Third",
            json!([{"focused":"Third"}]),
            2,
        );
        let browser = FakeBrowser::new([
            key_observation("First"),
            key_observation("Second"),
            key_observation("Third"),
        ])
        .always_dispatching(performed(&["key_down", "key_up"]));
        let provider = ScriptedProvider::new([Turn::key("Tab")]);

        let artifacts = driven(&job, &browser, &provider, &mut MemoryJournal::default());

        assert!(artifacts.stop.is_none());
        assert_eq!(artifacts.verdicts[0].result, VerdictResult::Satisfied);
        assert_eq!(provider.calls(), 2);
        assert_eq!(action_operations(&artifacts.trace, "action_fact").len(), 2);
    }

    /// One way a step reaches the forced debug stop: the pages and provider turns of the run, the
    /// dispatches that precede the stop, and whether the caller first executes an offered hover.
    struct ForcedStop {
        job: Job,
        pages: Vec<Observation>,
        turns: Vec<Turn>,
        dispatches: Vec<Result<PerformFact, PerformError>>,
        execute_first: bool,
        dispatched: Vec<PreparedOperation>,
        offered: (&'static str, &'static str),
    }

    #[test]
    fn debug_force_stop_fires_on_the_first_mutation_after_scrolls_and_hovers() {
        let mut scrollable = observed("Plan");
        scrollable
            .elements
            .push(button(1, 7, "Actions for Groceries"));
        scrollable.viewport.document_height = 2_000.0;
        let cases = [
            ForcedStop {
                job: force_stop(mutation_job()),
                pages: vec![text_field("")],
                turns: vec![Turn::type_text()],
                dispatches: vec![],
                execute_first: false,
                dispatched: vec![],
                offered: ("TYPE_TEXT", "Name"),
            },
            ForcedStop {
                job: force_stop(click_job()),
                pages: vec![scrollable],
                turns: vec![Turn::scroll_down(), Turn::click("1")],
                dispatches: vec![performed(&["scroll_down"])],
                execute_first: false,
                dispatched: vec![PreparedOperation::ScrollDown],
                offered: ("CLICK", "Actions for Groceries"),
            },
            ForcedStop {
                job: force_stop(click_job()),
                pages: vec![plan_before_hover(), plan_groceries_revealed()],
                turns: vec![Turn::hover("R1_1"), Turn::click("2")],
                dispatches: vec![performed(&["mouse_move"])],
                execute_first: false,
                dispatched: vec![PreparedOperation::Hover],
                offered: ("CLICK", "Actions for Groceries"),
            },
            ForcedStop {
                job: force_stop(click_job()),
                pages: vec![
                    plan_before_hover(),
                    plan_before_hover(),
                    plan_before_hover(),
                    plan_groceries_revealed(),
                ],
                turns: vec![low_hover_turn(), low_hover_turn(), Turn::click("2")],
                dispatches: vec![performed(&["mouse_move"])],
                execute_first: true,
                dispatched: vec![PreparedOperation::Hover],
                offered: ("CLICK", "Actions for Groceries"),
            },
        ];
        for case in cases {
            let redactor = Redactor::for_job(&case.job).unwrap();
            let browser = FakeBrowser::new(case.pages).dispatching(case.dispatches);
            let provider = ScriptedProvider::new(case.turns);
            let mut machine = HostedMachine::new(&case.job, &redactor);
            let mut journal = MemoryJournal::default();
            drive(&mut machine, &browser, &provider, &mut journal);
            if case.execute_first {
                let candidate = offered_hover(&machine);
                dispose(
                    &mut machine,
                    execute(&candidate.id),
                    &browser,
                    &provider,
                    &mut journal,
                );
                assert!(machine.artifacts.stop.is_none());
                drive(&mut machine, &browser, &provider, &mut journal);
            }

            let label = case.offered.0;
            assert_eq!(
                paused(&machine),
                ("debug_forced_stop", RunState::Uncertain, true),
                "{label}"
            );
            assert_eq!(browser.operations(), case.dispatched, "{label}");
            let escalation = &machine.artifacts.escalations.last().unwrap().1;
            assert_eq!(escalation["offered_candidate"]["operation"], case.offered.0);
            assert_eq!(
                escalation["offered_candidate"]["target_name"],
                case.offered.1
            );
            assert_eq!(
                escalation["permitted_mutations"],
                json!(["CLICK", "TYPE_TEXT", "PRESS_KEY"])
            );
            let exported = escalation.to_string();
            for identity in ["document_id", "node_id", "target_index"] {
                assert!(!exported.contains(identity), "{label} {identity}");
            }
        }
    }

    #[test]
    fn hover_then_click_satisfies_the_step_in_two_actions_with_one_mutation() {
        let browser = FakeBrowser::new([
            plan_before_hover(),
            plan_groceries_revealed(),
            plan_menu_open(),
        ])
        .dispatching([
            performed(&["mouse_move"]),
            performed(&["mouse_press", "mouse_release"]),
        ]);
        let provider = ScriptedProvider::new([Turn::hover("R1_1"), Turn::click("2")]);

        let artifacts = driven(
            &click_job(),
            &browser,
            &provider,
            &mut MemoryJournal::default(),
        );

        assert!(
            artifacts.stop.is_none(),
            "{:?}",
            artifacts.stop.map(|stop| stop.code)
        );
        assert_eq!(artifacts.verdicts[0].result, VerdictResult::Satisfied);
        assert_eq!(artifacts.steps[0].1["mutation_limit_consumed"], 1);
        assert_eq!(
            trace_events(&artifacts),
            [
                "observation",
                "action_prepared:HOVER",
                "action_fact:HOVER",
                "observation",
                "action_prepared:CLICK",
                "action_fact:CLICK",
                "observation",
                "final_verification_observation",
            ]
        );
        let groceries = json!({"index":1,"name":"Groceries","reveal":"Actions for Groceries","reveals_on_hover":["Actions for Groceries"]});
        assert_eq!(artifacts.trace[1]["hover_target"], groceries);
        assert_eq!(artifacts.trace[2]["fact"]["hover_target"], groceries);
        assert_eq!(artifacts.trace[2]["fact"]["outcome"], "observed");
        assert_eq!(
            artifacts.trace[2]["fact"]["suboperations"],
            json!(["mouse_move"])
        );
        assert_eq!(
            artifacts.observations[1].1["elements"][1]["name"],
            "Actions for Groceries"
        );
        let inputs = browser.inputs.lock().unwrap();
        assert_eq!(
            inputs
                .iter()
                .map(|input| (input.operation, input.node_id))
                .collect::<Vec<_>>(),
            [
                (PreparedOperation::Hover, 41),
                (PreparedOperation::Click, 41)
            ]
        );
        let requests = provider.requests.lock().unwrap();
        assert_eq!(
            requests[0]["questions"]["click_target"]["criteria"]["R1_1"]["revealed_by_hover"],
            true
        );
        assert_eq!(
            requests[1]["questions"]["click_target"]["criteria"]["2"]["name"],
            "Actions for Groceries"
        );
        assert!(
            requests[1]["state"]["recent_actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|action| action.as_str().unwrap().contains(
                    r#""hover_target":{"index":1,"name":"Groceries","reveal":"Actions for Groceries","reveals_on_hover":["Actions for Groceries"]}"#
                ))
        );
    }

    #[test]
    fn autonomous_dispatch_proven_not_performed_is_reobserved_and_retried_without_a_mutation() {
        for failure in [
            PerformError::Rejected("covered".into()),
            PerformError::NotPerformed("not queued".into()),
        ] {
            let browser = FakeBrowser::new([
                plan_before_hover(),
                plan_before_hover(),
                plan_groceries_revealed(),
                plan_menu_open(),
            ])
            .dispatching([
                Err(failure),
                performed(&["mouse_move"]),
                performed(&["mouse_release"]),
            ]);
            let provider =
                ScriptedProvider::new([Turn::hover("R1_1"), Turn::hover("R1_1"), Turn::click("2")]);

            let artifacts = driven(
                &click_job(),
                &browser,
                &provider,
                &mut MemoryJournal::default(),
            );

            assert!(
                artifacts.stop.is_none(),
                "{:?}",
                artifacts.stop.map(|stop| stop.code)
            );
            let outcomes: Vec<_> = artifacts
                .trace
                .iter()
                .filter(|entry| entry["event"] == "action_fact")
                .map(|entry| entry["fact"]["outcome"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(outcomes, ["not_performed", "observed", "observed"]);
            assert_eq!(artifacts.steps[0].1["mutation_limit_consumed"], 1);
        }
    }

    #[test]
    fn a_permit_that_cannot_be_prepared_escalates_without_journal_or_dispatch() {
        let job = click_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let browser = FakeBrowser::new([plan_before_hover()]);
        let mut journal = MemoryJournal::default();
        let values = Values::new(&job);
        let cancellation = manuvra_chrome::InputCancellation::default();
        let mut policy = policy::Policy::new(&job.options, "http://127.0.0.1:4351/");
        let mut artifacts = RunArtifacts::new(&job, &redactor);
        let mut judgments = mutation_judgments("CLICK", 1.0);
        judgments.click_target.choice = "R1_1".into();
        let policy::Next::Mutate(permit) = policy.decide(
            &job.steps[0],
            &plan_before_hover(),
            &judgments,
            DoneResult::NotSatisfied,
            false,
            false,
        ) else {
            panic!("hover permit");
        };
        let mut remounted = plan_before_hover();
        remounted.hover_regions[0].node_id = 99;
        let captured = Captured {
            raw: remounted,
            artifact: ("observation".into(), Value::Null, None),
            redaction_verified: true,
        };
        let mut driver = StepDriver {
            job: &job,
            redactor: &redactor,
            browser: &browser,
            evaluator: &NoProvider,
            journal: &mut journal,
            values: &values,
            cancellation: &cancellation,
            policy: &mut policy,
            index: 0,
            step: &job.steps[0],
            artifacts: &mut artifacts,
            observation_number: 0,
            done_unknown_reobserved: false,
            operation_gate_reobserved: false,
            mutations: 0,
            awaiting_final_done_reobservation: false,
        };

        let progress = driver.mutate(DoneResult::NotSatisfied, &captured, &judgments, *permit);

        let StepProgress::Stop(stop) = progress else {
            panic!("an unprepared permit stops the step");
        };
        assert_eq!(
            (stop.code, stop.state),
            ("candidate_revalidation_failed", RunState::Uncertain)
        );
        assert!(artifacts.escalation.is_some());
        assert!(!artifacts.pending.as_ref().unwrap().ambiguous_mutation);
        assert_eq!(browser.dispatched(), 0);
        assert!(journal.entries.is_empty());
    }

    #[test]
    fn natural_done_uncertainty_reobserves_once_and_never_dispatches() {
        let job = natural(
            mutation_job(),
            "El campo Name contiene el valor proporcionado",
        );
        let browser = FakeBrowser::new([text_field("Wanted")]);
        let provider = ScriptedProvider::new([Turn::type_text().noul(0.50)]);

        let artifacts = driven(&job, &browser, &provider, &mut MemoryJournal::default());

        assert_eq!(artifacts.stop.as_ref().unwrap().code, "done_uncertain");
        assert_eq!(artifacts.observations.len(), 2);
        assert_eq!(artifacts.decisions.len(), 2);
        assert_eq!(artifacts.verdicts[0].result, VerdictResult::Unresolved);
        assert_eq!(browser.dispatched(), 0);
        assert_eq!(provider.calls(), 2);
    }

    /// Runs `check` on a step driver over the empty Name field of the mutation job.
    fn with_step_driver(check: impl FnOnce(&mut StepDriver<'_>, &Captured)) -> RunArtifacts {
        let job = mutation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let captured = Captured {
            raw: text_field(""),
            artifact: ("observation".into(), Value::Null, None),
            redaction_verified: true,
        };
        let browser = FakeBrowser::new([text_field("")]);
        let mut journal = MemoryJournal::default();
        let values = Values::new(&job);
        let cancellation = manuvra_chrome::InputCancellation::default();
        let mut policy = policy::Policy::new(&job.options, "http://127.0.0.1:4351/");
        let mut artifacts = RunArtifacts::new(&job, &redactor);
        let mut driver = StepDriver {
            job: &job,
            redactor: &redactor,
            browser: &browser,
            evaluator: &NoProvider,
            journal: &mut journal,
            values: &values,
            cancellation: &cancellation,
            policy: &mut policy,
            index: 0,
            step: &job.steps[0],
            artifacts: &mut artifacts,
            observation_number: 0,
            done_unknown_reobserved: false,
            operation_gate_reobserved: false,
            mutations: 0,
            awaiting_final_done_reobservation: false,
        };
        check(&mut driver, &captured);
        artifacts
    }

    #[test]
    fn step_driver_escalates_uncertainty_and_ends_the_run_on_every_other_stop() {
        let done = DoneResult::NotSatisfied;
        let typed_answer = mutation_judgments("TYPE_TEXT", 1.0);
        with_step_driver(|driver, captured| {
            for next in [policy::Next::ReobserveOperation, policy::Next::Wait] {
                assert!(matches!(
                    driver.apply_next(done, captured, &typed_answer, next),
                    StepProgress::Continue
                ));
            }
            let blocked = driver.apply_after_reobserve(
                done,
                captured,
                &typed_answer,
                policy::Next::Stop(policy::PolicyStop::Blocked("operation_blocked")),
            );
            assert!(matches!(
                blocked,
                StepProgress::Stop(Stop {
                    code: "operation_blocked",
                    state: RunState::Blocked,
                    ..
                })
            ));
        });

        let artifacts = with_step_driver(|driver, captured| {
            let stop = driver.policy_stop(
                done,
                captured,
                &typed_answer,
                policy::PolicyStop::Uncertain("operation_below_gate"),
            );
            assert_eq!(
                (stop.code, stop.state),
                ("operation_below_gate", RunState::Uncertain)
            );
        });
        assert!(artifacts.escalation.is_some());
        assert!(artifacts.pending.as_ref().unwrap().candidate.is_some());

        with_step_driver(|driver, captured| {
            let stop = driver.policy_stop(
                done,
                captured,
                &typed_answer,
                policy::PolicyStop::Blocked("value_not_provided"),
            );
            assert_eq!(
                (stop.code, stop.state),
                ("value_not_provided", RunState::Blocked)
            );
            assert_eq!(stop.details["observed_field"], "Name");
            assert_eq!(stop.details["known_value_names"], json!(["name"]));
        });

        for (failure, code, state, incomplete) in [
            (
                actions::ActionStop::EvidenceUnavailable,
                "evidence_unavailable",
                RunState::Blocked,
                false,
            ),
            (
                actions::ActionStop::ReadbackMismatch,
                "write_readback_mismatch",
                RunState::Failed,
                false,
            ),
            (
                actions::ActionStop::IncompleteEvidence,
                "evidence_incomplete_after_dispatch",
                RunState::Blocked,
                true,
            ),
            (
                actions::ActionStop::InvalidPermit,
                "candidate_revalidation_failed",
                RunState::Uncertain,
                false,
            ),
            (
                actions::ActionStop::Uncertain,
                "action_outcome_uncertain",
                RunState::Uncertain,
                false,
            ),
        ] {
            let artifacts = with_step_driver(|driver, captured| {
                let stop = driver.action_stop(done, captured, &typed_answer, failure);
                assert_eq!((stop.code, stop.state), (code, state));
            });
            assert_eq!(
                artifacts.escalation.is_some(),
                state == RunState::Uncertain,
                "{code}"
            );
            assert_eq!(artifacts.evidence_incomplete, incomplete, "{code}");
        }
        let unescalated = super::policy_stop(
            policy::PolicyStop::Uncertain("operation_below_gate"),
            &Redactor::for_job(&mutation_job()).unwrap(),
            &mutation_job().steps[0],
        );
        assert_eq!(unescalated.state, RunState::Blocked);
    }

    #[test]
    fn structured_done_passes_or_blocks_when_the_provider_is_unavailable() {
        let passed = driven(
            &job("Ready"),
            &FakeBrowser::new([observed("Ready")]),
            &NoProvider,
            &mut MemoryJournal::default(),
        );
        assert!(passed.stop.is_none());
        assert_eq!(passed.verdicts[0].result, VerdictResult::Satisfied);

        let blocked = driven(
            &job("Ready"),
            &FakeBrowser::new([observed("Not yet")]),
            &NoProvider,
            &mut MemoryJournal::default(),
        );
        assert_eq!(blocked.observations.len(), 1);
        let (state, reason, exit_code, overall) = terminal_fields(blocked.stop);
        assert_eq!(state, RunState::Blocked);
        assert_eq!(reason.unwrap().code, "provider_unavailable");
        assert_eq!(exit_code, 3);
        assert_eq!(overall, VerdictResult::Unresolved);
    }

    #[test]
    fn mutation_limit_waits_for_one_final_reobservation_without_a_second_judgment() {
        let mut job = mutation_job();
        job.steps[0].mutation_limit = 1;
        let browser = FakeBrowser::new([text_field(""), text_field(""), text_field("Wanted")])
            .dispatching([typed("Wanted")]);
        let provider = ScriptedProvider::new([Turn::type_text()]);

        let artifacts = driven(&job, &browser, &provider, &mut MemoryJournal::default());

        assert!(artifacts.stop.is_none());
        assert_eq!(artifacts.observations.len(), 4);
        assert_eq!(artifacts.decisions.len(), 1);
        assert_eq!(provider.calls(), 1);
        assert_eq!(artifacts.verdicts[0].result, VerdictResult::Satisfied);
        assert_eq!(
            trace_events(&artifacts)
                .iter()
                .filter(|event| event.starts_with("action_"))
                .collect::<Vec<_>>(),
            ["action_prepared:TYPE_TEXT", "action_fact:TYPE_TEXT"]
        );
    }

    #[test]
    fn done_unknown_and_low_operation_gate_each_receive_their_own_reobservation() {
        let browser = FakeBrowser::new([
            observed(""),
            text_field(""),
            text_field(""),
            text_field("Wanted"),
        ])
        .dispatching([typed("Wanted")]);
        let provider = ScriptedProvider::new([
            Turn::type_text().confidence(0.69),
            Turn::type_text().confidence(0.95),
        ]);

        let artifacts = driven(
            &mutation_job(),
            &browser,
            &provider,
            &mut MemoryJournal::default(),
        );

        assert!(artifacts.stop.is_none());
        assert_eq!(artifacts.observations.len(), 5);
        assert_eq!(artifacts.decisions.len(), 2);
        assert_eq!(artifacts.verdicts[0].result, VerdictResult::Satisfied);
        assert_eq!(provider.calls(), 2);
        assert_eq!(browser.dispatched(), 1);
    }

    #[test]
    fn unverifiable_masking_and_control_failure_stop_the_step() {
        for (failure, code) in [
            ("redaction_unverifiable", "redaction_unverifiable"),
            ("marker", "browser_control_failed"),
        ] {
            let browser = FakeBrowser::capturing(
                [Err(BrowserError::Control(failure.into()))],
                observed("Ready"),
            );
            let artifacts = driven(
                &job("Ready"),
                &browser,
                &NoProvider,
                &mut MemoryJournal::default(),
            );
            let stop = artifacts.stop.unwrap();
            assert_eq!((stop.code, stop.state), (code, RunState::Blocked));
        }
    }
    #[test]
    fn a_reveal_that_exposes_nothing_stops_on_replay_without_clicking() {
        let browser =
            FakeBrowser::new([plan_before_hover()]).dispatching([performed(&["mouse_move"])]);
        let provider = ScriptedProvider::new([Turn::hover("R1_1")]);
        let artifacts = driven(
            &click_job(),
            &browser,
            &provider,
            &mut MemoryJournal::default(),
        );
        assert_eq!(artifacts.stop.unwrap().code, "replay_forbidden");
        assert_eq!(browser.dispatched(), 1);
        assert_eq!(provider.calls(), 2);
        let inputs = browser.inputs.lock().unwrap();
        assert_eq!(inputs[0].operation, PreparedOperation::Hover);
        assert!(artifacts.pending.unwrap().candidate.is_none());
    }
}
