use super::*;
use crate::run::{NoProvider, ScriptedProvider, Turn, observed, parse_job};

fn job(expectations: Value) -> manuvra_contract::Job {
    parse_job(json!({
        "schema_version":1,"target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
        "context":{"journey":"Color","revision":"fixture","environment":"synthetic","actor":"owner","authority":"observe"},
        "steps":[{"id":"ready","goal":"Observe Ready","done_when":[{"text_visible":"Ready"}]}],
        "expectations":expectations
    }))
}

fn color(text: &str, rgba: [u8; 4], node: u64) -> Value {
    json!({"node_id":node,"context":"main","text":text,"name":null,"role":null,
        "in_dialog":null,"container":null,"dialog_node_id":null,"container_node_id":null,
        "channel":"accessible","color":{"raw":"fixture computed color","rgba":rgba}})
}

fn page(colors: Vec<Value>) -> Observation {
    let mut value = serde_json::to_value(observed("Ready")).unwrap();
    value["colors"] = json!(colors);
    value["colors_complete"] = json!(true);
    value["color_scopes"] = json!([]);
    serde_json::from_value(value).unwrap()
}

#[test]
fn color_text_normalizes_requested_whitespace_but_preserves_case() {
    let observed = page(vec![color("Inherited amount", [185, 28, 28, 255], 1)]);
    for (text, outcome) in [
        ("  Inherited  amount  ", DoneResult::Satisfied),
        ("inherited amount", DoneResult::NotSatisfied),
    ] {
        assert_eq!(
            verify_color(
                json!({"target":{"text":text},"equals":"#b91c1c"}),
                &observed
            )
            .outcome,
            outcome
        );
    }
}

#[test]
fn color_text_uses_the_browser_unicode_whitespace_set() {
    for (requested, painted) in [
        ("Amount\u{85}line", "Amount\u{85}line"),
        ("Amount\u{feff}line", "Amount line"),
        (
            concat!(
                "Amount\t\n\u{b}\u{c}\r \u{a0}\u{1680}",
                "\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\u{2005}",
                "\u{2006}\u{2007}\u{2008}\u{2009}\u{200a}",
                "\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}line"
            ),
            "Amount line",
        ),
        ("Amount\u{180e}line", "Amount\u{180e}line"),
        ("Amount\u{200b}line", "Amount\u{200b}line"),
    ] {
        let observed = page(vec![color(painted, [185, 28, 28, 255], 1)]);
        assert_eq!(
            verify_color(
                json!({"target":{"text":requested},"equals":"#b91c1c"}),
                &observed
            )
            .outcome,
            DoneResult::Satisfied,
            "{requested:?}"
        );
    }
}

#[test]
fn color_names_ignore_case_and_enforce_the_requested_role() {
    let mut control = color("", [31, 41, 55, 255], 1);
    control["text"] = Value::Null;
    control["name"] = json!("Current month");
    control["role"] = json!("button");
    let observed = page(vec![control]);
    for (name, role, outcome) in [
        ("CURRENT MONTH", "button", DoneResult::Satisfied),
        ("Current month", "link", DoneResult::NotSatisfied),
    ] {
        assert_eq!(
            verify_color(
                json!({"target":{"name":name,"role":role},"equals":"#1f2937"}),
                &observed
            )
            .outcome,
            outcome
        );
    }
}

