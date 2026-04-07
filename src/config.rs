use anyhow::{Context, Result};
use solana_sdk::{
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
    signer::Signer,
};
use std::{env, str::FromStr, sync::Arc};

#[derive(Clone)]
pub struct OracleConfig {
    pub solana_rpc_url: String,
    pub solana_ws_url: String,
    pub oracle_keypair: Arc<Keypair>,
    pub escrow_program_id: Pubkey,
    pub bind_addr: String,
    pub evaluation_timeout_ms: u64,
    pub evidence_registry_url: String,
}

impl OracleConfig {
    pub fn from_env() -> Result<Self> {
        let solana_rpc_url =
            env::var("SOLANA_RPC_URL").unwrap_or_else(|_| "https://api.devnet.solana.com".into());
        let solana_ws_url =
            env::var("SOLANA_WS_URL").unwrap_or_else(|_| "wss://api.devnet.solana.com".into());

        let keypair_path =
            env::var("ORACLE_KEYPAIR_PATH").context("ORACLE_KEYPAIR_PATH is required")?;
        let oracle_keypair = read_keypair_file(&keypair_path).map_err(|e| {
            anyhow::anyhow!("Failed to read oracle keypair at {}: {}", keypair_path, e)
        })?;

        let escrow_program_id = env::var("ESCROW_PROGRAM_ID")
            .map(|s| Pubkey::from_str(&s).expect("Invalid ESCROW_PROGRAM_ID"))
            .unwrap_or(sla_escrow_api::ID);

        let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:4020".into());

        let evaluation_timeout_ms = env::var("EVALUATION_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30_000);

        let evidence_registry_url =
            env::var("EVIDENCE_REGISTRY_URL").unwrap_or_else(|_| "http://localhost:4021".into());

        Ok(Self {
            solana_rpc_url,
            solana_ws_url,
            oracle_keypair: Arc::new(oracle_keypair),
            escrow_program_id,
            bind_addr,
            evaluation_timeout_ms,
            evidence_registry_url,
        })
    }

    pub fn oracle_pubkey(&self) -> Pubkey {
        self.oracle_keypair.pubkey()
    }
}
