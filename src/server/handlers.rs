use crate::domain::storage::StorageProvider;
use crate::server::{error::ServerError, validation, AppState};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};

pub async fn store_artifact<T: StorageProvider>(
    Path(hash): Path<String>,
    State(state): State<AppState<T>>,
    headers: HeaderMap,
    body: Body,
) -> Result<impl IntoResponse, ServerError> {
    validation::validate_hash(&hash)?;
    let declared = headers
        .get("content-length")
        .map(|value| {
            value
                .to_str()
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or(ServerError::BadRequest)
        })
        .transpose()?;
    if declared.is_some_and(|n| n > state.config.max_upload_bytes) {
        return Err(ServerError::TooLarge);
    }
    let spool = tempfile::Builder::new()
        .prefix("upload-")
        .tempfile_in(&state.uploads.directory)
        .map_err(|_| ServerError::InternalError)?;
    let (file, path) = spool.into_parts();
    let mut file = tokio::fs::File::from_std(file);
    let received = super::uploads::receive(body, &state.config, Some(&mut file)).await;
    drop(file);
    let bytes = received.as_ref().ok().copied();
    tracing::info!(event = "spool", added_bytes = bytes);
    let result = async {
        let bytes = received?;
        if declared.is_some_and(|n| n != bytes) {
            return Err(ServerError::BadRequest);
        }
        state.storage.store(&hash, &path, bytes).await?;
        Ok((StatusCode::OK, ""))
    }
    .await;
    path.close().map_err(|_| {
        tracing::error!(event = "spool", cleanup_error = true);
        ServerError::InternalError
    })?;
    tracing::info!(event = "spool", removed_bytes = bytes);
    result
}

pub async fn retrieve_artifact<T: StorageProvider>(
    Path(hash): Path<String>,
    State(state): State<AppState<T>>,
) -> Result<impl IntoResponse, ServerError> {
    validation::validate_hash(&hash)?;

    let reader = state.storage.retrieve(&hash).await?;
    let stream = tokio_util::io::ReaderStream::new(reader);
    let body = Body::from_stream(stream);

    Ok((
        StatusCode::OK,
        [("content-type", "application/octet-stream")],
        body,
    ))
}

pub async fn health_check() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}
