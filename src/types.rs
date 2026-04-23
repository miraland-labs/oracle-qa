use serde::{Deserialize, Serialize};

/// Off-chain SLA document that defines the quality contract.
/// `payment.sla_hash` MUST equal SHA256(UTF-8 octets of the SLA JSON); see `spec/api-quality-v1/NORMATIVE.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlaDocument {
    pub version: u32,
    pub endpoint: String,
    pub method: String,
    #[serde(default)]
    pub response_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub required_fields: Vec<String>,
    #[serde(default = "default_max_latency")]
    pub max_latency_ms: u64,
    #[serde(default = "default_min_status")]
    pub min_status_code: u16,
    #[serde(default = "default_max_status")]
    pub max_status_code: u16,
    #[serde(default)]
    pub min_body_length: Option<usize>,
}

fn default_max_latency() -> u64 {
    5000
}
fn default_min_status() -> u16 {
    200
}
fn default_max_status() -> u16 {
    299
}

/// Off-chain delivery evidence submitted by the seller.
/// `payment.delivery_hash` MUST equal SHA256(UTF-8 octets of the evidence JSON); see `spec/api-quality-v1/NORMATIVE.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryEvidence {
    pub status_code: u16,
    pub latency_ms: u64,
    pub response_body: serde_json::Value,
    #[serde(default)]
    pub response_headers: Option<serde_json::Map<String, serde_json::Value>>,
    pub timestamp: i64,
}

/// Result of the oracle's SLA evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationResult {
    pub approved: bool,
    pub resolution_reason: u16,
    pub checks: Vec<CheckResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

/// A pending evaluation job from the chain monitor.
/// All fields are populated from on-chain Payment state; some are reserved
/// for future use (fee calculation, deadline-aware evaluation, audit logging).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct EvaluationJob {
    pub payment_uid: [u8; 32],
    pub payment_pubkey: solana_sdk::pubkey::Pubkey,
    pub sla_hash: [u8; 32],
    pub delivery_hash: [u8; 32],
    pub amount: u64,
    pub mint: solana_sdk::pubkey::Pubkey,
    pub oracle_authority: solana_sdk::pubkey::Pubkey,
    pub expires_at: i64,
}
