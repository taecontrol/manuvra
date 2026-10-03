//! Capturing the target for a step: a verifiably redacted observation and screenshot, or a
//! withheld screenshot when masking cannot be verified, and the redacted export of each.

use super::artifacts::RunArtifacts;
use crate::actions;
use crate::evidence::Redactor;
use crate::verification::DoneResult;
use manuvra_chrome::{BrowserError, CapturedPage, Observation};
use manuvra_contract::{Assertion, DoneCondition, Expectation, Job};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) trait BrowserPage {
    fn capture_redacted_page(&self, sensitive: &[String]) -> Result<CapturedPage, BrowserError>;
    fn capture_redacted_matching_page(
        &self,
        sensitive: &[String],
        _unchanged: &dyn Fn(&Observation, &Observation) -> bool,
    ) -> Result<CapturedPage, BrowserError> {
        self.capture_redacted_page(sensitive)
    }
    fn observe_page(&self) -> Result<Observation, BrowserError>;
}

pub(super) trait DriveBrowser: BrowserPage + actions::Performer {}
impl<T: BrowserPage + actions::Performer> DriveBrowser for T {}

pub(super) struct Captured {
    pub(super) raw: Observation,
    pub(super) artifact: (String, Value, Option<Vec<u8>>),
    pub(super) redaction_verified: bool,
}

pub(super) fn capture_step(
    browser: &(impl BrowserPage + ?Sized),
    redactor: &Redactor,
    assertions: &[Assertion],
    step: usize,
    attempt: usize,
) -> Result<Captured, String> {
    let name = format!("o_{step:04}_{attempt}");
    let sensitive = redactor.sensitive_values();
    let captured = capture_assertions(browser, &sensitive, assertions);
    match captured {
        Ok(captured) if captured.redaction.verifies(sensitive.len()) => {
            let observation = redacted_observation(&captured.observation, redactor)?;
            Ok(Captured {
                raw: captured.observation,
                artifact: (name, observation, Some(captured.screenshot.bytes)),
                redaction_verified: true,
            })
        }
        Ok(_) => withheld_capture(browser, redactor, name),
        Err(BrowserError::Control(message)) if message == "redaction_unverifiable" => {
            withheld_capture(browser, redactor, name)
        }
        Err(error) => Err(redactor.redact_external_text(&error.to_string())),
    }
}

fn capture_assertions(
    browser: &(impl BrowserPage + ?Sized),
    sensitive: &[String],
    assertions: &[Assertion],
) -> Result<CapturedPage, BrowserError> {
    if !assertions.iter().any(needs_painted_fence) {
        return browser.capture_redacted_page(sensitive);
    }
    let facts =
        |observation: &Observation| crate::verification::assertion_checks(assertions, observation);
    browser
        .capture_redacted_matching_page(sensitive, &|before, after| facts(before) == facts(after))
}

fn needs_painted_fence(assertion: &Assertion) -> bool {
    match assertion {
        Assertion::Color(_) => true,
        Assertion::TextVisible(wanted) => wanted.include_aria_hidden,
        Assertion::TextAbsent(wanted) => wanted.include_aria_hidden,
        _ => false,
    }
}

pub(super) fn done_assertions(done: &DoneCondition) -> &[Assertion] {
    match done {
        DoneCondition::Structured(assertions) => assertions,
        DoneCondition::NaturalLanguage(_) => &[],
    }
}

pub(super) fn final_assertions(job: &Job) -> Vec<Assertion> {
    job.expectations
        .iter()
        .flat_map(|expectation| match expectation {
            Expectation::Structured(expectation) => expectation.assertions.clone(),
            Expectation::NaturalLanguage(_) => Vec::new(),
        })
        .collect()
}

fn withheld_capture(
    browser: &(impl BrowserPage + ?Sized),
    redactor: &Redactor,
    name: String,
) -> Result<Captured, String> {
    let raw = browser
        .observe_page()
        .map_err(|error| redactor.redact_external_text(&error.to_string()))?;
    let mut observation = redacted_observation(&raw, redactor)?;
    if let Value::Object(fields) = &mut observation {
        fields.insert(
            "screenshot".into(),
            json!({"withheld":"redaction_unverifiable"}),
        );
    }
    Ok(Captured {
        raw,
        artifact: (name, observation, None),
        redaction_verified: false,
    })
}

