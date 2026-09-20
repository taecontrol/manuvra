use crate::judgment::{Judgments, Operation, selected_operation};
use crate::verification::DoneResult;
use manuvra_chrome::{Element, Observation};
use manuvra_contract::{DoneCondition, JobOptions, Step};
#[cfg(any(target_os = "linux", test))]
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use url::Url;

const GATE: f64 = 0.70;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) id: String,
    pub(crate) operation: Operation,
    pub(crate) target_index: Option<u64>,
    pub(crate) target_name: Option<String>,
    target_identity: TargetIdentity,
    pub(crate) target_role: Option<String>,
    pub(crate) target_dialog: Option<String>,
    pub(crate) target_input_type: Option<String>,
    pub(crate) value_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TargetIdentity {
    node_id: Option<u64>,
    document_id: String,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Serialize)]
struct OfferedCandidate<'a> {
    id: &'a str,
    operation: Operation,
    target_name: Option<&'a str>,
    target_role: Option<&'a str>,
    target_dialog: Option<&'a str>,
    target_input_type: Option<&'a str>,
    value_name: Option<&'a str>,
}

impl Candidate {
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn offered(&self) -> serde_json::Value {
        serde_json::to_value(OfferedCandidate {
            id: &self.id,
            operation: self.operation,
            target_name: self.target_name.as_deref(),
            target_role: self.target_role.as_deref(),
            target_dialog: self.target_dialog.as_deref(),
            target_input_type: self.target_input_type.as_deref(),
            value_name: self.value_name.as_deref(),
        })
        .expect("offered candidate is serializable")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyStop {
    Uncertain(&'static str),
    Blocked(&'static str),
}

#[derive(Debug)]
pub enum Next {
    Complete,
    ReobserveDone,
    ReobserveOperation,
    Wait,
    Mutate(Box<Permit>),
    Stop(PolicyStop),
}

#[derive(Debug)]
pub struct Permit {
    candidate: Candidate,
    document_id: String,
    replay_key: String,
    action_sequence: u64,
}

impl Permit {
    pub(crate) fn consume(self) -> (Candidate, String, String, u64) {
        (
            self.candidate,
            self.document_id,
            self.replay_key,
            self.action_sequence,
        )
    }
}

pub struct Policy {
    started: Instant,
    max_actions: u16,
    max_model_calls: u16,
    active_timeout: Duration,
    actions: u16,
    model_calls: u16,
    step_mutations: u8,
    replay: HashSet<String>,
    allowed_origins: Vec<String>,
    paused_at: Option<Instant>,
    paused_total: Duration,
}

impl Policy {
    pub fn new(options: &JobOptions, target_url: &str) -> Self {
        let allowed_origins = options
            .allowed_origins
            .clone()
            .unwrap_or_else(|| vec![target_url.to_owned()])
            .iter()
            .filter_map(|value| origin(value))
            .collect();
        Self {
            started: Instant::now(),
            max_actions: options.max_actions.unwrap_or(80),
            max_model_calls: options.max_model_calls.unwrap_or(120),
            active_timeout: Duration::from_millis(u64::from(
                options.active_timeout_ms.unwrap_or(120_000),
            )),
            actions: 0,
            model_calls: 0,
            step_mutations: 0,
            replay: HashSet::new(),
            allowed_origins,
            paused_at: None,
            paused_total: Duration::ZERO,
        }
    }

    pub fn begin_step(&mut self) {
        self.step_mutations = 0;
    }

    pub fn check_active(&self) -> Result<(), PolicyStop> {
        (self.active_elapsed() < self.active_timeout)
            .then_some(())
            .ok_or(PolicyStop::Blocked("budget_exhausted"))
    }

    pub fn record_model_call(&mut self) -> Result<Instant, PolicyStop> {
        self.check_active()?;
        if self.model_calls >= self.max_model_calls {
            return Err(PolicyStop::Blocked("budget_exhausted"));
        }
        self.model_calls += 1;
        Ok(Instant::now() + self.active_timeout.saturating_sub(self.active_elapsed()))
    }

    pub fn decide(
        &mut self,
        step: &Step,
        observation: &Observation,
        judgments: &Judgments,
        done: DoneResult,
        done_reobserved: bool,
        operation_reobserved: bool,
    ) -> Next {
        if let Some(next) = done_first(step, done, done_reobserved) {
            return next;
        }
        if let Some(capability) = unsupported_capability(observation, judgments) {
            return Next::Stop(PolicyStop::Blocked(capability));
        }
        let Ok(operation) = selected_operation(judgments) else {
            return Next::Stop(PolicyStop::Blocked("provider_invalid_response"));
        };
        if let Some(next) = operation_gate(judgments, operation_reobserved) {
            return next;
        }
        match operation {
            Operation::Wait => Next::Wait,
            Operation::Blocked => Next::Stop(PolicyStop::Blocked("operation_blocked")),
            Operation::Click | Operation::TypeText => {
                self.authorize(step, observation, judgments, operation)
            }
        }
    }

    fn authorize(
        &mut self,
        step: &Step,
        observation: &Observation,
        judgments: &Judgments,
        operation: Operation,
    ) -> Next {
        if let Err(stop) = self.authorize_context(step, observation) {
            return Next::Stop(stop);
        }
        let candidate = match self.candidate(observation, judgments, operation) {
            Ok(candidate) => candidate,
            Err(stop) => return Next::Stop(stop),
        };
        self.mint(observation, candidate)
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn caller_candidate(
        &self,
        observation: &Observation,
        judgments: &Judgments,
    ) -> Result<Candidate, PolicyStop> {
        let operation = selected_operation(judgments)
            .map_err(|_| PolicyStop::Blocked("provider_invalid_response"))?;
        match operation {
            Operation::Click | Operation::TypeText => {
                self.candidate(observation, judgments, operation)
            }
            _ => Err(PolicyStop::Blocked("provider_invalid_response")),
        }
    }

    fn candidate(
        &self,
        observation: &Observation,
        judgments: &Judgments,
        operation: Operation,
    ) -> Result<Candidate, PolicyStop> {
        let target = selected_target(observation, judgments, operation)?;
        let value_name = selected_value(judgments, operation)?;
        Ok(Candidate {
            id: format!("c_{}", self.actions + 1),
            operation,
            target_index: Some(target.index),
            target_name: Some(target.name.clone()),
            target_identity: TargetIdentity {
                node_id: Some(target.node_id),
                document_id: observation.document_id.clone(),
            },
            target_role: Some(target.role.clone()),
            target_dialog: target.in_dialog.clone(),
            target_input_type: target.input_type.clone(),
            value_name,
        })
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn authorize_caller(
        &mut self,
        step: &Step,
        observation: &Observation,
        candidate: &Candidate,
    ) -> Result<Permit, PolicyStop> {
        self.authorize_context(step, observation)?;
        let target = candidate
            .target_index
            .and_then(|index| observation.elements.iter().find(|item| item.index == index))
            .filter(|target| candidate_matches(observation, target, candidate))
            .ok_or(PolicyStop::Uncertain("candidate_revalidation_failed"))?;
        let mut current = candidate.clone();
        current.target_name = Some(target.name.clone());
        match self.mint(observation, current) {
            Next::Mutate(permit) => Ok(*permit),
            Next::Stop(stop) => Err(stop),
            _ => unreachable!("caller authorization only mints or stops"),
        }
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn release_unused(&mut self, permit: Box<Permit>) -> Candidate {
        let (candidate, _, replay_key, action_sequence) = permit.consume();
        debug_assert_eq!(action_sequence, u64::from(self.actions));
        let removed = self.replay.remove(&replay_key);
        debug_assert!(removed);
        self.actions = self.actions.saturating_sub(1);
        self.step_mutations = self.step_mutations.saturating_sub(1);
        candidate
    }

    fn authorize_context(&self, step: &Step, observation: &Observation) -> Result<(), PolicyStop> {
        if self.actions >= self.max_actions
            || self.step_mutations >= step.mutation_limit
            || self.active_elapsed() >= self.active_timeout
        {
            return Err(PolicyStop::Blocked("budget_exhausted"));
        }
        let allowed = origin(&observation.url)
            .is_some_and(|current| self.allowed_origins.iter().any(|origin| origin == &current));
        allowed
            .then_some(())
            .ok_or(PolicyStop::Blocked("origin_not_allowed"))
    }

    fn mint(&mut self, observation: &Observation, candidate: Candidate) -> Next {
        let Some(target) = candidate
            .target_index
            .and_then(|index| observation.elements.iter().find(|item| item.index == index))
        else {
            return Next::Stop(PolicyStop::Uncertain("candidate_revalidation_failed"));
        };
        let replay_key = replay_key(observation, target, &candidate);
        if !self.replay.insert(replay_key.clone()) {
            return Next::Stop(PolicyStop::Uncertain("replay_forbidden"));
        }
        self.actions += 1;
        self.step_mutations += 1;
        Next::Mutate(Box::new(Permit {
            candidate,
            document_id: observation.document_id.clone(),
            replay_key,
            action_sequence: u64::from(self.actions),
        }))
    }

    pub fn active_ms(&self) -> u128 {
        self.active_elapsed().as_millis()
    }

    pub fn step_mutations(&self) -> u8 {
        self.step_mutations
    }

    pub fn pause(&mut self) {
        if self.paused_at.is_none() {
            self.paused_at = Some(Instant::now());
        }
    }

    pub fn resume(&mut self) {
        if let Some(paused_at) = self.paused_at.take() {
            self.paused_total = self.paused_total.saturating_add(paused_at.elapsed());
        }
    }

    fn active_elapsed(&self) -> Duration {
        let current_pause = self.paused_at.map_or(Duration::ZERO, |at| at.elapsed());
        self.started
            .elapsed()
            .saturating_sub(self.paused_total.saturating_add(current_pause))
    }
}

#[cfg(any(target_os = "linux", test))]
fn candidate_matches(observation: &Observation, target: &Element, candidate: &Candidate) -> bool {
    candidate_identity_matches(observation, target, candidate)
        && candidate_semantics_match(target, candidate)
        && candidate_operation_supported(target, candidate.operation)
}

#[cfg(any(target_os = "linux", test))]
fn candidate_identity_matches(
    observation: &Observation,
    target: &Element,
    candidate: &Candidate,
) -> bool {
    observation.document_id == candidate.target_identity.document_id
        && Some(target.node_id) == candidate.target_identity.node_id
}

#[cfg(any(target_os = "linux", test))]
fn candidate_semantics_match(target: &Element, candidate: &Candidate) -> bool {
    Some(target.name.as_str()) == candidate.target_name.as_deref()
        && Some(target.role.as_str()) == candidate.target_role.as_deref()
        && target.in_dialog == candidate.target_dialog
        && target.input_type == candidate.target_input_type
}

#[cfg(any(target_os = "linux", test))]
fn candidate_operation_supported(target: &Element, operation: Operation) -> bool {
    let expected = match operation {
        Operation::Click => "CLICK",
        Operation::TypeText => "TYPE_TEXT",
        _ => return false,
    };
    target.operations.iter().any(|item| item == expected)
}

fn operation_gate(judgments: &Judgments, already_reobserved: bool) -> Option<Next> {
    (judgments.operation.confidence < GATE).then_some({
        if already_reobserved {
            Next::Stop(PolicyStop::Uncertain("operation_below_gate"))
        } else {
            Next::ReobserveOperation
        }
    })
}

pub fn done_first(step: &Step, done: DoneResult, already_reobserved: bool) -> Option<Next> {
    match done {
        DoneResult::Satisfied => Some(Next::Complete),
        DoneResult::Unknown if already_reobserved => {
            let reason = match &step.done_when {
                DoneCondition::NaturalLanguage(_) => "done_uncertain",
                DoneCondition::Structured(_) => "done_unknown",
            };
            Some(Next::Stop(PolicyStop::Uncertain(reason)))
        }
        DoneResult::Unknown => Some(Next::ReobserveDone),
        DoneResult::NotSatisfied => None,
    }
}

fn selected_target<'a>(
    observation: &'a Observation,
    judgments: &Judgments,
    operation: Operation,
) -> Result<&'a Element, PolicyStop> {
    let answer = if operation == Operation::Click {
        &judgments.click_target
    } else {
        &judgments.type_target
    };
    parse_target(observation, &answer.choice)
        .ok_or(PolicyStop::Blocked("provider_invalid_response"))
}

fn selected_value(
    judgments: &Judgments,
    operation: Operation,
) -> Result<Option<String>, PolicyStop> {
    if operation != Operation::TypeText {
        return Ok(None);
    }
    if judgments.type_value.choice == "NONE_FITS" {
        return Err(PolicyStop::Blocked("value_not_provided"));
    }
    Ok(Some(judgments.type_value.choice.clone()))
}

fn origin(url: &str) -> Option<String> {
    let url = Url::parse(url).ok()?;
    let origin = url.origin().ascii_serialization();
    (origin != "null").then_some(origin)
}

fn parse_target<'a>(observation: &'a Observation, selected: &str) -> Option<&'a Element> {
    let index = selected.parse::<u64>().ok()?;
    observation
        .elements
        .iter()
        .find(|element| element.index == index)
}

fn replay_key(observation: &Observation, target: &Element, candidate: &Candidate) -> String {
    let stable = serde_json::json!({
        "operation":candidate.operation,"target_role":target.role,
        "target_name":target.name.to_ascii_lowercase(),"dialog":target.in_dialog,
        "input_type":target.input_type,"value_name":candidate.value_name,
        "route":observation.route,
    });
    hex::encode(Sha256::digest(stable.to_string().as_bytes()))
}

fn unsupported_capability(
    observation: &Observation,
    judgments: &Judgments,
) -> Option<&'static str> {
    let selected_target = match selected_operation(judgments).ok() {
        Some(Operation::Click) => parse_target(observation, &judgments.click_target.choice),
        Some(Operation::TypeText) => parse_target(observation, &judgments.type_target.choice),
        _ => None,
    };
    [
        needs_native_select(observation, selected_target).then_some("native_select"),
        needs_autocomplete(selected_target).then_some("autocomplete_requires_suggestions"),
        needs_scroll(observation, judgments).then_some("scroll_required"),
        needs_unsupported_surface(observation, selected_target).then_some("unsupported_surface"),
    ]
    .into_iter()
    .flatten()
    .next()
}

fn needs_unsupported_surface(observation: &Observation, selected: Option<&Element>) -> bool {
    !observation.coverage.gaps.is_empty() && (observation.elements.is_empty() || selected.is_none())
}

fn needs_native_select(observation: &Observation, selected: Option<&Element>) -> bool {
    selected.is_some_and(|element| element.operations.iter().any(|op| op == "SELECT"))
        || (observation
            .elements
            .iter()
            .any(|element| element.operations == ["SELECT"])
            && !observation.elements.iter().any(supports_direct_input))
}

fn supports_direct_input(element: &Element) -> bool {
    element
        .operations
        .iter()
        .any(|operation| operation == "CLICK" || operation == "TYPE_TEXT")
}

fn needs_autocomplete(selected: Option<&Element>) -> bool {
    selected.is_some_and(|element| {
        element.role == "combobox"
            && element.operations.iter().any(|op| op == "TYPE_TEXT")
            && element.expanded != Some(true)
    })
}

fn needs_scroll(observation: &Observation, judgments: &Judgments) -> bool {
    observation.elements.is_empty()
        && observation.viewport.document_height > f64::from(observation.viewport.height)
        && judgments.operation.choice != "WAIT"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judgment::ChoiceJudgment;
    use manuvra_chrome::{Coverage, Rect, ViewportState};
    use manuvra_contract::{DoneCondition, JobOptions};
    use serde_json::Value;
    use std::collections::BTreeMap;

    fn observation(operation: &str, role: &str) -> Observation {
        Observation {
            document_id: "d".into(),
            url: "http://example.test/".into(),
            route: "/".into(),
            title: "x".into(),
            dialogs: vec![],
            focused: None,
            visible_text: "".into(),
            covered_text: "".into(),
            dialog_texts: BTreeMap::new(),
            elements: vec![Element {
                index: 1,
                node_id: 9,
                context: "main".into(),
                role: role.into(),
                name: "Create".into(),
                input_type: None,
                value: "".into(),
                checked: None,
                selected: None,
                expanded: Some(false),
                disabled: false,
                in_dialog: None,
                operations: vec![operation.into()],
                rect: Rect {
                    x: 1.,
                    y: 1.,
                    width: 1.,
                    height: 1.,
                },
            }],
            viewport: ViewportState {
                width: 100,
                height: 100,
                scroll_x: 0.,
                scroll_y: 0.,
                document_height: 100.,
            },
            coverage: Coverage::default(),
        }
    }
    fn choice(value: &str) -> ChoiceJudgment {
        ChoiceJudgment {
            choice: value.into(),
            probabilities: BTreeMap::from([(value.into(), 1.0)]),
            confidence: 1.0,
        }
    }
    fn judgments(operation: &str) -> Judgments {
        Judgments {
            operation: choice(operation),
            click_target: choice("1"),
            type_target: choice("1"),
            type_value: choice("name"),
            step_done: 0.5,
            usage: BTreeMap::new(),
            request_id: None,
            model: "jev".into(),
            request: Value::Null,
        }
    }
    fn step() -> Step {
        Step {
            id: "submit".into(),
            goal: "submit".into(),
            done_when: DoneCondition::Structured(vec![]),
            requires_values: vec![],
            mutation_limit: 2,
        }
    }

    fn natural_step() -> Step {
        Step {
            done_when: DoneCondition::NaturalLanguage(
                "El campo Account name contiene el valor proporcionado".into(),
            ),
            ..step()
        }
    }

    fn decide_not_done(
        policy: &mut Policy,
        step: &Step,
        observation: &Observation,
        judgments: &Judgments,
        operation_reobserved: bool,
    ) -> Next {
        policy.decide(
            step,
            observation,
            judgments,
            DoneResult::NotSatisfied,
            false,
            operation_reobserved,
        )
    }

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
    fn replay_ledger_is_global_across_steps_for_the_same_submit_semantics() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let first_step = step();
        let mut later_step = step();
        later_step.id = "confirm-again".into();
        let first = decide_not_done(
            &mut policy,
            &first_step,
            &observation("CLICK", "button"),
            &judgments("CLICK"),
            false,
        );
        assert!(matches!(first, Next::Mutate(_)));
        policy.begin_step();
        let mut remounted = observation("CLICK", "button");
        remounted.document_id = "new-document-token".into();
        remounted.elements[0].node_id = 999;
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &later_step,
                &remounted,
                &judgments("CLICK"),
                false
            ),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
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
    fn only_operation_confidence_is_gated() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut click = judgments("CLICK");
        click.type_target.confidence = 0.01;
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("CLICK", "button"),
                &click,
                false
            ),
            Next::Mutate(_)
        ));
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        click.click_target.confidence = 0.69;
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("CLICK", "button"),
                &click,
                false
            ),
            Next::Mutate(_)
        ));
        let mut typed = judgments("TYPE_TEXT");
        typed.type_value.confidence = 0.01;
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("TYPE_TEXT", "textbox"),
                &typed,
                false
            ),
            Next::Mutate(_)
        ));
    }

    #[test]
    fn native_select_stops_before_authorization() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("SELECT", "combobox"),
                &judgments("CLICK"),
                false
            ),
            Next::Stop(PolicyStop::Blocked("native_select"))
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
    fn operation_gate_reobserves_once_and_none_fits_never_mints_a_permit() {
        let mut low = judgments("CLICK");
        low.operation.confidence = 0.69;
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("CLICK", "button"),
                &low,
                false
            ),
            Next::ReobserveOperation
        ));
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("CLICK", "button"),
                &low,
                true
            ),
            Next::Stop(PolicyStop::Uncertain("operation_below_gate"))
        ));
        let mut none = judgments("TYPE_TEXT");
        none.type_value = choice("NONE_FITS");
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("TYPE_TEXT", "textbox"),
                &none,
                false
            ),
            Next::Stop(PolicyStop::Blocked("value_not_provided"))
        ));
    }

    #[test]
    fn done_first_prevents_a_confident_operation_from_dispatching() {
        let mut judgment = judgments("CLICK");
        judgment.operation.confidence = 0.95;
        judgment.step_done = 0.50;
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            policy.decide(
                &natural_step(),
                &observation("CLICK", "button"),
                &judgment,
                DoneResult::Unknown,
                false,
                false
            ),
            Next::ReobserveDone
        ));
        assert_eq!(policy.actions, 0);
        assert!(matches!(
            policy.decide(
                &natural_step(),
                &observation("CLICK", "button"),
                &judgment,
                DoneResult::Unknown,
                true,
                false
            ),
            Next::Stop(PolicyStop::Uncertain("done_uncertain"))
        ));
        assert_eq!(policy.actions, 0);
    }

    #[test]
    fn recorded_iteration_two_spanish_done_judgments_stop_instead_of_advancing() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/iteration2-spanish-done-records.json"
        ))
        .unwrap();
        for record in fixture["records"].as_array().unwrap() {
            assert_eq!(record["first"]["step"], 2);
            assert_eq!(record["first"]["call"], 2);
            assert_eq!(record["first"]["retry"], false);
            assert!(record["first"]["structured_done"].is_null());
            assert_eq!(record["retry"]["step"], 2);
            assert_eq!(record["retry"]["call"], 3);
            assert_eq!(record["retry"]["retry"], true);
            assert!(record["retry"]["structured_done"].is_null());
            let replay = |decision: &Value| {
                let mut judgment = judgments(decision["operation"].as_str().unwrap());
                judgment.step_done = decision["step_done"].as_f64().unwrap();
                judgment.operation.confidence = decision["confidence"].as_f64().unwrap();
                judgment
            };
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
            assert!(matches!(
                policy.decide(
                    &natural_step(),
                    &observation("TYPE_TEXT", "textbox"),
                    &replay(&record["first"]),
                    DoneResult::Unknown,
                    false,
                    false
                ),
                Next::ReobserveDone
            ));
            assert!(matches!(
                policy.decide(
                    &natural_step(),
                    &observation("TYPE_TEXT", "textbox"),
                    &replay(&record["retry"]),
                    DoneResult::Unknown,
                    true,
                    false
                ),
                Next::Stop(PolicyStop::Uncertain("done_uncertain"))
            ));
            assert_eq!(policy.actions, 0, "record {}", record["source"]);
        }
    }

    #[test]
    fn recorded_iteration_two_decisions_replay_done_first_and_gate_consumption() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/iteration2-policy-records.json"
        ))
        .unwrap();
        let records = fixture["records"].as_array().unwrap();
        let from_record = |record: &Value| {
            let operation = record["operation"].as_str().unwrap();
            let confidence = record["confidence"].as_f64().unwrap();
            let mut replay = judgments(operation);
            replay.operation.confidence = confidence;
            replay.operation.probabilities = BTreeMap::from([(operation.into(), confidence)]);
            if let Some(target) = record.get("target").and_then(Value::as_str) {
                replay.click_target = choice(target);
            }
            replay
        };

        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert_eq!(records[0]["structured_done"], true);
        // The recorded speculative operation is deliberately not passed to policy:
        // production consumes the authoritative structured result first.
        assert_eq!(policy.actions, 0);

        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("CLICK", "button"),
                &from_record(&records[1]),
                records[1]["retry"].as_bool().unwrap()
            ),
            Next::ReobserveOperation
        ));
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("CLICK", "button"),
                &from_record(&records[2]),
                records[2]["retry"].as_bool().unwrap()
            ),
            Next::Stop(PolicyStop::Uncertain("operation_below_gate"))
        ));

        let mut recorded_page = observation("CLICK", records[3]["target_role"].as_str().unwrap());
        recorded_page.elements[0].index = records[3]["target"].as_str().unwrap().parse().unwrap();
        recorded_page.elements[0].name = records[3]["target_name"].as_str().unwrap().into();
        let mut fresh_policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(
                &mut fresh_policy,
                &step(),
                &recorded_page,
                &from_record(&records[3]),
                false
            ),
            Next::Mutate(_)
        ));
    }

    #[test]
    fn origin_and_model_call_budgets_stop_before_authorization() {
        let options = JobOptions {
            allowed_origins: Some(vec!["http://allowed.test".into()]),
            max_model_calls: Some(1),
            ..JobOptions::default()
        };
        let mut policy = Policy::new(&options, "http://allowed.test/");
        policy.record_model_call().unwrap();
        assert_eq!(
            policy.record_model_call(),
            Err(PolicyStop::Blocked("budget_exhausted"))
        );
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("CLICK", "button"),
                &judgments("CLICK"),
                false
            ),
            Next::Stop(PolicyStop::Blocked("origin_not_allowed"))
        ));
    }
}
