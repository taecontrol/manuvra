//! Code-owned run policy: done-first, the per-operation confidence gates, budgets, the replay
//! ledger, and origin checks. Only this policy mints a permit, and no mutation is dispatched
//! without one.

mod candidate;
mod gates;
mod ledger;
mod replay_key;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
mod revalidation;
pub(crate) mod scroll;
mod surface;
mod targeting;
pub(crate) use targeting::{ClickChoice, click_choice, reveal_key};

pub(crate) use candidate::Candidate;
pub use candidate::HoverTarget;
pub use gates::done_first;
pub(crate) use targeting::offered_select_option;

use crate::judgment::{Judgments, Operation};
use crate::verification::DoneResult;
use gates::ready_operation;
use manuvra_chrome::Observation;
use manuvra_contract::{JobOptions, JobValue, Step};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};
use url::Url;

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
    scroll_attempts: HashMap<String, u16>,
    step: u32,
    /// Every minted replay key with the operation it charged, so refunds return exactly that.
    replay: HashMap<String, Operation>,
    /// Focus identity of each key press whose outcome is not yet known to be observed or not
    /// performed, by replay key. An uncertain press may have changed the anchor's own state, so
    /// while it stays here the same key on the same focus identity is refused whatever that
    /// state now shows.
    unsettled_keys: HashMap<String, String>,
    allowed_origins: Vec<String>,
    /// Caller-provided value texts by name, so a `SELECT` is authorized only for an offered option.
    provided_values: HashMap<String, String>,
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
            scroll_attempts: HashMap::new(),
            step: 0,
            replay: HashMap::new(),
            unsettled_keys: HashMap::new(),
            allowed_origins,
            provided_values: HashMap::new(),
            paused_at: None,
            paused_total: Duration::ZERO,
        }
    }

    pub fn with_provided_values(mut self, values: &BTreeMap<String, JobValue>) -> Self {
        self.provided_values = values
            .iter()
            .map(|(name, value)| (name.clone(), value.value.clone()))
            .collect();
        self
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
            .and_then(|()| self.candidate(observation, judgments, operation))
            .and_then(|candidate| {
                self.check_select_value(observation, &candidate)
                    .map(|()| candidate)
            });
        match authorized {
            Ok(candidate) => self.mint(observation, candidate),
            Err(stop) => Next::Stop(stop),
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

fn origin(url: &str) -> Option<String> {
    let url = Url::parse(url).ok()?;
    let origin = url.origin().ascii_serialization();
    (origin != "null").then_some(origin)
}

#[cfg(test)]
mod tests {
    pub(super) mod support;

    use super::*;
    use support::*;

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
                &hover(Some("R1_1")),
                false
            ),
            Next::Stop(PolicyStop::Blocked("origin_not_allowed"))
        ));
        let mut popup = hover_page();
        popup.coverage.gaps = vec!["popup".into()];
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &popup, &hover(Some("R1_1")), false),
            Next::Stop(PolicyStop::UnsupportedSurface("popup_or_new_tab"))
        ));
        assert_eq!(policy.actions, 0);
    }
}
