//! OpenAI Decisions (`POST /v1/decisions`) ↔ canonical System One decisions.
//!
//! Both directions are needed: OpenAI-format clients are routed to any
//! decision model ([`ingress_to_canonical`], [`canonical_to_ingress_response`]),
//! and any client is routed to OpenAI's `gpt-6-luna`
//! ([`canonical_to_upstream`], [`upstream_to_canonical`]).
//!
//! | OpenAI                                  | System One                                   |
//! |-----------------------------------------|----------------------------------------------|
//! | `predicate` → `probability`             | `noul` → `noul`                              |
//! | `choice.choices[{value, description}]`  | `choice.criteria{value: description}`        |
//! | `score.levels[{label, description}]`    | `score.criteria[...]`, level = position from 0 |
//! | answers array in question order         | answers keyed by question name               |
//! | `refusal` answer                        | `{"type": "refusal"}` (no System One equivalent) |
//! | `input` messages with `input_image`     | `state` with placeholder strings + `assets`  |
//!
//! Lossy edges, each refused or dropped deliberately:
//! - noul `criteria` become extra predicate instructions; OpenAI has no field.
//! - Box questions and assets referenced from questions have no OpenAI form
//!   and are refused (routing excludes box-incapable routes first).
//! - `prompt_cache_breakpoint` and assistant/tool input items have no System
//!   One form: ingress refuses non-user items and drops breakpoints.
//! - Choice values are typed in OpenAI (`true` vs `"true"`); System One keys
//!   are strings, so values that collide as text are refused.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::decisions::{
    DecisionAsset, DecisionQuestion, DecisionsIngress, DecisionsRequest, DecisionsResponse,
    DecisionsUsage, ImageAsset, NoulCriteria, PlaceholderSegment,
};

