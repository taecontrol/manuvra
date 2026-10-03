//! Caller dispositions on a paused run: `abort`, `retry_observation`, and `advance` attestations
//! of a step or of final verification. `execute` is answered in the sibling `execute` module.

use super::HostedTermination;
use super::artifacts::{PendingEscalation, record_step};
use super::capture::{Captured, DriveBrowser, capture_step, record_capture, redacted_value};
use super::escalation::{escalate_verification, reissue_escalation};
use super::final_verification::{
    VerificationProgress, capture_verification_advance, evaluate_final, record_verification,
};
use super::machine::HostedMachine;
use super::stops::{Stop, control_stop, policy_stop, step_detail};
use crate::actions;
use crate::verification::{DoneResult, natural_numeric_literals_satisfied};
use manuvra_chrome::Observation;
use manuvra_contract::{
    Disposition, DispositionRequest, DoneCondition, StepVerdict, VerdictResult,
};
use serde_json::{Value, json};

impl HostedMachine<'_> {
    pub(super) fn apply(
        &mut self,
        request: DispositionRequest,
        browser: &impl DriveBrowser,
        evaluator: &impl manuvra_jev::Evaluator,
        journal: &mut impl actions::ActionJournal,
        cancellation: &manuvra_chrome::InputCancellation,
    ) -> Option<HostedTermination> {
        let name = format!("request_{:04}", self.artifacts.dispositions.len() + 1);
        self.artifacts
            .dispositions
            .push((name, redacted_value(&request, self.redactor)));
        match request.disposition {
            Disposition::Abort(_) => {
                self.artifacts.caller_assisted = true;
                Some(HostedTermination::Aborted)
            }
            Disposition::RetryObservation(_) => {
                self.artifacts.caller_assisted = true;
                self.clear_pause();
                None
            }
            Disposition::Advance(advance) => {
                if self.artifacts.pending_verification.is_some() {
                    self.apply_verification_advance(&advance.rationale, browser, evaluator);
                } else {
                    self.apply_advance(&advance.rationale, browser);
                }
                None
            }
            Disposition::Execute(execute) => {
                if self.artifacts.pending_verification.is_some() {
                    self.reissue_verification("execute_not_permitted");
                } else {
                    self.apply_execute(
                        &execute.candidate_id,
                        browser,
                        evaluator,
                        journal,
                        cancellation,
                    );
                }
                None
            }
        }
    }

    pub(super) fn clear_pause(&mut self) {
        self.artifacts.stop = None;
        self.artifacts.escalation = None;
        self.artifacts.pending = None;
        self.artifacts.pending_verification = None;
    }

    fn apply_verification_advance(
        &mut self,
        rationale: &str,
        browser: &impl DriveBrowser,
        evaluator: &impl manuvra_jev::Evaluator,
    ) {
        let Some(pending) = self.artifacts.pending_verification.clone() else {
            return;
        };
        if rationale.trim().is_empty() || !pending.attestable() {
            self.reissue_verification("advance_not_permitted");
            return;
        }
        let observation = match capture_verification_advance(
            self.job,
            self.redactor,
            browser,
            &self.policy,
            &mut self.artifacts,
        ) {
            Ok(observation) => observation,
            Err(stop) => return self.end_with(stop),
        };
        let assertions = super::capture::final_assertions(self.job);
        let prior_hash = relevant_state_hash(&pending.observation, &assertions);
        let current_hash = relevant_state_hash(&observation, &assertions);
        let identity_unchanged = pending.observation.document_id == observation.document_id;
        let facts_unchanged = prior_hash == current_hash;
        self.artifacts.trace.push(json!({
            "event":"verification_disposition_check",
            "kind":"advance",
            "document_identity_unchanged":identity_unchanged,
            "relevant_facts_unchanged":facts_unchanged,
            "prior_state_hash":prior_hash,
            "current_state_hash":current_hash,
        }));
        if !(identity_unchanged && facts_unchanged) {
            self.recheck_changed_verification(observation, evaluator);
            return;
        }
        self.accept_verification_attestation(rationale);
    }

    fn accept_verification_attestation(&mut self, rationale: &str) {
        for verdict in &mut self.artifacts.expectation_verdicts {
            if verdict.result == VerdictResult::Unresolved {
                verdict.result = VerdictResult::Satisfied;
            }
        }
        if let Some(Value::Object(record)) = &mut self.artifacts.verification {
            record.insert("basis".into(), json!("caller_attestation"));
            record.insert(
                "rationale".into(),
                json!(self.redactor.redact_export_text(rationale)),
            );
            record.insert(
                "expectations".into(),
                serde_json::to_value(&self.artifacts.expectation_verdicts).unwrap_or(Value::Null),
            );
        }
        self.artifacts.caller_assisted = true;
        self.verification_complete = true;
        self.clear_pause();
    }

    fn recheck_changed_verification(
        &mut self,
        observation: Observation,
        evaluator: &impl manuvra_jev::Evaluator,
    ) {
        self.artifacts.stop = None;
        self.artifacts.escalation = None;
        self.artifacts.pending_verification = None;
        let report = match evaluate_final(
            self.job,
            self.redactor,
            &observation,
            &self.values,
            evaluator,
            &mut self.policy,
            &mut self.artifacts,
        ) {
            Ok(report) => report,
            Err(stop) => return self.end_with(stop),
        };
        match record_verification(
            report,
            observation,
            self.redactor,
            &mut self.artifacts,
            "verification_state_changed",
        ) {
            VerificationProgress::Complete => self.verification_complete = true,
            VerificationProgress::Stop(stop) => self.artifacts.stop = Some(stop),
        }
    }

    /// Ends the run with a terminal stop reached while answering a disposition: the pause is
    /// cleared, so no escalation is published with it and the run is never driven again.
    pub(super) fn end_with(&mut self, stop: Stop) {
        self.clear_pause();
        self.artifacts.stop = Some(stop);
    }

    fn reissue_verification(&mut self, reason: &'static str) {
        self.artifacts.stop = Some(escalate_verification(
            &mut self.artifacts,
            self.redactor,
            reason,
        ));
    }

    fn apply_advance(&mut self, rationale: &str, browser: &impl DriveBrowser) {
        let Some(pending) = self.artifacts.pending.clone() else {
            self.reissue("stale_escalation", None);
            return;
        };
        let observation = match self.capture_for_disposition(browser, "advance_observation") {
            Ok(captured) => {
                self.artifacts.observations.push(captured.artifact);
                captured.raw
            }
            Err(stop) => return self.end_with(stop),
        };
        match self.check_advance(&pending, &observation, rationale) {
            Ok(()) => self.attest_step(),
            Err(reason) => self.reissue(reason, Some(pending)),
        }
    }

    /// An advance attests the escalated observation, so the fresh one must show the same document
    /// and the same relevant facts, and the escalation must permit an attestation.
    fn check_advance(
        &mut self,
        pending: &PendingEscalation,
        current: &Observation,
        rationale: &str,
    ) -> Result<(), &'static str> {
        let step = &self.job.steps[self.index];
        let assertions = super::capture::done_assertions(&step.done_when);
        let prior_hash = relevant_state_hash(&pending.observation, assertions);
        let current_hash = relevant_state_hash(current, assertions);
        let identity_unchanged = pending.observation.document_id == current.document_id;
        let facts_unchanged = prior_hash == current_hash;
        let permitted = advance_permitted(step, pending, rationale);
        self.artifacts.trace.push(json!({
            "event":"disposition_check",
            "kind":"advance",
            "permitted":permitted,
            "document_identity_unchanged":identity_unchanged,
            "relevant_state_unchanged":facts_unchanged,
            "prior_state_hash":prior_hash,
            "current_state_hash":current_hash,
            "numeric_checks_satisfied":natural_condition_numeric_checks_satisfied(step, pending),
            "pending_ambiguous_mutation":pending.ambiguous_mutation,
            "pending_candidate":pending.candidate.is_some(),
        }));
        if !(identity_unchanged && facts_unchanged) {
            return Err("relevant_state_changed");
        }
        permitted.then_some(()).ok_or("advance_not_permitted")
    }

    fn attest_step(&mut self) {
        let step = &self.job.steps[self.index];
        self.artifacts.caller_assisted = true;
        self.artifacts.verdicts[self.index] = StepVerdict {
            id: self.redactor.redact_export_text(&step.id),
            result: VerdictResult::Satisfied,
            basis: Some("caller_attestation".into()),
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
            "caller_attestation",
        );
        self.advance_step();
        self.clear_pause();
    }

    /// A fresh observation for a disposition. Like an autonomous observation it must be on an
    /// allowed origin and verifiably redacted before a provider call or completion rests on it.
    pub(super) fn capture_for_disposition(
        &mut self,
        browser: &impl DriveBrowser,
        event: &str,
    ) -> Result<Captured, Stop> {
        let step = &self.job.steps[self.index];
        let captured = capture_step(
            browser,
            self.redactor,
            super::capture::done_assertions(&step.done_when),
            self.index + 1,
            self.artifacts.observations.len() + 1,
        )
        .map_err(control_stop)?;
        self.policy
            .check_origin(&captured.raw)
            .map_err(|stop| policy_stop(stop, self.redactor, step))?;
        if !captured.redaction_verified {
            record_capture(
                &mut self.artifacts,
                self.redactor,
                step,
                &captured,
                event,
                DoneResult::Unknown,
            );
            return Err(Stop::blocked(
                "redaction_unverifiable",
                step_detail(self.redactor, step),
            ));
        }
        Ok(captured)
    }

    pub(super) fn reissue(&mut self, reason: &'static str, pending: Option<PendingEscalation>) {
        let step = &self.job.steps[self.index];
        self.artifacts.pending = pending;
        self.artifacts.stop = Some(reissue_escalation(
            &mut self.artifacts,
            self.redactor,
            self.index,
            step,
            reason,
        ));
    }
}

