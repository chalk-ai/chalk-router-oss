//! Router Decisions, native System One, and normalized provider payloads.
//!
//! [`DecisionsRequest`] is the canonical shape every ingress is converted into
//! before routing: System One's `state` plus named questions, with images as
//! System One `assets`. Each backend renders it into its provider's wire
//! format; `translation::decisions_openai` covers OpenAI's Decisions API.
//! Top-level fields the router does not interpret are kept in
//! [`DecisionsRequest::extra`] and forwarded only to an upstream that speaks
//! the ingress format itself, the same rule Chat Completions follows.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const MAX_OBSERVABILITY_ID_CHARS: usize = 256;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DecisionsIngress {
    /// `POST /v1/alpha/decisions`: OpenRouter's Decisions contract.
    #[default]
    Alpha,
    /// `POST /v1/systemone`: TypeSafe's native contract.
    SystemOne,
    /// `POST /v1/decisions`: OpenAI's Decisions API, translated at ingress.
    OpenAi,
}

/// Image formats System One providers accept. TypeSafe takes PNG and JPEG
/// only; Perplexity and OpenAI also take WebP, which their adapters allow.
const IMAGE_FORMATS: &[&str] = &["png", "jpg", "jpeg", "webp"];

fn numeric_version(model: &str, prefix: &str, components: usize) -> bool {
    model.strip_prefix(prefix).is_some_and(|version| {
        let mut parts = version.split('.');
        (0..components).all(|_| {
            parts.next().is_some_and(|part| {
                !part.is_empty() && part.chars().all(|character| character.is_ascii_digit())
            })
        }) && parts.next().is_none()
    })
}

/// OpenRouter identifies the same pinned Jev release without the patch
/// component, for example `typesafe/jev-1.13`.
pub fn is_openrouter_jev_version(model: &str) -> bool {
    numeric_version(model, "typesafe/jev-", 2)
}

/// Whether a TypeSafe model id answers box questions: Jev 1.14 and later.
/// `jev-latest` still names 1.13; `None` for ids whose release is unknown.
pub fn jev_answers_boxes(model: &str) -> Option<bool> {
    let model = model.strip_prefix("typesafe/").unwrap_or(model);
    match model {
        "jev-preview" => return Some(true),
        "jev-latest" => return Some(false),
        _ => {}
    }
    let mut parts = model.strip_prefix("jev-")?.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    Some(major > 1 || minor >= 14)
}

/// OpenAI's Decisions model, also served on chat. Decisions requests reach it
/// through `translation::decisions_openai`.
pub fn is_openai_decision_model(model: &str) -> bool {
    model == "gpt-6-luna" || model.starts_with("gpt-6-luna-")
}

/// Perplexity's decision models, served on `POST /v1/decisions`.
pub fn is_perplexity_decision_model(model: &str) -> bool {
    model
        .strip_prefix("perplexity/")
        .unwrap_or(model)
        .starts_with("pplx-decider")
}

/// Decision models OpenRouter serves only through its Decisions endpoint.
/// Its catalog (`output_modalities=decisions`) is authoritative once a listing
/// has been discovered; this keeps routing correct before that. Moving `~`
/// aliases are deliberately absent: pricing and capabilities follow pinned
/// releases.
pub fn is_openrouter_decision_model(model: &str) -> bool {
    let model = model.strip_suffix(":free").unwrap_or(model);
    let Some((vendor, slug)) = model.split_once('/') else {
        return false;
    };
    match vendor {
        "typesafe" => is_openrouter_jev_version(model),
        "openai" => slug.ends_with("-decisions"),
        "perplexity" => slug.starts_with("pplx-decider"),
        "cloudflare" => slug.starts_with("clef"),
        "microsoft" => slug.starts_with("microsoft-decision-"),
        "inception" => slug.starts_with("mercury-decide"),
        "liquid" => slug == "d1" || slug.starts_with("d1-"),
        "upstage" => slug.starts_with("solar-decide"),
        "nace-ai" => slug.starts_with("drex-"),
        "respan" => slug.starts_with("span-"),
        "scaledown" => slug.starts_with("classify-"),
        "togethercomputer" => slug.starts_with("tev"),
        "jaredpalmer" => slug.starts_with("kev-"),
        _ => false,
    }
}

