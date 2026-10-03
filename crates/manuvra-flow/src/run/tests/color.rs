use super::support::*;
use crate::evidence::Redactor;
use crate::run::machine::HostedMachine;
use manuvra_chrome::Observation;
use manuvra_contract::{DispositionKind, VerdictResult};
use serde_json::json;

fn job(claim: bool) -> manuvra_contract::Job {
    let mut expectations = vec![
        json!({"id":"color","assertions":[{"color":{"target":{"text":"Amount"},"equals":"#b91c1c"}}]}),
    ];
    if claim {
        expectations.push(json!({"id":"natural","claim":"Ready is present"}));
    }
    parse_job(json!({
        "schema_version":1,"target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
        "context":{"journey":"Color","revision":"fixture","environment":"synthetic","actor":"owner","authority":"observe"},
        "steps":[{"id":"ready","goal":"Observe Ready","done_when":[{"text_visible":"Ready"}]}],
        "expectations":expectations
    }))
}

fn page(rgba: Option<[u8; 4]>) -> Observation {
    let mut value = serde_json::to_value(observed("Ready")).unwrap();
    value["colors"] = json!([{"node_id":1,"context":"main","text":"Amount","name":null,"role":null,
        "in_dialog":null,"container":null,"dialog_node_id":null,"container_node_id":null,
        "channel":"accessible","color":{"raw":"fixture computed color","rgba":rgba}}]);
    value["colors_complete"] = json!(true);
    value["color_scopes"] = json!([]);
    serde_json::from_value(value).unwrap()
}

#[test]
fn final_color_is_fresh_after_an_earlier_satisfied_color_step() {
    let mut wire = serde_json::to_value(job(false)).unwrap();
    wire["steps"][0]["done_when"] = wire["expectations"][0]["assertions"].clone();
    wire["steps"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"later","goal":"Observe Ready","done_when":[{"text_visible":"Ready"}]}));
    let job = parse_job(wire);
    let browser = FakeBrowser::new([
        page(Some([185, 28, 28, 255])),
        page(Some([31, 41, 55, 255])),
        page(Some([31, 41, 55, 255])),
    ]);
    let artifacts = driven(&job, &browser, &NoProvider, &mut MemoryJournal::default());
    assert_eq!(artifacts.verdicts[0].result, VerdictResult::Satisfied);
    assert_eq!(
        artifacts.expectation_verdicts[0].result,
        VerdictResult::NotSatisfied
    );
    assert_eq!(artifacts.stop.unwrap().code, "expectation_not_met");
    assert_eq!(browser.dispatched(), 0);
    assert!(!artifacts.caller_assisted);
    assert!(artifacts.trace.iter().any(
        |entry| entry["assertion_checks"][0]["color"]["target"]["rgba"]
            == json!([185, 28, 28, 255])
    ));
}

#[test]
fn unknown_structured_final_never_offers_or_accepts_advance() {
    for mixed in [false, true] {
        let job = job(mixed);
        let browser = FakeBrowser::new([page(None)]);
        let provider = ScriptedProvider::new([Turn::verdict(0.5)]);
        let redactor = Redactor::for_job(&job).unwrap();
        let mut machine = HostedMachine::new(&job, &redactor);
        let mut journal = MemoryJournal::default();
        drive(&mut machine, &browser, &provider, &mut journal);
        assert_eq!(
            machine.artifacts.escalation.as_ref().unwrap().dispositions,
            [DispositionKind::RetryObservation, DispositionKind::Abort]
        );
        dispose(
            &mut machine,
            advance("I know the color"),
            &browser,
            &provider,
            &mut journal,
        );
        assert!(!machine.verification_complete);
        assert!(!machine.artifacts.caller_assisted);
        assert_eq!(
            machine.artifacts.expectation_verdicts[0].result,
            VerdictResult::Unresolved
        );
        assert_eq!(browser.dispatched(), 0);
    }
}

