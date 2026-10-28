//! Structured output (`response_format`) normalization across providers.
//!
//! chalk-router speaks the OpenAI wire format, where structured output is
//! requested via `response_format`:
//!
//! ```json
//! {"type": "json_schema", "json_schema": {"name": "...", "strict": true, "schema": { ... }}}
//! {"type": "json_object"}
//! ```
//!
//! OpenAI (native) and Gemini (via its OpenAI-compatible endpoint) accept this
//! field verbatim, so those backends forward it untouched. The native Anthropic
//! Messages backend does not, so this module provides:
//!
//! * [`ResponseFormat`] — a typed view of the raw `response_format` value.
//! * Order-preserving schema normalization lives in [`crate::schema_json::SchemaJson`].
//! * [`validate_json_output`] — a dependency-free sanity check applied to the
//!   returned content.
//!
//! The Anthropic backend uses [`ResponseFormat`] to build the native
//! `output_config.format` request; see `backend/anthropic.rs`.

use std::sync::LazyLock;

use serde_json::{json, Value};

use crate::schema_json::SchemaJson;

/// Default schema name used when the caller's `json_schema` omits `name`.
pub const DEFAULT_SCHEMA_NAME: &str = "json_output";

/// Env var: when truthy, the OpenAI backend rewrites a `strict: true` schema to
/// satisfy OpenAI's strict-mode requirements (`additionalProperties: false`
/// everywhere, every property `required`). Off by default so we never silently
/// change a caller's schema semantics.
pub const OPENAI_STRICT_NORMALIZE_ENV: &str = "OPENAI_STRICT_SCHEMA_NORMALIZE";

/// Env var: when truthy, structured-output content is checked for JSON
/// validity / required-key presence and a warning is logged on mismatch. Never
/// fails the response — purely observability.
pub const VALIDATE_ENV: &str = "STRUCTURED_OUTPUT_VALIDATE";

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Whether OpenAI strict-schema normalization is enabled (see [`OPENAI_STRICT_NORMALIZE_ENV`]).
pub fn openai_strict_normalize_enabled() -> bool {
    env_truthy(OPENAI_STRICT_NORMALIZE_ENV)
}

/// Whether structured-output validation is enabled (see [`VALIDATE_ENV`]).
pub fn validation_enabled() -> bool {
    env_truthy(VALIDATE_ENV)
}

/// Typed view of the OpenAI `response_format` request field.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseFormat<'a> {
    /// `{"type": "json_object"}` — JSON mode: valid JSON, no schema enforcement.
    JsonObject,
    /// `{"type": "json_schema", "json_schema": {...}}` — schema-constrained.
    JsonSchema {
        name: &'a str,
        strict: bool,
        schema: &'a SchemaJson,
    },
}

fn permissive_schema() -> &'static SchemaJson {
    static SCHEMA: LazyLock<SchemaJson> = LazyLock::new(|| json!({"type": "object"}).into());
    &SCHEMA
}

impl<'a> ResponseFormat<'a> {
    /// Parse a raw `response_format` value. Returns `Ok(None)` when the value is
    /// absent, `{"type":"text"}`, or otherwise not a structured-output request
    /// (i.e. "nothing to do — forward as-is"). Names and schemas borrow the
    /// retained request; missing schemas keep the permissive object default.
    pub fn parse(v: &'a SchemaJson) -> serde_json::Result<Option<Self>> {
        if !v.is_object() {
            return Ok(None);
        }
        Ok(match v.member_ref("type")?.and_then(|kind| kind.as_str()) {
            Some("json_object") => Some(ResponseFormat::JsonObject),
            Some("json_schema") => {
                let Some(js) = v.member_ref("json_schema")? else {
                    return Ok(None);
                };
                let (name, strict, schema) = if js.is_object() {
                    (
                        js.member_ref("name")?
                            .and_then(|name| name.as_str())
                            .filter(|name| !name.is_empty())
                            .unwrap_or(DEFAULT_SCHEMA_NAME),
                        js.member_ref("strict")?
                            .and_then(|strict| strict.as_bool())
                            .unwrap_or(false),
                        js.member_ref("schema")?
                            .unwrap_or_else(|| permissive_schema()),
                    )
                } else {
                    (DEFAULT_SCHEMA_NAME, false, permissive_schema())
                };
                Some(ResponseFormat::JsonSchema {
                    name,
                    strict,
                    schema,
                })
            }
            _ => None,
        })
    }

    /// The schema to enforce, if any. `json_object` mode has no schema.
    pub fn schema(&self) -> Option<&Value> {
        match self {
            ResponseFormat::JsonSchema { schema, .. } => Some(schema.value()),
            ResponseFormat::JsonObject => None,
        }
    }

