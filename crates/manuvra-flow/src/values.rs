use manuvra_chrome::Observation;
use manuvra_contract::{Job, JobValue};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub struct Values<'a> {
    values: &'a BTreeMap<String, JobValue>,
    redact_values: &'a [String],
}

impl<'a> Values<'a> {
    pub fn new(job: &'a Job) -> Self {
        Self {
            values: &job.values,
            redact_values: job.options.redact_values.as_deref().unwrap_or(&[]),
        }
    }

    pub fn resolve(&self, name: &str) -> Option<&'a str> {
        self.values.get(name).map(|value| value.value.as_str())
    }

    pub fn known_names(&self) -> Vec<String> {
        self.values.keys().cloned().collect()
    }

    pub fn model_view(&self, observation: &Observation) -> Value {
        let elements: Vec<_> = observation
            .elements
            .iter()
            .map(|element| {
                let matches: Vec<_> = self
                    .values
                    .iter()
                    .filter(|(_, value)| rendering_matches(value, &element.value))
                    .map(|(name, _)| name)
                    .collect();
                let select_options: Vec<_> = element
                    .select_options
                    .iter()
                    .map(|option| {
                        let matches: Vec<_> = self
                            .values
                            .iter()
                            .filter(|(_, value)| {
                                rendering_matches(value, &option.value)
                                    || rendering_matches(value, &option.label)
                            })
                            .map(|(name, _)| name)
                            .collect();
                        json!({
                            "label":self.mask(&option.label),
                            "equals_value_names":matches,
                            "disabled":option.disabled,
                            "selected":option.selected
                        })
                    })
                    .collect();
                json!({
                    "index":element.index,"role":element.role,"name":self.mask(&element.name),
                    "input_type":element.input_type,"nonempty":!element.value.trim().is_empty(),
                    "equals_value_names":matches,"checked":element.checked,"selected":element.selected,
                    "expanded":element.expanded,"disabled":element.disabled,"in_dialog":element.in_dialog,
                    "operations":element.operations,"select_options":select_options
                })
            })
            .collect();
        json!({
            "url":self.mask(&observation.url),"route":self.mask(&observation.route),
            "title":self.mask(&observation.title),
            "dialogs":observation.dialogs.iter().map(|value|self.mask(value)).collect::<Vec<_>>(),
            "focused":observation.focused,"visible_text":self.mask(&observation.visible_text),
            "covered_text":self.mask(&observation.covered_text),"elements":elements,
            "coverage":observation.coverage
        })
    }

    pub fn descriptions(&self) -> Value {
        Value::Object(
            self.values
                .iter()
                .map(|(name, value)| (name.clone(), Value::String(self.mask(&value.description))))
                .collect(),
        )
    }

    pub(crate) fn mask(&self, text: &str) -> String {
        self.mask_matching(text, |_, _| true)
    }

    pub(crate) fn mask_sensitive(&self, text: &str) -> String {
        self.mask_matching(text, |name, value| {
            value.secret || self.redact_values.iter().any(|redacted| redacted == name)
        })
    }

    fn mask_matching(&self, text: &str, include: impl Fn(&str, &JobValue) -> bool) -> String {
        let mut output = text.to_owned();
        let mut renderings: Vec<_> = self
            .values
            .iter()
            .filter(|(name, value)| include(name, value))
            .flat_map(|(name, value)| {
                value_renderings(value)
                    .into_iter()
                    .map(move |rendering| (name, rendering))
            })
            .filter(|(_, rendering)| !rendering.is_empty())
            .collect();
        renderings.sort_by_key(|(_, rendering)| std::cmp::Reverse(rendering.len()));
        for (name, rendering) in renderings {
            output = output.replace(rendering, &format!("<value:{name}>"));
        }
        output
    }
}

fn rendering_matches(value: &JobValue, observed: &str) -> bool {
    value_renderings(value).contains(&observed)
}