pub fn validate_session_id(session_id: &str) -> Result<(), String> {
    validate_observability_id("session_id", session_id)
}

fn validate_observability_id(field: &str, value: &str) -> Result<(), String> {
    if value.chars().count() > MAX_OBSERVABILITY_ID_CHARS {
        Err(format!("`{field}` must be at most 256 characters"))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionsRequest {
    pub model: String,
    pub state: Value,
    pub questions: BTreeMap<String, DecisionQuestion>,
    /// Images keyed by the placeholder that stands for them in `state`,
    /// `instructions`, or criteria (System One's `assets`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub assets: BTreeMap<String, DecisionAsset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Ingress fields the router does not interpret (OpenRouter's `provider`,
    /// `trace` and `user`; OpenAI's prompt-cache controls). Forwarded only when
    /// the upstream speaks the ingress format; see [`Self::forwards_extra_to`].
    #[serde(flatten)]
    pub extra: Map<String, Value>,
    #[serde(skip)]
    pub ingress: DecisionsIngress,
}

impl DecisionsRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.model.trim().is_empty() {
            return Err("`model` must not be empty".to_string());
        }
        if let Some(session_id) = &self.session_id {
            validate_session_id(session_id)?;
        }
        validate_assets(&self.assets)?;
        validate_payload(&self.state, &self.questions)
    }

    /// Whether `extra` belongs in a body sent in `upstream`'s format.
    pub fn forwards_extra_to(&self, upstream: DecisionsIngress) -> bool {
        self.ingress == upstream && !self.extra.is_empty()
    }

    pub fn has_box_questions(&self) -> bool {
        self.questions
            .values()
            .any(|question| matches!(question, DecisionQuestion::Box { .. }))
    }

    /// Assets as System One providers accept them: inline base64 only.
    pub fn system_one_assets(&self) -> Result<BTreeMap<String, DecisionAsset>, String> {
        self.assets
            .iter()
            .map(|(key, asset)| Ok((key.clone(), asset.system_one(key)?)))
            .collect()
    }

    pub fn into_system_one_request(self) -> Result<SystemOneRequest, String> {
        let assets = self.system_one_assets()?;
        Ok(SystemOneRequest {
            model: self.model,
            state: self.state,
            questions: self.questions,
            assets,
        })
    }

    pub fn from_system_one(request: SystemOneRequest) -> Self {
        Self {
            model: format!("typesafe/{}", request.model),
            state: request.state,
            questions: request.questions,
            assets: request.assets,
            session_id: None,
            extra: Map::new(),
            ingress: DecisionsIngress::SystemOne,
        }
    }
}

/// A System One asset. Only images exist today.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DecisionAsset {
    Image(ImageAsset),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageAsset {
    /// `png`, `jpg`, `jpeg`, or `webp`; required with `data_base64`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
    /// Router extension for OpenAI's `input_image`: a public HTTP(S) URL.
    /// Only the OpenAI adapter can deliver it; System One providers need bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Router extension for OpenAI's `input_image.detail`; System One has no
    /// equivalent and drops it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ImageAsset {
    /// `data:image/<format>;base64,<bytes>`, or the URL as given.
    pub fn data_url(&self) -> Option<String> {
        if let Some(url) = &self.url {
            return Some(url.clone());
        }
        let format = match self.format.as_deref()? {
            "jpg" => "jpeg",
            other => other,
        };
        Some(format!(
            "data:image/{format};base64,{}",
            self.data_base64.as_deref()?
        ))
    }

    /// Parse an OpenAI `image_url`: a base64 data URL or an HTTP(S) URL.
    pub fn from_url(url: &str, detail: Option<String>) -> Result<Self, String> {
        if let Some(rest) = url.strip_prefix("data:") {
            let (media, data) = rest
                .split_once(',')
                .ok_or("image data URLs must contain a comma")?;
            let format = media
                .strip_suffix(";base64")
                .and_then(|media| media.strip_prefix("image/"))
                .ok_or("image data URLs must be base64 `image/*` data")?
                .to_ascii_lowercase();
            return Ok(Self {
                format: Some(format),
                data_base64: Some(data.to_string()),
                url: None,
                detail,
            });
        }
        if url.starts_with("https://") || url.starts_with("http://") {
            return Ok(Self {
                format: None,
                data_base64: None,
                url: Some(url.to_string()),
                detail,
            });
        }
        Err("`image_url` must be a base64 data URL or an HTTP(S) URL".to_string())
    }
}

