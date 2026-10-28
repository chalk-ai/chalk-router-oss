//! Retained JSON with shared source storage and lazy provider envelopes.
//!
//! Child handles share both the original text and semantic tree. Object edits
//! copy only keys and handles; serialization writes the final envelope once.
//! Container indexes borrow raw ranges through serde, never copy subtrees.

use std::collections::HashSet;
use std::fmt;
use std::ops::{Deref, Range};
use std::sync::{Arc, OnceLock};

use serde::de::{MapAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{
    value::{to_raw_value, RawValue},
    Value,
};

#[derive(Clone)]
pub struct SchemaJson(Arc<Node>);

#[derive(Debug)]
struct Source {
    raw: Box<RawValue>,
    view: Value,
}
#[derive(Debug)]
enum PathElement {
    Key(String),
    Index(usize),
}
#[derive(Debug)]
struct ViewPath {
    parent: Option<Arc<ViewPath>>,
    entry: PathElement,
}
#[derive(Debug)]
enum Representation {
    Retained {
        source: Arc<Source>,
        range: Range<usize>,
        path: Option<Arc<ViewPath>>,
    },
    Object(Arc<ObjectData>),
    Array(Vec<SchemaJson>),
    Scalar(Value),
}
#[derive(Debug)]
struct Node {
    representation: Representation,
    view: OnceLock<Value>,
    object: OnceLock<Result<Arc<ObjectData>, String>>,
    array: OnceLock<Result<Vec<SchemaJson>, String>>,
}

#[derive(Debug)]
struct ObjectData {
    fields: Vec<(String, SchemaJson)>,
    original: Option<(Arc<Source>, Option<Arc<ViewPath>>)>,
    view: OnceLock<Value>,
}
impl ObjectData {
    fn new(fields: Vec<(String, SchemaJson)>) -> Self {
        Self {
            fields,
            original: None,
            view: OnceLock::new(),
        }
    }
    fn field(&self, key: &str) -> Option<&SchemaJson> {
        self.fields
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }
    fn with_member(&self, key: &str, value: SchemaJson) -> Arc<Self> {
        let mut fields: Vec<_> = effective_members(&self.fields)
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect();
        if let Some((_, existing)) = fields.iter_mut().find(|(name, _)| name == key) {
            *existing = value;
        } else {
            fields.push((key.to_string(), value));
        }
        Arc::new(Self::new(fields))
    }
    fn value(&self) -> &Value {
        if let Some((source, path)) = &self.original {
            return source_value(source, path);
        }
        self.view.get_or_init(|| object_view(&self.fields))
    }
}

/// An object validated once at ingress. Clones share fields and cached views;
/// scalar routing edits cannot fail and never copy unchanged payloads.
#[derive(Clone)]
pub struct SchemaObject(Arc<ObjectData>);
impl SchemaObject {
    pub fn field(&self, key: &str) -> Option<&SchemaJson> {
        self.0.field(key)
    }
    pub fn set_member(&mut self, key: &str, value: SchemaJson) {
        self.0 = self.0.with_member(key, value);
    }
    pub fn value(&self) -> &Value {
        self.0.value()
    }
    pub fn retained(&self) -> SchemaJson {
        SchemaJson::new(Representation::Object(self.0.clone()))
    }
}
impl Deref for SchemaObject {
    type Target = Value;
    fn deref(&self) -> &Value {
        self.value()
    }
}
impl fmt::Debug for SchemaObject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value().fmt(formatter)
    }
}
impl Serialize for SchemaObject {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_object(&self.0.fields, serializer)
    }
}

fn source_value<'a>(source: &'a Source, path: &Option<Arc<ViewPath>>) -> &'a Value {
    fn lookup<'a>(root: &'a Value, path: &ViewPath) -> &'a Value {
        let parent = path
            .parent
            .as_ref()
            .map_or(root, |parent| lookup(root, parent));
        match &path.entry {
            PathElement::Key(key) => &parent[key],
            PathElement::Index(index) => &parent[*index],
        }
    }
    path.as_ref()
        .map_or(&source.view, |path| lookup(&source.view, path))
}
impl PartialEq for SchemaObject {
    fn eq(&self, other: &Self) -> bool {
        self.value() == other.value()
    }
}

fn object_view(fields: &[(String, SchemaJson)]) -> Value {
    Value::Object(
        fields
            .iter()
            .map(|(key, value)| (key.clone(), value.owned_view()))
            .collect(),
    )
}
fn serialize_object<S: Serializer>(
    fields: &[(String, SchemaJson)],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let members = effective_members(fields);
    let mut object = serializer.serialize_map(Some(members.len()))?;
    for (key, value) in members {
        object.serialize_entry(key, value)?;
    }
    object.end()
}
fn effective_members(fields: &[(String, SchemaJson)]) -> Vec<(&str, &SchemaJson)> {
    let mut seen = HashSet::new();
    let mut members: Vec<_> = fields
        .iter()
        .rev()
        .filter(|(key, _)| seen.insert(key.as_str()))
        .map(|(key, value)| (key.as_str(), value))
        .collect();
    members.reverse();
    members
}
fn serialize_array<S: Serializer>(values: &[SchemaJson], serializer: S) -> Result<S::Ok, S::Error> {
    let mut array = serializer.serialize_seq(Some(values.len()))?;
    for value in values {
        array.serialize_element(value)?;
    }
    array.end()
}
fn invalid(message: &str) -> serde_json::Error {
    <serde_json::Error as serde::de::Error>::custom(message)
}

