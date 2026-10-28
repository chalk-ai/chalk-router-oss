//! Jev prompting: serve OpenAI chat-completion prompts with TypeSafe's Jev.
//!
//! Jev is a System One model. It answers typed questions about a piece of
//! state with calibrated probabilities instead of generating text, so a chat
//! prompt needs two translations before TypeSafe can serve it:
//!
//! * the conversation becomes the `state` Jev reads, and
//! * the request's `response_format` JSON schema becomes the `questions` Jev
//!   answers. Every property of the schema object is one typed question, and
//!   the typed answers come back as the JSON object the schema describes.
//!
//! | schema property                                        | question | answer            |
//! |--------------------------------------------------------|----------|-------------------|
//! | `boolean`                                              | noul     | `true` / `false`  |
//! | `number` bounded to `[0, 1]`                           | noul     | probability       |
//! | `string` with `enum` or `oneOf` consts                 | choice   | the chosen option |
//! | `integer` with `oneOf` consts, `enum`, or min/max      | score    | the nearest level |
//!
//! A property's `description` (falling back to `title`, then its name) is the
//! question's `instructions`; the `description` of each `oneOf` entry is the
//! criterion for that option or level. Number-typed properties expose Jev's
//! raw probability, which is how a chat client reaches the calibration that
//! the native `/alpha/decisions` API reports on every answer.
//!
//! Everything here is wire-agnostic: [`JevPrompt::plan`] builds the native
//! System One request and remembers how to shape each answer, and
//! [`JevPrompt::into_chat_completion`] turns TypeSafe's reply into a chat
//! completion. `TypeSafeBackend` owns the HTTP round trip between the two.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::decisions::{DecisionQuestion, DecisionsResponse, NoulCriteria, SystemOneRequest};
use crate::schema_json::SchemaJson;
use crate::structured_output::ResponseFormat;
use crate::types::{ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, Usage};

/// How clients must shape a prompt for Jev. Returned with every rejection so
/// the fix is in the error rather than in documentation the caller has not
/// read.
pub const PROMPTING_GUIDE: &str = "Jev answers typed questions about your prompt instead of \
generating text. Set `response_format` to a `json_schema` whose object properties are `boolean` \
(yes/no), `string` with `enum` (choice), `integer` with `oneOf` levels or `minimum`/`maximum` \
(score), or `number` bounded to [0, 1] (probability). Each property becomes one question and the \
reply is the matching JSON object. Use POST /v1/alpha/decisions for the native questions API.";

/// The most options a choice question may carry, per TypeSafe's contract.
const MAX_CHOICE_OPTIONS: usize = 255;
/// Score rubrics carry between two and ten ordered levels.
const SCORE_LEVELS: std::ops::RangeInclusive<usize> = 2..=10;

/// How one schema property's typed answer is rendered back into JSON.
#[derive(Debug, Clone, PartialEq)]
enum AnswerShape {
    /// A `boolean` property: `true` when Jev leans yes.
    Bool,
    /// A `number` property bounded to `[0, 1]`: Jev's raw yes-probability.
    Probability,
    /// A `string` property with options: the chosen option name.
    Choice,
    /// An `integer` property with ordered levels: the JSON value of the level
    /// nearest Jev's expected score, indexed by level.
    Score { levels: Vec<Value> },
}

/// A chat prompt translated into a System One request, plus the plan for
/// rendering the typed answers back into the schema's JSON object.
#[derive(Debug, Clone)]
pub struct JevPrompt {
    request: SystemOneRequest,
    shapes: Vec<(String, AnswerShape)>,
}

impl JevPrompt {
    /// Translate a chat completion request into a System One request.
    ///
    /// The error is a client-facing explanation: it names the offending field
    /// and ends with [`PROMPTING_GUIDE`], so the caller can fix the prompt.
    pub fn plan(request: &ChatCompletionRequest) -> Result<Self, String> {
        Self::plan_inner(request).map_err(|problem| format!("{problem} {PROMPTING_GUIDE}"))
    }

