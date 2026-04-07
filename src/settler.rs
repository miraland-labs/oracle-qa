use crate::{config::OracleConfig, error::OracleError, types::EvaluationJob};
use sla_escrow_api::sdk::EscrowSdk;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, transaction::Transaction};
use tracing::{info, warn};

/// Build, sign, and send a ConfirmOracle transaction.
pub async fn settle(
    config: &OracleConfig,
    job: &EvaluationJob,
    approved: bool,
    resolution_reason: u16,
) -> Result<String, OracleError> {
    let rpc = RpcClient::new_with_commitment(
        config.solana_rpc_url.clone(),
        CommitmentConfig::confirmed(),
    );

    let resolution_state: u8 = if approved { 1 } else { 2 };

    let payment_uid_hex = hex::encode(job.payment_uid);

    let ix = EscrowSdk::confirm_oracle(
        config.oracle_pubkey(),
        job.mint,
        &payment_uid_hex,
        job.delivery_hash,
        resolution_state,
        resolution_reason,
    );

    let recent_blockhash = rpc
        .get_latest_blockhash()
        .await
        .map_err(|e| OracleError::Settlement(format!("Failed to get blockhash: {}", e)))?;

    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&config.oracle_pubkey()),
        &[config.oracle_keypair.as_ref()],
        recent_blockhash,
    );

    let sig = rpc
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

/// Check if a payment is still eligible for oracle confirmation.
/// Returns false if already resolved, expired, or not assigned to this oracle.
pub async fn is_eligible(config: &OracleConfig, job: &EvaluationJob) -> Result<bool, OracleError> {
    let rpc = RpcClient::new_with_commitment(
        config.solana_rpc_url.clone(),
        CommitmentConfig::confirmed(),
    );

    let account = rpc
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

    if payment.oracle_authority != config.oracle_pubkey() {
        return Ok(false);
    }

    // Already resolved
    if payment.resolution_state != 0 {
        return Ok(false);
    }

    // Check expiry (use current time as rough estimate; on-chain clock may differ slightly)
    let now = chrono::Utc::now().timestamp();
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
