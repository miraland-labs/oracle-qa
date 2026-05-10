use crate::{
    error::OracleError,
    server::AppState,
    types::{EvaluationJob, EvaluationResult, SlaDocument},
};
use sha2::{Digest, Sha256};
use sla_escrow_api::sdk::EscrowSdk;
use solana_sdk::{
    commitment_config::CommitmentConfig, sysvar::clock::Clock, transaction::Transaction,
};
use std::sync::Arc;
use tracing::{info, warn};

/// Build, sign, and send a `ConfirmOracle` transaction using the shared RPC client.
pub async fn settle(
    state: &Arc<AppState>,
    job: &EvaluationJob,
    approved: bool,
    resolution_reason: u16,
    resolution_hash: [u8; 32],
) -> Result<String, OracleError> {
    let resolution_state: u8 = if approved { 1 } else { 2 };

    let payment_uid_hex = hex::encode(job.payment_uid);

    let ix = EscrowSdk::confirm_oracle(
        state.config.oracle_pubkey(),
        job.mint,
        &payment_uid_hex,
        job.delivery_hash,
        resolution_hash,
        resolution_state,
        resolution_reason,
    );

    let recent_blockhash = state
        .rpc
        .get_latest_blockhash()
        .await
        .map_err(|e| OracleError::Settlement(format!("Failed to get blockhash: {}", e)))?;

    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&state.config.oracle_pubkey()),
        &[state.config.oracle_keypair.as_ref()],
        recent_blockhash,
    );

    let sig = state
        .rpc
        .send_and_confirm_transaction(&tx)
        .await
        .map_err(|e| OracleError::Settlement(format!("Transaction failed: {}", e)))?;

    let verdict = if approved { "APPROVED" } else { "REJECTED" };
    info!(
        "Settlement {} for payment {}: sig={}",
        verdict, payment_uid_hex, sig
    );

    Ok(sig.to_string())
}

/// Deterministic audit fingerprint committed into `ConfirmOracle.resolution_hash`.
pub fn compute_resolution_hash(
    job: &EvaluationJob,
    sla: &SlaDocument,
    result: &EvaluationResult,
) -> Result<[u8; 32], OracleError> {
    let payload = serde_json::json!({
        "profile": "x402/oracle-qa/resolution/v1",
        "paymentUid": hex::encode(job.payment_uid),
        "paymentPubkey": job.payment_pubkey.to_string(),
        "slaHash": hex::encode(job.sla_hash),
        "deliveryHash": hex::encode(job.delivery_hash),
        "slaVersion": sla.version,
        "slaProfileId": sla.profile_id,
        "approved": result.approved,
        "resolutionReason": result.resolution_reason,
        "checks": result.checks,
    });
    let bytes = serde_json::to_vec(&payload)
        .map_err(|e| OracleError::Settlement(format!("resolution hash encode: {}", e)))?;
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

/// Read the on-chain Clock sysvar; fall back to the wall clock on RPC error so the
/// oracle still makes progress on transient infra hiccups (operators see the degraded
/// state via `/health.chain_connected`).
async fn chain_unix_timestamp(state: &AppState) -> i64 {
    match state
        .rpc
        .get_account_with_commitment(
            &solana_sdk::sysvar::clock::ID,
            CommitmentConfig::confirmed(),
        )
        .await
    {
        Ok(resp) => match resp.value {
            Some(account) => match bincode::deserialize::<Clock>(&account.data) {
                Ok(clock) => clock.unix_timestamp,
                Err(e) => {
                    warn!(
                        "Failed to decode on-chain Clock, falling back to wall clock: {}",
                        e
                    );
                    chrono::Utc::now().timestamp()
                }
            },
            None => {
                warn!("Clock sysvar account missing; falling back to wall clock");
                chrono::Utc::now().timestamp()
            }
        },
        Err(e) => {
            warn!(
                "RPC get_account for Clock sysvar failed, falling back to wall clock: {}",
                e
            );
            chrono::Utc::now().timestamp()
        }
    }
}

/// Check if a payment is still eligible for oracle confirmation.
/// Returns false if already resolved, expired (by on-chain clock), or not assigned to this oracle.
pub async fn is_eligible(state: &Arc<AppState>, job: &EvaluationJob) -> Result<bool, OracleError> {
    let account = state
        .rpc
        .get_account_with_commitment(&job.payment_pubkey, CommitmentConfig::confirmed())
        .await?
        .value;

    let Some(account) = account else {
        warn!("Payment account {} no longer exists", job.payment_pubkey);
        return Ok(false);
    };

    if account.data.len() < 8 + std::mem::size_of::<sla_escrow_api::state::Payment>() {
        return Ok(false);
    }

    let payment: &sla_escrow_api::state::Payment = bytemuck::from_bytes(
        &account.data[8..8 + std::mem::size_of::<sla_escrow_api::state::Payment>()],
    );

    if payment.oracle_authority != state.config.oracle_pubkey() {
        return Ok(false);
    }

    // Defense-in-depth against stale chain reads: the on-chain ConfirmOracle handler
    // itself enforces `delivery_timestamp != 0`, so refuse early rather than sending a
    // doomed tx that would waste the oracle's SOL on fees.
    if payment.delivery_timestamp == 0 {
        return Ok(false);
    }

    // Already resolved
    if payment.resolution_state != 0 {
        return Ok(false);
    }

    // Prefer on-chain Clock over the operator's wall clock; keeps eligibility consistent
    // with what the program will observe when the tx lands.
    let now = chain_unix_timestamp(state).await;
    if now > payment.expires_at {
        warn!(
            "Payment {} has expired (expires_at={}, now={})",
            hex::encode(payment.payment_uid),
            payment.expires_at,
            now
        );
        return Ok(false);
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CheckResult, EvaluationJob, EvaluationResult, SlaDocument};
    use solana_sdk::pubkey::Pubkey;

    fn job() -> EvaluationJob {
        EvaluationJob {
            payment_uid: [1u8; 32],
            payment_pubkey: Pubkey::new_unique(),
            sla_hash: [2u8; 32],
            delivery_hash: [3u8; 32],
            amount: 100,
            mint: Pubkey::new_unique(),
            oracle_authority: Pubkey::new_unique(),
            expires_at: 1_900_000_000,
        }
    }

    fn sla() -> SlaDocument {
        SlaDocument {
            version: 1,
            profile_id: Some(crate::types::API_QUALITY_V1_PROFILE_ID.into()),
            endpoint: "https://api.example.test".into(),
            method: "GET".into(),
            response_schema: None,
            required_fields: vec![],
            max_latency_ms: 5000,
            min_status_code: 200,
            max_status_code: 299,
            min_body_length: None,
        }
    }

    fn result(approved: bool) -> EvaluationResult {
        EvaluationResult {
            approved,
            resolution_reason: if approved { 0 } else { 1 },
            checks: vec![CheckResult {
                name: "status_code".into(),
                passed: approved,
                detail: "test".into(),
            }],
        }
    }

    #[test]
    fn resolution_hash_is_deterministic_and_outcome_bound() {
        let job = job();
        let sla = sla();

        let approved_a = compute_resolution_hash(&job, &sla, &result(true)).unwrap();
        let approved_b = compute_resolution_hash(&job, &sla, &result(true)).unwrap();
        let rejected = compute_resolution_hash(&job, &sla, &result(false)).unwrap();

        assert_eq!(approved_a, approved_b);
        assert_ne!(approved_a, [0u8; 32]);
        assert_ne!(approved_a, rejected);
    }
}
