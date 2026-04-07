use crate::{
    config::OracleConfig,
    error::OracleError,
    evaluator::Evaluator,
    settler,
    types::{DeliveryEvidence, EvaluationJob, EvaluationResult, SlaDocument},
};
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};

/// Fetch an off-chain document by its SHA256 hash from the evidence registry.
async fn fetch_evidence<T: serde::de::DeserializeOwned>(
    registry_url: &str,
    hash: &[u8; 32],
    parse_error: fn(String) -> OracleError,
) -> Result<T, OracleError> {
    let hash_hex = hex::encode(hash);
    let url = format!("{}/{}", registry_url, hash_hex);

    let response = reqwest::get(url).await?;
    if !response.status().is_success() {
        return Err(OracleError::EvidenceNotFound(format!(
            "Registry returned {} for hash {}",
            response.status(),
            hash_hex
        )));
    }

    let body: serde_json::Value = response.json().await?;

    let canonical = serde_json::to_string(&body).unwrap_or_default();
    let computed = Sha256::digest(canonical.as_bytes());
    if computed.as_slice() != hash {
        return Err(OracleError::EvidenceNotFound(format!(
            "Hash mismatch: expected {}, got {}",
            hash_hex,
            hex::encode(computed)
        )));
    }

    serde_json::from_value(body).map_err(|e| parse_error(format!("Failed to parse: {}", e)))
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
    let sla: SlaDocument = fetch_evidence(
        &config.evidence_registry_url,
        &job.sla_hash,
        OracleError::SlaParse,
    )
    .await
    .map_err(|e| {
        warn!("Failed to fetch SLA for {}: {}", uid_hex, e);
        e
    })?;

    let evidence: DeliveryEvidence = fetch_evidence(
        &config.evidence_registry_url,
        &job.delivery_hash,
        OracleError::DeliveryParse,
    )
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
