//! Caller `execute` dispositions: the step's done condition is reobserved first, then the
//! offered candidate is revalidated and dispatched once under caller authority.

use super::artifacts::{PendingEscalation, done_basis, record_step};
use super::capture::{Captured, DriveBrowser, record_capture, redacted_value};
use super::machine::HostedMachine;
use super::stops::{Stop, policy_stop, provider_stop, terminal_action_stop};
use crate::verification::{DoneResult, check_done, check_natural_done};
use crate::{actions, judgment, policy};
use manuvra_chrome::Observation;
use manuvra_contract::{DoneCondition, StepVerdict, VerdictResult};

impl HostedMachine<'_> {
    pub(super) fn apply_execute(
        &mut self,
        candidate_id: &str,
        browser: &impl DriveBrowser,
        evaluator: &impl manuvra_jev::Evaluator,
        journal: &mut impl actions::ActionJournal,
        cancellation: &manuvra_chrome::InputCancellation,
    ) {
        let (pending, candidate) = match self.offered_candidate(candidate_id) {
            Ok(value) => value,
            Err(reason) => {
                self.reissue(reason, self.artifacts.pending.clone());
                return;
            }
        };
        let ResumeObservation {
            captured,
            done,
            noul,
        } = match self.resume_observation(browser, evaluator) {
            Ok(value) => value,
            Err(stop) => return self.end_with(stop),
        };
        let step = &self.job.steps[self.index];
        record_capture(
            &mut self.artifacts,
            self.redactor,
            step,
            &captured,
            "resume_observation",
            done,
        );
        if done == DoneResult::Satisfied {
            self.complete_resumed_step();
            return;
        }
        if done == DoneResult::Unknown {
            self.reissue_unknown_done(pending, captured.raw, noul);
            return;
        }
        self.perform_caller_candidate(
            pending,
            candidate,
            captured,
            done,
            browser,
            journal,
            cancellation,
        );
    }

    fn reissue_unknown_done(
        &mut self,
        mut pending: PendingEscalation,
        observation: Observation,
        noul: Option<f64>,
    ) {
        pending.done = DoneResult::Unknown;
        pending.noul = noul;
        pending.candidate = None;
        pending.ambiguous_mutation = false;
        pending.observation = observation;
        let reason = match self.job.steps[self.index].done_when {
            DoneCondition::NaturalLanguage(_) => "done_uncertain",
            DoneCondition::Structured(_) => "done_unknown",
        };
        self.reissue(reason, Some(pending));
    }

    fn offered_candidate(
        &self,
        candidate_id: &str,
    ) -> Result<(PendingEscalation, policy::Candidate), &'static str> {
        let pending = self.artifacts.pending.clone().ok_or("stale_escalation")?;
        let candidate = pending
            .candidate
            .clone()
            .filter(|candidate| candidate.id == candidate_id)
            .ok_or("candidate_not_offered")?;
        Ok((pending, candidate))
    }

    fn resume_observation(
        &mut self,
        browser: &impl DriveBrowser,
        evaluator: &impl manuvra_jev::Evaluator,
    ) -> Result<ResumeObservation, Stop> {
        let captured = self.capture_for_disposition(browser, "resume_observation")?;
        let step = &self.job.steps[self.index];
        let (done, noul) = match &step.done_when {
            DoneCondition::Structured(assertions) => (
                check_done(assertions, &captured.raw, &self.job.values),
                None,
            ),
            DoneCondition::NaturalLanguage(condition) => {
                self.judge_resume_done(condition, &captured, evaluator)?
            }
        };
        Ok(ResumeObservation {
            captured,
            done,
            noul,
        })
    }

    fn judge_resume_done(
        &mut self,
        condition: &str,
        captured: &Captured,
        evaluator: &impl manuvra_jev::Evaluator,
    ) -> Result<(DoneResult, Option<f64>), Stop> {
        let step = &self.job.steps[self.index];
        let deadline = self
            .policy
            .record_model_call()
            .map_err(|stop| policy_stop(stop, self.redactor, step))?;
        let judgments = judgment::judge(
            evaluator,
            step,
            &captured.raw,
            &self.artifacts.trace,
            &self.values,
            deadline,
        )
        .map_err(|error| provider_stop(&error, self.redactor, step))?;
        self.artifacts.decisions.push((
            format!("d_{:04}", self.artifacts.decisions.len() + 1),
            redacted_value(&judgments, self.redactor),
        ));
        Ok((
            check_natural_done(condition, &captured.raw, judgments.step_done),
            Some(judgments.step_done),
        ))
    }

    fn complete_resumed_step(&mut self) {
        let step = &self.job.steps[self.index];
        self.artifacts.caller_assisted = true;
        let basis = done_basis(step);
        self.artifacts.verdicts[self.index] = StepVerdict {
            id: self.redactor.redact_export_text(&step.id),
            result: VerdictResult::Satisfied,
            basis: Some(basis.into()),
        };
        record_step(
            &mut self.artifacts,
            self.redactor,
            self.index,
            step,
            DoneResult::Satisfied,
            self.policy.step_mutations(),
            true,
            self.policy.active_ms(),
            basis,
        );
        self.advance_step();
        self.clear_pause();
    }

    #[allow(clippy::too_many_arguments)]
    fn perform_caller_candidate(
        &mut self,
        pending: PendingEscalation,
        candidate: policy::Candidate,
        captured: Captured,
        done: DoneResult,
        browser: &impl DriveBrowser,
        journal: &mut impl actions::ActionJournal,
        cancellation: &manuvra_chrome::InputCancellation,
    ) {
        let step = &self.job.steps[self.index];
        let permit = match self
            .policy
            .authorize_caller(step, &captured.raw, &candidate)
        {
            Ok(permit) => permit,
            Err(policy::PolicyStop::Uncertain(reason)) => {
                self.reissue_without_candidate(reason, pending, captured.raw);
                return;
            }
            Err(stop) => return self.end_with(policy_stop(stop, self.redactor, step)),
        };
        let journal_start = journal.entries().len();
        let performed = actions::perform_with_basis(
            permit,
            browser,
            &captured.raw,
            &self.values,
            journal,
            cancellation,
            "caller_authority",
        );
        self.artifacts
            .trace
            .extend(journal.entries()[journal_start..].iter().cloned());
        self.artifacts.caller_assisted = true;
        self.clear_pause();
        self.settle_caller_action(performed, pending, captured.raw, done);
    }

    /// A proven non-effect releases the replay key and reissues without a candidate, a possibly
    /// performed action reissues as an ambiguous mutation, and an evidence or readback failure
    /// ends the run.
    fn settle_caller_action(
        &mut self,
        performed: Result<actions::ActionFact, actions::ActionStop>,
        pending: PendingEscalation,
        observation: Observation,
        done: DoneResult,
    ) {
        match performed {
            Ok(fact) => self.policy.record_observed(&fact.replay_key),
            Err(actions::ActionStop::Reobserve(replay_key)) => {
                self.policy.release_not_performed(&replay_key);
                self.reissue_without_candidate(
                    "candidate_revalidation_failed",
                    pending,
                    observation,
                );
            }
            Err(actions::ActionStop::InvalidPermit) => {
                self.reissue_without_candidate(
                    "candidate_revalidation_failed",
                    pending,
                    observation,
                );
            }
            Err(actions::ActionStop::Uncertain) => self.reissue_ambiguous(done, observation),
            Err(stop) => {
                let step = &self.job.steps[self.index];
                let stop = terminal_action_stop(stop, &mut self.artifacts, self.redactor, step);
                self.end_with(stop);
            }
        }
    }

    fn reissue_without_candidate(
        &mut self,
        reason: &'static str,
        mut pending: PendingEscalation,
        observation: Observation,
    ) {
        pending.candidate = None;
        pending.observation = observation;
        self.reissue(reason, Some(pending));
    }

    fn reissue_ambiguous(&mut self, done: DoneResult, observation: Observation) {
        let pending = PendingEscalation {
            done,
            noul: None,
            candidate: None,
            observation,
            ambiguous_mutation: true,
        };
        self.reissue("action_outcome_uncertain", Some(pending));
    }
}

