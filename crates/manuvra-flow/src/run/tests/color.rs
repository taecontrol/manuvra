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