    fn plan_inner(request: &ChatCompletionRequest) -> Result<Self, String> {
        if request.model.trim().is_empty() {
            return Err("`model` must not be empty.".to_string());
        }
        if request
            .tools
            .as_ref()
            .is_some_and(|tools| !tools.is_empty())
        {
            return Err("Jev does not call tools; remove `tools`.".to_string());
        }
        let format = request
            .response_format
            .as_ref()
            .ok_or_else(|| "`response_format` must be a `json_schema`.".to_string())?;
        let Some(ResponseFormat::JsonSchema { schema, .. }) =
            ResponseFormat::parse(format).map_err(|error| error.to_string())?
        else {
            return Err("`response_format` must be a `json_schema`.".to_string());
        };

        let state = build_state(&request.messages)?;
        let (questions, shapes) = build_questions(schema)?;
        let prompt = Self {
            request: SystemOneRequest {
                model: request.model.clone(),
                state,
                questions,
                assets: Default::default(),
            },
            shapes,
        };
        prompt.request.validate()?;
        Ok(prompt)
    }

    /// Semantic System One view for inspection and validation. Use
    /// [`Self::wire_request`] to serialize translated questions in schema order.
    pub fn request(&self) -> &SystemOneRequest {
        &self.request
    }

    /// Ordered wire representation for the translated chat request. The typed
    /// System One view remains authoritative for validation and question values.
    pub fn wire_request(&self) -> Result<SchemaJson, String> {
        let questions = self
            .shapes
            .iter()
            .map(|(name, _)| {
                let question = self
                    .request
                    .questions
                    .get(name)
                    .ok_or_else(|| format!("missing planned question `{name}`"))?;
                let value =
                    SchemaJson::from_serializable(question).map_err(|error| error.to_string())?;
                Ok((name.clone(), value))
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(SchemaJson::object([
            ("model", Value::String(self.request.model.clone()).into()),
            ("state", self.request.state.clone().into()),
            ("questions", SchemaJson::object(questions)),
        ]))
    }

    /// Render TypeSafe's reply as a chat completion whose single assistant
    /// message is the JSON object the request's schema described.
    ///
    /// The error names an answer TypeSafe returned in a shape the plan did not
    /// expect; callers surface it as an upstream (502) failure.
    pub fn into_chat_completion(
        self,
        response: DecisionsResponse,
    ) -> Result<ChatCompletionResponse, String> {
        let mut content = Vec::new();
        for (name, shape) in &self.shapes {
            let answer = response
                .answers
                .get(name)
                .ok_or_else(|| format!("TypeSafe returned no answer for `{name}`"))?;
            content.push((name.clone(), render_answer(name, shape, answer)?.into()));
        }
        let content = serde_json::to_string(&SchemaJson::object(content))
            .map_err(|error| format!("answers did not serialize: {error}"))?;

        let prompt_tokens = clamp_tokens(response.usage.input_tokens);
        let completion_tokens = clamp_tokens(response.usage.output_tokens);
        Ok(ChatCompletionResponse {
            id: response
                .id
                .unwrap_or_else(|| format!("chatcmpl-{}", uuid::Uuid::new_v4().simple())),
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp(),
            model: response.model,
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: Some(Value::String(content)),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(Usage {
                prompt_tokens,
                completion_tokens: Some(completion_tokens),
                total_tokens: prompt_tokens.saturating_add(completion_tokens),
                ..Default::default()
            }),
        })
    }
}

fn clamp_tokens(tokens: u64) -> i32 {
    i32::try_from(tokens).unwrap_or(i32::MAX)
}

/// The conversation as Jev's `state`: a lone user message is passed as plain
/// text, anything longer as an ordered list of `{role, content}` turns so the
/// model still sees who said what.
fn build_state(messages: &[ChatMessage]) -> Result<Value, String> {
    if messages.is_empty() {
        return Err("`messages` must not be empty.".to_string());
    }
    let mut turns = Vec::with_capacity(messages.len());
    for message in messages {
        if message.role == "tool"
            || message
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty())
        {
            return Err(
                "Jev does not call tools; remove tool messages and `tool_calls`.".to_string(),
            );
        }
        turns.push((message.role.as_str(), message_text(message)?));
    }
    if turns.iter().all(|(_, text)| text.trim().is_empty()) {
        return Err("`messages` must contain text for Jev to evaluate.".to_string());
    }
    match turns.as_slice() {
        [("user", text)] => Ok(Value::String(text.clone())),
        _ => Ok(Value::Array(
            turns
                .into_iter()
                .map(|(role, content)| json!({"role": role, "content": content}))
                .collect(),
        )),
    }
}

fn message_text(message: &ChatMessage) -> Result<String, String> {
    match &message.content {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => {
            let mut texts = Vec::with_capacity(parts.len());
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => texts.push(
                        part.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    ),
                    Some(other) => {
                        return Err(format!(
                            "Jev reads text only; `{}` message content of type `{other}` is not supported.",
                            message.role
                        ))
                    }
                    None => {
                        return Err(format!(
                            "`{}` message content parts must declare a `type`.",
                            message.role
                        ))
                    }
                }
            }
            Ok(texts.join("\n"))
        }
        Some(_) => Err(format!(
            "`{}` message content must be a string or an array of content parts.",
            message.role
        )),
    }
}

