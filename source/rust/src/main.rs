use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use aisearch_emulator::api::{build_router, AppState};
use aisearch_emulator::config::{Config, StorageMode, DEFAULT_PORT};
use aisearch_emulator::storage::{InMemoryStorage, Storage};
use anyhow::{bail, Context};
use serde_json::Value;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("healthcheck") => return run_healthcheck(),
        Some("help" | "--help" | "-h") => {
            print_usage();
            return Ok(());
        }
        Some(other) => bail!("unknown command {other:?}; run with --help for usage"),
        None => {}
    }

    let config = Config::from_env().map_err(anyhow::Error::from)?;
    init_logging(&config.log_level);

    if config.storage_mode == StorageMode::File {
        bail!(
            "EMULATOR_STORAGE__MODE=file is not yet implemented (Phase 2). \
             Use EMULATOR_STORAGE__MODE=memory."
        );
    }

    let storage: Arc<dyn Storage> = Arc::new(InMemoryStorage::new());
    let state = AppState::new(config.clone(), storage);
    let app = build_router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    tracing::info!(
        port = config.port,
        api_versions = %config.api_versions.join(","),
        "aisearch-emulator listening"
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server failed")?;
    Ok(())
}

fn print_usage() {
    println!(
        "aisearch-emulator: local HTTP-compatible emulator for Azure AI Search\n\n\
         USAGE:\n  \
aisearch-emulator              Start the HTTP service\n  \
aisearch-emulator healthcheck  Check GET /health against EMULATOR_PORT (for Docker HEALTHCHECK)\n\n\
         CONFIGURATION (environment variables):\n  \
EMULATOR_PORT            Listen port (default 8080)\n  \
EMULATOR_STORAGE__MODE   memory | file (default memory; file fails fast)\n  \
EMULATOR_API_VERSIONS    Comma-separated API versions (default 2024-07-01)\n  \
EMULATOR_LOG_LEVEL       Log level (default info)\n  \
EMULATOR_ENABLE_ADMIN    Enable POST /admin/reset (default true)"
    );
}

fn init_logging(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .init();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => tracing::info!("received SIGINT, shutting down"),
        () = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}

/// Performs `GET /health` against `EMULATOR_PORT` using a plain TCP socket so
/// the static binary needs no HTTP client dependency. Exits non-zero on any
/// failure (used as the Docker `HEALTHCHECK`).
fn run_healthcheck() -> anyhow::Result<()> {
    let port = std::env::var("EMULATOR_PORT")
        .ok()
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    let addr = format!("127.0.0.1:{port}");

    let mut stream = TcpStream::connect(&addr)
        .with_context(|| format!("healthcheck: cannot connect to {addr}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let request = format!("GET /health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .context("healthcheck: write failed")?;

    let mut buffer = Vec::new();
    stream
        .read_to_end(&mut buffer)
        .context("healthcheck: read failed")?;
    let text = String::from_utf8_lossy(&buffer);
    let head = text.split("\r\n\r\n").next().unwrap_or("");
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
    let status_code = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");

    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    if status_code == "200" && parsed.get("status").and_then(Value::as_str) == Some("ok") {
        println!("healthy");
        Ok(())
    } else {
        bail!("healthcheck failed: status {status_code}, body {body}");
    }
}