#[test]
fn color_only_change_while_paused_cannot_be_advance_attested() {
    let job = job(true);
    let browser = FakeBrowser::new([
        page(Some([185, 28, 28, 255])),
        page(Some([185, 28, 28, 255])),
        page(Some([31, 41, 55, 255])),
    ]);
    let provider = ScriptedProvider::new([Turn::verdict(0.5)]);
    let redactor = Redactor::for_job(&job).unwrap();
    let mut machine = HostedMachine::new(&job, &redactor);
    let mut journal = MemoryJournal::default();
    drive(&mut machine, &browser, &provider, &mut journal);
    assert!(
        machine
            .artifacts
            .escalation
            .as_ref()
            .unwrap()
            .dispositions
            .contains(&DispositionKind::Advance)
    );
    dispose(
        &mut machine,
        advance("Ready is present"),
        &browser,
        &provider,
        &mut journal,
    );
    assert!(!machine.verification_complete);
    assert!(!machine.artifacts.caller_assisted);
    assert_eq!(
        machine.artifacts.expectation_verdicts[0].result,
        VerdictResult::NotSatisfied
    );
    assert_eq!(
        machine.artifacts.expectation_verdicts[1].result,
        VerdictResult::NotRun
    );
    assert_eq!(provider.calls(), 1);
    assert_eq!(
        machine.artifacts.stop.as_ref().unwrap().code,
        "expectation_not_met"
    );
    assert_eq!(browser.dispatched(), 0);
}

#[test]
fn unchanged_satisfied_color_and_uncertain_claim_allow_natural_attestation() {
    let job = job(true);
    let browser = FakeBrowser::new([page(Some([185, 28, 28, 255]))]);
    let provider = ScriptedProvider::new([Turn::verdict(0.5)]);
    let redactor = Redactor::for_job(&job).unwrap();
    let mut machine = HostedMachine::new(&job, &redactor);
    let mut journal = MemoryJournal::default();
    drive(&mut machine, &browser, &provider, &mut journal);
    dispose(
        &mut machine,
        advance("Ready is present"),
        &browser,
        &provider,
        &mut journal,
    );
    assert!(machine.verification_complete);
    assert!(machine.artifacts.caller_assisted);
    assert!(
        machine
            .artifacts
            .expectation_verdicts
            .iter()
            .all(|v| v.result == VerdictResult::Satisfied)
    );
    assert_eq!(machine.artifacts.expectation_verdicts[0].noul, None);
    assert_eq!(provider.calls(), 1);
}

#[test]
fn unrelated_color_changes_do_not_invalidate_eligible_natural_attestation() {
    for structured in [false, true] {
        let mut wire = serde_json::to_value(job(true)).unwrap();
        if !structured {
            wire["expectations"].as_array_mut().unwrap().remove(0);
        }
        let job = parse_job(wire);
        let mut first = page(Some([185, 28, 28, 255]));
        let mut unrelated = first.colors[0].clone();
        unrelated.node_id = 2;
        unrelated.text = Some("Animated decoration".into());
        first.colors.push(unrelated);
        let mut changed = first.clone();
        changed.colors[1].color.rgba = Some([31, 41, 55, 255]);
        let browser = FakeBrowser::new([first.clone(), first, changed]);
        let provider = ScriptedProvider::new([Turn::verdict(0.5)]);
        let redactor = Redactor::for_job(&job).unwrap();
        let mut machine = HostedMachine::new(&job, &redactor);
        let mut journal = MemoryJournal::default();
        drive(&mut machine, &browser, &provider, &mut journal);
        dispose(
            &mut machine,
            advance("Ready is present"),
            &browser,
            &provider,
            &mut journal,
        );
        assert!(
            machine.verification_complete,
            "irrelevant color changed with structured={structured}"
        );
        assert!(machine.artifacts.caller_assisted);
        assert_eq!(provider.calls(), 1);
    }
}

