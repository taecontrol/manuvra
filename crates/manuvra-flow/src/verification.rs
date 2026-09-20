use crate::values::Values;
use manuvra_chrome::{Element, Observation};
use manuvra_contract::{
    Assertion, AssertionScope, Expectation, ExpectationVerdict, JobValue, NumericCheck,
    VerdictResult,
};
use manuvra_jev::{Answer, Evaluator, JevError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoneResult {
    Satisfied,
    NotSatisfied,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct VerificationReport {
    pub verdicts: Vec<ExpectationVerdict>,
    pub outcome: DoneResult,
    pub record: Value,
}

pub fn verify(
    expectations: &[Expectation],
    observation: &Observation,
    values: &Values<'_>,
    evaluator: &(impl Evaluator + ?Sized),
    deadline: Instant,
) -> Result<VerificationReport, JevError> {
    if expectations.is_empty() {
        return Ok(VerificationReport {
            verdicts: Vec::new(),
            outcome: DoneResult::Satisfied,
            record: json!({"phase":"verification","expectations":[],"provider":null}),
        });
    }
    let request = expectation_request(expectations, observation, values);
    let evaluation = evaluator.evaluate(&request, deadline)?;
    let nouls = expectation_nouls(expectations, &evaluation.answers)?;
    let verdicts = expectations
        .iter()
        .zip(nouls.iter().copied())
        .map(|(expectation, noul)| expectation_verdict(expectation, observation, values, noul))
        .collect::<Vec<_>>();
    let outcome = aggregate_expectations(&verdicts);
    Ok(VerificationReport {
        record: json!({
            "phase":"verification",
            "expectations":verdicts,
            "provider":{
                "model":evaluation.model,
                "request_id":evaluation.request_id,
                "usage":evaluation.usage,
                "request":request,
                "nouls":nouls,
            }
        }),
        verdicts,
        outcome,
    })
}

fn expectation_request(
    expectations: &[Expectation],
    observation: &Observation,
    values: &Values<'_>,
) -> Value {
    let questions = expectations
        .iter()
        .enumerate()
        .map(|(index, expectation)| {
            let claim = values.mask(&expectation.claim);
            (
                expectation_question_id(index),
                json!({
                    "type":"noul",
                    "instructions":format!("Is this exact claim true in the final page state: {claim}"),
                    "criteria":{
                        "true":format!("Every part of this claim is true now: {claim}"),
                        "false":format!("At least one part of this claim is false now: {claim}")
                    }
                }),
            )
        })
        .collect::<Map<_, _>>();
    json!({
        "model":"jev-latest",
        "state":{
            "phase":"final_verification",
            "page":values.model_view(observation),
            "claims":expectations.iter().enumerate().map(|(index, expectation)|json!({
                "question_id":expectation_question_id(index),
                "claim":values.mask(&expectation.claim)
            })).collect::<Vec<_>>()
        },
        "questions":questions
    })
}

fn expectation_nouls(
    expectations: &[Expectation],
    answers: &BTreeMap<String, Answer>,
) -> Result<Vec<f64>, JevError> {
    if answers.len() != expectations.len() {
        return Err(JevError::InvalidResponse(
            "expectation answer ids do not match claims".into(),
        ));
    }
    expectations
        .iter()
        .enumerate()
        .map(
            |(index, _)| match answers.get(&expectation_question_id(index)) {
                Some(Answer::Noul { noul }) if noul.is_finite() && (0.0..=1.0).contains(noul) => {
                    Ok(*noul)
                }
                Some(Answer::Noul { .. }) => Err(JevError::InvalidResponse(
                    "expectation Noul was outside 0 to 1".into(),
                )),
                _ => Err(JevError::InvalidResponse(format!(
                    "answer {} was not an expectation Noul",
                    expectation_question_id(index)
                ))),
            },
        )
        .collect()
}

fn expectation_question_id(index: usize) -> String {
    format!("claim_{:04}", index + 1)
}

fn expectation_verdict(
    expectation: &Expectation,
    observation: &Observation,
    values: &Values<'_>,
    noul: f64,
) -> ExpectationVerdict {
    let checks = expectation_numeric_checks(expectation, observation, values);
    let any_missing = checks
        .iter()
        .any(|check| check.state == NumericState::Missing);
    let any_ambiguous = checks
        .iter()
        .any(|check| check.state == NumericState::Ambiguous);
    let result = if noul <= 0.20 || any_missing {
        VerdictResult::NotSatisfied
    } else if any_ambiguous || noul < 0.80 {
        VerdictResult::Unresolved
    } else {
        VerdictResult::Satisfied
    };
    ExpectationVerdict {
        id: expectation.id.clone(),
        result,
        noul: Some(noul),
        numeric_checks: checks.into_iter().map(|check| check.exported).collect(),
    }
}

fn aggregate_expectations(verdicts: &[ExpectationVerdict]) -> DoneResult {
    if verdicts
        .iter()
        .any(|verdict| verdict.result == VerdictResult::NotSatisfied)
    {
        DoneResult::NotSatisfied
    } else if verdicts
        .iter()
        .any(|verdict| verdict.result != VerdictResult::Satisfied)
    {
        DoneResult::Unknown
    } else {
        DoneResult::Satisfied
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumericState {
    Present,
    Missing,
    Ambiguous,
}

struct EvaluatedNumericCheck {
    exported: NumericCheck,
    state: NumericState,
}

fn expectation_numeric_checks(
    expectation: &Expectation,
    observation: &Observation,
    values: &Values<'_>,
) -> Vec<EvaluatedNumericCheck> {
    let numeric_claim = values.mask_sensitive(&expectation.claim);
    let scoped_literals = expectation
        .exact_literals
        .iter()
        .filter(|exact| exact.within_text.is_some())
        .map(|exact| exact.literal.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut requested = BTreeMap::new();
    for literal in numeric_literals(&numeric_claim) {
        if !scoped_literals.contains(literal) {
            requested.insert((literal.to_owned(), None), ());
        }
    }
    for exact in &expectation.exact_literals {
        requested.insert((exact.literal.clone(), exact.within_text.clone()), ());
    }
    requested
        .into_iter()
        .map(|((literal, within_text), ())| {
            let state = numeric_check_state(observation, &literal, within_text.as_deref());
            EvaluatedNumericCheck {
                exported: NumericCheck {
                    literal,
                    present: state == NumericState::Present,
                    within_text,
                },
                state,
            }
        })
        .collect()
}

fn numeric_check_state(
    observation: &Observation,
    literal: &str,
    within_text: Option<&str>,
) -> NumericState {
    let Some(scope) = within_text else {
        return if observation_contains_numeric_literal(observation, literal) {
            NumericState::Present
        } else {
            NumericState::Missing
        };
    };
    let containers = text_containers(observation)
        .filter(|container| container.contains(scope))
        .collect::<Vec<_>>();
    if containers.len() != 1 {
        NumericState::Ambiguous
    } else if numeric_literal_present(containers[0], literal) {
        NumericState::Present
    } else {
        NumericState::Missing
    }
}

fn text_containers(observation: &Observation) -> impl Iterator<Item = &str> {
    observation
        .visible_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .chain(observation.dialog_texts.values().map(String::as_str))
        .chain(observation.elements.iter().flat_map(|element| {
            [element.name.as_str(), element.value.as_str()]
                .into_iter()
                .filter(|text| !text.is_empty())
        }))
}

pub fn check_natural_done(condition: &str, observation: &Observation, noul: f64) -> DoneResult {
    if noul <= 0.20 {
        return DoneResult::NotSatisfied;
    }
    if noul < 0.80 {
        return DoneResult::Unknown;
    }
    if natural_numeric_literals_satisfied(condition, observation) {
        DoneResult::Satisfied
    } else {
        DoneResult::NotSatisfied
    }
}

pub fn natural_numeric_literals_satisfied(condition: &str, observation: &Observation) -> bool {
    numeric_literals(condition)
        .iter()
        .all(|literal| observation_contains_numeric_literal(observation, literal))
}

pub fn check_done(
    assertions: &[Assertion],
    observation: &Observation,
    values: &BTreeMap<String, JobValue>,
) -> DoneResult {
    let mut unknown = false;
    for assertion in assertions {
        match check_assertion(assertion, observation, values) {
            DoneResult::NotSatisfied => return DoneResult::NotSatisfied,
            DoneResult::Unknown => unknown = true,
            DoneResult::Satisfied => {}
        }
    }
    if unknown {
        DoneResult::Unknown
    } else {
        DoneResult::Satisfied
    }
}

fn check_assertion(
    assertion: &Assertion,
    observation: &Observation,
    values: &BTreeMap<String, JobValue>,
) -> DoneResult {
    match assertion {
        Assertion::TextVisible(wanted) => text_visible(observation, wanted),
        Assertion::TextAbsent(wanted) => text_absent(observation, wanted),
        Assertion::FieldNonempty(field) => field_nonempty(observation, field),
        Assertion::FieldEqualsValue(field) => field_equals(observation, field, values),
        Assertion::DialogOpen(dialog) => dialog_open(observation, &dialog.dialog_open),
        Assertion::DialogClosed(dialog) => dialog_closed(observation, &dialog.dialog_closed),
        Assertion::UrlContains(url) => truth(observation.url.contains(&url.url_contains)),
    }
}

fn text_visible(observation: &Observation, wanted: &manuvra_contract::TextVisible) -> DoneResult {
    let Some(text) = scoped_text(observation, wanted.scope.as_ref()) else {
        return DoneResult::Unknown;
    };
    if text.contains(&wanted.text_visible) {
        DoneResult::Satisfied
    } else if scope_complete(observation, wanted.scope.as_ref()) {
        DoneResult::NotSatisfied
    } else {
        DoneResult::Unknown
    }
}

fn text_absent(observation: &Observation, wanted: &manuvra_contract::TextAbsent) -> DoneResult {
    let Some(text) = scoped_text(observation, wanted.scope.as_ref()) else {
        return DoneResult::Unknown;
    };
    if text.contains(&wanted.text_absent) {
        DoneResult::NotSatisfied
    } else if scope_complete(observation, wanted.scope.as_ref()) {
        DoneResult::Satisfied
    } else {
        DoneResult::Unknown
    }
}

fn field_nonempty(
    observation: &Observation,
    field: &manuvra_contract::FieldNonempty,
) -> DoneResult {
    field_match(
        observation,
        &field.field,
        field.dialog.as_deref(),
        field.role.as_deref(),
    )
    .map_or(DoneResult::Unknown, |element| {
        truth(!element.value.is_empty())
    })
}

fn field_equals(
    observation: &Observation,
    field: &manuvra_contract::FieldEqualsValue,
    values: &BTreeMap<String, JobValue>,
) -> DoneResult {
    let Some(element) = field_match(
        observation,
        &field.field,
        field.dialog.as_deref(),
        field.role.as_deref(),
    ) else {
        return DoneResult::Unknown;
    };
    let Some(value) = values.get(&field.equals_value) else {
        return DoneResult::Unknown;
    };
    truth(value_renderings(value).any(|wanted| element.value == wanted))
}

fn dialog_closed(observation: &Observation, wanted: &str) -> DoneResult {
    match matching_count(&observation.dialogs, wanted) {
        0 if viewport_complete(observation) => DoneResult::Satisfied,
        0 => DoneResult::Unknown,
        1 => DoneResult::NotSatisfied,
        _ => DoneResult::Unknown,
    }
}

fn dialog_open(observation: &Observation, wanted: &str) -> DoneResult {
    match matching_count(&observation.dialogs, wanted) {
        0 if viewport_complete(observation) => DoneResult::NotSatisfied,
        0 => DoneResult::Unknown,
        1 => DoneResult::Satisfied,
        _ => DoneResult::Unknown,
    }
}

fn scoped_text<'a>(
    observation: &'a Observation,
    scope: Option<&AssertionScope>,
) -> Option<&'a str> {
    match scope {
        None | Some(AssertionScope::Viewport(_)) => Some(&observation.visible_text),
        Some(AssertionScope::Dialog(dialog)) => unique_dialog_text(observation, &dialog.dialog),
    }
}

fn unique_dialog_text<'a>(observation: &'a Observation, wanted: &str) -> Option<&'a str> {
    if matching_count(&observation.dialogs, wanted) != 1 {
        return None;
    }
    let mut matches = observation
        .dialog_texts
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case(wanted));
    let (_, text) = matches.next()?;
    matches.next().is_none().then_some(text.as_str())
}