#[test]
fn color_false_conjunct_dominates_unknown_in_both_orders_and_skips_natural_claims() {
    let observed = page(vec![
        color("False", [31, 41, 55, 255], 1),
        color("Duplicate", [185, 28, 28, 255], 2),
        color("Duplicate", [185, 28, 28, 255], 3),
    ]);
    for order in [["False", "Duplicate"], ["Duplicate", "False"]] {
        let assertions =
            order.map(|text| json!({"color":{"target":{"text":text},"equals":"#b91c1c"}}));
        let job = job(
            json!([{"id":"both","assertions":assertions}, {"id":"natural","claim":"Ready exists"}]),
        );
        let report = verify(
            &job.expectations,
            &observed,
            &Values::new(&job),
            &NoProvider,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(report.outcome, DoneResult::NotSatisfied);
        assert_eq!(report.verdicts[0].result, VerdictResult::NotSatisfied);
        assert_eq!(report.verdicts[1].result, VerdictResult::NotRun);
    }
}

#[test]
fn color_ancestor_scopes_keep_their_identity_across_shadow_contexts() {
    let mut amount = color("Amount", [185, 28, 28, 255], 1);
    amount["context"] = json!("main/shadow:2");
    amount["dialog_node_id"] = json!(10);
    amount["container_node_id"] = json!(11);
    let mut snapshot = serde_json::to_value(page(vec![amount])).unwrap();
    snapshot["color_scopes"] = json!([
        {"node_id":10,"context":"main","name":"Details","kind":"dialog","dialog_node_id":null},
        {"node_id":11,"context":"main","name":"Checking","kind":"container","dialog_node_id":10}
    ]);
    assert_eq!(verify_color(json!({"target":{"text":"Amount","dialog":"Details","container":"Checking"},"equals":"#b91c1c"}), &serde_json::from_value(snapshot).unwrap()).outcome, DoneResult::Satisfied);
}

#[test]
fn color_unverifiable_paint_fails_closed_for_its_owner() {
    let mut amount = color("Amount", [185, 28, 28, 255], 1);
    amount["paint_complete"] = json!(false);
    let observed = page(vec![amount, color("Reference", [185, 28, 28, 255], 2)]);
    assert_eq!(
        verify_color(
            json!({"target":{"text":"Amount"},"equals":"#b91c1c"}),
            &observed
        )
        .outcome,
        DoneResult::Unknown
    );
    assert_eq!(
        verify_color(
            json!({"target":{"text":"Reference"},"equals":"#b91c1c"}),
            &observed
        )
        .outcome,
        DoneResult::Satisfied
    );
}

#[test]
fn color_missing_owner_dominates_an_unsupported_relative_owner() {
    let mut unsupported = color("Unsupported", [185, 28, 28, 255], 1);
    unsupported["color"]["rgba"] = Value::Null;
    let observed = page(vec![unsupported]);
    for (target, reference) in [("Missing", "Unsupported"), ("Unsupported", "Missing")] {
        let report = verify_color(
            json!({"target":{"text":target},"different_from":{"text":reference}}),
            &observed,
        );
        assert_eq!(report.outcome, DoneResult::NotSatisfied);
        assert_eq!(
            report.record["expectations"][0]["assertion_checks"][0]["color"]["reason"],
            "missing"
        );
    }
}

#[test]
fn transparent_duplicates_do_not_make_an_eligible_owner_ambiguous() {
    let observed = page(vec![
        color("Amount", [185, 28, 28, 255], 1),
        color("Amount", [185, 28, 28, 0], 2),
    ]);
    assert_eq!(
        verify_color(
            json!({"target":{"text":"Amount"},"equals":"#b91c1c"}),
            &observed
        )
        .outcome,
        DoneResult::Satisfied
    );
}

#[test]
fn color_scope_kind_and_case_distinguish_dialogs_from_containers() {
    let mut amount = color("Amount", [185, 28, 28, 255], 1);
    amount["container_node_id"] = json!(10);
    let mut snapshot = serde_json::to_value(page(vec![amount])).unwrap();
    snapshot["color_scopes"] = json!([
        {"node_id":10,"context":"main","name":"Shared","kind":"container","dialog_node_id":null},
        {"node_id":11,"context":"main","name":"Shared","kind":"dialog","dialog_node_id":null}
    ]);
    assert_eq!(
        verify_color(
            json!({"target":{"text":"Amount","container":"SHARED"},"equals":"#b91c1c"}),
            &serde_json::from_value(snapshot).unwrap()
        )
        .outcome,
        DoneResult::Satisfied
    );
}

fn final_color(body: Value) -> Value {
    json!({"id":"color","assertions":[{"color":body}]})
}

fn verify_color(body: Value, page: &Observation) -> VerificationReport {
    let job = job(json!([final_color(body)]));
    verify(
        &job.expectations,
        page,
        &Values::new(&job),
        &NoProvider,
        Instant::now(),
    )
    .expect("a color check needs no provider")
}

#[test]
fn color_equality_and_inequality_share_the_inclusive_channel_tolerance() {
    for (actual, tolerance, equality) in [
        ([185, 28, 28, 255], 0, true),
        ([186, 28, 28, 255], 1, true),
        ([187, 28, 28, 255], 1, false),
        ([185, 28, 28, 254], 0, false),
    ] {
        let observed = page(vec![
            color("Amount", actual, 1),
            color("Reference", [185, 28, 28, 255], 2),
        ]);
        for comparison in ["equals", "same_as", "different_from"] {
            let mut body = json!({"target":{"text":"Amount"},"tolerance":tolerance});
            body[comparison] = if comparison == "equals" {
                json!("#b91c1c")
            } else {
                json!({"text":"Reference"})
            };
            let expected = if comparison == "different_from" {
                !equality
            } else {
                equality
            };
            assert_eq!(
                verify_color(body, &observed).verdicts[0].result,
                if expected {
                    VerdictResult::Satisfied
                } else {
                    VerdictResult::NotSatisfied
                }
            );
        }
    }
}

#[test]
fn color_relative_reference_tracks_the_theme_and_retains_both_values() {
    for rgba in [[185, 28, 28, 255], [248, 113, 113, 255]] {
        let observed = page(vec![color("Amount", rgba, 1), color("Reference", rgba, 2)]);
        let report = verify_color(
            json!({"target":{"text":"Amount"},"same_as":{"text":"Reference"}}),
            &observed,
        );
        assert_eq!(report.outcome, DoneResult::Satisfied);
        let checks = &report.record["expectations"][0]["assertion_checks"][0]["color"];
        assert_eq!(checks["target"]["rgba"], json!(rgba));
        assert_eq!(checks["reference"]["rgba"], json!(rgba));
        assert_eq!(report.record["provider"], Value::Null);
    }
}

#[test]
fn color_unknown_missing_and_zero_alpha_never_satisfy_inequality() {
    for (colors, expected) in [
        (vec![], DoneResult::NotSatisfied),
        (
            vec![
                color("Amount", [185, 28, 28, 255], 1),
                color("Amount", [31, 41, 55, 255], 2),
            ],
            DoneResult::Unknown,
        ),
        (
            vec![color("Amount", [185, 28, 28, 0], 1)],
            DoneResult::NotSatisfied,
        ),
    ] {
        let mut colors = colors;
        colors.push(color("Reference", [31, 41, 55, 255], 10));
        let report = verify_color(
            json!({"target":{"text":"Amount"},"different_from":{"text":"Reference"}}),
            &page(colors),
        );
        assert_eq!(report.outcome, expected);
        assert!(
            report.record["expectations"][0]["assertion_checks"][0]["color"]["reason"].is_string()
        );
    }
    let mut unsupported = color("Amount", [185, 28, 28, 255], 1);
    unsupported["color"]["rgba"] = Value::Null;
    assert_eq!(
        verify_color(
            json!({"target":{"text":"Amount"},"equals":"#b91c1c"}),
            &page(vec![unsupported])
        )
        .outcome,
        DoneResult::Unknown
    );
    let mut incomplete =
        serde_json::to_value(page(vec![color("Amount", [185, 28, 28, 255], 1)])).unwrap();
    incomplete["colors_complete"] = json!(false);
    assert_eq!(
        verify_color(
            json!({"target":{"text":"Amount"},"equals":"#b91c1c"}),
            &serde_json::from_value(incomplete).unwrap()
        )
        .outcome,
        DoneResult::Unknown
    );
}

#[test]
fn color_names_and_painted_aria_hidden_text_bind_to_distinct_owners() {
    let mut named = color("", [31, 41, 55, 255], 1);
    named["text"] = Value::Null;
    named["name"] = json!("Current month");
    named["role"] = json!("button");
    let mut painted = color("-$9.00", [185, 28, 28, 255], 2);
    painted["channel"] = json!("painted_aria_hidden");
    let observed = page(vec![named, painted]);
    let report = verify_color(
        json!({"target":{"text":"-$9.00"},"equals":"#b91c1c"}),
        &observed,
    );
    assert_eq!(report.outcome, DoneResult::Satisfied);
    assert_eq!(
        report.record["expectations"][0]["assertion_checks"][0]["color"]["target"]["channel"],
        "painted_aria_hidden"
    );
    assert_eq!(
        verify_color(
            json!({"target":{"name":"CURRENT MONTH","role":"button"},"equals":"#b91c1c"}),
            &observed
        )
        .outcome,
        DoneResult::NotSatisfied
    );
}

#[test]
fn final_false_structured_check_short_circuits_provider_and_preserves_not_run_claims() {
    let job = job(
        json!([{"id":"natural","claim":"Ready is present"},final_color(json!({"target":{"text":"Amount"},"equals":"#b91c1c"}))]),
    );
    let observed = page(vec![color("Amount", [31, 41, 55, 255], 1)]);
    let report = verify(
        &job.expectations,
        &observed,
        &Values::new(&job),
        &NoProvider,
        Instant::now(),
    )
    .unwrap();
    assert_eq!(report.outcome, DoneResult::NotSatisfied);
    assert_eq!(report.verdicts[0].result, VerdictResult::NotRun);
    assert_eq!(report.verdicts[1].result, VerdictResult::NotSatisfied);
    assert_eq!(report.record["provider"], Value::Null);
}

#[test]
fn mixed_final_checks_preserve_order_and_only_judge_natural_claims() {
    let job = job(
        json!([final_color(json!({"target":{"text":"Amount"},"equals":"#b91c1c"})),{"id":"natural","claim":"Ready is present"},{"id":"existing","assertions":[{"text_visible":"Ready"}]}]),
    );
    let provider = ScriptedProvider::new([Turn::verdict(0.95)]);
    let observed = page(vec![color("Amount", [185, 28, 28, 255], 1)]);
    let report = verify(
        &job.expectations,
        &observed,
        &Values::new(&job),
        &provider,
        Instant::now(),
    )
    .unwrap();
    assert_eq!(report.outcome, DoneResult::Satisfied);
    assert_eq!(
        report
            .verdicts
            .iter()
            .map(|v| v.id.as_str())
            .collect::<Vec<_>>(),
        ["color", "natural", "existing"]
    );
    assert_eq!(provider.calls(), 1);
    assert_eq!(
        report.record["provider"]["request"]["questions"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    assert!(
        report.record["provider"]["request"]["questions"]
            .get("claim_0002")
            .is_some()
    );
    assert_eq!(report.verdicts[0].noul, None);
    assert_eq!(report.verdicts[1].noul, Some(0.95));
}

#[test]
fn color_scope_must_be_unique_even_when_only_one_owner_matches() {
    let mut amount = color("Amount", [185, 28, 28, 255], 1);
    amount["container"] = json!("Checking");
    amount["container_node_id"] = json!(11);
    let mut snapshot = serde_json::to_value(page(vec![amount])).unwrap();
    snapshot["color_scopes"] = json!([{ "node_id":11,"context":"main","name":"Checking","kind":"container","dialog_node_id":null}]);
    let target = json!({"target":{"text":"Amount","container":"Checking"},"equals":"#b91c1c"});
    assert_eq!(
        verify_color(
            target.clone(),
            &serde_json::from_value(snapshot.clone()).unwrap()
        )
        .outcome,
        DoneResult::Satisfied
    );
    snapshot["color_scopes"].as_array_mut().unwrap().push(json!({"node_id":12,"context":"main","name":"Checking","kind":"container","dialog_node_id":null}));
    assert_eq!(
        verify_color(target, &serde_json::from_value(snapshot).unwrap()).outcome,
        DoneResult::Unknown
    );
}

#[test]
fn transparent_reference_and_coverage_gaps_cannot_prove_color_inequality() {
    let body = json!({"target":{"text":"Amount"},"different_from":{"text":"Reference"}});
    let observed = page(vec![
        color("Amount", [185, 28, 28, 255], 1),
        color("Reference", [31, 41, 55, 0], 2),
    ]);
    assert_eq!(
        verify_color(body, &observed).outcome,
        DoneResult::NotSatisfied
    );
    let mut observed = page(vec![color("Amount", [185, 28, 28, 255], 1)]);
    observed.coverage.gaps.push("cross_origin_frame".into());
    assert_eq!(
        verify_color(
            json!({"target":{"text":"Amount"},"equals":"#b91c1c"}),
            &observed
        )
        .outcome,
        DoneResult::Unknown
    );
    assert_eq!(
        verify_color(
            json!({"target":{"text":"Absent"},"equals":"#b91c1c"}),
            &observed
        )
        .outcome,
        DoneResult::Unknown
    );
}
