use crate::{
    config::OracleConfig,
    error::OracleError,
    types::{EvaluationJob, RuntimeHealth},
};
use base64::{engine::general_purpose::STANDARD as B64_ENGINE, Engine};
use futures_util::StreamExt;
use sla_escrow_api::{event::DeliverySubmittedEvent, instruction::EscrowInstruction};
use solana_client::nonblocking::pubsub_client::PubsubClient;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcTransactionConfig, RpcTransactionLogsConfig};
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey, signature::Signature};
use solana_transaction_status::{
    option_serializer::OptionSerializer, EncodedTransaction, UiCompiledInstruction, UiInstruction,
    UiMessage, UiParsedInstruction, UiPartiallyDecodedInstruction, UiTransactionEncoding,
};
use solana_transaction_status_client_types::ParsedAccount;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};

/// Parse `DeliverySubmittedEvent` from RPC log lines (`sol_log_data` → `Program data: <base64>`).
pub fn parse_delivery_events_from_logs(logs: &[String]) -> Vec<DeliverySubmittedEvent> {
    const PREFIX: &str = "Program data: ";
    let mut out = Vec::new();
    let expected = std::mem::size_of::<DeliverySubmittedEvent>();
    for line in logs {
        let line = line.trim();
        let Some(b64) = line.strip_prefix(PREFIX) else {
            continue;
        };
        let Ok(bytes) = B64_ENGINE.decode(b64.trim()) else {
            continue;
        };
        if bytes.len() != expected {
            continue;
        }
        if let Ok(ev) = bytemuck::try_from_bytes::<DeliverySubmittedEvent>(bytes.as_slice()) {
            out.push(*ev);
        }
    }
    out
}

fn submit_delivery_discriminant() -> u8 {
    EscrowInstruction::SubmitDelivery as u8
}

/// Returns payment PDA from a partially decoded CPI-style instruction.
fn payment_from_partial_ix(
    ix: &UiPartiallyDecodedInstruction,
    escrow_program: &str,
) -> Option<Pubkey> {
    if ix.program_id != escrow_program {
        return None;
    }
    let data = bs58::decode(&ix.data).into_vec().ok()?;
    if data.first().copied() != Some(submit_delivery_discriminant()) {
        return None;
    }
    // submit_delivery: [seller, bank, config, escrow, payment]
    if ix.accounts.len() < 5 {
        return None;
    }
    ix.accounts[4].parse().ok()
}

/// Returns payment PDA from a compiled instruction + full account key list.
fn payment_from_compiled_ix(
    ix: &UiCompiledInstruction,
    account_keys: &[ParsedAccount],
    escrow_program: &Pubkey,
) -> Option<Pubkey> {
    let program_id = account_keys
        .get(ix.program_id_index as usize)?
        .pubkey
        .parse::<Pubkey>()
        .ok()?;
    if program_id != *escrow_program {
        return None;
    }
    let data = bs58::decode(&ix.data).into_vec().ok()?;
    if data.first().copied() != Some(submit_delivery_discriminant()) {
        return None;
    }
    let pay_idx = *ix.accounts.get(4)? as usize;
    account_keys.get(pay_idx)?.pubkey.parse().ok()
}

fn collect_payment_candidates_from_instructions(
    instructions: &[UiInstruction],
    account_keys: &[ParsedAccount],
    escrow_program: &Pubkey,
) -> Vec<Pubkey> {
    let escrow_str = escrow_program.to_string();
    let mut out = Vec::new();
    for ix in instructions {
        match ix {
            UiInstruction::Parsed(UiParsedInstruction::PartiallyDecoded(p)) => {
                if let Some(pk) = payment_from_partial_ix(p, &escrow_str) {
                    out.push(pk);
                }
            }
            UiInstruction::Parsed(UiParsedInstruction::Parsed(_)) => {}
            UiInstruction::Compiled(c) => {
                if let Some(pk) = payment_from_compiled_ix(c, account_keys, escrow_program) {
                    out.push(pk);
                }
            }
        }
    }
    out
}