fn redacted_observation(raw: &Observation, redactor: &Redactor) -> Result<Value, String> {
    let redact = |text: &str| redactor.redact_external_text(text);
    let elements = raw
        .elements
        .iter()
        .map(|element| {
            let mut exported = json!({
                "index":element.index,
                "role":element.role,
                "name":redact(&element.name),
                "input_type":element.input_type,
                "value":redact(&element.value),
                "checked":element.checked,
                "selected":element.selected,
                "expanded":element.expanded,
                "disabled":element.disabled,
                "in_dialog":element.in_dialog.as_ref().map(|dialog|redact(dialog)),
                "operations":element.operations,
                "select_options":element.select_options.iter().map(|option|json!({
                    "label":redact(&option.label),
                    "value":redact(&option.value),
                    "disabled":option.disabled,
                    "selected":option.selected,
                })).collect::<Vec<_>>(),
                "rect":element.rect,
            });
            if let Some(container) = &element.container {
                exported["container"] = json!(redact(container));
            }
            if element.shares_name {
                exported["shares_name"] = json!(true);
            }
            exported
        })
        .collect::<Vec<_>>();
    let mut exported = json!({
        "url":redact(&raw.url),
        "route":redact(&raw.route),
        "title":redact(&raw.title),
        "dialogs":raw.dialogs.iter().map(|dialog|redact(dialog)).collect::<Vec<_>>(),
        "focused":raw.focused,
        "focus_anchor":raw.focus_anchor.as_ref().map(|anchor|json!({
            "role":redact(&anchor.role),
            "name":redact(&anchor.name),
            "in_dialog":anchor.in_dialog.as_ref().map(|dialog|redact(dialog)),
            "covered":anchor.covered,
            "active_descendant":anchor.active_descendant.as_ref().map(|item|json!({
                "id":redact(&item.id),"role":redact(&item.role),"name":redact(&item.name),
                "selected":item.selected,"checked":item.checked,
            })),
            "expanded":anchor.expanded,"selected":anchor.selected,"checked":anchor.checked,
        })),
        "visible_text":redact(&raw.visible_text),
        "covered_text":redact(&raw.covered_text),
        "dialog_texts":raw.dialog_texts.iter().map(|(name,text)|(redact(name),redact(text))).collect::<BTreeMap<_,_>>(),
        "elements":elements,
        "viewport":raw.viewport,
        "coverage":raw.coverage,
    });
    if let Some(container) = raw
        .focus_anchor
        .as_ref()
        .and_then(|anchor| anchor.container.as_ref())
    {
        exported["focus_anchor"]["container"] = json!(redact(container));
    }
    if let Value::Object(fields) = &mut exported {
        fields.extend(exported_hover_regions(raw, redactor));
        fields.extend(exported_scroll_regions(raw, redactor));
        fields.extend(exported_colors(raw, redactor));
        if let Some(painted) = &raw.painted_text {
            fields.insert(
                "painted_text".into(),
                exported_painted_text(painted, redactor),
            );
        }
    }
    Ok(exported)
}

fn exported_painted_text(
    painted: &manuvra_chrome::PaintedTextObservation,
    redactor: &Redactor,
) -> Value {
    let inventory = |text: &manuvra_chrome::TextInventory| {
        json!({
            "accessible":redactor.redact_external_text(&text.accessible),
            "painted_aria_hidden":redactor.redact_external_text(&text.painted_aria_hidden),
            "complete":text.complete,
        })
    };
    json!({
        "viewport":inventory(&painted.viewport),
        "dialogs":painted.dialogs.iter().map(|(name,text)|
            (redactor.redact_external_text(name),inventory(text))).collect::<BTreeMap<_,_>>(),
    })
}

