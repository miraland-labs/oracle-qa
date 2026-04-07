use thiserror::Error;

#[derive(Debug, Error)]
pub enum OracleError {
    #[error("Chain error: {0}")]
    Chain(String),

    #[error("Evidence not found for hash: {0}")]
    EvidenceNotFound(String),

    #[error("SLA document parse error: {0}")]
    SlaParse(String),

    #[error("Delivery evidence parse error: {0}")]
    DeliveryParse(String),

    #[error("Evaluation failed: {0}")]
    Evaluation(String),

    #[error("Settlement failed: {0}")]
    Settlement(String),
}

impl From<solana_client::client_error::ClientError> for OracleError {
    fn from(e: solana_client::client_error::ClientError) -> Self {
        OracleError::Chain(e.to_string())
    }
}

impl From<reqwest::Error> for OracleError {
    fn from(e: reqwest::Error) -> Self {
        OracleError::EvidenceNotFound(e.to_string())
    }
}