type Questions = BTreeMap<String, DecisionQuestion>;

/// One typed question per property of the schema object, in schema order.
fn build_questions(schema: &SchemaJson) -> Result<(Questions, Vec<(String, AnswerShape)>), String> {
    let Some(object) = schema.as_object() else {
        return Err("`response_format.json_schema.schema` must be an object schema.".to_string());
    };
    if let Some(kind) = object.get("type") {
        if !type_names(kind).contains(&"object") {
            return Err(
                "`response_format.json_schema.schema` must have `type: object`.".to_string(),
            );
        }
    }
    let properties = schema
        .member("properties")
        .map_err(|error| error.to_string())?
        .filter(|properties| {
            properties
                .as_object()
                .is_some_and(|properties| !properties.is_empty())
        })
        .ok_or_else(|| {
            "`response_format.json_schema.schema` must declare at least one property.".to_string()
        })?
        .members()
        .map_err(|error| error.to_string())?;

    let mut questions = BTreeMap::new();
    let mut shapes = Vec::with_capacity(properties.len());
    for (name, property) in properties {
        if name.trim().is_empty() {
            return Err("schema property names must not be empty.".to_string());
        }
        let (question, shape) = build_question(&name, &property)
            .map_err(|problem| format!("schema property `{name}`: {problem}"))?;
        questions.insert(name.clone(), question);
        shapes.push((name.clone(), shape));
    }
    Ok((questions, shapes))
}

/// The JSON Schema `type` names a property declares, with `null` removed so
/// nullable fields translate like their non-null form.
fn type_names(kind: &Value) -> Vec<&str> {
    match kind {
        Value::String(name) => vec![name.as_str()],
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .filter(|name| *name != "null")
            .collect(),
        _ => Vec::new(),
    }
}

/// The options a property lists: each constant value and, for `oneOf` /
/// `anyOf` entries, the description that becomes its criterion.
type Options = Vec<(Value, Option<Value>)>;

/// One `oneOf` / `anyOf` / `enum` option: its constant value and, for the
/// first two forms, the description that becomes its criterion.
fn options(property: &Value) -> Result<Option<Options>, String> {
    if let Some(values) = property.get("enum").and_then(Value::as_array) {
        return Ok(Some(
            values.iter().cloned().map(|value| (value, None)).collect(),
        ));
    }
    let Some(alternatives) = property
        .get("oneOf")
        .or_else(|| property.get("anyOf"))
        .and_then(Value::as_array)
    else {
        return Ok(None);
    };
    alternatives
        .iter()
        .filter(|alternative| alternative.get("type").and_then(Value::as_str) != Some("null"))
        .map(|alternative| {
            let value = alternative
                .get("const")
                .cloned()
                .ok_or_else(|| "every `oneOf` entry must declare a `const`.".to_string())?;
            let description = alternative
                .get("description")
                .or_else(|| alternative.get("title"))
                .filter(|description| !description.is_null())
                .cloned();
            Ok((value, description))
        })
        .collect::<Result<Vec<_>, String>>()
        .map(Some)
}

