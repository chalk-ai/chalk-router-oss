//! Ordered multipart request parts. Routing observes the model without dropping
//! unfamiliar fields, repeated options, filenames, MIME types, or file bytes.
use crate::backend::BackendError;
use bytes::Bytes;

#[derive(Clone, Debug)]
pub struct AudioPart {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub headers: reqwest::header::HeaderMap,
    pub body: Bytes,
}

#[derive(Clone, Copy, Debug)]
pub enum AudioOperation {
    Transcription,
    Translation,
}

#[derive(Clone, Debug)]
pub struct NativeAudioRequest {
    pub operation: AudioOperation,
    pub parts: Vec<AudioPart>,
    model: String,
}

impl NativeAudioRequest {
    pub async fn read(
        multipart: &mut axum::extract::Multipart,
        operation: AudioOperation,
    ) -> Result<Self, BackendError> {
        let mut parts = Vec::new();
        while let Some(field) = multipart.next_field().await.map_err(|error| {
            BackendError::api_status(400, format!("invalid audio multipart body: {error}"))
        })? {
            let name = field.name().unwrap_or_default().to_owned();
            let filename = field.file_name().map(str::to_owned);
            let content_type = field.content_type().map(str::to_owned);
            let mut headers = field.headers().clone();
            for name in ["content-disposition", "content-type", "content-length"] {
                headers.remove(name);
            }
            let body = field.bytes().await.map_err(|error| {
                BackendError::api_status(400, format!("invalid audio multipart field: {error}"))
            })?;
            parts.push(AudioPart {
                name,
                filename,
                content_type,
                headers,
                body,
            });
        }
        Self::from_parts(operation, parts)
    }

    pub fn from_parts(
        operation: AudioOperation,
        parts: Vec<AudioPart>,
    ) -> Result<Self, BackendError> {
        let mut models = parts.iter().filter(|part| part.name == "model");
        let model = models
            .next()
            .ok_or_else(|| BackendError::api_status(400, "Missing required field: model"))?;
        if models.next().is_some() {
            return Err(BackendError::api_status(
                400,
                "audio requests require exactly one model field",
            ));
        }
        let model = std::str::from_utf8(&model.body)
            .map_err(|_| BackendError::api_status(400, "model must be UTF-8 text"))?
            .to_string();
        if model.is_empty() {
            return Err(BackendError::api_status(400, "model must not be empty"));
        }
        if !parts.iter().any(|part| part.name == "file") {
            return Err(BackendError::api_status(
                400,
                "Missing required field: file",
            ));
        }
        Ok(Self {
            operation,
            parts,
            model,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn set_model(&mut self, model: String) {
        for part in &mut self.parts {
            if part.name == "model" {
                part.body = Bytes::from(model.clone());
            }
        }
        self.model = model;
    }

    pub fn text(&self, name: &str) -> Option<String> {
        self.parts
            .iter()
            .find(|part| part.name == name)
            .and_then(|part| std::str::from_utf8(&part.body).ok())
            .map(str::to_owned)
    }

    pub fn file(&self) -> &AudioPart {
        self.parts
            .iter()
            .find(|part| part.name == "file")
            .expect("validated file field")
    }

    pub fn multipart(&self) -> Result<reqwest::multipart::Form, BackendError> {
        let mut form = reqwest::multipart::Form::new();
        for part in &self.parts {
            let mut outgoing =
                reqwest::multipart::Part::bytes(part.body.to_vec()).headers(part.headers.clone());
            if let Some(filename) = &part.filename {
                outgoing = outgoing.file_name(filename.clone());
            }
            if let Some(mime) = &part.content_type {
                outgoing = outgoing.mime_str(mime).map_err(|error| {
                    BackendError::api_status(400, format!("invalid part MIME type: {error}"))
                })?;
            }
            form = form.part(part.name.clone(), outgoing);
        }
        Ok(form)
    }

    /// Legacy adapters may only accept their typed shape. Fail explicitly
    /// rather than reporting success after silently discarding a part.
    pub fn require_compatible(&self) -> Result<(), BackendError> {
        let mut seen = std::collections::HashSet::new();
        for part in &self.parts {
            if ![
                "file",
                "model",
                "language",
                "prompt",
                "response_format",
                "temperature",
            ]
            .contains(&part.name.as_str())
                || !seen.insert(&part.name)
                || (!matches!(part.name.as_str(), "file")
                    && std::str::from_utf8(&part.body).is_err())
                || !part.headers.is_empty()
                || part
                    .content_type
                    .as_deref()
                    .is_some_and(|mime| match part.name.as_str() {
                        "file" => mime != "application/octet-stream",
                        _ => mime != "text/plain" && mime != "text/plain; charset=utf-8",
                    })
            {
                return Err(BackendError::api_status(
                    400,
                    format!(
                        "this audio adapter cannot represent multipart field {}",
                        part.name
                    ),
                ));
            }
        }
        if matches!(self.operation, AudioOperation::Translation) && self.text("language").is_some()
        {
            return Err(BackendError::api_status(
                400,
                "this translation adapter cannot represent language",
            ));
        }
        if self
            .text("temperature")
            .is_some_and(|value| value.parse::<f32>().is_err())
        {
            return Err(BackendError::api_status(
                400,
                "this audio adapter cannot represent temperature",
            ));
        }
        Ok(())
    }
}