fn value_renderings(value: &JobValue) -> Vec<&str> {
    let mut values = vec![value.value.as_str()];
    if let Some(formats) = &value.formats {
        values.extend(formats.iso.iter().map(String::as_str));
        values.extend(formats.display.iter().map(String::as_str));
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use manuvra_chrome::{Coverage, Element, Rect, SelectOption, ViewportState};
    use manuvra_contract::{Job, ValueFormats};
    use serde_json::json;

    #[test]
    fn model_view_exposes_names_descriptions_and_equality_but_no_raw_values() {
        let job = Job::parse(serde_json::to_vec(&json!({"schema_version":1,"target":{"kind":"browser","url":"http://example.test"},"context":{"journey":"x","revision":"x","environment":"x","actor":"x","authority":"x"},"values":{"secret_name":{"value":"raw-secret-742","description":"Account name","secret":true,"formats":{"display":"RAW SECRET"}}},"steps":[{"id":"x","goal":"fill","done_when":[{"field":"Account","equals_value":"secret_name"}]}]})).unwrap().as_slice()).unwrap();
        let observation = Observation {
            document_id: "internal-document-token".into(),
            url: "http://example.test/raw-secret-742".into(),
            route: "/raw-secret-742".into(),
            title: "raw-secret-742".into(),
            dialogs: vec![],
            focused: None,
            visible_text: "raw-secret-742".into(),
            covered_text: "RAW SECRET".into(),
            dialog_texts: BTreeMap::new(),
            elements: vec![Element {
                index: 1,
                node_id: 981_723,
                context: "main".into(),
                role: "textbox".into(),
                name: "raw-secret-742".into(),
                input_type: Some("text".into()),
                value: "raw-secret-742".into(),
                checked: None,
                selected: None,
                expanded: None,
                disabled: false,
                in_dialog: None,
                operations: vec!["TYPE_TEXT".into()],
                select_options: vec![],
                rect: Rect {
                    x: 0.,
                    y: 0.,
                    width: 1.,
                    height: 1.,
                },
            }],
            viewport: ViewportState {
                width: 1,
                height: 1,
                scroll_x: 0.,
                scroll_y: 0.,
                document_height: 1.,
            },
            coverage: Coverage::default(),
        };
        let serialized = Values::new(&job).model_view(&observation).to_string();
        assert!(!serialized.contains("raw-secret-742"));
        assert!(!serialized.contains("RAW SECRET"));
        assert!(!serialized.contains("internal-document-token"));
        assert!(!serialized.contains("981723"));
        assert!(!serialized.contains("document_id"));
        assert!(!serialized.contains("node_id"));
        assert!(serialized.contains("secret_name"));
        assert!(serialized.contains("equals_value_names"));
        let _ = ValueFormats {
            iso: None,
            display: None,
        };
    }

    #[test]
    fn sensitive_mask_leaves_public_values_but_removes_secret_and_redacted_values() {
        let job = Job::parse(
            serde_json::to_vec(&json!({
                "schema_version":1,
                "target":{"kind":"browser","url":"http://example.test"},
                "context":{"journey":"x","revision":"x","environment":"x","actor":"x","authority":"x"},
                "values":{
                    "secret":{"value":"private-742","description":"secret","secret":true},
                    "redacted":{"value":"hidden-815","description":"redacted"},
                    "public":{"value":"visible-926","description":"public"}
                },
                "steps":[{"id":"x","goal":"observe","done_when":[{"url_contains":"example.test"}]}],
                "options":{"redact_values":["redacted"]}
            }))
            .unwrap()
            .as_slice(),
        )
        .unwrap();
        assert_eq!(
            Values::new(&job).mask_sensitive("private-742 hidden-815 visible-926"),
            "<value:secret> <value:redacted> visible-926"
        );
    }

    #[test]
    fn model_view_exposes_select_matches_without_values_or_browser_identities() {
        let job = Job::parse(
            serde_json::to_vec(&json!({
                "schema_version":1,
                "target":{"kind":"browser","url":"http://example.test"},
                "context":{"journey":"x","revision":"x","environment":"x","actor":"x","authority":"x"},
                "values":{"country":{"value":"secret-country-742","description":"Country","secret":true}},
                "steps":[{"id":"x","goal":"Choose country","done_when":[{"field":"Country","equals_value":"country"}]}]
            }))
            .unwrap()
            .as_slice(),
        )
        .unwrap();
        let mut observation = serde_json::from_value::<Observation>(json!({
            "document_id":"internal-document-token","url":"http://example.test/","route":"/","title":"x",
            "elements":[],"viewport":{"width":1,"height":1,"scroll_x":0.0,"scroll_y":0.0,"document_height":1.0}
        }))
        .unwrap();
        observation.elements.push(Element {
            index: 1,
            node_id: 981_723,
            context: "main".into(),
            role: "combobox".into(),
            name: "Country".into(),
            input_type: None,
            value: String::new(),
            checked: None,
            selected: None,
            expanded: None,
            disabled: false,
            in_dialog: None,
            operations: vec!["SELECT".into()],
            select_options: vec![SelectOption {
                node_id: 812_345,
                label: "secret-country-742".into(),
                value: "secret-country-742".into(),
                disabled: false,
                selected: false,
            }],
            rect: Rect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
        });

        let serialized = Values::new(&job).model_view(&observation).to_string();
        assert!(serialized.contains("country"));
        assert!(serialized.contains("equals_value_names"));
        assert!(serialized.contains("<value:country>"));
        assert!(!serialized.contains("secret-country-742"));
        assert!(!serialized.contains("981723"));
        assert!(!serialized.contains("812345"));
        assert!(!serialized.contains("node_id"));
    }
}