impl DecisionAsset {
    pub fn image(&self) -> &ImageAsset {
        match self {
            Self::Image(image) => image,
        }
    }

    fn system_one(&self, key: &str) -> Result<Self, String> {
        let image = self.image();
        match (&image.data_base64, &image.format) {
            (Some(_), Some(_)) => Ok(Self::Image(ImageAsset {
                url: None,
                detail: None,
                ..image.clone()
            })),
            _ => Err(format!(
                "asset `{key}` is an image URL; this provider needs `format` and `data_base64`"
            )),
        }
    }
}

fn validate_assets(assets: &BTreeMap<String, DecisionAsset>) -> Result<(), String> {
    for (key, asset) in assets {
        if key.is_empty() {
            return Err("asset placeholders must not be empty".to_string());
        }
        let image = asset.image();
        match (&image.data_base64, &image.url) {
            (Some(_), None) => {
                let format = image.format.as_deref().unwrap_or_default();
                if !IMAGE_FORMATS.contains(&format) {
                    return Err(format!(
                        "asset `{key}`: `format` must be one of {}",
                        IMAGE_FORMATS.join(", ")
                    ));
                }
            }
            (None, Some(url)) if url.starts_with("https://") || url.starts_with("http://") => {}
            _ => {
                return Err(format!(
                    "asset `{key}` needs either `format` and `data_base64` or an HTTP(S) `url`"
                ));
            }
        }
    }
    Ok(())
}

/// Split `text` at asset placeholders, in order. Placeholders are matched
/// longest first so one that contains another is never split apart.
pub fn split_at_placeholders<'a>(
    text: &'a str,
    assets: &'a BTreeMap<String, DecisionAsset>,
) -> Vec<PlaceholderSegment<'a>> {
    let mut keys: Vec<&String> = assets.keys().collect();
    keys.sort_by_key(|key| std::cmp::Reverse(key.len()));
    let mut segments = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let next = keys
            .iter()
            .filter_map(|key| rest.find(key.as_str()).map(|at| (at, *key)))
            .min_by_key(|(at, key)| (*at, std::cmp::Reverse(key.len())));
        let Some((at, key)) = next else {
            segments.push(PlaceholderSegment::Text(rest));
            break;
        };
        if at > 0 {
            segments.push(PlaceholderSegment::Text(&rest[..at]));
        }
        segments.push(PlaceholderSegment::Asset(key, &assets[key]));
        rest = &rest[at + key.len()..];
    }
    segments
}

