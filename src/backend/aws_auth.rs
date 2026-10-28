//! SigV4 authentication for finalized Bedrock requests. Stored bearer keys
//! never enter this credential chain, including after an HTTP rejection.
use super::provider::BackendError;
use reqsign::aws::{Credential, DefaultCredentialProvider, RequestSigner};
use reqsign::{HttpSend, Signer};
use reqwest::{Client, Request};

// Keep credential fetches on the router's configured HTTP client instead of
// silently replacing its connection/egress policy during the reqsign upgrade.
#[derive(Debug)]
struct CredentialHttp(Client);

impl HttpSend for CredentialHttp {
    async fn http_send(
        &self,
        request: http::Request<bytes::Bytes>,
    ) -> reqsign::Result<http::Response<bytes::Bytes>> {
        let request = Request::try_from(request)
            .map_err(|e| reqsign::Error::unexpected("invalid credential request").with_source(e))?;
        let response = self.0.execute(request).await.map_err(|e| {
            reqsign::Error::unexpected("credential HTTP request failed").with_source(e)
        })?;
        let mut result = http::Response::builder()
            .status(response.status())
            .version(response.version());
        *result.headers_mut().expect("valid response builder") = response.headers().clone();
        let body = response.bytes().await.map_err(|e| {
            reqsign::Error::unexpected("credential response body failed").with_source(e)
        })?;
        result
            .body(body)
            .map_err(|e| reqsign::Error::unexpected("invalid credential response").with_source(e))
    }
}

pub(crate) struct BedrockSigv4 {
    signer: Signer<Credential>,
}

impl BedrockSigv4 {
    pub(crate) fn new(client: Client, region: impl Into<String>) -> Self {
        Self {
            signer: Signer::new(
                reqsign::default_context().with_http_send(CredentialHttp(client)),
                DefaultCredentialProvider::new(),
                RequestSigner::new("bedrock", &region.into()),
            ),
        }
    }

    pub(crate) async fn sign(&self, request: &mut Request) -> Result<(), BackendError> {
        // The body and URL have already been finalized. A placeholder bearer
        // header must not participate in the signature.
        request.headers_mut().remove(reqwest::header::AUTHORIZATION);
        // reqsign otherwise defaults to UNSIGNED-PAYLOAD. Bedrock requires
        // the digest of the exact finalized JSON body in its SigV4 request.
        let body = match request.body() {
            Some(body) => body.as_bytes().ok_or_else(|| {
                BackendError::ConfigError("Bedrock SigV4 requires a buffered request body".into())
            })?,
            None => &[],
        };
        let digest = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(body));
        request.headers_mut().insert(
            "x-amz-content-sha256",
            reqwest::header::HeaderValue::from_str(&digest).expect("SHA256 is a valid header"),
        );
        let (mut parts, ()) = http::Request::builder()
            .method(request.method().clone())
            .uri(request.url().as_str())
            .body(())
            .map_err(|e| BackendError::ConfigError(format!("invalid signing request: {e}")))?
            .into_parts();
        parts.headers = request.headers().clone();
        self.signer
            .sign(&mut parts, None)
            .await
            .map_err(|e| BackendError::ConfigError(format!("Bedrock SigV4 signing failed: {e}")))?;
        *request.url_mut() = reqwest::Url::parse(&parts.uri.to_string())
            .map_err(|e| BackendError::ConfigError(format!("invalid signed request URL: {e}")))?;
        *request.headers_mut() = parts.headers;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn signs_finalized_openai_path_body_and_session_credentials_offline() {
        let client = Client::new();
        let signer = BedrockSigv4 {
            signer: Signer::new(
                reqsign::default_context().with_http_send(CredentialHttp(client.clone())),
                reqsign::aws::StaticCredentialProvider::new("offline-access", "offline-secret")
                    .with_session_token("offline-session"),
                RequestSigner::new("bedrock", "us-west-2"),
            ),
        };
        let body = r#"{"model":"us.openai.gpt-5.6-luna","reasoning_effort":"none"}"#;
        let mut request = client
            .post("https://bedrock-runtime.us-west-2.amazonaws.com/openai/v1/chat/completions")
            .bearer_auth("unused")
            .header("content-type", "application/json")
            .body(body)
            .build()
            .unwrap();
        signer.sign(&mut request).await.unwrap();
        let auth = request.headers()["authorization"].to_str().unwrap();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=offline-access/"));
        assert!(auth.contains("/us-west-2/bedrock/aws4_request"));
        assert_eq!(request.headers()["x-amz-security-token"], "offline-session");
        assert_eq!(request.url().path(), "/openai/v1/chat/completions");
        assert_eq!(request.body().unwrap().as_bytes().unwrap(), body.as_bytes());
        assert_eq!(
            request.headers()["x-amz-content-sha256"],
            format!(
                "{:x}",
                <sha2::Sha256 as sha2::Digest>::digest(body.as_bytes())
            )
        );
    }
}
