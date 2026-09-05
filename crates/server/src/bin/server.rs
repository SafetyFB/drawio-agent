//! Runnable server binary: load config from env, build AppState, bind,
//! serve with graceful shutdown.

use std::process::ExitCode;

use drawio_agent_server::{build_app_state, run_server, shutdown_signal, ServerConfig};
use tracing::info;

fn main() -> ExitCode {
    let config = match ServerConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return ExitCode::from(1);
        }
    };

    let std_listener = match std::net::TcpListener::bind(config.bind_addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bind error on {}: {e}", config.bind_addr);
            return ExitCode::from(1);
        }
    };
    if let Err(e) = std_listener.set_nonblocking(true) {
        eprintln!("set_nonblocking failed: {e}");
        return ExitCode::from(1);
    }
    let listener = match tokio::net::TcpListener::from_std(std_listener) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("tokio listener conversion failed: {e}");
            return ExitCode::from(1);
        }
    };

    info!(addr = %config.bind_addr, "drawio-agent-server starting");
    let state = build_app_state(&config);
    let static_dir = config.static_dir.clone();

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("tokio runtime init failed: {e}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = runtime.block_on(async move {
        run_server(listener, state, static_dir, shutdown_signal()).await
    }) {
        eprintln!("server error: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