fn advance_permitted(
    step: &manuvra_contract::Step,
    pending: &PendingEscalation,
    rationale: &str,
) -> bool {
    let DoneCondition::NaturalLanguage(condition) = &step.done_when else {
        return false;
    };
    if pending.done != DoneResult::Unknown || pending.noul.is_none_or(|noul| noul <= 0.20) {
        return false;
    }
    natural_numeric_literals_satisfied(condition, &pending.observation)
        && !pending.ambiguous_mutation
        && pending.candidate.is_none()
        && !rationale.trim().is_empty()
}

pub(super) fn natural_condition_numeric_checks_satisfied(
    step: &manuvra_contract::Step,
    pending: &PendingEscalation,
) -> bool {
    let DoneCondition::NaturalLanguage(condition) = &step.done_when else {
        return false;
    };
    natural_numeric_literals_satisfied(condition, &pending.observation)
}

fn relevant_state_hash(
    observation: &Observation,
    assertions: &[manuvra_contract::Assertion],
) -> String {
    use sha2::{Digest, Sha256};

    let mut state = relevant_state(observation);
    let colors = crate::verification::color_assertion_checks(assertions, observation);
    if !colors.is_empty() {
        state["assertion_checks"] = json!(colors);
    }
    hex::encode(Sha256::digest(state.to_string()))
}