impl SchemaJson {
    /// Replace only retained string tokens at known object paths. Unlike normal
    /// serialization, this keeps every other byte (including whitespace,
    /// duplicate unrelated members, and numeric spellings) unchanged.
    pub(crate) fn rewrite_string_members(
        input: &str,
        paths: &[&[&str]],
        rewrite: impl Fn(&str) -> String,
    ) -> serde_json::Result<Option<String>> {
        let root: Self = serde_json::from_str(input)?;
        let leading = input.len() - input.trim_start().len();
        let mut replacements = Vec::new();
        'paths: for path in paths {
            let mut member = &root;
            for key in *path {
                if !member.is_object() {
                    continue 'paths;
                }
                match member.member_ref(key)? {
                    Some(child) => member = child,
                    None => continue 'paths,
                }
            }
            if let (Some(value), Representation::Retained { range, .. }) =
                (member.value().as_str(), &member.0.representation)
            {
                let replacement = rewrite(value);
                if replacement != value {
                    replacements.push((
                        range.start + leading..range.end + leading,
                        serde_json::to_string(&replacement)?,
                    ));
                }
            }
        }
        if replacements.is_empty() {
            return Ok(None);
        }
        replacements.sort_by_key(|(range, _)| range.start);
        let mut output = String::with_capacity(input.len());
        let mut cursor = 0;
        for (range, replacement) in replacements {
            if range.start < cursor {
                return Err(invalid("overlapping retained string replacements"));
            }
            output.push_str(&input[cursor..range.start]);
            output.push_str(&replacement);
            cursor = range.end;
        }
        output.push_str(&input[cursor..]);
        Ok(Some(output))
    }

    fn new(representation: Representation) -> Self {
        Self(Arc::new(Node {
            representation,
            view: OnceLock::new(),
            object: OnceLock::new(),
            array: OnceLock::new(),
        }))
    }
    /// Semantic inspection only; serialization uses the retained representation.
    pub fn value(&self) -> &Value {
        match &self.0.representation {
            Representation::Retained { source, path, .. } => source_value(source, path),
            Representation::Scalar(value) => value,
            Representation::Object(object) => object.value(),
            Representation::Array(_) => self.0.view.get_or_init(|| self.owned_view()),
        }
    }
    // Build a semantic view in one traversal, without populating a copy at every
    // ancestor. Retained subtrees already have a shared validated semantic view.
    fn owned_view(&self) -> Value {
        match &self.0.representation {
            Representation::Object(object) => object_view(&object.fields),
            Representation::Array(values) => {
                Value::Array(values.iter().map(Self::owned_view).collect())
            }
            _ => self.value().clone(),
        }
    }
    pub fn is_object(&self) -> bool {
        match &self.0.representation {
            Representation::Object(_) => true,
            Representation::Retained { source, range, .. } => {
                source.raw.get()[range.clone()].starts_with('{')
            }
            Representation::Scalar(_) | Representation::Array(_) => false,
        }
    }
    pub fn is_array(&self) -> bool {
        match &self.0.representation {
            Representation::Array(_) => true,
            Representation::Retained { source, range, .. } => {
                source.raw.get()[range.clone()].starts_with('[')
            }
            Representation::Scalar(_) | Representation::Object(_) => false,
        }
    }
    pub fn from_serializable(value: &impl Serialize) -> serde_json::Result<Self> {
        Self::from_raw(to_raw_value(value)?)
    }
    fn from_raw(raw: Box<RawValue>) -> serde_json::Result<Self> {
        let view = serde_json::from_str(raw.get())?;
        let range = 0..raw.get().len();
        Ok(Self::new(Representation::Retained {
            source: Arc::new(Source { raw, view }),
            range,
            path: None,
        }))
    }
    fn child(
        source: &Arc<Source>,
        path: &Option<Arc<ViewPath>>,
        raw: &RawValue,
        entry: PathElement,
    ) -> Self {
        let start = raw.get().as_ptr() as usize - source.raw.get().as_ptr() as usize;
        let path = Some(Arc::new(ViewPath {
            parent: path.clone(),
            entry,
        }));
        Self::new(Representation::Retained {
            source: source.clone(),
            range: start..start + raw.get().len(),
            path,
        })
    }
    fn object_data(&self) -> serde_json::Result<&Arc<ObjectData>> {
        if let Representation::Object(object) = &self.0.representation {
            return Ok(object);
        }
        self.0
            .object
            .get_or_init(|| match &self.0.representation {
                Representation::Retained {
                    source,
                    range,
                    path,
                } if self.is_object() => {
                    let fields: BorrowedObject<'_> =
                        serde_json::from_str(&source.raw.get()[range.clone()])
                            .map_err(|error| error.to_string())?;
                    let fields = fields
                        .0
                        .into_iter()
                        .map(|(key, raw)| {
                            let child =
                                Self::child(source, path, raw, PathElement::Key(key.clone()));
                            (key, child)
                        })
                        .collect();
                    Ok(Arc::new(ObjectData {
                        fields,
                        original: Some((source.clone(), path.clone())),
                        view: OnceLock::new(),
                    }))
                }
                _ => Err("expected a JSON object".to_string()),
            })
            .as_ref()
            .map_err(|message| invalid(message))
    }
    fn object_fields(&self) -> serde_json::Result<&[(String, Self)]> {
        Ok(&self.object_data()?.fields)
    }
    /// Missing members and invalid containers are distinct. Returned children
    /// are shared handles, not copies of raw text or semantic subtrees.
    pub fn member(&self, key: &str) -> serde_json::Result<Option<Self>> {
        Ok(self.member_ref(key)?.cloned())
    }
    /// Borrow the last occurrence of a member from the cached object index.
    pub fn member_ref(&self, key: &str) -> serde_json::Result<Option<&Self>> {
        Ok(self.object_data()?.field(key))
    }
    pub fn members_ref(&self) -> serde_json::Result<Vec<(&str, &Self)>> {
        Ok(effective_members(self.object_fields()?))
    }
    /// Effective members in declaration order; last duplicate occurrence wins.
    pub fn members(&self) -> serde_json::Result<Vec<(String, Self)>> {
        Ok(self
            .members_ref()?
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect())
    }
    fn array_elements(&self) -> serde_json::Result<&[Self]> {
        if let Representation::Array(values) = &self.0.representation {
            return Ok(values);
        }
        self.0
            .array
            .get_or_init(|| match &self.0.representation {
                Representation::Retained {
                    source,
                    range,
                    path,
                } if self.is_array() => {
                    let raws: Vec<&RawValue> =
                        serde_json::from_str(&source.raw.get()[range.clone()])
                            .map_err(|error| error.to_string())?;
                    Ok(raws
                        .into_iter()
                        .enumerate()
                        .map(|(index, raw)| {
                            Self::child(source, path, raw, PathElement::Index(index))
                        })
                        .collect())
                }
                _ => Err("expected a JSON array".to_string()),
            })
            .as_ref()
            .map(Vec::as_slice)
            .map_err(|message| invalid(message))
    }
    pub fn elements(&self) -> serde_json::Result<Vec<Self>> {
        Ok(self.array_elements()?.to_vec())
    }
    pub fn object<K: Into<String>>(fields: impl IntoIterator<Item = (K, Self)>) -> Self {
        Self::new(Representation::Object(Arc::new(ObjectData::new(
            fields
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
        ))))
    }
    pub fn array(values: impl IntoIterator<Item = Self>) -> Self {
        Self::new(Representation::Array(values.into_iter().collect()))
    }
    pub fn into_object(self) -> serde_json::Result<SchemaObject> {
        Ok(SchemaObject(self.object_data()?.clone()))
    }
    pub fn set_member(&mut self, key: &str, value: Self) -> serde_json::Result<()> {
        *self = Self::new(Representation::Object(
            self.object_data()?.with_member(key, value),
        ));
        Ok(())
    }
    pub fn remove_member(&mut self, key: &str) -> serde_json::Result<()> {
        let mut fields = self.members()?;
        fields.retain(|(name, _)| name != key);
        *self = Self::object(fields);
        Ok(())
    }
    pub fn normalize_strict(&mut self) -> serde_json::Result<()> {
        *self = normalize(self, SchemaContext::Schema)?;
        Ok(())
    }
    pub fn normalize_response_format(&mut self) -> serde_json::Result<()> {
        if let Some(mut config) = self.member("json_schema")? {
            if let Some(mut schema) = config.member("schema")? {
                schema.normalize_strict()?;
                config.set_member("schema", schema)?;
            }
            self.set_member("json_schema", config)?;
        }
        Ok(())
    }
    /// Formatting and ordinary map order are canonicalized; actual schema
    /// property dictionaries retain declaration order. Duplicates are uncacheable.
    pub fn canonical_for_cache(&self) -> serde_json::Result<Self> {
        self.canonical(SchemaContext::Schema)
    }
    pub fn canonical_response_format_for_cache(&self) -> serde_json::Result<Self> {
        self.canonical(SchemaContext::ResponseFormat)
    }
    fn canonical(&self, context: SchemaContext) -> serde_json::Result<Self> {
        if self.is_object() {
            let mut seen = HashSet::new();
            if self
                .object_fields()?
                .iter()
                .any(|(key, _)| !seen.insert(key))
            {
                return Err(invalid("duplicate schema member"));
            }
            let mut fields = self.members_ref()?;
            if !matches!(context, SchemaContext::Properties) {
                fields.sort_by(|a, b| a.0.cmp(b.0));
            }
            Ok(Self::object(
                fields
                    .into_iter()
                    .map(|(key, value)| Ok((key.to_string(), value.canonical(context.child(key))?)))
                    .collect::<serde_json::Result<Vec<_>>>()?,
            ))
        } else if self.is_array() {
            Ok(Self::array(
                self.array_elements()?
                    .iter()
                    .map(|value| value.canonical(context.array_child()))
                    .collect::<serde_json::Result<Vec<_>>>()?,
            ))
        } else {
            Ok(self.value().clone().into())
        }
    }
    /// Telemetry copy: keep original order/tokens and remove encrypted content
    /// at every depth. Unaffected subtrees remain shared.
    pub fn redact_encrypted_content(&self) -> serde_json::Result<Self> {
        if self.is_object() {
            Ok(Self::object(
                self.members_ref()?
                    .into_iter()
                    .filter(|(key, _)| *key != "encrypted_content")
                    .map(|(key, value)| Ok((key.to_string(), value.redact_encrypted_content()?)))
                    .collect::<serde_json::Result<Vec<_>>>()?,
            ))
        } else if self.is_array() {
            Ok(Self::array(
                self.array_elements()?
                    .iter()
                    .map(Self::redact_encrypted_content)
                    .collect::<serde_json::Result<Vec<_>>>()?,
            ))
        } else {
            Ok(self.clone())
        }
    }
}

