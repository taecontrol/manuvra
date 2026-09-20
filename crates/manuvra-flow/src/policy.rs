use crate::judgment::{Judgments, Operation, selected_operation};
use manuvra_chrome::{Element, Observation};
use manuvra_contract::{JobOptions, Step};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use url::Url;

const GATE: f64 = 0.70;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub id: String,
    pub operation: Operation,
    pub target_index: Option<u64>,
    pub target_name: Option<String>,
    pub value_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyStop {
    Uncertain(&'static str),
    Blocked(&'static str),
}

#[derive(Debug)]
pub enum Next {
    Reobserve,
    Wait,
    Mutate(Permit),
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
        }
    }

    pub fn begin_step(&mut self) {
        self.step_mutations = 0;
    }

    pub fn check_active(&self) -> Result<(), PolicyStop> {
        (self.started.elapsed() < self.active_timeout)
            .then_some(())
            .ok_or(PolicyStop::Blocked("budget_exhausted"))
    }

    pub fn record_model_call(&mut self) -> Result<Instant, PolicyStop> {
        self.check_active()?;
        if self.model_calls >= self.max_model_calls {
            return Err(PolicyStop::Blocked("budget_exhausted"));
        }
        self.model_calls += 1;
        Ok(self.started + self.active_timeout)
    }

    pub fn decide(
        &mut self,
        step: &Step,
        observation: &Observation,
        judgments: &Judgments,
        already_reobserved: bool,
    ) -> Next {
        if let Some(capability) = unsupported_capability(observation, judgments) {
            return Next::Stop(PolicyStop::Blocked(capability));
        }
        let Ok(operation) = selected_operation(judgments) else {
            return Next::Stop(PolicyStop::Blocked("provider_invalid_response"));
        };
        if let Some(next) = operation_gate(judgments, already_reobserved) {
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
        let target = match selected_target(observation, judgments, operation) {
            Ok(target) => target,
            Err(stop) => return Next::Stop(stop),
        };
        let value_name = match selected_value(judgments, operation) {
            Ok(value) => value,
            Err(stop) => return Next::Stop(stop),
        };
        let candidate = Candidate {
            id: format!("c_{}_{}", self.actions + 1, target.index),
            operation,
            target_index: Some(target.index),
            target_name: Some(target.name.clone()),
            value_name,
        };
        self.mint(observation, target, candidate)
    }

    fn authorize_context(&self, step: &Step, observation: &Observation) -> Result<(), PolicyStop> {
        if self.actions >= self.max_actions
            || self.step_mutations >= step.mutation_limit
            || self.started.elapsed() >= self.active_timeout
        {
            return Err(PolicyStop::Blocked("budget_exhausted"));
        }
        let allowed = origin(&observation.url)
            .is_some_and(|current| self.allowed_origins.iter().any(|origin| origin == &current));
        allowed
            .then_some(())
            .ok_or(PolicyStop::Blocked("origin_not_allowed"))
    }

    fn mint(&mut self, observation: &Observation, target: &Element, candidate: Candidate) -> Next {
        let replay_key = replay_key(observation, target, &candidate);
        if !self.replay.insert(replay_key.clone()) {
            return Next::Stop(PolicyStop::Uncertain("replay_forbidden"));
        }
        self.actions += 1;
        self.step_mutations += 1;
        Next::Mutate(Permit {
            candidate,
            document_id: observation.document_id.clone(),
            replay_key,
            action_sequence: u64::from(self.actions),
        })
    }

    pub fn active_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }
}

fn operation_gate(judgments: &Judgments, already_reobserved: bool) -> Option<Next> {
    (judgments.operation.confidence < GATE).then_some({
        if already_reobserved {
            Next::Stop(PolicyStop::Uncertain("operation_below_gate"))
        } else {
            Next::Reobserve
        }
    })
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
            && !observation.elements.iter().any(supports_slice_three_input))
}

fn supports_slice_three_input(element: &Element) -> bool {
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

    #[test]
    fn permit_is_single_owner_and_replay_survives_wait_remount_shape() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let first = policy.decide(
            &step(),
            &observation("CLICK", "button"),
            &judgments("CLICK"),
            false,
        );
        assert!(matches!(first, Next::Mutate(_)));
        assert!(matches!(
            policy.decide(
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
            policy.decide(&step(), &remount, &judgments("CLICK"), false),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
    }

    #[test]
    fn replay_ledger_is_global_across_steps_for_the_same_submit_semantics() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let first_step = step();
        let mut later_step = step();
        later_step.id = "confirm-again".into();
        let first = policy.decide(
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
            policy.decide(&later_step, &remounted, &judgments("CLICK"), false),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
    }

    #[test]
    fn only_operation_confidence_is_gated() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut click = judgments("CLICK");
        click.type_target.confidence = 0.01;
        assert!(matches!(
            policy.decide(&step(), &observation("CLICK", "button"), &click, false),
            Next::Mutate(_)
        ));
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        click.click_target.confidence = 0.69;
        assert!(matches!(
            policy.decide(&step(), &observation("CLICK", "button"), &click, false),
            Next::Mutate(_)
        ));
        let mut typed = judgments("TYPE_TEXT");
        typed.type_value.confidence = 0.01;
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            policy.decide(&step(), &observation("TYPE_TEXT", "textbox"), &typed, false),
            Next::Mutate(_)
        ));
    }

    #[test]
    fn native_select_stops_before_authorization() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            policy.decide(
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
            policy.decide(&step(), &control, &judgments("CLICK"), false),
            Next::Mutate(_)
        ));
        let mut option = observation("CLICK", "option");
        option.elements[0].name = "+ New currency or asset…".into();
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            policy.decide(&step(), &option, &judgments("CLICK"), false),
            Next::Mutate(_)
        ));
    }

    #[test]
    fn operation_gate_reobserves_once_and_none_fits_never_mints_a_permit() {
        let mut low = judgments("CLICK");
        low.operation.confidence = 0.69;
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            policy.decide(&step(), &observation("CLICK", "button"), &low, false),
            Next::Reobserve
        ));
        assert!(matches!(
            policy.decide(&step(), &observation("CLICK", "button"), &low, true),
            Next::Stop(PolicyStop::Uncertain("operation_below_gate"))
        ));
        let mut none = judgments("TYPE_TEXT");
        none.type_value = choice("NONE_FITS");
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            policy.decide(&step(), &observation("TYPE_TEXT", "textbox"), &none, false),
            Next::Stop(PolicyStop::Blocked("value_not_provided"))
        ));
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
            policy.decide(
                &step(),
                &observation("CLICK", "button"),
                &from_record(&records[1]),
                records[1]["retry"].as_bool().unwrap()
            ),
            Next::Reobserve
        ));
        assert!(matches!(
            policy.decide(
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
            fresh_policy.decide(&step(), &recorded_page, &from_record(&records[3]), false),
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
            policy.decide(
                &step(),
                &observation("CLICK", "button"),
                &judgments("CLICK"),
                false
            ),
            Next::Stop(PolicyStop::Blocked("origin_not_allowed"))
        ));
    }
}
