use crate::telemetry;
use async_trait::async_trait;
use aws_config::default_provider::credentials::DefaultCredentialsChain;
use aws_config::environment::region::EnvironmentVariableRegionProvider;
use aws_config::imds::region::ImdsRegionProvider;
use aws_config::meta::region::future::ProvideRegion as ProvideRegionFuture;
use aws_config::meta::region::{ProvideRegion, RegionProviderChain};
use aws_config::profile::region::ProfileFileRegionProvider;
use aws_config::provider_config::ProviderConfig;
use aws_credential_types::provider::future::ProvideCredentials as ProvideCredentialsFuture;
use aws_sdk_s3::config::timeout::TimeoutConfig;
use aws_sdk_s3::config::SharedHttpClient;
use aws_sdk_s3::config::{Credentials, ProvideCredentials};
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::{config::Region, Client, Config as S3Config};
use aws_smithy_http_client::tls::rustls_provider::CryptoMode;
use aws_smithy_http_client::{tls, Builder as HttpClientBuilder};
use clap::Parser;
use opentelemetry::{
    trace::{FutureExt, SpanKind, Status, TraceContextExt},
    Context, KeyValue,
};
use tokio::io::AsyncRead;

use crate::domain::{
    config::{ConfigError, ConfigValidator},
    storage::{StorageError, StorageProvider},
};

/// HTTPS client backed by rustls + ring.
///
/// Avoids the SDK default (`aws-lc-rs` → `aws-lc-sys`), which needs a
/// C/CMake/NASM toolchain and broke cross-platform release builds. Disabling
/// `default-https-client` drops the SDK's auto connector, so this is wired
/// explicitly into the S3 client and the credential/region chains below.
fn https_client() -> SharedHttpClient {
    HttpClientBuilder::new()
        .tls_provider(tls::Provider::Rustls(CryptoMode::Ring))
        .build_https()
}

#[derive(Parser, Debug, Clone)]
pub struct AwsStorageConfig {
    #[arg(
        long,
        env = "AWS_REGION",
        help = "AWS region (e.g., us-west-2). Auto-discovered from environment, AWS config, or EC2/ECS metadata if not provided"
    )]
    pub region: Option<String>,

    #[arg(
        long,
        env = "AWS_ACCESS_KEY_ID",
        help = "AWS access key ID. Optional - uses AWS credential provider chain (environment, config file, IAM roles) if not provided"
    )]
    pub access_key_id: Option<String>,

    #[arg(
        long,
        env = "AWS_SECRET_ACCESS_KEY",
        help = "AWS secret access key. Required if --access-key-id is provided"
    )]
    pub secret_access_key: Option<String>,

    #[arg(
        long,
        env = "AWS_SESSION_TOKEN",
        help = "AWS session token for temporary security credentials. Optional"
    )]
    pub session_token: Option<String>,

    #[arg(
        long,
        env = "S3_BUCKET_NAME",
        help = "S3 bucket name for cache storage"
    )]
    pub bucket_name: String,

    #[arg(
        long,
        env = "S3_ENDPOINT_URL",
        help = "Custom S3 endpoint URL (e.g., http://localhost:9000 for MinIO). Optional - uses AWS S3 if not provided"
    )]
    pub endpoint_url: Option<String>,

    #[arg(
        long,
        env = "S3_TIMEOUT",
        default_value = "30",
        help = "S3 operation timeout in seconds"
    )]
    pub timeout_seconds: u64,

    #[arg(long, env = "S3_PREFIX", default_value = "nx-cache")]
    pub prefix: String,
}

impl ProvideRegion for AwsStorageConfig {
    fn region(&self) -> ProvideRegionFuture<'_> {
        let region = self.region.clone();
        ProvideRegionFuture::new(async move {
            // Rebuild the env -> profile -> IMDS chain with our client, since
            // `or_default_provider()` would have no transport without it.
            let provider_config = ProviderConfig::default().with_http_client(https_client());
            RegionProviderChain::first_try(region.map(Region::new))
                .or_else(EnvironmentVariableRegionProvider::new())
                .or_else(
                    ProfileFileRegionProvider::builder()
                        .configure(&provider_config)
                        .build(),
                )
                .or_else(
                    ImdsRegionProvider::builder()
                        .configure(&provider_config)
                        .build(),
                )
                .region()
                .await
        })
    }
}

