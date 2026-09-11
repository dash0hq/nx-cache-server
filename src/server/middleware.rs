use crate::domain::storage::StorageProvider;
use crate::server::error::ServerError;
use crate::server::AppState;
use axum::{
    extract::{Request, State},
    http::Method,
    middleware::Next,
    response::{IntoResponse, Response},
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
    tokio::spawn(async move {
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
        drop(permit);
        tracing::info!(
            event = "uploads",
            active = state.config.max_uploads - uploads.permits.available_permits()
        );
        response
    })
    .await
    .map_err(|_| ServerError::InternalError)
}

pub async fn observe(request: Request, next: Next) -> Response {
    let method = match *request.method() {
        Method::GET => "GET",
        Method::PUT => "PUT",
        _ => "other",
    };
    let start = std::time::Instant::now();
    let response = next.run(request).await;
    tracing::info!(
        event = "request",
        method,
        status = response.status().as_u16(),
        elapsed_ms = start.elapsed().as_millis() as u64
    );
    response
}
