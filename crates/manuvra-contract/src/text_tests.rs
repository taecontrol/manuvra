use super::*;
use serde_json::{Value, json};

#[test]
fn text_opt_in_is_a_boolean_in_both_assertions_and_structured_locations() {
    for kind in ["text_visible", "text_absent"] {
        for flag in [None, Some(json!(false)), Some(json!(true))] {
            let mut assertion = json!({kind:"$0.00","scope":{"dialog":"Month details"}});
            if let Some(flag) = flag {
                assertion["include_aria_hidden"] = flag;
            }
            let wire = json!({
                "schema_version":1,"target":{"kind":"browser","url":"http://example.test/"},
                "context":{"journey":"Text","revision":"fixture","environment":"synthetic","actor":"owner","authority":"observe"},
                "steps":[{"id":"ready","goal":"Observe amount","done_when":[assertion.clone()]}],
                "expectations":[{"id":"amount","assertions":[assertion.clone()]}]
            });
            let job: Job = serde_json::from_value(wire)
                .expect("the shared text opt-in must be accepted by the job contract");
            job.validate().unwrap();
            let exported = serde_json::to_value(job).unwrap();
            assert_eq!(exported["steps"][0]["done_when"][0][kind], "$0.00");
            assert_eq!(
                exported["expectations"][0]["assertions"][0]["include_aria_hidden"]
                    .as_bool()
                    .unwrap_or(false),
                assertion["include_aria_hidden"].as_bool().unwrap_or(false)
            );
        }
    }
    let schema = serde_json::to_value(schemars::schema_for!(Job)).unwrap();
    assert_eq!(
        schema["$defs"]["TextVisible"]["properties"]["include_aria_hidden"]["type"],
        "boolean"
    );
    assert_eq!(
        schema["$defs"]["TextAbsent"]["properties"]["include_aria_hidden"]["type"],
        "boolean"
    );
}

#[test]
fn text_opt_in_rejects_unknown_fields_and_non_boolean_values() {
    for kind in ["text_visible", "text_absent"] {
        for flag in [Value::Null, json!("true"), json!(1), json!([]), json!({})] {
            assert!(
                serde_json::from_value::<Assertion>(
                    json!({kind:"Amount","include_aria_hidden":flag})
                )
                .is_err()
            );
        }
        assert!(
            serde_json::from_value::<Assertion>(
                json!({kind:"Amount","include_aria_hidden":true,"unknown":true})
            )
            .is_err()
        );
    }
}