    /// The schema to enforce, defaulting to a permissive object for `json_object`
    /// mode (Anthropic tools/`output_config` require an object schema).
    pub fn schema_or_permissive(&self) -> &SchemaJson {
        match self {
            Self::JsonSchema { schema, .. } => schema,
            Self::JsonObject => permissive_schema(),
        }
    }
}

/// Result of validating a structured-output string.
#[derive(Debug, PartialEq)]
pub enum Validation {
    Ok,
    /// The content was not parseable JSON.
    InvalidJson,
    /// Parsed, but a schema-`required` key was missing at the top level.
    MissingRequired(Vec<String>),
}

/// Dependency-free validation of a structured-output JSON string against a schema.
/// Confirms the string parses as JSON and that top-level `required` keys are
/// present. This is a sanity check, not a full JSON Schema validator.
pub fn validate_json_output(content: &str, schema: Option<&Value>) -> Validation {
    let parsed: Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => return Validation::InvalidJson,
    };
    if let (Some(schema), Some(obj)) = (schema, parsed.as_object()) {
        if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
            let missing: Vec<String> = required
                .iter()
                .filter_map(|k| k.as_str())
                .filter(|k| !obj.contains_key(*k))
                .map(|k| k.to_string())
                .collect();
            if !missing.is_empty() {
                return Validation::MissingRequired(missing);
            }
        }
    }
    Validation::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recognizes_json_schema_and_object_and_text() {
        let value = json!({
            "type": "json_schema",
            "json_schema": {"name": "weather", "strict": true, "schema": {"type": "object"}}
        })
        .into();
        let Some(ResponseFormat::JsonSchema {
            name,
            strict,
            schema,
        }) = ResponseFormat::parse(&value).unwrap()
        else {
            panic!("expected json_schema");
        };
        assert_eq!(name, "weather");
        assert!(strict);
        assert_eq!(schema.value(), &json!({"type": "object"}));
        let config = value.member_ref("json_schema").unwrap().unwrap();
        assert!(std::ptr::eq(
            schema,
            config.member_ref("schema").unwrap().unwrap()
        ));

        assert_eq!(
            ResponseFormat::parse(&json!({"type": "json_object"}).into()).unwrap(),
            Some(ResponseFormat::JsonObject)
        );
        assert_eq!(
            ResponseFormat::parse(&json!({"type": "text"}).into()).unwrap(),
            None
        );
        assert_eq!(ResponseFormat::parse(&json!({}).into()).unwrap(), None);
    }

    #[test]
    fn parse_defaults_missing_name_and_strict() {
        let value = json!({
            "type": "json_schema",
            "json_schema": {"schema": {"type": "object"}}
        })
        .into();
        let rf = ResponseFormat::parse(&value).unwrap().unwrap();
        match rf {
            ResponseFormat::JsonSchema { name, strict, .. } => {
                assert_eq!(name, DEFAULT_SCHEMA_NAME);
                assert!(!strict);
            }
            _ => panic!("expected json_schema"),
        }
    }

    #[test]
    fn borrowed_inspection_preserves_permissive_defaults_and_explicit_schemas() {
        for config in [Value::Null, json!(42), json!("odd"), json!([]), json!({})] {
            let format = json!({"type":"json_schema","json_schema":config}).into();
            let parsed = ResponseFormat::parse(&format).unwrap().unwrap();
            assert_eq!(
                parsed.schema_or_permissive().value(),
                &json!({"type":"object"})
            );
        }
        for schema in [Value::Null, json!(42), json!([])] {
            let format = json!({"type":"json_schema","json_schema":{"schema":schema}}).into();
            let parsed = ResponseFormat::parse(&format).unwrap().unwrap();
            assert_eq!(parsed.schema_or_permissive().value(), &schema);
        }
        assert_eq!(
            ResponseFormat::parse(&json!({"type":"json_schema"}).into()).unwrap(),
            None
        );
        for value in [Value::Null, json!([]), json!(42), json!("text")] {
            assert_eq!(ResponseFormat::parse(&value.into()).unwrap(), None);
        }
    }

    #[test]
    fn validation_detects_invalid_and_missing() {
        let schema = json!({"type": "object", "required": ["name", "age"]});
        assert_eq!(
            validate_json_output("{not json", Some(&schema)),
            Validation::InvalidJson
        );
        assert_eq!(
            validate_json_output(r#"{"name": "x"}"#, Some(&schema)),
            Validation::MissingRequired(vec!["age".to_string()])
        );
        assert_eq!(
            validate_json_output(r#"{"name": "x", "age": 3}"#, Some(&schema)),
            Validation::Ok
        );
    }
}