/// OpenAI's `POST /v1/decisions` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiDecisionsRequest {
    pub model: String,
    pub input: Value,
    pub questions: Vec<OpenAiQuestion>,
    /// `prompt_cache_key`, `prompt_cache_options`, `safety_identifier`, and
    /// future fields: forwarded only to OpenAI itself.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum OpenAiQuestion {
    Predicate {
        instructions: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Choice {
        instructions: String,
        choices: Vec<OpenAiChoice>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Score {
        instructions: String,
        levels: Vec<OpenAiLevel>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

impl OpenAiQuestion {
    fn name(&self) -> Option<&str> {
        match self {
            Self::Predicate { name, .. } | Self::Choice { name, .. } | Self::Score { name, .. } => {
                name.as_deref()
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChoice {
    /// A string or a boolean.
    pub value: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiLevel {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// What [`canonical_to_ingress_response`] needs to rebuild OpenAI answers:
/// question order, the caller's names, and typed choice values.
#[derive(Debug, Clone)]
pub struct OpenAiAnswerPlan {
    questions: Vec<PlannedQuestion>,
}

#[derive(Debug, Clone)]
struct PlannedQuestion {
    key: String,
    name: Option<String>,
    kind: PlannedKind,
}

#[derive(Debug, Clone)]
enum PlannedKind {
    Predicate,
    /// Canonical criterion key → the caller's typed value, in caller order.
    Choice(Vec<(String, Value)>),
    Score(Vec<String>),
}

fn choice_key(value: &Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Bool(flag) => Ok(flag.to_string()),
        _ => Err("choice `value` must be a string or a boolean".to_string()),
    }
}

/// Text for a System One guidance value (string, object, or array).
fn guidance_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Convert an OpenAI-format request into the canonical request routed by the
/// Decisions pipeline, plus the plan for rendering answers back.
pub fn ingress_to_canonical(
    request: OpenAiDecisionsRequest,
) -> Result<(DecisionsRequest, OpenAiAnswerPlan), String> {
    if request.questions.is_empty() {
        return Err("`questions` must contain at least one question".to_string());
    }
    let mut used: std::collections::HashSet<String> = request
        .questions
        .iter()
        .filter_map(|question| question.name().map(str::to_string))
        .collect();
    if used.len()
        != request
            .questions
            .iter()
            .filter(|q| q.name().is_some())
            .count()
    {
        return Err("question `name`s must be unique".to_string());
    }

    let mut questions = BTreeMap::new();
    let mut planned = Vec::with_capacity(request.questions.len());
    for (index, question) in request.questions.into_iter().enumerate() {
        let name = question.name().map(str::to_string);
        let key = match &name {
            Some(name) => name.clone(),
            None => {
                let mut candidate = format!("question_{}", index + 1);
                while used.contains(&candidate) {
                    candidate.push('_');
                }
                used.insert(candidate.clone());
                candidate
            }
        };
        let (canonical, kind) = match question {
            OpenAiQuestion::Predicate { instructions, .. } => (
                DecisionQuestion::Noul {
                    instructions: Value::String(instructions),
                    criteria: None,
                    extra: Map::new(),
                },
                PlannedKind::Predicate,
            ),
            OpenAiQuestion::Choice {
                instructions,
                choices,
                ..
            } => {
                let mut criteria = BTreeMap::new();
                let mut order = Vec::with_capacity(choices.len());
                for choice in choices {
                    let key = choice_key(&choice.value)
                        .map_err(|error| format!("question `{key}`: {error}"))?;
                    if criteria
                        .insert(key.clone(), choice.description.map(Value::String))
                        .is_some()
                    {
                        return Err(format!(
                            "question `{key}`: choice values must be distinct as text"
                        ));
                    }
                    order.push((key, choice.value));
                }
                (
                    DecisionQuestion::Choice {
                        instructions: Value::String(instructions),
                        criteria,
                        extra: Map::new(),
                    },
                    PlannedKind::Choice(order),
                )
            }
            OpenAiQuestion::Score {
                instructions,
                levels,
                ..
            } => {
                let labels = levels.iter().map(|level| level.label.clone()).collect();
                let criteria = levels
                    .into_iter()
                    .map(|level| match level.description {
                        Some(description) => {
                            json!({"label": level.label, "description": description})
                        }
                        None => Value::String(level.label),
                    })
                    .collect();
                (
                    DecisionQuestion::Score {
                        instructions: Value::String(instructions),
                        criteria,
                        extra: Map::new(),
                    },
                    PlannedKind::Score(labels),
                )
            }
        };
        questions.insert(key.clone(), canonical);
        planned.push(PlannedQuestion { key, name, kind });
    }

    let (state, assets) = input_to_state(request.input)?;
    Ok((
        DecisionsRequest {
            model: request.model,
            state,
            questions,
            assets,
            session_id: None,
            extra: request.extra,
            ingress: DecisionsIngress::OpenAi,
        },
        OpenAiAnswerPlan { questions: planned },
    ))
}

/// OpenAI `input` → System One `state` + `assets`. A single text stays a
/// string; mixed text and images become an array whose image entries are
/// placeholder strings, each a fresh UUID so it cannot collide with text.
fn input_to_state(input: Value) -> Result<(Value, BTreeMap<String, DecisionAsset>), String> {
    let items = match input {
        Value::String(text) => return Ok((Value::String(text), BTreeMap::new())),
        Value::Array(items) => items,
        _ => return Err("`input` must be a string or an array of messages".to_string()),
    };
    let unsupported =
        "`input` items must be user messages with `input_text` or `input_image` parts";
    let mut parts = Vec::new();
    let mut assets = BTreeMap::new();
    for item in items {
        let Value::Object(item) = item else {
            return Err(unsupported.to_string());
        };
        let is_message = item
            .get("type")
            .is_none_or(|kind| kind.as_str() == Some("message"));
        if !is_message || item.get("role").and_then(Value::as_str) != Some("user") {
            return Err(unsupported.to_string());
        }
        match item.get("content") {
            Some(Value::String(text)) => parts.push(Value::String(text.clone())),
            Some(Value::Array(content)) => {
                for part in content {
                    match part.get("type").and_then(Value::as_str) {
                        Some("input_text") => {
                            let text = part
                                .get("text")
                                .and_then(Value::as_str)
                                .ok_or("`input_text` parts need a string `text`")?;
                            parts.push(Value::String(text.to_string()));
                        }
                        Some("input_image") => {
                            let url = part
                                .get("image_url")
                                .and_then(Value::as_str)
                                .ok_or("`input_image` parts need a string `image_url`")?;
                            let detail = part
                                .get("detail")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                            let placeholder = uuid::Uuid::new_v4().to_string();
                            assets.insert(
                                placeholder.clone(),
                                DecisionAsset::Image(ImageAsset::from_url(url, detail)?),
                            );
                            parts.push(Value::String(placeholder));
                        }
                        _ => return Err(unsupported.to_string()),
                    }
                }
            }
            _ => return Err(unsupported.to_string()),
        }
    }
    let state = match parts.len() {
        0 => return Err("`input` must contain at least one text or image part".to_string()),
        1 if assets.is_empty() => parts.pop().unwrap_or_default(),
        _ => Value::Array(parts),
    };
    Ok((state, assets))
}

/// Render canonical answers as OpenAI's response, in the caller's question
/// order. A question the upstream did not answer is reported as a refusal.
pub fn canonical_to_ingress_response(
    plan: &OpenAiAnswerPlan,
    response: &DecisionsResponse,
) -> Result<Value, String> {
    let mut answers = Vec::with_capacity(plan.questions.len());
    for question in &plan.questions {
        let refusal = json!({"type": "refusal", "name": question.name});
        let Some(answer) = response.answers.get(&question.key) else {
            answers.push(refusal);
            continue;
        };
        if answer.get("type").and_then(Value::as_str) == Some("refusal") {
            answers.push(refusal);
            continue;
        }
        let number = |field: &str| -> Result<f64, String> {
            answer.get(field).and_then(Value::as_f64).ok_or_else(|| {
                format!(
                    "upstream answer `{}` is missing numeric `{field}`",
                    question.key
                )
            })
        };
        let probabilities = answer.get("probabilities").and_then(Value::as_object);
        answers.push(match &question.kind {
            PlannedKind::Predicate => json!({
                "type": "predicate",
                "name": question.name,
                "probability": number("noul")?,
            }),
            PlannedKind::Choice(order) => {
                let chosen = answer
                    .get("choice")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        format!("upstream answer `{}` is missing `choice`", question.key)
                    })?;
                let typed = order
                    .iter()
                    .find(|(key, _)| key == chosen)
                    .map(|(_, value)| value.clone())
                    .ok_or_else(|| {
                        format!(
                            "upstream answer `{}` chose unknown value `{chosen}`",
                            question.key
                        )
                    })?;
                json!({
                    "type": "choice",
                    "name": question.name,
                    "choice": typed,
                    "confidence": number("confidence")?,
                    "probabilities": order
                        .iter()
                        .map(|(key, value)| json!({
                            "value": value,
                            "probability": probabilities
                                .and_then(|map| map.get(key))
                                .and_then(Value::as_f64)
                                .unwrap_or(0.0),
                        }))
                        .collect::<Vec<_>>(),
                })
            }
            PlannedKind::Score(labels) => json!({
                "type": "score",
                "name": question.name,
                "score": number("score")?,
                "confidence": number("confidence")?,
                "probabilities": labels
                    .iter()
                    .enumerate()
                    .map(|(level, label)| json!({
                        "value": level,
                        "label": label,
                        "probability": probabilities
                            .and_then(|map| map.get(&level.to_string()))
                            .and_then(Value::as_f64)
                            .unwrap_or(0.0),
                    }))
                    .collect::<Vec<_>>(),
            }),
        });
    }
    Ok(json!({
        "answers": answers,
        "model": response.model,
        "usage": openai_usage(&response.usage),
    }))
}

/// OpenAI's usage object. Detail fields an OpenAI upstream reported survive
/// in `DecisionsUsage::extra`; other providers report none, so they are zero.
fn openai_usage(usage: &DecisionsUsage) -> Value {
    let detail = |field: &str, default: Value| usage.extra.get(field).cloned().unwrap_or(default);
    json!({
        "input_tokens": usage.input_tokens,
        "input_tokens_details": detail(
            "input_tokens_details",
            json!({"cache_write_tokens": 0, "cached_tokens": 0}),
        ),
        "output_tokens": usage.output_tokens,
        "output_tokens_details": detail(
            "output_tokens_details",
            json!({"reasoning_tokens": 0}),
        ),
        "total_tokens": usage.input_tokens.saturating_add(usage.output_tokens),
    })
}

/// Render a canonical request as an OpenAI request body. Question names are
/// the canonical keys, so [`upstream_to_canonical`] can match answers by name.
pub fn canonical_to_upstream(request: &DecisionsRequest) -> Result<Value, String> {
    if crate::decisions::questions_reference_assets(&request.questions, &request.assets) {
        return Err(
            "OpenAI Decisions cannot place images inside question instructions or criteria"
                .to_string(),
        );
    }
    let mut questions = Vec::with_capacity(request.questions.len());
    for (key, question) in &request.questions {
        let instructions = guidance_text(question.instructions());
        questions.push(match question {
            DecisionQuestion::Box { .. } => {
                return Err("OpenAI Decisions does not support box questions".to_string());
            }
            DecisionQuestion::Noul { criteria, .. } => json!({
                "type": "predicate",
                "name": key,
                "instructions": noul_instructions(instructions, criteria.as_ref()),
            }),
            DecisionQuestion::Choice { criteria, .. } => json!({
                "type": "choice",
                "name": key,
                "instructions": instructions,
                "choices": criteria
                    .iter()
                    .map(|(value, description)| {
                        let mut choice = json!({"value": value});
                        if let Some(description) = description {
                            choice["description"] = Value::String(guidance_text(description));
                        }
                        choice
                    })
                    .collect::<Vec<_>>(),
            }),
            DecisionQuestion::Score { criteria, .. } => json!({
                "type": "score",
                "name": key,
                "instructions": instructions,
                "levels": criteria.iter().map(score_level).collect::<Vec<_>>(),
            }),
        });
    }
    let mut body = json!({
        "model": request.model,
        "input": state_to_input(request),
        "questions": questions,
    });
    if request.forwards_extra_to(DecisionsIngress::OpenAi) {
        if let Value::Object(body) = &mut body {
            for (name, value) in &request.extra {
                body.entry(name.clone()).or_insert_with(|| value.clone());
            }
        }
    }
    Ok(body)
}

fn noul_instructions(instructions: String, criteria: Option<&NoulCriteria>) -> String {
    match criteria.and_then(|criteria| criteria.yes.as_ref().zip(criteria.no.as_ref())) {
        Some((yes, no)) => format!(
            "{instructions}\n\nTrue when: {}\nFalse when: {}",
            guidance_text(yes),
            guidance_text(no)
        ),
        None => instructions,
    }
}

/// A score criterion as an OpenAI level: `{label, description}` objects
/// (what [`ingress_to_canonical`] writes) round-trip; anything else is a label.
fn score_level(criterion: &Value) -> Value {
    if let Some(label) = criterion.get("label").and_then(Value::as_str) {
        let mut level = json!({"label": label});
        if let Some(description) = criterion.get("description") {
            level["description"] = Value::String(guidance_text(description));
        }
        return level;
    }
    json!({"label": guidance_text(criterion)})
}

/// System One `state` → OpenAI `input`. Without images the state is the text
/// (JSON is serialized). With images, the state's text is split at each
/// placeholder into ordered `input_text`/`input_image` parts; an asset the
/// state never mentions follows the text.
fn state_to_input(request: &DecisionsRequest) -> Value {
    let text = match &request.state {
        Value::String(text) => text.clone(),
        Value::Array(items) if items.iter().all(Value::is_string) => items
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    };
    if request.assets.is_empty() {
        return Value::String(text);
    }
    let mut parts = Vec::new();
    let mut placed = std::collections::HashSet::new();
    for segment in crate::decisions::split_at_placeholders(&text, &request.assets) {
        match segment {
            PlaceholderSegment::Text(text) => {
                if !text.trim().is_empty() {
                    parts.push(json!({"type": "input_text", "text": text}));
                }
            }
            PlaceholderSegment::Asset(key, asset) => {
                placed.insert(key.to_string());
                parts.push(input_image(asset.image()));
            }
        }
    }
    for (key, asset) in &request.assets {
        if !placed.contains(key) {
            parts.push(input_image(asset.image()));
        }
    }
    json!([{"type": "message", "role": "user", "content": parts}])
}

fn input_image(image: &ImageAsset) -> Value {
    let mut part = json!({
        "type": "input_image",
        "image_url": image.data_url().unwrap_or_default(),
    });
    if let Some(detail) = &image.detail {
        part["detail"] = Value::String(detail.clone());
    }
    part
}

/// OpenAI's response → canonical answers keyed by question name, with score
/// levels described by the request's own criteria.
pub fn upstream_to_canonical(
    request: &DecisionsRequest,
    response: Value,
) -> Result<DecisionsResponse, String> {
    let model = response
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&request.model)
        .to_string();
    let answers_in = response
        .get("answers")
        .and_then(Value::as_array)
        .ok_or("OpenAI Decisions response is missing `answers`")?;
    let keys: Vec<&String> = request.questions.keys().collect();
    let mut answers = BTreeMap::new();
    for (index, answer) in answers_in.iter().enumerate() {
        let key = match answer.get("name").and_then(Value::as_str) {
            Some(name) => name.to_string(),
            None => keys
                .get(index)
                .map(|key| key.to_string())
                .ok_or("OpenAI Decisions returned more answers than questions")?,
        };
        let number = |field: &str| answer.get(field).cloned().unwrap_or(Value::Null);
        let probabilities = answer
            .get("probabilities")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let converted = match answer.get("type").and_then(Value::as_str) {
            Some("predicate") => json!({"type": "noul", "noul": number("probability")}),
            Some("choice") => json!({
                "type": "choice",
                "choice": answer.get("choice").map(choice_text).unwrap_or_default(),
                "confidence": number("confidence"),
                "probabilities": probabilities
                    .iter()
                    .map(|entry| (
                        entry.get("value").map(choice_text).unwrap_or_default(),
                        entry.get("probability").cloned().unwrap_or(Value::Null),
                    ))
                    .collect::<Map<_, _>>(),
            }),
            Some("score") => {
                let criteria = match request.questions.get(&key) {
                    Some(DecisionQuestion::Score { criteria, .. }) => criteria.as_slice(),
                    _ => &[],
                };
                let level = |entry: &Value| {
                    entry
                        .get("value")
                        .and_then(Value::as_u64)
                        .map(|level| level.to_string())
                        .unwrap_or_default()
                };
                json!({
                    "type": "score",
                    "score": number("score"),
                    "confidence": number("confidence"),
                    "legend": criteria
                        .iter()
                        .enumerate()
                        .map(|(level, criterion)| (level.to_string(), criterion.clone()))
                        .collect::<Map<_, _>>(),
                    "probabilities": probabilities
                        .iter()
                        .map(|entry| (
                            level(entry),
                            entry.get("probability").cloned().unwrap_or(Value::Null),
                        ))
                        .collect::<Map<_, _>>(),
                })
            }
            Some("refusal") => json!({"type": "refusal"}),
            other => {
                return Err(format!(
                    "OpenAI Decisions returned an unknown answer type {other:?}"
                ))
            }
        };
        answers.insert(key, converted);
    }

    let usage = response.get("usage").cloned().unwrap_or_default();
    let tokens = |field: &str| usage.get(field).and_then(Value::as_u64).unwrap_or(0);
    let mut extra = Map::new();
    for field in ["input_tokens_details", "output_tokens_details"] {
        if let Some(value) = usage.get(field) {
            extra.insert(field.to_string(), value.clone());
        }
    }
    Ok(DecisionsResponse {
        id: None,
        model,
        provider: None,
        answers,
        usage: DecisionsUsage {
            input_tokens: tokens("input_tokens"),
            output_tokens: tokens("output_tokens"),
            cost: None,
            extra,
        },
        extra: Map::new(),
    })
}

fn choice_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn openai_request(value: Value) -> OpenAiDecisionsRequest {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn ingress_maps_every_question_type_and_keeps_order_names_and_typed_values() {
        let (canonical, plan) = ingress_to_canonical(openai_request(json!({
            "model": "typesafe/jev-1.14",
            "input": "Checkout is blank",
            "questions": [
                {"type": "score", "name": "severity", "instructions": "How severe?",
                 "levels": [{"label": "low"}, {"label": "high", "description": "Revenue loss"}]},
                {"type": "predicate", "instructions": "Is it a bug?"},
                {"type": "choice", "name": "escalate", "instructions": "Escalate?",
                 "choices": [{"value": true, "description": "Page on-call"}, {"value": "later"}]}
            ],
            "prompt_cache_key": "tickets"
        })))
        .unwrap();
        assert_eq!(canonical.ingress, DecisionsIngress::OpenAi);
        assert_eq!(canonical.state, "Checkout is blank");
        assert_eq!(canonical.extra["prompt_cache_key"], "tickets");
        let native = serde_json::to_value(&canonical.questions).unwrap();
        assert_eq!(
            native["severity"]["criteria"],
            json!(["low", {"label": "high", "description": "Revenue loss"}])
        );
        assert_eq!(native["question_2"]["type"], "noul");
        assert_eq!(
            native["escalate"]["criteria"],
            json!({"true": "Page on-call", "later": null})
        );

        let response: DecisionsResponse = serde_json::from_value(json!({
            "model": "typesafe/jev-1.14",
            "answers": {
                "severity": {"type": "score", "score": 0.8, "confidence": 0.7,
                             "legend": {"0": "low", "1": "high"},
                             "probabilities": {"0": 0.2, "1": 0.8}},
                "question_2": {"type": "noul", "noul": 0.9},
                "escalate": {"type": "choice", "choice": "true", "confidence": 0.6,
                             "probabilities": {"true": 0.6, "later": 0.4}}
            },
            "usage": {"input_tokens": 10, "output_tokens": 3}
        }))
        .unwrap();
        let rendered = canonical_to_ingress_response(&plan, &response).unwrap();
        assert_eq!(rendered["answers"][0]["name"], "severity");
        assert_eq!(
            rendered["answers"][0]["probabilities"][1],
            json!({"value": 1, "label": "high", "probability": 0.8})
        );
        assert_eq!(
            rendered["answers"][1],
            json!({"type": "predicate", "name": null, "probability": 0.9})
        );
        assert_eq!(rendered["answers"][2]["choice"], true);
        assert_eq!(rendered["usage"]["total_tokens"], 13);
    }

    #[test]
    fn ingress_images_become_placeholder_assets_and_refusals_survive() {
        let (canonical, plan) = ingress_to_canonical(openai_request(json!({
            "model": "openai/gpt-6-luna",
            "input": [{"role": "user", "content": [
                {"type": "input_text", "text": "Front door camera"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "low"}
            ]}],
            "questions": [{"type": "predicate", "name": "person", "instructions": "Person?"}]
        })))
        .unwrap();
        let state = canonical.state.as_array().unwrap();
        assert_eq!(state[0], "Front door camera");
        let placeholder = state[1].as_str().unwrap();
        let image = canonical.assets[placeholder].image();
        assert_eq!(image.format.as_deref(), Some("png"));
        assert_eq!(image.data_base64.as_deref(), Some("AAAA"));
        assert_eq!(image.detail.as_deref(), Some("low"));

        let response: DecisionsResponse = serde_json::from_value(json!({
            "model": "m", "answers": {"person": {"type": "refusal"}},
            "usage": {"input_tokens": 1, "output_tokens": 0}
        }))
        .unwrap();
        let rendered = canonical_to_ingress_response(&plan, &response).unwrap();
        assert_eq!(
            rendered["answers"][0],
            json!({"type": "refusal", "name": "person"})
        );
    }

    #[test]
    fn ingress_refuses_shapes_without_a_system_one_form() {
        for (input, questions, message) in [
            (
                json!([{"role": "assistant", "content": "hi"}]),
                json!([{"type": "predicate", "instructions": "x"}]),
                "user messages",
            ),
            (
                json!("x"),
                json!([{"type": "choice", "instructions": "x",
                        "choices": [{"value": true}, {"value": "true"}]}]),
                "distinct as text",
            ),
            (
                json!("x"),
                json!([{"type": "predicate", "name": "a", "instructions": "x"},
                       {"type": "predicate", "name": "a", "instructions": "y"}]),
                "unique",
            ),
        ] {
            let error = ingress_to_canonical(openai_request(json!({
                "model": "m", "input": input, "questions": questions
            })))
            .unwrap_err();
            assert!(error.contains(message), "{error}");
        }
    }

    fn canonical(value: Value) -> DecisionsRequest {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn upstream_request_renders_questions_images_and_forwards_only_openai_extras() {
        let mut request = canonical(json!({
            "model": "gpt-6-luna",
            "state": ["Door camera", "img-1"],
            "assets": {"img-1": {"type": "image", "format": "jpg", "data_base64": "BBBB"}},
            "questions": {
                "urgent": {"type": "noul", "instructions": "Urgent?",
                           "criteria": {"true": "now", "false": "later"}},
                "team": {"type": "choice", "instructions": "Team?",
                         "criteria": {"web": "Browser", "pay": null}},
                "severity": {"type": "score", "instructions": "Severity?",
                             "criteria": ["low", {"label": "high", "description": "outage"}]}
            },
            "provider": {"order": ["OpenAI"]}
        }));
        let body = canonical_to_upstream(&request).unwrap();
        assert!(
            body.get("provider").is_none(),
            "Alpha extras stay off OpenAI"
        );
        assert_eq!(
            body["input"][0]["content"],
            json!([
                {"type": "input_text", "text": "Door camera\n"},
                {"type": "input_image", "image_url": "data:image/jpeg;base64,BBBB"}
            ])
        );
        let questions = body["questions"].as_array().unwrap();
        assert_eq!(
            questions[0]["levels"][1],
            json!({"label": "high", "description": "outage"})
        );
        assert_eq!(
            questions[1]["choices"],
            json!([
                {"value": "pay"}, {"value": "web", "description": "Browser"}
            ])
        );
        assert_eq!(
            questions[2]["instructions"],
            "Urgent?\n\nTrue when: now\nFalse when: later"
        );

        request.ingress = DecisionsIngress::OpenAi;
        request.extra = Map::from_iter([("prompt_cache_key".to_string(), json!("k"))]);
        assert_eq!(
            canonical_to_upstream(&request).unwrap()["prompt_cache_key"],
            "k"
        );
    }

    #[test]
    fn upstream_refuses_box_and_question_images() {
        let boxed = canonical(json!({
            "model": "gpt-6-luna", "state": "x",
            "questions": {"where": {"type": "box", "instructions": "Find it"}}
        }));
        assert!(canonical_to_upstream(&boxed).unwrap_err().contains("box"));
        let referenced = canonical(json!({
            "model": "gpt-6-luna", "state": "x",
            "assets": {"ref-1": {"type": "image", "format": "png", "data_base64": "A"}},
            "questions": {"same": {"type": "noul", "instructions": "Like ref-1?"}}
        }));
        assert!(canonical_to_upstream(&referenced)
            .unwrap_err()
            .contains("instructions"));
    }

    #[test]
    fn upstream_response_becomes_system_one_answers() {
        let request = canonical(json!({
            "model": "gpt-6-luna", "state": "x",
            "questions": {
                "severity": {"type": "score", "instructions": "?", "criteria": ["low", "high"]},
                "team": {"type": "choice", "instructions": "?", "criteria": {"web": null, "pay": null}},
                "urgent": {"type": "noul", "instructions": "?"},
                "unsafe": {"type": "noul", "instructions": "?"}
            }
        }));
        let response = upstream_to_canonical(
            &request,
            json!({
                "model": "gpt-6-luna-2026-10-01",
                "answers": [
                    {"type": "score", "name": "severity", "score": 0.4, "confidence": 0.6,
                     "probabilities": [{"value": 0, "label": "low", "probability": 0.6},
                                       {"value": 1, "label": "high", "probability": 0.4}]},
                    {"type": "choice", "name": "team", "choice": "pay", "confidence": 0.9,
                     "probabilities": [{"value": "web", "probability": 0.1},
                                       {"value": "pay", "probability": 0.9}]},
                    {"type": "predicate", "name": "urgent", "probability": 0.25},
                    {"type": "refusal", "name": "unsafe"}
                ],
                "usage": {"input_tokens": 40, "output_tokens": 4, "total_tokens": 44,
                          "input_tokens_details": {"cached_tokens": 8, "cache_write_tokens": 0}}
            }),
        )
        .unwrap();
        assert_eq!(response.model, "gpt-6-luna-2026-10-01");
        assert_eq!(
            response.answers["severity"]["legend"],
            json!({"0": "low", "1": "high"})
        );
        assert_eq!(response.answers["severity"]["probabilities"]["1"], 0.4);
        assert_eq!(response.answers["team"]["probabilities"]["pay"], 0.9);
        assert_eq!(
            response.answers["urgent"],
            json!({"type": "noul", "noul": 0.25})
        );
        assert_eq!(response.answers["unsafe"], json!({"type": "refusal"}));
        assert_eq!(response.usage.input_tokens, 40);
        assert_eq!(
            response.usage.extra["input_tokens_details"]["cached_tokens"],
            8
        );
    }
}