struct ResumeObservation {
    captured: Captured,
    done: DoneResult,
    noul: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::Redactor;
    use crate::run::tests::support::*;
    use manuvra_chrome::{BrowserError, PerformError, PreparedOperation};
    use manuvra_contract::{Job, RunState};
    use serde_json::{Value, json};

    #[test]
    fn unverifiable_resume_redaction_blocks_before_the_provider() {
        let job = natural(click_job(), "The actions menu for Groceries is open");
        let redactor = Redactor::for_job(&job).unwrap();
        let provider = ScriptedProvider::new([low_hover_turn()]);
        let mut machine = HostedMachine::new(&job, &redactor);
        let before = FakeBrowser::new([plan_before_hover()]);
        drive(
            &mut machine,
            &before,
            &provider,
            &mut MemoryJournal::default(),
        );
        let candidate = offered_hover(&machine);
        let unverifiable = FakeBrowser::capturing(
            [Err(BrowserError::Control("redaction_unverifiable".into()))],
            plan_menu_open(),
        );

        dispose(
            &mut machine,
            execute(&candidate.id),
            &unverifiable,
            &provider,
            &mut MemoryJournal::default(),
        );

        let stop = machine.artifacts.stop.as_ref().unwrap();
        assert_eq!(
            (stop.code, stop.state),
            ("redaction_unverifiable", RunState::Blocked)
        );
        assert!(machine.artifacts.escalation.is_none());
        assert_eq!(provider.calls(), 2);
        assert_eq!(unverifiable.dispatched(), 0);
        let (_, withheld, screenshot) = machine.artifacts.observations.last().unwrap();
        assert_eq!(withheld["screenshot"]["withheld"], "redaction_unverifiable");
        assert!(screenshot.is_none());
    }

