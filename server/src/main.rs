mod config;
mod http_probe;
mod monitor;
mod store;
mod web;

use std::{env, error::Error, path::PathBuf};

use crate::{config::AppConfig, monitor::start_tasks, store::Store};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    init_logging();
    if let Err(error) = run().await {
        error!(error = %error, "server exited with an error");
        std::process::exit(1);
    }
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi_sanitization(false)
        .with_target(false)
        .compact()
        .init();
}

async fn run() -> Result<(), Box<dyn Error>> {
    let config_path = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.yaml"));
    let config = AppConfig::load(&config_path)?;
    let store = Store::open(
        &config.storage.path,
        config.storage.chunk_minutes,
        config.storage.retention_days,
        config.storage.cache_max_mb,
    )?;
    info!(
        config_path = %config_path.display(),
        server_name = %config.server.name,
        monitor_count = config.monitors.len(),
        "configuration loaded"
    );
    info!(
        storage_path = %config.storage.path,
        chunk_minutes = config.storage.chunk_minutes,
        retention_days = config.storage.retention_days,
        cache_max_mb = config.storage.cache_max_mb,
        "history storage ready"
    );
    start_tasks(&config, store.clone());
    web::serve(config, store).await?;
    Ok(())
}
