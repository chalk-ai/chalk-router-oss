use bytes::Bytes;
use opendal::Operator;
use sha2::{Digest, Sha256};
use std::{borrow::Cow, sync::Arc};

/// Object storage client that writes to s3:// or gs:// URIs.
#[derive(Clone)]
pub struct ObjectStore {
    operator: Operator,
    /// The prefix path within the bucket (e.g. "images/")
    prefix: String,
}

impl ObjectStore {
    /// Share the cloud client while isolating paths using a traversal-safe digest.
    pub fn for_environment(&self, environment: &str) -> Self {
        Self {
            operator: self.operator.clone(),
            prefix: format!(
                "{}environments/{}/",
                self.prefix,
                hex_sha256(environment.as_bytes())
            ),
        }
    }

    #[cfg(test)]
    pub(crate) fn memory() -> Self {
        let operator = Operator::new(opendal::services::Memory::default())
            .expect("memory object store should initialize");
        Self {
            operator,
            // Match a bucket-root production configuration such as
            // `gs://chalk-router-dev-objects`; confinement must not depend on
            // a non-empty prefix.
            prefix: String::new(),
        }
    }

    /// Create from a URI like `s3://bucket/prefix` or `gs://bucket/prefix`.
    pub fn from_uri(uri: &str) -> Result<Self, opendal::Error> {
        let (scheme, bucket, prefix) = parse_uri(uri)?;

        let operator = match scheme {
            "s3" => {
                let mut builder = opendal::services::S3::default();
                builder = builder.bucket(bucket);
                // Region, credentials, endpoint picked up from env (AWS_REGION, etc.)
                if let Ok(region) = std::env::var("AWS_REGION") {
                    builder = builder.region(&region);
                }
                if let Ok(endpoint) = std::env::var("AWS_ENDPOINT_URL") {
                    builder = builder.endpoint(&endpoint);
                }
                Operator::new(builder)?
            }
            "gcs" => {
                let mut builder = opendal::services::Gcs::default();
                builder = builder.bucket(bucket);
                if let Ok(cred) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
                    builder = builder.credential_path(&cred);
                }
                Operator::new(builder)?
            }
            _ => {
                return Err(opendal::Error::new(
                    opendal::ErrorKind::ConfigInvalid,
                    "unsupported scheme, expected s3:// or gs://",
                ));
            }
        };

        Ok(Self { operator, prefix })
    }

    /// Write `data` to object storage under `yyyy-mm-dd/<sha256-hex>` with the
    /// given extension. Returns the full object key.
    pub async fn put_hashed(&self, data: &[u8], extension: &str) -> Result<String, opendal::Error> {
        self.put_hashed_bytes(Bytes::copy_from_slice(data), extension)
            .await
    }

    /// Shared-buffer variant used by retried background trace uploads so each
    /// attempt does not make another full-size copy.
    pub(crate) async fn put_hashed_bytes(
        &self,
        data: Bytes,
        extension: &str,
    ) -> Result<String, opendal::Error> {
        let hash = hex_sha256(&data);
        let date = chrono::Utc::now().format("%Y-%m-%d");
        let key = if extension.is_empty() {
            format!("{}{}/{}", self.prefix, date, hash)
        } else {
            format!("{}{}/{}.{}", self.prefix, date, hash, extension)
        };
        self.operator.write(&key, data).await?;
        Ok(key)
    }

    /// Return the full URI (e.g. s3://bucket/prefix/2026-04-09/abc123.png) for a key.
    pub fn uri_for_key(&self, key: &str) -> String {
        // The operator info contains the scheme and bucket; reconstruct the URI.
        let info = self.operator.info();
        let scheme = info.scheme();
        let name = info.name();
        format!("{}://{}/{}", scheme, name, key)
    }

    /// Read an object addressed by one of this store's URIs when it is small
    /// enough for a downstream exporter to inline. URIs for another store and
    /// objects above `max_bytes` are intentionally left unresolved.
    pub(crate) async fn read_uri_limited(
        &self,
        uri: &str,
        max_bytes: usize,
    ) -> Result<Option<Bytes>, opendal::Error> {
        let info = self.operator.info();
        let prefix = format!("{}://{}/", info.scheme(), info.name());
        let Some(key) = uri.strip_prefix(&prefix) else {
            return Ok(None);
        };
        let Some(relative_key) = key.strip_prefix(&self.prefix) else {
            return Ok(None);
        };
        if relative_key.is_empty()
            || relative_key
                .split('/')
                .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
            || !is_trace_content_key(relative_key)
        {
            return Ok(None);
        }
        let metadata = self.operator.stat(key).await?;
        if metadata.content_length() > max_bytes as u64 {
            return Ok(None);
        }
        let bytes = self.operator.read(key).await?.to_bytes();
        if bytes.len() > max_bytes {
            return Ok(None);
        }
        Ok(Some(bytes))
    }
}

