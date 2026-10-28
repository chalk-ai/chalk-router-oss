//! Split shared batches before retrying each environment group independently.
use std::collections::BTreeMap;

use futures::future::BoxFuture;
use opentelemetry::KeyValue;
use opentelemetry_sdk::{
    export::trace::{ExportResult, SpanData, SpanExporter},
    Resource,
};

pub fn environment(span: &SpanData) -> &str {
    span.attributes
        .iter()
        .find_map(|kv| {
            if kv.key.as_str() != super::ENVIRONMENT_ID_ATTRIBUTE {
                return None;
            }
            match &kv.value {
                opentelemetry::Value::String(value) => Some(value.as_str()),
                _ => None,
            }
        })
        .unwrap_or("")
}

pub fn group(batch: Vec<SpanData>) -> BTreeMap<String, Vec<SpanData>> {
    let mut groups: BTreeMap<String, Vec<SpanData>> = BTreeMap::new();
    for span in batch {
        groups
            .entry(environment(&span).into())
            .or_default()
            .push(span);
    }
    groups
}

pub fn resource_for_environment(base: &Resource, environment: &str) -> Resource {
    let mut attributes: Vec<_> = base
        .iter()
        .filter(|(key, _)| key.as_str() != super::ENVIRONMENT_ID_ATTRIBUTE)
        .map(|(key, value)| KeyValue::new(key.clone(), value.clone()))
        .collect();
    if !environment.is_empty() {
        attributes.push(KeyValue::new(
            super::ENVIRONMENT_ID_ATTRIBUTE,
            environment.to_owned(),
        ));
    }
    Resource::new(attributes)
}

#[derive(Debug)]
pub struct TenantResourceExporter {
    inner: super::otlp_retry::RetryingSpanExporter,
    resource: Resource,
}

impl TenantResourceExporter {
    pub fn new(inner: impl SpanExporter + 'static) -> Self {
        Self {
            inner: super::otlp_retry::RetryingSpanExporter::new(inner),
            resource: Resource::default(),
        }
    }
}

impl SpanExporter for TenantResourceExporter {
    fn export(&mut self, batch: Vec<SpanData>) -> BoxFuture<'static, ExportResult> {
        let mut exports = Vec::new();
        for (environment, spans) in group(batch) {
            // Each retry future captures this resource. A successful group is
            // never replayed because a different tenant's request failed.
            let resource = if environment.is_empty() {
                self.resource.clone()
            } else {
                resource_for_environment(&self.resource, &environment)
            };
            self.inner.set_resource(&resource);
            exports.push(self.inner.export(spans));
        }
        Box::pin(async move {
            for result in futures::future::join_all(exports).await {
                result?;
            }
            Ok(())
        })
    }
    fn set_resource(&mut self, resource: &Resource) {
        self.resource = resource.clone();
    }
    fn shutdown(&mut self) {
        self.inner.shutdown();
    }
}
