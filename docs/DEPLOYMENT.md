# Deploying oracle-qa on Ubuntu 24.04 (VPS)

`oracle-qa` is a **long-running** Tokio process (WebSocket log subscriber + HTTP API). It is **not** suited to Vercel-style serverless.

For a full Devnet validation before Mainnet/default-oracle advertising, follow [`DEVNET_E2E_RUNBOOK.md`](DEVNET_E2E_RUNBOOK.md).

## 1. Build on the server (or CI)

```bash
sudo apt update && sudo apt install -y build-essential pkg-config libssl-dev
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
cd oracle-qa && cargo build --release
```

Binary: `target/release/oracle-qa`

## 2. Systemd unit

Create `/etc/systemd/system/oracle-qa.service`:

```ini
[Unit]
Description=oracle-qa SLA-Escrow API quality oracle
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=oracle
Group=oracle
WorkingDirectory=/opt/oracle-qa
EnvironmentFile=/etc/oracle-qa.env
ExecStart=/opt/oracle-qa/oracle-qa
Restart=on-failure
RestartSec=5
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
```

Place the binary at `/opt/oracle-qa/oracle-qa`, copy `.env.example` to `/etc/oracle-qa.env`, set permissions (`chmod 600 /etc/oracle-qa.env`), then:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now oracle-qa
sudo systemctl status oracle-qa
```

## 3. Reverse proxy (optional)

Bind `BIND_ADDR=127.0.0.1:4020` and expose **HTTPS** with nginx or Caddy in front for `/health` and operator-only `/evaluate` (firewall or mTLS recommended for manual triggers).

## 4. Operational checklist

- Oracle keypair funded with SOL on the target cluster for `ConfirmOracle` fees.
- **PostgreSQL ledger:** run `psql "$DATABASE_URL" -f migrations/init.sql`, then set `DATABASE_URL` in `/etc/oracle-qa.env`. The ledger records `oracle_jobs`, `oracle_verdicts`, and append-only `oracle_lifecycle_events` for restart-safe dedupe and audits.
- **Manual evaluation auth:** set `ORACLE_OPERATOR_TOKEN_SHA256` (preferred) or `ORACLE_OPERATOR_TOKEN`; keep `ORACLE_ALLOW_UNAUTHENTICATED_MANUAL_EVALUATE=false` in production. Expose `POST /evaluate` only through mTLS, VPN, or an authenticated operator proxy.
- **CORS:** leave `ORACLE_CORS_ALLOWED_ORIGINS` unset for server-to-server operation, or set explicit operator console origins. Do not use permissive browser CORS for a production oracle key.
- **Evidence registry:** `EVIDENCE_REGISTRY_URL` or **`EVIDENCE_REGISTRY_URLS`** reachable from the VPS (same region reduces latency). Mirrors let you survive a single host outage; the oracle verifies **SHA-256(raw body)** before parsing JSON. Optional **`EVIDENCE_REGISTRY_AUTH_HEADER`** if the registry is not public read.
- **`EVIDENCE_FETCH_MAX_RETRIES`** / **`EVIDENCE_FETCH_RETRY_BASE_MS`** for transient 5xx or timeouts (still **fail closed** on hash mismatch).
- `ESCROW_PROGRAM_ID` matches the deployment buyers/sellers use with pr402.
- **Strict profile:** keep `ORACLE_STRICT_PROFILE=true` for the default API-quality oracle; only disable for legacy devnet artifacts.

### Chain worker behavior

- **Event-driven jobs:** The worker subscribes to program logs and decodes **`DeliverySubmittedEvent`** (program data / parsed instructions) to find the **payment PDA** where possible, with a fallback scan of transaction account keys if parsing is incomplete.
- **Dedupe:** While an evaluation is running for a given on-chain **`payment_uid`**, duplicate log lines for the same UID are skipped (the UID is released if the pipeline errors or times out so a later retry can run).
- **Single-writer expectation:** Running **two** oracle operator instances with the **same** keypair against the same payments can race `ConfirmOracle` and waste fees; use one primary worker or shard by program/deploy.
- **Dead letters:** failed jobs are retried while their payment UID is released for later events; after `ORACLE_DEAD_LETTER_MAX_ATTEMPTS`, the ledger marks them `dead_letter` for manual review.

### Health and observability

`GET /health` now reports RPC connectivity, WebSocket subscription state, last WebSocket message time, queue depth, registry reachability, oracle SOL balance, database enablement, and strict-profile mode. Alert on:

- `chain_connected=false`
- `websocket_connected=false`
- stale `last_websocket_message_at`
- low `oracle_balance_lamports`
- rising `total_errors` in `/stats`
- repeated `dead_letter` lifecycle events in Postgres

### Failover

Use one primary oracle worker per authority. A standby may run with the service stopped or with the oracle key unavailable. During failover, stop the primary, move or unlock the key on the standby, confirm the same `DATABASE_URL`, and start the standby. The on-chain `resolution_state` and the Postgres ledger together prevent ambiguous duplicate settlement state.

## 5. pr402 buyer/facilitator alignment (SLA-Escrow)

When buyers use **[pr402](https://github.com/miralandlabs/pr402)** to fund escrows this oracle resolves:

- **`paymentRequirements.accepted.extra`** (and the mirrored proof) should include **`beneficiary`** or **`merchantWallet`** so verify/build can encode **`FundPayment.seller`** correctly.
- **`facilitatorPaysTransactionFees: true`** on **`POST /api/v1/facilitator/build-sla-escrow-payment-tx`** is rejected unless the facilitator sets **`PR402_SLA_ESCROW_ALLOW_FACILITATOR_FEE_SPONSORSHIP`** (default off); omit the flag for buyer-paid Solana fees.
- **Payment mint allowlist:** pr402 enforces **`PR402_ALLOWED_PAYMENT_MINTS`** on SLA-Escrow **`/verify`**, **`/settle`**, and **`build-sla-escrow-payment-tx`** (same as **`exact`**). Ensure seller **`accepts[].asset`** and test escrows use an allowlisted SPL mint, or buyers fail before this oracle runs.
- **Agent discovery:** facilitators advertise **`/agent-payTo-semantics.json`** via **`GET /api/v1/facilitator/capabilities`** (`agentManifest.payToSemantics`) for `payTo` / mint-policy hints.
- **Default oracle hints:** the same **`/capabilities`** response may include **`slaEscrowOracleQa`** (profile id **`x402/oracle-qa/api-quality/v1`**, spec URL, optional advertised **`oracle_authority`** pubkey). Fund escrows with the **`oracle_authority`** you trust; do not rely on defaults without verification.
- **Scheme on proofs:** pr402 **`verifyBodyTemplate`** uses wire **`exact`** / **`sla-escrow`** in `scheme` fields; **`POST /verify`** / **`/settle`** also accept `v2:solana:*` aliases and normalize.
