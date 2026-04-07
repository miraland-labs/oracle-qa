use crate::{config::OracleConfig, error::OracleError, types::EvaluationJob};
use futures_util::StreamExt;
use sla_escrow_api::state::Payment;
use solana_client::nonblocking::pubsub_client::PubsubClient;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcTransactionLogsConfig, RpcTransactionLogsFilter};
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Reads a Payment PDA from chain and returns an EvaluationJob if it's relevant to this oracle.
pub async fn read_payment(
    rpc: &RpcClient,
    payment_pubkey: &Pubkey,
    oracle_pubkey: &Pubkey,
) -> Result<Option<EvaluationJob>, OracleError> {
    let account = rpc
        .get_account_with_commitment(payment_pubkey, CommitmentConfig::confirmed())
        .await?
        .value
        .ok_or_else(|| {
            OracleError::Chain(format!("Payment account {} not found", payment_pubkey))
        })?;

    // Skip 8-byte discriminator
    if account.data.len() < 8 + std::mem::size_of::<Payment>() {
        return Err(OracleError::Chain("Payment account data too short".into()));
    }
    let payment: &Payment =
        bytemuck::from_bytes(&account.data[8..8 + std::mem::size_of::<Payment>()]);

    // Only process payments assigned to this oracle
    if payment.oracle_authority != *oracle_pubkey {
        return Ok(None);
    }

    // Only process if delivery has been submitted and oracle hasn't resolved yet
    if payment.delivery_timestamp == 0 || payment.resolution_state != 0 {
        return Ok(None);
    }

    Ok(Some(EvaluationJob {
        payment_uid: payment.payment_uid,
        payment_pubkey: *payment_pubkey,
        sla_hash: payment.sla_hash,
        delivery_hash: payment.delivery_hash,
        amount: payment.amount,
        mint: payment.mint,
        oracle_authority: payment.oracle_authority,
        expires_at: payment.expires_at,
    }))
}

/// Subscribe to on-chain logs for the escrow program and emit EvaluationJobs
/// when a DeliverySubmittedEvent is detected.
pub async fn monitor_deliveries(config: Arc<OracleConfig>, tx: mpsc::Sender<EvaluationJob>) {
    loop {
        info!("Connecting to Solana WebSocket at {}", config.solana_ws_url);

        match PubsubClient::new(&config.solana_ws_url).await {
            Ok(pubsub) => {
                let filter =
                    RpcTransactionLogsFilter::Mentions(vec![config.escrow_program_id.to_string()]);
                let log_config = RpcTransactionLogsConfig {
                    commitment: Some(CommitmentConfig::confirmed()),
                };

                match pubsub.logs_subscribe(filter, log_config).await {
                    Ok((mut stream, _unsub)) => {
                        info!("Subscribed to escrow program logs");

                        while let Some(log_response) = stream.next().await {
                            let logs = &log_response.value.logs;

                            // Look for DeliverySubmittedEvent signature in logs
                            let has_delivery = logs.iter().any(|l| {
                                l.contains("DeliverySubmittedEvent") || l.contains("Program data:")
                            });
                            if !has_delivery {
                                continue;
                            }

                            // Extract the payment PDA from transaction accounts
                            // The log itself doesn't carry account keys directly,
                            // so we fetch the transaction to get account info.
                            if let Ok(sig) = log_response
                                .value
                                .signature
                                .parse::<solana_sdk::signature::Signature>()
                            {
                                let rpc = RpcClient::new(config.solana_rpc_url.clone());
                                match fetch_delivery_job(&rpc, &sig, &config).await {
                                    Ok(Some(job)) => {
                                        info!(
                                            "New delivery detected: payment_uid={}",
                                            hex::encode(job.payment_uid)
                                        );
                                        if tx.send(job).await.is_err() {
                                            error!("Job channel closed, stopping monitor");
                                            return;
                                        }
                                    }
                                    Ok(None) => {} // Not our oracle
                                    Err(e) => warn!("Failed to process delivery tx: {}", e),
                                }
                            }
                        }
                        warn!("Log subscription stream ended, reconnecting...");
                    }
                    Err(e) => {
                        error!("Failed to subscribe to logs: {}", e);
                    }
                }
            }
            Err(e) => {
                error!("WebSocket connection failed: {}", e);
            }
        }

        // Backoff before reconnect
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}

/// Given a transaction signature, extract the payment account and build an EvaluationJob.
async fn fetch_delivery_job(
    rpc: &RpcClient,
    sig: &solana_sdk::signature::Signature,
    config: &OracleConfig,
) -> Result<Option<EvaluationJob>, OracleError> {
    use solana_transaction_status::UiTransactionEncoding;

    let tx = rpc
        .get_transaction(sig, UiTransactionEncoding::Base64)
        .await
        .map_err(|e| OracleError::Chain(format!("Failed to fetch tx {}: {}", sig, e)))?;

    let account_keys: Vec<Pubkey> = match &tx.transaction.transaction {
        solana_transaction_status::EncodedTransaction::Json(ui_tx) => match &ui_tx.message {
            solana_transaction_status::UiMessage::Parsed(parsed) => parsed
                .account_keys
                .iter()
                .filter_map(|k| k.pubkey.parse::<Pubkey>().ok())
                .collect(),
            solana_transaction_status::UiMessage::Raw(raw) => raw
                .account_keys
                .iter()
                .filter_map(|k| k.parse::<Pubkey>().ok())
                .collect(),
        },
        _ => return Ok(None),
    };

    // The SubmitDelivery instruction's account layout:
    // [seller, bank, config, escrow, payment]
    // The payment account is the last one in the instruction.
    // We try each account key that might be a payment PDA.
    for key in &account_keys {
        if let Ok(Some(job)) = read_payment(rpc, key, &config.oracle_pubkey()).await {
            return Ok(Some(job));
        }
    }

    Ok(None)
}