#[test]
fn scope_only_ambiguity_while_paused_cannot_be_advance_attested() {
    let mut wire = serde_json::to_value(job(true)).unwrap();
    wire["expectations"][0]["assertions"][0]["color"]["target"]["container"] = json!("Checking");
    let job = parse_job(wire);
    let mut first = page(Some([185, 28, 28, 255]));
    first.colors[0].container_node_id = Some(10);
    first.colors[0].container = Some("Checking".into());
    first.color_scopes.push(manuvra_chrome::ColorScope {
        node_id: 10,
        context: "main".into(),
        name: "Checking".into(),
        kind: manuvra_chrome::ColorScopeKind::Container,
        dialog_node_id: None,
    });
    let mut changed = first.clone();
    let mut duplicate = changed.color_scopes[0].clone();
    duplicate.node_id = 11;
    changed.color_scopes.push(duplicate);
    let browser = FakeBrowser::new([first.clone(), first, changed]);
    let provider = ScriptedProvider::new([Turn::verdict(0.5), Turn::verdict(0.5)]);
    let redactor = Redactor::for_job(&job).unwrap();
    let mut machine = HostedMachine::new(&job, &redactor);
    let mut journal = MemoryJournal::default();
    drive(&mut machine, &browser, &provider, &mut journal);
    dispose(
        &mut machine,
        advance("Ready is present"),
        &browser,
        &provider,
        &mut journal,
    );
    assert!(!machine.verification_complete);
    assert!(!machine.artifacts.caller_assisted);
    assert_eq!(
        machine.artifacts.expectation_verdicts[0].result,
        VerdictResult::Unresolved
    );
    assert_eq!(
        machine.artifacts.escalation.as_ref().unwrap().dispositions,
        [DispositionKind::RetryObservation, DispositionKind::Abort]
    );
}

#[test]
fn structured_final_consumes_no_model_call_even_when_the_budget_is_used() {
    let mut wire = serde_json::to_value(job(false)).unwrap();
    wire["options"] = json!({"max_model_calls":1});
    let job = parse_job(wire);
    let redactor = Redactor::for_job(&job).unwrap();
    let mut machine = HostedMachine::new(&job, &redactor);
    machine.policy.record_model_call().unwrap();
    let browser = FakeBrowser::new([page(Some([185, 28, 28, 255]))]);
    drive(
        &mut machine,
        &browser,
        &NoProvider,
        &mut MemoryJournal::default(),
    );
    assert!(machine.verification_complete);
    assert!(machine.artifacts.stop.is_none());
    assert!(!machine.artifacts.caller_assisted);
}

#[test]
fn classified_color_protocol_words_preserve_typed_escalation_verdicts() {
    for classified in ["color", "comparator", "unresolved"] {
        let mut wire = serde_json::to_value(job(false)).unwrap();
        wire["values"] =
            json!({"marker":{"value":classified,"description":"classified text","secret":true}});
        wire["expectations"][0]["assertions"][0]["color"]["target"]["text"] = json!(classified);
        let job = parse_job(wire);
        let mut snapshot = serde_json::to_value(page(Some([185, 28, 28, 255]))).unwrap();
        snapshot["colors"][0]["text"] = json!(classified);
        let mut duplicate = snapshot["colors"][0].clone();
        duplicate["node_id"] = json!(2);
        snapshot["colors"].as_array_mut().unwrap().push(duplicate);
        let browser = FakeBrowser::new([serde_json::from_value(snapshot).unwrap()]);
        let artifacts = driven(&job, &browser, &NoProvider, &mut MemoryJournal::default());
        assert_eq!(
            artifacts.stop.as_ref().unwrap().code,
            "verification_uncertain"
        );
        let payload = &artifacts.escalations.last().unwrap().1;
        let verdicts: Vec<manuvra_contract::ExpectationVerdict> =
            serde_json::from_value(payload["expectations"].clone())
                .expect("classified text cannot corrupt typed color fields or enum values");
        assert_eq!(verdicts, artifacts.expectation_verdicts);
        let redactor = Redactor::for_job(&job).unwrap();
        let check = &payload["expectations"][0]["assertion_checks"][0]["color"];
        assert_eq!(
            check["target"]["selector"]["text"],
            redactor.redact_export_text(classified)
        );
        assert_eq!(check["result"], "unresolved");
        assert_eq!(check["comparator"], "equals");
    }
}
