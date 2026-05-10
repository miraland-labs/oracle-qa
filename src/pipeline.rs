use crate::{
    error::OracleError,
    evaluator::{Evaluator, QualityOracle},
    server::AppState,
    settler,
    types::{DeliveryEvidence, EvaluationJob, EvaluationResult, SlaDocument},
};
use reqwest::header::{HeaderMap, AUTHORIZATION};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};
use tracing::{error, info, warn};

pub struct PipelineOutcome {
    pub result: EvaluationResult,
    pub signature: Option<String>,
    pub resolution_hash: [u8; 32],
}

/// Fetch raw bytes from registry mirrors with retry/backoff; verify SHA256 before parsing JSON.
async fn fetch_evidence<T: serde::de::DeserializeOwned>(
    state: &AppState,
    hash: &[u8; 32],
    parse_error: fn(String) -> OracleError,
) -> Result<T, OracleError> {
    let hash_hex = hex::encode(hash);
    let client = &state.http;
    let config = &state.config;

    let mut headers = HeaderMap::new();
    if let Some(auth) = &config.evidence_registry_auth_header {
        let h = AUTHORIZATION;
        headers.insert(
            h,
            auth.parse()
                .map_err(|e: reqwest::header::InvalidHeaderValue| {
                    OracleError::EvidenceNotFound(format!("invalid AUTH header: {}", e))
                })?,
        );
    }

    let mut last_err = String::new();
    for base in &config.evidence_registry_urls {
        let url = format!("{}/{}", base.trim_end_matches('/'), hash_hex);
        for attempt in 0..config.evidence_fetch_max_retries {
            let req = client.get(&url).headers(headers.clone());
            match req.send().await {
                Ok(response) => {
                    if !response.status().is_success() {
                        last_err = format!("Registry returned {} for {}", response.status(), url);
                        if response.status().is_server_error()
                            && attempt + 1 < config.evidence_fetch_max_retries
                        {
                            tokio::time::sleep(Duration::from_millis(
                                config.evidence_fetch_retry_base_ms * (1 << attempt),
                            ))
                            .await;
                            continue;
                        }
                        break;
                    }
                    match response.bytes().await {
                        Ok(raw) => {
                            let computed = Sha256::digest(&raw);
                            if computed.as_slice() != hash {
                                return Err(OracleError::EvidenceNotFound(format!(
                                    "Hash mismatch for {}: document bytes do not match on-chain hash (got {})",
                                    hash_hex,
                                    hex::encode(computed)
                                )));
                            }
                            return serde_json::from_slice(&raw)
                                .map_err(|e| parse_error(format!("Failed to parse JSON: {}", e)));
                        }
                        Err(e) => {
                            last_err = format!("read body: {}", e);
                        }
                    }
                }
                Err(e) => {
                    last_err = e.to_string();
                }
            }
            if attempt + 1 < config.evidence_fetch_max_retries {
                tokio::time::sleep(Duration::from_millis(
                    config.evidence_fetch_retry_base_ms * (1 << attempt),
                ))
                .await;
            }
        }
    }

    // Surface the miss in /metrics so operators can alert on sustained registry outages.
    {
        let mut stats = state.stats.write().await;
        stats.total_evidence_fetch_failures = stats.total_evidence_fetch_failures.saturating_add(1);
    }

    Err(OracleError::EvidenceNotFound(format!(
        "{} (tried {} base URL(s), up to {} retries each): {}",
        hash_hex,
        config.evidence_registry_urls.len(),
        config.evidence_fetch_max_retries,
        last_err
    )))
}