fn build_question(name: &str, property: &Value) -> Result<(DecisionQuestion, AnswerShape), String> {
    let Some(object) = property.as_object() else {
        return Err("must be a schema object.".to_string());
    };
    let instructions = object
        .get("description")
        .or_else(|| object.get("title"))
        .filter(|value| matches!(value, Value::String(_) | Value::Object(_) | Value::Array(_)))
        .cloned()
        .unwrap_or_else(|| Value::String(name.to_string()));
    let options = options(property)?;
    let kind = match object.get("type").map(type_names) {
        Some(names) if names.len() == 1 => Some(names[0]),
        Some(names) if names.is_empty() => None,
        Some(names) => {
            return Err(format!(
                "declares several types ({}); Jev answers one typed question per property.",
                names.join(", ")
            ))
        }
        None => None,
    };
    let kind = kind
        .or_else(|| infer_type(options.as_deref()))
        .ok_or_else(|| {
            "must declare a `type` of boolean, string, integer, or number.".to_string()
        })?;

    match kind {
        "boolean" => Ok((
            DecisionQuestion::Noul {
                instructions,
                criteria: noul_criteria(options.as_deref())?,
                extra: Map::new(),
            },
            AnswerShape::Bool,
        )),
        "number" => {
            let bounded = object.get("minimum").and_then(Value::as_f64) == Some(0.0)
                && object.get("maximum").and_then(Value::as_f64) == Some(1.0);
            if !bounded {
                return Err(
                    "a `number` property is a probability and must set `minimum: 0` and `maximum: 1`."
                        .to_string(),
                );
            }
            Ok((
                DecisionQuestion::Noul {
                    instructions,
                    criteria: None,
                    extra: Map::new(),
                },
                AnswerShape::Probability,
            ))
        }
        "string" => {
            let options = options.ok_or_else(|| {
                "a `string` property must list its options with `enum` or `oneOf` consts."
                    .to_string()
            })?;
            if options.is_empty() || options.len() > MAX_CHOICE_OPTIONS {
                return Err(format!(
                    "must list between 1 and {MAX_CHOICE_OPTIONS} options."
                ));
            }
            let mut criteria = BTreeMap::new();
            for (value, description) in options {
                let Value::String(option) = value else {
                    return Err("every option of a `string` property must be a string.".to_string());
                };
                if option.trim().is_empty() {
                    return Err("option names must not be empty.".to_string());
                }
                if criteria.insert(option.clone(), description).is_some() {
                    return Err(format!("option `{option}` is listed twice."));
                }
            }
            Ok((
                DecisionQuestion::Choice {
                    instructions,
                    criteria,
                    extra: Map::new(),
                },
                AnswerShape::Choice,
            ))
        }
        "integer" => {
            let levels = match options {
                Some(options) => {
                    let mut levels = options
                        .into_iter()
                        .map(|(value, description)| {
                            let level = value.as_i64().ok_or_else(|| {
                                "every level of an `integer` property must be an integer."
                                    .to_string()
                            })?;
                            Ok((level, description))
                        })
                        .collect::<Result<Vec<_>, String>>()?;
                    levels.sort_by_key(|(level, _)| *level);
                    if levels.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                        return Err("levels must be distinct.".to_string());
                    }
                    levels
                }
                None => {
                    let (Some(minimum), Some(maximum)) = (
                        object.get("minimum").and_then(Value::as_i64),
                        object.get("maximum").and_then(Value::as_i64),
                    ) else {
                        return Err("an `integer` property must list its levels with `oneOf` consts or bound them with `minimum` and `maximum`.".to_string());
                    };
                    if maximum < minimum {
                        return Err("`maximum` must not be below `minimum`.".to_string());
                    }
                    (minimum..=maximum)
                        .take(*SCORE_LEVELS.end() + 1)
                        .map(|level| (level, None))
                        .collect()
                }
            };
            if !SCORE_LEVELS.contains(&levels.len()) {
                return Err(format!(
                    "must have between {} and {} levels.",
                    SCORE_LEVELS.start(),
                    SCORE_LEVELS.end()
                ));
            }
            let criteria = levels
                .iter()
                .map(|(level, description)| {
                    description
                        .clone()
                        .unwrap_or_else(|| Value::String(level.to_string()))
                })
                .collect();
            Ok((
                DecisionQuestion::Score {
                    instructions,
                    criteria,
                    extra: Map::new(),
                },
                AnswerShape::Score {
                    levels: levels.into_iter().map(|(level, _)| json!(level)).collect(),
                },
            ))
        }
        other => Err(format!(
            "type `{other}` is not supported; use boolean, string, integer, or number."
        )),
    }
}