/// Object-backed span content is always written as
/// `yyyy-mm-dd/<sha256-lowerhex>.json`. Enforcing that shape keeps hydration
/// inside the trace-content keyspace even when the configured store prefix is
/// empty and the URI came from a client-authored span.
fn is_trace_content_key(key: &str) -> bool {
    let mut parts = key.split('/');
    let (Some(date), Some(file), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    if chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err() {
        return false;
    }
    let Some(hash) = file.strip_suffix(".json") else {
        return false;
    };
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn hex_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    result.iter().map(|b| format!("{:02x}", b)).collect()
}

fn parse_uri(uri: &str) -> Result<(&'static str, &str, String), opendal::Error> {
    let (scheme, rest) = if let Some(rest) = uri.strip_prefix("s3://") {
        ("s3", rest)
    } else if let Some(rest) = uri.strip_prefix("gs://") {
        ("gcs", rest)
    } else {
        return Err(opendal::Error::new(
            opendal::ErrorKind::ConfigInvalid,
            "URI must start with s3:// or gs://",
        ));
    };

    let (bucket, prefix) = match rest.find('/') {
        Some(idx) => {
            let b = &rest[..idx];
            let mut p = rest[idx + 1..].to_string();
            if !p.is_empty() && !p.ends_with('/') {
                p.push('/');
            }
            (b, p)
        }
        None => (rest, String::new()),
    };

    Ok((scheme, bucket, prefix))
}

/// The parts of an RFC 2397 data URI. The scheme, media type and `base64`
/// token are case-insensitive, and parameters such as `charset` may precede
/// `base64`; `media_type` is lowercased.
pub struct DataUri<'a> {
    pub media_type: Cow<'a, str>,
    pub base64: bool,
    /// `None` when the URI has no comma separating metadata from data.
    pub data: Option<&'a str>,
}

impl<'a> DataUri<'a> {
    /// Normalize and strictly validate the payload without retaining decoded
    /// bytes. Callers forwarding inline base64 can reuse the returned string.
    pub fn validated_base64(&self) -> Result<Cow<'a, str>, String> {
        let data = self
            .data
            .ok_or_else(|| "invalid data URI: missing comma".to_string())?;
        let data = normalize_base64(data);
        let mut decoder = base64::read::DecoderReader::new(
            data.as_bytes(),
            &base64::engine::general_purpose::STANDARD,
        );
        std::io::copy(&mut decoder, &mut std::io::sink())
            .map_err(|e| format!("base64 decode error: {e}"))?;
        Ok(data)
    }
}

/// Borrow ordinary base64; only wrapped input needs a normalized allocation.
fn normalize_base64(data: &str) -> Cow<'_, str> {
    let Some(first_whitespace) = data.bytes().position(|byte| byte.is_ascii_whitespace()) else {
        return Cow::Borrowed(data);
    };
    let mut normalized = String::with_capacity(data.len());
    normalized.push_str(&data[..first_whitespace]);
    for part in data[first_whitespace..].split_ascii_whitespace() {
        normalized.push_str(part);
    }
    Cow::Owned(normalized)
}