/// Execute the full evaluation pipeline for a single job:
/// 1. Check eligibility on-chain (using the shared RPC client and on-chain clock).
/// 2. Fetch SLA document and delivery evidence from the evidence registry (raw-bytes hash-verified).
/// 3. Evaluate compliance via the configured [`QualityOracle`].
/// 4. Submit the `ConfirmOracle` transaction.
pub async fn run_pipeline(
    state: &Arc<AppState>,
    job: &EvaluationJob,
) -> Result<PipelineOutcome, OracleError> {
    let uid_hex = hex::encode(job.payment_uid);
    info!("Pipeline started for payment {}", uid_hex);

    // Step 1: Verify the payment is still eligible
    if !settler::is_eligible(state, job).await? {
        return Err(OracleError::Evaluation(format!(
            "Payment {} is no longer eligible for oracle confirmation",
            uid_hex
        )));
    }

    // Step 2: Fetch SLA document and delivery evidence
    let sla: SlaDocument = fetch_evidence(state, &job.sla_hash, OracleError::SlaParse)
        .await
        .map_err(|e| {
            warn!("Failed to fetch SLA for {}: {}", uid_hex, e);
            e
        })?;

    let evidence: DeliveryEvidence =
        fetch_evidence(state, &job.delivery_hash, OracleError::DeliveryParse)
            .await
            .map_err(|e| {
                warn!("Failed to fetch delivery evidence for {}: {}", uid_hex, e);
                e
            })?;

    info!(
        "Evaluating payment {}: endpoint={}, latency={}ms, status={}",
        uid_hex, sla.endpoint, evidence.latency_ms, evidence.status_code
    );

    // Step 3: Evaluate
    let oracle_impl = Evaluator::new(state.config.strict_profile);
    let result = oracle_impl.evaluate(&sla, &evidence)?;
    let resolution_hash = settler::compute_resolution_hash(job, &sla, &result)?;

    let verdict = if result.approved {
        "APPROVED"
    } else {
        "REJECTED"
    };
    info!(
        "Evaluation {}: payment={} ({} checks, {} passed)",
        verdict,
        uid_hex,
        result.checks.len(),
        result.checks.iter().filter(|c| c.passed).count()
    );
    for check in &result.checks {
        let icon = if check.passed { "+" } else { "-" };
        info!("  [{}] {}: {}", icon, check.name, check.detail);
    }

    // Step 4: Settle on-chain
    let sig = match settler::settle(
        state,
        job,
        result.approved,
        result.resolution_reason,
        resolution_hash,
    )
    .await
    {
        Ok(sig) => {
            info!("Settlement confirmed: sig={}", sig);
            Some(sig)
        }
        Err(e) => {
            error!("Settlement failed for {}: {}", uid_hex, e);
            return Err(e);
        }
    };

    Ok(PipelineOutcome {
        result,
        signature: sig,
        resolution_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::OracleConfig,
        server::{AppState, OracleStats},
        types::{RuntimeHealth, API_QUALITY_V1_PROFILE_ID},
    };
    use axum::{routing::get, Router};
    use solana_client::nonblocking::rpc_client::RpcClient;
    use solana_sdk::{pubkey::Pubkey, signature::Keypair};
    use std::{collections::VecDeque, sync::Arc, time::Instant};
    use tokio::sync::RwLock;

    fn test_config(base_url: String) -> OracleConfig {
        OracleConfig {
            solana_rpc_url: "http://127.0.0.1:8899".into(),
            solana_ws_url: "ws://127.0.0.1:8900".into(),
            oracle_keypair: Arc::new(Keypair::new()),
            escrow_program_id: Pubkey::new_unique(),
            bind_addr: "127.0.0.1:0".into(),
            evaluation_timeout_ms: 30_000,
            evidence_registry_urls: vec![base_url],
            evidence_registry_auth_header: None,
            evidence_fetch_max_retries: 1,
            evidence_fetch_retry_base_ms: 1,
            database_url: None,
            operator_token_sha256: None,
            allow_unauthenticated_manual_evaluate: false,
            cors_allowed_origins: vec![],
            manual_evaluate_rate_limit: 30,
            manual_evaluate_rate_window_ms: 60_000,
            strict_profile: true,
            dead_letter_max_attempts: 5,
            job_channel_capacity: 16,
            require_event_match: false,
            backfill_lookback_signatures: 0,
        }
    }

    fn test_state(base_url: String) -> Arc<AppState> {
        let config = test_config(base_url);
        let rpc = Arc::new(RpcClient::new(config.solana_rpc_url.clone()));
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap();
        Arc::new(AppState {
            config,
            stats: RwLock::new(OracleStats::default()),
            health: Arc::new(RwLock::new(RuntimeHealth::default())),
            manual_evaluate_requests: RwLock::new(VecDeque::new()),
            db: None,
            started_at: Instant::now(),
            http,
            rpc,
        })
    }

    async fn spawn_registry(body: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = Arc::new(body);
        let app = Router::new().route(
            "/{hash}",
            get(move || {
                let body = body.clone();
                async move { body.as_str().to_owned() }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{}", addr)
    }

    #[tokio::test]
    async fn fetch_evidence_accepts_hash_bound_document() {
        let body = serde_json::json!({
            "version": 1,
            "profile_id": API_QUALITY_V1_PROFILE_ID,
            "endpoint": "https://api.example.test",
            "method": "GET"
        })
        .to_string();
        let digest = Sha256::digest(body.as_bytes());
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&digest);
        let state = test_state(spawn_registry(body).await);

        let sla: SlaDocument = fetch_evidence(&state, &hash, OracleError::SlaParse)
            .await
            .unwrap();

        assert_eq!(sla.version, 1);
        assert_eq!(sla.profile_id.as_deref(), Some(API_QUALITY_V1_PROFILE_ID));
    }

    #[tokio::test]
    async fn fetch_evidence_rejects_hash_mismatch() {
        let state = test_state(spawn_registry("{\"version\":1}".into()).await);
        let err = fetch_evidence::<SlaDocument>(&state, &[9u8; 32], OracleError::SlaParse)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("Hash mismatch"));
    }
}