fn scope_complete(observation: &Observation, scope: Option<&AssertionScope>) -> bool {
    match scope {
        None | Some(AssertionScope::Viewport(_)) => viewport_complete(observation),
        Some(AssertionScope::Dialog(dialog)) => {
            viewport_complete(observation)
                && unique_dialog_text(observation, &dialog.dialog).is_some()
        }
    }
}

fn viewport_complete(observation: &Observation) -> bool {
    observation.coverage.viewport_complete
        && observation.coverage.open_shadow_roots
        && observation.coverage.slots
        && observation.coverage.same_origin_frames
        && observation.coverage.gaps.is_empty()
}

fn field_match<'a>(
    observation: &'a Observation,
    name: &str,
    dialog: Option<&str>,
    role: Option<&str>,
) -> Option<&'a Element> {
    let mut matches = observation.elements.iter().filter(|element| {
        element.name.eq_ignore_ascii_case(name)
            && dialog.is_none_or(|wanted| {
                element
                    .in_dialog
                    .as_deref()
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(wanted))
            })
            && role.is_none_or(|wanted| element.role.eq_ignore_ascii_case(wanted))
    });
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

fn matching_count(items: &[String], wanted: &str) -> usize {
    items
        .iter()
        .filter(|item| item.eq_ignore_ascii_case(wanted))
        .count()
}