#[derive(Clone, Copy)]
enum SchemaContext {
    ResponseFormat,
    JsonSchemaConfig,
    Schema,
    Properties,
    Definitions,
    SchemaArray,
    Data,
}
impl SchemaContext {
    fn child(self, key: &str) -> Self {
        match self {
            Self::ResponseFormat if key == "json_schema" => Self::JsonSchemaConfig,
            Self::JsonSchemaConfig if key == "schema" => Self::Schema,
            Self::Properties | Self::Definitions => Self::Schema,
            Self::Schema => match key {
                "properties" => Self::Properties,
                "$defs" | "definitions" | "patternProperties" | "dependentSchemas"
                | "dependencies" => Self::Definitions,
                "items" => Self::Schema,
                "allOf" | "anyOf" | "oneOf" | "prefixItems" => Self::SchemaArray,
                "contentSchema"
                | "additionalProperties"
                | "unevaluatedProperties"
                | "additionalItems"
                | "unevaluatedItems"
                | "contains"
                | "propertyNames"
                | "not"
                | "if"
                | "then"
                | "else" => Self::Schema,
                _ => Self::Data,
            },
            _ => Self::Data,
        }
    }
    fn array_child(self) -> Self {
        match self {
            Self::SchemaArray | Self::Schema => Self::Schema,
            _ => Self::Data,
        }
    }
}

