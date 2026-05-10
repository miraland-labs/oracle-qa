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
use solana_client::nonblocking::rpc_client::RpcClient;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
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
    info!("║      x402 SLA-Escrow ecosystem                      ║");
    info!("╚══════════════════════════════════════════════════════╝");
    info!("Oracle pubkey:       {}", config.oracle_pubkey());
    info!("Program ID:          {}", config.escrow_program_id);
    info!("RPC:                 {}", config.solana_rpc_url);
    info!("WebSocket:           {}", config.solana_ws_url);
    info!("Bind address:        {}", config.bind_addr);
    info!("Evidence URLs:       {:?}", config.evidence_registry_urls);
    info!("Strict profile:      {}", config.strict_profile);
    info!("Strict event match:  {}", config.require_event_match);
    info!(
        "Backfill lookback:   {} signatures",
        config.backfill_lookback_signatures
    );

    let db = match db::OracleDb::from_url(config.database_url.as_deref()) {
        None => {
            warn!("DATABASE_URL unset; oracle ledger is disabled (restart-safe dedupe off)");
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

    let rpc = Arc::new(RpcClient::new(config.solana_rpc_url.clone()));
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;

    let state = Arc::new(AppState {
        config: config.clone(),
        stats: RwLock::new(OracleStats::default()),
        health: runtime_health.clone(),
        manual_evaluate_requests: RwLock::new(std::collections::VecDeque::new()),
        db: db.clone(),
        started_at: Instant::now(),
        http,
        rpc: rpc.clone(),
    });

    // Job channel: chain monitor → evaluation worker.
    let (job_tx, mut job_rx) = mpsc::channel::<types::EvaluationJob>(config.job_channel_capacity);

    // Startup backfill: catch up on deliveries the worker would otherwise miss if the
    // process was offline when the log notifications arrived. Runs once and then exits.
    {
        let cfg = Arc::new(config.clone());
        let rpc_b = rpc.clone();
        let tx_b = job_tx.clone();
        let db_b = db.clone();
        let health_b = runtime_health.clone();
        tokio::spawn(async move {
            chain::backfill_missed_deliveries(cfg, rpc_b, tx_b, db_b, health_b).await;
        });
    }

    // Live chain monitor (WebSocket log subscription).
    {
        let cfg = Arc::new(config.clone());
        let rpc_m = rpc.clone();
        let tx_m = job_tx.clone();
        let health_m = runtime_health.clone();
        tokio::spawn(async move {
            chain::monitor_deliveries(cfg, rpc_m, tx_m, health_m).await;
        });
    }

    // When the ledger is disabled we fall back to in-memory dedupe. With Postgres the
    // ledger itself is the source of truth and survives restarts.
    let processed_mem: Arc<Mutex<HashSet<[u8; 32]>>> = Arc::new(Mutex::new(HashSet::new()));
    let attempts_mem: Arc<Mutex<HashMap<[u8; 32], u32>>> = Arc::new(Mutex::new(HashMap::new()));

    let worker_state = state.clone();
    let processed_worker = processed_mem.clone();
    let attempts_worker = attempts_mem.clone();

    tokio::spawn(async move {
        info!("Evaluation worker started");
        while let Some(job) = job_rx.recv().await {
            let uid_hex = hex::encode(job.payment_uid);
            let uid = job.payment_uid;

            {
                let mut health = worker_state.health.write().await;
                health.queue_depth = job_rx.len();
            }

            // Dedupe: ledger first, then in-memory. The ledger check also survives
            // process restarts and cross-instance races (with a single authority the
            // second instance simply observes the settled row).
            if let Some(ledger) = &worker_state.db {
                match ledger.is_terminal(&uid).await {
                    Ok(true) => {
                        warn!(
                            "Skipping {}: ledger already marks this payment_uid as terminal",
                            uid_hex
                        );
                        continue;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!(error = %e, "ledger is_terminal probe failed; proceeding cautiously");
                    }
                }
            } else {
                // In-memory fallback (ledger disabled): same semantics as v0.1.
                let mut seen = processed_worker.lock().await;
                if !seen.insert(uid) {
                    warn!(
                        "Skipping duplicate in-memory job payment_uid={} (ledger disabled)",
                        uid_hex
                    );
                    continue;
                }
            }

            if let Some(db) = &worker_state.db {
                if let Err(e) = db.record_detected(&job).await {
                    warn!(error = %e, "ledger detected record failed");
                }
                if let Err(e) = db.record_queued(&job).await {
                    warn!(error = %e, "ledger queued record failed");
                }
            }

            info!("Processing job: payment={}", uid_hex);

            // Attempt count: prefer the ledger's post-increment state (so restarts are
            // accurate) and fall back to the in-memory map when Postgres is disabled.
            let attempt_count = if let Some(ledger) = &worker_state.db {
                match ledger.record_started(&job).await {
                    Ok(()) => match ledger.attempt_count(&uid).await {
                        Ok(n) if n > 0 => n as u32,
                        _ => 1,
                    },
                    Err(e) => {
                        warn!(error = %e, "ledger started record failed");
                        1
                    }
                }
            } else {
                let mut attempts = attempts_worker.lock().await;
                let count = attempts.entry(uid).or_insert(0);
                *count += 1;
                *count
            };

            let timeout =
                tokio::time::Duration::from_millis(worker_state.config.evaluation_timeout_ms);

            match tokio::time::timeout(timeout, pipeline::run_pipeline(&worker_state, &job)).await {
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
                        // Persist the high-water slot so a restart's backfill can skip
                        // anything older than this completed job.
                        chain::persist_slot_watermark(db, &worker_state.health).await;
                    }
                    let mut stats = worker_state.stats.write().await;
                    stats.total_evaluated += 1;
                    if outcome.result.approved {
                        stats.total_approved += 1;
                    } else {
                        stats.total_rejected += 1;
                    }
                    stats.last_evaluation_at = Some(chrono::Utc::now().to_rfc3339());
                    // In-memory attempts map grows unboundedly otherwise.
                    attempts_worker.lock().await.remove(&uid);
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
                        // Release the in-memory dedupe set so a retriggered event can re-run.
                        // With ledger enabled, `is_terminal` is still false so retries proceed.
                        processed_worker.lock().await.remove(&uid);
                    }
                    let mut stats = worker_state.stats.write().await;
                    stats.total_errors += 1;
                    if should_dead_letter {
                        stats.total_dead_letter += 1;
                        attempts_worker.lock().await.remove(&uid);
                    }
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
                    if should_dead_letter {
                        stats.total_dead_letter += 1;
                        attempts_worker.lock().await.remove(&uid);
                    }
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