/// A property that lists options without a `type` is typed by its options.
fn infer_type(options: Option<&[(Value, Option<Value>)]>) -> Option<&'static str> {
    let options = options?;
    let first = &options.first()?.0;
    let inferred = match first {
        Value::Bool(_) => "boolean",
        Value::String(_) => "string",
        Value::Number(number) if number.is_i64() => "integer",
        _ => return None,
    };
    options
        .iter()
        .all(|(value, _)| match inferred {
            "boolean" => value.is_boolean(),
            "string" => value.is_string(),
            _ => value.is_i64(),
        })
        .then_some(inferred)
}

/// `oneOf` entries for `true` and `false` describe what each answer means.
fn noul_criteria(
    options: Option<&[(Value, Option<Value>)]>,
) -> Result<Option<NoulCriteria>, String> {
    let Some(options) = options else {
        return Ok(None);
    };
    let mut yes = None;
    let mut no = None;
    for (value, description) in options {
        match value {
            Value::Bool(true) => yes = description.clone(),
            Value::Bool(false) => no = description.clone(),
            _ => {
                return Err(
                    "every option of a `boolean` property must be true or false.".to_string(),
                )
            }
        }
    }
    match (yes, no) {
        (Some(yes), Some(no)) => Ok(Some(NoulCriteria {
            yes: Some(yes),
            no: Some(no),
            extra: Map::new(),
        })),
        (None, None) => Ok(None),
        _ => Err("describe both the `true` and the `false` option, or neither.".to_string()),
    }
}