/// The observed facts an attestation rests on. Hover regions are part of the observation the
/// escalation publishes and of the provider's view; they are included only when listed, so pages
/// without them hash exactly as before hover regions existed.
fn relevant_state(observation: &Observation) -> Value {
    let mut state = json!({
        "url": observation.url,
        "route": observation.route,
        "title": observation.title,
        "dialogs": observation.dialogs,
        "dialog_texts": observation.dialog_texts,
        "focused": observation.focused,
        "focus_anchor": observation.focus_anchor,
        "visible_text": observation.visible_text,
        "covered_text": observation.covered_text,
        "elements": observation.elements,
        "coverage": observation.coverage,
    });
    if !observation.hover_regions.is_empty() {
        state["hover_regions"] = json!(observation.hover_regions);
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::Redactor;
    use crate::policy;
    use crate::run::escalation::allowed_dispositions;
    use crate::run::tests::support::*;
    use manuvra_chrome::BrowserError;
    use manuvra_contract::{DispositionKind, Job, RunState};

    /// A run paused on uncertain final verification of the expectation job.
    fn verification_escalation<'a>(
        job: &'a Job,
        redactor: &'a Redactor,
        browser: &FakeBrowser,
        provider: &ScriptedProvider,
    ) -> HostedMachine<'a> {
        let mut machine = HostedMachine::new(job, redactor);
        drive(
            &mut machine,
            browser,
            provider,
            &mut MemoryJournal::default(),
        );
        let escalation = machine.artifacts.escalation.clone().unwrap();
        assert_eq!(escalation.phase, "verification");
        machine
    }

    #[test]
    fn verification_uncertainty_allows_attestation_but_refuses_execute() {
        let job = expectation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let browser = FakeBrowser::new([observed("Ready"), observed("Ready balance 12.34")]);
        let provider = ScriptedProvider::new([Turn::verdict(0.50)]);
        let mut machine = verification_escalation(&job, &redactor, &browser, &provider);
        let escalation = machine.artifacts.escalation.clone().unwrap();
        assert_eq!(escalation.step_id, None);
        assert_eq!(
            escalation.dispositions,
            [
                DispositionKind::Advance,
                DispositionKind::RetryObservation,
                DispositionKind::Abort,
            ]
        );
        let mut journal = MemoryJournal::default();

        dispose(
            &mut machine,
            execute("not-offered"),
            &browser,
            &provider,
            &mut journal,
        );
        assert_eq!(
            machine.artifacts.stop.as_ref().unwrap().code,
            "execute_not_permitted"
        );
        assert!(!machine.verification_complete);

        dispose(
            &mut machine,
            advance("Observed the final account facts directly."),
            &browser,
            &provider,
            &mut journal,
        );
        assert!(machine.verification_complete);
        assert!(machine.artifacts.stop.is_none());
        assert!(machine.artifacts.caller_assisted);
        assert_eq!(
            machine.artifacts.expectation_verdicts[0].result,
            VerdictResult::Satisfied
        );
        assert_eq!(
            machine.artifacts.verification.as_ref().unwrap()["basis"],
            "caller_attestation"
        );
    }

    #[test]
    fn verification_attestation_rechecks_changed_facts_identity_and_focus() {
        let mut scoped = expectation_job();
        let manuvra_contract::Expectation::NaturalLanguage(expectation) =
            &mut scoped.expectations[0]
        else {
            panic!("natural expectation")
        };
        expectation.exact_literals = vec![manuvra_contract::ExactLiteral {
            literal: "12.34".into(),
            within_text: Some("Wallet".into()),
        }];
        let wallet = observed("Ready\nWallet balance 12.34");
        let mut reserve = wallet.clone();
        reserve.visible_text = "Ready\nWallet balance 12.34\nWallet reserve 12.34".into();
        let balance = observed("Ready balance 12.34");
        let mut remounted = balance.clone();
        remounted.document_id = "remounted-document".into();
        let mut unfocused = text_field("");
        unfocused.visible_text = "Ready balance 12.34".into();
        let mut focused = unfocused.clone();
        focused.focused = Some(1);
        for (job, attested, current, identity_unchanged, facts_unchanged) in [
            (scoped, wallet, reserve, true, false),
            (expectation_job(), balance, remounted, false, true),
            (expectation_job(), unfocused, focused, true, false),
        ] {
            let redactor = Redactor::for_job(&job).unwrap();
            let browser = FakeBrowser::new([observed("Ready"), attested, current]);
            let provider = ScriptedProvider::new([Turn::verdict(0.50)]);
            let mut machine = verification_escalation(&job, &redactor, &browser, &provider);

            dispose(
                &mut machine,
                advance("Caller attests the prior final facts."),
                &browser,
                &provider,
                &mut MemoryJournal::default(),
            );

            assert!(!machine.verification_complete);
            assert!(!machine.artifacts.caller_assisted);
            assert_eq!(machine.artifacts.observations.len(), 3);
            assert_eq!(
                machine.artifacts.stop.as_ref().unwrap().code,
                "verification_state_changed"
            );
            assert_eq!(
                machine.artifacts.expectation_verdicts[0].result,
                VerdictResult::Unresolved
            );
            assert_eq!(provider.calls(), 2);
            let check = machine
                .artifacts
                .trace
                .iter()
                .find(|entry| entry["event"] == "verification_disposition_check")
                .unwrap();
            assert_eq!(check["document_identity_unchanged"], identity_unchanged);
            assert_eq!(check["relevant_facts_unchanged"], facts_unchanged);
        }
    }

    #[test]
    fn retry_observation_rechecks_final_expectations_without_resetting_the_run() {
        let job = expectation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let browser = FakeBrowser::new([observed("Ready"), observed("Ready balance 12.34")]);
        let provider = ScriptedProvider::new([Turn::verdict(0.50), Turn::verdict(0.95)]);
        let mut machine = verification_escalation(&job, &redactor, &browser, &provider);
        let mut journal = MemoryJournal::default();

        dispose(&mut machine, retry(), &browser, &provider, &mut journal);
        drive(&mut machine, &browser, &provider, &mut journal);

        assert!(machine.verification_complete);
        assert!(machine.artifacts.stop.is_none());
        assert_eq!(machine.artifacts.observations.len(), 3);
        assert_eq!(machine.artifacts.expectation_verdicts[0].noul, Some(0.95));
    }

    #[test]
    fn retry_observation_preserves_model_budget_and_replay_ledger() {
        let mut job = force_stop(mutation_job());
        job.options.max_model_calls = Some(1);
        let redactor = Redactor::for_job(&job).unwrap();
        let observation = text_field("");
        let browser = FakeBrowser::new([observation.clone()]);
        let provider = ScriptedProvider::new([Turn::type_text()]);
        let mut machine = HostedMachine::new(&job, &redactor);
        let mut journal = MemoryJournal::default();
        drive(&mut machine, &browser, &provider, &mut journal);
        let candidate = offered(&machine);
        let _reserved = machine
            .policy
            .authorize_caller(&job.steps[0], &observation, &candidate)
            .unwrap();

        dispose(&mut machine, retry(), &browser, &NoProvider, &mut journal);

        assert!(machine.artifacts.stop.is_none());
        assert!(matches!(
            machine.policy.record_model_call(),
            Err(policy::PolicyStop::Blocked("budget_exhausted"))
        ));
        assert!(matches!(
            machine
                .policy
                .authorize_caller(&job.steps[0], &observation, &candidate),
            Err(policy::PolicyStop::Uncertain("replay_forbidden"))
        ));
    }

    fn attestable(observation: Observation) -> PendingEscalation {
        PendingEscalation {
            done: DoneResult::Unknown,
            noul: Some(0.5),
            candidate: None,
            observation,
            ambiguous_mutation: false,
        }
    }

    #[test]
    fn advance_requires_natural_uncertainty_without_pending_mutation_and_unchanged_state() {
        let job = natural(job("unused"), "The journey is complete");
        let redactor = Redactor::for_job(&job).unwrap();
        let observation = observed("unchanged");
        let browser = FakeBrowser::new([observation.clone()]);
        let mut machine = HostedMachine::new(&job, &redactor);
        machine.artifacts.pending = Some(attestable(observation.clone()));

        machine.apply_advance("caller verified the condition", &browser);

        assert_eq!(machine.index, 1);
        assert_eq!(
            machine.artifacts.verdicts[0].basis.as_deref(),
            Some("caller_attestation")
        );
        assert_eq!(machine.artifacts.observations.len(), 1);
        let advance_check = machine
            .artifacts
            .trace
            .iter()
            .find(|entry| entry["event"] == "disposition_check")
            .unwrap();
        assert_eq!(advance_check["document_identity_unchanged"], true);
        assert_eq!(advance_check["relevant_state_unchanged"], true);
        assert_eq!(advance_check["numeric_checks_satisfied"], true);
        assert_eq!(
            advance_check["prior_state_hash"],
            advance_check["current_state_hash"]
        );

        let allowed = attestable(observed("unchanged"));
        let mut low_noul = allowed.clone();
        low_noul.noul = Some(0.20);
        assert!(!advance_permitted(&job.steps[0], &low_noul, "attested"));
        let mut not_satisfied = allowed.clone();
        not_satisfied.done = DoneResult::NotSatisfied;
        assert!(!advance_permitted(
            &job.steps[0],
            &not_satisfied,
            "attested"
        ));
        let mut ambiguous = allowed.clone();
        ambiguous.ambiguous_mutation = true;
        assert!(!advance_permitted(&job.steps[0], &ambiguous, "attested"));
        assert!(!advance_permitted(&job.steps[0], &allowed, ""));

        let numeric_job = natural(job.clone(), "The balance is 12.34");
        assert!(!advance_permitted(
            &numeric_job.steps[0],
            &allowed,
            "attested"
        ));
        assert!(
            !allowed_dispositions(&numeric_job.steps[0], &allowed)
                .contains(&DispositionKind::Advance)
        );

        let structured_job = mutation_job();
        let structured_redactor = Redactor::for_job(&structured_job).unwrap();
        let mut structured = HostedMachine::new(&structured_job, &structured_redactor);
        structured.artifacts.pending = Some(PendingEscalation {
            done: DoneResult::NotSatisfied,
            ..attestable(observation)
        });
        structured.apply_advance("not allowed", &browser);
        assert_eq!(structured.index, 0);
        assert_eq!(
            structured.artifacts.stop.as_ref().unwrap().code,
            "advance_not_permitted"
        );
        assert!(!structured.artifacts.caller_assisted);
        assert!(!advance_permitted(
            &structured_job.steps[0],
            &allowed,
            "attested"
        ));
    }

    #[test]
    fn advance_is_refused_when_the_document_or_a_hover_region_changed() {
        let job = natural(job("unused"), "Groceries is archived");
        let redactor = Redactor::for_job(&job).unwrap();
        let attested = plan_before_hover();
        let mut revealing = attested.clone();
        revealing.hover_regions[0].reveals_on_hover = vec!["Unarchive Groceries".into()];
        assert_eq!(revealing.visible_text, attested.visible_text);
        let mut remounted = attested.clone();
        remounted.document_id = "remounted-document".into();
        for (current, identity_unchanged, facts_unchanged) in
            [(revealing, true, false), (remounted, false, true)]
        {
            let mut machine = HostedMachine::new(&job, &redactor);
            machine.artifacts.pending = Some(attestable(attested.clone()));

            machine.apply_advance(
                "caller verified the condition",
                &FakeBrowser::new([current]),
            );

            assert_eq!(machine.index, 0);
            assert_eq!(
                paused(&machine),
                ("relevant_state_changed", RunState::Uncertain, false)
            );
            assert!(!machine.artifacts.caller_assisted);
            assert_eq!(machine.artifacts.observations.len(), 1);
            let check = machine
                .artifacts
                .trace
                .iter()
                .find(|entry| entry["event"] == "disposition_check")
                .unwrap();
            assert_eq!(check["document_identity_unchanged"], identity_unchanged);
            assert_eq!(check["relevant_state_unchanged"], facts_unchanged);
        }
    }

    #[test]
    fn advance_observation_is_guarded_like_every_other_observation() {
        let job = natural(job("unused"), "The journey is complete");
        let redactor = Redactor::for_job(&job).unwrap();
        for (browser, code) in [
            (
                FakeBrowser::new([foreign(observed("unchanged"))]),
                "origin_not_allowed",
            ),
            (
                FakeBrowser::capturing(
                    [Err(BrowserError::Control("redaction_unverifiable".into()))],
                    observed("unchanged"),
                ),
                "redaction_unverifiable",
            ),
            (
                FakeBrowser::capturing(
                    [Err(BrowserError::Control("target closed".into()))],
                    observed("unchanged"),
                ),
                "browser_control_failed",
            ),
        ] {
            let mut machine = HostedMachine::new(&job, &redactor);
            machine.artifacts.pending = Some(attestable(observed("unchanged")));

            machine.apply_advance("caller verified the condition", &browser);

            let stop = machine.artifacts.stop.as_ref().unwrap();
            assert_eq!((stop.code, stop.state), (code, RunState::Blocked));
            assert!(machine.artifacts.escalation.is_none());
            assert_eq!(machine.index, 0);
            assert!(!machine.artifacts.caller_assisted);
        }
    }

    #[test]
    fn relevant_state_without_hover_regions_hashes_as_before_hover_regions_existed() {
        assert_eq!(
            relevant_state_hash(&observed("unchanged"), &[]),
            "d6eaad215cf80aaf06c0b26ea36d0d650848ac1cfb5ac198d8283893fcc8ab4d"
        );
    }
}
