//! The hosted loop: it drives the run, pauses on an escalation until a disposition or a
//! termination answers it, and publishes running, paused, and terminal checkpoints.

use super::artifacts::RunArtifacts;
use super::browser::{HostedBrowser, StartedBrowser, StartupFailure, browser_config};
use super::machine::HostedMachine;
use super::publish::{
    apply_artifact_verdict, publish_browser_error, publish_browser_error_with_provenance,
    publish_bundle, run_result_with_assistance,
};
use super::stops::terminal_fields;
use super::{FlowConfig, FlowOutcome, HostedControl, HostedEvent, HostedTermination, target_url};
use crate::actions;
use crate::evidence::Redactor;
use manuvra_chrome::OwnedBrowser;
use manuvra_contract::{Cleanup, Job, Reason, RunState, VerdictResult};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Instant;

pub fn run_hosted(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    provider_key: Option<String>,
    control: &dyn HostedControl,
) -> Result<FlowOutcome, String> {
    run_with_browser(job, config, redactor, provider_key, control)
}

fn run_with_browser(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    provider_key: Option<String>,
    control: &dyn HostedControl,
) -> Result<FlowOutcome, String> {
    let started = StartedBrowser::launch(browser_config(job, &config), target_url(job))
        .and_then(StartedBrowser::navigate);
    match started {
        Ok(started) => finish_browser_run(
            job,
            config,
            redactor,
            started.browser,
            started.provenance,
            provider_key,
            control,
        ),
        Err(StartupFailure::Launch(error)) => publish_browser_error(job, config, redactor, error),
        Err(StartupFailure::AfterLaunch(failure)) => publish_browser_error_with_provenance(
            job,
            config,
            redactor,
            failure.error,
            failure.provenance,
            failure.cleanup,
        ),
    }
}

fn finish_browser_run(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    mut browser: OwnedBrowser,
    provenance: Value,
    provider_key: Option<String>,
    control: &dyn HostedControl,
) -> Result<FlowOutcome, String> {
    let mut journal =
        actions::DurableJournal::open(&config.evidence_root, &config.run_id, redactor)?;
    finish_hosted_browser_run(
        job,
        config,
        redactor,
        &mut browser,
        provenance,
        &LazyEvaluator::new(provider_key),
        &mut journal,
        &control.cancellation(),
        control,
    )
}

/// The action journal of a hosted run, cleared once the run's evidence is published.
pub(super) trait RunJournal: actions::ActionJournal {
    fn clear(&mut self) -> Result<(), String>;
}

