use crate::domain::storage::StorageProvider;
use crate::server::error::ServerError;
use crate::server::AppState;
use crate::telemetry;
use axum::{
    extract::{Request, State},
    http::Method,
    middleware::Next,
    response::{IntoResponse, Response},
};
use opentelemetry::{
    propagation::TextMapPropagator,
    trace::{FutureExt, SpanKind, Status, TraceContextExt},
    Context, KeyValue,
};
use subtle::ConstantTimeEq;

pub async fn auth_middleware<T>(
    State(state): State<AppState<T>>,
    request: Request,
    next: Next,
) -> Result<Response, ServerError>
where
    T: StorageProvider,
{
    // Extract Bearer token from Authorization header
    let token = request
        .headers()
        .get("authorization")
        .and_then(|header| header.to_str().ok())
        .and_then(|auth_value| auth_value.strip_prefix("Bearer "));

    let token = match token {
        Some(t) => t,
        None => return Err(ServerError::Unauthorized),
    };

    // Constant-time comparisons for security. Both tokens are always
    // compared so timing does not reveal which one matched.
    let is_read_write = bool::from(
        token
            .as_bytes()
            .ct_eq(state.config.service_access_token.as_bytes()),
    );
    let is_read_only = state
        .config
        .read_only_access_token
        .as_deref()
        .is_some_and(|read_only| bool::from(token.as_bytes().ct_eq(read_only.as_bytes())));

    if !is_read_write && !is_read_only {
        return Err(ServerError::Unauthorized);
    }

    if request.method() == Method::GET {
        return Ok(next.run(request).await);
    }
    let permit = state
        .uploads
        .permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| ServerError::Busy)?;
    // Own the body and permit independently of the HTTP caller. Never abort a spool writer.
    let context = Context::current();
    tokio::spawn(
        async move {
            let active = telemetry::ActiveUpload::start();
            let uploads = state.uploads;
            tracing::info!(
                event = "uploads",
                active = state.config.max_uploads - uploads.permits.available_permits()
            );
            let response = if !is_read_write {
                match super::uploads::receive(request.into_body(), &state.config, None).await {
                    Ok(_) => ServerError::Forbidden.into_response(),
                    Err(error) => error.into_response(),
                }
            } else {
                next.run(request).await
            };
            drop(active);
            drop(permit);
            tracing::info!(
                event = "uploads",
                active = state.config.max_uploads - uploads.permits.available_permits()
            );
            response
        }
        .with_context(context),
    )
    .await
    .map_err(|_| ServerError::InternalError)
}

pub async fn observe(request: Request, next: Next) -> Response {
    let method = match *request.method() {
        Method::GET => "GET",
        Method::PUT => "PUT",
        _ => "other",
    };
    let headers: std::collections::HashMap<_, _> = ["traceparent", "tracestate"]
        .into_iter()
        .filter_map(|key| {
            request
                .headers()
                .get(key)?
                .to_str()
                .ok()
                .map(|value| (key.to_owned(), value.to_owned()))
        })
        .collect();
    let parent = opentelemetry_sdk::propagation::TraceContextPropagator::new().extract(&headers);
    let mut attributes = vec![
        KeyValue::new("http.request.method", method),
        KeyValue::new("http.route", "/v1/cache/{hash}"),
    ];
    let name = match method {
        "GET" => "GET /v1/cache/{hash}",
        "PUT" => "PUT /v1/cache/{hash}",
        _ => "HTTP /v1/cache/{hash}",
    };
    let span = telemetry::Span::new(name, SpanKind::Server, &parent, attributes.clone());
    let start = std::time::Instant::now();
    let response = next.run(request).with_context(span.0.clone()).await;
    let status = response.status().as_u16();
    let status_attribute = KeyValue::new("http.response.status_code", i64::from(status));
    span.0.span().set_attribute(status_attribute.clone());
    if response.status().is_server_error() {
        span.0.span().set_status(Status::error("request failed"));
    }
    attributes.push(status_attribute);
    telemetry::instruments()
        .requests
        .record(start.elapsed().as_secs_f64(), &attributes);
    tracing::info!(
        event = "request",
        method,
        status,
        elapsed_ms = start.elapsed().as_millis() as u64
    );
    response
}
