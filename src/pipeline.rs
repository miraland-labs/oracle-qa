use crate::{
    config::OracleConfig,
    error::OracleError,
    evaluator::Evaluator,
    settler,
    types::{DeliveryEvidence, EvaluationJob, EvaluationResult, SlaDocument},
};
use reqwest::header::{HeaderMap, AUTHORIZATION};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tracing::{error, info, warn};

/// Fetch raw bytes from registry mirrors with retry/backoff; verify SHA256 before parsing JSON.
async fn fetch_evidence<T: serde::de::DeserializeOwned>(
    config: &OracleConfig,
    hash: &[u8; 32],
    parse_error: fn(String) -> OracleError,
) -> Result<T, OracleError> {
    let hash_hex = hex::encode(hash);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| OracleError::EvidenceNotFound(e.to_string()))?;

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

    Err(OracleError::EvidenceNotFound(format!(
        "{} (tried {} base URL(s), up to {} retries each): {}",
        hash_hex,
        config.evidence_registry_urls.len(),
        config.evidence_fetch_max_retries,
        last_err
    )))
}

/// Execute the full evaluation pipeline for a single job:
/// 1. Check eligibility on-chain
/// 2. Fetch SLA document and delivery evidence
/// 3. Evaluate compliance
/// 4. Submit ConfirmOracle transaction
pub async fn run_pipeline(
    config: &OracleConfig,
    job: &EvaluationJob,
) -> Result<(EvaluationResult, Option<String>), OracleError> {
    let uid_hex = hex::encode(job.payment_uid);
    info!("Pipeline started for payment {}", uid_hex);

    // Step 1: Verify the payment is still eligible
    if !settler::is_eligible(config, job).await? {
        return Err(OracleError::Evaluation(format!(
            "Payment {} is no longer eligible for oracle confirmation",
            uid_hex
        )));
    }

    // Step 2: Fetch SLA document and delivery evidence
    let sla: SlaDocument = fetch_evidence(config, &job.sla_hash, OracleError::SlaParse)
        .await
        .map_err(|e| {
            warn!("Failed to fetch SLA for {}: {}", uid_hex, e);
            e
        })?;

    let evidence: DeliveryEvidence =
        fetch_evidence(config, &job.delivery_hash, OracleError::DeliveryParse)
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
    let result = Evaluator::evaluate(&sla, &evidence)?;

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
    let sig = match settler::settle(config, job, result.approved, result.resolution_reason).await {
        Ok(sig) => {
            info!("Settlement confirmed: sig={}", sig);
            Some(sig)
        }
        Err(e) => {
            error!("Settlement failed for {}: {}", uid_hex, e);
            return Err(e);
        }
    };

    Ok((result, sig))
}