fn value_renderings(value: &JobValue) -> impl Iterator<Item = &str> {
    std::iter::once(value.value.as_str()).chain(value.formats.iter().flat_map(|formats| {
        [formats.iso.as_deref(), formats.display.as_deref()]
            .into_iter()
            .flatten()
    }))
}

fn truth(value: bool) -> DoneResult {
    if value {
        DoneResult::Satisfied
    } else {
        DoneResult::NotSatisfied
    }
}

fn numeric_literals(text: &str) -> Vec<&str> {
    let mut literals = Vec::new();
    let mut cursor = 0;
    while let Some((literal, next)) = next_numeric_literal(text, cursor) {
        literals.push(literal);
        cursor = next;
    }
    literals
}

fn next_numeric_literal(text: &str, from: usize) -> Option<(&str, usize)> {
    let bytes = text.as_bytes();
    let start = (from..bytes.len()).find(|index| numeric_starts_at(bytes, *index))?;
    let integer_start = start + usize::from(matches!(bytes[start], b'+' | b'-'));
    let integer_end = digits_end(bytes, integer_start);
    let end = if integer_end + 1 < bytes.len()
        && bytes[integer_end] == b'.'
        && bytes[integer_end + 1].is_ascii_digit()
    {
        digits_end(bytes, integer_end + 1)
    } else {
        integer_end
    };
    Some((&text[start..end], end))
}

