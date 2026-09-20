use manuvra_chrome::{Element, Observation};
use manuvra_contract::{Assertion, AssertionScope, JobValue};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoneResult {
    Satisfied,
    NotSatisfied,
    Unknown,
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

#[cfg(test)]
mod tests {
    use super::*;
    use manuvra_chrome::{Coverage, Rect, ViewportState};
    use manuvra_contract::{
        DialogClosed, DialogOpen, FieldEqualsValue, FieldNonempty, RequiredTrue, TextAbsent,
        TextVisible, UrlContains,
    };
    use std::collections::BTreeMap;

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