impl fmt::Debug for SchemaJson {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value().fmt(formatter)
    }
}

impl Deref for SchemaJson {
    type Target = Value;
    fn deref(&self) -> &Value {
        self.value()
    }
}
impl From<Value> for SchemaJson {
    fn from(value: Value) -> Self {
        match value {
            Value::Object(fields) => {
                Self::object(fields.into_iter().map(|(key, value)| (key, value.into())))
            }
            Value::Array(values) => Self::array(values.into_iter().map(Self::from)),
            value => Self::new(Representation::Scalar(value)),
        }
    }
}
impl PartialEq for SchemaJson {
    fn eq(&self, other: &Self) -> bool {
        self.value() == other.value()
    }
}
impl PartialEq<Value> for SchemaJson {
    fn eq(&self, other: &Value) -> bool {
        self.value() == other
    }
}
impl Serialize for SchemaJson {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Containers use the same last-value-wins view as routing. Keep scalar
        // source tokens so number spelling and schema declaration order survive.
        match &self.0.representation {
            Representation::Retained {
                source,
                range,
                path,
            } => {
                if self.is_object() {
                    serialize_object(
                        self.object_fields().map_err(serde::ser::Error::custom)?,
                        serializer,
                    )
                } else if self.is_array() {
                    serialize_array(
                        self.array_elements().map_err(serde::ser::Error::custom)?,
                        serializer,
                    )
                } else if path.is_none() {
                    source.raw.serialize(serializer)
                } else {
                    let raw: &RawValue = serde_json::from_str(&source.raw.get()[range.clone()])
                        .map_err(serde::ser::Error::custom)?;
                    raw.serialize(serializer)
                }
            }
            Representation::Scalar(value) => value.serialize(serializer),
            Representation::Object(object) => serialize_object(&object.fields, serializer),
            Representation::Array(values) => serialize_array(values, serializer),
        }
    }
}
impl<'de> Deserialize<'de> for SchemaJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::from_raw(Box::<RawValue>::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}
struct BorrowedObject<'a>(Vec<(String, &'a RawValue)>);
impl<'de> Deserialize<'de> for BorrowedObject<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor;
        impl<'de> Visitor<'de> for ObjectVisitor {
            type Value = BorrowedObject<'de>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut fields = Vec::new();
                while let Some(entry) = map.next_entry::<String, &'de RawValue>()? {
                    fields.push(entry);
                }
                Ok(BorrowedObject(fields))
            }
        }
        deserializer.deserialize_map(ObjectVisitor)
    }
}

