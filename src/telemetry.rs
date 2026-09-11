//! Explicit OTLP export. No automatic SDK spans, request URLs, headers or baggage.
use opentelemetry::{
    global,
    metrics::{Counter, Histogram, UpDownCounter},
    trace::{SpanKind, TraceContextExt, Tracer},
    Context, KeyValue,
};
use opentelemetry_sdk::{metrics::SdkMeterProvider, trace::SdkTracerProvider, Resource};
use std::sync::OnceLock;

pub struct Telemetry {
    traces: Option<SdkTracerProvider>,
    metrics: Option<SdkMeterProvider>,
}

impl Telemetry {
    pub fn init() -> Result<Self, &'static str> {
        let mut telemetry = Self {
            traces: None,
            metrics: None,
        };
        if std::env::var("OTEL_SDK_DISABLED").is_ok_and(|v| v.eq_ignore_ascii_case("true")) {
            return Ok(telemetry);
        }
        let traces = enabled("TRACES")?;
        let metrics = enabled("METRICS")?;
        if !traces && !metrics {
            return Ok(telemetry);
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let resource = Resource::builder()
            .with_attributes([KeyValue::new("service.version", env!("CARGO_PKG_VERSION"))])
            .build();
        if traces {
            let exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .build()
                .map_err(|_| "Cannot configure OTLP traces exporter")?;
            let provider = SdkTracerProvider::builder()
                .with_resource(resource.clone())
                .with_batch_exporter(exporter)
                .build();
            global::set_tracer_provider(provider.clone());
            telemetry.traces = Some(provider);
        }
        if metrics {
            let exporter = opentelemetry_otlp::MetricExporter::builder()
                .with_http()
                .build()
                .map_err(|_| "Cannot configure OTLP metrics exporter")?;
            let provider = SdkMeterProvider::builder()
                .with_resource(resource)
                .with_periodic_exporter(exporter)
                .build();
            global::set_meter_provider(provider.clone());
            telemetry.metrics = Some(provider);
        }
        Ok(telemetry)
    }

    pub fn shutdown(self) {
        if let Some(provider) = self.metrics {
            if provider.shutdown().is_err() {
                tracing::warn!(
                    event = "telemetry",
                    signal = "metrics",
                    "OTLP shutdown failed"
                );
            }
        }
        if let Some(provider) = self.traces {
            if provider.shutdown().is_err() {
                tracing::warn!(
                    event = "telemetry",
                    signal = "traces",
                    "OTLP shutdown failed"
                );
            }
        }
    }
}

fn enabled(signal: &str) -> Result<bool, &'static str> {
    let endpoint = std::env::var(format!("OTEL_EXPORTER_OTLP_{signal}_ENDPOINT"))
        .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT"));
    let Ok(endpoint) = endpoint else {
        return Ok(false);
    };
    // OTLP parses the original string as an HTTP URI, not URL's whitespace-normalized form.
    let uri: axum::http::Uri = endpoint.parse().map_err(|_| "Invalid OTLP endpoint")?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.authority().is_none() {
        return Err("Invalid OTLP endpoint");
    }
    let url = reqwest::Url::parse(&endpoint).map_err(|_| "Invalid OTLP endpoint")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("OTLP endpoint must be HTTP/S without credentials, query or fragment");
    }
    let protocol = std::env::var(format!("OTEL_EXPORTER_OTLP_{signal}_PROTOCOL"))
        .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL"));
    if protocol.is_ok_and(|v| v != "http/protobuf") {
        return Err("Only OTLP http/protobuf is supported");
    }
    Ok(true)
}

/// End this operation even when a cancelled future leaves context in a detached upload.
pub struct Span(pub Context);

impl Span {
    pub fn new(
        name: &'static str,
        kind: SpanKind,
        parent: &Context,
        attributes: Vec<KeyValue>,
    ) -> Self {
        let tracer = global::tracer("nx-cache-server");
        let span = tracer
            .span_builder(name)
            .with_kind(kind)
            .with_attributes(attributes)
            .start_with_context(&tracer, parent);
        Self(parent.with_span(span))
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        self.0.span().end();
    }
}

pub struct Instruments {
    pub requests: Histogram<f64>,
    pub s3: Histogram<f64>,
    pub lookups: Counter<u64>,
    pub active_uploads: UpDownCounter<i64>,
    pub spool_size: Histogram<u64>,
    pub artifacts: Histogram<u64>,
    pub cleanup_errors: Counter<u64>,
}

/// The binary initializes providers once, before serving. Disabled export uses no-op instruments.
pub fn instruments() -> &'static Instruments {
    static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();
    INSTRUMENTS.get_or_init(|| {
        let meter = global::meter("nx-cache-server");
        Instruments {
            requests: meter
                .f64_histogram("http.server.request.duration")
                .with_unit("s")
                .with_boundaries(vec![
                    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 120.0,
                ])
                .with_description(
                    "Time until a response is constructed, excluding streamed download delivery",
                )
                .build(),
            s3: meter
                .f64_histogram("nx.cache.s3.duration")
                .with_unit("s")
                .with_boundaries(vec![
                    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 120.0,
                ])
                .with_description("One PUT attempt or GET SDK call, including SDK retries")
                .build(),
            lookups: meter.u64_counter("nx.cache.lookups").build(),
            active_uploads: meter.i64_up_down_counter("nx.cache.uploads.active").build(),
            spool_size: meter
                .u64_histogram("nx.cache.spool.size")
                .with_unit("By")
                .with_boundaries(vec![
                    1024.0,
                    65536.0,
                    1048576.0,
                    16777216.0,
                    268435456.0,
                    5368709120.0,
                ])
                .with_description("Completed inbound spool sizes, not live disk occupancy")
                .build(),
            artifacts: meter
                .u64_histogram("nx.cache.artifact.size")
                .with_unit("By")
                .with_boundaries(vec![
                    1024.0,
                    65536.0,
                    1048576.0,
                    16777216.0,
                    268435456.0,
                    5368709120.0,
                ])
                .with_description(
                    "Successfully stored or retrieved object sizes, not client-delivered bytes",
                )
                .build(),
            cleanup_errors: meter.u64_counter("nx.cache.spool.cleanup.errors").build(),
        }
    })
}

pub struct ActiveUpload;

impl ActiveUpload {
    pub fn start() -> Self {
        instruments().active_uploads.add(1, &[]);
        Self
    }
}

impl Drop for ActiveUpload {
    fn drop(&mut self) {
        instruments().active_uploads.add(-1, &[]);
    }
}
