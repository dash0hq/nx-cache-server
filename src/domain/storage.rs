use async_trait::async_trait;
use thiserror::Error;
use tokio::io::AsyncRead;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("Object not found")]
    NotFound,
    #[error("Object already exists")]
    AlreadyExists,
    #[error("Storage operation failed")]
    OperationFailed,
}

#[async_trait]
pub trait StorageProvider: Send + Sync + 'static {
    /// Atomically create an object from a complete temporary file.
    async fn store(
        &self,
        hash: &str,
        path: &std::path::Path,
        length: u64,
    ) -> Result<(), StorageError>;

    /// Retrieve object as a stream from storage
    /// Returns NotFound error if object doesn't exist
    async fn retrieve(&self, hash: &str)
        -> Result<Box<dyn AsyncRead + Send + Unpin>, StorageError>;
}
