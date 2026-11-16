//! Argument parsing helpers for tool handlers.

use serde_json::{Map, Value};

use crate::error::{Error, Result};

/// Parsed tool arguments with basic helpers.
#[derive(Debug, Clone)]
pub struct Args {
    map: Map<String, Value>,
}

impl Args {
    pub fn parse(value: Option<Value>, allowed: &[&str]) -> Result<Self> {
        let map = match value {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(map)) => map,
            Some(_) => {
                return Err(Error::InvalidArguments(
                    "arguments must be an object".into(),
                ))
            }
        };
        if !allowed.is_empty() {
            for key in map.keys() {
                if !allowed.contains(&key.as_str()) {
                    return Err(Error::InvalidArguments(format!("unknown argument '{key}'")));
                }
            }
        }
        Ok(Self { map })
    }

    pub fn required_string(&self, key: &str) -> Result<String> {
        self.map
            .get(key)
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| Error::InvalidArguments(format!("missing or invalid '{key}'")))
    }

    pub fn optional_string(&self, key: &str) -> Option<String> {
        self.map
            .get(key)
            .and_then(Value::as_str)
            .map(ToString::to_string)
    }

    pub fn optional_bool(&self, key: &str) -> Option<bool> {
        self.map.get(key).and_then(Value::as_bool)
    }

    pub fn value(&self, key: &str) -> Option<&Value> {
        self.map.get(key)
    }
}