fn exported_colors(raw: &Observation, redactor: &Redactor) -> Vec<(String, Value)> {
    if !raw.colors_complete && raw.colors.is_empty() && raw.color_scopes.is_empty() {
        return Vec::new();
    }
    let redact = |text: &str| redactor.redact_external_text(text);
    let optional = |text: &Option<String>| text.as_deref().map(redact);
    vec![
        ("colors_complete".into(), json!(raw.colors_complete)),
        ("colors".into(), json!(raw.colors.iter().map(|color| json!({
            "text":optional(&color.text),"name":optional(&color.name),"role":optional(&color.role),
            "in_dialog":optional(&color.in_dialog),"container":optional(&color.container),"channel":color.channel,"paint_complete":color.paint_complete,
            "color":{"raw":redact(&color.color.raw),"rgba":color.color.rgba}
        })).collect::<Vec<_>>())),
        ("color_scopes".into(), json!(raw.color_scopes.iter().map(|scope| json!({"name":redact(&scope.name),"kind":scope.kind})).collect::<Vec<_>>())),
    ]
}

fn exported_scroll_regions(raw: &Observation, redactor: &Redactor) -> Vec<(String, Value)> {
    let mut fields = Vec::new();
    if !raw.scroll_regions.is_empty() {
        let regions:Vec<_> = raw.scroll_regions.iter().map(|r| json!({
            "name":redactor.redact_external_text(&r.name), "overlay":r.overlay.as_ref().map(|s|redactor.redact_external_text(s)),
            "can_scroll_up":r.can_scroll_up,"can_scroll_down":r.can_scroll_down,
            "scroll_top":r.scroll_top,"scroll_height":r.scroll_height,"client_height":r.client_height,"rect":r.rect
        })).collect();
        fields.push(("scroll_regions".into(), json!(regions)));
    }
    if let Some(overlay) = &raw.overlay {
        fields.push((
            "overlay".into(),
            json!({"name":redactor.redact_external_text(&overlay.name)}),
        ));
    }
    if raw.scroll_regions_truncated {
        fields.push(("scroll_regions_truncated".into(), json!(true)));
    }
    fields
}

