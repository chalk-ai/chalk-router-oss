#![allow(clippy::result_large_err)] // tonic::Status is the gRPC wire error

use chrono::Utc;
use serde_json::Value;
use tonic::{Request, Response, Status};

use crate::telemetry_settings::{TelemetryExporter, TelemetrySettingsStoreError};
use crate::AppState;

use super::conv;
use super::proto;
use super::proto::telemetry_settings_service_server::TelemetrySettingsService;

pub struct TelemetrySettingsServer {
    pub state: AppState,
}

fn to_proto(exporter: &TelemetryExporter) -> Result<proto::TelemetryExporter, Status> {
    let config_json = serde_json::to_string(&exporter.config)
        .map_err(|error| Status::internal(format!("failed to encode exporter config: {error}")))?;
    Ok(proto::TelemetryExporter {
        id: exporter.id.clone(),
        name: exporter.name.clone(),
        exporter_type: exporter.exporter_type().to_string(),
        config_json,
        enabled: exporter.enabled,
        api_key_set: exporter.api_key_set(),
        created_at: Some(conv::ts_to_proto(exporter.created_at)),
        updated_at: Some(conv::ts_to_proto(exporter.updated_at)),
        read_only: exporter.read_only,
    })
}

fn map_error(error: TelemetrySettingsStoreError) -> Status {
    match error {
        TelemetrySettingsStoreError::NotFound => Status::not_found(error.to_string()),
        TelemetrySettingsStoreError::AlreadyExists(message) => Status::already_exists(message),
        TelemetrySettingsStoreError::InvalidArgument(message) => Status::invalid_argument(message),
        TelemetrySettingsStoreError::FailedPrecondition(message) => {
            Status::failed_precondition(message)
        }
        TelemetrySettingsStoreError::Other(message) => Status::internal(message),
    }
}

fn from_proto(
    operation: proto::UpsertTelemetryExporterOperation,
) -> Result<(TelemetryExporter, Option<String>), Status> {
    let mut config: Value = serde_json::from_str(&operation.config_json).map_err(|error| {
        Status::invalid_argument(format!("config_json must be valid JSON: {error}"))
    })?;
    if !operation.exporter_type.trim().is_empty() {
        if let Some(config) = config.as_object_mut() {
            config.insert(
                "type".to_string(),
                Value::String(operation.exporter_type.trim().to_ascii_lowercase()),
            );
        }
    }
    let now = Utc::now();
    Ok((
        TelemetryExporter {
            id: operation.id,
            name: operation.name,
            config,
            enabled: operation.enabled,
            read_only: false,
            api_key: String::new(),
            created_at: now,
            updated_at: now,
        },
        operation.api_key,
    ))
}

#[tonic::async_trait]
impl TelemetrySettingsService for TelemetrySettingsServer {
    async fn list_telemetry_exporters(
        &self,
        _request: Request<proto::ListTelemetryExportersRequest>,
    ) -> Result<Response<proto::ListTelemetryExportersResponse>, Status> {
        let state = self.state.for_request(&_request);
        let exporters = state
            .telemetry_settings_service()
            .list()
            .await
            .map_err(map_error)?;
        let exporters = exporters
            .iter()
            .map(to_proto)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Response::new(proto::ListTelemetryExportersResponse {
            exporters,
        }))
    }

    async fn upsert_telemetry_exporter(
        &self,
        request: Request<proto::UpsertTelemetryExporterRequest>,
    ) -> Result<Response<proto::UpsertTelemetryExporterResponse>, Status> {
        let state = self.state.for_request(&request);
        let operation = request
            .into_inner()
            .exporter
            .ok_or_else(|| Status::invalid_argument("exporter is required"))?;
        let (exporter, api_key) = from_proto(operation)?;
        let exporter = state
            .telemetry_settings_service()
            .upsert(exporter, api_key)
            .await
            .map_err(map_error)?;
        Ok(Response::new(proto::UpsertTelemetryExporterResponse {
            exporter: Some(to_proto(&exporter)?),
        }))
    }

    async fn delete_telemetry_exporter(
        &self,
        request: Request<proto::DeleteTelemetryExporterRequest>,
    ) -> Result<Response<proto::DeleteTelemetryExporterResponse>, Status> {
        let state = self.state.for_request(&request);
        let id = request.into_inner().id;
        state
            .telemetry_settings_service()
            .delete(&id)
            .await
            .map_err(map_error)?;
        Ok(Response::new(proto::DeleteTelemetryExporterResponse { id }))
    }

    async fn test_telemetry_exporter(
        &self,
        request: Request<proto::TestTelemetryExporterRequest>,
    ) -> Result<Response<proto::TestTelemetryExporterResponse>, Status> {
        let state = self.state.for_request(&request);
        let operation = request
            .into_inner()
            .exporter
            .ok_or_else(|| Status::invalid_argument("exporter is required"))?;
        let (exporter, api_key) = from_proto(operation)?;
        let outcome = state
            .telemetry_settings_service()
            .test(exporter, api_key)
            .await;
        Ok(Response::new(proto::TestTelemetryExporterResponse {
            ok: outcome.ok,
            error: outcome.error,
            latency: outcome.latency_ms.map(conv::duration_from_millis),
        }))
    }
}
