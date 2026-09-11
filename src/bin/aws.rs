use clap::Parser;
use nx_cache_server::domain::config::{ConfigValidator, ServerConfig};
use nx_cache_server::infra::aws::{AwsStorageConfig, S3Storage};
use nx_cache_server::server::run_server;

#[derive(Parser)]
#[command(name = "nx-cache-aws")]
#[command(about = "Nx Remote Cache Server - AWS S3 Backend")]
struct AwsCli {
    #[command(flatten)]
    server: ServerConfig,

    #[command(flatten)]
    storage: AwsStorageConfig,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = AwsCli::parse();
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer};
    let level = if cli.server.debug {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };
    // SDK debug traces can contain object keys and signed request details.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer().with_filter(tracing_subscriber::filter::filter_fn(
                move |meta| meta.target().starts_with("nx_cache") && *meta.level() <= level,
            )),
        )
        .init();
    tracing::debug!(
        max_upload_bytes = cli.server.max_upload_bytes,
        max_uploads = cli.server.max_uploads,
        "Upload limits"
    );

    // Validate server configuration
    if let Err(e) = cli.server.validate().await {
        eprintln!("{}", e);
        std::process::exit(1);
    }

    // Validate storage configuration
    if let Err(e) = cli.storage.validate().await {
        eprintln!("{}", e);
        std::process::exit(1);
    }

    // Initialize storage
    let storage = match S3Storage::new(&cli.storage).await {
        Ok(storage) => storage,
        Err(e) => {
            eprintln!();
            eprintln!("Failed to initialize S3 storage: {}", e);
            eprintln!();
            eprintln!("Please check your AWS credentials and configuration.");
            std::process::exit(1);
        }
    };

    // Run server
    tracing::info!(
        "Server starting on {}",
        std::net::SocketAddr::new(cli.server.bind_address, cli.server.port)
    );
    if let Err(e) = run_server(storage, &cli.server).await {
        eprintln!();
        eprintln!("Server error: {}", e);
        std::process::exit(1);
    }

    Ok(())
}
