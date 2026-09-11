pub mod error;
pub mod handlers;
pub mod middleware;
pub mod uploads;
pub mod validation;

use crate::domain::{config::ServerConfig, storage::StorageProvider};
use axum::{
    middleware::from_fn_with_state,
    routing::{get, put},
    Router,
};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState<T: StorageProvider> {
    pub storage: Arc<T>,
    pub config: Arc<ServerConfig>,
    pub uploads: Arc<uploads::Uploads>,
}

pub fn create_router<T: StorageProvider + Clone>(app_state: &AppState<T>) -> Router<AppState<T>> {
    let protected_routes = Router::new()
        .route("/v1/cache/{hash}", get(handlers::retrieve_artifact::<T>))
        .route("/v1/cache/{hash}", put(handlers::store_artifact::<T>))
        .route_layer(from_fn_with_state(
            app_state.clone(),
            middleware::auth_middleware::<T>,
        ))
        .route_layer(axum::middleware::from_fn(middleware::observe));

    // Combine public and protected routes
    Router::new()
        .route("/health", get(handlers::health_check)) // Public route - no auth required
        .merge(protected_routes)
}

pub async fn run_server<T: StorageProvider + Clone>(
    storage: T,
    config: &ServerConfig,
) -> Result<(), std::io::Error> {
    let app_state = AppState {
        storage: Arc::new(storage),
        config: Arc::new(config.clone()),
        uploads: Arc::new(uploads::Uploads::new(config)?),
    };

    let uploads = app_state.uploads.clone();
    let app = create_router::<T>(&app_state).with_state(app_state);
    let addr = std::net::SocketAddr::new(config.bind_address, config.port);
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!("Server running on {}", addr);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            #[cfg(unix)]
            {
                let mut terminate =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("SIGTERM handler");
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            }
            #[cfg(not(unix))]
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    // Detached upload tasks retain permits through persistence and cleanup.
    let _all = uploads
        .permits
        .acquire_many(config.max_uploads as u32)
        .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::storage::StorageError;
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use std::collections::HashMap;
    use std::net::{IpAddr, Ipv4Addr};
    use tokio::{
        io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader},
        sync::RwLock,
    };
    use tower::ServiceExt;

    #[derive(Clone)]
    struct AbsentStorage;

    #[derive(Clone)]
    struct PresentStorage;

    #[derive(Clone, Default)]
    struct MemoryStorage {
        entries: Arc<RwLock<HashMap<String, Vec<u8>>>>,
        fail_cleanup: bool,
    }

    #[async_trait::async_trait]
    impl StorageProvider for AbsentStorage {
        async fn store(
            &self,
            _hash: &str,
            _path: &std::path::Path,
            _length: u64,
        ) -> Result<(), StorageError> {
            Ok(())
        }

        async fn retrieve(
            &self,
            _hash: &str,
        ) -> Result<Box<dyn AsyncRead + Send + Unpin>, StorageError> {
            Err(StorageError::NotFound)
        }
    }

    #[async_trait::async_trait]
    impl StorageProvider for PresentStorage {
        async fn store(
            &self,
            _hash: &str,
            _path: &std::path::Path,
            _length: u64,
        ) -> Result<(), StorageError> {
            Err(StorageError::AlreadyExists)
        }

        async fn retrieve(
            &self,
            _hash: &str,
        ) -> Result<Box<dyn AsyncRead + Send + Unpin>, StorageError> {
            Err(StorageError::NotFound)
        }
    }

    #[async_trait::async_trait]
    impl StorageProvider for MemoryStorage {
        async fn store(
            &self,
            hash: &str,
            path: &std::path::Path,
            _length: u64,
        ) -> Result<(), StorageError> {
            let bytes = tokio::fs::read(path)
                .await
                .map_err(|_| StorageError::OperationFailed)?;
            if self.fail_cleanup {
                std::fs::remove_file(path).unwrap();
                std::fs::create_dir(path).unwrap(); // Inject an unlink failure on every OS.
            }
            let mut entries = self.entries.write().await;
            if entries.contains_key(hash) {
                return Err(StorageError::AlreadyExists);
            }
            entries.insert(hash.to_owned(), bytes);
            Ok(())
        }

        async fn retrieve(
            &self,
            hash: &str,
        ) -> Result<Box<dyn AsyncRead + Send + Unpin>, StorageError> {
            let bytes = self
                .entries
                .read()
                .await
                .get(hash)
                .cloned()
                .ok_or(StorageError::NotFound)?;
            Ok(Box::new(std::io::Cursor::new(bytes)))
        }
    }

    fn test_config() -> ServerConfig {
        ServerConfig {
            port: 0,
            bind_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            service_access_token: "read-write-token".to_string(),
            read_only_access_token: Some("read-only-token".to_string()),
            debug: false,
            max_upload_bytes: 16 * 1024 * 1024,
            max_uploads: 2,
            upload_timeout_seconds: 1,
            spool_directory: std::path::PathBuf::new(),
        }
    }

    fn authorized_request(method: &str, path: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", "Bearer read-write-token")
            .body(body)
            .unwrap()
    }

    fn test_app<T: StorageProvider + Clone>(storage: T) -> (Router, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config.spool_directory = directory.path().join("spool");
        let app_state = AppState {
            storage: Arc::new(storage),
            uploads: Arc::new(uploads::Uploads::new(&config).unwrap()),
            config: Arc::new(config),
        };
        (create_router(&app_state).with_state(app_state), directory)
    }

    #[tokio::test]
    async fn cleanup_failure_is_not_reported_as_success() {
        let (app, _directory) = test_app(MemoryStorage {
            fail_cleanup: true,
            ..Default::default()
        });
        let response = app
            .oneshot(authorized_request(
                "PUT",
                "/v1/cache/cleanup",
                Body::from("artifact"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn cancelling_during_persistence_retains_the_spool_and_permit() {
        #[derive(Clone, Default)]
        struct PausedStorage {
            entered: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
        }
        #[async_trait::async_trait]
        impl StorageProvider for PausedStorage {
            async fn store(
                &self,
                _: &str,
                path: &std::path::Path,
                length: u64,
            ) -> Result<(), StorageError> {
                self.entered.notify_one();
                self.release.notified().await;
                assert_eq!(length, 9);
                assert_eq!(tokio::fs::read(path).await.unwrap(), b"persisted");
                Ok(())
            }
            async fn retrieve(
                &self,
                _: &str,
            ) -> Result<Box<dyn AsyncRead + Send + Unpin>, StorageError> {
                Err(StorageError::NotFound)
            }
        }
        let storage = PausedStorage::default();
        let directory = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config.max_uploads = 1;
        config.spool_directory = directory.path().join("spool");
        let state = AppState {
            storage: Arc::new(storage.clone()),
            uploads: Arc::new(uploads::Uploads::new(&config).unwrap()),
            config: Arc::new(config),
        };
        let app = create_router(&state).with_state(state.clone());
        let caller = tokio::spawn(app.clone().oneshot(authorized_request(
            "PUT",
            "/v1/cache/persist",
            Body::from("persisted"),
        )));
        storage.entered.notified().await;
        caller.abort();
        assert_eq!(state.uploads.permits.available_permits(), 0);
        assert_eq!(
            std::fs::read_dir(&state.uploads.directory).unwrap().count(),
            2
        );
        storage.release.notify_one();
        let _permit = state.uploads.permits.acquire().await.unwrap();
        assert_eq!(
            std::fs::read_dir(&state.uploads.directory).unwrap().count(),
            1
        );
    }

    #[tokio::test]
    async fn auth_matrix_and_exact_nx_error_content_type() {
        for (token, method, status) in [
            (None, "GET", 401),
            (Some("invalid"), "PUT", 401),
            (Some("read-only-token"), "GET", 404),
            (Some("read-only-token"), "PUT", 403),
            (Some("read-write-token"), "GET", 404),
            (Some("read-write-token"), "PUT", 200),
        ] {
            let (app, _directory) = test_app(MemoryStorage::default());
            let mut request = Request::builder().method(method).uri("/v1/cache/matrix");
            if let Some(token) = token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            let response = app
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), status);
            if status == 401 {
                assert_eq!(response.headers()["content-type"], "text/plain");
            }
        }
    }

    #[tokio::test]
    async fn upload_boundaries_lengths_and_cleanup() {
        for (actual, declared, expected) in [
            (16, None, 200),
            (17, None, 413),
            (7, Some(6), 400),
            (7, Some(8), 400),
            (16, Some(16), 200),
            (1, Some(17), 413),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut config = test_config();
            config.max_upload_bytes = 16;
            config.spool_directory = directory.path().join("spool");
            let storage = MemoryStorage::default();
            let state = AppState {
                storage: Arc::new(storage.clone()),
                uploads: Arc::new(uploads::Uploads::new(&config).unwrap()),
                config: Arc::new(config),
            };
            let app = create_router(&state).with_state(state.clone());
            let mut request =
                authorized_request("PUT", "/v1/cache/length", Body::from(vec![0x79; actual]));
            if let Some(length) = declared {
                request
                    .headers_mut()
                    .insert("content-length", length.to_string().parse().unwrap());
            }
            assert_eq!(
                app.oneshot(request).await.unwrap().status().as_u16(),
                expected
            );
            assert_eq!(
                std::fs::read_dir(&state.uploads.directory).unwrap().count(),
                1,
                "only the lock remains"
            );
            assert_eq!(
                storage.entries.read().await.contains_key("length"),
                expected == 200
            );
        }
    }

    #[tokio::test]
    async fn cancellation_does_not_release_capacity_before_body_timeout_and_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config.max_uploads = 1;
        config.spool_directory = directory.path().join("spool");
        let storage = MemoryStorage::default();
        let state = AppState {
            storage: Arc::new(storage.clone()),
            uploads: Arc::new(uploads::Uploads::new(&config).unwrap()),
            config: Arc::new(config),
        };
        let app = create_router(&state).with_state(state.clone());
        let (sender, receiver) =
            tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(1);
        sender
            .send(Ok(axum::body::Bytes::from_static(b"partial")))
            .await
            .unwrap();
        let body = Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(receiver));
        let task = tokio::spawn(app.clone().oneshot(authorized_request(
            "PUT",
            "/v1/cache/slow",
            body,
        )));
        while state.uploads.permits.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
        task.abort();
        let response = app
            .oneshot(authorized_request("PUT", "/v1/cache/busy", Body::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let _permit = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            state.uploads.permits.acquire(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(storage.entries.read().await.is_empty());
        assert_eq!(
            std::fs::read_dir(&state.uploads.directory).unwrap().count(),
            1
        );
        drop(sender);
    }

    #[tokio::test]
    async fn invalid_auth_does_not_read_an_unending_body() {
        let (app, _directory) = test_app(AbsentStorage);
        let body = Body::from_stream(tokio_stream::pending::<
            Result<axum::body::Bytes, std::io::Error>,
        >());
        let request = Request::put("/v1/cache/auth").body(body).unwrap();
        let response =
            tokio::time::timeout(std::time::Duration::from_millis(100), app.oneshot(request))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn config_and_exclusive_spool_recovery() {
        use crate::domain::config::ConfigValidator;
        let directory = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config.port = 3000;
        config.spool_directory = directory.path().join("spool");
        assert!(config.validate().await.is_ok());
        let owner = uploads::Uploads::new(&config).unwrap();
        let stale = owner.directory.join("upload-abandoned");
        std::fs::write(&stale, "abandoned").unwrap();
        assert!(uploads::Uploads::new(&config).is_err());
        assert!(stale.exists());
        drop(owner);
        let _owner = uploads::Uploads::new(&config).unwrap();
        assert!(!stale.exists());
        config.read_only_access_token = Some(config.service_access_token.clone());
        assert!(config.validate().await.is_err());
        config.read_only_access_token = Some(String::new());
        assert!(config.validate().await.is_err());
        config.read_only_access_token = None;
        config.max_uploads = 0;
        assert!(config.validate().await.is_err());
        for hash in ["../key", "é", "", &"a".repeat(129)] {
            assert!(validation::validate_hash(hash).is_err());
        }
        assert!(validation::validate_hash("A0-b_c").is_ok());
    }

    #[tokio::test]
    async fn successful_upload_returns_ok_and_preserves_artifact_bytes() {
        let storage = MemoryStorage::default();
        let (app, _directory) = test_app(storage.clone());
        let artifact = b"exact artifact bytes\0\xff";

        let response = app
            .oneshot(authorized_request(
                "PUT",
                "/v1/cache/deadbeef",
                Body::from(artifact.as_slice()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(storage.entries.read().await["deadbeef"], artifact);
    }

    #[tokio::test]
    async fn retrieve_returns_exact_artifact_with_binary_content_type() {
        let storage = MemoryStorage::default();
        let artifact = b"exact artifact bytes\0\xff";
        storage
            .entries
            .write()
            .await
            .insert("deadbeef".to_owned(), artifact.to_vec());
        let (app, _directory) = test_app(storage);

        let response = app
            .oneshot(authorized_request(
                "GET",
                "/v1/cache/deadbeef",
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-type"],
            "application/octet-stream"
        );
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            artifact.as_slice()
        );
    }

    #[tokio::test]
    async fn collision_does_not_replace_the_stored_artifact() {
        let storage = MemoryStorage::default();
        let artifact = b"original artifact";
        storage
            .entries
            .write()
            .await
            .insert("deadbeef".to_owned(), artifact.to_vec());
        let (app, _directory) = test_app(storage.clone());

        let response = app
            .oneshot(authorized_request(
                "PUT",
                "/v1/cache/deadbeef",
                Body::from("replacement"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(storage.entries.read().await["deadbeef"], artifact);
    }

    #[tokio::test]
    async fn health_check_is_public() {
        let (app, _directory) = test_app(MemoryStorage::default());
        let response = app
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            "OK"
        );
    }

    #[tokio::test]
    async fn collision_is_reported_without_closing_the_upload() {
        let (app, _directory) = test_app(PresentStorage);

        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        const BODY_LEN: usize = 8 * 1024 * 1024;
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                format!(
                    "PUT /v1/cache/deadbeef HTTP/1.1\r\nHost: localhost\r\n\
                     Authorization: Bearer read-write-token\r\nContent-Length: {BODY_LEN}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let chunk = vec![0u8; 64 * 1024];
        let mut sent = 0;
        while sent < BODY_LEN {
            stream
                .write_all(&chunk)
                .await
                .expect("connection closed while the client was still uploading");
            sent += chunk.len();
        }

        let mut status_line = String::new();
        BufReader::new(stream)
            .read_line(&mut status_line)
            .await
            .unwrap();
        assert!(
            status_line.starts_with("HTTP/1.1 409"),
            "expected a 409 status line, got: {status_line}"
        );
    }

    /// A refused write must still reach the client as a 403. The client is
    /// mid-upload when the decision is made, so the body has to be taken to
    /// completion first — otherwise the connection closes under it and the
    /// client only ever sees a write error.
    #[tokio::test]
    async fn read_only_write_is_refused_without_closing_the_upload() {
        let (app, _directory) = test_app(AbsentStorage);

        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // Bigger than the socket buffers, so the response is necessarily
        // decided while the upload is still in flight. A body small enough to
        // fit in the kernel buffer passes with or without the drain.
        const BODY_LEN: usize = 8 * 1024 * 1024;
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                format!(
                    "PUT /v1/cache/deadbeef HTTP/1.1\r\nHost: localhost\r\n\
                     Authorization: Bearer read-only-token\r\nContent-Length: {BODY_LEN}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let chunk = vec![0u8; 64 * 1024];
        let mut sent = 0;
        while sent < BODY_LEN {
            stream
                .write_all(&chunk)
                .await
                .expect("connection closed while the client was still uploading");
            sent += chunk.len();
        }

        let mut status_line = String::new();
        BufReader::new(stream)
            .read_line(&mut status_line)
            .await
            .unwrap();
        assert!(
            status_line.starts_with("HTTP/1.1 403"),
            "expected a 403 status line, got: {status_line}"
        );
    }
}
