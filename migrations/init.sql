-- oracle-qa production ledger bootstrap (PostgreSQL 15+).
-- Run once before enabling DATABASE_URL:
--   psql "$DATABASE_URL" -f migrations/init.sql

CREATE TABLE IF NOT EXISTS oracle_jobs (
    id                    BIGSERIAL PRIMARY KEY,
    payment_uid           TEXT NOT NULL UNIQUE,
    payment_pubkey        TEXT NOT NULL,
    mint                  TEXT NOT NULL,
    amount                BIGINT NOT NULL,
    sla_hash              TEXT NOT NULL,
    delivery_hash         TEXT NOT NULL,
    oracle_authority      TEXT NOT NULL,
    expires_at            TIMESTAMPTZ NOT NULL,
    status                TEXT NOT NULL DEFAULT 'detected',
    attempts              INTEGER NOT NULL DEFAULT 0,
    locked_at             TIMESTAMPTZ,
    started_at            TIMESTAMPTZ,
    completed_at          TIMESTAMPTZ,
    last_error            TEXT,
    settlement_signature  TEXT,
    resolution_hash       TEXT,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_oracle_jobs_status
    ON oracle_jobs (status ASC, updated_at ASC);

CREATE INDEX IF NOT EXISTS idx_oracle_jobs_payment_pubkey
    ON oracle_jobs (payment_pubkey ASC);

CREATE INDEX IF NOT EXISTS idx_oracle_jobs_oracle_authority
    ON oracle_jobs (oracle_authority ASC);

CREATE TABLE IF NOT EXISTS oracle_verdicts (
    id                    BIGSERIAL PRIMARY KEY,
    oracle_job_id         BIGINT NOT NULL REFERENCES oracle_jobs (id) ON DELETE CASCADE,
    approved              BOOLEAN NOT NULL,
    resolution_reason     INTEGER NOT NULL,
    resolution_hash       TEXT NOT NULL,
    checks                JSONB NOT NULL,
    registry_sources      JSONB,
    settlement_signature  TEXT,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT oracle_verdicts_one_row_per_job UNIQUE (oracle_job_id)
);

CREATE INDEX IF NOT EXISTS idx_oracle_verdicts_resolution_hash
    ON oracle_verdicts (resolution_hash ASC);

CREATE TABLE IF NOT EXISTS oracle_lifecycle_events (
    id             BIGSERIAL PRIMARY KEY,
    oracle_job_id  BIGINT REFERENCES oracle_jobs (id) ON DELETE CASCADE,
    payment_uid    TEXT NOT NULL,
    event          TEXT NOT NULL,
    payload        JSONB,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_oracle_lifecycle_events_job
    ON oracle_lifecycle_events (oracle_job_id ASC, created_at ASC);

CREATE INDEX IF NOT EXISTS idx_oracle_lifecycle_events_payment_uid
    ON oracle_lifecycle_events (payment_uid ASC, created_at ASC);

CREATE INDEX IF NOT EXISTS idx_oracle_lifecycle_events_event
    ON oracle_lifecycle_events (event ASC);

CREATE TABLE IF NOT EXISTS oracle_parameters (
    id             BIGSERIAL PRIMARY KEY,
    param_name     TEXT NOT NULL,
    param_value    TEXT NOT NULL,
    inactive       BOOLEAN NOT NULL DEFAULT FALSE,
    effective_from TIMESTAMPTZ,
    expires_at     TIMESTAMPTZ,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX IF NOT EXISTS uniq_oracle_parameters_param_name
    ON oracle_parameters (param_name ASC);

