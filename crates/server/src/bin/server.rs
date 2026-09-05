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
            return ExitCode::FAILURE;
        }
    };

    let std_listener = match std::net::TcpListener::bind(config.bind_addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bind error on {}: {e}", config.bind_addr);
            return ExitCode::FAILURE;
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to build tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    info!(addr = %config.bind_addr, "drawio-agent-server starting");
    // Everything that needs the tokio runtime (listener conversion,
    // chromium launch via build_app_state, the serve loop) runs inside
    // block_on — from_std on a std listener outside a runtime panics.
    let result = runtime.block_on(async move {
        std_listener
            .set_nonblocking(true)
            .map_err(|e| format!("set_nonblocking: {e}"))?;
        let listener = tokio::net::TcpListener::from_std(std_listener)
            .map_err(|e| format!("from_std: {e}"))?;
        let state = build_app_state(&config)
            .await
            .map_err(|e| format!("build_app_state: {e}"))?;
        run_server(
            listener,
            state,
            config.static_dir.clone().unwrap_or_default(),
            shutdown_signal(),
        )
        .await
        .map_err(|e| e.to_string())
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("server error: {e}");
            ExitCode::FAILURE
        }
    }
}