impl ProvideCredentials for AwsStorageConfig {
    fn provide_credentials<'a>(&'a self) -> ProvideCredentialsFuture<'a>
    where
        Self: 'a,
    {
        match (self.access_key_id.as_ref(), self.secret_access_key.as_ref()) {
            (Some(access_key_id), Some(secret_access_key)) => {
                ProvideCredentialsFuture::ready(Ok(Credentials::new(
                    access_key_id,
                    secret_access_key,
                    self.session_token.clone(),
                    None,
                    "nx-cache-server",
                )))
            }
            _ => ProvideCredentialsFuture::new(async {
                // `DefaultCredentialsChain::build()` panics without a configured
                // connector once `default-https-client` is disabled.
                let provider_config = ProviderConfig::default().with_http_client(https_client());
                DefaultCredentialsChain::builder()
                    .configure(provider_config)
                    .region(self.clone())
                    .build()
                    .await
                    .provide_credentials()
                    .await
            }),
        }
    }
}

impl ConfigValidator for AwsStorageConfig {
    async fn validate(&self) -> Result<(), ConfigError> {
        if self.bucket_name.is_empty() {
            return Err(ConfigError::MissingField("S3_BUCKET_NAME"));
        }
        if self.timeout_seconds == 0 || self.timeout_seconds > 300 {
            return Err(ConfigError::Invalid("S3_TIMEOUT must be 1..=300 seconds"));
        }
        if self.prefix.is_empty()
            || self.prefix.len() > 128
            || !self
                .prefix
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            return Err(ConfigError::Invalid(
                "S3_PREFIX must be a 1..=128 byte ASCII alphanumeric, hyphen or underscore segment",
            ));
        }
        if let Some(endpoint_url) = &self.endpoint_url {
            if !endpoint_url.starts_with("http://") && !endpoint_url.starts_with("https://") {
                return Err(ConfigError::Invalid(
                    "S3 endpoint URL must start with http:// or https://",
                ));
            }
        }
        match (self.access_key_id.as_ref(), self.secret_access_key.as_ref()) {
            (Some(..), None) => return Err(ConfigError::MissingField("AWS_SECRET_ACCESS_KEY")),
            (None, Some(..)) => return Err(ConfigError::MissingField("AWS_ACCESS_KEY_ID")),
            _ => {}
        }
        if self.region().await.is_none() {
            return Err(ConfigError::MissingField("AWS_REGION"));
        }

        Ok(())
    }
}

#[derive(Clone)]
pub struct S3Storage {
    client: Client,
    bucket_name: String,
    prefix: String,
}

impl S3Storage {
    pub async fn new(config: &AwsStorageConfig) -> Result<Self, StorageError> {
        // Resolve region once - validation already ensured it exists
        let region = config.region().await.ok_or_else(|| {
            tracing::error!("AWS_REGION must be set");
            StorageError::OperationFailed
        })?;

        let mut s3_config_builder = S3Config::builder()
            .behavior_version_latest()
            .http_client(https_client())
            .region(region)
            .credentials_provider(config.clone())
            .timeout_config(
                TimeoutConfig::builder()
                    .operation_timeout(std::time::Duration::from_secs(config.timeout_seconds))
                    .build(),
            );

        // Configure for custom S3-compatible endpoints (MinIO, Hetzner, etc.)
        if let Some(endpoint_url) = &config.endpoint_url {
            s3_config_builder = s3_config_builder
                .endpoint_url(endpoint_url)
                .force_path_style(true); // Required for most S3-compatible services
        }

        let s3_config = s3_config_builder.build();

        let client = Client::from_conf(s3_config);

        Ok(Self {
            client,
            bucket_name: config.bucket_name.clone(),
            prefix: config.prefix.clone(),
        })
    }
}

