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
        let (actual, evidence) = check(json!({kind:text,"include_aria_hidden":true}), &observed);
        assert_eq!(actual, wanted);
        assert_eq!(
            evidence["reason"],
            if wanted == DoneResult::Unknown {
                json!("incomplete_coverage")
            } else {
                Value::Null
            }
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

#[test]
fn painted_text_requires_traversal_but_ignores_legacy_channel_truncation() {
    let baseline = serde_json::to_value(page("Ready", "$0.00", true)).unwrap();
    for (field, value) in [
        ("viewport_complete", json!(false)),
        ("open_shadow_roots", json!(false)),
        ("slots", json!(false)),
        ("same_origin_frames", json!(false)),
        ("gaps", json!(["closed_shadow_root"])),
        ("gaps", json!(["canvas"])),
        ("gaps", json!(["generated_content"])),
        (
            "gaps",
            json!(["covered_text_truncated", "cross_origin_frame"]),
        ),
    ] {
        let mut wire = baseline.clone();
        wire["coverage"][field] = value;
        let observed = serde_json::from_value(wire).unwrap();
        for kind in ["text_visible", "text_absent"] {
            assert_eq!(
                check(
                    json!({kind:"Missing","include_aria_hidden":true}),
                    &observed
                )
                .0,
                DoneResult::Unknown
            );
        }
        assert_eq!(
            check(
                json!({"text_visible":"$0.00","include_aria_hidden":true}),
                &observed
            )
            .0,
            DoneResult::Satisfied
        );
    }
    for gap in [
        "visible_text_truncated",
        "covered_text_truncated",
        "dialog_text_truncated",
    ] {
        let mut wire = baseline.clone();
        wire["coverage"]["viewport_complete"] = json!(false);
        wire["coverage"]["gaps"] = json!([gap]);
        let observed = serde_json::from_value(wire).unwrap();
        assert_eq!(
            check(
                json!({"text_absent":"Missing","include_aria_hidden":true,"scope":"viewport"}),
                &observed
            )
            .0,
            DoneResult::Satisfied
        );
        assert_eq!(
            check(json!({"text_absent":"Missing"}), &observed).0,
            DoneResult::Unknown
        );
    }
}

#[test]
fn unique_dialog_requires_a_unique_inventory_in_the_selected_observation() {
    let mut wire = serde_json::to_value(page("Ready", "$0.00", true)).unwrap();
    wire["dialogs"] = json!(["Details"]);
    wire["dialog_texts"] = json!({"Details":"Legacy amount"});
    for inventories in [
        json!({}),
        json!({"Details":inventory("", "$0.00", true),"DETAILS":inventory("", "$0.00", true)}),
    ] {
        wire["painted_text"]["dialogs"] = inventories;
        let observed = serde_json::from_value(wire.clone()).unwrap();
        let (outcome, evidence) = check(
            json!({"text_absent":"Missing","scope":{"dialog":"Details"},"include_aria_hidden":true}),
            &observed,
        );
        assert_eq!(outcome, DoneResult::Unknown);
        assert_eq!(evidence["reason"], "incomplete_coverage");
        assert_eq!(
            check(
                json!({"text_visible":"Legacy amount","scope":{"dialog":"Details"}}),
                &observed
            )
            .0,
            DoneResult::Satisfied
        );
    }
    wire["painted_text"]["viewport"]["complete"] = json!(false);
    wire["painted_text"]["dialogs"] = json!({"Details":inventory("", "$0.00", true)});
    let observed = serde_json::from_value(wire).unwrap();
    assert_eq!(
        check(
            json!({"text_absent":"Missing","scope":{"dialog":"Details"},"include_aria_hidden":true}),
            &observed
        )
        .0,
        DoneResult::Satisfied
    );
}

#[test]
fn painted_text_does_not_change_natural_provider_requests_or_numeric_proof() {
    use crate::run::{ScriptedProvider, Turn};

    let job: manuvra_contract::Job = serde_json::from_value(json!({
        "schema_version":1,"target":{"kind":"browser","url":"http://example.test/"},
        "context":{"journey":"Text","revision":"fixture","environment":"synthetic","actor":"owner","authority":"observe"},
        "steps":[{"id":"ready","goal":"Observe Ready","done_when":[{"text_visible":"Ready"}]}],
        "expectations":[{"id":"natural","claim":"Balance $0.00"}]
    })).unwrap();
    let mut baseline = page("Ready", "$0.00", true);
    let mut added = baseline.clone();
    baseline.painted_text = None;
    added.painted_text.as_mut().unwrap().viewport.accessible = "Balance $0.00".into();
    let provider = ScriptedProvider::new([Turn::verdict(0.95), Turn::verdict(0.95)]);
    for observed in [&baseline, &added] {
        let report = verify(
            &job.expectations,
            observed,
            &Values::new(&job),
            &provider,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(report.outcome, DoneResult::NotSatisfied);
        assert_eq!(report.verdicts[0].noul, Some(0.95));
        assert!(!report.verdicts[0].numeric_checks[0].present);
    }
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
}

#[test]
fn text_protocol_literals_do_not_make_redacted_checks_fail_the_leak_scan() {
    use crate::evidence::{Redactor, redacted_assertion_checks};

    let mut observed = page("Ready", "Amount", true);
    observed.dialogs = vec!["Details".into()];
    observed.dialog_texts = BTreeMap::from([("Details".into(), "Amount".into())]);
    let assertions: Vec<Assertion> = serde_json::from_value(json!([
        {"text_visible":"Amount","include_aria_hidden":true},
        {"text_visible":"Amount","scope":{"dialog":"Details"}},
        {"text_absent":"Amount","scope":{"dialog":"Missing"},"include_aria_hidden":true}
    ]))
    .unwrap();
    let checks = assertion_checks(&assertions, &observed);
    for literal in [
        "painted_text",
        "include_aria_hidden",
        "searched_channels",
        "matched_channel",
        "dialog_text",
        "ambiguous_or_missing_scope",
    ] {
        let mut job = crate::run::parse_job(json!({
            "schema_version":1,"target":{"kind":"browser","url":"http://example.test/"},
            "context":{"journey":"Text","revision":"fixture","environment":"synthetic","actor":"owner","authority":"observe"},
            "steps":[{"id":"ready","goal":"Observe Ready","done_when":[{"text_visible":"Ready"}]}]
        }));
        job.values.insert(
            "collision".into(),
            manuvra_contract::JobValue {
                value: literal.into(),
                description: "classified protocol collision".into(),
                formats: None,
                secret: true,
            },
        );
        let redactor = Redactor::for_job(&job).unwrap();
        let exported = json!({
            "painted_text":observed.painted_text,
            "assertion_checks":redacted_assertion_checks(&checks, &redactor)
        });
        assert!(exported.to_string().contains(literal), "{literal}");
        assert!(
            !redactor.contains_export_leak(exported.to_string().as_bytes()),
            "{literal}"
        );
    }
}
