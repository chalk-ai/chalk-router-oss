use crate::capability_requirements::{
    audio_speech_requirements, audio_transcription_requirements, audio_translation_requirements,
};
use crate::middleware::ResolvedApiKey;
use crate::native_audio::{AudioOperation, NativeAudioRequest};
use crate::native_json::NativeJsonRequest;
use crate::quota::TokenReservation;
use crate::tenancy::RequestState as State;
use crate::types::CreateSpeechRequest;
use axum::{
    extract::{Json, Multipart},
    response::Response,
};

use super::routing::{acquire_request_quota, run_passthrough};
use super::trace_attributes::{ContentDirection, LlmSpanKind, LlmSpanWriter};

fn set_audio_input_attributes(
    span: &LlmSpanWriter,
    language: Option<&str>,
    prompt: Option<&str>,
    response_format: Option<&str>,
    temperature: Option<f32>,
) {
    let mut params = serde_json::Map::new();
    if let Some(language) = language {
        params.insert("language".into(), serde_json::json!(language));
    }
    if let Some(response_format) = response_format {
        params.insert("response_format".into(), serde_json::json!(response_format));
    }
    if let Some(temperature) = temperature {
        params.insert("temperature".into(), serde_json::json!(temperature));
    }
    // A transcription/translation prompt is customer content, not an
    // always-on invocation parameter. `record_text` applies both the content-
    // logging gate and the shared byte budget.
    if let Some(prompt) = prompt {
        span.record_text(ContentDirection::Input, prompt.to_string());
    }
    if !params.is_empty() {
        if let Ok(params_json) = serde_json::to_string(&params) {
            span.set_invocation_parameters_json(params_json);
        }
    }
}

