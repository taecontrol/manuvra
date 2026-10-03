//! Observations, judgments, and steps shared by the policy's unit tests.

use crate::judgment::{ChoiceJudgment, Judgments};
use crate::policy::{Next, Permit, Policy};
use crate::test_support::hover_region as region;
use crate::verification::DoneResult;
use manuvra_chrome::{
    Coverage, Element, FocusAnchor, Observation, Rect, SelectOption, ViewportState,
};
use manuvra_contract::{DoneCondition, JobValue, Step};
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) fn observation(operation: &str, role: &str) -> Observation {
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
        colors: Vec::new(),
        colors_complete: false,
        color_scopes: Vec::new(),
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
            container: None,
            shares_name: false,
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
        scroll_regions: Vec::new(),
        overlay: None,
        scroll_regions_truncated: false,
        hover_regions_truncated: false,
        hover_rules_unreadable: false,
    }
}
pub(crate) fn choice(value: &str) -> ChoiceJudgment {
    ChoiceJudgment {
        choice: value.into(),
        probabilities: BTreeMap::from([(value.into(), 1.0)]),
        confidence: 1.0,
    }
}
pub(crate) fn judgments(operation: &str) -> Judgments {
    Judgments {
        operation: choice(operation),
        click_target: choice("1"),
        type_target: choice("1"),
        select_target: choice("1"),
        type_value: choice("name"),
        key: choice("Escape"),
        step_done: 0.5,
        usage: BTreeMap::new(),
        request_id: None,
        model: "jev".into(),
        request: Value::Null,
    }
}

pub(crate) fn focused(name: &str) -> Observation {
    let mut observed = observation("CLICK", "button");
    observed.focus_anchor = Some(FocusAnchor {
        node_id: 9,
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
    observed
}

pub(crate) fn file_input_focus() -> Observation {
    let mut observed = focused("Receipt");
    observed.elements[0].name = "Receipt".into();
    observed.elements[0].role = "button".into();
    observed.elements[0].input_type = Some("file".into());
    observed
}

pub(crate) fn step() -> Step {
    Step {
        id: "submit".into(),
        goal: "submit".into(),
        done_when: DoneCondition::Structured(vec![]),
        requires_values: vec![],
        mutation_limit: 2,
    }
}

pub(crate) fn natural_step() -> Step {
    Step {
        done_when: DoneCondition::NaturalLanguage(
            "El campo Account name contiene el valor proporcionado".into(),
        ),
        ..step()
    }
}

pub(crate) fn decide_not_done(
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

pub(crate) fn provided(value: &str) -> BTreeMap<String, JobValue> {
    BTreeMap::from([(
        "name".to_owned(),
        JobValue {
            value: value.into(),
            description: "account".into(),
            formats: None,
            secret: false,
        },
    )])
}

pub(crate) fn native_select(options: &[(&str, &str, bool)]) -> Observation {
    let mut page = observation("SELECT", "combobox");
    page.elements[0].select_options = options
        .iter()
        .enumerate()
        .map(|(position, (label, value, disabled))| SelectOption {
            node_id: 20 + position as u64,
            label: (*label).into(),
            value: (*value).into(),
            disabled: *disabled,
            selected: false,
        })
        .collect();
    page
}

pub(crate) fn hover_page() -> Observation {
    let mut page = observation("CLICK", "button");
    page.hover_regions = vec![region(1, "Groceries", 41), region(2, "Rent", 42)];
    page
}

pub(crate) fn hover(target: Option<&str>) -> Judgments {
    let mut judgment = judgments("CLICK");
    judgment.click_target = target
        .map(choice)
        .unwrap_or_else(|| choice("NO_CLICK_TARGET"));
    judgment
}

pub(crate) fn one_mutation_step() -> Step {
    Step {
        mutation_limit: 1,
        ..step()
    }
}

pub(crate) fn minted(next: Next) -> Permit {
    match next {
        Next::Mutate(permit) => *permit,
        other => panic!("expected a permit, got {other:?}"),
    }
}