fn render_answer(name: &str, shape: &AnswerShape, answer: &Value) -> Result<Value, String> {
    let field = |key: &str| {
        answer
            .get(key)
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("TypeSafe answer for `{name}` has no numeric `{key}`"))
    };
    match shape {
        AnswerShape::Bool => Ok(Value::Bool(field("noul")? >= 0.5)),
        AnswerShape::Probability => Ok(json!(field("noul")?)),
        AnswerShape::Choice => answer
            .get("choice")
            .filter(|choice| choice.is_string())
            .cloned()
            .ok_or_else(|| format!("TypeSafe answer for `{name}` has no `choice`")),
        AnswerShape::Score { levels } => {
            let score = field("score")?;
            let index = score.round().clamp(0.0, (levels.len() - 1) as f64) as usize;
            Ok(levels[index].clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat_request(value: Value) -> ChatCompletionRequest {
        serde_json::from_value(value).unwrap()
    }

    fn schema_request(properties: Value) -> ChatCompletionRequest {
        chat_request(json!({
            "model": "jev-latest",
            "messages": [{"role": "user", "content": "I was charged twice and nobody answers."}],
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "triage",
                    "schema": {"type": "object", "properties": properties}
                }
            }
        }))
    }

    fn native(prompt: &JevPrompt) -> Value {
        serde_json::to_value(prompt.request()).unwrap()
    }

    #[test]
    fn schema_properties_become_typed_questions() {
        let prompt = JevPrompt::plan(&schema_request(json!({
            "billing": {"type": "boolean", "description": "Is this about billing?"},
            "tone": {
                "type": "string",
                "description": "What is the tone?",
                "oneOf": [
                    {"const": "angry", "description": "upset or hostile"},
                    {"const": "calm", "description": "neutral or polite"}
                ]
            },
            "urgency": {
                "type": "integer",
                "description": "How urgent is this?",
                "oneOf": [
                    {"const": 1, "description": "Can wait"},
                    {"const": 2, "description": "This week"},
                    {"const": 3, "description": "Today"}
                ]
            },
            "spam_probability": {"type": "number", "minimum": 0, "maximum": 1, "description": "Spam?"}
        })))
        .unwrap();

        let native = native(&prompt);
        assert_eq!(native["model"], "jev-latest");
        assert_eq!(native["state"], "I was charged twice and nobody answers.");
        assert_eq!(
            native["questions"]["billing"],
            json!({"type": "noul", "instructions": "Is this about billing?"})
        );
        assert_eq!(
            native["questions"]["tone"],
            json!({
                "type": "choice",
                "instructions": "What is the tone?",
                "criteria": {"angry": "upset or hostile", "calm": "neutral or polite"}
            })
        );
        assert_eq!(
            native["questions"]["urgency"],
            json!({
                "type": "score",
                "instructions": "How urgent is this?",
                "criteria": ["Can wait", "This week", "Today"]
            })
        );
        assert_eq!(
            native["questions"]["spam_probability"],
            json!({"type": "noul", "instructions": "Spam?"})
        );
    }

    #[test]
    fn typed_answers_render_as_the_schema_object() {
        let prompt = JevPrompt::plan(&schema_request(json!({
            "billing": {"type": "boolean"},
            "tone": {"type": "string", "enum": ["angry", "calm"]},
            "urgency": {"type": "integer", "minimum": 1, "maximum": 3},
            "spam_probability": {"type": "number", "minimum": 0, "maximum": 1}
        })))
        .unwrap();
        assert_eq!(
            native(&prompt)["questions"]["urgency"]["criteria"],
            json!(["1", "2", "3"])
        );
        assert_eq!(
            native(&prompt)["questions"]["tone"]["criteria"],
            json!({"angry": null, "calm": null})
        );

        let response: DecisionsResponse = serde_json::from_value(json!({
            "model": "jev-1.13.0",
            "answers": {
                "billing": {"type": "noul", "noul": 0.98},
                "tone": {"type": "choice", "choice": "angry", "confidence": 1.0, "probabilities": {"angry": 1.0, "calm": 0.0}},
                "urgency": {"type": "score", "score": 1.6, "confidence": 0.9, "legend": {}, "probabilities": {}},
                "spam_probability": {"type": "noul", "noul": 0.03}
            },
            "usage": {"input_tokens": 392, "output_tokens": 73}
        }))
        .unwrap();
        let completion = prompt.into_chat_completion(response).unwrap();

        assert_eq!(completion.object, "chat.completion");
        assert_eq!(completion.model, "jev-1.13.0");
        assert!(completion.id.starts_with("chatcmpl-"));
        let choice = &completion.choices[0];
        assert_eq!(choice.finish_reason.as_deref(), Some("stop"));
        assert_eq!(choice.message.role, "assistant");
        let content: Value =
            serde_json::from_str(choice.message.content.as_ref().unwrap().as_str().unwrap())
                .unwrap();
        assert_eq!(
            content,
            json!({"billing": true, "tone": "angry", "urgency": 3, "spam_probability": 0.03})
        );
        let usage = completion.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 392);
        assert_eq!(usage.completion_tokens, Some(73));
        assert_eq!(usage.total_tokens, 465);
    }

    #[test]
    fn conversations_become_role_tagged_state() {
        let prompt = JevPrompt::plan(&chat_request(json!({
            "model": "jev-latest",
            "messages": [
                {"role": "system", "content": "You triage support tickets."},
                {"role": "user", "content": [{"type": "text", "text": "Refund"}, {"type": "text", "text": "please"}]},
                {"role": "assistant", "content": "Noted."},
                {"role": "user", "content": "Now!"}
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": {"schema": {"type": "object", "properties": {"urgent": {"type": "boolean"}}}}
            }
        })))
        .unwrap();
        assert_eq!(
            native(&prompt)["state"],
            json!([
                {"role": "system", "content": "You triage support tickets."},
                {"role": "user", "content": "Refund\nplease"},
                {"role": "assistant", "content": "Noted."},
                {"role": "user", "content": "Now!"}
            ])
        );
        assert_eq!(
            native(&prompt)["questions"]["urgent"]["instructions"],
            "urgent"
        );
    }

    #[test]
    fn boolean_options_describe_noul_criteria_and_untyped_options_are_inferred() {
        let prompt = JevPrompt::plan(&schema_request(json!({
            "spam": {
                "type": "boolean",
                "oneOf": [
                    {"const": true, "description": "unsolicited advertising"},
                    {"const": false, "description": "a legitimate conversation"}
                ]
            },
            "team": {"enum": ["frontend", "payments"]},
            "severity": {"enum": [0, 1]}
        })))
        .unwrap();
        let native = native(&prompt);
        assert_eq!(
            native["questions"]["spam"]["criteria"],
            json!({"true": "unsolicited advertising", "false": "a legitimate conversation"})
        );
        assert_eq!(native["questions"]["team"]["type"], "choice");
        assert_eq!(native["questions"]["severity"]["type"], "score");
    }

    #[test]
    fn prompts_without_a_schema_or_with_tools_are_explained() {
        let error = JevPrompt::plan(&chat_request(json!({
            "model": "jev-latest",
            "messages": [{"role": "user", "content": "Is this spam?"}]
        })))
        .unwrap_err();
        assert!(
            error.starts_with("`response_format` must be a `json_schema`."),
            "{error}"
        );
        assert!(error.ends_with(PROMPTING_GUIDE), "{error}");

        let error = JevPrompt::plan(&chat_request(json!({
            "model": "jev-latest",
            "messages": [{"role": "user", "content": "Is this spam?"}],
            "tools": [{"type": "function", "function": {"name": "lookup"}}],
            "response_format": {"type": "json_schema", "json_schema": {"schema": {"type": "object", "properties": {"spam": {"type": "boolean"}}}}}
        })))
        .unwrap_err();
        assert!(error.starts_with("Jev does not call tools"), "{error}");
    }

    #[test]
    fn an_inconsistent_plan_is_an_internal_backend_error() {
        let mut prompt =
            JevPrompt::plan(&schema_request(json!({"flag":{"type":"boolean"}}))).unwrap();
        prompt.request.questions.clear();
        let error = crate::backend::typesafe::TypeSafeBackend::wire_prompt(&prompt).unwrap_err();
        assert!(
            matches!(&error, crate::backend::BackendError::Other(message) if message.contains("missing planned question"))
        );
        let response = crate::handlers::backend_error_response(&error);
        assert_eq!(
            response.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn odd_schema_containers_keep_the_legacy_validation_message() {
        for config in [Value::Null, json!(42), json!("odd"), json!([]), json!({})] {
            let request = chat_request(json!({
                "model":"jev", "messages":[{"role":"user","content":"hello"}],
                "response_format":{"type":"json_schema","json_schema":config}
            }));
            let error = JevPrompt::plan(&request).unwrap_err();
            assert!(
                error.starts_with(
                    "`response_format.json_schema.schema` must declare at least one property."
                ),
                "{error}"
            );
            assert!(error.ends_with(PROMPTING_GUIDE));
        }
    }

    #[test]
    fn unsupported_properties_name_the_field() {
        for (property, expected) in [
            (json!({"type": "string"}), "must list its options"),
            (
                json!({"type": "number"}),
                "must set `minimum: 0` and `maximum: 1`",
            ),
            (json!({"type": "integer"}), "must list its levels"),
            (
                json!({"type": "integer", "minimum": 0, "maximum": 20}),
                "between 2 and 10 levels",
            ),
            (
                json!({"type": "array", "items": {"type": "string"}}),
                "type `array` is not supported",
            ),
            (
                json!({"type": ["string", "integer"], "enum": ["a"]}),
                "declares several types",
            ),
            (
                json!({"type": "string", "oneOf": [{"description": "no const"}]}),
                "must declare a `const`",
            ),
        ] {
            let error = JevPrompt::plan(&schema_request(json!({"field": property}))).unwrap_err();
            assert!(error.starts_with("schema property `field`:"), "{error}");
            assert!(error.contains(expected), "{error}");
        }

        let error = JevPrompt::plan(&schema_request(json!({}))).unwrap_err();
        assert!(error.contains("at least one property"), "{error}");
    }

    #[test]
    fn image_content_is_rejected_before_reaching_typesafe() {
        let error = JevPrompt::plan(&chat_request(json!({
            "model": "jev-latest",
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
            ]}],
            "response_format": {"type": "json_schema", "json_schema": {"schema": {"type": "object", "properties": {"cat": {"type": "boolean"}}}}}
        })))
        .unwrap_err();
        assert!(
            error.contains("type `image_url` is not supported"),
            "{error}"
        );
    }

    #[test]
    fn malformed_answers_are_upstream_errors() {
        let prompt =
            JevPrompt::plan(&schema_request(json!({"billing": {"type": "boolean"}}))).unwrap();
        let response: DecisionsResponse = serde_json::from_value(json!({
            "model": "jev-1.13.0",
            "answers": {"billing": {"type": "noul"}},
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }))
        .unwrap();
        assert_eq!(
            prompt.clone().into_chat_completion(response).unwrap_err(),
            "TypeSafe answer for `billing` has no numeric `noul`"
        );

        let response: DecisionsResponse = serde_json::from_value(json!({
            "model": "jev-1.13.0",
            "answers": {},
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }))
        .unwrap();
        assert_eq!(
            prompt.into_chat_completion(response).unwrap_err(),
            "TypeSafe returned no answer for `billing`"
        );
    }
}