pub async fn create_speech(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    Json(req): Json<NativeJsonRequest<CreateSpeechRequest>>,
) -> Response {
    let span = tracing::info_span!("create_speech", otel.name = "audio.speech",);
    let span = LlmSpanWriter::new(span, state.object_store.clone(), LlmSpanKind::Llm);
    super::stamp_authenticated_principal(&span, &api_key);

    span.clone().instrument(async move {
        let current_span = span;
        current_span.set_model_system(req.model.clone(), state.backend.name().to_string());
        current_span.record_text(ContentDirection::Input, req.input.clone());

        // OpenInference: invocation parameters
        let mut params = serde_json::Map::new();
        params.insert("voice".into(), serde_json::json!(req.voice));
        if let Some(ref fmt) = req.response_format {
            params.insert("response_format".into(), serde_json::json!(fmt));
        }
        if let Some(speed) = req.speed {
            params.insert("speed".into(), serde_json::json!(speed));
        }
        if let Ok(params_json) = serde_json::to_string(&params) {
            current_span.set_invocation_parameters_json(params_json);
        }

        let response_format = req.response_format.clone().unwrap_or_else(|| "mp3".to_string());


        let route = match run_passthrough(&state, &api_key, &current_span, &req.model, &audio_speech_requirements(), "openai_audio.speech").await {
            Ok(route) => route,
            Err(error) => return super::backend_error_response(&error),
        };

        let mut quota_permit = match acquire_request_quota(
            &state,
            &api_key,
            TokenReservation::NotApplicable,
        )
        .await
        {
            Ok(permit) => permit,
            Err(error) => return super::backend_error_response(&error),
        };
        let dispatch = async {
            let mut req = req;
            req.set_model(route.stripped_model.clone());
            route.backend.create_native_speech(req).await
        }
        .await;
        match dispatch {
            Ok(response) => {
                let audio_bytes = response.body.clone();
                let content_type = response.content_type.split(';').next().unwrap_or("application/octet-stream");
                let output_format = match content_type {
                    "audio/ogg" | "audio/opus" => "opus", "audio/aac" => "aac",
                    "audio/flac" => "flac", "audio/wav" | "audio/x-wav" => "wav",
                    "audio/pcm" | "audio/L16" => "pcm", "audio/mpeg" => "mp3",
                    _ => response_format.as_str(),
                };
                if let Some(permit) = quota_permit.take() {
                    permit.finish(None).await;
                }
                // Upload output audio to object store
                if current_span.content_enabled() {
                    if let Some(ref store) = state.object_store {
                    match store.put_hashed(&audio_bytes, output_format).await {
                        Ok(key) => {
                            let uri = store.uri_for_key(&key);
                            if current_span.try_content(
                                ContentDirection::Output,
                                "audio.0.url",
                                uri,
                            ) {
                                current_span.set_mime_type(
                                    ContentDirection::Output,
                                    content_type,
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Failed to upload speech audio to object store");
                        }
                    }
                    }
                }

                super::native_body_response(response)
            }
            Err(e) => {
                if let Some(permit) = quota_permit.take() {
                    permit.finish_failed().await;
                }
                tracing::error!(error = %e, "Speech synthesis backend error");
                super::backend_error_response(&e)
            }
        }
    })
    .await
}

pub async fn create_transcription(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    mut multipart: Multipart,
) -> Response {
    let span = tracing::info_span!("create_transcription", otel.name = "audio.transcription",);
    let span = LlmSpanWriter::new(span, state.object_store.clone(), LlmSpanKind::Llm);
    super::stamp_authenticated_principal(&span, &api_key);

    span.clone().instrument(async move {
        let current_span = span;
        current_span.set_system(state.backend.name().to_string());

        let mut native = match NativeAudioRequest::read(&mut multipart, AudioOperation::Transcription).await {
            Ok(request) => request,
            Err(error) => return super::backend_error_response(&error),
        };
        let model = native.model().to_owned();
        let file_bytes = native.file().body.clone();
        let file_name = native.file().filename.clone();
        let prompt = native.text("prompt");
        let response_format = native.text("response_format");
        let temperature = native.text("temperature").and_then(|value| value.parse::<f32>().ok());
        let language = native.text("language");
        current_span.set_model_system(model.clone(), state.backend.name().to_string());

        set_audio_input_attributes(
            &current_span,
            language.as_deref(),
            prompt.as_deref(),
            response_format.as_deref(),
            temperature,
        );

        let file_name = file_name.unwrap_or_else(|| "audio.wav".to_string());

        let route = match run_passthrough(&state, &api_key, &current_span, &model, &audio_transcription_requirements(), "openai_audio.transcription").await {
            Ok(route) => route,
            Err(error) => return super::backend_error_response(&error),
        };

        // Upload input audio to object store
        let audio_ext = file_name
            .rsplit('.')
            .next()
            .filter(|ext| ext.len() <= 10)
            .unwrap_or("wav");
        if current_span.content_enabled() {
            if let Some(ref store) = state.object_store {
            match store.put_hashed(&file_bytes, audio_ext).await {
                Ok(key) => {
                    let uri = store.uri_for_key(&key);
                    if current_span.try_content(
                        ContentDirection::Input,
                        "audio.0.url",
                        uri,
                    ) {
                        current_span.set_mime_type(
                            ContentDirection::Input,
                            format!("audio/{audio_ext}"),
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to upload transcription audio to object store");
                }
            }
            }
        }

        let mut quota_permit = match acquire_request_quota(
            &state,
            &api_key,
            TokenReservation::NotApplicable,
        )
        .await
        {
            Ok(permit) => permit,
            Err(error) => return super::backend_error_response(&error),
        };
        let dispatch = async {
            native.set_model(route.stripped_model.clone());
            route.backend.create_native_audio(native).await
        }
        .await;
        match dispatch {
            Ok(response) => {
                if let Some(permit) = quota_permit.take() {
                    permit.finish(None).await;
                }
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&response.body) {
                    if let Some(text) = value.get("text").and_then(serde_json::Value::as_str) { current_span.record_text(ContentDirection::Output, text.to_owned()); }
                } else if response.content_type.starts_with("text/") {
                    current_span.record_text(ContentDirection::Output, String::from_utf8_lossy(&response.body).into_owned());
                }
                super::native_body_response(response)
            }
            Err(e) => {
                if let Some(permit) = quota_permit.take() {
                    permit.finish_failed().await;
                }
                tracing::error!(error = %e, "Transcription backend error");
                super::backend_error_response(&e)
            }
        }
    })
    .await
}

pub async fn create_translation(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    mut multipart: Multipart,
) -> Response {
    let span = tracing::info_span!("create_translation", otel.name = "audio.translation",);
    let span = LlmSpanWriter::new(span, state.object_store.clone(), LlmSpanKind::Llm);
    super::stamp_authenticated_principal(&span, &api_key);

    span.clone().instrument(async move {
        let current_span = span;
        current_span.set_system(state.backend.name().to_string());

        let mut native = match NativeAudioRequest::read(&mut multipart, AudioOperation::Translation).await {
            Ok(request) => request,
            Err(error) => return super::backend_error_response(&error),
        };
        let model = native.model().to_owned();
        let file_bytes = native.file().body.clone();
        let file_name = native.file().filename.clone();
        let prompt = native.text("prompt");
        let response_format = native.text("response_format");
        let temperature = native.text("temperature").and_then(|value| value.parse::<f32>().ok());
        current_span.set_model_system(model.clone(), state.backend.name().to_string());

        set_audio_input_attributes(
            &current_span,
            None,
            prompt.as_deref(),
            response_format.as_deref(),
            temperature,
        );

        let file_name = file_name.unwrap_or_else(|| "audio.wav".to_string());

        let route = match run_passthrough(&state, &api_key, &current_span, &model, &audio_translation_requirements(), "openai_audio.translation").await {
            Ok(route) => route,
            Err(error) => return super::backend_error_response(&error),
        };

        // Upload input audio to object store
        let audio_ext = file_name
            .rsplit('.')
            .next()
            .filter(|ext| ext.len() <= 10)
            .unwrap_or("wav");
        if current_span.content_enabled() {
            if let Some(ref store) = state.object_store {
            match store.put_hashed(&file_bytes, audio_ext).await {
                Ok(key) => {
                    let uri = store.uri_for_key(&key);
                    if current_span.try_content(
                        ContentDirection::Input,
                        "audio.0.url",
                        uri,
                    ) {
                        current_span.set_mime_type(
                            ContentDirection::Input,
                            format!("audio/{audio_ext}"),
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to upload translation audio to object store");
                }
            }
            }
        }

        let mut quota_permit = match acquire_request_quota(
            &state,
            &api_key,
            TokenReservation::NotApplicable,
        )
        .await
        {
            Ok(permit) => permit,
            Err(error) => return super::backend_error_response(&error),
        };
        let dispatch = async {
            native.set_model(route.stripped_model.clone());
            route.backend.create_native_audio(native).await
        }
        .await;
        match dispatch {
            Ok(response) => {
                if let Some(permit) = quota_permit.take() {
                    permit.finish(None).await;
                }
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&response.body) {
                    if let Some(text) = value.get("text").and_then(serde_json::Value::as_str) { current_span.record_text(ContentDirection::Output, text.to_owned()); }
                } else if response.content_type.starts_with("text/") {
                    current_span.record_text(ContentDirection::Output, String::from_utf8_lossy(&response.body).into_owned());
                }
                super::native_body_response(response)
            }
            Err(e) => {
                if let Some(permit) = quota_permit.take() {
                    permit.finish_failed().await;
                }
                tracing::error!(error = %e, "Translation backend error");
                super::backend_error_response(&e)
            }
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
    use opentelemetry_sdk::trace::{SimpleSpanProcessor, TracerProvider};
    use tracing_subscriber::layer::SubscriberExt;

    fn input_attributes(log_content: bool) -> std::collections::HashMap<String, String> {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("audio-input-test")));
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let span = tracing::info_span!("audio_input");
            let writer = LlmSpanWriter::new_with_limits(
                span,
                LlmSpanKind::Llm,
                8,
                0,
                super::super::trace_attributes::MAX_LLM_SPAN_CONTENT_BYTES,
                log_content,
            );
            set_audio_input_attributes(
                &writer,
                Some("en"),
                Some("customer prompt"),
                Some("json"),
                Some(0.25),
            );
        }
        for result in provider.force_flush() {
            result.expect("span should flush");
        }
        exporter
            .get_finished_spans()
            .expect("finished spans")
            .into_iter()
            .find(|span| span.name.as_ref() == "audio_input")
            .expect("audio input span")
            .attributes
            .into_iter()
            .map(|attribute| (attribute.key.to_string(), attribute.value.to_string()))
            .collect()
    }

    #[test]
    fn prompt_is_content_gated_and_never_an_invocation_parameter() {
        let metadata_only = input_attributes(false);
        let parameters: serde_json::Value = serde_json::from_str(
            metadata_only
                .get("llm.invocation_parameters")
                .expect("safe invocation parameters"),
        )
        .unwrap();
        assert_eq!(parameters["language"], "en");
        assert!(parameters.get("prompt").is_none());
        assert!(!metadata_only.contains_key("input.value"));

        let content = input_attributes(true);
        assert_eq!(
            content.get("input.value").map(String::as_str),
            Some("customer prompt")
        );
        let parameters: serde_json::Value = serde_json::from_str(
            content
                .get("llm.invocation_parameters")
                .expect("safe invocation parameters"),
        )
        .unwrap();
        assert!(parameters.get("prompt").is_none());
    }
}
