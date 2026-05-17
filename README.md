# oracle-qa — DEPRECATED (superseded by `oracles/oracle-api-quality`)

> **This project has been renamed and refactored** into the multi-category oracle workspace at [`miraland-labs/oracles`](https://github.com/miraland-labs/oracles), the standalone sibling repo to this hub.
> 
> - **New canonical crate:** `oracle-api-quality` in the [`oracles`](https://github.com/miraland-labs/oracles) workspace — same JSON-shaped SLA evaluator, now sharing chain-monitoring / registration / settlement / ledger code with two sibling oracles via `oracle-common`.
> - **New canonical profile id:** `x402/oracles/api-quality/v1` (replaces `x402/oracle-qa/api-quality/v1`).
> - **DB schema:** identical tables (`oracle_jobs`, `oracle_verdicts`, `oracle_lifecycle_events`, `oracle_parameters`) plus four new tables; run `psql "$DATABASE_URL" -f oracle-common/migrations/init.sql` from the new workspace.
> - **Operator script paths:** `scripts/install.sh api-quality ...` in the new workspace replaces the manual `/etc/systemd/system/oracle-qa.service` recipe in `docs/DEPLOYMENT.md`.
> - **All env vars** the old `oracle-qa` consumed are honored unchanged: `EVIDENCE_REGISTRY_URL` (singular), `EVIDENCE_REGISTRY_URLS` (mirrors), `ORACLE_OPERATOR_TOKEN_SHA256`, `X-Oracle-Token` header, `ORACLE_BACKFILL_LOOKBACK_SIGNATURES`, `ORACLE_REQUIRE_EVENT_MATCH`, `ORACLE_STRICT_PROFILE`, `ORACLE_DEAD_LETTER_MAX_ATTEMPTS`, etc.
>
> This directory is kept as an archive for historical reference and is **not built or tested**. It will be deleted in a subsequent cleanup.

---

## Original README (archived below for reference)

# oracle-qa — API Response Quality Oracle

The **first official oracle** for the x402 SLA-Escrow ecosystem.

`oracle-qa` is a standalone Tokio/Axum service that monitors the SLA-Escrow on-chain program for delivery submissions, evaluates API response quality against SLA contracts, and settles verdicts on-chain via `ConfirmOracle`.

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                        oracle-qa                             │
│                                                              │
│  ┌─────────────┐    ┌─────────────┐    ┌─────────────────┐  │
│  │  Chain       │───>│  Pipeline   │───>│  Settler        │  │
│  │  Monitor     │    │             │    │                 │  │
│  │  (WebSocket) │    │  1. Fetch   │    │  ConfirmOracle  │  │
│  │              │    │  2. Eval    │    │  tx → chain     │  │
│  └─────────────┘    └─────────────┘    └─────────────────┘  │
│         │                  │                                 │
│         │           ┌──────┴──────┐                          │
│         │           │  Evaluator  │                          │
│         │           │  - Status   │                          │
│         │           │  - Latency  │                          │
│         │           │  - Schema   │                          │
│         │           │  - Fields   │                          │
│         │           └─────────────┘                          │
│                                                              │
│  ┌─────────────────────────────────────────────────────────┐ │
│  │  HTTP API (Axum)                                        │ │
│  │  GET  /         — service info                          │ │
│  │  GET  /health   — chain connectivity check              │ │
│  │  GET  /stats    — evaluation statistics                 │ │
│  │  POST /evaluate — manual evaluation trigger             │ │
│  └─────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────┘
         │                                         │
    Solana Devnet                          Evidence Registry
    (WebSocket logs)                       (SLA docs + delivery
                                            evidence by hash)
```

## How It Works

1. **Chain Monitor** subscribes to SLA-Escrow program logs via Solana WebSocket (`logsSubscribe`). When a **`DeliverySubmittedEvent`** is detected (parsed from **program data** / transaction metadata where available), it derives the **payment PDA** and builds an evaluation job; duplicate **`payment_uid`** values are ignored while a job is in flight (see [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md)).
2. **Pipeline** fetches the SLA document and delivery evidence from an off-chain registry (keyed by SHA256 hash), verifies hash integrity, then runs the evaluator.
3. **Evaluator** checks the delivery against SLA requirements:
  - HTTP status code within range
  - Response latency under threshold
  - Required fields present in response body
  - JSON Schema validation (if schema specified)
  - Minimum body length (if specified)
4. **Settler** builds and signs a `ConfirmOracle` transaction with the verdict (Approved/Rejected) and submits it to the chain.

## Formal specification (shared SLA rules)

Interoperability requires a **published profile**, not ad-hoc JSON per seller. See **[`spec/README.md`](spec/README.md)** — profile **`x402/oracle-qa/api-quality/v1`** with a normative document, JSON Schemas, and examples under [`spec/api-quality-v1/`](spec/api-quality-v1/NORMATIVE.md). SLAs may include optional **`profile_id`** so the wrong oracle binary cannot silently apply the wrong rule set (evaluator rejects a mismatch).

### Trust model (bootstrap default oracle)

This oracle implements **hash-bound SLA compliance on seller-attested delivery snapshots**: on-chain commits bind **`SHA256(SLA bytes)`** and **`SHA256(delivery bytes)`**; evaluation checks whether the **fetched** delivery JSON satisfies the **fetched** SLA rules (status, latency, schema, fields, length). **Integrity of committed bytes** is cryptographic; **truth of the underlying HTTP call** is **not** proven unless you add attestations, reputation, monitoring, or a stronger profile (see [`spec/signed-delivery-v2/DRAFT.md`](spec/signed-delivery-v2/DRAFT.md)).

### When not to use oracle-qa (as the sole arbiter)

- **High-value or adversarial counterparties** where forged off-chain JSON is a material risk without extra controls.
- **Regulated or third-party attestations** that must bind delivery to a specific TLS session or independent witness.
- **Domain semantics** beyond JSON shape and declared metrics (use a **domain oracle** or fork the evaluator).

In those cases, treat this deployment as a **reference / bootstrap** and operate a purpose-built oracle or add verification tiers per the spec drafts.

## SLA Document Format

```json
{
  "version": 1,
  "profile_id": "x402/oracle-qa/api-quality/v1",
  "endpoint": "https://api.example.com/v1/data",
  "method": "GET",
  "response_schema": {
    "type": "object",
    "required": ["result", "status"]
  },
  "required_fields": ["result", "status"],
  "max_latency_ms": 5000,
  "min_status_code": 200,
  "max_status_code": 299,
  "min_body_length": 100
}
```

The on-chain `sla_hash` = `SHA256(raw_bytes)` where **raw_bytes** are the exact file bytes you upload to the evidence registry (see [Evidence Registry](#evidence-registry)). Same rule for `delivery_hash`.

## Delivery Evidence Format

```json
{
  "status_code": 200,
  "latency_ms": 342,
  "response_body": { "result": "data", "status": "ok" },
  "response_headers": { "content-type": "application/json" },
  "timestamp": 1712345678
}
```

Use the **same byte sequence** for hashing that the registry will return on `GET` (typically UTF-8 JSON file bytes, minified or pretty—must be identical).

## Quick Start

```bash
# 1. Generate oracle keypair
solana-keygen new -o ~/.config/solana/oracle-keypair.json

# 2. Fund oracle on Devnet
solana airdrop 2 $(solana-keygen pubkey ~/.config/solana/oracle-keypair.json) --url devnet

# 3. Configure
cp .env.example .env
# Edit .env: set ORACLE_KEYPAIR_PATH, ESCROW_PROGRAM_ID, EVIDENCE_REGISTRY_URL (or EVIDENCE_REGISTRY_URLS)

# 4. Run
cargo run --release
```

## Production readiness defaults

For a public default oracle, run with:

- `DATABASE_URL` enabled and initialized with [`migrations/init.sql`](migrations/init.sql) so job state, verdicts, and lifecycle events survive restarts.
- `ORACLE_OPERATOR_TOKEN_SHA256` configured so `POST /evaluate` is operator-only. Leave `ORACLE_ALLOW_UNAUTHENTICATED_MANUAL_EVALUATE=false` outside local demos.
- `ORACLE_STRICT_PROFILE=true` so SLA documents must declare `version: 1` and `profile_id: "x402/oracle-qa/api-quality/v1"`.
- `ORACLE_CORS_ALLOWED_ORIGINS` restricted to trusted operator consoles, or unset when the HTTP API is only accessed server-to-server.

`ConfirmOracle` uses a non-zero `resolution_hash`: a deterministic SHA-256 fingerprint over the payment UID, SLA hash, delivery hash, SLA profile/version, approval result, resolution reason, and check results. The full evidence remains off-chain, while the chain stores an audit handle that operators and counterparties can recompute.

Before advertising an oracle authority through `pr402` capabilities, complete the Devnet validation in [`docs/DEVNET_E2E_RUNBOOK.md`](docs/DEVNET_E2E_RUNBOOK.md).

## Configuration


| Variable                | Default                         | Description                            |
| ----------------------- | ------------------------------- | -------------------------------------- |
| `SOLANA_RPC_URL`        | `https://api.devnet.solana.com` | Solana JSON-RPC endpoint               |
| `SOLANA_WS_URL`         | `wss://api.devnet.solana.com`   | Solana WebSocket endpoint              |
| `ORACLE_KEYPAIR_PATH`   | *required*                      | Path to oracle operator keypair        |
| `ESCROW_PROGRAM_ID`     | Program default                 | SLA-Escrow program ID                  |
| `BIND_ADDR`             | `127.0.0.1:4020`                | HTTP server bind address               |
| `EVALUATION_TIMEOUT_MS` | `30000`                         | Max evaluation time per job            |
| `EVIDENCE_REGISTRY_URL` | `http://localhost:4021`       | Single registry base URL (legacy; used when `EVIDENCE_REGISTRY_URLS` is unset) |
| `EVIDENCE_REGISTRY_URLS` | *(unset)*                    | Comma-separated mirror base URLs; tried **in order** per artifact; first **hash-valid** response wins |
| `EVIDENCE_REGISTRY_AUTH_HEADER` | *(unset)*             | Optional `Authorization` header value on registry GET (e.g. `Bearer …`) |
| `EVIDENCE_FETCH_MAX_RETRIES` | `3`                      | Per-URL retries for transient HTTP failures (still fail closed on hash mismatch) |
| `EVIDENCE_FETCH_RETRY_BASE_MS` | `200`                  | Exponential backoff base delay            |
| `DATABASE_URL`           | *(unset)*                       | Optional PostgreSQL ledger; run `migrations/init.sql` first |
| `ORACLE_OPERATOR_TOKEN_SHA256` | *(unset)*               | SHA-256 hex digest for operator-only `POST /evaluate` |
| `ORACLE_OPERATOR_TOKEN`  | *(unset)*                       | Plain token convenience; prefer the digest form in production |
| `ORACLE_ALLOW_UNAUTHENTICATED_MANUAL_EVALUATE` | `false` | Local-only escape hatch for manual evaluation without auth |
| `ORACLE_CORS_ALLOWED_ORIGINS` | *(unset)*                | Comma-separated browser origins allowed to call the HTTP API |
| `ORACLE_STRICT_PROFILE`  | `true`                          | Require API quality profile id and version `1` |
| `ORACLE_DEAD_LETTER_MAX_ATTEMPTS` | `5`                 | Stop automatic retries after this many worker attempts |
| `ORACLE_JOB_CHANNEL_CAPACITY` | `256`                   | Bounded chain-monitor-to-worker queue size |
| `ORACLE_REQUIRE_EVENT_MATCH` | `false`                    | Refuse to emit jobs unless the tx carries a matching `DeliverySubmittedEvent` (recommended on Mainnet) |
| `ORACLE_BACKFILL_LOOKBACK_SIGNATURES` | `2000`            | Max signatures scanned on startup to recover deliveries missed while offline (`0` disables) |
| `RUST_LOG`              | `oracle_qa=info`                | Log level filter                       |


## Evidence Registry

The oracle fetches SLA documents and delivery evidence from an off-chain registry. With one base URL:

```
GET {EVIDENCE_REGISTRY_URL}/{sha256_hex_hash}
```

With **`EVIDENCE_REGISTRY_URLS`**, the same path is tried against each base URL in order until a response’s **raw body** hashes to the committed value.

The response **body bytes** must satisfy `SHA256(body) ==` the 32-byte hash committed on-chain (oracle verifies **raw bytes** before parsing JSON). Parties should:

1. **Agree** on a JSON schema for SLA + evidence (this repo documents example shapes).
2. **Compute** `sha256` over the **exact** file bytes to be hosted.
3. **Upload** those bytes to the registry at the path above.

Suitable backends: nginx `alias` of static files named by hash, S3 object keyed by hash, IPFS (content id matches hash only when CID is raw-leaf compatible—use a pinning flow that checks the hash), or a small internal service.

**Three-party read access:** The registry must be **readable by the oracle** (HTTP GET). Buyers and sellers usually obtain the same URLs **out-of-band** (embedded in your marketplace API, x402 `resource` metadata, or a shared manifest). The chain only stores **hashes**—not URLs—so document where humans/agents fetch the full payload.

## HTTP API

### `GET /health`

Reports live chain/WebSocket/DB/registry status plus the monitor's last observed slot.

```json
{
  "status": "healthy",
  "oracle_pubkey": "OracLe...",
  "program_id": "Escr4...",
  "chain_connected": true,
  "websocket_connected": true,
  "last_seen_slot": 287654321,
  "deliveries_observed": 42,
  "registry_reachable": true
}
```

### `GET /stats`

```json
{
  "total_evaluated": 42,
  "total_approved": 38,
  "total_rejected": 3,
  "total_errors": 1,
  "total_dead_letter": 0,
  "total_evidence_fetch_failures": 0,
  "uptime_seconds": 86400,
  "last_evaluation_at": "2026-04-06T12:00:00Z"
}
```

### `GET /metrics`

Prometheus text exposition (`text/plain; version=0.0.4`). Emits counters and gauges for
`total_evaluated`, `total_approved`, `total_rejected`, `total_errors`, `total_dead_letter`,
`total_evidence_fetch_failures`, `queue_depth`, `websocket_connected`,
`deliveries_observed`, `last_seen_slot`, and process `uptime_seconds`.

```bash
curl -sS http://127.0.0.1:4020/metrics
```

### `POST /evaluate`

Manual evaluation trigger for a specific payment PDA.

Production deployments require `Authorization: Bearer <operator-token>` or `X-Oracle-Token: <operator-token>` unless explicitly configured otherwise.

```bash
curl -X POST http://localhost:4020/evaluate \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $ORACLE_OPERATOR_TOKEN" \
  -d '{"payment_pubkey": "PayMeNt..."}'
```

## For Oracle Developers

This project serves as the **reference implementation** for building x402 SLA-Escrow oracles. To create your own domain-specific oracle:

1. Fork this project
2. Implement the [`QualityOracle`](src/evaluator.rs) trait on your own type (`profile_id` + `evaluate`) — the chain monitor, evidence fetcher, settler, ledger, and HTTP surface remain the same
3. Wire your implementation into [`pipeline.rs`](src/pipeline.rs) in place of the default `Evaluator`
4. Define your own SLA document schema (and, optionally, a new `profile_id` + JSON Schema under `spec/`)

The evaluation interface is intentionally simple: given an SLA document and delivery evidence, return an `EvaluationResult` with `approved` + `resolution_reason` + itemized `checks`.

## Operational characteristics

- **Restart-safe dedupe** — with `DATABASE_URL` set, the worker checks the ledger for a terminal state (`settled` / `dead_letter`) before re-running. No in-memory `HashSet` resets on restart; duplicate log events are absorbed cheaply.
- **Startup backfill** — on launch, `oracle-qa` scans the last `ORACLE_BACKFILL_LOOKBACK_SIGNATURES` program signatures via `getSignaturesForAddress`, decodes any matching `SubmitDelivery` instructions, and emits evaluation jobs for deliveries that landed while this oracle was offline. The ledger's `oracle_parameters.chain.last_seen_slot` watermark bounds the scan on subsequent restarts.
- **Strict event matching** — set `ORACLE_REQUIRE_EVENT_MATCH=true` to refuse emitting a job from a log notification unless the transaction carries a matching `DeliverySubmittedEvent` for the same `payment_uid` + `delivery_hash`. Recommended on Mainnet; disable only for RPC providers that truncate program-data.
- **Shared clients** — a single `reqwest::Client` and `Arc<RpcClient>` are held in `AppState` and used across the chain monitor, pipeline, evaluator, settler, and HTTP handlers. No per-request connection pool churn.
- **On-chain clock** — `is_eligible` reads the Solana `Clock` sysvar rather than the wall clock before submitting `ConfirmOracle`; this keeps eligibility consistent with what the program observes when the tx lands.

## License

Apache-2.0