// Construct shared output nodes and serialize only once at the wire boundary.
fn normalize(value: &SchemaJson, context: SchemaContext) -> serde_json::Result<SchemaJson> {
    if matches!(context, SchemaContext::Data) {
        return Ok(value.clone());
    }
    if value.is_object() {
        let mut fields = value.members()?;
        if matches!(context, SchemaContext::Schema) {
            if fields.iter().any(|(key, _)| key == "anyOf") {
                fields.retain(|(key, _)| key != "oneOf");
            } else if let Some((key, _)) = fields.iter_mut().find(|(key, _)| key == "oneOf") {
                *key = "anyOf".to_string();
            }
            let is_object = value
                .member("type")?
                .is_some_and(|kind| kind.as_str() == Some("object"))
                || fields.iter().any(|(key, _)| key == "properties");
            if is_object {
                let required = fields
                    .iter()
                    .find(|(key, _)| key == "properties")
                    .filter(|(_, props)| props.is_object())
                    .map(|(_, props)| {
                        props.members_ref().map(|members| {
                            Value::Array(
                                members
                                    .into_iter()
                                    .map(|(key, _)| Value::String(key.to_string()))
                                    .collect(),
                            )
                        })
                    })
                    .transpose()?;
                for (key, replacement) in
                    std::iter::once(("additionalProperties", Value::Bool(false)))
                        .chain(required.map(|required| ("required", required)))
                {
                    if let Some((_, existing)) = fields.iter_mut().find(|(name, _)| name == key) {
                        *existing = replacement.into();
                    } else {
                        fields.push((key.to_string(), replacement.into()));
                    }
                }
            }
        }
        Ok(SchemaJson::object(
            fields
                .into_iter()
                .map(|(key, child)| {
                    let child = normalize(&child, context.child(&key))?;
                    Ok((key, child))
                })
                .collect::<serde_json::Result<Vec<_>>>()?,
        ))
    } else if value.is_array() {
        Ok(SchemaJson::array(
            value
                .array_elements()?
                .iter()
                .map(|child| normalize(child, context.array_child()))
                .collect::<serde_json::Result<Vec<_>>>()?,
        ))
    } else {
        Ok(value.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn locator_string_edits_preserve_unrelated_wire_bytes_and_escape_replacements() {
        let input = " \n{\"response\":{\"\\u0069d\":\"old\\u002did\",\"future\":1.0000000000000001},\"response_id\":\"other\",\"id\":\"message-id\",\"duplicate\":1,\"duplicate\":2} \n";
        let output = SchemaJson::rewrite_string_members(
            input,
            &[&["response", "id"], &["response_id"]],
            |id| format!("wrapped-\"{id}"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            output,
            input
                .replace("\"old\\u002did\"", "\"wrapped-\\\"old-id\"")
                .replace("\"other\"", "\"wrapped-\\\"other\"")
        );
        assert_eq!(
            serde_json::from_str::<Value>(&output).unwrap()["response"]["id"],
            "wrapped-\"old-id"
        );
    }

    #[test]
    fn locator_string_edits_leave_missing_non_string_and_non_object_paths_untouched() {
        for input in [
            r#"{"response":"opaque","response_id":null}"#,
            r#"{"response":{"id":12}}"#,
            r#"{"response":{"other":"opaque"}}"#,
            r#""opaque""#,
        ] {
            assert!(SchemaJson::rewrite_string_members(
                input,
                &[&["response", "id"], &["response_id"]],
                |_| "changed".into()
            )
            .unwrap()
            .is_none());
        }
    }

    #[test]
    fn validated_object_clones_share_cached_views_and_mutate_independently() {
        let value: SchemaJson = serde_json::from_str(
            r#"{"model":"original","model":"public","input":{"z":1.2300e-4,"a":null}}"#,
        )
        .unwrap();
        let mut object = value.clone().into_object().unwrap();
        assert!(Arc::ptr_eq(value.object_data().unwrap(), &object.0));
        assert!(std::ptr::eq(value.value(), object.value()));
        assert!(object.retained().canonical_for_cache().is_err());
        object.set_member("model", json!("route").into());
        let clone = object.clone();
        let view = object.value();
        assert!(std::ptr::eq(view, clone.value()));
        assert!(std::ptr::eq(view, object.retained().value()));
        let input = clone.field("input").unwrap().clone();
        object.set_member("model", json!("alternate").into());
        assert_eq!(clone.value()["model"], "route");
        assert_eq!(object.value()["model"], "alternate");
        assert!(Arc::ptr_eq(&input.0, &object.field("input").unwrap().0));
        assert_eq!(
            serde_json::to_string(&object).unwrap(),
            r#"{"model":"alternate","input":{"z":1.2300e-4,"a":null}}"#
        );
    }

    #[test]
    fn wire_containers_are_compact_and_unique_without_losing_order_or_number_tokens() {
        let raw = r#"{
            "type": "json_schema", "t\u0079pe": "text",
            "nested": [{ "z": null, "a": 1, "z": 1.2300e-4 }],
            "properties": { "oneOf": { "type": "string" }, "type": {} }
        }"#;
        let value: SchemaJson = serde_json::from_str(raw).unwrap();
        let expected = r#"{"type":"text","nested":[{"a":1,"z":1.2300e-4}],"properties":{"oneOf":{"type":"string"},"type":{}}}"#;
        for wire in [
            serde_json::to_string(&value).unwrap(),
            serde_json::to_string(&value.clone().into_object().unwrap()).unwrap(),
        ] {
            assert_eq!(wire, expected);
            assert_eq!(
                serde_json::from_str::<Value>(&wire).unwrap(),
                *value.value()
            );
        }
        // Forwarding must not erase the original duplicate evidence for caching.
        assert!(value.canonical_for_cache().is_err());
        let nested: SchemaJson =
            serde_json::from_str(r#"{"properties":{"z":{"type":"number","type":"string"}}}"#)
                .unwrap();
        serde_json::to_string(&nested).unwrap();
        assert!(nested.canonical_for_cache().is_err());
    }

    #[test]
    fn constructed_objects_also_forward_last_duplicate_members() {
        let value = SchemaJson::object([
            ("z".to_string(), json!(1).into()),
            ("a".to_string(), json!(null).into()),
            ("z".to_string(), json!(2).into()),
        ]);
        assert_eq!(value["z"], 2);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"{"a":null,"z":2}"#
        );
        assert!(value.canonical_for_cache().is_err());
    }

    #[test]
    fn strict_normalization_distinguishes_dictionaries_and_data_from_schema_nodes() {
        let mut schema: SchemaJson = serde_json::from_str(
            r#"{
            "type":"object",
            "properties":{
                "type":{"type":"string"},
                "properties":{"type":"string"},
                "oneOf":{"type":"string"},
                "real_union":{"oneOf":[{"properties":{"oneOf":{},"type":{}}}]}
            },
            "$defs":{"properties":{"type":"object"},"oneOf":{"oneOf":[{"type":"string"}]}},
            "patternProperties":{"oneOf":{"type":"object"}},
            "dependentSchemas":{"properties":{"type":"object"}},
            "default":{"type":"object","properties":{"oneOf":{}},"oneOf":[{}]},
            "enum":[{"type":"object","properties":{},"oneOf":[{}]}],
            "const":{"properties":{},"oneOf":[{}]},
            "examples":[{"properties":{},"oneOf":[{}]}],
            "unknown":{"type":"object","properties":{},"oneOf":[{}]}
        }"#,
        )
        .unwrap();
        let original = schema.value().clone();
        schema.normalize_strict().unwrap();
        assert_eq!(
            schema["required"],
            json!(["type", "properties", "oneOf", "real_union"])
        );
        assert_eq!(schema["additionalProperties"], false);
        let properties = &schema["properties"];
        assert_eq!(properties.as_object().unwrap().len(), 4);
        for name in ["type", "properties", "oneOf"] {
            assert_eq!(properties[name], original["properties"][name]);
        }
        assert_eq!(
            properties["real_union"]["anyOf"][0]["required"],
            json!(["oneOf", "type"])
        );
        assert!(properties["real_union"].get("oneOf").is_none());
        assert_eq!(schema["$defs"]["properties"]["additionalProperties"], false);
        assert!(schema["$defs"].get("required").is_none());
        assert!(schema["$defs"].get("anyOf").is_none());
        assert_eq!(
            schema["$defs"]["oneOf"]["anyOf"],
            json!([{"type":"string"}])
        );
        assert_eq!(
            schema["patternProperties"]["oneOf"]["additionalProperties"],
            false
        );
        assert_eq!(
            schema["dependentSchemas"]["properties"]["additionalProperties"],
            false
        );
        for name in ["default", "enum", "const", "examples", "unknown"] {
            assert_eq!(
                schema[name], original[name],
                "embedded data changed: {name}"
            );
        }
        let wire = serde_json::to_string(&schema).unwrap();
        assert!(wire.contains(r#""properties":{"type":{"type":"string"},"properties":{"type":"string"},"oneOf":{"type":"string"}"#));
    }
    #[test]
    fn edits_share_large_subtrees_and_leave_views_lazy() {
        let original: SchemaJson = serde_json::from_str(&format!(
            r#"{{"model":"public","input":{{"blob":"{}","schema":{{"z":1.2300e-4,"a":null}}}}}}"#,
            "x".repeat(1024 * 1024)
        ))
        .unwrap();
        let input = original.member("input").unwrap().unwrap();
        assert!(std::ptr::eq(input.value(), &original["input"]));
        let mut edited = original.clone();
        for index in 0..8 {
            edited
                .set_member("model", json!(format!("route-{index}")).into())
                .unwrap();
        }
        let after = edited.member("input").unwrap().unwrap();
        assert!(Arc::ptr_eq(&input.0, &after.0));
        assert!(edited.object_data().unwrap().view.get().is_none());
        let wire = serde_json::to_string(&edited).unwrap();
        assert!(wire.contains("1.2300e-4"));
        assert!(edited.object_data().unwrap().view.get().is_none());
        assert_eq!(original["model"], "public");
        assert_eq!(edited["model"], "route-7");
    }

    #[test]
    fn child_indexes_are_reused_and_share_the_source() {
        let value: SchemaJson =
            serde_json::from_str(r#"{"tools":[{"z":1,"a":2},{"b":3}]}"#).unwrap();
        let tools = value.member("tools").unwrap().unwrap();
        let first = tools.elements().unwrap();
        let second = tools.elements().unwrap();
        assert!(Arc::ptr_eq(&first[0].0, &second[0].0));
        assert!(std::ptr::eq(first[0].value(), &value["tools"][0]));
        assert_eq!(
            serde_json::to_string(&first[0]).unwrap(),
            r#"{"z":1,"a":2}"#
        );
    }

    fn canonical(raw: &str) -> String {
        let value: SchemaJson = serde_json::from_str(raw).unwrap();
        serde_json::to_string(&value.canonical_for_cache().unwrap()).unwrap()
    }

    #[test]
    fn cache_context_distinguishes_property_names_from_schema_keywords() {
        let a = r#"{"properties":{"properties":{"description":"x","type":"object","properties":{"z":{},"a":{}}}}}"#;
        let b = r#"{"properties":{"properties":{"properties":{"z":{},"a":{}},"type":"object","description":"x"}}}"#;
        assert_eq!(canonical(a), canonical(b));
        assert_ne!(
            canonical(a),
            canonical(&b.replace(r#""z":{},"a":{}"#, r#""a":{},"z":{}"#))
        );
    }

    #[test]
    fn cache_data_and_definitions_sort_names_but_preserve_nested_schema_order() {
        let a = r#"{"default":{"properties":{"z":1,"a":2}},"const":{"properties":{"z":1,"a":2}},"enum":[{"properties":{"z":1,"a":2}}],"$defs":{"z":{"properties":{"b":{},"a":{}}},"a":{}}}"#;
        let b = r#"{"$defs":{"a":{},"z":{"properties":{"b":{},"a":{}}}},"enum":[{"properties":{"a":2,"z":1}}],"const":{"properties":{"a":2,"z":1}},"default":{"properties":{"a":2,"z":1}}}"#;
        assert_eq!(canonical(a), canonical(b));
        assert_ne!(
            canonical(a),
            canonical(&b.replace(r#""b":{},"a":{}"#, r#""a":{},"b":{}"#))
        );
        let output: SchemaJson = serde_json::from_str(r#"{"type":"json_schema","json_schema":{"extra":{"properties":{"z":1,"a":2}},"schema":{"properties":{"z":{},"a":{}}}}}"#).unwrap();
        let encoded =
            serde_json::to_string(&output.canonical_response_format_for_cache().unwrap()).unwrap();
        assert!(encoded.contains(r#""extra":{"properties":{"a":2,"z":1}}"#));
        assert!(encoded.contains(r#""schema":{"properties":{"z":{},"a":{}}}"#));
    }

    #[test]
    fn retained_redaction_removes_escaped_keys_and_shadowed_secrets() {
        let value: SchemaJson = serde_json::from_str(r#"{"items":[{"hidden":{"encrypted_content":"shadowed"},"hidden":{},"encrypted\u005fcontent":"secret","safe":1.2300e-4}],"tools":{"properties":{"z":{},"a":{}}}}"#).unwrap();
        let wire = serde_json::to_string(&value.redact_encrypted_content().unwrap()).unwrap();
        assert!(!wire.contains("shadowed"));
        assert!(!wire.contains("secret"));
        assert!(!wire.contains("encrypted"));
        assert!(wire.contains("1.2300e-4"));
        assert!(wire.contains(r#""properties":{"z":{},"a":{}}"#));
    }

    #[test]
    fn constructed_provider_envelopes_do_not_reparse_deep_validated_children() {
        let mut value = serde_json::json!({"minimum":1});
        for _ in 0..120 {
            value = serde_json::json!({"nested":value});
        }
        let mut schema: SchemaJson = value.into();
        for _ in 0..16 {
            schema = SchemaJson::object([("wrapper".to_string(), schema)]);
        }
        assert!(serde_json::to_string(&schema)
            .unwrap()
            .contains(r#""minimum":1"#));
    }

    #[test]
    fn duplicate_members_use_the_last_value_for_lookup_and_normalization() {
        let mut format: SchemaJson = serde_json::from_str(r#"{"type":"json_schema","json_schema":null,"json_schema":{"strict":true,"schema":{"type":"object","properties":{"z":{"minimum":1.2300e-4},"a":{}},"required":["a"]}}}"#).unwrap();
        assert!(format.member("json_schema").unwrap().unwrap()["strict"] == true);
        assert!(format.canonical_for_cache().is_err());
        format.normalize_response_format().unwrap();
        let wire = serde_json::to_string(&format).unwrap();
        assert_eq!(wire.matches("\"json_schema\":").count(), 1);
        assert!(wire.contains(r#""required":["z","a"]"#));
        assert!(wire.contains("1.2300e-4"));
        assert_eq!(
            format["json_schema"]["schema"]["additionalProperties"],
            false
        );
    }

    #[test]
    fn member_distinguishes_missing_members_from_invalid_containers() {
        let value: SchemaJson = serde_json::from_str(r#"{"z":{},"a":{"x":1}}"#).unwrap();
        assert!(value.member("missing").unwrap().is_none());
        assert_eq!(
            value
                .member("a")
                .unwrap()
                .unwrap()
                .member("x")
                .unwrap()
                .unwrap(),
            json!(1)
        );
        for raw in ["null", "[]", "1", "true", r#""text""#] {
            let scalar: SchemaJson = serde_json::from_str(raw).unwrap();
            assert!(scalar.member("schema").is_err());
        }
    }

    #[test]
    fn mutation_updates_both_views_and_deduplicates_effective_members() {
        let mut value: SchemaJson = serde_json::from_str(r#"{"z":0,"a":1,"z":2}"#).unwrap();
        assert_eq!(value.member("z").unwrap().unwrap(), json!(2));
        value.set_member("z", json!(3).into()).unwrap();
        assert_eq!(value["z"], 3);
        assert_eq!(serde_json::to_string(&value).unwrap(), r#"{"a":1,"z":3}"#);
        value.remove_member("z").unwrap();
        assert!(value.get("z").is_none());
        assert_eq!(serde_json::to_string(&value).unwrap(), r#"{"a":1}"#);
    }

    #[test]
    fn strict_normalization_keeps_existing_anyof_and_nested_scalar_tokens() {
        let mut value: SchemaJson = serde_json::from_str(r#"{"oneOf":[{"type":"integer"}],"anyOf":[{"type":"number","minimum":1.2300e-4}],"properties":{"z":{"items":[{"properties":{"q":{},"b":{}}}]},"a":{}},"required":["a"]}"#).unwrap();
        value.normalize_strict().unwrap();
        let wire = serde_json::to_string(&value).unwrap();
        assert!(!wire.contains("oneOf"));
        assert!(wire.contains(r#""anyOf":[{"type":"number","minimum":1.2300e-4}]"#));
        assert!(wire.contains(r#""required":["z","a"]"#));
        assert!(wire.contains(r#""required":["q","b"]"#));
    }

    #[test]
    fn openai_strict_adds_additional_properties_and_required() {
        let mut schema: crate::schema_json::SchemaJson = json!({
            "type": "object",
            "properties": {
                "a": {"type": "string"},
                "b": {"type": "integer"},
                "nested": {
                    "type": "object",
                    "properties": {"c": {"type": "string"}}
                }
            },
            "required": ["a"]
        })
        .into();
        schema.normalize_strict().unwrap();
        assert_eq!(schema["additionalProperties"], json!(false));
        let mut req: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        req.sort();
        assert_eq!(req, vec!["a", "b", "nested"]);
        assert_eq!(
            schema["properties"]["nested"]["additionalProperties"],
            json!(false)
        );
        assert_eq!(schema["properties"]["nested"]["required"], json!(["c"]));
    }

    #[test]
    fn oneof_becomes_anyof() {
        let mut schema: crate::schema_json::SchemaJson =
            json!({"oneOf": [{"type": "string"}, {"type": "integer"}]}).into();
        schema.normalize_strict().unwrap();
        assert!(schema.get("oneOf").is_none());
        assert_eq!(
            schema["anyOf"],
            json!([{"type": "string"}, {"type": "integer"}])
        );
    }

    #[test]
    fn raw_schema_and_numbers_survive_while_ordinary_values_stay_sorted() {
        let raw =
            r#"{"properties":{"z":{"minimum":1.2300e-4},"a":{"properties":{"yy":{},"bb":{}}}}}"#;
        let schema: SchemaJson = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_string(&schema).unwrap(), raw);
        assert_eq!(
            serde_json::to_string(&schema.value()).unwrap(),
            r#"{"properties":{"a":{"properties":{"bb":{},"yy":{}}},"z":{"minimum":0.000123}}}"#
        );
    }

    #[test]
    fn strict_normalization_preserves_recursive_properties_and_required_order() {
        let mut schema: SchemaJson = serde_json::from_str(r#"{"type":"object","properties":{"z":{"oneOf":[{"type":"string"},{"type":"null"}]},"a":{"type":"array","items":{"properties":{"yy":{},"bb":{}}}},"m":{"$defs":{"nested":{"properties":{"q":{},"c":{}}}}}}}"#).unwrap();
        schema.normalize_strict().unwrap();
        let wire = serde_json::to_string(&schema).unwrap();
        assert!(wire.contains(r#""required":["z","a","m"]"#));
        assert!(wire.contains(r#""properties":{"yy":{},"bb":{}}"#));
        assert!(wire.contains(r#""required":["yy","bb"]"#));
        assert!(wire.contains(r#""required":["q","c"]"#));
        assert!(wire.contains("\"anyOf\""));
        assert!(!wire.contains("\"oneOf\""));
    }
}