/// `None` when `url` is not a `data:` URI.
pub fn split_data_uri(url: &str) -> Option<DataUri<'_>> {
    let (scheme, rest) = url.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("data") {
        return None;
    }
    let (meta, data) = match rest.split_once(',') {
        Some((meta, data)) => (meta, Some(data)),
        None => (rest, None),
    };
    let mut params = meta.split(';').map(str::trim);
    let media_type = params.next().unwrap_or_default();
    let media_type = if media_type.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(media_type.to_ascii_lowercase())
    } else {
        Cow::Borrowed(media_type)
    };
    let base64 = params.any(|param| param.eq_ignore_ascii_case("base64"));
    Some(DataUri {
        media_type,
        base64,
        data,
    })
}

/// Resolve caller-supplied image content independently of provider-admin trust.
/// Remote URLs always use connect-time DNS and redirect guards, with no proxies
/// or inherited provider credentials. There is no private-network exception.
pub async fn fetch_image_bytes(url: &str) -> Result<(Vec<u8>, String), String> {
    if let Some(uri) = split_data_uri(url) {
        let extension = image_extension(&uri.media_type)?;
        if !uri.base64 {
            return Err("image data URI must use base64".to_string());
        }
        let b64 = uri.validated_base64()?;
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.as_ref())
            .map_err(|e| format!("base64 decode error: {e}"))?;
        Ok((bytes, extension))
    } else {
        crate::egress::check_url(url).map_err(|e| format!("image URL not allowed: {e}"))?;
        static DOWNLOADS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);
        download_image(
            url,
            &crate::egress::guarded_client(std::time::Duration::from_secs(10)),
            &DOWNLOADS,
            MAX_IMAGE_DOWNLOAD_BYTES,
            IMAGE_DOWNLOAD_TIMEOUT,
        )
        .await
    }
}

const MAX_IMAGE_DOWNLOAD_BYTES: usize = 20 * 1024 * 1024;
const IMAGE_DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