/// Replace asset placeholders inside `state` with provider image parts, for
/// providers that take images inline (Perplexity). A string that is exactly a
/// placeholder becomes the part; a string containing placeholders becomes an
/// array of text and parts. Assets the state never mentions are appended.
pub fn state_with_inline_images(
    state: &Value,
    assets: &BTreeMap<String, DecisionAsset>,
    part: impl Fn(&ImageAsset) -> Value,
) -> Value {
    fn walk(
        value: &Value,
        assets: &BTreeMap<String, DecisionAsset>,
        part: &dyn Fn(&ImageAsset) -> Value,
        placed: &mut std::collections::HashSet<String>,
    ) -> Value {
        match value {
            Value::String(text) => {
                let segments = split_at_placeholders(text, assets);
                if !segments
                    .iter()
                    .any(|segment| matches!(segment, PlaceholderSegment::Asset(..)))
                {
                    return value.clone();
                }
                let mut parts: Vec<Value> = segments
                    .into_iter()
                    .map(|segment| match segment {
                        PlaceholderSegment::Text(text) => Value::String(text.to_string()),
                        PlaceholderSegment::Asset(key, asset) => {
                            placed.insert(key.to_string());
                            part(asset.image())
                        }
                    })
                    .collect();
                if parts.len() == 1 {
                    parts.pop().unwrap_or_default()
                } else {
                    Value::Array(parts)
                }
            }
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| walk(item, assets, part, placed))
                    .collect(),
            ),
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(name, item)| (name.clone(), walk(item, assets, part, placed)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    let mut placed = std::collections::HashSet::new();
    let state = walk(state, assets, &part, &mut placed);
    let unplaced: Vec<Value> = assets
        .iter()
        .filter(|(key, _)| !placed.contains(key.as_str()))
        .map(|(_, asset)| part(asset.image()))
        .collect();
    if unplaced.is_empty() {
        return state;
    }
    let mut items = match state {
        Value::Array(items) => items,
        other => vec![other],
    };
    items.extend(unplaced);
    Value::Array(items)
}

#[derive(Debug, PartialEq)]
pub enum PlaceholderSegment<'a> {
    Text(&'a str),
    Asset(&'a str, &'a DecisionAsset),
}

/// Whether any question's instructions or criteria mention an asset, which
/// only System One's placeholder substitution can express.
pub fn questions_reference_assets(
    questions: &BTreeMap<String, DecisionQuestion>,
    assets: &BTreeMap<String, DecisionAsset>,
) -> bool {
    !assets.is_empty()
        && questions.values().any(|question| {
            let text = serde_json::to_string(question).unwrap_or_default();
            assets.keys().any(|key| text.contains(key.as_str()))
        })
}

fn validate_questions(questions: &BTreeMap<String, DecisionQuestion>) -> Result<(), String> {
    if questions.is_empty() {
        return Err("`questions` must contain at least one question".to_string());
    }
    for (id, question) in questions {
        if id.trim().is_empty() {
            return Err("question ids must not be empty".to_string());
        }
        question
            .validate()
            .map_err(|error| format!("question `{id}`: {error}"))?;
    }
    Ok(())
}

fn validate_payload(
    state: &Value,
    questions: &BTreeMap<String, DecisionQuestion>,
) -> Result<(), String> {
    if !matches!(state, Value::String(_) | Value::Object(_) | Value::Array(_)) {
        return Err("`state` must be a string, object, or array".to_string());
    }
    validate_questions(questions)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DecisionQuestion {
    /// Locate something in an image: the answer is a bounding box given as
    /// fractions of the image (`top`, `bottom`, `left`, `right`). TypeSafe Jev
    /// 1.14 and later only.
    Box {
        instructions: Value,
        #[serde(flatten, skip_serializing)]
        extra: Map<String, Value>,
    },
    Noul {
        instructions: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
        #[serde(flatten, skip_serializing)]
        extra: Map<String, Value>,
    },
    Choice {
        instructions: Value,
        criteria: BTreeMap<String, Option<Value>>,
        #[serde(flatten, skip_serializing)]
        extra: Map<String, Value>,
    },
    Score {
        instructions: Value,
        criteria: Vec<Value>,
        #[serde(flatten, skip_serializing)]
        extra: Map<String, Value>,
    },
}

impl DecisionQuestion {
    pub fn instructions(&self) -> &Value {
        match self {
            Self::Box { instructions, .. }
            | Self::Noul { instructions, .. }
            | Self::Choice { instructions, .. }
            | Self::Score { instructions, .. } => instructions,
        }
    }

    fn validate(&self) -> Result<(), String> {
        validate_text_structure(self.instructions(), "`instructions`")?;

        match self {
            Self::Box { .. } => {}
            Self::Noul { criteria, .. } => {
                if let Some(criteria) = criteria {
                    if criteria.yes.is_none() || criteria.no.is_none() {
                        return Err(
                            "noul `criteria` must contain both `true` and `false`".to_string()
                        );
                    }
                    if let Some(value) = &criteria.yes {
                        validate_text_structure(value, "`criteria.true`")?;
                    }
                    if let Some(value) = &criteria.no {
                        validate_text_structure(value, "`criteria.false`")?;
                    }
                }
            }
            Self::Choice { criteria, .. } => {
                if criteria.is_empty() || criteria.len() > 255 {
                    return Err("choice `criteria` must contain 1 to 255 options".to_string());
                }
                for (option, description) in criteria {
                    if option.trim().is_empty() {
                        return Err("choice option names must not be empty".to_string());
                    }
                    if let Some(description) = description {
                        validate_text_structure(description, "choice criterion")?;
                    }
                }
            }
            Self::Score { criteria, .. } => {
                if !(2..=10).contains(&criteria.len()) {
                    return Err("score `criteria` must contain 2 to 10 levels".to_string());
                }
                for value in criteria {
                    validate_text_structure(value, "score criterion")?;
                }
            }
        }
        Ok(())
    }
}

fn validate_text_structure(value: &Value, field: &str) -> Result<(), String> {
    if matches!(value, Value::String(_) | Value::Object(_) | Value::Array(_)) {
        Ok(())
    } else {
        Err(format!("{field} must be a string, object, or array"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoulCriteria {
    #[serde(rename = "true", default, skip_serializing_if = "Option::is_none")]
    pub yes: Option<Value>,
    #[serde(rename = "false", default, skip_serializing_if = "Option::is_none")]
    pub no: Option<Value>,
    #[serde(flatten, skip_serializing)]
    pub extra: Map<String, Value>,
}

/// TypeSafe's native `POST /v1/systemone` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemOneRequest {
    pub model: String,
    pub state: Value,
    pub questions: BTreeMap<String, DecisionQuestion>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub assets: BTreeMap<String, DecisionAsset>,
}

impl SystemOneRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.model.trim().is_empty() {
            return Err("`model` must not be empty".to_string());
        }
        validate_assets(&self.assets)?;
        validate_payload(&self.state, &self.questions)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionsResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub answers: BTreeMap<String, Value>,
    pub usage: DecisionsUsage,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionsUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemOneResponse {
    pub model: String,
    pub answers: BTreeMap<String, Value>,
    pub usage: SystemOneUsage,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemOneUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl From<DecisionsResponse> for SystemOneResponse {
    fn from(response: DecisionsResponse) -> Self {
        let model = response
            .model
            .strip_prefix("openrouter/")
            .unwrap_or(&response.model);
        let model = model.strip_prefix("typesafe/").unwrap_or(model).to_string();
        Self {
            model,
            answers: response.answers,
            usage: SystemOneUsage {
                input_tokens: response.usage.input_tokens,
                output_tokens: response.usage.output_tokens,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: Value) -> DecisionsRequest {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn native_payload_omits_router_session_metadata() {
        let request = request(serde_json::json!({
            "model": "typesafe/jev-1.13.0",
            "state": "urgent ticket",
            "questions": {
                "urgent": {
                    "type": "noul",
                    "instructions": "Is it urgent?",
                    "criteria": {
                        "true": "urgent",
                        "false": "not urgent",
                        "future_criterion_field": true
                    },
                    "future_question_field": true
                }
            },
            "session_id": "session-1"
        }));

        request.validate().unwrap();
        let native = serde_json::to_value(request.into_system_one_request().unwrap()).unwrap();
        assert_eq!(native["model"], "typesafe/jev-1.13.0");
        assert!(native.get("session_id").is_none());
        assert!(native["questions"]["urgent"]
            .get("future_question_field")
            .is_none());
        assert!(native["questions"]["urgent"]["criteria"]
            .get("future_criterion_field")
            .is_none());
    }

    #[test]
    fn validates_question_cardinality() {
        let invalid = request(serde_json::json!({
            "model": "typesafe/jev-1.13.0",
            "state": "ticket",
            "questions": {
                "severity": {"type": "score", "instructions": "Severity?", "criteria": ["low"]}
            }
        }));
        assert_eq!(
            invalid.validate().unwrap_err(),
            "question `severity`: score `criteria` must contain 2 to 10 levels"
        );

        let invalid = request(serde_json::json!({
            "model": "typesafe/jev-1.13.0",
            "state": "ticket",
            "questions": {
                "urgent": {
                    "type": "noul",
                    "instructions": "Urgent?",
                    "criteria": {"true": "urgent"}
                }
            }
        }));
        assert_eq!(
            invalid.validate().unwrap_err(),
            "question `urgent`: noul `criteria` must contain both `true` and `false`"
        );
    }

    #[test]
    fn system_one_rejects_unknown_fields_while_router_decisions_keeps_them() {
        let alias: SystemOneRequest = serde_json::from_value(serde_json::json!({
            "model": "jev-latest",
            "state": "ticket",
            "questions": {
                "urgent": {"type": "noul", "instructions": "Urgent?"}
            }
        }))
        .unwrap();
        alias.validate().unwrap();

        let body = serde_json::json!({
            "model": "typesafe/jev-1.13.0",
            "state": "ticket",
            "questions": {
                "urgent": {"type": "noul", "instructions": "Urgent?"}
            },
            "provider": {"order": ["TypeSafe"]}
        });
        assert!(serde_json::from_value::<SystemOneRequest>(body.clone()).is_err());

        // Chat Completions forwards fields it does not interpret; Router
        // Decisions keeps them for an upstream that speaks its format.
        let mut request = serde_json::from_value::<DecisionsRequest>(body).unwrap();
        assert_eq!(request.extra["provider"]["order"][0], "TypeSafe");
        assert!(request.forwards_extra_to(DecisionsIngress::Alpha));
        assert!(!request.forwards_extra_to(DecisionsIngress::OpenAi));
        request.ingress = DecisionsIngress::OpenAi;
        assert!(!request.forwards_extra_to(DecisionsIngress::Alpha));
    }

    #[test]
    fn box_questions_and_image_assets_validate_and_reach_system_one_as_base64() {
        let request = request(serde_json::json!({
            "model": "typesafe/jev-1.14.0",
            "state": {"camera": "img-1"},
            "assets": {
                "img-1": {"type": "image", "format": "png", "data_base64": "AAAA", "detail": "low"}
            },
            "questions": {"mug": {"type": "box", "instructions": "Find the mug"}}
        }));
        request.validate().unwrap();
        assert!(request.has_box_questions());
        let native = serde_json::to_value(request.into_system_one_request().unwrap()).unwrap();
        assert_eq!(
            native["questions"]["mug"],
            serde_json::json!({
                "type": "box", "instructions": "Find the mug"
            })
        );
        assert_eq!(
            native["assets"]["img-1"],
            serde_json::json!({
                "type": "image", "format": "png", "data_base64": "AAAA"
            })
        );

        let url = request_with_asset(serde_json::json!({
            "type": "image", "url": "https://example.com/door.png"
        }));
        url.validate().unwrap();
        assert!(url
            .into_system_one_request()
            .unwrap_err()
            .contains("image URL"));
        for invalid in [
            serde_json::json!({"type": "image", "format": "gif", "data_base64": "A"}),
            serde_json::json!({"type": "image", "format": "png"}),
            serde_json::json!({"type": "image", "url": "ftp://example.com/a.png"}),
        ] {
            assert!(request_with_asset(invalid).validate().is_err());
        }
        assert!(
            serde_json::from_value::<DecisionsRequest>(serde_json::json!({
                "model": "m", "state": "s",
                "questions": {"q": {"type": "noul", "instructions": "?"}},
                "assets": {"a": {"type": "image", "part": "upload-1"}}
            }))
            .is_err(),
            "multipart `part` assets are not accepted over JSON"
        );
    }

    fn request_with_asset(asset: Value) -> DecisionsRequest {
        request(serde_json::json!({
            "model": "typesafe/jev-1.14.0",
            "state": "a",
            "assets": {"a": asset},
            "questions": {"q": {"type": "noul", "instructions": "?"}}
        }))
    }

    #[test]
    fn placeholders_split_longest_first_and_inline_into_state() {
        let assets: BTreeMap<String, DecisionAsset> = serde_json::from_value(serde_json::json!({
            "img": {"type": "image", "format": "png", "data_base64": "A"},
            "img-2": {"type": "image", "format": "jpg", "data_base64": "B"},
            "unused": {"type": "image", "format": "png", "data_base64": "C"}
        }))
        .unwrap();
        let segments = split_at_placeholders("see img-2 then img.", &assets);
        assert_eq!(segments.len(), 5);
        assert!(matches!(segments[1], PlaceholderSegment::Asset("img-2", _)));
        assert!(matches!(segments[3], PlaceholderSegment::Asset("img", _)));

        let state = state_with_inline_images(
            &serde_json::json!({"photo": "img", "note": "left img-2 right"}),
            &assets,
            |image| Value::String(image.data_url().unwrap()),
        );
        assert_eq!(
            state,
            serde_json::json!([
                {"photo": "data:image/png;base64,A",
                 "note": ["left ", "data:image/jpeg;base64,B", " right"]},
                "data:image/png;base64,C"
            ])
        );
    }

    #[test]
    fn decision_model_families_exclude_moving_aliases() {
        for model in [
            "typesafe/jev-1.14",
            "openai/gpt-6-luna-decisions",
            "perplexity/pplx-decider-v1.2",
            "cloudflare/clef-flash",
            "microsoft/microsoft-decision-1",
            "inception/mercury-decide:free",
            "liquid/d1",
            "liquid/d1-ultra",
            "upstage/solar-decide",
            "nace-ai/drex-v2.0",
            "respan/span-02",
            "scaledown/classify-1",
            "togethercomputer/tev1-4b-experimental",
            "jaredpalmer/kev-4b",
        ] {
            assert!(is_openrouter_decision_model(model), "{model}");
        }
        for model in [
            "~typesafe/jev-latest",
            "~perplexity/pplx-decider-latest",
            "typesafe/jev-latest",
            "openai/gpt-6-luna",
            "liquid/lfm-7b",
        ] {
            assert!(!is_openrouter_decision_model(model), "{model}");
        }
        assert_eq!(jev_answers_boxes("jev-1.14.0"), Some(true));
        assert_eq!(jev_answers_boxes("typesafe/jev-1.14"), Some(true));
        assert_eq!(jev_answers_boxes("jev-preview"), Some(true));
        assert_eq!(jev_answers_boxes("jev-1.13.0"), Some(false));
        assert_eq!(jev_answers_boxes("jev-latest"), Some(false));
        assert_eq!(jev_answers_boxes("future-system-one"), None);
    }

    #[test]
    fn system_one_response_removes_router_metadata() {
        let response: DecisionsResponse = serde_json::from_value(serde_json::json!({
            "id": "decision-1",
            "model": "openrouter/typesafe/jev-1.13",
            "provider": "OpenRouter",
            "answers": {"urgent": true},
            "usage": {"input_tokens": 12, "output_tokens": 3, "cost": 0.1}
        }))
        .unwrap();
        let rendered = serde_json::to_value(SystemOneResponse::from(response)).unwrap();
        assert_eq!(rendered["model"], "jev-1.13");
        assert!(rendered.get("id").is_none());
        assert!(rendered.get("provider").is_none());
        assert!(rendered["usage"].get("cost").is_none());
    }

    #[test]
    fn observability_ids_use_openapi_character_limits() {
        assert!(validate_session_id(&"é".repeat(256)).is_ok());
        assert_eq!(
            validate_session_id(&"é".repeat(257)).unwrap_err(),
            "`session_id` must be at most 256 characters"
        );
    }
}
