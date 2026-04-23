mod chain;
mod config;
mod error;
mod evaluator;
mod pipeline;
mod server;
mod settler;
mod types;

use config::OracleConfig;
use server::{AppState, OracleStats};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "oracle_qa=info,tower_http=info".into()),
        )
        .init();

    let config = OracleConfig::from_env()?;

    info!("╔══════════════════════════════════════════════════════╗");
    info!("║      oracle-qa — API Response Quality Oracle        ║");
    info!("║      x402 SLA-Escrow Ecosystem                      ║");
    info!("╚══════════════════════════════════════════════════════╝");
    info!("Oracle pubkey:  {}", config.oracle_pubkey());
    info!("Program ID:     {}", config.escrow_program_id);
    info!("RPC:            {}", config.solana_rpc_url);
    info!("WebSocket:      {}", config.solana_ws_url);
    info!("Bind address:   {}", config.bind_addr);
    info!("Evidence URL:   {}", config.evidence_registry_url);

    let state = Arc::new(AppState {
        config: config.clone(),
        stats: RwLock::new(OracleStats::default()),
        started_at: Instant::now(),
    });

    // Job channel: chain monitor -> evaluation worker
    let (job_tx, mut job_rx) = mpsc::channel::<types::EvaluationJob>(64);

    // Spawn the chain monitor (WebSocket log subscription)
    let monitor_config = Arc::new(config.clone());
    tokio::spawn(async move {
        chain::monitor_deliveries(monitor_config, job_tx).await;
    });

    // Spawn the evaluation worker
    let worker_state = state.clone();
    tokio::spawn(async move {
        info!("Evaluation worker started");
        while let Some(job) = job_rx.recv().await {
            let uid_hex = hex::encode(job.payment_uid);
            info!("Processing job: payment={}", uid_hex);

            let timeout =
                tokio::time::Duration::from_millis(worker_state.config.evaluation_timeout_ms);

            match tokio::time::timeout(timeout, pipeline::run_pipeline(&worker_state.config, &job))
                .await
            {
                Ok(Ok((result, _sig))) => {
                    let mut stats = worker_state.stats.write().await;
                    stats.total_evaluated += 1;
                    if result.approved {
                        stats.total_approved += 1;
                    } else {
                        stats.total_rejected += 1;
                    }
                    stats.last_evaluation_at = Some(chrono::Utc::now().to_rfc3339());
                }
                Ok(Err(e)) => {
                    error!("Pipeline error for {}: {}", uid_hex, e);
                    let mut stats = worker_state.stats.write().await;
                    stats.total_errors += 1;
                }
                Err(_) => {
                    warn!(
                        "Pipeline timeout for {} ({}ms)",
                        uid_hex, worker_state.config.evaluation_timeout_ms
                    );
                    let mut stats = worker_state.stats.write().await;
                    stats.total_errors += 1;
                }
            }
        }
    });

    // Start the HTTP server
    let app = server::create_router(state);
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    info!("HTTP server listening on {}", config.bind_addr);

    axum::serve(listener, app).await?;

    Ok(())
}