#[async_trait]
impl StorageProvider for S3Storage {
    async fn store(
        &self,
        hash: &str,
        path: &std::path::Path,
        length: u64,
    ) -> Result<(), StorageError> {
        use aws_sdk_s3::{
            config::retry::RetryConfig,
            primitives::{ByteStream, Length},
        };
        for attempt in 0..3 {
            let body = ByteStream::read_from()
                .path(path)
                .length(Length::Exact(length))
                .build()
                .await
                .map_err(|_| StorageError::OperationFailed)?;
            let span = telemetry::Span::new(
                "S3 PutObject",
                SpanKind::Client,
                &Context::current(),
                vec![
                    KeyValue::new("rpc.system", "aws-api"),
                    KeyValue::new("rpc.service", "S3"),
                    KeyValue::new("rpc.method", "PutObject"),
                ],
            );
            let start = std::time::Instant::now();
            let result = self
                .client
                .put_object()
                .bucket(&self.bucket_name)
                .key(format!("{}/{}", self.prefix, hash))
                .if_none_match("*")
                .content_length(length as i64)
                .body(body)
                .customize()
                .config_override(S3Config::builder().retry_config(RetryConfig::disabled()))
                .send()
                .with_context(span.0.clone())
                .await;
            telemetry::instruments().s3.record(
                start.elapsed().as_secs_f64(),
                &[
                    KeyValue::new("operation", "put"),
                    KeyValue::new("error", result.is_err()),
                ],
            );
            if result.is_err() {
                span.0.span().set_status(Status::error("S3 PUT failed"));
            }
            drop(span);
            tracing::info!(
                event = "s3",
                operation = "put",
                elapsed_ms = start.elapsed().as_millis() as u64,
                error = result.is_err(),
                bytes = length
            );
            let Err(error) = result else {
                telemetry::instruments()
                    .artifacts
                    .record(length, &[KeyValue::new("operation", "put")]);
                return Ok(());
            };
            let status = error.raw_response().map(|r| r.status().as_u16());
            let code = error.as_service_error().and_then(|e| e.code());
            match put_failure(status, code) {
                PutFailure::Exists => return Err(StorageError::AlreadyExists),
                PutFailure::Retry if attempt < 2 => (),
                _ => return Err(StorageError::OperationFailed),
            }
            tokio::time::sleep(std::time::Duration::from_millis(50 * (attempt + 1))).await;
        }
        Err(StorageError::OperationFailed)
    }

