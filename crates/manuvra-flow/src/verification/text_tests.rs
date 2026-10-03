use super::*;
use crate::run::{NoProvider, observed as legacy_observed};

fn inventory(accessible: &str, hidden: &str, complete: bool) -> Value {
    json!({"accessible":accessible,"painted_aria_hidden":hidden,"complete":complete})
}

fn page(accessible: &str, hidden: &str, complete: bool) -> Observation {
    let mut wire = serde_json::to_value(legacy_observed(accessible)).unwrap();
    wire["covered_text"] = json!(format!("{hidden}\nInert amount"));
    wire["painted_text"] = json!({"viewport":inventory(accessible,hidden,complete),"dialogs":{}});
    serde_json::from_value(wire).unwrap()
}

fn check(wire: Value, page: &Observation) -> (DoneResult, Value) {
    let assertion: Assertion = serde_json::from_value(wire).expect("accepted text assertion");
    let report = evaluate_assertions(&[assertion], page, &BTreeMap::new());
    (
        report.outcome,
        serde_json::to_value(report.assertion_checks).unwrap()[0]["text"].clone(),
    )
}

#[test]
fn text_channels_share_presence_absence_and_accessible_precedence() {
    for (accessible, hidden, found, channel) in [
        ("$0.00", "", true, json!("accessible")),
        ("", "$0.00", true, json!("painted_aria_hidden")),
        ("$0.00", "$0.00", true, json!("accessible")),
        ("", "", false, Value::Null),
    ] {
        let page = page(accessible, hidden, true);
        for (kind, presence) in [("text_visible", true), ("text_absent", false)] {
            for include in [false, true] {
                let (outcome, evidence) =
                    check(json!({kind:"$0.00","include_aria_hidden":include}), &page);
                let matched = if include {
                    found
                } else {
                    !accessible.is_empty()
                };
                assert_eq!(
                    outcome,
                    if matched == presence {
                        DoneResult::Satisfied
                    } else {
                        DoneResult::NotSatisfied
                    }
                );
                assert_eq!(
                    evidence["searched_channels"],
                    if include {
                        json!(["accessible", "painted_aria_hidden"])
                    } else {
                        json!(["accessible"])
                    }
                );
                assert_eq!(
                    evidence["matched_channel"],
                    if matched {
                        if include {
                            channel.clone()
                        } else {
                            json!("accessible")
                        }
                    } else {
                        Value::Null
                    }
                );
                assert_eq!(evidence["assertion"][kind], "$0.00");
            }
        }
    }
}

#[test]
fn text_dialog_opt_in_is_isolated_while_legacy_inner_text_stays_unchanged() {
    let mut wire = serde_json::to_value(page("Outside amount", "$0.00", true)).unwrap();
    wire["dialogs"] = json!(["Month details", "Other details"]);
    wire["dialog_texts"] = json!({"Month details":"Inert amount","Other details":"$0.00"});
    wire["painted_text"]["dialogs"] = json!({
        "Month details":inventory("Inside amount", "$2.00", true),
        "Other details":inventory("", "$0.00", true)
    });
    let page: Observation = serde_json::from_value(wire.clone()).unwrap();
    for (text, include, expected, channel) in [
        ("$0.00", true, DoneResult::NotSatisfied, Value::Null),
        (
            "$2.00",
            true,
            DoneResult::Satisfied,
            json!("painted_aria_hidden"),
        ),
        (
            "Inert amount",
            false,
            DoneResult::Satisfied,
            json!("dialog_text"),
        ),
        ("Inert amount", true, DoneResult::NotSatisfied, Value::Null),
    ] {
        let (actual, evidence) = check(
            json!({"text_visible":text,"scope":{"dialog":"month DETAILS"},"include_aria_hidden":include}),
            &page,
        );
        assert_eq!(actual, expected);
        assert_eq!(evidence["matched_channel"], channel);
    }
    for titles in [json!([]), json!(["Month details", "MONTH DETAILS"])] {
        wire["dialogs"] = titles;
        let page: Observation = serde_json::from_value(wire.clone()).unwrap();
        for kind in ["text_visible", "text_absent"] {
            let (outcome, evidence) = check(
                json!({kind:"$2.00","scope":{"dialog":"Month details"},"include_aria_hidden":true}),
                &page,
            );
            assert_eq!(outcome, DoneResult::Unknown);
            assert_eq!(evidence["reason"], "ambiguous_or_missing_scope");
        }
    }
}

#[test]
fn painted_text_incompleteness_cannot_prove_a_missing_match_or_change_defaults() {
    let observed = page("Ready", "$0.00", false);
    for (kind, text, wanted) in [
        ("text_visible", "$0.00", DoneResult::Satisfied),
        ("text_absent", "$0.00", DoneResult::NotSatisfied),
        ("text_visible", "Missing", DoneResult::Unknown),
        ("text_absent", "Missing", DoneResult::Unknown),
    ] {
        assert_eq!(
            check(json!({kind:text,"include_aria_hidden":true}), &observed).0,
            wanted
        );
    }
    assert_eq!(
        check(json!({"text_absent":"Missing"}), &observed).0,
        DoneResult::Satisfied
    );
    let mut wire = serde_json::to_value(page("Ready", "", true)).unwrap();
    wire["coverage"]["gaps"] = json!(["cross_origin_frame"]);
    wire["coverage"]["same_origin_frames"] = json!(false);
    let observed = serde_json::from_value(wire).unwrap();
    assert_eq!(
        check(
            json!({"text_absent":"Missing","include_aria_hidden":true}),
            &observed
        )
        .0,
        DoneResult::Unknown
    );
    let mut old = serde_json::to_value(legacy_observed("Ready")).unwrap();
    old.as_object_mut().unwrap().remove("painted_text");
    let old = serde_json::from_value(old).unwrap();
    assert_eq!(
        check(
            json!({"text_absent":"Missing","include_aria_hidden":true}),
            &old
        )
        .0,
        DoneResult::Unknown
    );
}

#[test]
fn text_matching_is_case_sensitive_substring_and_never_crosses_channels() {
    let observed = page("September", "Amount $0.00", true);
    for (text, outcome) in [
        ("$0.0", DoneResult::Satisfied),
        ("amount", DoneResult::NotSatisfied),
        ("September\nAmount", DoneResult::NotSatisfied),
        ("Inert amount", DoneResult::NotSatisfied),
    ] {
        assert_eq!(
            check(
                json!({"text_visible":text,"include_aria_hidden":true}),
                &observed
            )
            .0,
            outcome
        );
    }
}

#[test]
fn false_painted_text_checks_short_circuit_natural_verification_without_a_provider() {
    let job: manuvra_contract::Job = serde_json::from_value(json!({
        "schema_version":1,"target":{"kind":"browser","url":"http://example.test/"},
        "context":{"journey":"Text","revision":"fixture","environment":"synthetic","actor":"owner","authority":"observe"},
        "steps":[{"id":"ready","goal":"Observe Ready","done_when":[{"text_visible":"Ready"}]}],
        "expectations":[{"id":"natural","claim":"Ready exists"},{"id":"missing","assertions":[{"text_visible":"Missing","include_aria_hidden":true}]}]
    })).expect("accepted opt-in job");
    let report = verify(
        &job.expectations,
        &page("Ready", "$0.00", true),
        &Values::new(&job),
        &NoProvider,
        Instant::now(),
    )
    .unwrap();
    assert_eq!(report.outcome, DoneResult::NotSatisfied);
    assert_eq!(report.verdicts[0].result, VerdictResult::NotRun);
    assert_eq!(report.record["provider"], Value::Null);
}
