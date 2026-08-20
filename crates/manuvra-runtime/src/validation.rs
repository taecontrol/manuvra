use serde_json::{Map, Value};
use std::collections::HashSet;

pub struct Input<'a> {
    map: &'a Map<String, Value>,
}

impl<'a> Input<'a> {
    pub fn new(value: &'a Value, allowed: &[&str]) -> Result<Self, String> {
        let map = value
            .as_object()
            .ok_or_else(|| "input must be an object".to_owned())?;
        let allowed = allowed.iter().copied().collect::<HashSet<_>>();
        if let Some(unknown) = map.keys().find(|key| !allowed.contains(key.as_str())) {
            return Err(format!("unknown input field {unknown}"));
        }
        Ok(Self { map })
    }

    pub fn string(&self, key: &str) -> Result<&'a str, String> {
        self.map
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("{key} must be a non-empty string"))
    }

    pub fn optional_string(&self, key: &str) -> Result<Option<&'a str>, String> {
        match self.map.get(key) {
            None => Ok(None),
            Some(value) => value
                .as_str()
                .map(Some)
                .ok_or_else(|| format!("{key} must be a string")),
        }
    }

    pub fn boolean(&self, key: &str, default: bool) -> Result<bool, String> {
        match self.map.get(key) {
            None => Ok(default),
            Some(value) => value
                .as_bool()
                .ok_or_else(|| format!("{key} must be a boolean")),
        }
    }

    pub fn unsigned(&self, key: &str, default: Option<u64>) -> Result<u64, String> {
        self.map
            .get(key)
            .and_then(Value::as_u64)
            .or(default)
            .ok_or_else(|| format!("{key} must be a non-negative integer"))
    }

    pub fn value(&self, key: &str) -> Result<&'a Value, String> {
        self.map
            .get(key)
            .ok_or_else(|| format!("{key} is required"))
    }

    pub fn optional_value(&self, key: &str) -> Option<&'a Value> {
        self.map.get(key)
    }
}