    async fn retrieve(
        &self,
        hash: &str,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>, StorageError> {
        let span = telemetry::Span::new(
            "S3 GetObject",
            SpanKind::Client,
            &Context::current(),
            vec![
                KeyValue::new("rpc.system", "aws-api"),
                KeyValue::new("rpc.service", "S3"),
                KeyValue::new("rpc.method", "GetObject"),
            ],
        );
        let start = std::time::Instant::now();
        let result = self
            .client
            .get_object()
            .bucket(&self.bucket_name)
            .key(format!("{}/{}", self.prefix, hash))
            .send()
            .with_context(span.0.clone())
            .await
            .map_err(|e| match e.into_service_error() {
                GetObjectError::NoSuchKey(_) => StorageError::NotFound,
                _ => {
                    tracing::error!(event = "s3", operation = "get", error = true);
                    StorageError::OperationFailed
                }
            });
        let outcome = match &result {
            Ok(_) => "hit",
            Err(StorageError::NotFound) => "miss",
            Err(_) => "error",
        };
        telemetry::instruments()
            .lookups
            .add(1, &[KeyValue::new("outcome", outcome)]);
        telemetry::instruments().s3.record(
            start.elapsed().as_secs_f64(),
            &[
                KeyValue::new("operation", "get"),
                KeyValue::new("error", outcome == "error"),
            ],
        );
        if outcome == "error" {
            span.0.span().set_status(Status::error("S3 GET failed"));
        }
        span.0
            .span()
            .set_attribute(KeyValue::new("nx.cache.outcome", outcome));
        let result = result?;
        if let Some(bytes) = result.content_length().and_then(|n| u64::try_from(n).ok()) {
            telemetry::instruments()
                .artifacts
                .record(bytes, &[KeyValue::new("operation", "get")]);
        }
        tracing::info!(
            event = "s3",
            operation = "get",
            elapsed_ms = start.elapsed().as_millis() as u64,
            bytes = result.content_length().unwrap_or(0)
        );

        // Direct streaming - no buffering
        Ok(Box::new(result.body.into_async_read()))
    }
}

#[derive(Debug, PartialEq)]
enum PutFailure {
    Exists,
    Retry,
    Failed,
}

fn put_failure(status: Option<u16>, code: Option<&str>) -> PutFailure {
    match (status, code) {
        (Some(412), Some("PreconditionFailed")) => PutFailure::Exists,
        (Some(409), Some("ConditionalRequestConflict")) => PutFailure::Retry,
        _ => PutFailure::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn conditional_retries_replay_the_complete_body_and_stop_after_three_attempts() {
        use axum::{
            body::{to_bytes, Body},
            http::{Request, StatusCode},
            routing::put,
            Router,
        };
        use std::sync::{Arc, Mutex};
        let payload = b"asymmetric replay\0\xff0123456789";
        for (status, code, expected_attempts) in [
            (409, "ConditionalRequestConflict", 3),
            (409, "OtherConflict", 1),
            (412, "PreconditionFailed", 1),
        ] {
            let received = Arc::new(Mutex::new(Vec::new()));
            let captured = received.clone();
            let app = Router::new().route(
                "/bucket/prefix/key",
                put(move |request: Request<Body>| {
                    let captured = captured.clone();
                    async move {
                        assert_eq!(request.headers()["if-none-match"], "*");
                        let body = to_bytes(request.into_body(), 8192).await.unwrap();
                        captured.lock().unwrap().push(body);
                        (
                            StatusCode::from_u16(status).unwrap(),
                            [("content-type", "application/xml")],
                            format!("<Error><Code>{code}</Code></Error>"),
                        )
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let storage = S3Storage::new(&AwsStorageConfig {
                region: Some("us-east-1".into()),
                access_key_id: Some("local".into()),
                secret_access_key: Some("local-secret".into()),
                session_token: None,
                bucket_name: "bucket".into(),
                endpoint_url: Some(endpoint),
                timeout_seconds: 2,
                prefix: "prefix".into(),
            })
            .await
            .unwrap();
            let file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(file.path(), payload).unwrap();
            let result = storage
                .store("key", file.path(), payload.len() as u64)
                .await;
            assert!(result.is_err());
            assert_eq!(
                matches!(result, Err(StorageError::AlreadyExists)),
                status == 412
            );
            let bodies = received.lock().unwrap();
            assert_eq!(bodies.len(), expected_attempts);
            // Decode aws-chunked framing; chunk boundaries can differ between attempts.
            for body in bodies.iter() {
                let mut encoded = body.as_ref();
                let mut decoded = Vec::new();
                loop {
                    let end = encoded.windows(2).position(|w| w == b"\r\n").unwrap();
                    let line = std::str::from_utf8(&encoded[..end]).unwrap();
                    let size = usize::from_str_radix(line.split(';').next().unwrap(), 16).unwrap();
                    if size == 0 {
                        break;
                    }
                    encoded = &encoded[end + 2..];
                    decoded.extend_from_slice(&encoded[..size]);
                    encoded = &encoded[size + 2..];
                }
                assert_eq!(decoded, payload);
            }
            server.abort();
        }
    }

    #[test]
    fn conditional_errors_are_not_generic_http_conflicts() {
        assert_eq!(
            put_failure(Some(412), Some("PreconditionFailed")),
            PutFailure::Exists
        );
        assert_eq!(
            put_failure(Some(409), Some("ConditionalRequestConflict")),
            PutFailure::Retry
        );
        for (status, code) in [
            (409, "Other"),
            (412, "AccessDenied"),
            (500, "PreconditionFailed"),
            (403, "AccessDenied"),
        ] {
            assert_eq!(put_failure(Some(status), Some(code)), PutFailure::Failed);
        }
    }
}
