mod config;
mod http_probe;
mod monitor;
mod store;
mod web;

use std::{env, error::Error, path::PathBuf};

use crate::{config::AppConfig, monitor::start_tasks, store::Store};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("wk-status-371tti-net: {error}");
        std::process::exit(1);
    }
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
    )?;
    println!(
        "Loaded {} monitor(s) from {}",
        config.monitors.len(),
        config_path.display()
    );
    start_tasks(&config, store.clone());
    web::serve(config, store).await?;
    Ok(())
}
