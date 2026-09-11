//! Test the real binary and decoded OTLP, without Docker or external endpoints.
#![cfg(unix)]

use axum::{
    body::Bytes,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Router,
};
use opentelemetry_proto::tonic::{
    collector::{metrics::v1::ExportMetricsServiceRequest, trace::v1::ExportTraceServiceRequest},
    common::v1::{any_value::Value, KeyValue},
    metrics::v1::metric::Data,
};
use prost::Message;
use std::{
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn listen(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}

fn command(directory: &std::path::Path, s3: &str, port: u16) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nx-cache-aws"));
    command
        .env_clear()
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .envs([
            ("AWS_REGION", "us-east-1"),
            ("AWS_ACCESS_KEY_ID", "private-access"),
            ("AWS_SECRET_ACCESS_KEY", "private-secret"),
            ("AWS_EC2_METADATA_DISABLED", "true"),
            ("S3_ENDPOINT_URL", s3),
            ("S3_BUCKET_NAME", "private-bucket"),
            ("S3_PREFIX", "private-prefix"),
            ("SERVICE_ACCESS_TOKEN", "private-write"),
            ("READ_ONLY_ACCESS_TOKEN", "private-read"),
            ("BIND_ADDRESS", "127.0.0.1"),
            ("DEBUG", "true"),
            ("OTEL_SERVICE_NAME", "cache-telemetry-test"),
            (
                "OTEL_RESOURCE_ATTRIBUTES",
                "deployment.environment.name=test",
            ),
            (
                "OTEL_EXPORTER_OTLP_HEADERS",
                "Authorization=Bearer%20private-otlp",
            ),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", "200"),
            // Long intervals prove shutdown flushes both signals.
            ("OTEL_BSP_SCHEDULE_DELAY", "60000"),
            ("OTEL_METRIC_EXPORT_INTERVAL", "60000"),
        ])
        .env("PORT", port.to_string())
        .env("SPOOL_DIRECTORY", directory.join("spool"));
    command
}

