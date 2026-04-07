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

1. **Chain Monitor** subscribes to SLA-Escrow program logs via Solana WebSocket (`logsSubscribe`). When a `DeliverySubmittedEvent` is detected, it reads the payment PDA to build an evaluation job.
2. **Pipeline** fetches the SLA document and delivery evidence from an off-chain registry (keyed by SHA256 hash), verifies hash integrity, then runs the evaluator.
3. **Evaluator** checks the delivery against SLA requirements:
  - HTTP status code within range
  - Response latency under threshold
  - Required fields present in response body
  - JSON Schema validation (if schema specified)
  - Minimum body length (if specified)
4. **Settler** builds and signs a `ConfirmOracle` transaction with the verdict (Approved/Rejected) and submits it to the chain.

## SLA Document Format

```json
{
  "version": 1,
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

The on-chain `sla_hash` = `SHA256(canonical_json(sla_document))`.

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

The on-chain `delivery_hash` = `SHA256(canonical_json(delivery_evidence))`.

## Quick Start

```bash
# 1. Generate oracle keypair
solana-keygen new -o ~/.config/solana/oracle-keypair.json

# 2. Fund oracle on Devnet
solana airdrop 2 $(solana-keygen pubkey ~/.config/solana/oracle-keypair.json) --url devnet

# 3. Configure
cp .env.example .env
# Edit .env: set ORACLE_KEYPAIR_PATH, ESCROW_PROGRAM_ID, EVIDENCE_REGISTRY_URL

# 4. Run
cargo run --release
```

## Configuration


| Variable                | Default                         | Description                            |
| ----------------------- | ------------------------------- | -------------------------------------- |
| `SOLANA_RPC_URL`        | `https://api.devnet.solana.com` | Solana JSON-RPC endpoint               |
| `SOLANA_WS_URL`         | `wss://api.devnet.solana.com`   | Solana WebSocket endpoint              |
| `ORACLE_KEYPAIR_PATH`   | *required*                      | Path to oracle operator keypair        |
| `ESCROW_PROGRAM_ID`     | Program default                 | SLA-Escrow program ID                  |
| `BIND_ADDR`             | `127.0.0.1:4020`                | HTTP server bind address               |
| `EVALUATION_TIMEOUT_MS` | `30000`                         | Max evaluation time per job            |
| `EVIDENCE_REGISTRY_URL` | `http://localhost:4021`         | Base URL for fetching evidence by hash |
| `RUST_LOG`              | `oracle_qa=info`                | Log level filter                       |


## Evidence Registry

The oracle fetches SLA documents and delivery evidence from an off-chain registry at:

```
GET {EVIDENCE_REGISTRY_URL}/{sha256_hex_hash}
```

The registry must return the JSON document whose SHA256 matches the requested hash. This can be:

- A simple key-value HTTP server
- IPFS gateway (with hash-to-CID mapping)
- Arweave gateway
- Any content-addressable storage

## HTTP API

### `GET /health`

```json
{
  "status": "healthy",
  "oracle_pubkey": "OracLe...",
  "program_id": "Escr4...",
  "chain_connected": true
}
```

### `GET /stats`

```json
{
  "total_evaluated": 42,
  "total_approved": 38,
  "total_rejected": 3,
  "total_errors": 1,
  "uptime_seconds": 86400,
  "last_evaluation_at": "2026-04-06T12:00:00Z"
}
```

### `POST /evaluate`

Manual evaluation trigger for a specific payment PDA.

```bash
curl -X POST http://localhost:4020/evaluate \
  -H "Content-Type: application/json" \
  -d '{"payment_pubkey": "PayMeNt..."}'
```

## For Oracle Developers

This project serves as the **reference implementation** for building x402 SLA-Escrow oracles. To create your own domain-specific oracle:

1. Fork this project
2. Replace the `Evaluator` with your domain logic
3. Keep the chain monitor, pipeline, and settler modules
4. Define your own SLA document schema for your domain

The evaluation interface is intentionally simple: given an SLA document and delivery evidence, return approved/rejected with check details.

## License

Apache-2.0