fn numeric_starts_at(text: &[u8], index: usize) -> bool {
    text[index].is_ascii_digit()
        || (matches!(text[index], b'+' | b'-')
            && text.get(index + 1).is_some_and(u8::is_ascii_digit))
}

fn digits_end(text: &[u8], mut index: usize) -> usize {
    while text.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    index
}

fn observation_contains_numeric_literal(observation: &Observation, literal: &str) -> bool {
    numeric_literal_present(&observation.visible_text, literal)
        || observation
            .elements
            .iter()
            .any(|element| numeric_literal_present(&element.value, literal))
}

fn numeric_literal_present(text: &str, literal: &str) -> bool {
    text.match_indices(literal).any(|(start, _)| {
        let end = start + literal.len();
        numeric_start_boundary(text.as_bytes(), start) && numeric_end_boundary(text.as_bytes(), end)
    })
}

fn numeric_start_boundary(text: &[u8], index: usize) -> bool {
    index == 0 || !matches!(text[index - 1], b'0'..=b'9' | b'.' | b'+' | b'-')
}

fn numeric_end_boundary(text: &[u8], index: usize) -> bool {
    index == text.len() || !matches!(text[index], b'0'..=b'9' | b'.' | b'+' | b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use manuvra_chrome::{Coverage, Rect, ViewportState};
    use manuvra_contract::{
        DialogClosed, DialogOpen, FieldEqualsValue, FieldNonempty, RequiredTrue, TextAbsent,
        TextVisible, UrlContains,
    };
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::time::Instant;

    struct CapturingExpectationEvaluator(Mutex<Option<Value>>);

    impl Evaluator for CapturingExpectationEvaluator {
        fn evaluate(
            &self,
            request: &Value,
            _deadline: Instant,
        ) -> Result<manuvra_jev::Evaluation, JevError> {
            *self.0.lock().unwrap() = Some(request.clone());
            let answers = request["questions"]
                .as_object()
                .unwrap()
                .keys()
                .map(|id| (id.clone(), Answer::Noul { noul: 0.95 }))
                .collect();
            Ok(manuvra_jev::Evaluation {
                answers,
                usage: BTreeMap::new(),
                request_id: Some("verification-capture".into()),
                model: "jev-1.13.0".into(),
            })
        }
    }

    fn observation() -> Observation {
        Observation {
            document_id: "d".into(),
            url: "http://example.test/saved".into(),
            route: "/saved".into(),
            title: "Saved".into(),
            dialogs: vec!["Create account".into()],
            focused: Some(1),
            visible_text: "Saved Create account".into(),
            covered_text: "Hidden background".into(),
            dialog_texts: BTreeMap::from([("Create account".into(), "Account name Saved".into())]),
            elements: vec![Element {
                index: 1,
                node_id: 1,
                context: "main".into(),
                role: "textbox".into(),
                name: "Account name".into(),
                input_type: Some("text".into()),
                value: "Wallet".into(),
                checked: None,
                selected: None,
                expanded: None,
                disabled: false,
                in_dialog: Some("Create account".into()),
                operations: vec!["TYPE_TEXT".into()],
                rect: Rect {
                    x: 1.,
                    y: 1.,
                    width: 10.,
                    height: 10.,
                },
            }],
            viewport: ViewportState {
                width: 800,
                height: 600,
                scroll_x: 0.,
                scroll_y: 0.,
                document_height: 600.,
            },
            coverage: Coverage::default(),
        }
    }

    fn values() -> BTreeMap<String, JobValue> {
        BTreeMap::from([(
            "name".into(),
            JobValue {
                value: "Wallet".into(),
                description: "name".into(),
                formats: None,
                secret: false,
            },
        )])
    }

    fn job_without_values() -> manuvra_contract::Job {
        manuvra_contract::Job::parse(
            serde_json::to_vec(&json!({
                "schema_version":1,
                "target":{"kind":"browser","url":"http://example.test"},
                "context":{"journey":"verify","revision":"r","environment":"e","actor":"a","authority":"a"},
                "steps":[{"id":"ready","goal":"observe","done_when":[{"url_contains":"example.test"}]}]
            }))
            .unwrap()
            .as_slice(),
        )
        .unwrap()
    }

    #[test]
    fn natural_done_bands_are_inclusive_at_the_accepted_boundaries() {
        let observation = observation();
        for (noul, expected) in [
            (0.19, DoneResult::NotSatisfied),
            (0.20, DoneResult::NotSatisfied),
            (0.21, DoneResult::Unknown),
            (0.79, DoneResult::Unknown),
            (0.80, DoneResult::Satisfied),
            (0.81, DoneResult::Satisfied),
        ] {
            assert_eq!(
                check_natural_done("The saved account is visible", &observation, noul),
                expected,
                "Noul {noul}"
            );
        }
    }

    #[test]
    fn natural_done_requires_exact_numeric_literals_before_advancing() {
        let mut observation = observation();
        observation.visible_text = "Balance 112.34".into();
        assert_eq!(
            check_natural_done("The balance is 12.34", &observation, 0.95),
            DoneResult::NotSatisfied
        );
        observation.elements[0].value = "12.34".into();
        assert_eq!(
            check_natural_done("The balance is 12.34", &observation, 0.95),
            DoneResult::Satisfied
        );
        assert_eq!(
            check_natural_done("The range is +7 through -12.34", &observation, 0.95),
            DoneResult::NotSatisfied
        );
        observation.visible_text = "Range +7 through -12.34".into();
        assert_eq!(
            check_natural_done("The range is +7 through -12.34", &observation, 0.95),
            DoneResult::Satisfied
        );
    }

    #[test]
    fn numeric_literal_extraction_preserves_signs_decimals_and_integers() {
        assert_eq!(
            numeric_literals("No number; then -12.34, +7 and 0."),
            ["-12.34", "+7", "0"]
        );
        assert!(numeric_literals("A sign + without digits").is_empty());
    }

    #[test]
    fn expectation_bands_and_numeric_checks_are_code_owned() {
        let job = job_without_values();
        let values = Values::new(&job);
        let mut observed = observation();
        observed.visible_text = "Wallet\nBalance 12.34\nDelta -7 and +2".into();
        let expectation = Expectation {
            id: "balance".into(),
            claim: "The balance is 12.34 and delta is -7 with +2 entries.".into(),
            exact_literals: Vec::new(),
        };
        for (noul, result) in [
            (0.19, VerdictResult::NotSatisfied),
            (0.20, VerdictResult::NotSatisfied),
            (0.21, VerdictResult::Unresolved),
            (0.79, VerdictResult::Unresolved),
            (0.80, VerdictResult::Satisfied),
            (0.81, VerdictResult::Satisfied),
        ] {
            assert_eq!(
                expectation_verdict(&expectation, &observed, &values, noul).result,
                result
            );
        }
        observed.visible_text = "Balance 112.34, delta -70, +20 entries".into();
        let verdict = expectation_verdict(&expectation, &observed, &values, 0.95);
        assert_eq!(verdict.result, VerdictResult::NotSatisfied);
        assert!(verdict.numeric_checks.iter().all(|check| !check.present));
    }

    #[test]
    fn missing_literal_overrides_high_noul_and_low_noul_overrides_present_literal() {
        let job = job_without_values();
        let values = Values::new(&job);
        let mut observed = observation();
        observed.visible_text = "Balance 12.34".into();
        let expectation = Expectation {
            id: "balance".into(),
            claim: "The balance is 12.34.".into(),
            exact_literals: Vec::new(),
        };
        assert_eq!(
            expectation_verdict(&expectation, &observed, &values, 0.10).result,
            VerdictResult::NotSatisfied
        );
        observed.visible_text = "No balance is shown".into();
        assert_eq!(
            expectation_verdict(&expectation, &observed, &values, 0.95).result,
            VerdictResult::NotSatisfied
        );
    }

    #[test]
    fn within_text_requires_exactly_one_container() {
        let job = job_without_values();
        let values = Values::new(&job);
        let mut observed = observation();
        let expectation = Expectation {
            id: "wallet".into(),
            claim: "The wallet exists.".into(),
            exact_literals: vec![manuvra_contract::ExactLiteral {
                literal: "12.34".into(),
                within_text: Some("Review wallet".into()),
            }],
        };
        observed.visible_text = "Review wallet balance 12.34".into();
        assert_eq!(
            expectation_verdict(&expectation, &observed, &values, 0.95).result,
            VerdictResult::Satisfied
        );
        observed.visible_text = "Other wallet balance 12.34".into();
        assert_eq!(
            expectation_verdict(&expectation, &observed, &values, 0.95).result,
            VerdictResult::Unresolved
        );
        observed.visible_text = "Review wallet balance 12.34\nReview wallet pending 12.34".into();
        assert_eq!(
            expectation_verdict(&expectation, &observed, &values, 0.95).result,
            VerdictResult::Unresolved
        );
        observed.visible_text = "Review wallet balance 112.34".into();
        assert_eq!(
            expectation_verdict(&expectation, &observed, &values, 0.95).result,
            VerdictResult::NotSatisfied
        );
        observed.visible_text = "Other wallet balance 12.34".into();
        assert_eq!(
            expectation_verdict(&expectation, &observed, &values, 0.10).result,
            VerdictResult::NotSatisfied,
            "a confidently false semantic judgment cannot be attested past an ambiguous scope"
        );
    }

    #[test]
    fn exact_literal_evaluation_is_independent_of_declaration_order() {
        let job = job_without_values();
        let values = Values::new(&job);
        let mut observed = observation();
        observed.visible_text = "Wallet balance 12.34\nReserve balance 12.34".into();
        let exact = |within_text: Option<&str>| manuvra_contract::ExactLiteral {
            literal: "12.34".into(),
            within_text: within_text.map(str::to_owned),
        };
        let expectation = |exact_literals| Expectation {
            id: "balance".into(),
            claim: "The final balance is 12.34.".into(),
            exact_literals,
        };
        let forward = expectation_numeric_checks(
            &expectation(vec![exact(None), exact(Some("Wallet"))]),
            &observed,
            &values,
        )
        .into_iter()
        .map(|check| check.exported)
        .collect::<Vec<_>>();
        let reverse = expectation_numeric_checks(
            &expectation(vec![exact(Some("Wallet")), exact(None)]),
            &observed,
            &values,
        )
        .into_iter()
        .map(|check| check.exported)
        .collect::<Vec<_>>();
        assert_eq!(forward, reverse);
        assert_eq!(forward.len(), 2);
    }

    #[test]
    fn verification_asks_one_noul_per_claim_without_exposing_secret_values() {
        let job = manuvra_contract::Job::parse(
            serde_json::to_vec(&json!({
                "schema_version":1,
                "target":{"kind":"browser","url":"http://example.test"},
                "context":{"journey":"verify","revision":"r","environment":"e","actor":"a","authority":"a"},
                "values":{"account_name":{"value":"classified-wallet-742","description":"Account name","secret":true}},
                "steps":[{"id":"ready","goal":"observe","done_when":[{"url_contains":"example.test"}]}],
                "expectations":[
                    {"id":"account","claim":"The classified-wallet-742 account exists."},
                    {"id":"balance","claim":"The classified-wallet-742 account has balance 12.34."}
                ]
            }))
            .unwrap()
            .as_slice(),
        )
        .unwrap();
        let mut observed = observation();
        observed.visible_text = "classified-wallet-742\n12.34".into();
        let evaluator = CapturingExpectationEvaluator(Mutex::new(None));
        let report = verify(
            &job.expectations,
            &observed,
            &Values::new(&job),
            &evaluator,
            Instant::now(),
        )
        .unwrap();
        let request = evaluator.0.lock().unwrap().clone().unwrap();
        let request_text = request.to_string();
        assert_eq!(request["questions"].as_object().unwrap().len(), 2);
        assert!(!request_text.contains("classified-wallet-742"));
        assert!(request_text.contains("<value:account_name>"));
        assert_eq!(report.outcome, DoneResult::Satisfied);
        assert!(report.verdicts.iter().all(|verdict| {
            verdict
                .numeric_checks
                .iter()
                .all(|check| check.literal != "742")
        }));
    }

    #[test]
    fn all_seven_assertion_forms_are_checked() {
        let assertions = vec![
            Assertion::TextVisible(TextVisible {
                text_visible: "Saved".into(),
                scope: None,
            }),
            Assertion::TextAbsent(TextAbsent {
                text_absent: "Missing".into(),
                scope: None,
            }),
            Assertion::FieldNonempty(FieldNonempty {
                field: "Account name".into(),
                nonempty: RequiredTrue,
                dialog: None,
                role: None,
            }),
            Assertion::FieldEqualsValue(FieldEqualsValue {
                field: "Account name".into(),
                equals_value: "name".into(),
                dialog: Some("Create account".into()),
                role: Some("textbox".into()),
            }),
            Assertion::DialogOpen(DialogOpen {
                dialog_open: "create ACCOUNT".into(),
            }),
            Assertion::DialogClosed(DialogClosed {
                dialog_closed: "Other".into(),
            }),
            Assertion::UrlContains(UrlContains {
                url_contains: "/saved".into(),
            }),
        ];
        assert_eq!(
            check_done(&assertions, &observation(), &values()),
            DoneResult::Satisfied
        );
    }

    #[test]
    fn ambiguity_is_unknown_and_covered_text_is_not_visible() {
        let mut observed = observation();
        observed.elements.push(observed.elements[0].clone());
        let field = Assertion::FieldNonempty(FieldNonempty {
            field: "Account name".into(),
            nonempty: RequiredTrue,
            dialog: None,
            role: None,
        });
        assert_eq!(
            check_done(&[field], &observed, &values()),
            DoneResult::Unknown
        );
        let covered = Assertion::TextVisible(TextVisible {
            text_visible: "Hidden background".into(),
            scope: None,
        });
        assert_eq!(
            check_done(&[covered], &observed, &values()),
            DoneResult::NotSatisfied
        );
    }

    #[test]
    fn negative_requires_complete_coverage() {
        let mut observed = observation();
        let absent = Assertion::TextAbsent(TextAbsent {
            text_absent: "Missing".into(),
            scope: None,
        });
        observed.coverage.gaps.push("cross_origin_frame".into());
        observed.coverage.same_origin_frames = false;
        assert_eq!(
            check_done(&[absent], &observed, &values()),
            DoneResult::Unknown
        );
        let missing_dialog = Assertion::DialogOpen(DialogOpen {
            dialog_open: "Missing".into(),
        });
        assert_eq!(
            check_done(&[missing_dialog], &observed, &values()),
            DoneResult::Unknown
        );
    }

    #[test]
    fn duplicate_dialog_titles_make_scoped_text_unknown() {
        let mut observed = observation();
        observed.dialogs.push("create ACCOUNT".into());
        let scoped = AssertionScope::Dialog(manuvra_contract::DialogScope {
            dialog: "Create account".into(),
        });
        let visible = Assertion::TextVisible(TextVisible {
            text_visible: "Saved".into(),
            scope: Some(scoped),
        });
        assert_eq!(
            check_done(&[visible], &observed, &values()),
            DoneResult::Unknown
        );
    }

    #[test]
    fn truncated_viewport_and_dialog_text_cannot_prove_absence() {
        let mut observed = observation();
        observed.coverage.viewport_complete = false;
        observed.coverage.gaps.extend([
            "visible_text_truncated".into(),
            "dialog_text_truncated".into(),
        ]);
        let viewport = Assertion::TextVisible(TextVisible {
            text_visible: "Beyond the retained prefix".into(),
            scope: None,
        });
        let dialog = Assertion::TextAbsent(TextAbsent {
            text_absent: "Beyond the retained dialog prefix".into(),
            scope: Some(AssertionScope::Dialog(manuvra_contract::DialogScope {
                dialog: "Create account".into(),
            })),
        });
        assert_eq!(
            check_done(&[viewport], &observed, &values()),
            DoneResult::Unknown
        );
        assert_eq!(
            check_done(&[dialog], &observed, &values()),
            DoneResult::Unknown
        );
    }

    #[test]
    fn recorded_money_snapshots_cover_structured_resolution() {
        let dialog: Observation = serde_json::from_str(include_str!(
            "../../../tests/fixtures/recorded-money-dialog.json"
        ))
        .unwrap();
        let final_page: Observation = serde_json::from_str(include_str!(
            "../../../tests/fixtures/recorded-money-final.json"
        ))
        .unwrap();
        let recorded_values = BTreeMap::from([(
            "account_name".into(),
            JobValue {
                value: "Unit seed wallet".into(),
                description: "recorded account name".into(),
                formats: None,
                secret: false,
            },
        )]);
        let dialog_assertions = vec![
            Assertion::DialogOpen(DialogOpen {
                dialog_open: "Create account".into(),
            }),
            Assertion::TextVisible(TextVisible {
                text_visible: "Opening balance".into(),
                scope: Some(AssertionScope::Dialog(manuvra_contract::DialogScope {
                    dialog: "Create account".into(),
                })),
            }),
            Assertion::TextAbsent(TextAbsent {
                text_absent: "Review wallet".into(),
                scope: Some(AssertionScope::Viewport(
                    manuvra_contract::ViewportScope::Viewport,
                )),
            }),
            Assertion::FieldNonempty(FieldNonempty {
                field: "account NAME".into(),
                nonempty: RequiredTrue,
                dialog: Some("Create account".into()),
                role: Some("textbox".into()),
            }),
            Assertion::FieldEqualsValue(FieldEqualsValue {
                field: "Account name".into(),
                equals_value: "account_name".into(),
                dialog: Some("Create account".into()),
                role: Some("textbox".into()),
            }),
            Assertion::UrlContains(UrlContains {
                url_contains: "127.0.0.1:4351".into(),
            }),
        ];
        assert_eq!(
            check_done(&dialog_assertions, &dialog, &recorded_values),
            DoneResult::Satisfied
        );
        let final_assertions = vec![
            Assertion::DialogClosed(DialogClosed {
                dialog_closed: "Create account".into(),
            }),
            Assertion::TextVisible(TextVisible {
                text_visible: "Review wallet".into(),
                scope: None,
            }),
        ];
        assert_eq!(
            check_done(&final_assertions, &final_page, &recorded_values),
            DoneResult::Satisfied
        );
    }
}