/// Reads a Payment PDA from chain and returns an EvaluationJob if it's relevant to this oracle.
pub async fn read_payment(
    rpc: &RpcClient,
    payment_pubkey: &Pubkey,
    oracle_pubkey: &Pubkey,
) -> Result<Option<EvaluationJob>, OracleError> {
    use sla_escrow_api::state::Payment;

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
/// when a `SubmitDelivery` / delivery event is detected.
pub async fn monitor_deliveries(
    config: Arc<OracleConfig>,
    tx: mpsc::Sender<EvaluationJob>,
    health: Arc<RwLock<RuntimeHealth>>,
) {
    loop {
        info!("Connecting to Solana WebSocket at {}", config.solana_ws_url);

        match PubsubClient::new(&config.solana_ws_url).await {
            Ok(pubsub) => {
                let filter =
                    solana_client::rpc_config::RpcTransactionLogsFilter::Mentions(vec![config
                        .escrow_program_id
                        .to_string()]);
                let log_config = RpcTransactionLogsConfig {
                    commitment: Some(CommitmentConfig::confirmed()),
                };

                match pubsub.logs_subscribe(filter, log_config).await {
                    Ok((mut stream, _unsub)) => {
                        info!("Subscribed to escrow program logs");
                        {
                            let mut h = health.write().await;
                            h.websocket_connected = true;
                            h.last_websocket_connected_at = Some(chrono::Utc::now().to_rfc3339());
                            h.last_monitor_error = None;
                        }

                        while let Some(log_response) = stream.next().await {
                            {
                                let mut h = health.write().await;
                                h.last_websocket_message_at = Some(chrono::Utc::now().to_rfc3339());
                            }
                            let logs = &log_response.value.logs;
                            let has_delivery = logs.iter().any(|l| {
                                l.contains("DeliverySubmittedEvent") || l.contains("Program data:")
                            });
                            if !has_delivery {
                                continue;
                            }

                            if let Ok(sig) = log_response.value.signature.parse::<Signature>() {
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
                                    Ok(None) => {}
                                    Err(e) => warn!("Failed to process delivery tx: {}", e),
                                }
                            }
                        }
                        warn!("Log subscription stream ended, reconnecting...");
                        let mut h = health.write().await;
                        h.websocket_connected = false;
                        h.last_monitor_error = Some("log subscription stream ended".into());
                    }
                    Err(e) => {
                        error!("Failed to subscribe to logs: {}", e);
                        let mut h = health.write().await;
                        h.websocket_connected = false;
                        h.last_monitor_error = Some(e.to_string());
                    }
                }
            }
            Err(e) => {
                error!("WebSocket connection failed: {}", e);
                let mut h = health.write().await;
                h.websocket_connected = false;
                h.last_monitor_error = Some(e.to_string());
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}

/// Prefer structured `SubmitDelivery` account layout; fall back to scanning account keys; optional log event cross-check.
async fn fetch_delivery_job(
    rpc: &RpcClient,
    sig: &Signature,
    config: &OracleConfig,
) -> Result<Option<EvaluationJob>, OracleError> {
    let tx_config = RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::JsonParsed),
        commitment: Some(CommitmentConfig::confirmed()),
        max_supported_transaction_version: Some(0),
    };

    let enc = rpc
        .get_transaction_with_config(sig, tx_config)
        .await
        .map_err(|e| OracleError::Chain(format!("Failed to fetch tx {}: {}", sig, e)))?;

    let EncodedTransaction::Json(ui_tx) = &enc.transaction.transaction else {
        warn!(
            "getTransaction for {} was not JsonParsed; cannot extract SubmitDelivery layout",
            sig
        );
        return Ok(None);
    };

    let oracle_pk = config.oracle_pubkey();
    let escrow = config.escrow_program_id;

    let log_events: Vec<DeliverySubmittedEvent> = enc
        .transaction
        .meta
        .as_ref()
        .and_then(|m| match &m.log_messages {
            OptionSerializer::Some(logs) => Some(parse_delivery_events_from_logs(logs)),
            _ => None,
        })
        .unwrap_or_default();

    if let UiMessage::Parsed(pm) = &ui_tx.message {
        let mut candidates = collect_payment_candidates_from_instructions(
            &pm.instructions,
            &pm.account_keys,
            &escrow,
        );

        if let Some(OptionSerializer::Some(groups)) =
            enc.transaction.meta.as_ref().map(|m| &m.inner_instructions)
        {
            for g in groups {
                candidates.extend(collect_payment_candidates_from_instructions(
                    &g.instructions,
                    &pm.account_keys,
                    &escrow,
                ));
            }
        }

        candidates.sort_by_key(|p| p.to_string());
        candidates.dedup();

        for pk in candidates {
            if let Ok(Some(job)) = read_payment(rpc, &pk, &oracle_pk).await {
                if event_matches_job(&log_events, &job) {
                    return Ok(Some(job));
                }
                if log_events.is_empty() {
                    // No parsed program data (some RPC truncations); still try the structured account path.
                    return Ok(Some(job));
                }
            }
        }

        let account_keys: Vec<Pubkey> = pm
            .account_keys
            .iter()
            .filter_map(|a| a.pubkey.parse().ok())
            .collect();

        for key in account_keys {
            if let Ok(Some(job)) = read_payment(rpc, &key, &oracle_pk).await {
                if event_matches_job(&log_events, &job) || log_events.is_empty() {
                    return Ok(Some(job));
                }
            }
        }
    }

    Ok(None)
}

fn event_matches_job(events: &[DeliverySubmittedEvent], job: &EvaluationJob) -> bool {
    if events.is_empty() {
        return true;
    }
    events
        .iter()
        .any(|e| e.payment_uid == job.payment_uid && e.delivery_hash == job.delivery_hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::pubkey::Pubkey;

    #[test]
    fn parse_delivery_event_roundtrip() {
        let ev = DeliverySubmittedEvent {
            payment_uid: [7u8; 32],
            delivery_hash: [8u8; 32],
            timestamp: 1_700_000_000,
            seller: Pubkey::new_unique(),
        };
        let raw = bytemuck::bytes_of(&ev);
        let line = format!("Program data: {}", B64_ENGINE.encode(raw));
        let parsed = parse_delivery_events_from_logs(&[line]);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].payment_uid, ev.payment_uid);
        assert_eq!(parsed[0].delivery_hash, ev.delivery_hash);
        assert_eq!(parsed[0].timestamp, ev.timestamp);
    }
}