// The public entry point validates destinations and supplies the guarded client.
// Keep the transport separate so resource-limit tests can use local servers.
async fn download_image(
    url: &str,
    client: &reqwest::Client,
    downloads: &tokio::sync::Semaphore,
    max_bytes: usize,
    deadline: std::time::Duration,
) -> Result<(Vec<u8>, String), String> {
    // Fail fast instead of allowing an unbounded queue of waiting requests.
    // Drop releases capacity on success, every error, timeout, and cancellation.
    let _permit = downloads
        .try_acquire()
        .map_err(|_| "too many concurrent image downloads".to_string())?;
    tokio::time::timeout(deadline, async {
        let mut resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("failed to fetch image: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("image fetch returned status {}", resp.status()));
        }
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let extension = image_extension(content_type)?;
        if resp
            .content_length()
            .is_some_and(|len| len > max_bytes as u64)
        {
            return Err(format!("image exceeds {max_bytes} byte download limit"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| format!("failed to read image body: {e}"))?
        {
            if chunk.len() > max_bytes - bytes.len() {
                return Err(format!("image exceeds {max_bytes} byte download limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok((bytes, extension))
    })
    .await
    .map_err(|_| "image download deadline exceeded".to_string())?
}

fn image_extension(content_type: &str) -> Result<String, String> {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let subtype = mime
        .strip_prefix("image/")
        .filter(|s| {
            !s.is_empty()
                && *s != "bin"
                && s.len() <= 64
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'+' | b'.'))
        })
        .ok_or_else(|| "image content requires an image MIME type".to_string())?;
    Ok(subtype.to_string())
}

/// Optional, globally shared object store. `None` if ROUTER_OBJECT_STORE_URI is unset.
pub type SharedObjectStore = Option<Arc<ObjectStore>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_uris_are_split_case_insensitively() {
        let uri = split_data_uri("DATA:IMAGE/PNG;charset=utf-8;BASE64,aGk=").unwrap();
        assert_eq!(
            (uri.media_type.as_ref(), uri.base64, uri.data),
            ("image/png", true, Some("aGk="))
        );
        let uri = split_data_uri("data:image/png,raw").unwrap();
        assert_eq!((uri.base64, uri.data), (false, Some("raw")));
        assert!(split_data_uri("data:image/png;base64")
            .unwrap()
            .data
            .is_none());
        assert!(split_data_uri("https://example.com/a.png").is_none());
        assert!(split_data_uri("no-scheme").is_none());
    }

    #[test]
    fn ordinary_data_uris_borrow_metadata_and_base64() {
        let uri = split_data_uri("data:image/png;base64,aGk=").unwrap();
        assert!(matches!(uri.media_type, Cow::Borrowed("image/png")));
        assert!(matches!(
            uri.validated_base64().unwrap(),
            Cow::Borrowed("aGk=")
        ));
        assert!(matches!(
            split_data_uri("DATA:IMAGE/PNG;base64,aGk=")
                .unwrap()
                .media_type,
            Cow::Owned(_)
        ));
        assert_eq!(normalize_base64("aG\tk=\r\n \x0c"), "aGk=");
        // Non-ASCII whitespace is not silently removed.
        assert_eq!(normalize_base64("aGk=\u{a0}"), "aGk=\u{a0}");
    }

    #[test]
    fn inline_base64_validation_matches_standard_decoder_across_buffer_boundaries() {
        use base64::{engine::general_purpose::STANDARD, Engine};

        let mut cases = vec![
            String::new(),
            "Zg==".into(),
            "Zm8=".into(),
            "Zm9v".into(),
            "Zg".into(),
            "Zg=".into(),
            "Zg===".into(),
            "Zh==".into(),
            "Zm9=".into(),
            "Zg==AAAA".into(),
            "!AAA".into(),
            "aGk=\u{a0}".into(),
            "aGk=\x0b".into(),
        ];
        for length in [1, 2, 3, 766, 767, 768, 769, 1534, 1535, 1536, 8192] {
            let encoded = STANDARD.encode(vec![42; length]);
            cases.push(encoded.clone());
            // A correctly padded block must not hide trailing data in the next
            // decoder buffer, including data following an exact 1024-byte block.
            cases.push(format!("{encoded}AAAA"));
            cases.push(format!("{encoded}="));
            cases.push(format!("{encoded}!"));
            for offset in [0, 1023, 1024, 1025, encoded.len() - 1] {
                if offset < encoded.len() {
                    let mut invalid = encoded.clone().into_bytes();
                    invalid[offset] = b'!';
                    cases.push(String::from_utf8(invalid).unwrap());
                }
            }
        }
        for data in cases {
            let uri = DataUri {
                media_type: Cow::Borrowed("image/png"),
                base64: true,
                data: Some(&data),
            };
            assert_eq!(
                uri.validated_base64().is_ok(),
                STANDARD.decode(&data).is_ok(),
                "base64 validation disagreed for {} bytes",
                data.len()
            );
        }
        assert!(split_data_uri("data:image/png;base64")
            .unwrap()
            .validated_base64()
            .is_err());
    }

    async fn download_server() -> (
        String,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::{body::Body, response::Response, routing::get, Router};
        use futures::StreamExt;
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let started = active.clone();
        let app = Router::new()
            .route(
                "/ok",
                get(|| async { ([("content-type", "image/png")], vec![1u8; 16]) }),
            )
            .route(
                "/large",
                get(|| async { ([("content-type", "image/png")], vec![1u8; 32]) }),
            )
            .route(
                "/chunked",
                get(|| async {
                    let stream = futures::stream::iter(
                        (0..4).map(|_| Ok::<_, std::io::Error>(Bytes::from_static(b"12345678"))),
                    );
                    Response::builder()
                        .header("content-type", "image/png")
                        .body(Body::from_stream(stream))
                        .unwrap()
                }),
            )
            .route(
                "/headers",
                get(|| async {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    ([("content-type", "image/png")], "image")
                }),
            )
            .route(
                "/trickle",
                get(|| async {
                    let stream = futures::stream::unfold((), |_| async {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        Some((Ok::<_, std::io::Error>(Bytes::from_static(b"x")), ()))
                    });
                    Response::builder()
                        .header("content-type", "image/png")
                        .body(Body::from_stream(stream))
                        .unwrap()
                }),
            )
            .route(
                "/bad-type",
                get(|| async {
                    Response::builder()
                        .header("content-type", "text/plain")
                        .body(Body::from_stream(futures::stream::pending::<
                            Result<Bytes, std::io::Error>,
                        >()))
                        .unwrap()
                }),
            )
            .route(
                "/stall",
                get(move || {
                    started.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async {
                        let stream = futures::stream::once(async {
                            Ok::<_, std::io::Error>(Bytes::from_static(b"image"))
                        })
                        .chain(futures::stream::pending());
                        Response::builder()
                            .header("content-type", "image/png")
                            .body(Body::from_stream(stream))
                            .unwrap()
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}"), active, server)
    }

    #[tokio::test]
    async fn remote_download_limits_cover_headers_chunks_and_complete_read() {
        let (url, _, server) = download_server().await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let slots = tokio::sync::Semaphore::new(1);
        let deadline = std::time::Duration::from_millis(200);
        let (bytes, ext) = download_image(&format!("{url}/ok"), &client, &slots, 16, deadline)
            .await
            .unwrap();
        assert_eq!((bytes.len(), ext.as_str()), (16, "png"));
        for (path, expected) in [
            ("large", "byte download limit"),
            ("chunked", "byte download limit"),
            ("headers", "deadline exceeded"),
            ("stall", "deadline exceeded"),
            ("trickle", "deadline exceeded"),
            ("bad-type", "image MIME type"),
        ] {
            let max_bytes = if path == "trickle" { 1024 } else { 16 };
            let error = download_image(
                &format!("{url}/{path}"),
                &client,
                &slots,
                max_bytes,
                deadline,
            )
            .await
            .unwrap_err();
            assert!(error.contains(expected), "{path}: {error}");
            assert_eq!(slots.available_permits(), 1, "{path} leaked capacity");
        }
        server.abort();
    }

    #[tokio::test]
    async fn misleading_content_length_cannot_force_buffering_or_a_hung_read() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (framing, body, expected) in [
            ("Content-Length: 1000000", "short", "byte download limit"),
            ("Content-Length: 16", "short", "failed to read image body"),
            (
                "Content-Length: 1\r\nTransfer-Encoding: chunked",
                "20\r\n12345678123456781234567812345678\r\n0\r\n\r\n",
                "byte download limit",
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                socket.read(&mut request).await.unwrap();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: image/png\r\n{framing}\r\nConnection: close\r\n\r\n{body}").as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            });
            let slots = tokio::sync::Semaphore::new(1);
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let error = download_image(
                &format!("http://{address}/"),
                &client,
                &slots,
                16,
                std::time::Duration::from_secs(2),
            )
            .await
            .unwrap_err();
            assert!(error.contains(expected), "{error}");
            assert_eq!(slots.available_permits(), 1);
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn download_concurrency_fails_fast_and_cancellation_releases_capacity() {
        use std::sync::atomic::Ordering;
        let (url, started, server) = download_server().await;
        let slots = Arc::new(tokio::sync::Semaphore::new(2));
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let (url, slots, client) = (url.clone(), slots.clone(), client.clone());
            tasks.push(tokio::spawn(async move {
                download_image(
                    &format!("{url}/stall"),
                    &client,
                    &slots,
                    16,
                    std::time::Duration::from_secs(5),
                )
                .await
            }));
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while started.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let error = download_image(
            &format!("{url}/stall"),
            &client,
            &slots,
            16,
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(error.contains("too many concurrent"), "{error}");
        assert_eq!(started.load(Ordering::SeqCst), 2);
        for task in tasks {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
        assert_eq!(slots.available_permits(), 2);
        assert!(download_image(
            &format!("{url}/ok"),
            &client,
            &slots,
            16,
            std::time::Duration::from_secs(1)
        )
        .await
        .is_ok());
        server.abort();
    }

    #[test]
    fn image_mime_types_are_required_and_normalized() {
        for mime in [
            "",
            "text/plain",
            "application/json",
            "image/",
            "image/bin",
            "image/../secret",
            "image/png, text/html",
        ] {
            assert!(image_extension(mime).is_err(), "{mime}");
        }
        assert_eq!(image_extension("IMAGE/PNG; charset=binary").unwrap(), "png");
        assert_eq!(image_extension("image/svg+xml").unwrap(), "svg+xml");
    }

    #[tokio::test]
    async fn non_image_and_non_base64_data_uris_are_denied() {
        for uri in [
            "data:text/plain;base64,aGk=",
            "data:image/png,aGk=",
            "data:image/;base64,aGk=",
        ] {
            assert!(fetch_image_bytes(uri).await.is_err(), "{uri}");
        }
    }

    #[tokio::test]
    async fn public_only_refuses_images_at_private_addresses_before_any_request() {
        for url in [
            "http://127.0.0.1:1/a.png",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/a.png",
            "http://localhost/a.png",
            "http://10.0.0.1/a.png",
            "http://192.168.1.1/a.png",
            "http://[fc00::1]/a.png",
            "http://[::ffff:127.0.0.1]/a.png",
            "file:///etc/passwd",
            "ftp://example.com/photo.png",
        ] {
            let error = fetch_image_bytes(url).await.unwrap_err();
            assert!(error.contains("image URL not allowed"), "{url}: {error}");
        }
        assert!(fetch_image_bytes("data:image/png;base64,aGk=")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn data_uri_images_decode_mime_wrapped_base64() {
        let (bytes, extension) = fetch_image_bytes("Data:Image/JPEG;base64,aW1h\r\nZ2U=\n")
            .await
            .unwrap();
        assert_eq!(
            (bytes.as_slice(), extension.as_str()),
            (&b"image"[..], "jpeg")
        );
    }

    #[tokio::test]
    async fn identical_content_is_confined_to_its_environment() {
        let root = ObjectStore::memory();
        let a = root.for_environment("../team-a");
        let b = root.for_environment("team-b");
        let key_a = a.put_hashed(b"same content", "json").await.unwrap();
        let key_b = b.put_hashed(b"same content", "json").await.unwrap();
        assert_ne!(key_a, key_b);
        assert!(!key_a.contains(".."));
        let uri_a = a.uri_for_key(&key_a);
        assert_eq!(
            a.read_uri_limited(&uri_a, 64).await.unwrap(),
            Some(Bytes::from_static(b"same content"))
        );
        assert_eq!(b.read_uri_limited(&uri_a, 64).await.unwrap(), None);
    }

    #[tokio::test]
    async fn limited_reads_are_confined_to_the_configured_prefix() {
        let store = ObjectStore::memory();
        assert!(
            store.prefix.is_empty(),
            "test must cover a bucket-root store"
        );
        let in_prefix_key = store.put_hashed(b"allowed", "json").await.unwrap();
        let in_prefix_uri = store.uri_for_key(&in_prefix_key);
        assert_eq!(
            store.read_uri_limited(&in_prefix_uri, 64).await.unwrap(),
            Some(Bytes::from_static(b"allowed"))
        );

        store
            .operator
            .write("other/secret.json", Bytes::from_static(b"secret"))
            .await
            .unwrap();
        let outside_uri = store.uri_for_key("other/secret.json");
        assert_eq!(
            store.read_uri_limited(&outside_uri, 64).await.unwrap(),
            None
        );
    }

    #[test]
    fn trace_content_keys_require_date_hash_and_json_extension() {
        assert!(is_trace_content_key(&format!(
            "2026-08-24/{}.json",
            "a".repeat(64)
        )));
        for key in [
            "other/secret.json",
            "2026-08-24/not-a-hash.json",
            "2026-08-24/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA.json",
            "2026-08-24/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.png",
            "2026-99-99/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json",
        ] {
            assert!(!is_trace_content_key(key), "accepted malformed key: {key}");
        }
    }
}
