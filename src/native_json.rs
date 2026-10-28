//! JSON forwarding with an independent typed observation view.
use crate::backend::BackendError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{value::RawValue, Value};
use std::{collections::BTreeMap, ops::Deref};

#[derive(Debug, Clone)]
pub struct NativeJsonRequest<T> {
    fields: BTreeMap<String, Box<RawValue>>,
    view: T,
}

impl<T> Serialize for NativeJsonRequest<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.fields.serialize(serializer)
    }
}

impl<'de, T: serde::de::DeserializeOwned> Deserialize<'de> for NativeJsonRequest<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fields = UniqueFields::deserialize(deserializer)?.0;
        for name in ["input", "stream_options"] {
            if let Some(raw) = fields.get(name) {
                validate_unique_subtree(raw).map_err(serde::de::Error::custom)?;
            }
        }
        let body = serde_json::to_vec(&fields).map_err(serde::de::Error::custom)?;
        let view = serde_json::from_slice(&body).map_err(serde::de::Error::custom)?;
        Ok(Self { fields, view })
    }
}

impl<T> Deref for NativeJsonRequest<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.view
    }
}

impl<T: Serialize + serde::de::DeserializeOwned + Clone> NativeJsonRequest<T> {
    pub fn from_typed(view: T) -> Result<Self, BackendError> {
        let bytes = serde_json::to_vec(&view).map_err(|e| BackendError::Other(e.to_string()))?;
        serde_json::from_slice(&bytes).map_err(|e| BackendError::Other(e.to_string()))
    }
    pub fn value(&self) -> Value {
        observation_value(&self.fields)
    }
    pub fn set_field<V: Serialize>(&mut self, name: &str, value: &V) -> Result<(), BackendError> {
        self.fields.insert(
            name.to_owned(),
            serde_json::value::to_raw_value(value)
                .map_err(|e| BackendError::Other(e.to_string()))?,
        );
        Ok(())
    }
    pub fn compatibility_request(&self) -> Result<T, BackendError> {
        let represented: Value = serde_json::from_slice(
            &serde_json::to_vec(&self.view).map_err(|e| BackendError::Other(e.to_string()))?,
        )
        .map_err(|e| BackendError::Other(e.to_string()))?;
        for (name, raw) in &self.fields {
            let value: Value = serde_json::from_str(raw.get()).map_err(|_| {
                BackendError::api_status(
                    400,
                    format!("this translated route cannot observe {name} losslessly"),
                )
            })?;
            if !value.is_null() && represented.get(name) != Some(&value) {
                return Err(BackendError::api_status(
                    400,
                    format!("this translated route cannot represent {name} losslessly"),
                ));
            }
        }
        Ok(self.view.clone())
    }
}

impl NativeJsonRequest<crate::types::CreateEmbeddingRequest> {
    pub fn set_model(&mut self, model: String) {
        self.view.model = model.clone();
        self.set_field("model", &model).expect("model serializes");
    }
}
impl NativeJsonRequest<crate::types::CreateImageRequest> {
    pub fn set_model(&mut self, model: String) {
        self.view.model = Some(model.clone());
        self.set_field("model", &model).expect("model serializes");
    }
}
impl NativeJsonRequest<crate::types::CreateSpeechRequest> {
    pub fn set_model(&mut self, model: String) {
        self.view.model = model.clone();
        self.set_field("model", &model).expect("model serializes");
    }
}

/// Reject ambiguous top-level input while keeping unfamiliar subtrees raw.
pub(crate) struct UniqueFields(pub BTreeMap<String, Box<RawValue>>);
impl<'de> Deserialize<'de> for UniqueFields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldsVisitor;
        impl<'de> serde::de::Visitor<'de> for FieldsVisitor {
            type Value = UniqueFields;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an object with unique field names")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut fields = BTreeMap::new();
                while let Some(name) = map.next_key::<String>()? {
                    if fields.contains_key(&name) {
                        return Err(serde::de::Error::custom(format!("duplicate field: {name}")));
                    }
                    fields.insert(name, map.next_value()?);
                }
                Ok(UniqueFields(fields))
            }
        }
        deserializer.deserialize_map(FieldsVisitor)
    }
}

pub(crate) fn validate_unique_subtree(raw: &RawValue) -> Result<(), serde_json::Error> {
    match raw.get().trim_start().as_bytes().first() {
        Some(b'{') => {
            let fields: UniqueFields = serde_json::from_str(raw.get())?;
            for child in fields.0.values() {
                validate_unique_subtree(child)?;
            }
        }
        Some(b'[') => {
            let values: Vec<Box<RawValue>> = serde_json::from_str(raw.get())?;
            for child in values {
                validate_unique_subtree(&child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// An observation may omit numbers outside `Value`'s finite range. They remain
/// present in authoritative native serialization and invalidate translation.
pub(crate) fn observation_value(fields: &BTreeMap<String, Box<RawValue>>) -> Value {
    Value::Object(
        fields
            .iter()
            .filter_map(|(name, raw)| {
                serde_json::from_str(raw.get())
                    .ok()
                    .map(|value| (name.clone(), value))
            })
            .collect(),
    )
}
