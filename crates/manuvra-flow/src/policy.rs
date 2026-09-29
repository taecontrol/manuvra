use crate::judgment::{ChoiceJudgment, Judgments, Operation, hover_region_key, selected_operation};
use crate::verification::DoneResult;
use manuvra_chrome::{Element, FocusAnchor, FocusSurface, HoverRegion, Key, Observation};
use manuvra_contract::{DoneCondition, JobOptions, Step};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use url::Url;

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
    pub(crate) key: Option<Key>,
    pub(crate) focus_anchor: Option<FocusAnchor>,
    pub(crate) hover_target: Option<HoverTarget>,
}

/// The hover region a `HOVER` candidate targets, as recorded in evidence. Its dispatch identity,
/// the region's first hidden control, stays in the candidate's target identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoverTarget {
    pub index: u64,
    pub name: String,
    pub reveals_on_hover: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TargetIdentity {
    node_id: Option<u64>,
    document_id: String,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Serialize)]
struct OfferedCandidate<'a> {
    id: &'a str,
    operation: Operation,
    target_name: Option<&'a str>,
    target_role: Option<&'a str>,
    target_dialog: Option<&'a str>,
    target_input_type: Option<&'a str>,
    value_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<Key>,
    #[serde(skip_serializing_if = "Option::is_none")]
    focus_anchor: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hover_target: Option<OfferedHoverTarget<'a>>,
}

/// The hover region a caller is offered: its name and the controls it reveals, never its index
/// or dispatch identity.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Serialize)]
struct OfferedHoverTarget<'a> {
    name: &'a str,
    reveals_on_hover: &'a [String],
}

impl Candidate {
    #[cfg(any(target_os = "linux", target_os = "macos", test))]
    pub(crate) fn offered(&self) -> serde_json::Value {
        serde_json::to_value(OfferedCandidate {
            id: &self.id,
            operation: self.operation,
            target_name: self.target_name.as_deref(),
            target_role: self.target_role.as_deref(),
            target_dialog: self.target_dialog.as_deref(),
            target_input_type: self.target_input_type.as_deref(),
            value_name: self.value_name.as_deref(),
            key: self.key,
            focus_anchor: (self.operation == Operation::PressKey).then(|| {
                self.focus_anchor.as_ref().map_or(serde_json::Value::Null, |anchor| {
                    serde_json::json!({"role":anchor.role,"name":anchor.name,"dialog":anchor.in_dialog})
                })
            }),
            hover_target: self.hover_target.as_ref().map(|region| OfferedHoverTarget {
                name: &region.name,
                reveals_on_hover: &region.reveals_on_hover,
            }),
        })
        .expect("offered candidate is serializable")
    }

