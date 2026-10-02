//! Fakes and builders shared by the run's unit tests: a scripted browser, provider, journal, and
//! hosted control, recorded observation and job builders, and helpers that drive the hosted
//! machine the way the hosted loop does.

use crate::evidence::Redactor;
use crate::run::artifacts::RunArtifacts;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::run::browser::HostedBrowser;
use crate::run::capture::BrowserPage;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::run::hosted::{RunJournal, finish_hosted_browser_run};
use crate::run::machine::HostedMachine;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::run::{FlowConfig, FlowOutcome};
use crate::run::{HostedControl, HostedEvent, HostedTermination};
use crate::test_support::hover_region;
use crate::{actions, judgment, policy};
use manuvra_chrome::{
    BrowserError, CapturedPage, Coverage, Element, FocusAnchor, Observation, PerformError,
    PerformFact, PreparedInput, PreparedOperation, Rect, RedactionProof, Screenshot, ViewportState,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use manuvra_contract::Cleanup;
use manuvra_contract::{
    Disposition, DispositionKind, DispositionRequest, DoneCondition, Job, RunState, SchemaVersion,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Instant;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use tempfile::TempDir;

/// Serves scripted captures in order and then its fallback page, answers each dispatch with
/// the next scripted outcome or its repeated one, and records every prepared input. An
/// unscripted dispatch fails the test.
pub(crate) struct FakeBrowser {
    captures: Mutex<VecDeque<Result<CapturedPage, BrowserError>>>,
    fallback: Observation,
    outcomes: Mutex<VecDeque<Result<PerformFact, PerformError>>>,
    repeated_outcome: Option<Result<PerformFact, PerformError>>,
    pub(crate) inputs: Mutex<Vec<PreparedInput>>,
}

impl FakeBrowser {
    /// Serves the pages in order, then keeps serving the last one.
    pub(crate) fn new(pages: impl IntoIterator<Item = Observation>) -> Self {
        let pages: Vec<_> = pages.into_iter().collect();
        let fallback = pages.last().cloned().expect("scripted page");
        Self::capturing(pages.into_iter().map(captured), fallback)
    }

    pub(crate) fn capturing(
        captures: impl IntoIterator<Item = Result<CapturedPage, BrowserError>>,
        fallback: Observation,
    ) -> Self {
        Self {
            captures: Mutex::new(captures.into_iter().collect()),
            fallback,
            outcomes: Mutex::new(VecDeque::new()),
            repeated_outcome: None,
            inputs: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn dispatching(
        self,
        outcomes: impl IntoIterator<Item = Result<PerformFact, PerformError>>,
    ) -> Self {
        Self {
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            ..self
        }
    }

    pub(crate) fn always_dispatching(self, outcome: Result<PerformFact, PerformError>) -> Self {
        Self {
            repeated_outcome: Some(outcome),
            ..self
        }
    }

    pub(crate) fn operations(&self) -> Vec<PreparedOperation> {
        self.inputs
            .lock()
            .unwrap()
            .iter()
            .map(|input| input.operation)
            .collect()
    }

    pub(crate) fn dispatched(&self) -> usize {
        self.inputs.lock().unwrap().len()
    }
}

impl BrowserPage for FakeBrowser {
    fn capture_redacted_page(&self, sensitive: &[String]) -> Result<CapturedPage, BrowserError> {
        self.captures
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| captured(self.fallback.clone()))
            .map(|mut page| {
                page.redaction.sensitive_values_checked = sensitive.len();
                page
            })
    }
    fn observe_page(&self) -> Result<Observation, BrowserError> {
        Ok(self.fallback.clone())
    }
}

impl actions::Performer for FakeBrowser {
    fn dispatch(
        &self,
        input: PreparedInput,
        _cancellation: &manuvra_chrome::InputCancellation,
    ) -> Result<PerformFact, PerformError> {
        self.inputs.lock().unwrap().push(input);
        let scripted = self.outcomes.lock().unwrap().pop_front();
        scripted
            .or_else(|| self.repeated_outcome.clone())
            .expect("unscripted dispatch")
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl HostedBrowser for FakeBrowser {
    fn cleanup_hosted(&mut self) -> Cleanup {
        Cleanup {
            browser: "closed".into(),
            profile: "removed".into(),
            application_state: "caller_owned".into(),
        }
    }
}

pub(crate) fn captured(observation: Observation) -> Result<CapturedPage, BrowserError> {
    Ok(CapturedPage {
        observation,
        screenshot: Screenshot {
            bytes: b"fake-png".to_vec(),
            width: 1,
            height: 1,
        },
        redaction: RedactionProof {
            sensitive_values_checked: 0,
            matched_values: 0,
            mask_count: 0,
        },
    })
}

pub(crate) fn performed(suboperations: &[&str]) -> Result<PerformFact, PerformError> {
    Ok(PerformFact {
        scroll_readback: Vec::new(),
        readback: None,
        readback_matches: None,
        suboperations: suboperations.iter().map(|&name| name.to_owned()).collect(),
    })
}

pub(crate) fn typed(readback: &str) -> Result<PerformFact, PerformError> {
    Ok(PerformFact {
        scroll_readback: Vec::new(),
        readback: Some(readback.into()),
        readback_matches: None,
        suboperations: vec![],
    })
}

pub(crate) struct NoProvider;
impl manuvra_jev::Evaluator for NoProvider {
    fn evaluate(
        &self,
        _request: &Value,
        _deadline: Instant,
    ) -> Result<manuvra_jev::Evaluation, manuvra_jev::JevError> {
        Err(manuvra_jev::JevError::Unavailable)
    }
}

/// One scripted provider answer: the chosen operation and its targets, value and key, and the
/// Noul given to every Noul question.
#[derive(Clone)]
pub(crate) struct Turn {
    operation: &'static str,
    confidence: f64,
    click_target: &'static str,
    target_confidence: f64,
    type_target: &'static str,
    select_target: &'static str,
    type_value: &'static str,
    key: &'static str,
    key_confidence: f64,
    noul: f64,
}

impl Turn {
    pub(crate) fn new(operation: &'static str) -> Self {
        Self {
            operation,
            confidence: 0.95,
            click_target: "NO_CLICK_TARGET",
            target_confidence: 1.0,
            type_target: "NO_TYPE_TEXT_TARGET",
            select_target: "NO_SELECT_TARGET",
            type_value: "NONE_FITS",
            key: "Escape",
            key_confidence: 1.0,
            noul: 0.01,
        }
    }

    pub(crate) fn click(target: &'static str) -> Self {
        Self {
            click_target: target,
            ..Self::new("CLICK")
        }
    }

    pub(crate) fn scroll_down() -> Self {
        Self::new("SCROLL_DOWN")
    }

    pub(crate) fn hover(reveal: &'static str) -> Self {
        Self::click(reveal)
    }

    pub(crate) fn type_text() -> Self {
        Self {
            type_target: "1",
            type_value: "name",
            ..Self::new("TYPE_TEXT")
        }
    }

    pub(crate) fn select() -> Self {
        Self {
            select_target: "1",
            type_value: "name",
            ..Self::new("SELECT")
        }
    }

    pub(crate) fn key(key: &'static str) -> Self {
        Self {
            key,
            ..Self::new("PRESS_KEY")
        }
    }

    /// A final-verification answer: every expectation receives this Noul.
    pub(crate) fn verdict(noul: f64) -> Self {
        Self {
            noul,
            ..Self::new("WAIT")
        }
    }

    pub(crate) fn target_confidence(self, target_confidence: f64) -> Self {
        Self {
            target_confidence,
            ..self
        }
    }

    pub(crate) fn confidence(self, confidence: f64) -> Self {
        Self { confidence, ..self }
    }

    pub(crate) fn key_confidence(self, key_confidence: f64) -> Self {
        Self {
            key_confidence,
            ..self
        }
    }

    pub(crate) fn noul(self, noul: f64) -> Self {
        Self { noul, ..self }
    }

    fn answer(&self, id: &str, question: &Value) -> Option<manuvra_jev::Answer> {
        if question["type"] == "noul" {
            return Some(manuvra_jev::Answer::Noul { noul: self.noul });
        }
        let (selected, confidence) = match id {
            "operation" => (self.operation, self.confidence),
            "click_target" => (self.click_target, self.target_confidence),
            "type_target" => (self.type_target, 1.0),
            "select_target" => (self.select_target, 1.0),
            "type_value" => (self.type_value, 1.0),
            "key" => (self.key, self.key_confidence),
            other => panic!("unscripted question {other}"),
        };
        Some(manuvra_jev::Answer::Choice {
            choice: selected.into(),
            probabilities: BTreeMap::from([(selected.into(), confidence)]),
            confidence,
        })
    }
}

/// Answers each request with the next scripted turn, repeating the last one, and records
/// every request it receives.
pub(crate) struct ScriptedProvider {
    turns: Mutex<VecDeque<Turn>>,
    pub(crate) requests: Mutex<Vec<Value>>,
}

impl ScriptedProvider {
    pub(crate) fn new(turns: impl IntoIterator<Item = Turn>) -> Self {
        Self {
            turns: Mutex::new(turns.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl manuvra_jev::Evaluator for ScriptedProvider {
    fn evaluate(
        &self,
        request: &Value,
        _deadline: Instant,
    ) -> Result<manuvra_jev::Evaluation, manuvra_jev::JevError> {
        self.requests.lock().unwrap().push(request.clone());
        let turn = {
            let mut turns = self.turns.lock().unwrap();
            if turns.len() > 1 {
                turns.pop_front()
            } else {
                turns.front().cloned()
            }
            .expect("scripted turn")
        };
        let answers = request["questions"]
            .as_object()
            .expect("questions")
            .iter()
            .filter_map(|(id, question)| {
                turn.answer(id, question).map(|answer| (id.clone(), answer))
            })
            .collect();
        Ok(manuvra_jev::Evaluation {
            answers,
            usage: BTreeMap::new(),
            request_id: Some("scripted".into()),
            model: "jev-1.13.0".into(),
        })
    }
}

/// Records every journal entry; an injected failure refuses the entry at that position.
#[derive(Default)]
pub(crate) struct MemoryJournal {
    pub(crate) entries: Vec<Value>,
    fail_at: Option<usize>,
}

impl MemoryJournal {
    pub(crate) fn failing_at(position: usize) -> Self {
        Self {
            entries: Vec::new(),
            fail_at: Some(position),
        }
    }

    pub(crate) fn prepared(&self) -> Vec<&Value> {
        self.entries
            .iter()
            .filter(|entry| entry["event"] == "action_prepared")
            .collect()
    }
}

impl actions::ActionJournal for MemoryJournal {
    fn append(&mut self, value: &Value) -> Result<(), String> {
        if self.fail_at == Some(self.entries.len()) {
            return Err("injected journal failure".into());
        }
        self.entries.push(value.clone());
        Ok(())
    }
    fn entries(&self) -> &[Value] {
        &self.entries
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl RunJournal for MemoryJournal {
    fn clear(&mut self) -> Result<(), String> {
        Ok(())
    }
}

/// Answers each pause with the next scripted disposition and aborts once none remain. It
/// never reports a termination by itself.
#[derive(Default)]
pub(crate) struct ScriptedControl {
    checkpoints: Mutex<Vec<Value>>,
    dispositions: Mutex<VecDeque<Disposition>>,
    pub(crate) cancellation: manuvra_chrome::InputCancellation,
}

impl ScriptedControl {
    pub(crate) fn answering(dispositions: impl IntoIterator<Item = Disposition>) -> Self {
        Self {
            dispositions: Mutex::new(dispositions.into_iter().collect()),
            ..Self::default()
        }
    }

    pub(crate) fn checkpoints(&self) -> Vec<Value> {
        self.checkpoints.lock().unwrap().clone()
    }
}

impl HostedControl for ScriptedControl {
    fn cancellation(&self) -> manuvra_chrome::InputCancellation {
        self.cancellation.clone()
    }

    fn pause_deadline_unix_ms(&self) -> u64 {
        u64::MAX
    }

    fn publish_checkpoint(&self, result: &Value) -> Result<(), String> {
        self.checkpoints.lock().unwrap().push(result.clone());
        Ok(())
    }

    fn wait_while_paused(&self, escalation_id: &str) -> HostedEvent {
        self.dispositions.lock().unwrap().pop_front().map_or(
            HostedEvent::Termination(HostedTermination::Aborted),
            |disposition| HostedEvent::Disposition(request(escalation_id, disposition)),
        )
    }

    fn termination(&self) -> Option<HostedTermination> {
        None
    }
}

pub(crate) fn request(escalation_id: &str, disposition: Disposition) -> DispositionRequest {
    DispositionRequest {
        schema_version: SchemaVersion,
        escalation_id: escalation_id.into(),
        disposition,
    }
}

pub(crate) fn execute(candidate_id: &str) -> Disposition {
    Disposition::Execute(manuvra_contract::ExecuteDisposition {
        kind: manuvra_contract::ExecuteKind::Execute,
        candidate_id: candidate_id.into(),
    })
}

pub(crate) fn advance(rationale: &str) -> Disposition {
    Disposition::Advance(manuvra_contract::AdvanceDisposition {
        kind: manuvra_contract::AdvanceKind::Advance,
        rationale: rationale.into(),
    })
}

pub(crate) fn retry() -> Disposition {
    Disposition::RetryObservation(manuvra_contract::RetryObservationDisposition {
        kind: manuvra_contract::RetryObservationKind::RetryObservation,
    })
}

pub(crate) fn abort() -> Disposition {
    Disposition::Abort(manuvra_contract::AbortDisposition {
        kind: manuvra_contract::AbortKind::Abort,
    })
}

/// An uncertain stop always carries the escalation a disposition answers; the hosted loop
/// pauses only on one.
pub(crate) fn assert_pause_invariant(machine: &HostedMachine<'_>) {
    if let Some(stop) = &machine.artifacts.stop
        && stop.state == RunState::Uncertain
    {
        assert!(
            machine.artifacts.escalation.is_some(),
            "uncertain stop {} has no escalation",
            stop.code
        );
    }
}

/// Drives the machine as the hosted loop does, without publishing checkpoints.
pub(crate) fn drive(
    machine: &mut HostedMachine<'_>,
    browser: &FakeBrowser,
    provider: &impl manuvra_jev::Evaluator,
    journal: &mut MemoryJournal,
) {
    machine.drive(
        browser,
        provider,
        journal,
        &manuvra_chrome::InputCancellation::default(),
        None,
        &ScriptedControl::default(),
    );
    assert_pause_invariant(machine);
}

/// Answers the machine's current escalation with a disposition.
pub(crate) fn dispose(
    machine: &mut HostedMachine<'_>,
    disposition: Disposition,
    browser: &FakeBrowser,
    provider: &impl manuvra_jev::Evaluator,
    journal: &mut MemoryJournal,
) -> Option<HostedTermination> {
    let escalation_id = machine
        .artifacts
        .escalation
        .as_ref()
        .map_or_else(|| "e_stale".to_owned(), |escalation| escalation.id.clone());
    let termination = machine.apply(
        request(&escalation_id, disposition),
        browser,
        provider,
        journal,
        &manuvra_chrome::InputCancellation::default(),
    );
    assert_pause_invariant(machine);
    termination
}

/// The artifacts of a run driven autonomously until it completes or first stops.
pub(crate) fn driven(
    job: &Job,
    browser: &FakeBrowser,
    provider: &impl manuvra_jev::Evaluator,
    journal: &mut MemoryJournal,
) -> RunArtifacts {
    let redactor = Redactor::for_job(job).unwrap();
    let mut machine = HostedMachine::new(job, &redactor);
    drive(&mut machine, browser, provider, journal);
    machine.artifacts
}

/// A run of the hosted loop against fakes, with its published evidence.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) struct LoopRun {
    evidence: TempDir,
    pub(crate) outcome: FlowOutcome,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl LoopRun {
    pub(crate) fn state(&self) -> &str {
        self.outcome.result["state"].as_str().unwrap()
    }

    pub(crate) fn code(&self) -> &str {
        self.outcome.result["reason"]["code"]
            .as_str()
            .unwrap_or_default()
    }

    pub(crate) fn complete(&self) -> bool {
        self.outcome.result["evidence"]["complete"]
            .as_bool()
            .unwrap()
    }

    pub(crate) fn artifact(&self, relative: &str) -> std::path::PathBuf {
        self.evidence.path().join("r_loop").join(relative)
    }

    pub(crate) fn trace(&self) -> String {
        std::fs::read_to_string(self.artifact("trace.jsonl")).unwrap()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn run_loop(
    job: &Job,
    browser: &mut impl HostedBrowser,
    provider: &impl manuvra_jev::Evaluator,
    control: &ScriptedControl,
    journal: &mut MemoryJournal,
) -> LoopRun {
    let evidence = TempDir::new().unwrap();
    let redactor = Redactor::for_job(job).unwrap();
    let config = FlowConfig {
        request_id: "hosted-loop".into(),
        run_id: "r_loop".into(),
        evidence_root: evidence.path().to_path_buf(),
        browser: None,
        headless: true,
    };
    let outcome = finish_hosted_browser_run(
        job,
        config,
        &redactor,
        browser,
        json!({"fixture":"hosted-loop"}),
        provider,
        journal,
        &control.cancellation,
        control,
    )
    .unwrap();
    LoopRun { evidence, outcome }
}

pub(crate) fn observed(text: &str) -> Observation {
    Observation {
        document_id: "recorded-money-document".into(),
        url: "http://127.0.0.1:4351/".into(),
        route: "/".into(),
        title: "Money · Accounts".into(),
        dialogs: Vec::new(),
        focused: None,
        focus_anchor: None,
        visible_text: text.into(),
        covered_text: String::new(),
        dialog_texts: BTreeMap::new(),
        elements: Vec::new(),
        viewport: ViewportState {
            width: 1120,
            height: 780,
            scroll_x: 0.,
            scroll_y: 0.,
            document_height: 780.,
        },
        coverage: Coverage::default(),
        hover_regions: Vec::new(),
        scroll_regions: Vec::new(),
        overlay: None,
        scroll_regions_truncated: false,
        hover_regions_truncated: false,
        hover_rules_unreadable: false,
    }
}

pub(crate) fn foreign(mut observation: Observation) -> Observation {
    observation.url = "http://foreign.test/".into();
    observation
}

pub(crate) fn element(
    index: u64,
    node_id: u64,
    role: &str,
    name: &str,
    operation: &str,
) -> Element {
    Element {
        index,
        node_id,
        context: "main".into(),
        role: role.into(),
        name: name.into(),
        input_type: None,
        value: String::new(),
        checked: None,
        selected: None,
        expanded: None,
        disabled: false,
        in_dialog: None,
        container: None,
        shares_name: false,
        operations: vec![operation.into()],
        select_options: vec![],
        rect: Rect {
            x: 10.0,
            y: 10.0,
            width: 20.0,
            height: 10.0,
        },
    }
}

pub(crate) fn button(index: u64, node_id: u64, name: &str) -> Element {
    Element {
        expanded: Some(false),
        ..element(index, node_id, "button", name, "CLICK")
    }
}

pub(crate) fn text_field(value: &str) -> Observation {
    let mut observation = observed("");
    observation.elements.push(Element {
        input_type: Some("text".into()),
        value: value.into(),
        rect: Rect {
            x: 1.0,
            y: 1.0,
            width: 20.0,
            height: 10.0,
        },
        ..element(1, 7, "textbox", "Name", "TYPE_TEXT")
    });
    observation
}

pub(crate) fn key_observation(name: &str) -> Observation {
    let mut observation = observed("");
    observation.focus_anchor = Some(FocusAnchor {
        node_id: match name {
            "First" => 1,
            "Second" => 2,
            _ => 3,
        },
        context: "main".into(),
        role: "button".into(),
        name: name.into(),
        in_dialog: None,
        container: None,
        covered: true,
        surface: None,
        active_descendant: None,
        expanded: None,
        selected: None,
        checked: None,
        position: None,
    });
    observation
}

pub(crate) fn save_button_focus() -> Observation {
    let mut observation = key_observation("Save");
    observation.elements.push(button(1, 3, "Save"));
    observation
}

pub(crate) fn anchor_state(
    role: &str,
    expanded: Option<bool>,
    checked: Option<bool>,
) -> Observation {
    let mut observation = key_observation("Save");
    let anchor = observation.focus_anchor.as_mut().unwrap();
    anchor.role = role.into();
    anchor.expanded = expanded;
    anchor.checked = checked;
    observation
}

/// The Groceries and Rent rows before any hover: their action buttons are hidden.
pub(crate) fn plan_before_hover() -> Observation {
    let mut page = observed("Plan Groceries Rent");
    page.elements
        .push(button(1, 11, "Edit assigned amount for Groceries, $400.00"));
    page.hover_regions = vec![
        hover_region(1, "Groceries", 41),
        hover_region(2, "Rent", 42),
    ];
    page
}

/// After hovering Groceries: its action button is a candidate and its region is gone.
pub(crate) fn plan_groceries_revealed() -> Observation {
    let mut page = plan_before_hover();
    page.elements.push(button(2, 41, "Actions for Groceries"));
    page.hover_regions = vec![hover_region(1, "Rent", 42)];
    page
}

pub(crate) fn plan_menu_open() -> Observation {
    let mut page = plan_groceries_revealed();
    page.visible_text = "Plan Groceries Rent Rename Move to group… Delete category…".into();
    page
}

pub(crate) fn parse_job(value: Value) -> Job {
    Job::parse(serde_json::to_vec(&value).unwrap().as_slice()).unwrap()
}

pub(crate) fn job(wanted: &str) -> Job {
    parse_job(json!({
        "schema_version":1,
        "target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
        "context":{"journey":"observe","revision":"fixture","environment":"fake","actor":"synthetic","authority":"observe"},
        "steps":[{"id":"ready","goal":"observe","done_when":[{"text_visible":wanted}]}]
    }))
}

pub(crate) fn mutation_job() -> Job {
    parse_job(json!({
        "schema_version":1,
        "target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
        "context":{"journey":"mutate","revision":"fixture","environment":"fake","actor":"synthetic","authority":"mutate"},
        "values":{"name":{"value":"Wanted","description":"account name"}},
        "steps":[{"id":"fill","goal":"fill the account name","done_when":[{"field":"Name","equals_value":"name"}],"mutation_limit":2}]
    }))
}

pub(crate) fn force_stop(mut job: Job) -> Job {
    job.options.debug = Some(manuvra_contract::DebugOptions {
        force_stop_at_step: job.steps[0].id.clone(),
    });
    job
}

pub(crate) fn natural(mut job: Job, condition: &str) -> Job {
    job.steps[0].done_when = DoneCondition::NaturalLanguage(condition.into());
    job
}

pub(crate) fn expectation_job() -> Job {
    parse_job(json!({
        "schema_version":1,
        "target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
        "context":{"journey":"verify","revision":"fixture","environment":"fake","actor":"synthetic","authority":"observe"},
        "steps":[{"id":"ready","goal":"observe","done_when":[{"text_visible":"Ready"}]}],
        "expectations":[{"id":"balance","claim":"The final balance is 12.34."}]
    }))
}

pub(crate) fn click_job() -> Job {
    parse_job(json!({
        "schema_version":1,
        "target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
        "context":{"journey":"open","revision":"fixture","environment":"fake","actor":"synthetic","authority":"click"},
        "steps":[{"id":"open","goal":"Open the actions menu for Groceries.","done_when":[{"text_visible":"Move to group"}]}]
    }))
}

pub(crate) fn key_activation_job() -> Job {
    let mut job = job("Activations: 1");
    job.steps[0].goal = "Press Enter to activate Save".into();
    job.steps[0].mutation_limit = 2;
    job
}

pub(crate) fn key_job(id: &str, goal: &str, done_when: Value, mutation_limit: u8) -> Job {
    parse_job(json!({
        "schema_version":1,"target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
        "context":{"journey":"key","revision":"fixture","environment":"fake","actor":"synthetic","authority":"keys"},
        "steps":[{"id":id,"goal":goal,"done_when":done_when,"mutation_limit":mutation_limit}]
    }))
}

pub(crate) fn low_hover_turn() -> Turn {
    Turn::hover("R1_1").confidence(0.59)
}

/// A run paused on a below-gate `HOVER` escalation offering the Groceries region.
pub(crate) fn hover_escalation<'a>(job: &'a Job, redactor: &'a Redactor) -> HostedMachine<'a> {
    let browser = FakeBrowser::new([plan_before_hover()]);
    let provider = ScriptedProvider::new([low_hover_turn()]);
    let mut machine = HostedMachine::new(job, redactor);
    drive(
        &mut machine,
        &browser,
        &provider,
        &mut MemoryJournal::default(),
    );
    assert_eq!(
        machine.artifacts.stop.as_ref().unwrap().code,
        "operation_below_gate"
    );
    machine
}

pub(crate) fn offered(machine: &HostedMachine<'_>) -> policy::Candidate {
    machine
        .artifacts
        .pending
        .as_ref()
        .and_then(|pending| pending.candidate.clone())
        .expect("offered candidate")
}

pub(crate) fn offered_hover(machine: &HostedMachine<'_>) -> policy::Candidate {
    let candidate = offered(machine);
    assert_eq!(candidate.operation, judgment::Operation::Hover);
    candidate
}

/// The code and state of the run's stop, and whether the reissued escalation still offers a
/// candidate.
pub(crate) fn paused(machine: &HostedMachine<'_>) -> (&'static str, RunState, bool) {
    let stop = machine.artifacts.stop.as_ref().unwrap();
    let offered = machine
        .artifacts
        .pending
        .as_ref()
        .is_some_and(|pending| pending.candidate.is_some());
    (stop.code, stop.state, offered)
}

pub(crate) fn offers_execute(machine: &HostedMachine<'_>) -> bool {
    machine
        .artifacts
        .escalation
        .as_ref()
        .unwrap()
        .dispositions
        .contains(&DispositionKind::Execute)
}

pub(crate) fn action_operations(trace: &[Value], event: &str) -> Vec<String> {
    trace
        .iter()
        .filter(|entry| entry["event"] == event)
        .map(|entry| {
            entry
                .get("operation")
                .or_else(|| entry["fact"].get("operation"))
                .and_then(Value::as_str)
                .unwrap()
                .to_owned()
        })
        .collect()
}

pub(crate) fn trace_events(artifacts: &RunArtifacts) -> Vec<String> {
    artifacts
        .trace
        .iter()
        .map(|entry| {
            let event = entry["event"].as_str().unwrap();
            let operation = entry
                .get("operation")
                .or_else(|| entry.get("fact").and_then(|fact| fact.get("operation")));
            operation.map_or_else(
                || event.to_owned(),
                |operation| format!("{event}:{}", operation.as_str().unwrap()),
            )
        })
        .collect()
}

pub(crate) fn mutation_judgments(operation: &str, confidence: f64) -> judgment::Judgments {
    let choice = |selected: &str| judgment::ChoiceJudgment {
        choice: selected.into(),
        probabilities: BTreeMap::from([(selected.into(), 1.0)]),
        confidence,
    };
    judgment::Judgments {
        operation: choice(operation),
        click_target: choice("1"),
        type_target: choice("1"),
        select_target: choice("1"),
        type_value: choice("name"),
        key: choice("Escape"),
        step_done: 0.0,
        usage: BTreeMap::new(),
        request_id: None,
        model: "jev-test".into(),
        request: Value::Null,
    }
}
