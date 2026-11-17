//! `payment-matching-api [serve|migrate]`
//!
//! - `serve` (the default) runs the HTTP server.
//! - `migrate` applies the migrations embedded in the binary and exits. The server never migrates on its own:
//!   changing the schema is a deliberate step.

use std::error::Error;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use payment_matching_api::config::Config;
use payment_matching_api::http::{AppState, router};
use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .json()
        // The request span carries the id, the method and the path; one copy per line is enough.
        .with_span_list(false)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "stopped with an error");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let command = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "serve".to_owned());
    if command != "serve" && command != "migrate" {
        return Err(format!("unknown command {command:?}; expected `serve` or `migrate`").into());
    }

    let config = Config::from_env()?;
    let pool = PgPoolOptions::new()
        .max_connections(config.db_max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&config.database_url)
        .await?;

    if command == "migrate" {
        sqlx::migrate!().run(&pool).await?;
        tracing::info!("migrations applied");
        return Ok(());
    }

    let listener = TcpListener::bind(config.bind_addr).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    let app = router(AppState {
        pool: pool.clone(),
        config: Arc::new(config),
    });
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Requests in flight have finished; give the connections back before exiting.
    pool.close().await;
    tracing::info!("stopped");
    Ok(())
}

/// Resolves on Ctrl-C or, on Unix, SIGTERM (what `docker stop` and orchestrators send).
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!("shutdown signal received");
}
