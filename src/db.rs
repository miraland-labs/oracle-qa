use crate::{
    error::OracleError,
    types::{EvaluationJob, EvaluationResult},
};
use deadpool_postgres::{Config, Pool, PoolConfig, Runtime};
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use postgres_openssl::MakeTlsConnector;
use serde_json::json;
use std::{error::Error, time::Duration};
use tokio::time::timeout;
use tokio_postgres::types::Json;
use tracing::error;

#[derive(Clone)]
pub struct OracleDb {
    pool: Pool,
}

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("db pool: {0}")]
    Pool(String),
    #[error("db query: {0}")]
    Query(String),
    #[error("db query timed out")]
    Timeout,
}

fn format_err_chain(err: &dyn Error) -> String {
    let mut out = err.to_string();
    let mut src = err.source();
    while let Some(s) = src {
        out.push_str(" | ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    out
}

impl OracleDb {
    const WAIT: Duration = Duration::from_secs(15);
    const CREATE: Duration = Duration::from_secs(10);
    const RECYCLE: Duration = Duration::from_secs(30);
    const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

    pub fn connect(database_url: impl Into<String>) -> Result<Self, DbError> {
        let mut cfg = Config::new();
        cfg.url = Some(database_url.into());
        cfg.pool = Some(PoolConfig {
            max_size: 5,
            timeouts: deadpool_postgres::Timeouts {
                wait: Some(Self::WAIT),
                create: Some(Self::CREATE),
                recycle: Some(Self::RECYCLE),
            },
            ..Default::default()
        });

        let mut builder =
            SslConnector::builder(SslMethod::tls()).map_err(|e| DbError::Query(e.to_string()))?;
        builder.set_verify(SslVerifyMode::NONE);
        let tls = MakeTlsConnector::new(builder.build());
        let pool = cfg
            .create_pool(Some(Runtime::Tokio1), tls)
            .map_err(|e| DbError::Pool(format_err_chain(&e)))?;
        Ok(Self { pool })
    }

    pub fn from_url(database_url: Option<&str>) -> Option<Result<Self, DbError>> {
        database_url
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Self::connect)
    }

    pub async fn record_detected(&self, job: &EvaluationJob) -> Result<(), DbError> {
        self.upsert_job(job, "detected", None, None, None).await?;
        self.insert_event(job, "detected", json!({})).await
    }

    pub async fn record_queued(&self, job: &EvaluationJob) -> Result<(), DbError> {
        self.upsert_job(job, "queued", None, None, None).await?;
        self.insert_event(job, "queued", json!({})).await
    }

    pub async fn record_started(&self, job: &EvaluationJob) -> Result<(), DbError> {
        self.upsert_job(job, "running", None, None, None).await?;
        self.insert_event(job, "started", json!({})).await
    }

    pub async fn record_failed(&self, job: &EvaluationJob, error_msg: &str) -> Result<(), DbError> {
        self.upsert_job(job, "failed", Some(error_msg), None, None)
            .await?;
        self.insert_event(job, "failed", json!({ "error": error_msg }))
            .await
    }

    pub async fn record_dead_letter(
        &self,
        job: &EvaluationJob,
        error_msg: &str,
    ) -> Result<(), DbError> {
        self.upsert_job(job, "dead_letter", Some(error_msg), None, None)
            .await?;
        self.insert_event(job, "dead_letter", json!({ "error": error_msg }))
            .await
    }

    /// Return true when the payment UID has already reached a terminal state in the ledger.
    ///
    /// Terminal means "this oracle instance (or a peer sharing the same ledger) has already
    /// settled or permanently dead-lettered the job" — used for restart-safe dedupe so duplicate
    /// log events (e.g. on WS reconnect) do not trigger a redundant `ConfirmOracle` settle.
    pub async fn is_terminal(&self, payment_uid: &[u8; 32]) -> Result<bool, DbError> {
        const SQL: &str = r#"
            SELECT 1
              FROM oracle_jobs
             WHERE payment_uid = $1
               AND status IN ('settled', 'dead_letter')
             LIMIT 1
        "#;

        let client = self
            .pool
            .get()
            .await
            .map_err(|e| DbError::Pool(format_err_chain(&e)))?;
        let uid = hex::encode(payment_uid);
        match timeout(Self::QUERY_TIMEOUT, client.query_opt(SQL, &[&uid])).await {
            Ok(Ok(row)) => Ok(row.is_some()),
            Ok(Err(e)) => Err(DbError::Query(format_err_chain(&e))),
            Err(_) => Err(DbError::Timeout),
        }
    }

    /// Current attempt count for a payment UID (0 when no row exists).
    pub async fn attempt_count(&self, payment_uid: &[u8; 32]) -> Result<i32, DbError> {
        const SQL: &str = r#"
            SELECT attempts FROM oracle_jobs WHERE payment_uid = $1
        "#;
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| DbError::Pool(format_err_chain(&e)))?;
        let uid = hex::encode(payment_uid);
        match timeout(Self::QUERY_TIMEOUT, client.query_opt(SQL, &[&uid])).await {
            Ok(Ok(Some(row))) => Ok(row.get::<_, i32>(0)),
            Ok(Ok(None)) => Ok(0),
            Ok(Err(e)) => Err(DbError::Query(format_err_chain(&e))),
            Err(_) => Err(DbError::Timeout),
        }
    }

    /// Read an `oracle_parameters` row as a string (None when absent or inactive).
    pub async fn get_parameter(&self, name: &str) -> Result<Option<String>, DbError> {
        const SQL: &str = r#"
            SELECT param_value
              FROM oracle_parameters
             WHERE param_name = $1
               AND inactive = FALSE
             LIMIT 1
        "#;
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| DbError::Pool(format_err_chain(&e)))?;
        match timeout(Self::QUERY_TIMEOUT, client.query_opt(SQL, &[&name])).await {
            Ok(Ok(Some(row))) => Ok(Some(row.get::<_, String>(0))),
            Ok(Ok(None)) => Ok(None),
            Ok(Err(e)) => Err(DbError::Query(format_err_chain(&e))),
            Err(_) => Err(DbError::Timeout),
        }
    }

    /// Upsert an `oracle_parameters` row (active by default).
    pub async fn set_parameter(&self, name: &str, value: &str) -> Result<(), DbError> {
        const SQL: &str = r#"
            INSERT INTO oracle_parameters (param_name, param_value, inactive, updated_at)
            VALUES ($1, $2, FALSE, NOW())
            ON CONFLICT (param_name) DO UPDATE SET
                param_value = EXCLUDED.param_value,
                inactive    = FALSE,
                updated_at  = NOW()
        "#;
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| DbError::Pool(format_err_chain(&e)))?;
        match timeout(Self::QUERY_TIMEOUT, client.execute(SQL, &[&name, &value])).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(DbError::Query(format_err_chain(&e))),
            Err(_) => Err(DbError::Timeout),
        }
    }

    pub async fn record_settled(
        &self,
        job: &EvaluationJob,
        result: &EvaluationResult,
        signature: Option<&str>,
        resolution_hash: &[u8; 32],
    ) -> Result<(), DbError> {
        let hash_hex = hex::encode(resolution_hash);
        self.upsert_job(job, "settled", None, signature, Some(&hash_hex))
            .await?;

        const SQL: &str = r#"
            INSERT INTO oracle_verdicts (
                oracle_job_id, approved, resolution_reason, resolution_hash,
                checks, settlement_signature
            )
            SELECT id, $2, $3, $4, $5, $6
            FROM oracle_jobs
            WHERE payment_uid = $1
            ON CONFLICT (oracle_job_id) DO UPDATE SET
                approved = EXCLUDED.approved,
                resolution_reason = EXCLUDED.resolution_reason,
                resolution_hash = EXCLUDED.resolution_hash,
                checks = EXCLUDED.checks,
                settlement_signature = COALESCE(EXCLUDED.settlement_signature, oracle_verdicts.settlement_signature)
            "#;

        let client = self
            .pool
            .get()
            .await
            .map_err(|e| DbError::Pool(format_err_chain(&e)))?;
        let uid = hex::encode(job.payment_uid);
        let checks = Json(&result.checks);
        let db_result = timeout(
            Self::QUERY_TIMEOUT,
            client.execute(
                SQL,
                &[
                    &uid,
                    &result.approved,
                    &(result.resolution_reason as i32),
                    &hash_hex,
                    &checks,
                    &signature,
                ],
            ),
        )
        .await;

        match db_result {
            Ok(Ok(_)) => {
                self.insert_event(
                    job,
                    "settled",
                    json!({
                        "approved": result.approved,
                        "resolutionReason": result.resolution_reason,
                        "resolutionHash": hash_hex,
                        "signature": signature
                    }),
                )
                .await
            }
            Ok(Err(e)) => Err(DbError::Query(format_err_chain(&e))),
            Err(_) => Err(DbError::Timeout),
        }
    }

    async fn upsert_job(
        &self,
        job: &EvaluationJob,
        status: &str,
        last_error: Option<&str>,
        signature: Option<&str>,
        resolution_hash: Option<&str>,
    ) -> Result<(), DbError> {
        const SQL: &str = r#"
            INSERT INTO oracle_jobs (
                payment_uid, payment_pubkey, mint, amount, sla_hash, delivery_hash,
                oracle_authority, expires_at, status, attempts, started_at, completed_at,
                last_error, settlement_signature, resolution_hash, updated_at
            )
            VALUES (
                $1, $2, $3, $4, $5, $6,
                $7, to_timestamp($8::double precision), $9,
                CASE WHEN $9 = 'running' THEN 1 ELSE 0 END,
                CASE WHEN $9 = 'running' THEN NOW() ELSE NULL END,
                CASE WHEN $9 IN ('settled', 'failed', 'dead_letter') THEN NOW() ELSE NULL END,
                $10, $11, $12, NOW()
            )
            ON CONFLICT (payment_uid) DO UPDATE SET
                payment_pubkey = EXCLUDED.payment_pubkey,
                mint = EXCLUDED.mint,
                amount = EXCLUDED.amount,
                sla_hash = EXCLUDED.sla_hash,
                delivery_hash = EXCLUDED.delivery_hash,
                oracle_authority = EXCLUDED.oracle_authority,
                expires_at = EXCLUDED.expires_at,
                status = EXCLUDED.status,
                attempts = oracle_jobs.attempts + CASE WHEN EXCLUDED.status = 'running' THEN 1 ELSE 0 END,
                started_at = CASE WHEN EXCLUDED.status = 'running' THEN NOW() ELSE oracle_jobs.started_at END,
                completed_at = CASE WHEN EXCLUDED.status IN ('settled', 'failed', 'dead_letter') THEN NOW() ELSE oracle_jobs.completed_at END,
                last_error = COALESCE(EXCLUDED.last_error, oracle_jobs.last_error),
                settlement_signature = COALESCE(EXCLUDED.settlement_signature, oracle_jobs.settlement_signature),
                resolution_hash = COALESCE(EXCLUDED.resolution_hash, oracle_jobs.resolution_hash),
                updated_at = NOW()
            "#;

        let client = self
            .pool
            .get()
            .await
            .map_err(|e| DbError::Pool(format_err_chain(&e)))?;
        let uid = hex::encode(job.payment_uid);
        let payment_pubkey = job.payment_pubkey.to_string();
        let mint = job.mint.to_string();
        let amount = i64::try_from(job.amount).unwrap_or(i64::MAX);
        let sla_hash = hex::encode(job.sla_hash);
        let delivery_hash = hex::encode(job.delivery_hash);
        let oracle_authority = job.oracle_authority.to_string();

        match timeout(
            Self::QUERY_TIMEOUT,
            client.execute(
                SQL,
                &[
                    &uid,
                    &payment_pubkey,
                    &mint,
                    &amount,
                    &sla_hash,
                    &delivery_hash,
                    &oracle_authority,
                    &job.expires_at,
                    &status,
                    &last_error,
                    &signature,
                    &resolution_hash,
                ],
            ),
        )
        .await
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => {
                error!(error = %format_err_chain(&e), "oracle_jobs upsert failed");
                Err(DbError::Query(format_err_chain(&e)))
            }
            Err(_) => Err(DbError::Timeout),
        }
    }

    async fn insert_event(
        &self,
        job: &EvaluationJob,
        event: &str,
        payload: serde_json::Value,
    ) -> Result<(), DbError> {
        const SQL: &str = r#"
            INSERT INTO oracle_lifecycle_events (oracle_job_id, payment_uid, event, payload)
            SELECT id, $1, $2, $3
            FROM oracle_jobs
            WHERE payment_uid = $1
            "#;

        let client = self
            .pool
            .get()
            .await
            .map_err(|e| DbError::Pool(format_err_chain(&e)))?;
        let uid = hex::encode(job.payment_uid);
        let payload = Json(payload);
        match timeout(
            Self::QUERY_TIMEOUT,
            client.execute(SQL, &[&uid, &event, &payload]),
        )
        .await
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(DbError::Query(format_err_chain(&e))),
            Err(_) => Err(DbError::Timeout),
        }
    }
}

impl From<DbError> for OracleError {
    fn from(e: DbError) -> Self {
        OracleError::Database(e.to_string())
    }
}