impl RunJournal for actions::DurableJournal {
    fn clear(&mut self) -> Result<(), String> {
        actions::DurableJournal::clear(self)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn finish_hosted_browser_run(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    browser: &mut impl HostedBrowser,
    provenance: Value,
    evaluator: &impl manuvra_jev::Evaluator,
    journal: &mut impl RunJournal,
    cancellation: &manuvra_chrome::InputCancellation,
    control: &dyn HostedControl,
) -> Result<FlowOutcome, String> {
    let mut machine = HostedMachine::new(job, redactor);
    loop {
        if machine.artifacts.stop.is_none() {
            machine.drive(
                browser,
                evaluator,
                journal,
                cancellation,
                Some(&config),
                control,
            );
        }
        if let Some(termination) = control.termination() {
            return publish_active_hosted_stop(
                job,
                config,
                redactor,
                browser,
                provenance,
                machine.artifacts,
                journal,
                termination,
            );
        }
        let Some(escalation_id) = machine.paused_escalation_id() else {
            return finish_hosted_terminal(
                job, config, redactor, browser, provenance, machine, journal, control,
            );
        };
        if let Some(outcome) = handle_hosted_pause(
            job,
            &config,
            redactor,
            browser,
            provenance.clone(),
            evaluator,
            journal,
            cancellation,
            control,
            &mut machine,
            &escalation_id,
        )? {
            return Ok(outcome);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_hosted_pause(
    job: &Job,
    config: &FlowConfig,
    redactor: &Redactor,
    browser: &mut impl HostedBrowser,
    provenance: Value,
    evaluator: &impl manuvra_jev::Evaluator,
    journal: &mut impl RunJournal,
    cancellation: &manuvra_chrome::InputCancellation,
    control: &dyn HostedControl,
    machine: &mut HostedMachine<'_>,
    escalation_id: &str,
) -> Result<Option<FlowOutcome>, String> {
    set_hosted_escalation_deadline(&mut machine.artifacts, control.pause_deadline_unix_ms());
    let published = publish_pause_checkpoint(
        job,
        config,
        redactor,
        provenance.clone(),
        &machine.artifacts,
    )?;
    control.publish_checkpoint(&published.result)?;
    machine.policy.pause();
    match control.wait_while_paused(escalation_id) {
        HostedEvent::Termination(termination) => finalize_paused_termination(
            job,
            config,
            redactor,
            browser,
            provenance,
            journal,
            control,
            machine,
            termination,
        )
        .map(Some),
        HostedEvent::Disposition(request) => {
            machine.policy.resume();
            machine
                .apply(request, browser, evaluator, journal, cancellation)
                .map_or(Ok(None), |termination| {
                    finalize_paused_termination(
                        job,
                        config,
                        redactor,
                        browser,
                        provenance,
                        journal,
                        control,
                        machine,
                        termination,
                    )
                    .map(Some)
                })
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn finalize_paused_termination(
    job: &Job,
    config: &FlowConfig,
    redactor: &Redactor,
    browser: &mut impl HostedBrowser,
    provenance: Value,
    journal: &mut impl RunJournal,
    control: &dyn HostedControl,
    machine: &mut HostedMachine<'_>,
    termination: HostedTermination,
) -> Result<FlowOutcome, String> {
    let artifacts = std::mem::replace(&mut machine.artifacts, RunArtifacts::new(job, redactor));
    publish_active_hosted_stop(
        job,
        config.clone(),
        redactor,
        browser,
        provenance,
        artifacts,
        journal,
        termination,
    )
    .and_then(|outcome| publish_terminal_checkpoint(control, journal, outcome))
}

#[allow(clippy::too_many_arguments)]
fn finish_hosted_terminal(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    browser: &mut impl HostedBrowser,
    provenance: Value,
    machine: HostedMachine<'_>,
    journal: &mut impl RunJournal,
    control: &dyn HostedControl,
) -> Result<FlowOutcome, String> {
    let cleanup = browser.cleanup_hosted();
    let (state, reason, exit_code, overall) = terminal_fields(machine.artifacts.stop.clone());
    let result = run_result_with_assistance(
        job,
        &config,
        redactor,
        state,
        reason,
        overall,
        machine.artifacts.verdicts.clone(),
        machine.artifacts.escalation.clone(),
        cleanup.clone(),
        machine.artifacts.caller_assisted,
    )?;
    let mut result = result;
    apply_artifact_verdict(&mut result, &machine.artifacts)?;
    let outcome = publish_bundle(
        job,
        config,
        redactor,
        provenance,
        machine.artifacts.into_recorded(cleanup),
        result,
        exit_code,
    )?;
    control.publish_checkpoint(&outcome.result)?;
    let _ = journal.clear();
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
fn publish_active_hosted_stop(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    browser: &mut impl HostedBrowser,
    provenance: Value,
    artifacts: RunArtifacts,
    journal: &mut impl RunJournal,
    termination: HostedTermination,
) -> Result<FlowOutcome, String> {
    let cleanup = browser.cleanup_hosted();
    let (state, code, exit_code) = hosted_termination_fields(termination);
    let result = run_result_with_assistance(
        job,
        &config,
        redactor,
        state,
        Some(Reason {
            code: code.into(),
            details: BTreeMap::new(),
        }),
        VerdictResult::Unresolved,
        artifacts.verdicts.clone(),
        artifacts.escalation.clone(),
        cleanup.clone(),
        artifacts.caller_assisted,
    )?;
    let mut result = result;
    apply_artifact_verdict(&mut result, &artifacts)?;
    publish_bundle(
        job,
        config,
        redactor,
        provenance,
        artifacts.into_recorded(cleanup),
        result,
        exit_code,
    )
    .inspect(|_| {
        let _ = journal.clear();
    })
}

fn publish_terminal_checkpoint(
    control: &dyn HostedControl,
    journal: &mut impl RunJournal,
    outcome: FlowOutcome,
) -> Result<FlowOutcome, String> {
    control.publish_checkpoint(&outcome.result).map(|()| {
        let _ = journal.clear();
        outcome
    })
}

fn set_hosted_escalation_deadline(artifacts: &mut RunArtifacts, deadline: u64) {
    if let Some(escalation) = &mut artifacts.escalation {
        escalation.expires_at = deadline.to_string();
    }
}

fn publish_pause_checkpoint(
    job: &Job,
    config: &FlowConfig,
    redactor: &Redactor,
    provenance: Value,
    artifacts: &RunArtifacts,
) -> Result<FlowOutcome, String> {
    let (state, reason, exit_code, overall) = terminal_fields(artifacts.stop.clone());
    let retained = Cleanup {
        browser: "alive".into(),
        profile: "retained".into(),
        application_state: "caller_owned".into(),
    };
    let mut checkpoint = run_result_with_assistance(
        job,
        config,
        redactor,
        state,
        reason,
        overall,
        artifacts.verdicts.clone(),
        artifacts.escalation.clone(),
        retained.clone(),
        artifacts.caller_assisted,
    )?;
    apply_artifact_verdict(&mut checkpoint, artifacts)?;
    checkpoint["terminal"] = json!(false);
    publish_bundle(
        job,
        config.clone(),
        redactor,
        provenance,
        artifacts.recorded(retained),
        checkpoint,
        exit_code,
    )
}

fn hosted_termination_fields(termination: HostedTermination) -> (RunState, &'static str, u8) {
    match termination {
        HostedTermination::Aborted => (RunState::Aborted, "caller_aborted", 5),
        HostedTermination::PauseDeadlineElapsed => {
            (RunState::Expired, "resume_deadline_elapsed", 5)
        }
        HostedTermination::LifetimeElapsed => (RunState::Expired, "lifetime_elapsed", 5),
        HostedTermination::WatchdogLost => (RunState::Blocked, "watchdog_lost", 3),
    }
}

struct LazyEvaluator {
    client: OnceLock<Result<manuvra_jev::Client, manuvra_jev::JevError>>,
    provider_key: Option<String>,
}

impl LazyEvaluator {
    fn new(provider_key: Option<String>) -> Self {
        Self {
            client: OnceLock::new(),
            provider_key,
        }
    }
}

impl manuvra_jev::Evaluator for LazyEvaluator {
    fn evaluate(
        &self,
        request: &Value,
        deadline: Instant,
    ) -> Result<manuvra_jev::Evaluation, manuvra_jev::JevError> {
        self.client
            .get_or_init(|| {
                self.provider_key
                    .clone()
                    .map_or_else(manuvra_jev::Client::from_environment, |key| {
                        manuvra_jev::Client::from_key(key)
                    })
            })
            .as_ref()
            .map_err(Clone::clone)?
            .evaluate(request, deadline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::tests::live::*;
    use crate::run::tests::support::*;
    use manuvra_chrome::{
        BrowserError, CapturedPage, Element, Observation, PerformError, PerformFact,
    };
    use tempfile::TempDir;

    #[test]
    #[ignore = "requires the local Chromium executable"]
    fn hosted_scroll_reaches_the_locality_below_the_table_fold() {
        let fixture = HoverRevealFixture::with_body(include_str!(
            "../../../../tests/browser/scroll-app-shell-table.html"
        ));
        let url = fixture.url();
        let job = parse_job(
            json!({"schema_version":1,"target":{"kind":"browser","url":url},"context":{"journey":"scroll to locality","revision":"fixture","environment":"local Chromium","actor":"synthetic","authority":"open locality 52"},"steps":[{"id":"open","goal":"Open locality 52","done_when":[{"text_visible":"Opened locality 52"}]}]}),
        );
        let mut browser = LiveBrowser::open(&url);
        let provider = RowActionProvider::scrolling(vec![("Open locality 52", "Open locality 52")]);
        let mut journal = MemoryJournal::default();
        let run = run_loop(
            &job,
            &mut browser,
            &provider,
            &ScriptedControl::default(),
            &mut journal,
        );
        assert_eq!(run.state(), "passed", "{}", run.outcome.result);
        assert!(
            journal
                .prepared()
                .iter()
                .any(|e| e["scroll_target"]["name"] == "Localities")
        );
        assert!(
            browser
                .final_page
                .unwrap()
                .visible_text
                .contains("Opened locality 52")
        );
    }

    #[test]
    #[ignore = "requires the local Chromium executable"]
    fn hosted_run_stops_after_committed_navigation_to_a_foreign_origin() {
        let fixture = OriginFixture::start();
        let start_url = fixture.start_url();
        let job = parse_job(json!({
            "schema_version":1,
            "target":{"kind":"browser","url":start_url},
            "context":{"journey":"origin guard","revision":"fixture","environment":"local Chromium","actor":"synthetic","authority":"navigate only"},
            "steps":[{"id":"leave","goal":"Open Leave origin.","done_when":[{"text_visible":"Never present"}]}]
        }));
        let mut browser = LiveBrowser::open(&start_url);
        let provider = RowActionProvider::new(vec![("Open Leave origin.", "Leave origin")]);
        let mut journal = MemoryJournal::default();

        let run = run_loop(
            &job,
            &mut browser,
            &provider,
            &ScriptedControl::default(),
            &mut journal,
        );

        assert_eq!((run.state(), run.code()), ("blocked", "origin_not_allowed"));
        assert!(run.outcome.result["escalation"].is_null());
        assert_eq!(provider.calls(), 1);
        assert_eq!(
            journal.entries.len(),
            2,
            "only one prepared mutation may run"
        );
        assert_eq!(journal.entries[0]["event"], "action_prepared");
        assert_eq!(journal.entries[1]["event"], "action_fact");
        assert_eq!(journal.entries[1]["fact"]["outcome"], "observed");
        let committed = browser.final_page.expect("page observed before cleanup");
        assert!(committed.url.starts_with(&fixture.foreign_origin()));
    }

    #[test]
    #[ignore = "requires the local Chromium executable"]
    fn hosted_run_hovers_row_actions_into_view_and_never_clicks_another_row() {
        let fixture = HoverRevealFixture::start();
        let url = fixture.url();
        let job = parse_job(json!({
            "schema_version":1,
            "target":{"kind":"browser","url":url},
            "context":{"journey":"hover-revealed row actions","revision":"fixture","environment":"local Chromium","actor":"synthetic","authority":"delete a synthetic category"},
            "steps":[
                {"id":"groceries-menu","goal":"Open the actions menu for the Groceries category.","done_when":[{"text_visible":"Move to group…"}]},
                {"id":"rent-menu","goal":"Open the actions menu for the Rent category.","done_when":"The actions menu for Rent is open."},
                {"id":"choose-delete","goal":"Choose Delete category… for Rent.","done_when":[{"dialog_open":"Delete category?"}]},
                {"id":"confirm","goal":"Confirm the deletion.","done_when":[{"dialog_closed":"Delete category?"},{"text_absent":"Rent"},{"text_visible":"Groceries"}]}
            ]
        }));
        let mut browser = LiveBrowser::open(&url);
        let provider = RowActionProvider::new(vec![
            (
                "Open the actions menu for the Groceries category.",
                "Actions for Groceries",
            ),
            (
                "Open the actions menu for the Rent category.",
                "Actions for Rent",
            ),
            ("Choose Delete category… for Rent.", "Delete category…"),
            ("Confirm the deletion.", "Delete"),
        ]);
        let mut journal = MemoryJournal::default();

        let run = run_loop(
            &job,
            &mut browser,
            &provider,
            &ScriptedControl::default(),
            &mut journal,
        );

        assert_eq!(run.state(), "passed", "{}", run.outcome.result["reason"]);
        assert_eq!(run.outcome.result["verdict"]["caller_assisted"], false);
        assert_eq!(
            prepared_actions(&journal),
            [
                "HOVER Groceries",
                "CLICK Actions for Groceries",
                "HOVER Rent",
                "CLICK Actions for Rent",
                "CLICK Delete category…",
                "CLICK Delete",
            ]
        );
        assert!(
            journal
                .entries
                .iter()
                .filter(|entry| entry["event"] == "action_fact")
                .all(|entry| entry["fact"]["outcome"] == "observed")
        );
        let mut step_files: Vec<_> = std::fs::read_dir(run.artifact("steps"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        step_files.sort();
        let per_step: Vec<_> = step_files
            .iter()
            .map(|path| {
                let step: Value =
                    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
                step["mutation_limit_consumed"].as_u64().unwrap()
            })
            .collect();
        assert_eq!(per_step, [1, 1, 1, 1]);

        let choices = provider.choices.lock().unwrap();
        let (_, _, before_rent_hover) = choices
            .iter()
            .find(|(operation, target, request)| {
                operation == "CLICK"
                    && request["state"]["current_step"]["goal"]
                        == "Open the actions menu for the Rent category."
                    && request["questions"]["click_target"]["criteria"][target]["container"]
                        == "Rent"
                    && request["questions"]["click_target"]["criteria"][target]["revealed_by_hover"]
                        == true
            })
            .expect("the Rent row was hovered");
        let visible: Vec<_> = before_rent_hover["state"]["page"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|element| element["name"].as_str().unwrap())
            .collect();
        assert!(visible.contains(&"Actions for Groceries"));
        assert!(!visible.contains(&"Actions for Rent"));
        assert!(
            choices[0].2["state"]["page"]["elements"]
                .as_array()
                .unwrap()
                .iter()
                .all(|element| element["name"] != "Actions for Groceries")
        );
        for action in journal.prepared().into_iter().skip(2) {
            let name = action
                .get("hover_target")
                .unwrap_or(&action["target"])
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert!(!name.contains("Groceries"), "{action}");
        }
        let remaining = browser.final_page.expect("page observed before cleanup");
        assert!(!remaining.visible_text.contains("Rent"));
        assert!(remaining.visible_text.contains("Groceries"));
    }

    #[test]
    fn hosted_loop_publishes_running_checkpoints_before_the_terminal_result() {
        let job = parse_job(json!({
            "schema_version":1,
            "target":{"kind":"browser","url":"http://example.test/ready"},
            "context":{"journey":"checkpoint","revision":"r","environment":"e","actor":"a","authority":"a"},
            "values":{},
            "steps":[
                {"id":"first","goal":"observe first","done_when":[{"url_contains":"/ready"}]},
                {"id":"second","goal":"observe second","done_when":[{"url_contains":"/ready"}]}
            ]
        }));
        let ready = Observation {
            url: "http://example.test/ready".into(),
            route: "/ready".into(),
            ..observed("ready")
        };
        let control = ScriptedControl::default();

        let run = run_loop(
            &job,
            &mut FakeBrowser::new([ready]),
            &NoProvider,
            &control,
            &mut MemoryJournal::default(),
        );

        assert_eq!(run.state(), "passed");
        let checkpoints = control.checkpoints();
        let states: Vec<_> = checkpoints
            .iter()
            .map(|checkpoint| checkpoint["state"].as_str().unwrap())
            .collect();
        assert_eq!(states, ["running", "running", "passed"]);
        assert_eq!(checkpoints[0]["verdict"]["steps"][0]["result"], "satisfied");
        assert_eq!(
            checkpoints[0]["verdict"]["steps"][1]["result"],
            "unresolved"
        );
        assert_eq!(checkpoints[0]["evidence"]["complete"], false);
        assert_eq!(checkpoints[2]["terminal"], true);
    }

    #[test]
    fn caller_execute_round_trip_publishes_assisted_terminal_evidence() {
        let job = force_stop(mutation_job());
        let mut browser = FakeBrowser::new([text_field(""), text_field(""), text_field("Wanted")])
            .dispatching([typed("Wanted")]);
        let control = ScriptedControl::answering([execute("c_1")]);

        let run = run_loop(
            &job,
            &mut browser,
            &ScriptedProvider::new([Turn::type_text()]),
            &control,
            &mut MemoryJournal::default(),
        );

        assert_eq!(run.state(), "passed");
        assert_eq!(run.outcome.result["verdict"]["caller_assisted"], true);
        assert!(run.artifact("dispositions/request_0001.json").is_file());
        let checkpoints = control.checkpoints();
        assert_eq!(checkpoints[0]["state"], "uncertain");
        assert_eq!(checkpoints[0]["terminal"], false);
        assert!(
            checkpoints
                .iter()
                .any(|checkpoint| checkpoint["verdict"]["caller_assisted"] == true)
        );
    }

    #[test]
    fn hosted_pause_termination_closes_the_browser_and_publishes_abort() {
        let job = force_stop(mutation_job());

        let run = run_loop(
            &job,
            &mut FakeBrowser::new([text_field("")]),
            &ScriptedProvider::new([Turn::type_text()]),
            &ScriptedControl::default(),
            &mut MemoryJournal::default(),
        );

        assert_eq!((run.state(), run.code()), ("aborted", "caller_aborted"));
        assert_eq!(run.outcome.result["cleanup"]["browser"], "closed");
    }

    #[test]
    fn abort_persists_the_disposition_and_reports_actions_already_sent() {
        let job = force_stop(click_job());
        let mut page = observed("Plan");
        page.elements.push(button(1, 7, "Actions for Groceries"));
        page.viewport.document_height = 2_000.0;
        let mut browser = FakeBrowser::new([page]).dispatching([performed(&["scroll_down"])]);

        let run = run_loop(
            &job,
            &mut browser,
            &ScriptedProvider::new([Turn::scroll_down(), Turn::click("1")]),
            &ScriptedControl::answering([abort()]),
            &mut MemoryJournal::default(),
        );

        assert_eq!((run.state(), run.code()), ("aborted", "caller_aborted"));
        assert_eq!(run.outcome.result["verdict"]["caller_assisted"], true);
        let trace = run.trace();
        assert!(trace.contains("action_prepared"));
        assert!(trace.contains("action_fact"));
        assert!(run.artifact("dispositions/request_0001.json").is_file());
    }

    /// One caller `execute` whose outcome must end the run: the loop publishes it and never
    /// drives the run again.
    struct TerminalExecute {
        job: Job,
        resume: Result<CapturedPage, BrowserError>,
        outcome: Option<Result<PerformFact, PerformError>>,
        journal: MemoryJournal,
        stop: (&'static str, &'static str),
        complete: bool,
        dispatched: usize,
        provider_calls: usize,
    }

    #[test]
    fn caller_execute_failures_end_the_run_instead_of_driving_it_again() {
        let mut exhausted = mutation_job();
        exhausted.options.max_actions = Some(0);
        let mut judged = natural(mutation_job(), "The Name field holds the account name");
        judged.options.max_model_calls = Some(2);
        let cases = [
            TerminalExecute {
                job: mutation_job(),
                resume: captured(text_field("")),
                outcome: Some(typed("Wrong")),
                journal: MemoryJournal::default(),
                stop: ("failed", "write_readback_mismatch"),
                complete: true,
                dispatched: 1,
                provider_calls: 2,
            },
            TerminalExecute {
                job: mutation_job(),
                resume: captured(text_field("")),
                outcome: None,
                journal: MemoryJournal::failing_at(0),
                stop: ("blocked", "evidence_unavailable"),
                complete: true,
                dispatched: 0,
                provider_calls: 2,
            },
            TerminalExecute {
                job: mutation_job(),
                resume: captured(text_field("")),
                outcome: Some(typed("Wanted")),
                journal: MemoryJournal::failing_at(1),
                stop: ("blocked", "evidence_incomplete_after_dispatch"),
                complete: false,
                dispatched: 1,
                provider_calls: 2,
            },
            TerminalExecute {
                job: mutation_job(),
                resume: Err(BrowserError::Control("target closed".into())),
                outcome: None,
                journal: MemoryJournal::default(),
                stop: ("blocked", "browser_control_failed"),
                complete: true,
                dispatched: 0,
                provider_calls: 2,
            },
            TerminalExecute {
                job: mutation_job(),
                resume: captured(foreign(text_field("Wanted"))),
                outcome: None,
                journal: MemoryJournal::default(),
                stop: ("blocked", "origin_not_allowed"),
                complete: true,
                dispatched: 0,
                provider_calls: 2,
            },
            TerminalExecute {
                job: exhausted,
                resume: captured(text_field("")),
                outcome: None,
                journal: MemoryJournal::default(),
                stop: ("blocked", "budget_exhausted"),
                complete: true,
                dispatched: 0,
                provider_calls: 2,
            },
            TerminalExecute {
                job: judged,
                resume: captured(text_field("")),
                outcome: None,
                journal: MemoryJournal::default(),
                stop: ("blocked", "budget_exhausted"),
                complete: true,
                dispatched: 0,
                provider_calls: 2,
            },
        ];
        for mut case in cases {
            let mut browser = FakeBrowser::capturing(
                [
                    captured(text_field("")),
                    captured(text_field("")),
                    case.resume,
                ],
                text_field("Wanted"),
            )
            .dispatching(case.outcome);
            let provider = ScriptedProvider::new([Turn::type_text().confidence(0.5)]);

            let run = run_loop(
                &case.job,
                &mut browser,
                &provider,
                &ScriptedControl::answering([execute("c_1"), retry()]),
                &mut case.journal,
            );

            let label = case.stop.1;
            assert_eq!((run.state(), run.code()), case.stop, "{label}");
            assert_eq!(run.complete(), case.complete, "{label}");
            assert!(run.outcome.result["escalation"].is_null(), "{label}");
            assert_eq!(
                run.outcome.result["verdict"]["steps"][0]["result"], "unresolved",
                "{label}"
            );
            assert_eq!(browser.dispatched(), case.dispatched, "{label}");
            assert_eq!(provider.calls(), case.provider_calls, "{label}");
        }
    }

    #[test]
    fn verification_disposition_failures_end_the_run_instead_of_verifying_again() {
        let ready = observed("Ready");
        let attested = observed("Ready balance 12.34");
        let changed = observed("Ready balance 12.34 pending");
        for (final_capture, stop) in [
            (captured(changed), ("failed", "expectation_not_met")),
            (
                Err(BrowserError::Control("target closed".into())),
                ("blocked", "browser_control_failed"),
            ),
        ] {
            let mut browser = FakeBrowser::capturing(
                [
                    captured(ready.clone()),
                    captured(attested.clone()),
                    final_capture,
                ],
                attested.clone(),
            );
            let provider = ScriptedProvider::new([
                Turn::verdict(0.50),
                Turn::verdict(0.05),
                Turn::verdict(0.95),
            ]);

            let run = run_loop(
                &expectation_job(),
                &mut browser,
                &provider,
                &ScriptedControl::answering([advance("The balance is visible."), retry()]),
                &mut MemoryJournal::default(),
            );

            assert_eq!((run.state(), run.code()), stop);
            assert!(run.outcome.result["escalation"].is_null());
            assert!(provider.calls() <= 2, "{}", stop.1);
        }
    }

    #[test]
    fn select_of_a_value_the_page_does_not_offer_pauses_without_dispatch() {
        let job = parse_job(json!({
            "schema_version":1,
            "target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
            "context":{"journey":"select","revision":"fixture","environment":"fake","actor":"synthetic","authority":"select"},
            "values":{"name":{"value":"Wanted","description":"account name"}},
            "steps":[{"id":"choose","goal":"choose the account","done_when":[{"field":"Account","equals_value":"name"}]}]
        }));
        let mut page = observed("");
        page.elements.push(Element {
            select_options: vec![manuvra_chrome::SelectOption {
                node_id: 9,
                label: "Other".into(),
                value: "other".into(),
                disabled: false,
                selected: true,
            }],
            ..element(1, 8, "combobox", "Account", "SELECT")
        });
        let mut browser = FakeBrowser::new([page]);
        let control = ScriptedControl::default();

        let run = run_loop(
            &job,
            &mut browser,
            &ScriptedProvider::new([Turn::select()]),
            &control,
            &mut MemoryJournal::default(),
        );

        assert_eq!((run.state(), run.code()), ("aborted", "caller_aborted"));
        let paused = &control.checkpoints()[0];
        assert_eq!(paused["state"], "uncertain");
        assert_eq!(paused["reason"]["code"], "select_option_unavailable");
        assert!(paused["escalation"].is_object());
        assert_eq!(browser.dispatched(), 0);
    }

    #[test]
    fn cancellation_without_a_termination_ends_the_run_blocked() {
        let control = ScriptedControl::default();
        control.cancellation.cancel();

        let run = run_loop(
            &job("Ready"),
            &mut FakeBrowser::new([observed("Ready")]),
            &NoProvider,
            &control,
            &mut MemoryJournal::default(),
        );

        assert_eq!((run.state(), run.code()), ("blocked", "run_cancelled"));
        assert!(run.outcome.result["escalation"].is_null());
    }

    #[test]
    fn autonomous_journal_failures_end_the_run_truthfully() {
        for (journal, code, complete, dispatched) in [
            (
                MemoryJournal::failing_at(0),
                "evidence_unavailable",
                true,
                0,
            ),
            (
                MemoryJournal::failing_at(1),
                "evidence_incomplete_after_dispatch",
                false,
                1,
            ),
        ] {
            let mut journal = journal;
            let mut browser = FakeBrowser::new([plan_before_hover(), plan_groceries_revealed()])
                .dispatching([performed(&["mouse_move"])]);

            let run = run_loop(
                &click_job(),
                &mut browser,
                &ScriptedProvider::new([Turn::hover("R1_1")]),
                &ScriptedControl::default(),
                &mut journal,
            );

            assert_eq!((run.state(), run.code()), ("blocked", code));
            assert_eq!(run.complete(), complete, "{code}");
            assert_eq!(browser.dispatched(), dispatched, "{code}");
            let manifest: Value = serde_json::from_str(
                &std::fs::read_to_string(run.artifact("manifest.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(manifest["complete"], complete, "{code}");
        }
    }

    #[test]
    fn resume_observation_on_a_foreign_origin_never_completes_or_reaches_the_provider() {
        let structured = click_job();
        let judged = natural(click_job(), "The actions menu for Groceries is open");
        for job in [structured, judged] {
            let mut browser = FakeBrowser::new([
                plan_before_hover(),
                plan_before_hover(),
                foreign(plan_menu_open()),
            ]);
            let provider = ScriptedProvider::new([low_hover_turn()]);

            let run = run_loop(
                &job,
                &mut browser,
                &provider,
                &ScriptedControl::answering([execute("c_1")]),
                &mut MemoryJournal::default(),
            );

            assert_eq!((run.state(), run.code()), ("blocked", "origin_not_allowed"));
            assert_eq!(provider.calls(), 2);
            assert_eq!(browser.dispatched(), 0);
            assert_ne!(
                run.outcome.result["verdict"]["steps"][0]["result"],
                "satisfied"
            );
        }
    }

    #[test]
    fn final_verification_on_a_foreign_origin_never_reaches_the_provider() {
        let provider = ScriptedProvider::new([Turn::verdict(0.95)]);

        let run = run_loop(
            &expectation_job(),
            &mut FakeBrowser::new([observed("Ready"), foreign(observed("Ready balance 12.34"))]),
            &provider,
            &ScriptedControl::default(),
            &mut MemoryJournal::default(),
        );

        assert_eq!((run.state(), run.code()), ("blocked", "origin_not_allowed"));
        assert_eq!(provider.calls(), 0);
    }

    #[test]
    fn hosted_missing_browser_uses_the_production_browser_error_publisher() {
        let job = job("Ready");
        let redactor = Redactor::for_job(&job).unwrap();
        let temp = TempDir::new().unwrap();
        let outcome = run_hosted(
            &job,
            FlowConfig {
                request_id: "hosted-unavailable".into(),
                run_id: "r_hosted_unavailable".into(),
                evidence_root: temp.path().to_path_buf(),
                browser: Some(temp.path().join("missing-browser")),
                headless: true,
            },
            &redactor,
            None,
            &ScriptedControl::default(),
        )
        .unwrap();
        assert_eq!(outcome.exit_code, 3);
        assert_eq!(outcome.result["reason"]["code"], "browser_unavailable");
        assert!(
            temp.path()
                .join("r_hosted_unavailable/manifest.json")
                .is_file()
        );
    }

    #[test]
    fn hosted_termination_has_closed_truthful_result_fields() {
        assert_eq!(
            hosted_termination_fields(HostedTermination::Aborted),
            (RunState::Aborted, "caller_aborted", 5)
        );
        assert_eq!(
            hosted_termination_fields(HostedTermination::PauseDeadlineElapsed),
            (RunState::Expired, "resume_deadline_elapsed", 5)
        );
        assert_eq!(
            hosted_termination_fields(HostedTermination::LifetimeElapsed),
            (RunState::Expired, "lifetime_elapsed", 5)
        );
        assert_eq!(
            hosted_termination_fields(HostedTermination::WatchdogLost),
            (RunState::Blocked, "watchdog_lost", 3)
        );
    }
    #[test]
    #[ignore = "requires the local Chromium executable"]
    fn hosted_reveal_clicks_bravo_while_alphas_twin_is_already_visible() {
        let fixture = HoverRevealFixture::with_body(include_str!(
            "../../../../tests/browser/selected-row-twin.html"
        ));
        let url = fixture.url();
        let job = parse_job(
            json!({"schema_version":1,"target":{"kind":"browser","url":url},"context":{"journey":"row twin","revision":"fixture","environment":"local Chromium","actor":"synthetic","authority":"edit Bravo"},"steps":[{"id":"edit","goal":"Edit the Bravo row.","done_when":[{"text_visible":"Editing Bravo"}]}]}),
        );
        let mut browser = LiveBrowser::open(&url);
        let provider = ScriptedProvider::new([Turn::hover("R1_1"), Turn::click("2")]);
        let mut journal = MemoryJournal::default();
        let run = run_loop(
            &job,
            &mut browser,
            &provider,
            &ScriptedControl::default(),
            &mut journal,
        );
        assert_eq!(run.state(), "passed", "{}", run.outcome.result);
        assert_eq!(
            journal
                .prepared()
                .iter()
                .map(|e| e["operation"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["HOVER", "CLICK"]
        );
        assert_eq!(journal.prepared()[0]["hover_target"]["name"], "Bravo");
        assert_eq!(journal.prepared()[1]["target"]["container"], "Bravo");
        assert!(
            browser
                .final_page
                .unwrap()
                .visible_text
                .contains("Editing Bravo")
        );
    }

    #[test]
    #[ignore = "requires the local Chromium executable"]
    fn hosted_ambiguous_project_link_stops_without_execute_or_browser_input() {
        let fixture = HoverRevealFixture::with_body(include_str!(
            "../../../../tests/browser/project-sidebar.html"
        ));
        let url = fixture.url();
        let job = parse_job(
            json!({"schema_version":1,"target":{"kind":"browser","url":url},"context":{"journey":"project options","revision":"fixture","environment":"local Chromium","actor":"synthetic","authority":"open Gemini options"},"steps":[{"id":"open","goal":"Open more options for Project Gemini.","done_when":[{"text_visible":"Options for Project Gemini"}]}]}),
        );
        let browser = LiveBrowser::open(&url);
        let redactor = Redactor::for_job(&job).unwrap();
        let mut machine = HostedMachine::new(&job, &redactor);
        let provider = ScriptedProvider::new([Turn::click("2").target_confidence(0.69)]);
        let mut journal = MemoryJournal::default();
        machine.drive(
            &browser,
            &provider,
            &mut journal,
            &manuvra_chrome::InputCancellation::default(),
            None,
            &ScriptedControl::default(),
        );
        assert_eq!(
            machine.artifacts.stop.as_ref().unwrap().code,
            "target_below_gate"
        );
        assert_eq!(provider.calls(), 2);
        assert!(journal.entries.is_empty());
        assert!(
            machine
                .artifacts
                .pending
                .as_ref()
                .unwrap()
                .candidate
                .is_none()
        );
        assert_eq!(
            machine.artifacts.escalation.as_ref().unwrap().dispositions,
            [
                manuvra_contract::DispositionKind::RetryObservation,
                manuvra_contract::DispositionKind::Abort
            ]
        );
    }
}
