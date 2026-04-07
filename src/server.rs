use crate::config::OracleConfig;
use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::{cors::CorsLayer, trace::TraceLayer};

pub struct AppState {
    pub config: OracleConfig,
    pub stats: RwLock<OracleStats>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OracleStats {
    pub total_evaluated: u64,
    pub total_approved: u64,
    pub total_rejected: u64,
    pub total_errors: u64,
    pub uptime_seconds: u64,
    pub last_evaluation_at: Option<String>,
}

pub fn create_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/stats", get(stats))
        .route("/evaluate", post(manual_evaluate))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn root() -> impl IntoResponse {
    Json(serde_json::json!({
        "service": "oracle-qa",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "API Response Quality Oracle for x402 SLA-Escrow",
        "endpoints": {
            "GET /health": "Health check",
            "GET /stats": "Oracle statistics",
            "POST /evaluate": "Manual evaluation trigger"
        }
    }))
}

async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let rpc = RpcClient::new_with_commitment(
        state.config.solana_rpc_url.clone(),
        CommitmentConfig::confirmed(),
    );

    let chain_ok = rpc.get_slot().await.is_ok();

    let status = if chain_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status,
        Json(serde_json::json!({
            "status": if chain_ok { "healthy" } else { "degraded" },
            "oracle_pubkey": state.config.oracle_pubkey().to_string(),
            "program_id": state.config.escrow_program_id.to_string(),
            "chain_connected": chain_ok,
        })),
    )
}

async fn stats(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let stats = state.stats.read().await.clone();
    Json(stats)
}

#[derive(Deserialize)]
struct ManualEvaluateRequest {
    payment_pubkey: String,
}

#[derive(Serialize)]
struct ManualEvaluateResponse {
    approved: bool,
    signature: Option<String>,
    checks: Vec<crate::types::CheckResult>,
    error: Option<String>,
}

async fn manual_evaluate(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ManualEvaluateRequest>,
) -> impl IntoResponse {
    let payment_pubkey: solana_sdk::pubkey::Pubkey = match req.payment_pubkey.parse() {
        Ok(pk) => pk,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ManualEvaluateResponse {
                    approved: false,
                    signature: None,
                    checks: vec![],
                    error: Some(format!("Invalid pubkey: {}", e)),
                }),
            );
        }
    };

    let rpc = RpcClient::new_with_commitment(
        state.config.solana_rpc_url.clone(),
        CommitmentConfig::confirmed(),
    );

    let job = match crate::chain::read_payment(&rpc, &payment_pubkey, &state.config.oracle_pubkey())
        .await
    {
        Ok(Some(job)) => job,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(ManualEvaluateResponse {
                    approved: false,
                    signature: None,
                    checks: vec![],
                    error: Some("Payment not found or not assigned to this oracle".into()),
                }),
            );
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ManualEvaluateResponse {
                    approved: false,
                    signature: None,
                    checks: vec![],
                    error: Some(format!("Chain read error: {}", e)),
                }),
            );
        }
    };

    match crate::pipeline::run_pipeline(&state.config, &job).await {
        Ok((result, sig)) => {
            let mut stats = state.stats.write().await;
            stats.total_evaluated += 1;
            if result.approved {
                stats.total_approved += 1;
            } else {
                stats.total_rejected += 1;
            }
            stats.last_evaluation_at = Some(chrono::Utc::now().to_rfc3339());

            (
                StatusCode::OK,
                Json(ManualEvaluateResponse {
                    approved: result.approved,
                    signature: sig,
                    checks: result.checks,
                    error: None,
                }),
            )
        }
        Err(e) => {
            let mut stats = state.stats.write().await;
            stats.total_errors += 1;

            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ManualEvaluateResponse {
                    approved: false,
                    signature: None,
                    checks: vec![],
                    error: Some(e.to_string()),
                }),
            )
        }
    }
}
