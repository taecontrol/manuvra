use super::support::*;
use crate::evidence::Redactor;
use crate::run::machine::HostedMachine;
use manuvra_chrome::Observation;
use manuvra_contract::{DispositionKind, VerdictResult};
use serde_json::json;

fn job(claim: bool) -> manuvra_contract::Job {
    let mut expectations = vec![
        json!({"id":"text","assertions":[{"text_visible":"$0.00","include_aria_hidden":true}]}),
    ];
    if claim {
        expectations.push(json!({"id":"natural","claim":"Ready is present"}));
    }
    parse_job(json!({
        "schema_version":1,"target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
        "context":{"journey":"Text","revision":"fixture","environment":"synthetic","actor":"owner","authority":"observe"},
        "steps":[{"id":"ready","goal":"Observe Ready","done_when":[{"text_visible":"Ready"}]}],
        "expectations":expectations
    }))
}

fn page(hidden: &str, complete: bool) -> Observation {
    let mut wire = serde_json::to_value(observed("Ready")).unwrap();
    wire["covered_text"] = json!("unchanged covered text");
    wire["painted_text"] = json!({"viewport":{"accessible":"Ready","painted_aria_hidden":hidden,"complete":complete},"dialogs":{}});
    serde_json::from_value(wire).unwrap()
}

#[test]
fn final_painted_text_is_fresh_after_a_satisfied_painted_text_step() {
    let mut wire = serde_json::to_value(job(false)).unwrap();
    wire["steps"][0]["done_when"] = wire["expectations"][0]["assertions"].clone();
    wire["steps"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"later","goal":"Observe Ready","done_when":[{"text_visible":"Ready"}]}));
    let job = parse_job(wire);
    let browser = FakeBrowser::new([page("$0.00", true), page("", true), page("", true)]);
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
        |entry| entry["assertion_checks"][0]["text"]["matched_channel"] == "painted_aria_hidden"
    ));
}

#[test]
fn incomplete_painted_text_allows_only_retry_or_abort_and_retry_reads_fresh_text() {
    let job = job(false);
    let browser = FakeBrowser::new([page("", false), page("", false), page("$0.00", true)]);
    let redactor = Redactor::for_job(&job).unwrap();
    let mut machine = HostedMachine::new(&job, &redactor);
    let mut journal = MemoryJournal::default();
    drive(&mut machine, &browser, &NoProvider, &mut journal);
    assert_eq!(
        machine.artifacts.escalation.as_ref().unwrap().dispositions,
        [DispositionKind::RetryObservation, DispositionKind::Abort]
    );
    dispose(
        &mut machine,
        advance("I saw the amount"),
        &browser,
        &NoProvider,
        &mut journal,
    );
    assert!(!machine.verification_complete);
    assert!(!machine.artifacts.caller_assisted);
    dispose(&mut machine, retry(), &browser, &NoProvider, &mut journal);
    assert!(machine.verification_complete);
    assert_eq!(
        machine.artifacts.expectation_verdicts[0].result,
        VerdictResult::Satisfied
    );
    assert_eq!(browser.dispatched(), 0);
}

#[test]
fn advance_cannot_attest_a_painted_amount_that_became_ineligible() {
    let job = job(true);
    let browser = FakeBrowser::new([page("$0.00", true), page("$0.00", true), page("", true)]);
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
    assert!(!machine.artifacts.caller_assisted);
    assert_eq!(
        machine.artifacts.expectation_verdicts[0].result,
        VerdictResult::NotSatisfied
    );
    assert_eq!(provider.calls(), 1);
    assert_eq!(browser.dispatched(), 0);
}