/// Hover regions appear in evidence only when present, with page text redacted and without the
/// internal dispatch identity.
fn exported_hover_regions(raw: &Observation, redactor: &Redactor) -> Vec<(String, Value)> {
    let redact = |text: &str| redactor.redact_external_text(text);
    let regions = raw
        .hover_regions
        .iter()
        .map(|region| {
            json!({
                "index":region.index,
                "name":redact(&region.name),
                "reveals_on_hover":region.reveals_on_hover.iter().map(|name|redact(name)).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    let mut fields = Vec::new();
    if !regions.is_empty() {
        fields.push(("hover_regions".to_owned(), Value::Array(regions)));
    }
    if raw.hover_regions_truncated {
        fields.push(("hover_regions_truncated".to_owned(), Value::Bool(true)));
    }
    if raw.hover_rules_unreadable {
        fields.push(("hover_rules_unreadable".to_owned(), Value::Bool(true)));
    }
    fields
}

pub(super) fn redacted_value(value: &impl serde::Serialize, redactor: &Redactor) -> Value {
    let mut value = serde_json::to_value(value).unwrap_or(Value::Null);
    redactor.redact_export_value(&mut value);
    value
}

pub(super) fn record_capture(
    artifacts: &mut RunArtifacts,
    redactor: &Redactor,
    step: &manuvra_contract::Step,
    captured: &Captured,
    event: &str,
    done: DoneResult,
) {
    artifacts.observations.push(captured.artifact.clone());
    let mut record =
        json!({"event":event,"step_id":redactor.redact_export_text(&step.id),"done":done});
    if let manuvra_contract::DoneCondition::Structured(assertions) = &step.done_when {
        let checks = crate::verification::assertion_checks(assertions, &captured.raw);
        if !checks.is_empty() {
            record["assertion_checks"] = json!(crate::evidence::redacted_assertion_checks(
                &checks, redactor
            ));
        }
    }
    artifacts.trace.push(record);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::tests::support::*;
    use manuvra_chrome::{Element, FocusAnchor, Rect};

    struct PaintChangingBrowser {
        before: Observation,
        after: Observation,
    }

    impl BrowserPage for PaintChangingBrowser {
        fn capture_redacted_page(
            &self,
            _sensitive: &[String],
        ) -> Result<CapturedPage, BrowserError> {
            captured(self.before.clone())
        }

        fn capture_redacted_matching_page(
            &self,
            _sensitive: &[String],
            unchanged: &dyn Fn(&Observation, &Observation) -> bool,
        ) -> Result<CapturedPage, BrowserError> {
            if unchanged(&self.before, &self.after) {
                captured(self.before.clone())
            } else {
                captured(self.after.clone())
            }
        }

        fn observe_page(&self) -> Result<Observation, BrowserError> {
            Ok(self.after.clone())
        }
    }

    #[test]
    fn painted_text_capture_retries_when_presence_or_absence_changes_during_the_screenshot() {
        for (kind, expected) in [
            ("text_visible", DoneResult::NotSatisfied),
            ("text_absent", DoneResult::Satisfied),
        ] {
            let mut job = job("Ready");
            job.steps[0].done_when = serde_json::from_value(json!([{
                kind:"Amount","include_aria_hidden":true
            }]))
            .unwrap();
            let mut before = observed("Ready");
            before.painted_text = Some(manuvra_chrome::PaintedTextObservation {
                viewport: manuvra_chrome::TextInventory {
                    accessible: "Ready".into(),
                    painted_aria_hidden: "Amount".into(),
                    complete: true,
                },
                dialogs: BTreeMap::new(),
            });
            let mut after = before.clone();
            after
                .painted_text
                .as_mut()
                .unwrap()
                .viewport
                .painted_aria_hidden
                .clear();
            let browser = PaintChangingBrowser { before, after };
            let assertions = done_assertions(&job.steps[0].done_when);
            let captured = capture_step(
                &browser,
                &Redactor::for_job(&job).unwrap(),
                assertions,
                0,
                1,
            )
            .unwrap();
            assert_eq!(
                crate::verification::evaluate_assertions(assertions, &captured.raw, &job.values)
                    .outcome,
                expected
            );
            assert_eq!(
                captured.artifact.1["painted_text"]["viewport"]["painted_aria_hidden"],
                ""
            );
        }
    }

    #[test]
    fn unreadable_hover_rules_are_evidence_only_and_leave_done_coverage_intact() {
        let job = job("Ready");
        let redactor = Redactor::for_job(&job).unwrap();
        let mut observation = text_field("");
        let baseline = redacted_observation(&observation, &redactor).unwrap();
        observation.hover_rules_unreadable = true;
        let mut exported = redacted_observation(&observation, &redactor).unwrap();
        assert_eq!(
            exported
                .as_object_mut()
                .unwrap()
                .remove("hover_rules_unreadable"),
            Some(json!(true))
        );
        assert_eq!(exported, baseline);
        assert_eq!(observation.coverage, manuvra_chrome::Coverage::default());
    }

    #[test]
    fn exported_observation_keeps_public_indices_but_omits_browser_identity() {
        let mut job = mutation_job();
        job.values.get_mut("name").unwrap().secret = true;
        let redactor = Redactor::for_job(&job).unwrap();
        let mut observation = text_field("");
        observation.document_id = "internal-document-token".into();
        observation.elements[0].node_id = 981_723;
        observation.elements[0].container = Some("Wanted".into());
        observation.elements[0].shares_name = true;
        observation.elements[0].context = "main/shadow:981723".into();
        observation.focus_anchor = Some(FocusAnchor {
            node_id: 981_723,
            context: "main/shadow:981723".into(),
            role: "textbox".into(),
            name: "Wanted".into(),
            in_dialog: None,
            container: Some("Wanted".into()),
            covered: true,
            surface: None,
            active_descendant: None,
            expanded: None,
            selected: None,
            checked: None,
            position: None,
        });
        let exported = redacted_observation(&observation, &redactor).unwrap();
        let text = exported.to_string();

        assert_eq!(exported["elements"][0]["index"], 1);
        assert_eq!(exported["elements"][0]["name"], "Name");
        assert_eq!(exported["elements"][0]["shares_name"], true);
        assert!(
            exported["elements"][0]["container"]
                .as_str()
                .unwrap()
                .contains("<masked:")
        );
        assert!(
            exported["focus_anchor"]["container"]
                .as_str()
                .unwrap()
                .contains("<masked:")
        );
        assert!(!text.contains("internal-document-token"));
        assert!(!text.contains("981723"));
        assert!(!text.contains("document_id"));
        assert!(!text.contains("node_id"));
        assert!(!text.contains("main/shadow"));
        assert!(!text.contains("Wanted"));
        assert!(
            exported["focus_anchor"]["name"]
                .as_str()
                .unwrap()
                .contains("<masked:")
        );
        assert!(exported["elements"][0].get("context").is_none());
    }

    fn richly_observed() -> Observation {
        let mut observation = text_field("Wanted");
        observation.visible_text = "Ready Wanted".into();
        observation.covered_text = "Behind Wanted".into();
        observation.dialogs = vec!["Confirm Wanted".into()];
        observation.dialog_texts =
            BTreeMap::from([("Confirm Wanted".into(), "Keep Wanted?".into())]);
        observation.focused = Some(1);
        observation.elements.push(Element {
            index: 2,
            node_id: 8,
            context: "main/frame:3".into(),
            role: "combobox".into(),
            name: "Account".into(),
            input_type: None,
            value: "wanted-id".into(),
            checked: Some(false),
            selected: None,
            expanded: Some(true),
            disabled: false,
            in_dialog: Some("Confirm Wanted".into()),
            container: None,
            shares_name: false,
            operations: vec!["SELECT".into()],
            select_options: vec![manuvra_chrome::SelectOption {
                node_id: 9,
                label: "Wanted".into(),
                value: "wanted-id".into(),
                disabled: false,
                selected: true,
            }],
            rect: Rect {
                x: 2.5,
                y: 30.0,
                width: 120.0,
                height: 24.0,
            },
        });
        observation.coverage.viewport_complete = false;
        observation.coverage.gaps = vec!["canvas".into(), "visible_text_truncated".into()];
        observation
    }

    #[test]
    fn exported_observation_without_hover_regions_is_unchanged() {
        let mut job = mutation_job();
        job.values.get_mut("name").unwrap().secret = true;
        let redactor = Redactor::for_job(&job).unwrap();

        let exported = redacted_observation(&richly_observed(), &redactor)
            .unwrap()
            .to_string();

        let golden = r#"{"coverage":{"gaps":["canvas","visible_text_truncated"],"open_shadow_roots":true,"same_origin_frames":true,"slots":true,"viewport_complete":false},"covered_text":"Behind {m}","dialog_texts":{"Confirm {m}":"Keep {m}?"},"dialogs":["Confirm {m}"],"elements":[{"checked":null,"disabled":false,"expanded":null,"in_dialog":null,"index":1,"input_type":"text","name":"Name","operations":["TYPE_TEXT"],"rect":{"height":10.0,"width":20.0,"x":1.0,"y":1.0},"role":"textbox","select_options":[],"selected":null,"value":"{m}"},{"checked":false,"disabled":false,"expanded":true,"in_dialog":"Confirm {m}","index":2,"input_type":null,"name":"Account","operations":["SELECT"],"rect":{"height":24.0,"width":120.0,"x":2.5,"y":30.0},"role":"combobox","select_options":[{"disabled":false,"label":"{m}","selected":true,"value":"wanted-id"}],"selected":null,"value":"wanted-id"}],"focus_anchor":null,"focused":1,"route":"/","title":"Money · Accounts","url":"http://127.0.0.1:4351/","viewport":{"document_height":780.0,"height":780,"scroll_x":0.0,"scroll_y":0.0,"width":1120},"visible_text":"Ready {m}"}"#;
        assert_eq!(
            exported,
            golden.replace("{m}", "\u{e000}<masked:1>\u{e000}")
        );
    }

    #[test]
    fn persisted_observation_lists_redacted_hover_regions_without_browser_identity() {
        let mut job = job("Ready");
        job.values.insert(
            "category".into(),
            manuvra_contract::JobValue {
                value: "Groceries".into(),
                description: "classified category".into(),
                formats: None,
                secret: true,
            },
        );
        let redactor = Redactor::for_job(&job).unwrap();
        let mut observation = observed("Ready");
        observation.hover_regions = vec![
            manuvra_chrome::HoverRegion {
                index: 1,
                name: "Groceries".into(),
                reveals_on_hover: vec!["Actions for Groceries".into()],
                node_id: 981_723,
                reveal_roles: vec!["button".into()],
                reveal_node_ids: vec![981_723],
            },
            manuvra_chrome::HoverRegion {
                index: 2,
                name: "Rent".into(),
                reveals_on_hover: vec!["Actions for Rent".into(), "Pin Rent".into()],
                node_id: 981_724,
                reveal_roles: vec!["button".into(), "button".into()],
                reveal_node_ids: vec![981_724, 981_725],
            },
        ];
        observation.hover_regions_truncated = true;

        let artifacts = driven(
            &job,
            &FakeBrowser::new([observation]),
            &NoProvider,
            &mut MemoryJournal::default(),
        );

        assert!(artifacts.stop.is_none());
        let persisted = &artifacts.observations[0].1;
        let masked = "\u{e000}<masked:1>\u{e000}";
        assert_eq!(
            persisted["hover_regions"],
            json!([
                {"index":1,"name":masked,"reveals_on_hover":[format!("Actions for {masked}")]},
                {"index":2,"name":"Rent","reveals_on_hover":["Actions for Rent","Pin Rent"]}
            ])
        );
        assert_eq!(persisted["hover_regions_truncated"], true);
        assert!(
            persisted["coverage"]["viewport_complete"]
                .as_bool()
                .unwrap()
        );
        let text = persisted.to_string();
        assert!(!text.contains("Groceries"));
        assert!(!text.contains("node_id"));
        assert!(!text.contains("98172"));
        assert!(!redactor.contains_export_leak(text.as_bytes()));
    }

    #[test]
    fn scroll_observation_masks_names_and_omits_browser_identity() {
        let mut job = mutation_job();
        job.values.get_mut("name").unwrap().secret = true;
        let redactor = Redactor::for_job(&job).unwrap();
        let mut page = richly_observed();
        page.scroll_regions=serde_json::from_value(json!([{"node_id":987321,"name":"Wanted","overlay":"Wanted","parent_node_id":null,"can_scroll_up":false,"can_scroll_down":true,"scroll_top":0,"scroll_height":1000,"client_height":300,"rect":{"x":0,"y":0,"width":200,"height":300}}])).unwrap();
        page.scroll_regions_truncated = true;
        let exported = redacted_observation(&page, &redactor).unwrap();
        assert_eq!(exported["scroll_regions_truncated"], true);
        assert_eq!(
            exported["scroll_regions"][0]["name"],
            "\u{e000}<masked:1>\u{e000}"
        );
        let text = exported.to_string();
        assert!(!text.contains("Wanted"));
        assert!(!text.contains("node_id"));
        assert!(!text.contains("987321"));
    }

    #[test]
    fn classified_values_with_json_escapes_are_redacted_before_persistence() {
        let mut job = mutation_job();
        job.values.insert(
            "escaped".into(),
            manuvra_contract::JobValue {
                value: r#"Se"cr\et"#.into(),
                description: "classified value with JSON escapes".into(),
                formats: None,
                secret: true,
            },
        );
        let redactor = Redactor::for_job(&job).unwrap();
        let value = redacted_value(
            &json!({"rationale":r#"typed Se"cr\et"#,"nested":[{r#"Se"cr\et"#:true}]}),
            &redactor,
        );
        let exported = value.to_string();
        assert!(!exported.contains(r#"Se\"cr\\et"#), "{exported}");
        assert!(!redactor.contains_export_leak(exported.as_bytes()));
        assert!(value["rationale"].as_str().unwrap().starts_with("typed "));
        assert!(
            redactor.contains_export_leak(json!({"leaked":r#"Se"cr\et"#}).to_string().as_bytes())
        );
    }
}
