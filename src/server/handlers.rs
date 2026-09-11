use crate::domain::storage::StorageProvider;
use crate::server::{error::ServerError, validation, AppState};
use axum::{
    body::Body,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use tokio::io::AsyncWriteExt;
use tokio_stream::StreamExt;

pub async fn store_artifact<T: StorageProvider>(
    Path(hash): Path<String>,
    State(state): State<AppState<T>>,
    body: Body,
) -> Result<impl IntoResponse, ServerError> {
    validation::validate_hash(&hash)?;
    let spool = tempfile::NamedTempFile::new()?;
    let (file, path) = spool.into_parts();
    let mut file = tokio::fs::File::from_std(file);
    let mut body = body.into_data_stream();
    let mut length = 0u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|_| ServerError::BadRequest)?;
        length = length
            .checked_add(chunk.len() as u64)
            .ok_or(ServerError::TooLarge)?;
        if length > state.config.max_upload_bytes {
            return Err(ServerError::TooLarge);
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    state.storage.store(&hash, &path, length).await?;

    Ok((StatusCode::OK, ""))
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
