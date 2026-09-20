use manuvra_chrome::Observation;
use manuvra_contract::{Job, JobValue};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub struct Values<'a> {
    values: &'a BTreeMap<String, JobValue>,
}

impl<'a> Values<'a> {
    pub fn new(job: &'a Job) -> Self {
        Self {
            values: &job.values,
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
                json!({
                    "index":element.index,"role":element.role,"name":self.mask(&element.name),
                    "input_type":element.input_type,"nonempty":!element.value.trim().is_empty(),
                    "equals_value_names":matches,"checked":element.checked,"selected":element.selected,
                    "expanded":element.expanded,"disabled":element.disabled,"in_dialog":element.in_dialog,
                    "operations":element.operations
                })
            })
            .collect();
        json!({
            "document_id":observation.document_id,"url":self.mask(&observation.url),
            "route":self.mask(&observation.route),"title":self.mask(&observation.title),
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
        let mut output = text.to_owned();
        let mut renderings: Vec<_> = self
            .values
            .iter()
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
    use manuvra_chrome::{Coverage, Element, Rect, ViewportState};
    use manuvra_contract::{Job, ValueFormats};
    use serde_json::json;

    #[test]
    fn model_view_exposes_names_descriptions_and_equality_but_no_raw_values() {
        let job = Job::parse(serde_json::to_vec(&json!({"schema_version":1,"target":{"kind":"browser","url":"http://example.test"},"context":{"journey":"x","revision":"x","environment":"x","actor":"x","authority":"x"},"values":{"secret_name":{"value":"raw-secret-742","description":"Account name","secret":true,"formats":{"display":"RAW SECRET"}}},"steps":[{"id":"x","goal":"fill","done_when":[{"field":"Account","equals_value":"secret_name"}]}]})).unwrap().as_slice()).unwrap();
        let observation = Observation {
            document_id: "d".into(),
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
                node_id: 1,
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
        assert!(serialized.contains("secret_name"));
        assert!(serialized.contains("equals_value_names"));
        let _ = ValueFormats {
            iso: None,
            display: None,
        };
    }
}
