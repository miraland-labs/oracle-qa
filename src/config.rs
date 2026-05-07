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
    /// Ordered list of registry base URLs (mirrors). Tried in order per fetch; first success wins.
    pub evidence_registry_urls: Vec<String>,
    /// If set, sent as `Authorization: <value>` on registry GET requests (e.g. `Bearer <token>`).
    pub evidence_registry_auth_header: Option<String>,
    pub evidence_fetch_max_retries: u32,
    pub evidence_fetch_retry_base_ms: u64,
    pub database_url: Option<String>,
    pub operator_token_sha256: Option<[u8; 32]>,
    pub allow_unauthenticated_manual_evaluate: bool,
    pub cors_allowed_origins: Vec<String>,
    pub manual_evaluate_rate_limit: usize,
    pub manual_evaluate_rate_window_ms: u64,
    pub strict_profile: bool,
    pub dead_letter_max_attempts: u32,
    pub job_channel_capacity: usize,
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

        let evidence_registry_urls: Vec<String> = match env::var("EVIDENCE_REGISTRY_URLS") {
            Ok(s) => {
                let parts: Vec<String> = s
                    .split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect();
                if parts.is_empty() {
                    vec![env::var("EVIDENCE_REGISTRY_URL")
                        .unwrap_or_else(|_| "http://localhost:4021".into())]
                } else {
                    parts
                }
            }
            Err(_) => vec![env::var("EVIDENCE_REGISTRY_URL")
                .unwrap_or_else(|_| "http://localhost:4021".into())],
        };

        let evidence_registry_auth_header = env::var("EVIDENCE_REGISTRY_AUTH_HEADER")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let evidence_fetch_max_retries = env::var("EVIDENCE_FETCH_MAX_RETRIES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3);
        let evidence_fetch_retry_base_ms = env::var("EVIDENCE_FETCH_RETRY_BASE_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200);

        let database_url = env::var("DATABASE_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let operator_token = env::var("ORACLE_OPERATOR_TOKEN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let operator_token_sha256 = match env::var("ORACLE_OPERATOR_TOKEN_SHA256") {
            Ok(hex) if !hex.trim().is_empty() => {
                let decoded = hex::decode(hex.trim())
                    .context("ORACLE_OPERATOR_TOKEN_SHA256 must be 64 hex characters")?;
                let arr: [u8; 32] = decoded.try_into().map_err(|_| {
                    anyhow::anyhow!("ORACLE_OPERATOR_TOKEN_SHA256 must decode to 32 bytes")
                })?;
                Some(arr)
            }
            _ => operator_token.map(|token| sha256_bytes(token.as_bytes())),
        };

        let allow_unauthenticated_manual_evaluate =
            env_bool("ORACLE_ALLOW_UNAUTHENTICATED_MANUAL_EVALUATE", false);
        let cors_allowed_origins = env::var("ORACLE_CORS_ALLOWED_ORIGINS")
            .unwrap_or_default()
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect();
        let manual_evaluate_rate_limit = env::var("ORACLE_MANUAL_EVALUATE_RATE_LIMIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        let manual_evaluate_rate_window_ms = env::var("ORACLE_MANUAL_EVALUATE_RATE_WINDOW_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60_000);
        let strict_profile = env_bool("ORACLE_STRICT_PROFILE", true);
        let dead_letter_max_attempts = env::var("ORACLE_DEAD_LETTER_MAX_ATTEMPTS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);
        let job_channel_capacity = env::var("ORACLE_JOB_CHANNEL_CAPACITY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(256);

        Ok(Self {
            solana_rpc_url,
            solana_ws_url,
            oracle_keypair: Arc::new(oracle_keypair),
            escrow_program_id,
            bind_addr,
            evaluation_timeout_ms,
            evidence_registry_urls,
            evidence_registry_auth_header,
            evidence_fetch_max_retries,
            evidence_fetch_retry_base_ms,
            database_url,
            operator_token_sha256,
            allow_unauthenticated_manual_evaluate,
            cors_allowed_origins,
            manual_evaluate_rate_limit,
            manual_evaluate_rate_window_ms,
            strict_profile,
            dead_letter_max_attempts,
            job_channel_capacity,
        })
    }

    pub fn oracle_pubkey(&self) -> Pubkey {
        self.oracle_keypair.pubkey()
    }
}

fn env_bool(name: &str, default: bool) -> bool {
    env::var(name)
        .ok()
        .map(|s| {
            matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
}

fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}