    /// The observed hover region this candidate targets, when it is still the same region: same
    /// document, same hidden control, same name and reveals.
    pub(crate) fn hover_region<'a>(&self, observation: &'a Observation) -> Option<&'a HoverRegion> {
        let wanted = self.hover_target.as_ref()?;
        (observation.document_id == self.target_identity.document_id)
            .then_some(&observation.hover_regions)?
            .iter()
            .find(|region| {
                region.index == wanted.index
                    && Some(region.node_id) == self.target_identity.node_id
                    && region.name == wanted.name
                    && region.reveals_on_hover == wanted.reveals_on_hover
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyStop {
    Uncertain(&'static str),
    Blocked(&'static str),
    UnsupportedSurface(&'static str),
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
    #[cfg(any(target_os = "linux", target_os = "macos", test))]
    pub(crate) fn operation(&self) -> Operation {
        self.candidate.operation
    }

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
    fallbacks: u8,
    step: u32,
    /// Every minted replay key with the operation it charged, so refunds return exactly that.
    replay: HashMap<String, Operation>,
    /// Focus identity of each key press whose outcome is not yet known to be observed or not
    /// performed, by replay key. An uncertain press may have changed the anchor's own state, so
    /// while it stays here the same key on the same focus identity is refused whatever that
    /// state now shows.
    unsettled_keys: HashMap<String, String>,
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
            fallbacks: 0,
            step: 0,
            replay: HashMap::new(),
            unsettled_keys: HashMap::new(),
            allowed_origins,
            paused_at: None,
            paused_total: Duration::ZERO,
        }
    }

    pub fn begin_step(&mut self) {
        self.step = self.step.wrapping_add(1);
        self.step_mutations = 0;
        self.fallbacks = 0;
    }

    pub fn check_active(&self) -> Result<(), PolicyStop> {
        (self.active_elapsed() < self.active_timeout)
            .then_some(())
            .ok_or(PolicyStop::Blocked("budget_exhausted"))
    }

    pub fn check_origin(&self, observation: &Observation) -> Result<(), PolicyStop> {
        if observation.coverage.gaps.iter().any(|gap| gap == "popup") {
            return Err(PolicyStop::UnsupportedSurface("popup_or_new_tab"));
        }
        let allowed = origin(&observation.url)
            .is_some_and(|current| self.allowed_origins.iter().any(|origin| origin == &current));
        allowed
            .then_some(())
            .ok_or(PolicyStop::Blocked("origin_not_allowed"))
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
        let operation = match ready_operation(observation, judgments, operation_reobserved) {
            Ok(operation) => operation,
            Err(next) => return next,
        };
        self.decide_operation(step, observation, judgments, operation)
    }

    fn decide_operation(
        &mut self,
        step: &Step,
        observation: &Observation,
        judgments: &Judgments,
        operation: Operation,
    ) -> Next {
        match operation {
            Operation::Wait => self.fallback(Next::Wait),
            Operation::Blocked => Next::Stop(PolicyStop::Blocked("operation_blocked")),
            _ => self.authorize(step, observation, judgments, operation),
        }
    }

    fn authorize(
        &mut self,
        step: &Step,
        observation: &Observation,
        judgments: &Judgments,
        operation: Operation,
    ) -> Next {
        let authorized = self
            .authorize_context(step, observation)
            .and_then(|()| self.check_fallback_budget(operation))
            .and_then(|()| self.candidate(observation, judgments, operation));
        match authorized {
            Ok(candidate) => self.mint(observation, candidate),
            Err(stop) => Next::Stop(stop),
        }
    }

    fn check_fallback_budget(&self, operation: Operation) -> Result<(), PolicyStop> {
        (operation.mutates() || self.fallbacks < 8)
            .then_some(())
            .ok_or(PolicyStop::Blocked("budget_exhausted"))
    }

    #[cfg(any(target_os = "linux", target_os = "macos", test))]
    pub(crate) fn caller_candidate(
        &self,
        observation: &Observation,
        judgments: &Judgments,
    ) -> Result<Candidate, PolicyStop> {
        let operation = selected_operation(judgments)
            .map_err(|_| PolicyStop::Blocked("provider_invalid_response"))?;
        match operation {
            Operation::Click
            | Operation::TypeText
            | Operation::Select
            | Operation::Hover
            | Operation::PressKey => self.candidate(observation, judgments, operation),
            _ => Err(PolicyStop::Blocked("provider_invalid_response")),
        }
    }

    fn candidate(
        &self,
        observation: &Observation,
        judgments: &Judgments,
        operation: Operation,
    ) -> Result<Candidate, PolicyStop> {
        match operation {
            Operation::ScrollUp | Operation::ScrollDown => {
                self.scroll_candidate(observation, operation)
            }
            Operation::Hover => self.hover_candidate(observation, judgments),
            Operation::PressKey => self.key_candidate(observation, judgments),
            _ => self.element_candidate(observation, judgments, operation),
        }
    }

    fn element_candidate(
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
            key: None,
            focus_anchor: None,
            hover_target: None,
        })
    }

    fn key_candidate(
        &self,
        observation: &Observation,
        judgments: &Judgments,
    ) -> Result<Candidate, PolicyStop> {
        let key = Key::from_choice(&judgments.key.choice)
            .ok_or(PolicyStop::Blocked("provider_invalid_response"))?;
        let anchor = observation.focus_anchor.clone();
        Ok(Candidate {
            id: format!("c_{}", self.actions + 1),
            operation: Operation::PressKey,
            target_index: None,
            target_name: anchor.as_ref().map(|anchor| anchor.name.clone()),
            target_identity: TargetIdentity {
                node_id: anchor.as_ref().map(|anchor| anchor.node_id),
                document_id: observation.document_id.clone(),
            },
            target_role: anchor.as_ref().map(|anchor| anchor.role.clone()),
            target_dialog: anchor.as_ref().and_then(|anchor| anchor.in_dialog.clone()),
            target_input_type: None,
            value_name: None,
            key: Some(key),
            focus_anchor: anchor,
            hover_target: None,
        })
    }

    fn scroll_candidate(
        &self,
        observation: &Observation,
        operation: Operation,
    ) -> Result<Candidate, PolicyStop> {
        let can_scroll = match operation {
            Operation::ScrollUp => observation.viewport.scroll_y > 0.0,
            Operation::ScrollDown => {
                observation.viewport.scroll_y + f64::from(observation.viewport.height)
                    < observation.viewport.document_height
            }
            _ => false,
        };
        can_scroll
            .then(|| self.untargeted_candidate(observation, operation))
            .ok_or(PolicyStop::Blocked("operation_blocked"))
    }

    fn hover_candidate(
        &self,
        observation: &Observation,
        judgments: &Judgments,
    ) -> Result<Candidate, PolicyStop> {
        let region = selected_hover_region(observation, judgments)
            .ok_or(PolicyStop::Blocked("provider_invalid_response"))?;
        let mut candidate = self.untargeted_candidate(observation, Operation::Hover);
        candidate.target_identity.node_id = Some(region.node_id);
        candidate.hover_target = Some(HoverTarget {
            index: region.index,
            name: region.name.clone(),
            reveals_on_hover: region.reveals_on_hover.clone(),
        });
        Ok(candidate)
    }

    fn untargeted_candidate(&self, observation: &Observation, operation: Operation) -> Candidate {
        Candidate {
            id: format!("c_{}", self.actions + 1),
            operation,
            target_index: None,
            target_name: None,
            target_identity: TargetIdentity {
                node_id: None,
                document_id: observation.document_id.clone(),
            },
            target_role: None,
            target_dialog: None,
            target_input_type: None,
            value_name: None,
            key: None,
            focus_anchor: None,
            hover_target: None,
        }
    }

    fn fallback(&mut self, next: Next) -> Next {
        match self.check_fallback_budget(Operation::Wait) {
            Ok(()) => {
                self.charge(Operation::Wait);
                next
            }
            Err(stop) => Next::Stop(stop),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos", test))]
    pub(crate) fn authorize_caller(
        &mut self,
        step: &Step,
        observation: &Observation,
        candidate: &Candidate,
    ) -> Result<Permit, PolicyStop> {
        self.authorize_context(step, observation)?;
        self.check_fallback_budget(candidate.operation)?;
        let current = revalidate_caller_candidate(observation, candidate)?;
        match self.mint(observation, current) {
            Next::Mutate(permit) => Ok(*permit),
            Next::Stop(stop) => Err(stop),
            _ => unreachable!("caller authorization only mints or stops"),
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

    fn authorize_context(&self, step: &Step, observation: &Observation) -> Result<(), PolicyStop> {
        if self.actions >= self.max_actions
            || self.step_mutations >= step.mutation_limit
            || self.active_elapsed() >= self.active_timeout
        {
            return Err(PolicyStop::Blocked("budget_exhausted"));
        }
        self.check_origin(observation)
    }

    fn mint(&mut self, observation: &Observation, candidate: Candidate) -> Next {
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

    fn candidate_replay_key(
        &self,
        observation: &Observation,
        candidate: &Candidate,
    ) -> Option<String> {
        match candidate.operation {
            Operation::ScrollUp | Operation::ScrollDown => {
                Some(scroll_replay_key(observation, candidate.operation))
            }
            Operation::Hover => candidate
                .hover_region(observation)
                .map(|region| hover_replay_key(observation, region)),
            Operation::PressKey => {
                let state = candidate.focus_anchor.as_ref().map(focus_state);
                Some(self.key_digest(observation, candidate, state))
            }
            _ => candidate
                .target_index
                .and_then(|index| observation.elements.iter().find(|item| item.index == index))
                .map(|target| replay_key(observation, target, candidate)),
        }
    }

    /// The focus identity of a key press, without the anchor state: while a press with this
    /// identity is unsettled, the same key on the same focus is refused.
    fn key_identity(&self, observation: &Observation, candidate: &Candidate) -> Option<String> {
        (candidate.operation == Operation::PressKey)
            .then(|| self.key_digest(observation, candidate, None))
    }

    fn replay_forbidden(&self, replay_key: &str, key_identity: Option<&str>) -> bool {
        self.replay.contains_key(replay_key)
            || key_identity.is_some_and(|identity| {
                self.unsettled_keys
                    .values()
                    .any(|pending| pending == identity)
            })
    }

    /// Key press replay keys carry the step ordinal, so the same key on the same focus is
    /// refused only within one step.
    fn key_digest(
        &self,
        observation: &Observation,
        candidate: &Candidate,
        state: Option<serde_json::Value>,
    ) -> String {
        let stable = serde_json::json!({
            "operation":candidate.operation,"key":candidate.key,"route":observation.route,
            "step":self.step,"focus":candidate.focus_anchor.as_ref().map(focus_identity),
            "state":state,
        });
        hex::encode(Sha256::digest(stable.to_string().as_bytes()))
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

/// The offered candidate, when the fresh observation still shows the same target: for a key
/// press the same document and focus anchor on a supported surface, for a `HOVER` the same region
/// (document, hidden control, name, and reveals), otherwise the same element.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn candidate_matches(observation: &Observation, target: &Element, candidate: &Candidate) -> bool {
    candidate_identity_matches(observation, target, candidate)
        && candidate_semantics_match(target, candidate)
        && candidate_operation_supported(target, candidate.operation)
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn candidate_identity_matches(
    observation: &Observation,
    target: &Element,
    candidate: &Candidate,
) -> bool {
    observation.document_id == candidate.target_identity.document_id
        && Some(target.node_id) == candidate.target_identity.node_id
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn candidate_semantics_match(target: &Element, candidate: &Candidate) -> bool {
    Some(target.name.as_str()) == candidate.target_name.as_deref()
        && Some(target.role.as_str()) == candidate.target_role.as_deref()
        && target.in_dialog == candidate.target_dialog
        && target.input_type == candidate.target_input_type
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn candidate_operation_supported(target: &Element, operation: Operation) -> bool {
    let expected = match operation {
        Operation::Click => "CLICK",
        Operation::TypeText => "TYPE_TEXT",
        Operation::Select => "SELECT",
        _ => return false,
    };
    target.operations.iter().any(|item| item == expected)
}

/// The operation confidence the chosen operation must reach, proportional to the cost of a wrong
/// choice; a key press's key choice must reach the same gate as the press. `HOVER` is
/// non-mutating, bounded by the fallback budget and the run-wide replay ledger, and revalidated
/// before dispatch, while acting on the control it reveals still needs 0.70.
fn gate(operation: Operation) -> f64 {
    match operation {
        Operation::Hover => 0.60,
        _ => 0.70,
    }
}

fn operation_gate(
    judgments: &Judgments,
    operation: Operation,
    already_reobserved: bool,
) -> Option<Next> {
    (judgments.operation.confidence < gate(operation)).then_some({
        if already_reobserved {
            Next::Stop(PolicyStop::Uncertain("operation_below_gate"))
        } else {
            Next::ReobserveOperation
        }
    })
}

fn ready_operation(
    observation: &Observation,
    judgments: &Judgments,
    already_reobserved: bool,
) -> Result<Operation, Next> {
    let operation = selected_operation(judgments).ok();
    if let Some(surface) = selected_surface(observation, judgments, operation) {
        return Err(Next::Stop(PolicyStop::UnsupportedSurface(surface)));
    }
    let operation =
        operation.ok_or(Next::Stop(PolicyStop::Blocked("provider_invalid_response")))?;
    if let Some(next) = operation_gate(judgments, operation, already_reobserved) {
        return Err(next);
    }
    if operation == Operation::PressKey
        && let Some(next) = key_preflight(judgments, already_reobserved)
    {
        return Err(next);
    }
    Ok(operation)
}

fn selected_surface(
    observation: &Observation,
    judgments: &Judgments,
    operation: Option<Operation>,
) -> Option<&'static str> {
    if operation == Some(Operation::PressKey) {
        key_surface(observation, Key::from_choice(&judgments.key.choice))
    } else {
        unsupported_surface(observation, judgments)
    }
}

fn key_preflight(judgments: &Judgments, already_reobserved: bool) -> Option<Next> {
    if Key::from_choice(&judgments.key.choice).is_none() {
        return Some(Next::Stop(PolicyStop::Blocked("provider_invalid_response")));
    }
    key_gate(judgments, already_reobserved)
}

fn key_gate(judgments: &Judgments, already_reobserved: bool) -> Option<Next> {
    (judgments.key.confidence < gate(Operation::PressKey)).then_some(if already_reobserved {
        Next::Stop(PolicyStop::Uncertain("key_below_gate"))
    } else {
        Next::ReobserveOperation
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
    selected_element(observation, judgments, operation)
        .ok_or(PolicyStop::Blocked("provider_invalid_response"))
}

fn selected_element<'a>(
    observation: &'a Observation,
    judgments: &Judgments,
    operation: Operation,
) -> Option<&'a Element> {
    let answer: &ChoiceJudgment = match operation {
        Operation::Click => &judgments.click_target,
        Operation::TypeText => &judgments.type_target,
        Operation::Select => &judgments.select_target,
        _ => return None,
    };
    parse_target(observation, &answer.choice)
}

/// The listed hover region named by the `hover_target` answer, if any.
fn selected_hover_region<'a>(
    observation: &'a Observation,
    judgments: &Judgments,
) -> Option<&'a HoverRegion> {
    let choice = &judgments.hover_target.as_ref()?.choice;
    observation
        .hover_regions
        .iter()
        .find(|region| hover_region_key(region) == *choice)
}

fn selected_value(
    judgments: &Judgments,
    operation: Operation,
) -> Result<Option<String>, PolicyStop> {
    if !matches!(operation, Operation::TypeText | Operation::Select) {
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

/// The same region in an unchanged region list is one hover; any change to the list's names or
/// reveals makes it a new one.
fn hover_replay_key(observation: &Observation, region: &HoverRegion) -> String {
    let listed: Vec<_> = observation
        .hover_regions
        .iter()
        .map(|item| serde_json::json!({"name":item.name,"reveals_on_hover":item.reveals_on_hover}))
        .collect();
    let stable = serde_json::json!({
        "operation":Operation::Hover,
        "route":observation.route,
        "region_name":region.name,
        "reveals_on_hover":region.reveals_on_hover,
        "hover_regions":hex::encode(Sha256::digest(serde_json::Value::from(listed).to_string().as_bytes())),
    });
    hex::encode(Sha256::digest(stable.to_string().as_bytes()))
}

fn scroll_replay_key(observation: &Observation, operation: Operation) -> String {
    let stable = serde_json::json!({
        "operation":operation,
        "route":observation.route,
        "scroll_y":observation.viewport.scroll_y,
        "document_height":observation.viewport.document_height,
    });
    hex::encode(Sha256::digest(stable.to_string().as_bytes()))
}

fn focus_identity(anchor: &FocusAnchor) -> serde_json::Value {
    serde_json::json!({
        "role":anchor.role,
        "name":anchor.name.to_ascii_lowercase(),
        "dialog":anchor.in_dialog,
        "position":anchor.position,
    })
}

/// Anchor state a legitimate repeated key press changes, such as the active option of a
/// listbox. It distinguishes repeated presses only while no earlier press on the same focus
/// identity is unsettled.
fn focus_state(anchor: &FocusAnchor) -> serde_json::Value {
    serde_json::json!({
        "active_descendant":anchor.active_descendant,
        "expanded":anchor.expanded,
        "selected":anchor.selected,
        "checked":anchor.checked,
    })
}

/// A selected element or listed hover region is a target on the observed surface, so a surface
/// gap elsewhere on the page does not stop it.
fn unsupported_surface(observation: &Observation, judgments: &Judgments) -> Option<&'static str> {
    let operation = selected_operation(judgments).ok();
    let element =
        operation.and_then(|operation| selected_element(observation, judgments, operation));
    if element.is_some_and(|element| element.input_type.as_deref() == Some("file")) {
        return Some("file_input");
    }
    let targeted = element.is_some() || hover_region_selected(observation, judgments, operation);
    (!targeted)
        .then(|| named_surface_gap(&observation.coverage.gaps))
        .flatten()
}

fn hover_region_selected(
    observation: &Observation,
    judgments: &Judgments,
    operation: Option<Operation>,
) -> bool {
    operation == Some(Operation::Hover) && selected_hover_region(observation, judgments).is_some()
}

fn key_surface(observation: &Observation, key: Option<Key>) -> Option<&'static str> {
    let anchor = observation.focus_anchor.as_ref()?;
    anchor_surface(anchor).or_else(|| {
        (matches!(key, Some(Key::Enter | Key::Space)) && focused_file_input(observation, anchor))
            .then_some("file_input")
    })
}

fn focused_file_input(observation: &Observation, anchor: &FocusAnchor) -> bool {
    observation.elements.iter().any(|element| {
        element.node_id == anchor.node_id
            && element.context == anchor.context
            && element.input_type.as_deref() == Some("file")
    })
}

fn anchor_surface(anchor: &FocusAnchor) -> Option<&'static str> {
    match anchor.surface {
        Some(FocusSurface::Canvas) => Some("canvas_control"),
        Some(FocusSurface::CrossOriginFrame) => Some("cross_origin_frame"),
        Some(FocusSurface::ClosedShadowRoot) => Some("closed_shadow_root"),
        None if !anchor.covered => Some("closed_shadow_root"),
        None => None,
    }
}

fn named_surface_gap(gaps: &[String]) -> Option<&'static str> {
    [
        ("cross_origin_frame", "cross_origin_frame"),
        ("closed_shadow_root", "closed_shadow_root"),
        ("canvas", "canvas_control"),
        ("popup", "popup_or_new_tab"),
    ]
    .into_iter()
    .find_map(|(gap, surface)| gaps.iter().any(|actual| actual == gap).then_some(surface))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judgment::ChoiceJudgment;
    use manuvra_chrome::{Coverage, FocusAnchor, Rect, ViewportState};
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
            focus_anchor: None,
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
                select_options: vec![],
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
            hover_regions: Vec::new(),
            hover_regions_truncated: false,
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
            select_target: choice("1"),
            type_value: choice("name"),
            key: choice("Escape"),
            hover_target: None,
            step_done: 0.5,
            usage: BTreeMap::new(),
            request_id: None,
            model: "jev".into(),
            request: Value::Null,
        }
    }

    fn focused(name: &str) -> Observation {
        let mut observed = observation("CLICK", "button");
        observed.focus_anchor = Some(FocusAnchor {
            node_id: 9,
            context: "main".into(),
            role: "button".into(),
            name: name.into(),
            in_dialog: None,
            covered: true,
            surface: None,
            active_descendant: None,
            expanded: None,
            selected: None,
            checked: None,
            position: None,
        });
        observed
    }

    fn observed_press(
        policy: &mut Policy,
        step: &Step,
        observation: &Observation,
        judgments: &Judgments,
    ) -> Next {
        let next = decide_not_done(policy, step, observation, judgments, false);
        if let Next::Mutate(permit) = &next {
            policy.record_observed(&permit.replay_key);
        }
        next
    }

    fn file_input_focus() -> Observation {
        let mut observed = focused("Receipt");
        observed.elements[0].name = "Receipt".into();
        observed.elements[0].role = "button".into();
        observed.elements[0].input_type = Some("file".into());
        observed
    }

    #[test]
    fn key_gate_reobserves_once_then_stops_with_key_below_gate() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut key = judgments("PRESS_KEY");
        key.key.confidence = 0.69;
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &focused("A"), &key, false),
            Next::ReobserveOperation
        ));
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &focused("A"), &key, true),
            Next::Stop(PolicyStop::Uncertain("key_below_gate"))
        ));
        assert_eq!(policy.actions, 0);
    }

    #[test]
    fn operation_gate_precedes_key_validity_and_key_validity_precedes_the_key_gate() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut key = judgments("PRESS_KEY");
        key.operation.confidence = 0.69;
        key.key.confidence = 0.69;
        key.key.choice = "Control+W".into();
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &focused("A"), &key, false),
            Next::ReobserveOperation
        ));
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &focused("A"), &key, true),
            Next::Stop(PolicyStop::Uncertain("operation_below_gate"))
        ));
        key.operation.confidence = 1.0;
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &focused("A"), &key, false),
            Next::Stop(PolicyStop::Blocked("provider_invalid_response"))
        ));
    }

    #[test]
    fn keys_outside_the_closed_roster_are_invalid_responses() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut key = judgments("PRESS_KEY");
        for choice in ["Control+W", "F5", "escape", ""] {
            key.key.choice = choice.into();
            assert!(matches!(
                decide_not_done(&mut policy, &step(), &focused("A"), &key, false),
                Next::Stop(PolicyStop::Blocked("provider_invalid_response"))
            ));
            assert_eq!(
                policy.caller_candidate(&focused("A"), &key),
                Err(PolicyStop::Blocked("provider_invalid_response"))
            );
        }
        assert_eq!(policy.actions, 0);
    }

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
    fn caller_key_candidate_already_in_the_replay_ledger_is_forbidden() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let candidate = policy
            .caller_candidate(&focused("A"), &judgments("PRESS_KEY"))
            .unwrap();
        let permit = policy
            .authorize_caller(&step(), &focused("A"), &candidate)
            .unwrap();
        policy.record_observed(&permit.replay_key);
        assert!(matches!(
            policy.authorize_caller(&step(), &focused("A"), &candidate),
            Err(PolicyStop::Uncertain("replay_forbidden"))
        ));
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &focused("A"),
                &judgments("PRESS_KEY"),
                false
            ),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
        assert_eq!(policy.actions, 1);
    }

    #[test]
    fn key_permits_consume_budget_and_replay_tracks_focus_semantics() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let key = judgments("PRESS_KEY");
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &focused("A"),
                &judgments("WAIT"),
                false
            ),
            Next::Wait
        ));
        assert_eq!(policy.fallbacks, 1);
        let first = decide_not_done(&mut policy, &step(), &focused("A"), &key, false);
        let Next::Mutate(permit) = first else {
            panic!("first key permit")
        };
        assert_eq!(policy.fallbacks, 0);
        let (_, _, replay_key, sequence) = permit.consume();
        assert_eq!(sequence, 1);
        assert_eq!(policy.step_mutations(), 1);
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &focused("A"), &key, false),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &focused("B"), &key, false),
            Next::Mutate(_)
        ));
        assert_eq!(policy.step_mutations(), 2);
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &focused("C"), &key, false),
            Next::Stop(PolicyStop::Blocked("budget_exhausted"))
        ));
        policy.release_not_performed(&replay_key);
        assert_eq!(policy.step_mutations(), 1);
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &focused("A"), &key, false),
            Next::Mutate(_)
        ));
        let options = JobOptions {
            max_actions: Some(1),
            ..JobOptions::default()
        };
        let mut bounded = Policy::new(&options, "http://example.test/");
        assert!(matches!(
            decide_not_done(&mut bounded, &step(), &focused("A"), &key, false),
            Next::Mutate(_)
        ));
        assert!(matches!(
            decide_not_done(&mut bounded, &step(), &focused("B"), &key, false),
            Next::Stop(PolicyStop::Blocked("budget_exhausted"))
        ));
    }

    #[test]
    fn key_replay_tracks_active_descendant_and_aria_state_without_changing_click_replay() {
        use manuvra_chrome::ActiveDescendant;
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut step = step();
        step.mutation_limit = 8;
        let mut observed = focused("Choose item");
        observed.focus_anchor.as_mut().unwrap().role = "combobox".into();
        let mut arrow = judgments("PRESS_KEY");
        arrow.key = choice("ArrowDown");
        assert!(matches!(
            observed_press(&mut policy, &step, &observed, &arrow),
            Next::Mutate(_)
        ));
        assert!(matches!(
            observed_press(&mut policy, &step, &observed, &arrow),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
        let anchor = observed.focus_anchor.as_mut().unwrap();
        anchor.active_descendant = Some(ActiveDescendant {
            id: "alpha".into(),
            role: "option".into(),
            name: "Alpha".into(),
            selected: Some(false),
            checked: None,
        });
        assert!(matches!(
            observed_press(&mut policy, &step, &observed, &arrow),
            Next::Mutate(_)
        ));
        let descendant = observed
            .focus_anchor
            .as_mut()
            .unwrap()
            .active_descendant
            .as_mut()
            .unwrap();
        descendant.id = "beta".into();
        descendant.name = "Beta".into();
        assert!(matches!(
            observed_press(&mut policy, &step, &observed, &arrow),
            Next::Mutate(_)
        ));
        assert!(matches!(
            observed_press(&mut policy, &step, &observed, &arrow),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
        observed
            .focus_anchor
            .as_mut()
            .unwrap()
            .active_descendant
            .as_mut()
            .unwrap()
            .selected = Some(true);
        assert!(matches!(
            observed_press(&mut policy, &step, &observed, &arrow),
            Next::Mutate(_)
        ));
        observed.focus_anchor.as_mut().unwrap().expanded = Some(false);
        assert!(matches!(
            observed_press(&mut policy, &step, &observed, &arrow),
            Next::Mutate(_)
        ));
        observed.focus_anchor.as_mut().unwrap().checked = Some(true);
        assert!(matches!(
            observed_press(&mut policy, &step, &observed, &arrow),
            Next::Mutate(_)
        ));

        let mut click = judgments("CLICK");
        click.click_target = choice("1");
        let first_click = observation("CLICK", "button");
        assert!(matches!(
            decide_not_done(&mut policy, &step, &first_click, &click, false),
            Next::Mutate(_)
        ));
        let mut changed_aria = first_click.clone();
        changed_aria.focus_anchor = observed.focus_anchor;
        assert!(matches!(
            decide_not_done(&mut policy, &step, &changed_aria, &click, false),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
    }

    #[test]
    fn key_replay_distinguishes_unnamed_roving_items_by_position() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut step = step();
        step.mutation_limit = 8;
        let mut first = focused("");
        let anchor = first.focus_anchor.as_mut().unwrap();
        anchor.role = "treeitem".into();
        anchor.position = Some(1);
        let mut arrow = judgments("PRESS_KEY");
        arrow.key = choice("ArrowDown");
        assert!(matches!(
            observed_press(&mut policy, &step, &first, &arrow),
            Next::Mutate(_)
        ));
        let mut second = first.clone();
        second.focus_anchor.as_mut().unwrap().position = Some(2);
        assert!(matches!(
            observed_press(&mut policy, &step, &second, &arrow),
            Next::Mutate(_)
        ));
        assert!(matches!(
            observed_press(&mut policy, &step, &second, &arrow),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
    }

    #[test]
    fn unsettled_key_press_forbids_the_same_key_on_the_same_focus_whatever_its_state() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut step = step();
        step.mutation_limit = 8;
        let mut unchecked = focused("Accept terms");
        let anchor = unchecked.focus_anchor.as_mut().unwrap();
        anchor.role = "checkbox".into();
        anchor.checked = Some(false);
        let mut space = judgments("PRESS_KEY");
        space.key = choice("Space");
        assert!(matches!(
            decide_not_done(&mut policy, &step, &unchecked, &space, false),
            Next::Mutate(_)
        ));
        let mut checked = unchecked.clone();
        let anchor = checked.focus_anchor.as_mut().unwrap();
        anchor.checked = Some(true);
        anchor.expanded = Some(true);
        assert!(matches!(
            decide_not_done(&mut policy, &step, &checked, &space, false),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
        let candidate = policy.caller_candidate(&checked, &space).unwrap();
        assert!(matches!(
            policy.authorize_caller(&step, &checked, &candidate),
            Err(PolicyStop::Uncertain("replay_forbidden"))
        ));
        let mut other_key = space.clone();
        other_key.key = choice("Tab");
        assert!(matches!(
            observed_press(&mut policy, &step, &checked, &other_key),
            Next::Mutate(_)
        ));
        let mut other_focus = checked.clone();
        other_focus.focus_anchor.as_mut().unwrap().name = "Subscribe".into();
        assert!(matches!(
            observed_press(&mut policy, &step, &other_focus, &space),
            Next::Mutate(_)
        ));
        assert_eq!(policy.actions, 3);
    }

    #[test]
    fn not_performed_key_press_releases_its_unsettled_focus_identity() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut enter = judgments("PRESS_KEY");
        enter.key = choice("Enter");
        let mut collapsed = focused("Menu");
        collapsed.focus_anchor.as_mut().unwrap().expanded = Some(false);
        let Next::Mutate(permit) = decide_not_done(&mut policy, &step(), &collapsed, &enter, false)
        else {
            panic!("first key permit")
        };
        policy.release_not_performed(&permit.replay_key);
        let mut expanded = collapsed.clone();
        expanded.focus_anchor.as_mut().unwrap().expanded = Some(true);
        let Next::Mutate(permit) = decide_not_done(&mut policy, &step(), &expanded, &enter, false)
        else {
            panic!("released key permit")
        };
        policy.release_unused(permit);
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &collapsed, &enter, false),
            Next::Mutate(_)
        ));
    }

    #[test]
    fn key_replay_is_scoped_to_the_step() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut step = step();
        step.mutation_limit = 8;
        policy.begin_step();
        let escape = judgments("PRESS_KEY");
        let popover = focused("Breakdown");
        assert!(matches!(
            observed_press(&mut policy, &step, &popover, &escape),
            Next::Mutate(_)
        ));
        assert!(matches!(
            observed_press(&mut policy, &step, &popover, &escape),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
        let mut space = judgments("PRESS_KEY");
        space.key = choice("Space");
        assert!(matches!(
            decide_not_done(&mut policy, &step, &popover, &space, false),
            Next::Mutate(_)
        ));
        let mut changed = popover.clone();
        changed.focus_anchor.as_mut().unwrap().expanded = Some(true);
        assert!(matches!(
            decide_not_done(&mut policy, &step, &changed, &space, false),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));

        policy.begin_step();
        assert!(matches!(
            observed_press(&mut policy, &step, &popover, &escape),
            Next::Mutate(_)
        ));
        assert!(matches!(
            observed_press(&mut policy, &step, &changed, &space),
            Next::Mutate(_)
        ));
        assert!(matches!(
            observed_press(&mut policy, &step, &popover, &escape),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
    }

    #[test]
    fn activation_keys_on_a_focused_file_input_are_an_unsupported_surface() {
        let file = file_input_focus();
        for activation in ["Enter", "Space"] {
            let mut key = judgments("PRESS_KEY");
            key.key = choice(activation);
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
            assert!(matches!(
                decide_not_done(&mut policy, &step(), &file, &key, false),
                Next::Stop(PolicyStop::UnsupportedSurface("file_input"))
            ));
            let candidate = policy.caller_candidate(&file, &key).unwrap();
            assert_eq!(
                policy.authorize_caller(&step(), &file, &candidate).err(),
                Some(PolicyStop::UnsupportedSurface("file_input"))
            );
            assert_eq!(policy.actions, 0);
        }
        for other in [
            "Escape",
            "Tab",
            "Shift+Tab",
            "ArrowUp",
            "ArrowDown",
            "ArrowLeft",
            "ArrowRight",
            "Home",
            "End",
        ] {
            let mut key = judgments("PRESS_KEY");
            key.key = choice(other);
            assert!(matches!(
                decide_not_done(
                    &mut Policy::new(&JobOptions::default(), "http://example.test/"),
                    &step(),
                    &file,
                    &key,
                    false
                ),
                Next::Mutate(_)
            ));
        }
        let mut other_context = file_input_focus();
        other_context.focus_anchor.as_mut().unwrap().context = "main/frame:2".into();
        let mut enter = judgments("PRESS_KEY");
        enter.key = choice("Enter");
        assert!(matches!(
            decide_not_done(
                &mut Policy::new(&JobOptions::default(), "http://example.test/"),
                &step(),
                &other_context,
                &enter,
                false
            ),
            Next::Mutate(_)
        ));
    }

    #[test]
    fn unsupported_key_focus_stops_before_the_confidence_gates() {
        let mut canvas = focused("Drawing");
        canvas.focus_anchor.as_mut().unwrap().surface = Some(FocusSurface::Canvas);
        let mut low_operation = judgments("PRESS_KEY");
        low_operation.operation.confidence = 0.69;
        let mut low_key = judgments("PRESS_KEY");
        low_key.key.confidence = 0.69;
        let mut enter = low_key.clone();
        enter.key.choice = "Enter".into();
        for (observed, key) in [
            (&canvas, &low_operation),
            (&canvas, &low_key),
            (&file_input_focus(), &enter),
        ] {
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
            assert!(matches!(
                decide_not_done(&mut policy, &step(), observed, key, false),
                Next::Stop(PolicyStop::UnsupportedSurface(_))
            ));
        }
    }

    #[test]
    fn key_surface_checks_only_the_focus_anchor() {
        for gap in ["canvas", "cross_origin_frame", "closed_shadow_root"] {
            let mut observed = focused("A");
            observed.coverage.gaps.push(gap.into());
            assert!(
                matches!(
                    decide_not_done(
                        &mut Policy::new(&JobOptions::default(), "http://example.test/"),
                        &step(),
                        &observed,
                        &judgments("PRESS_KEY"),
                        false
                    ),
                    Next::Mutate(_)
                ),
                "{gap}"
            );
        }
        let mut popup = focused("A");
        popup.coverage.gaps.push("popup".into());
        assert!(matches!(
            decide_not_done(
                &mut Policy::new(&JobOptions::default(), "http://example.test/"),
                &step(),
                &popup,
                &judgments("PRESS_KEY"),
                false
            ),
            Next::Stop(PolicyStop::UnsupportedSurface("popup_or_new_tab"))
        ));
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut unsafe_focus = focused("frame");
        unsafe_focus.focus_anchor.as_mut().unwrap().role = "iframe".into();
        unsafe_focus.focus_anchor.as_mut().unwrap().covered = false;
        unsafe_focus.focus_anchor.as_mut().unwrap().surface = Some(FocusSurface::CrossOriginFrame);
        unsafe_focus.coverage.gaps.push("cross_origin_frame".into());
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &unsafe_focus,
                &judgments("PRESS_KEY"),
                false
            ),
            Next::Stop(PolicyStop::UnsupportedSurface("cross_origin_frame"))
        ));
        for (surface, reason) in [
            (FocusSurface::Canvas, "canvas_control"),
            (FocusSurface::ClosedShadowRoot, "closed_shadow_root"),
        ] {
            let mut anchor = focused("A");
            anchor.focus_anchor.as_mut().unwrap().surface = Some(surface);
            assert!(matches!(
                decide_not_done(&mut policy, &step(), &anchor, &judgments("PRESS_KEY"), false),
                Next::Stop(PolicyStop::UnsupportedSurface(actual)) if actual == reason
            ));
        }
        let mut uncovered = focused("A");
        uncovered.focus_anchor.as_mut().unwrap().covered = false;
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &uncovered,
                &judgments("PRESS_KEY"),
                false
            ),
            Next::Stop(PolicyStop::UnsupportedSurface("closed_shadow_root"))
        ));
        let mut no_focus = focused("A");
        no_focus.focus_anchor = None;
        assert!(matches!(
            decide_not_done(
                &mut Policy::new(&JobOptions::default(), "http://example.test/"),
                &step(),
                &no_focus,
                &judgments("PRESS_KEY"),
                false
            ),
            Next::Mutate(_)
        ));
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
    fn operation_and_key_confidence_are_gated_without_gating_targets_or_values() {
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

        let mut key = judgments("PRESS_KEY");
        key.key.confidence = 0.69;
        assert!(matches!(
            decide_not_done(
                &mut Policy::new(&JobOptions::default(), "http://example.test/"),
                &step(),
                &focused("A"),
                &key,
                false
            ),
            Next::ReobserveOperation
        ));
        for (operation, role) in [("CLICK", "button"), ("TYPE_TEXT", "textbox")] {
            let mut other = judgments(operation);
            other.key.confidence = 0.01;
            other.key.choice = "Control+W".into();
            assert!(matches!(
                decide_not_done(
                    &mut Policy::new(&JobOptions::default(), "http://example.test/"),
                    &step(),
                    &observation(operation, role),
                    &other,
                    false
                ),
                Next::Mutate(_)
            ));
        }
    }

    #[test]
    fn native_select_is_authorized_by_the_observed_select_target() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut answer = judgments("SELECT");
        answer.select_target = choice("1");
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("SELECT", "combobox"),
                &answer,
                false
            ),
            Next::Mutate(_)
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

    #[test]
    fn unsupported_surfaces_are_named_and_popup_is_stopped_immediately() {
        let mut file = observation("TYPE_TEXT", "textbox");
        file.elements[0].input_type = Some("file".into());
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &file, &judgments("TYPE_TEXT"), false),
            Next::Stop(PolicyStop::UnsupportedSurface("file_input"))
        ));

        for (gap, expected) in [
            ("cross_origin_frame", "cross_origin_frame"),
            ("closed_shadow_root", "closed_shadow_root"),
            ("canvas", "canvas_control"),
        ] {
            let mut observation = observation("CLICK", "button");
            observation.elements.clear();
            observation.coverage.gaps = vec![gap.into()];
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
            assert!(matches!(
                decide_not_done(
                    &mut policy,
                    &step(),
                    &observation,
                    &judgments("CLICK"),
                    false
                ),
                Next::Stop(PolicyStop::UnsupportedSurface(surface)) if surface == expected
            ));
        }

        let mut popup = observation("CLICK", "button");
        popup.coverage.gaps = vec!["popup".into()];
        let policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert_eq!(
            policy.check_origin(&popup),
            Err(PolicyStop::UnsupportedSurface("popup_or_new_tab"))
        );
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

    fn region(index: u64, name: &str, node_id: u64) -> HoverRegion {
        HoverRegion {
            index,
            name: name.into(),
            reveals_on_hover: vec![format!("Actions for {name}")],
            node_id,
        }
    }

    fn hover_page() -> Observation {
        let mut page = observation("CLICK", "button");
        page.hover_regions = vec![region(1, "Groceries", 41), region(2, "Rent", 42)];
        page
    }

    fn hover(target: Option<&str>) -> Judgments {
        let mut judgment = judgments("HOVER");
        judgment.hover_target = target.map(choice);
        judgment
    }

    fn one_mutation_step() -> Step {
        Step {
            mutation_limit: 1,
            ..step()
        }
    }

    fn minted(next: Next) -> Permit {
        match next {
            Next::Mutate(permit) => *permit,
            other => panic!("expected a permit, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_a_listed_region_mints_a_fallback_permit_naming_the_region() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let permit = minted(decide_not_done(
            &mut policy,
            &step(),
            &hover_page(),
            &hover(Some("R2")),
            false,
        ));
        assert_eq!(permit.operation(), Operation::Hover);
        let (candidate, document_id, _, sequence) = permit.consume();
        assert_eq!(document_id, "d");
        assert_eq!(sequence, 1);
        assert_eq!(candidate.target_index, None);
        assert_eq!(candidate.target_name, None);
        assert_eq!(
            candidate.hover_target,
            Some(HoverTarget {
                index: 2,
                name: "Rent".into(),
                reveals_on_hover: vec!["Actions for Rent".into()],
            })
        );
        assert_eq!(
            candidate.hover_region(&hover_page()),
            Some(&region(2, "Rent", 42))
        );
        assert_eq!(policy.actions, 1);
        assert_eq!(policy.fallbacks, 1);
        assert_eq!(policy.step_mutations(), 0);
    }

    #[test]
    fn hover_follows_done_first_and_the_operation_gate() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let selected = hover(Some("R1"));
        let decide = |policy: &mut Policy, done, done_reobserved, judgment: &Judgments, gate| {
            policy.decide(
                &natural_step(),
                &hover_page(),
                judgment,
                done,
                done_reobserved,
                gate,
            )
        };
        assert!(matches!(
            decide(&mut policy, DoneResult::Satisfied, false, &selected, false),
            Next::Complete
        ));
        assert!(matches!(
            decide(&mut policy, DoneResult::Unknown, false, &selected, false),
            Next::ReobserveDone
        ));
        assert!(matches!(
            decide(&mut policy, DoneResult::Unknown, true, &selected, false),
            Next::Stop(PolicyStop::Uncertain("done_uncertain"))
        ));
        let mut low = selected.clone();
        low.operation.confidence = 0.59;
        assert!(matches!(
            decide(&mut policy, DoneResult::NotSatisfied, false, &low, false),
            Next::ReobserveOperation
        ));
        assert!(matches!(
            decide(&mut policy, DoneResult::NotSatisfied, false, &low, true),
            Next::Stop(PolicyStop::Uncertain("operation_below_gate"))
        ));
        assert_eq!(policy.actions, 0);
    }

    #[test]
    fn a_hover_below_the_gate_is_offered_by_region_name_and_reveals_only() {
        let policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let candidate = policy
            .caller_candidate(&hover_page(), &hover(Some("R2")))
            .unwrap();
        assert_eq!(candidate.operation, Operation::Hover);
        assert_eq!(
            candidate.hover_region(&hover_page()),
            Some(&region(2, "Rent", 42))
        );
        assert_eq!(
            candidate.offered(),
            serde_json::json!({
                "id":"c_1","operation":"HOVER","target_name":null,"target_role":null,
                "target_dialog":null,"target_input_type":null,"value_name":null,
                "hover_target":{"name":"Rent","reveals_on_hover":["Actions for Rent"]}
            })
        );
        for unlisted in [None, Some("R9")] {
            assert_eq!(
                policy.caller_candidate(&hover_page(), &hover(unlisted)),
                Err(PolicyStop::Blocked("provider_invalid_response"))
            );
        }
        for fallback in ["SCROLL_DOWN", "WAIT", "BLOCKED"] {
            assert_eq!(
                policy.caller_candidate(&hover_page(), &judgments(fallback)),
                Err(PolicyStop::Blocked("provider_invalid_response"))
            );
        }
        let element = policy
            .caller_candidate(&hover_page(), &judgments("CLICK"))
            .unwrap()
            .offered();
        assert!(element.get("hover_target").is_none());
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
    fn hover_clears_a_lower_operation_gate_than_every_other_operation() {
        let gated = |operation: &str, confidence: f64, page: &Observation| {
            let mut judgment = judgments(operation);
            judgment.hover_target = Some(choice("R1"));
            judgment.operation.confidence = confidence;
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
            [false, true].map(|reobserved| {
                decide_not_done(&mut policy, &step(), page, &judgment, reobserved)
            })
        };
        for confidence in [0.60, 0.65] {
            let [first, _] = gated("HOVER", confidence, &hover_page());
            assert_eq!(minted(first).operation(), Operation::Hover, "{confidence}");
        }
        let [first, second] = gated("HOVER", 0.59, &hover_page());
        assert!(matches!(first, Next::ReobserveOperation));
        assert!(matches!(
            second,
            Next::Stop(PolicyStop::Uncertain("operation_below_gate"))
        ));
        let others = [
            "CLICK",
            "TYPE_TEXT",
            "SELECT",
            "PRESS_KEY",
            "SCROLL_UP",
            "SCROLL_DOWN",
            "WAIT",
            "BLOCKED",
        ];
        for operation in others {
            let [first, second] = gated(operation, 0.65, &hover_page());
            assert!(matches!(first, Next::ReobserveOperation), "{operation}");
            assert!(
                matches!(
                    second,
                    Next::Stop(PolicyStop::Uncertain("operation_below_gate"))
                ),
                "{operation}"
            );
        }
        let [first, _] = gated("CLICK", 0.70, &observation("CLICK", "button"));
        assert_eq!(minted(first).operation(), Operation::Click);
    }

    #[test]
    fn missing_or_unlisted_hover_target_is_an_invalid_provider_response() {
        for target in [None, Some("R9"), Some("1"), Some("2")] {
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
            assert!(
                matches!(
                    decide_not_done(&mut policy, &step(), &hover_page(), &hover(target), false),
                    Next::Stop(PolicyStop::Blocked("provider_invalid_response"))
                ),
                "{target:?}"
            );
            assert_eq!(policy.actions, 0);
        }
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
                    &hover(Some("R1")),
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
                &hover(Some("R1")),
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
                &hover(Some("R1")),
                false,
            ));
        }
        page.hover_regions[1].name = "Rent 8".into();
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &page, &hover(Some("R1")), false),
            Next::Stop(PolicyStop::Blocked("budget_exhausted"))
        ));
        assert_eq!(policy.step_mutations(), 0);
    }

    #[test]
    fn hover_respects_origin_and_popup_guards() {
        let options = JobOptions {
            allowed_origins: Some(vec!["http://allowed.test".into()]),
            ..JobOptions::default()
        };
        let mut policy = Policy::new(&options, "http://allowed.test/");
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &hover_page(),
                &hover(Some("R1")),
                false
            ),
            Next::Stop(PolicyStop::Blocked("origin_not_allowed"))
        ));
        let mut popup = hover_page();
        popup.coverage.gaps = vec!["popup".into()];
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &popup, &hover(Some("R1")), false),
            Next::Stop(PolicyStop::UnsupportedSurface("popup_or_new_tab"))
        ));
        assert_eq!(policy.actions, 0);
    }

    #[test]
    fn hover_then_click_fits_a_mutation_limit_of_one() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        minted(decide_not_done(
            &mut policy,
            &one_mutation_step(),
            &hover_page(),
            &hover(Some("R1")),
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
        for fallback in [hover(Some("R1")), judgments("SCROLL_DOWN")] {
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
            &hover(Some("R1")),
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
            &hover(Some("R1")),
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
            decide_not_done(&mut policy, &step(), &page, &hover(Some("R1")), false),
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
            &hover(Some("R2")),
            false,
        ));
        let candidate = policy.release_unused(Box::new(permit));
        assert_eq!(candidate.operation, Operation::Hover);
        assert_eq!(policy.step_mutations(), 1);
        assert_eq!(policy.actions, 1);
        assert_eq!(policy.fallbacks, 0);
        assert_eq!(policy.replay.len(), 1);
    }

    #[test]
    fn hover_replay_is_refused_for_an_unchanged_region_list_even_in_a_later_step() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let later = Step {
            id: "later".into(),
            ..step()
        };
        minted(decide_not_done(
            &mut policy,
            &step(),
            &hover_page(),
            &hover(Some("R1")),
            false,
        ));
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &hover_page(),
                &hover(Some("R1")),
                false
            ),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
        policy.begin_step();
        let mut remounted = hover_page();
        remounted.document_id = "new-document".into();
        remounted.hover_regions[0].node_id = 99;
        assert!(matches!(
            decide_not_done(&mut policy, &later, &remounted, &hover(Some("R1")), false),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
        let mut changed = hover_page();
        changed.hover_regions.push(region(3, "Utilities", 43));
        minted(decide_not_done(
            &mut policy,
            &later,
            &changed,
            &hover(Some("R1")),
            false,
        ));
    }

    #[test]
    fn a_listed_hover_target_is_not_stopped_by_a_canvas_gap() {
        let mut canvas = hover_page();
        canvas.elements.clear();
        canvas.coverage.gaps = vec!["canvas".into()];
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        minted(decide_not_done(
            &mut policy,
            &step(),
            &canvas,
            &hover(Some("R1")),
            false,
        ));
        for unlisted in [None, Some("R9")] {
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
            assert!(matches!(
                decide_not_done(&mut policy, &step(), &canvas, &hover(unlisted), false),
                Next::Stop(PolicyStop::UnsupportedSurface("canvas_control"))
            ));
        }
        let mut click = judgments("CLICK");
        click.hover_target = Some(choice("R1"));
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &canvas, &click, false),
            Next::Stop(PolicyStop::UnsupportedSurface("canvas_control"))
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