async fn start(command: &mut Command, url: &str) -> Server {
    let mut server = Server(command.spawn().unwrap());
    let client = reqwest::Client::new();
    for _ in 0..100 {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "server exited at startup"
        );
        if client.get(format!("{url}/health")).send().await.is_ok() {
            return server;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("server not listening");
}

async fn stop(server: &mut Server) {
    assert!(Command::new("kill")
        .args(["-TERM", &server.0.id().to_string()])
        .status()
        .unwrap()
        .success());
    for _ in 0..500 {
        if let Some(status) = server.0.try_wait().unwrap() {
            assert!(status.success());
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("shutdown did not finish");
}

fn attribute<'a>(attributes: &'a [KeyValue], name: &str) -> &'a Value {
    attributes
        .iter()
        .find(|a| a.key == name)
        .unwrap()
        .value
        .as_ref()
        .unwrap()
        .value
        .as_ref()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exports_safe_correlated_traces_and_metrics_and_flushes_on_shutdown() {
    let batches = Arc::new(Mutex::new(Vec::new()));
    let captured = batches.clone();
    let collector = Router::new().route(
        "/v1/{signal}",
        post(
            move |axum::extract::Path(signal): axum::extract::Path<String>,
                  headers: HeaderMap,
                  body: Bytes| {
                let captured = captured.clone();
                async move {
                    assert_eq!(headers["authorization"], "Bearer private-otlp");
                    assert_eq!(headers["content-type"], "application/x-protobuf");
                    assert!(matches!(signal.as_str(), "traces" | "metrics"));
                    captured.lock().unwrap().push((signal, body));
                    StatusCode::OK
                }
            },
        ),
    );
    let (endpoint, collector_task) = listen(collector).await;
    let s3 = Router::new().route(
        "/{bucket}/{prefix}/{key}",
        get(|| async {
            (
                StatusCode::NOT_FOUND,
                [("content-type", "application/xml")],
                "<Error><Code>NoSuchKey</Code></Error>",
            )
        })
        .put(|body: Bytes| async move {
            assert!(!body.is_empty());
            StatusCode::OK
        }),
    );
    let (s3, s3_task) = listen(s3).await;
    let directory = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("http://127.0.0.1:{port}");
    let mut cmd = command(directory.path(), &s3, port);
    cmd.env("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint);
    let mut server = start(&mut cmd, &url).await;
    let client = reqwest::Client::new();
    let get = client
        .get(format!("{url}/v1/cache/private-hash-get"))
        .bearer_auth("private-read")
        .header(
            "traceparent",
            "00-11111111111111111111111111111111-1234567890123456-01",
        )
        .header("baggage", "secret=private-baggage")
        .send();
    let put = client
        .put(format!("{url}/v1/cache/private-hash-put"))
        .bearer_auth("private-write")
        .header(
            "traceparent",
            "00-22222222222222222222222222222222-6543210987654321-01",
        )
        .body("private-artifact")
        .send();
    let (get, put) = tokio::join!(get, put);
    assert_eq!(get.unwrap().status(), 404);
    assert_eq!(put.unwrap().status(), 200);
    assert_eq!(
        client
            .put(format!("{url}/v1/cache/private-hash-ro"))
            .bearer_auth("private-read")
            .body("rejected")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        client
            .get(format!("{url}/v1/cache/private-hash-auth"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    stop(&mut server).await;

    let batches = batches.lock().unwrap();
    let traces: Vec<_> = batches
        .iter()
        .filter(|(signal, _)| signal == "traces")
        .map(|(_, body)| ExportTraceServiceRequest::decode(body.clone()).unwrap())
        .collect();
    let resources: Vec<_> = traces.iter().flat_map(|b| &b.resource_spans).collect();
    for resource in &resources {
        let attributes = &resource.resource.as_ref().unwrap().attributes;
        assert_eq!(
            attribute(attributes, "service.name"),
            &Value::StringValue("cache-telemetry-test".into())
        );
        assert_eq!(
            attribute(attributes, "deployment.environment.name"),
            &Value::StringValue("test".into())
        );
    }
    let spans: Vec<_> = resources
        .iter()
        .flat_map(|r| &r.scope_spans)
        .flat_map(|s| &s.spans)
        .collect();
    assert_eq!(spans.len(), 6);
    for (trace_id, parent, method, status) in [
        (0x11, "1234567890123456", "GET", 404),
        (0x22, "6543210987654321", "PUT", 200),
    ] {
        let http = spans
            .iter()
            .find(|s| s.trace_id == vec![trace_id; 16] && s.kind == 2)
            .unwrap();
        let child = spans
            .iter()
            .find(|s| s.trace_id == http.trace_id && s.kind == 3)
            .unwrap();
        let expected_parent: Vec<_> = (0..16)
            .step_by(2)
            .map(|i| u8::from_str_radix(&parent[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(http.parent_span_id, expected_parent);
        assert_eq!(child.parent_span_id, http.span_id);
        assert_eq!(
            attribute(&http.attributes, "http.request.method"),
            &Value::StringValue(method.into())
        );
        assert_eq!(
            attribute(&http.attributes, "http.response.status_code"),
            &Value::IntValue(status)
        );
    }
    let metrics: Vec<_> = batches
        .iter()
        .filter(|(signal, _)| signal == "metrics")
        .map(|(_, body)| ExportMetricsServiceRequest::decode(body.clone()).unwrap())
        .collect();
    let metrics: Vec<_> = metrics
        .iter()
        .flat_map(|b| &b.resource_metrics)
        .flat_map(|r| &r.scope_metrics)
        .flat_map(|s| &s.metrics)
        .collect();
    let metric = |name| metrics.iter().find(|m| m.name == name).unwrap();
    let Some(Data::Histogram(http)) = &metric("http.server.request.duration").data else {
        panic!("missing HTTP histogram")
    };
    assert_eq!(http.data_points.iter().map(|p| p.count).sum::<u64>(), 4);
    let Some(Data::Sum(active)) = &metric("nx.cache.uploads.active").data else {
        panic!("missing active uploads")
    };
    assert_eq!(
        active.data_points[0].value,
        Some(opentelemetry_proto::tonic::metrics::v1::number_data_point::Value::AsInt(0))
    );
    let Some(Data::Histogram(artifacts)) = &metric("nx.cache.artifact.size").data else {
        panic!("missing artifact bytes")
    };
    assert_eq!(artifacts.data_points[0].count, 1);
    assert_eq!(artifacts.data_points[0].sum, Some(16.0));
    let Some(Data::Sum(lookups)) = &metric("nx.cache.lookups").data else {
        panic!("missing lookups")
    };
    assert_eq!(
        attribute(&lookups.data_points[0].attributes, "outcome"),
        &Value::StringValue("miss".into())
    );
    for (_, batch) in batches.iter() {
        assert!(
            !batch.windows(8).any(|s| s == b"private-"),
            "private request data was exported"
        );
    }
    collector_task.abort();
    s3_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_invalid_and_unreachable_exporters_do_not_change_cache_responses() {
    let (s3, task) = listen(Router::new().route(
        "/{bucket}/{prefix}/{key}",
        get(|| async {
            (
                StatusCode::NOT_FOUND,
                "<Error><Code>NoSuchKey</Code></Error>",
            )
        }),
    ))
    .await;
    for (endpoint, disabled) in [
        (None, false),
        (Some("invalid"), true),
        (Some("http://127.0.0.1:1"), false),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{port}");
        let mut cmd = command(directory.path(), &s3, port);
        if let Some(endpoint) = endpoint {
            cmd.env("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint);
        }
        cmd.env("OTEL_SDK_DISABLED", disabled.to_string());
        let mut server = start(&mut cmd, &url).await;
        assert_eq!(
            reqwest::Client::new()
                .get(format!("{url}/v1/cache/key"))
                .bearer_auth("private-read")
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        stop(&mut server).await;
    }
    for (name, endpoint) in [
        (
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "https://secret:password@example.invalid",
        ),
        ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:4318\n"),
        (
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "http://127.0.0.1:4318/v1/traces\n",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let output = command(directory.path(), &s3, 3000)
            .env(name, endpoint)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("OTLP endpoint"));
        assert!(!error.contains("password"));
    }
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signal_endpoints_disable_sampling_and_slow_collectors() {
    let paths = Arc::new(Mutex::new(Vec::new()));
    let captured = paths.clone();
    let collector = Router::new().route(
        "/{signal}",
        post(
            move |axum::extract::Path(signal): axum::extract::Path<String>| {
                let captured = captured.clone();
                async move {
                    captured.lock().unwrap().push(signal);
                    // Longer than the 200 ms exporter timeout. A listening but stuck collector.
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    StatusCode::OK
                }
            },
        ),
    );
    let (endpoint, collector_task) = listen(collector).await;
    for (signal, disabled, sampler, expected) in [
        ("TRACES", false, "always_on", vec!["TRACES"]),
        ("METRICS", false, "always_on", vec!["METRICS"]),
        ("TRACES", true, "always_on", vec![]),
        ("TRACES", false, "always_off", vec![]),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{port}");
        let mut cmd = command(directory.path(), "http://127.0.0.1:1", port);
        cmd.env(
            format!("OTEL_EXPORTER_OTLP_{signal}_ENDPOINT"),
            format!("{endpoint}/{signal}"),
        )
        .env("OTEL_SDK_DISABLED", disabled.to_string())
        .env("OTEL_TRACES_SAMPLER", sampler);
        let mut server = start(&mut cmd, &url).await;
        assert_eq!(
            reqwest::Client::new()
                .get(format!("{url}/v1/cache/key"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let start = std::time::Instant::now();
        stop(&mut server).await;
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "collector delayed shutdown beyond its timeout"
        );
        assert_eq!(*paths.lock().unwrap(), expected);
        paths.lock().unwrap().clear();
    }
    collector_task.abort();
}