    #[test]
    fn caller_execute_press_key_offers_anchor_and_dispatches_once() {
        let job = force_stop(key_activation_job());
        let redactor = Redactor::for_job(&job).unwrap();
        let focused = key_observation("Save");
        let mut activated = focused.clone();
        activated.visible_text = "Activations: 1".into();
        let browser = FakeBrowser::new([focused.clone(), focused, activated])
            .dispatching([performed(&["key_down", "key_up"])]);
        let provider = ScriptedProvider::new([Turn::key("Enter")]);
        let mut machine = HostedMachine::new(&job, &redactor);
        let mut journal = MemoryJournal::default();
        drive(&mut machine, &browser, &provider, &mut journal);
        let offered = &machine.artifacts.escalations[0].1["offered_candidate"];
        assert_eq!(offered["operation"], "PRESS_KEY");
        assert_eq!(offered["key"], "Enter");
        assert_eq!(offered["focus_anchor"]["name"], "Save");
        assert!(
            machine.artifacts.escalations[0].1["permitted_mutations"]
                .as_array()
                .unwrap()
                .contains(&json!("PRESS_KEY"))
        );
        let candidate_id = offered["id"].as_str().unwrap().to_owned();

        dispose(
            &mut machine,
            execute(&candidate_id),
            &browser,
            &provider,
            &mut journal,
        );
        drive(&mut machine, &browser, &provider, &mut journal);

        assert_eq!(machine.index, 1);
        assert_eq!(
            machine.artifacts.verdicts[0].result,
            VerdictResult::Satisfied
        );
        let prepared = journal.prepared();
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0]["key"], "Enter");
        assert_eq!(provider.calls(), 1);
    }

    #[test]
    fn caller_execute_revalidates_the_offered_candidate_without_dispatch() {
        let key_job = force_stop(key_activation_job());
        let element_job = force_stop(mutation_job());
        let mut remounted_field = text_field("");
        remounted_field.elements[0].node_id = 99;
        for (job, pages, turn) in [
            (
                key_job,
                [key_observation("Save"), key_observation("Other")],
                Turn::key("Enter"),
            ),
            (
                element_job,
                [text_field(""), remounted_field],
                Turn::type_text(),
            ),
        ] {
            let redactor = Redactor::for_job(&job).unwrap();
            let browser = FakeBrowser::new(pages);
            let provider = ScriptedProvider::new([turn]);
            let mut machine = HostedMachine::new(&job, &redactor);
            let mut journal = MemoryJournal::default();
            drive(&mut machine, &browser, &provider, &mut journal);
            let candidate = offered(&machine);

            dispose(
                &mut machine,
                execute(&candidate.id),
                &browser,
                &provider,
                &mut journal,
            );

            assert_eq!(
                paused(&machine),
                ("candidate_revalidation_failed", RunState::Uncertain, false)
            );
            assert!(!offers_execute(&machine));
            assert!(!machine.artifacts.caller_assisted);
            assert!(journal.entries.is_empty());
            assert_eq!(browser.dispatched(), 0);
        }

        let job = click_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let mut remounted = plan_before_hover();
        remounted.hover_regions[0].node_id = 99;
        let mut renamed = plan_before_hover();
        renamed.hover_regions[0].name = "Groceries and dining".into();
        let mut revealing_other = plan_before_hover();
        revealing_other.hover_regions[0].reveals_on_hover = vec!["Rename Groceries".into()];
        let mut navigated = plan_before_hover();
        navigated.document_id = "another-document".into();
        for changed in [
            plan_groceries_revealed(),
            remounted,
            renamed,
            revealing_other,
            navigated,
        ] {
            let mut machine = hover_escalation(&job, &redactor);
            let candidate = offered_hover(&machine);
            let browser = FakeBrowser::new([changed]);
            dispose(
                &mut machine,
                execute(&candidate.id),
                &browser,
                &NoProvider,
                &mut MemoryJournal::default(),
            );
            assert_eq!(
                paused(&machine),
                ("candidate_revalidation_failed", RunState::Uncertain, false)
            );
            assert!(!offers_execute(&machine));
            assert!(!machine.artifacts.caller_assisted);
            assert_eq!(browser.dispatched(), 0);
        }

        let mut machine = hover_escalation(&job, &redactor);
        let candidate = offered_hover(&machine);
        let _reserved = machine
            .policy
            .authorize_caller(&job.steps[0], &plan_before_hover(), &candidate)
            .unwrap();
        let browser = FakeBrowser::new([plan_before_hover()]);
        dispose(
            &mut machine,
            execute(&candidate.id),
            &browser,
            &NoProvider,
            &mut MemoryJournal::default(),
        );
        assert_eq!(
            paused(&machine),
            ("replay_forbidden", RunState::Uncertain, false)
        );
        assert_eq!(browser.dispatched(), 0);
    }

    /// A run paused on a candidate for `operation`, with the pages its drive and execute see.
    fn caller_candidate_run(operation: &str) -> (Job, Vec<Observation>, Turn) {
        match operation {
            "CLICK" => (
                force_stop(key_activation_job()),
                vec![save_button_focus(); 2],
                Turn::click("1"),
            ),
            "PRESS_KEY" => (
                force_stop(key_activation_job()),
                vec![save_button_focus(); 2],
                Turn::key("Enter"),
            ),
            _ => (click_job(), vec![plan_before_hover(); 3], low_hover_turn()),
        }
    }

    #[test]
    fn caller_dispatch_proven_not_performed_releases_replay_and_reissues_without_a_candidate() {
        for operation in ["CLICK", "PRESS_KEY", "HOVER"] {
            for failure in [
                PerformError::Rejected("focus_changed".into()),
                PerformError::NotPerformed("cancelled before transport send".into()),
            ] {
                let (job, pages, turn) = caller_candidate_run(operation);
                let redactor = Redactor::for_job(&job).unwrap();
                let page = pages[0].clone();
                let browser = FakeBrowser::new(pages).dispatching([Err(failure)]);
                let provider = ScriptedProvider::new([turn]);
                let mut machine = HostedMachine::new(&job, &redactor);
                let mut journal = MemoryJournal::default();
                drive(&mut machine, &browser, &provider, &mut journal);
                let candidate = offered(&machine);
                let calls = provider.calls();
                assert_eq!(
                    machine.artifacts.escalations[0].1["offered_candidate"]["operation"],
                    operation
                );

                dispose(
                    &mut machine,
                    execute(&candidate.id),
                    &browser,
                    &provider,
                    &mut journal,
                );

                assert_eq!(
                    paused(&machine),
                    ("candidate_revalidation_failed", RunState::Uncertain, false),
                    "{operation}"
                );
                let reissued = &machine.artifacts.escalations.last().unwrap().1;
                assert_eq!(reissued["gate_reason"], "candidate_revalidation_failed");
                assert!(reissued["offered_candidate"].is_null());
                assert!(!offers_execute(&machine));
                assert!(machine.artifacts.caller_assisted);
                let prepared = journal.prepared();
                assert_eq!(prepared.len(), 1, "{operation}");
                assert_eq!(prepared[0]["operation"], operation);
                assert_eq!(prepared[0]["basis"], "caller_authority");
                let fact = journal
                    .entries
                    .iter()
                    .find(|event| event["event"] == "action_fact")
                    .unwrap();
                assert_eq!(fact["fact"]["outcome"], "not_performed");
                assert_eq!(machine.index, 0);
                assert_eq!(
                    machine.artifacts.verdicts[0].result,
                    VerdictResult::Unresolved
                );
                assert_eq!(provider.calls(), calls, "{operation}");
                assert_eq!(machine.policy.step_mutations(), 0);
                let permit = machine
                    .policy
                    .authorize_caller(&job.steps[0], &page, &candidate)
                    .expect("a proven non-effect releases its replay entry");
                machine.policy.release_unused(Box::new(permit));
            }
        }
    }

    #[test]
    fn possibly_performed_dispatch_escalates_as_an_ambiguous_action_and_keeps_the_replay_key() {
        let job = click_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let mut machine = hover_escalation(&job, &redactor);
        let candidate = offered_hover(&machine);
        let browser = FakeBrowser::new([plan_before_hover()])
            .dispatching([Err(PerformError::Uncertain("sent without answer".into()))]);
        dispose(
            &mut machine,
            execute(&candidate.id),
            &browser,
            &NoProvider,
            &mut MemoryJournal::default(),
        );
        assert_eq!(
            paused(&machine),
            ("action_outcome_uncertain", RunState::Uncertain, false)
        );
        assert!(
            machine
                .artifacts
                .pending
                .as_ref()
                .unwrap()
                .ambiguous_mutation
        );
        assert!(machine.artifacts.caller_assisted);
        assert_eq!(browser.dispatched(), 1);
        assert!(matches!(
            machine
                .policy
                .authorize_caller(&job.steps[0], &plan_before_hover(), &candidate),
            Err(policy::PolicyStop::Uncertain("replay_forbidden"))
        ));

        let browser = FakeBrowser::new([plan_before_hover()])
            .dispatching([Err(PerformError::Uncertain("sent without answer".into()))]);
        let artifacts = driven(
            &job,
            &browser,
            &ScriptedProvider::new([Turn::hover("R1_1")]),
            &mut MemoryJournal::default(),
        );
        assert_eq!(
            artifacts.stop.as_ref().unwrap().code,
            "action_outcome_uncertain"
        );
        assert_eq!(artifacts.escalations[0].1["offered_candidate"], Value::Null);
        assert!(artifacts.pending.as_ref().unwrap().ambiguous_mutation);
        assert_eq!(browser.dispatched(), 1);
    }

    #[test]
    fn uncertain_key_press_cannot_be_executed_or_replayed_after_retry_observation() {
        let lost_key_up = || PerformError::Uncertain("lost key response".into());
        for (key, before, after) in [
            ("Enter", key_observation("Save"), key_observation("Save")),
            (
                "Enter",
                anchor_state("button", Some(false), None),
                anchor_state("button", Some(true), None),
            ),
            (
                "Space",
                anchor_state("checkbox", None, Some(false)),
                anchor_state("checkbox", None, Some(true)),
            ),
        ] {
            let job = key_activation_job();
            let redactor = Redactor::for_job(&job).unwrap();
            let browser = FakeBrowser::new([before, after]).dispatching([Err(lost_key_up())]);
            let provider = ScriptedProvider::new([Turn::key(key)]);
            let mut machine = HostedMachine::new(&job, &redactor);
            let mut journal = MemoryJournal::default();
            drive(&mut machine, &browser, &provider, &mut journal);
            assert_eq!(
                paused(&machine),
                ("action_outcome_uncertain", RunState::Uncertain, false)
            );
            assert!(!offers_execute(&machine));

            dispose(&mut machine, retry(), &browser, &provider, &mut journal);
            drive(&mut machine, &browser, &provider, &mut journal);

            assert_eq!(
                machine.artifacts.stop.as_ref().unwrap().code,
                "replay_forbidden",
                "{key}"
            );
            assert_eq!(provider.calls(), 2);
            assert_eq!(journal.prepared().len(), 1, "{key}");
            let refused = &machine.artifacts.escalations[1].1;
            assert_eq!(refused["gate_reason"], "replay_forbidden");
            assert_eq!(refused["candidates"]["operation"]["choice"], "PRESS_KEY");
            assert_eq!(refused["candidates"]["key"]["choice"], key);
            let prepared = refused["recent_actions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|event| event["event"] == "action_prepared")
                .unwrap();
            assert_eq!(prepared["operation"], "PRESS_KEY");
            assert_eq!(prepared["key"], key);
            assert_eq!(prepared["focus_anchor"]["name"], "Save");
        }
    }

    #[test]
    fn executed_hover_runs_under_caller_authority_and_the_step_continues_autonomously() {
        let job = click_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let mut machine = hover_escalation(&job, &redactor);
        let candidate = offered_hover(&machine);
        let browser = FakeBrowser::new([
            plan_before_hover(),
            plan_groceries_revealed(),
            plan_menu_open(),
        ])
        .dispatching([performed(&["mouse_move"]), performed(&["mouse_release"])]);
        let provider = ScriptedProvider::new([Turn::click("2")]);
        let mut journal = MemoryJournal::default();

        dispose(
            &mut machine,
            execute(&candidate.id),
            &browser,
            &provider,
            &mut journal,
        );

        assert!(machine.artifacts.stop.is_none());
        assert!(machine.artifacts.caller_assisted);
        assert_eq!(machine.policy.step_mutations(), 0);
        drive(&mut machine, &browser, &provider, &mut journal);

        assert!(
            machine.artifacts.stop.is_none(),
            "{:?}",
            machine.artifacts.stop.as_ref().map(|stop| stop.code)
        );
        assert_eq!(machine.index, 1);
        assert_eq!(
            machine.artifacts.verdicts[0].result,
            VerdictResult::Satisfied
        );
        assert_eq!(machine.artifacts.steps[0].1["mutation_limit_consumed"], 1);
        let prepared: Vec<_> = journal
            .prepared()
            .into_iter()
            .map(|event| (event["operation"].clone(), event["basis"].clone()))
            .collect();
        assert_eq!(
            prepared,
            [
                (json!("HOVER"), json!("caller_authority")),
                (json!("CLICK"), json!("autonomous")),
            ]
        );
        assert_eq!(
            journal.entries[0]["hover_target"],
            json!({"index":1,"name":"Groceries","reveals_on_hover":["Actions for Groceries"],"reveal":"Actions for Groceries"})
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
        assert_eq!(
            provider.calls(),
            1,
            "an executed hover is not re-asked through the operation gate"
        );
        assert!(
            machine
                .artifacts
                .trace
                .iter()
                .any(|event| event["event"] == "action_fact"
                    && event["fact"]["operation"] == "HOVER"
                    && event["fact"]["outcome"] == "observed")
        );
    }

    #[test]
    fn executing_a_hover_requires_the_pending_escalation_and_its_offered_candidate() {
        let job = click_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let mut machine = hover_escalation(&job, &redactor);
        let candidate = offered_hover(&machine);
        let browser =
            FakeBrowser::new([plan_before_hover()]).dispatching([performed(&["mouse_move"])]);
        let mut journal = MemoryJournal::default();

        dispose(
            &mut machine,
            execute("c_9"),
            &browser,
            &NoProvider,
            &mut journal,
        );
        assert_eq!(
            paused(&machine),
            ("candidate_not_offered", RunState::Uncertain, true)
        );
        assert!(offers_execute(&machine));

        dispose(
            &mut machine,
            execute(&candidate.id),
            &browser,
            &NoProvider,
            &mut journal,
        );
        assert!(machine.artifacts.stop.is_none());
        dispose(
            &mut machine,
            execute(&candidate.id),
            &browser,
            &NoProvider,
            &mut journal,
        );
        assert_eq!(
            paused(&machine),
            ("stale_escalation", RunState::Uncertain, false)
        );
        assert_eq!(browser.operations(), [PreparedOperation::Hover]);
    }

    #[test]
    fn executing_a_hover_completes_or_reissues_on_the_fresh_done_result() {
        let job = click_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let mut machine = hover_escalation(&job, &redactor);
        let candidate = offered_hover(&machine);
        let done = FakeBrowser::new([plan_menu_open()]);
        dispose(
            &mut machine,
            execute(&candidate.id),
            &done,
            &NoProvider,
            &mut MemoryJournal::default(),
        );
        assert_eq!(machine.index, 1);
        assert!(machine.artifacts.stop.is_none());
        assert!(machine.artifacts.caller_assisted);
        assert_eq!(machine.artifacts.steps[0].1["done"], "satisfied");
        assert_eq!(done.dispatched(), 0);

        let mut machine = hover_escalation(&job, &redactor);
        let mut partial = plan_before_hover();
        partial.coverage.viewport_complete = false;
        let unknown = FakeBrowser::new([partial]);
        dispose(
            &mut machine,
            execute(&candidate.id),
            &unknown,
            &NoProvider,
            &mut MemoryJournal::default(),
        );
        assert_eq!(
            paused(&machine),
            ("done_unknown", RunState::Uncertain, false)
        );
        assert_eq!(machine.index, 0);
        assert_eq!(unknown.dispatched(), 0);
    }

    #[test]
    fn caller_execute_reobserves_done_first_then_dispatches_once_without_operation_reask() {
        let job = force_stop(mutation_job());
        let redactor = Redactor::for_job(&job).unwrap();
        let browser = FakeBrowser::new([text_field(""), text_field(""), text_field("Wanted")])
            .dispatching([typed("Wanted")]);
        let provider = ScriptedProvider::new([Turn::type_text()]);
        let mut journal = MemoryJournal::default();
        let mut machine = HostedMachine::new(&job, &redactor);
        drive(&mut machine, &browser, &provider, &mut journal);
        let candidate = offered(&machine);

        dispose(
            &mut machine,
            execute(&candidate.id),
            &browser,
            &provider,
            &mut journal,
        );
        drive(&mut machine, &browser, &provider, &mut journal);

        assert_eq!(machine.index, 1);
        assert!(machine.artifacts.caller_assisted);
        assert_eq!(
            provider.calls(),
            1,
            "resume execute must not re-ask the operation gate"
        );
        let prepared = journal.prepared();
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0]["basis"], "caller_authority");
    }

    #[test]
    fn resume_done_judgment_records_the_natural_language_decision() {
        let job = natural(mutation_job(), "The form is complete");
        let redactor = Redactor::for_job(&job).unwrap();
        let mut machine = HostedMachine::new(&job, &redactor);
        let captured = Captured {
            raw: text_field(""),
            artifact: ("o_resume".into(), json!({}), None),
            redaction_verified: true,
        };
        let Ok((done, noul)) = machine.judge_resume_done(
            "The form is complete",
            &captured,
            &ScriptedProvider::new([Turn::type_text()]),
        ) else {
            panic!("natural-language resume judgment should succeed");
        };
        assert_eq!(done, DoneResult::NotSatisfied);
        assert_eq!(noul, Some(0.01));
        assert_eq!(machine.artifacts.decisions.len(), 1);
    }

    #[test]
    fn resumed_unknown_done_reissues_the_condition_specific_reason() {
        for (job, reason) in [
            (
                natural(mutation_job(), "The form is complete"),
                "done_uncertain",
            ),
            (mutation_job(), "done_unknown"),
        ] {
            let redactor = Redactor::for_job(&job).unwrap();
            let mut machine = HostedMachine::new(&job, &redactor);
            let observation = text_field("");
            machine.reissue_unknown_done(
                PendingEscalation {
                    done: DoneResult::NotSatisfied,
                    noul: None,
                    candidate: None,
                    observation: observation.clone(),
                    ambiguous_mutation: false,
                },
                observation,
                Some(0.5),
            );
            assert_eq!(machine.artifacts.stop.as_ref().unwrap().code, reason);
            assert!(
                machine
                    .artifacts
                    .pending
                    .as_ref()
                    .unwrap()
                    .candidate
                    .is_none()
            );
        }
    }
}
