use opentelemetry::{global, trace::TracerProvider as _};
use opentelemetry_sdk::{propagation::TraceContextPropagator, trace::SdkTracerProvider};
use tracing_subscriber::{filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt};

pub struct Telemetry(Option<SdkTracerProvider>);

impl Telemetry {
    pub fn init() -> Result<Self, Box<dyn std::error::Error>> {
        let disabled = std::env::var("OTEL_SDK_DISABLED")
            .is_ok_and(|value| value.eq_ignore_ascii_case("true"));
        let enabled = !disabled
            && (std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_some()
                || std::env::var_os("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").is_some());
        let provider = if enabled {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .build()?;
            let provider = SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
                .build();
            global::set_text_map_propagator(TraceContextPropagator::new());
            Some(provider)
        } else {
            None
        };
        let layer = provider.as_ref().map(|provider| {
            tracing_opentelemetry::layer().with_tracer(provider.tracer("nx-cache-server"))
        });
        tracing_subscriber::registry()
            .with(LevelFilter::INFO)
            .with(tracing_subscriber::fmt::layer())
            .with(layer)
            .init();
        Ok(Self(provider))
    }

    pub fn shutdown(self) {
        if self.0.is_some_and(|provider| provider.shutdown().is_err()) {
            tracing::warn!("OpenTelemetry shutdown failed");
        }
    }
}
