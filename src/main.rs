mod chain;
mod config;
mod db;
mod error;
mod evaluator;
mod pipeline;
mod server;
mod settler;
mod types;

use config::OracleConfig;
use server::{AppState, OracleStats};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, Mutex, RwLock};
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
    info!("Evidence URLs:  {:?}", config.evidence_registry_urls);
    info!("Strict profile: {}", config.strict_profile);

    let db = match db::OracleDb::from_url(config.database_url.as_deref()) {
        None => {
            warn!("DATABASE_URL unset; oracle ledger is disabled");
            None
        }
        Some(Ok(db)) => {
            info!("PostgreSQL oracle ledger enabled");
            Some(db)
        }
        Some(Err(e)) => {
            warn!(error = %e, "DATABASE_URL is set but oracle ledger pool could not be created");
            None
        }
    };

    let runtime_health = Arc::new(RwLock::new(types::RuntimeHealth::default()));
    let state = Arc::new(AppState {
        config: config.clone(),
        stats: RwLock::new(OracleStats::default()),
        health: runtime_health.clone(),
        manual_evaluate_requests: RwLock::new(std::collections::VecDeque::new()),
        db,
        started_at: Instant::now(),
    });

    // Job channel: chain monitor -> evaluation worker
    let (job_tx, mut job_rx) = mpsc::channel::<types::EvaluationJob>(config.job_channel_capacity);

    // Spawn the chain monitor (WebSocket log subscription)
    let monitor_config = Arc::new(config.clone());
    let monitor_health = runtime_health;
    tokio::spawn(async move {
        chain::monitor_deliveries(monitor_config, job_tx, monitor_health).await;
    });

    let processed: Arc<Mutex<HashSet<[u8; 32]>>> = Arc::new(Mutex::new(HashSet::new()));
    let attempts: Arc<Mutex<HashMap<[u8; 32], u32>>> = Arc::new(Mutex::new(HashMap::new()));

    // Spawn the evaluation worker
    let worker_state = state.clone();
    let processed_worker = processed.clone();
    let attempts_worker = attempts.clone();
    tokio::spawn(async move {
        info!("Evaluation worker started");
        while let Some(job) = job_rx.recv().await {
            let uid_hex = hex::encode(job.payment_uid);
            let uid = job.payment_uid;
            {
                let mut health = worker_state.health.write().await;
                health.queue_depth = job_rx.len();
            }

            if let Some(db) = &worker_state.db {
                if let Err(e) = db.record_detected(&job).await {
                    warn!(error = %e, "ledger detected record failed");
                }
                if let Err(e) = db.record_queued(&job).await {
                    warn!(error = %e, "ledger queued record failed");
                }
            }

            {
                let mut seen = processed_worker.lock().await;
                if !seen.insert(uid) {
                    warn!("Skipping duplicate job payment_uid={}", uid_hex);
                    continue;
                }
            }

            info!("Processing job: payment={}", uid_hex);
            let attempt_count = {
                let mut attempts = attempts_worker.lock().await;
                let count = attempts.entry(uid).or_insert(0);
                *count += 1;
                *count
            };
            if let Some(db) = &worker_state.db {
                if let Err(e) = db.record_started(&job).await {
                    warn!(error = %e, "ledger started record failed");
                }
            }

            let timeout =
                tokio::time::Duration::from_millis(worker_state.config.evaluation_timeout_ms);

            match tokio::time::timeout(timeout, pipeline::run_pipeline(&worker_state.config, &job))
                .await
            {
                Ok(Ok(outcome)) => {
                    if let Some(ref s) = outcome.signature {
                        info!("Settlement signature for {}: {}", uid_hex, s);
                    }
                    if let Some(db) = &worker_state.db {
                        if let Err(e) = db
                            .record_settled(
                                &job,
                                &outcome.result,
                                outcome.signature.as_deref(),
                                &outcome.resolution_hash,
                            )
                            .await
                        {
                            warn!(error = %e, "ledger settled record failed");
                        }
                    }
                    let mut stats = worker_state.stats.write().await;
                    stats.total_evaluated += 1;
                    if outcome.result.approved {
                        stats.total_approved += 1;
                    } else {
                        stats.total_rejected += 1;
                    }
                    stats.last_evaluation_at = Some(chrono::Utc::now().to_rfc3339());
                }
                Ok(Err(e)) => {
                    error!("Pipeline error for {}: {}", uid_hex, e);
                    let should_dead_letter =
                        attempt_count >= worker_state.config.dead_letter_max_attempts;
                    if let Some(db) = &worker_state.db {
                        let record = if should_dead_letter {
                            db.record_dead_letter(&job, &e.to_string()).await
                        } else {
                            db.record_failed(&job, &e.to_string()).await
                        };
                        if let Err(db_err) = record {
                            warn!(error = %db_err, "ledger failure record failed");
                        }
                    }
                    if !should_dead_letter {
                        processed_worker.lock().await.remove(&uid);
                    }
                    let mut stats = worker_state.stats.write().await;
                    stats.total_errors += 1;
                }
                Err(_) => {
                    warn!(
                        "Pipeline timeout for {} ({}ms)",
                        uid_hex, worker_state.config.evaluation_timeout_ms
                    );
                    let should_dead_letter =
                        attempt_count >= worker_state.config.dead_letter_max_attempts;
                    if let Some(db) = &worker_state.db {
                        let record = if should_dead_letter {
                            db.record_dead_letter(&job, "pipeline timeout").await
                        } else {
                            db.record_failed(&job, "pipeline timeout").await
                        };
                        if let Err(e) = record {
                            warn!(error = %e, "ledger timeout record failed");
                        }
                    }
                    if !should_dead_letter {
                        processed_worker.lock().await.remove(&uid);
                    }
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
