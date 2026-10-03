use super::*;
use serde_json::{Value, json};

#[test]
fn eight_digit_color_literals_preserve_the_alpha_channel() {
    let check: ColorCheck =
        serde_json::from_value(json!({"target":{"text":"Amount"},"equals":"#0102031A"})).unwrap();
    let ColorCheck::Equals(check) = check else {
        panic!("exact literal")
    };
    assert_eq!(check.rgba(), Some([1, 2, 3, 26]));
}

fn job(assertion: Value, expectation: Value) -> Value {
    json!({
        "schema_version":1,"target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
        "context":{"journey":"Inspect color","revision":"fixture","environment":"synthetic","actor":"owner","authority":"observe"},
        "steps":[{"id":"color","goal":"Observe the amount","done_when":[assertion]}],
        "expectations":[expectation]
    })
}

#[test]
fn exact_and_relative_colors_are_additive_structured_job_forms() {
    for comparison in [
        json!({"equals":"#B91C1C"}),
        json!({"same_as":{"name":"Reference","role":"button"}}),
        json!({"different_from":{"text":"$12.34","container":"Checking"}}),
    ] {
        let mut color = comparison;
        color["target"] = json!({"text":"-$12.34","dialog":"Details"});
        color["tolerance"] = json!(2);
        let assertion = json!({"color":color});
        let input = job(
            assertion.clone(),
            json!({"id":"final","assertions":[assertion]}),
        );
        let parsed = Job::parse(&serde_json::to_vec(&input).unwrap())
            .expect("color is a supported structured assertion");
        let output = serde_json::to_value(parsed).unwrap();
        assert_eq!(
            output["steps"][0]["done_when"],
            input["steps"][0]["done_when"]
        );
        assert_eq!(output["expectations"], input["expectations"]);
        assert_eq!(output["schema_version"], 1);
    }
}

#[test]
fn color_contract_rejects_invalid_targets_comparisons_and_tolerances() {
    let valid = json!({"color":{"target":{"text":"-$12.34"},"equals":"#b91c1c"}});
    Job::parse(
        &serde_json::to_vec(&job(
            valid.clone(),
            json!({"id":"final","assertions":[valid]}),
        ))
        .unwrap(),
    )
    .unwrap();
    for color in [
        json!({"target":{},"equals":"#b91c1c"}),
        json!({"target":{"text":"  "},"equals":"#b91c1c"}),
        json!({"target":{"text":"\u{feff}"},"equals":"#b91c1c"}),
        json!({"target":{"text":"amount","name":"amount"},"equals":"#b91c1c"}),
        json!({"target":{"text":"amount","role":"button"},"equals":"#b91c1c"}),
        json!({"target":{"name":"amount","container":" "},"equals":"#b91c1c"}),
        json!({"target":{"name":"amount","role":" "},"equals":"#b91c1c"}),
        json!({"target":{"text":"amount","dialog":" "},"equals":"#b91c1c"}),
        json!({"target":{"name":"amount","unknown":true},"equals":"#b91c1c"}),
        json!({"target":{"text":"amount"},"equals":"red"}),
        json!({"target":{"text":"amount"},"equals":"#b91"}),
        json!({"target":{"text":"amount"},"equals":"#gg0000"}),
        json!({"target":{"text":"amount"},"equals":"#b91c1c","same_as":{"text":"other"}}),
        json!({"target":{"text":"amount"},"same_as":{"text":""}}),
        json!({"target":{"text":"amount"},"different_from":{"text":"other"},"tolerance":-1}),
        json!({"target":{"text":"amount"},"equals":"#b91c1c","tolerance":256}),
        json!({"target":{"text":"amount"},"equals":"#b91c1c","tolerance":0.5}),
        json!({"target":{"text":"amount"},"equals":"#b91c1c","property":"background-color"}),
        json!({"target":{"text":"amount"},"different_from":{"text":"other"},"unknown":true}),
    ] {
        let input = job(
            json!({"color":color}),
            json!({"id":"final","claim":"The amount exists"}),
        );
        assert!(
            Job::parse(&serde_json::to_vec(&input).unwrap()).is_err(),
            "{input}"
        );
    }
}

#[test]
fn color_assertion_rejects_unknown_outer_fields() {
    let input = job(
        json!({"color":{"target":{"text":"Amount"},"equals":"#b91c1c"},"unknown":true}),
        json!({"id":"final","claim":"Ready exists"}),
    );
    assert!(Job::parse(&serde_json::to_vec(&input).unwrap()).is_err());
}

#[test]
fn final_assertions_cannot_mix_claim_fields_and_validate_their_members() {
    let ready = json!({"text_visible":"Ready"});
    for expectation in [
        json!({"id":"final","assertions":[]}),
        json!({"id":"final","assertions":[{"color":{"target":{"text":"amount"},"equals":"bad"}}]}),
        json!({"id":"final","assertions":[ready.clone()],"claim":"Ready exists"}),
        json!({"id":"final","assertions":[ready.clone()],"exact_literals":[]}),
        json!({"id":"final","assertions":[ready.clone()],"unknown":true}),
    ] {
        assert!(
            Job::parse(&serde_json::to_vec(&job(ready.clone(), expectation.clone())).unwrap())
                .is_err(),
            "{expectation}"
        );
    }
    let input = job(ready.clone(), json!({"id":"final","assertions":[ready]}));
    assert!(Job::parse(&serde_json::to_vec(&input).unwrap()).is_ok());
    let schema = serde_json::to_value(schema_for!(Job)).unwrap().to_string();
    for field in [
        "color",
        "same_as",
        "different_from",
        "tolerance",
        "assertions",
    ] {
        assert!(
            schema.contains(&format!("\"{field}\"")),
            "missing job schema field {field}"
        );
    }
}
