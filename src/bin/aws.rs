use clap::Parser;
use nx_cache_server::domain::config::ServerConfig;
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
    let telemetry = nx_cache_server::telemetry::Telemetry::init()?;
    let cli = AwsCli::parse();

    // Validate server configuration
    if let Err(e) = cli.server.validate() {
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
    let result = run_server(storage, &cli.server).await;
    tokio::task::spawn_blocking(move || telemetry.shutdown()).await?;
    result?;

    Ok(())
}
