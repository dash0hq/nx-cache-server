//! One process owns a private spool directory. No upload wait queue.
use super::error::ServerError;
use crate::domain::config::ServerConfig;
use axum::body::Body;
use std::{fs::File, path::PathBuf, sync::Arc, time::Duration};
use tokio::{io::AsyncWriteExt, sync::Semaphore};
use tokio_stream::StreamExt;

pub struct Uploads {
    pub permits: Arc<Semaphore>,
    pub directory: PathBuf,
    _lock: File,
}

impl Uploads {
    pub fn new(config: &ServerConfig) -> std::io::Result<Self> {
        let directory = config.spool_directory.clone();
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            builder.mode(0o700);
            builder.create(&directory)?;
            if std::fs::metadata(&directory)?.permissions().mode() & 0o077 != 0 {
                return Err(std::io::Error::other(
                    "spool directory must be private (mode 0700)",
                ));
            }
        }
        #[cfg(not(unix))]
        builder.create(&directory)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join(".lock"))?;
        lock.try_lock().map_err(std::io::Error::other)?;
        // Only this process can own this directory. Never reap another live server's files.
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with("upload-")
                && entry.file_type()?.is_file()
            {
                std::fs::remove_file(entry.path())?;
            }
        }
        Ok(Self {
            permits: Arc::new(Semaphore::new(config.max_uploads)),
            directory,
            _lock: lock,
        })
    }
}

/// Count actual bytes, not just Content-Length. One absolute deadline includes disk writes.
/// The owning task is not aborted on client cancellation. Settle outstanding writes before unlink.
pub async fn receive(
    body: Body,
    config: &ServerConfig,
    mut file: Option<&mut tokio::fs::File>,
) -> Result<u64, ServerError> {
    let result = tokio::time::timeout(Duration::from_secs(config.upload_timeout_seconds), async {
        let mut stream = body.into_data_stream();
        let mut bytes = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| ServerError::BadRequest)?;
            bytes = bytes
                .checked_add(chunk.len() as u64)
                .ok_or(ServerError::TooLarge)?;
            if bytes > config.max_upload_bytes {
                return Err(ServerError::TooLarge);
            }
            if let Some(file) = file.as_mut() {
                file.write_all(&chunk)
                    .await
                    .map_err(|_| ServerError::InternalError)?;
            }
        }
        Ok(bytes)
    })
    .await
    .unwrap_or(Err(ServerError::Timeout));
    if let Some(file) = file {
        file.flush().await.map_err(|_| ServerError::InternalError)?;
    }
    result
}
