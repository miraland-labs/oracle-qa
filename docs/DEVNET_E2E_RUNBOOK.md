# oracle-qa Devnet E2E Runbook

This runbook validates `oracle-qa` as the default API-quality oracle for x402 `sla-escrow` before advertising it through `pr402` production capabilities.

## Goal

Prove this lifecycle on Devnet:

```text
Buyer funds SLA-Escrow via pr402
Seller submits hash-bound delivery evidence
oracle-qa detects the delivery event
oracle-qa fetches SLA + delivery bytes
oracle-qa evaluates x402/oracle-qa/api-quality/v1
oracle-qa submits ConfirmOracle with non-zero resolution_hash
Postgres ledger records job, verdict, and lifecycle events
```

## Preconditions

- `oracle-qa` compiled successfully.
- PostgreSQL is reachable from the oracle host.
- SLA-Escrow Devnet program id is known.
- The oracle keypair is funded with Devnet SOL.
- The same `oracle_authority` is included in `pr402` `ORACLE_AUTHORITIES`.
- Evidence registry can serve immutable bytes at `GET {base}/{sha256_hex}`.

## 1. Initialize Oracle Ledger

```bash
cd oracle-qa
psql "$DATABASE_URL" -f migrations/init.sql
```

Sanity check:

```bash
psql "$DATABASE_URL" -c '\dt oracle_*'
```

Expected tables:

- `oracle_jobs`
- `oracle_verdicts`
- `oracle_lifecycle_events`
- `oracle_parameters`

## 2. Configure oracle-qa

Use a real random operator token in production-like testing.

```bash
export SOLANA_RPC_URL="https://api.devnet.solana.com"
export SOLANA_WS_URL="wss://api.devnet.solana.com"
export ESCROW_PROGRAM_ID="<DEVNET_SLA_ESCROW_PROGRAM_ID>"
export ORACLE_KEYPAIR_PATH="$HOME/.config/solana/oracle-keypair.json"
export EVIDENCE_REGISTRY_URL="https://your-registry.example.com/evidence"
export DATABASE_URL="postgres://..."
export ORACLE_STRICT_PROFILE=true
export ORACLE_OPERATOR_TOKEN="replace-with-random-token"
export ORACLE_CORS_ALLOWED_ORIGINS=""
export RUST_LOG="oracle_qa=info,tower_http=info"
```

Start the service:

```bash
cargo run --release
```

## 3. Smoke Test HTTP Guardrails

Health should expose chain, WebSocket, registry, DB, queue, and strict-profile state:

```bash
curl -sS http://127.0.0.1:4020/health | jq .
```

Manual evaluation without auth should fail:

```bash
curl -sS -X POST http://127.0.0.1:4020/evaluate \
  -H "Content-Type: application/json" \
  -d '{"payment_pubkey":"11111111111111111111111111111111"}' | jq .
```

Manual evaluation with auth should pass auth and then fail only because the dummy payment is not assigned:

```bash
curl -sS -X POST http://127.0.0.1:4020/evaluate \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $ORACLE_OPERATOR_TOKEN" \
  -d '{"payment_pubkey":"11111111111111111111111111111111"}' | jq .
```

## 4. Configure pr402 Preview

On the Devnet facilitator deployment, set:

```bash
export ESCROW_PROGRAM_ID="<DEVNET_SLA_ESCROW_PROGRAM_ID>"
export ORACLE_AUTHORITIES="<ORACLE_PUBKEY>"
export PR402_DEFAULT_SLA_ORACLE_PUBKEY="<ORACLE_PUBKEY>"
export PR402_ORACLE_QA_SPEC_URL="https://github.com/miraland-labs/oracle-qa/blob/main/spec/api-quality-v1/NORMATIVE.md"
export PR402_ORACLE_EVIDENCE_REGISTRY_NOTE="SLA and delivery artifacts are fetched by SHA-256 hex path from the deployment evidence registry."
```

Confirm capability advertisement:

```bash
BASE="https://preview.ipay.sh"
curl -sS "$BASE/api/v1/facilitator/capabilities" | jq '.features, .slaEscrowOracleQa'
```

Expected:

- `.features.slaEscrow == true`
- `.slaEscrowOracleQa.profileId == "x402/oracle-qa/api-quality/v1"`
- `.slaEscrowOracleQa.defaultOperatorPubkey == "<ORACLE_PUBKEY>"`

## 5. Create Hash-Bound SLA And Delivery Artifacts

Example SLA:

```json
{
  "version": 1,
  "profile_id": "x402/oracle-qa/api-quality/v1",
  "endpoint": "https://seller.example.com/api/premium",
  "method": "GET",
  "required_fields": ["result"],
  "max_latency_ms": 5000,
  "min_status_code": 200,
  "max_status_code": 299
}
```

Example delivery:

```json
{
  "status_code": 200,
  "latency_ms": 250,
  "response_body": { "result": "ok" },
  "response_headers": { "content-type": "application/json" },
  "timestamp": 1770000000
}
```

Hash the exact bytes you upload:

```bash
shasum -a 256 sla.json
shasum -a 256 delivery.json
```

Upload each file to:

```text
{EVIDENCE_REGISTRY_URL}/{sha256_hex}
```

Before funding, verify the registry returns byte-identical content:

```bash
curl -sS "$EVIDENCE_REGISTRY_URL/<SLA_HASH>" | shasum -a 256
curl -sS "$EVIDENCE_REGISTRY_URL/<DELIVERY_HASH>" | shasum -a 256
```

## 6. Fund SLA-Escrow Through pr402

Use the buyer starter or direct builder flow:

```bash
curl -sS -X POST "$BASE/api/v1/facilitator/build-sla-escrow-payment-tx" \
  -H "Content-Type: application/json" \
  -d '{
    "payer": "<BUYER_PUBKEY>",
    "accepted": <SELLER_ACCEPTS_LINE>,
    "resource": <RESOURCE_FROM_402>,
    "slaHash": "<SLA_HASH>",
    "oracleAuthority": "<ORACLE_PUBKEY>"
  }' | jq .
```

Sign the returned transaction, fill `verifyBodyTemplate.paymentPayload.payload.transaction`, then:

```bash
curl -sS -X POST "$BASE/api/v1/facilitator/verify" \
  -H "Content-Type: application/json" \
  -d @verify-body.json | jq .

curl -sS -X POST "$BASE/api/v1/facilitator/settle" \
  -H "Content-Type: application/json" \
  -d @verify-body.json | jq .
```

Record the payment PDA / UID from your SLA-Escrow tooling or transaction logs.

## 7. Submit Delivery

Use the SLA-Escrow submit-delivery flow/tooling with:

```text
payment = <PAYMENT_PDA>
delivery_hash = <DELIVERY_HASH>
```

Once the transaction lands, `oracle-qa` should detect the log subscription event.

## 8. Verify Oracle Resolution

Watch service logs for:

```text
New delivery detected
Pipeline started
Evaluation APPROVED
Settlement confirmed
```

Check ledger:

```bash
psql "$DATABASE_URL" -c "select payment_uid,status,attempts,settlement_signature,resolution_hash from oracle_jobs order by updated_at desc limit 5;"
psql "$DATABASE_URL" -c "select approved,resolution_reason,resolution_hash,settlement_signature from oracle_verdicts order by created_at desc limit 5;"
psql "$DATABASE_URL" -c "select payment_uid,event,created_at from oracle_lifecycle_events order by created_at desc limit 20;"
```

Expected:

- `oracle_jobs.status = settled`
- `oracle_verdicts.approved = true`
- `resolution_hash` is 64 hex characters and not all zeros
- lifecycle includes `detected`, `queued`, `started`, `settled`

## 9. Failure Cases To Test

Before Mainnet defaulting, repeat with:

- wrong `profile_id`: should reject
- `version: 2`: should reject in strict mode
- missing required field: should reject
- registry hash mismatch: should fail closed and not approve
- no operator token on `/evaluate`: should reject
- oracle restart before delivery: after restart, logs/manual evaluate should still write coherent ledger state

## 10. Default Oracle Gate

Only advertise `oracle-qa` as the default production oracle when:

- three consecutive Devnet escrow flows approve correctly
- three negative cases reject correctly
- ledger rows are complete and queryable
- `/health` shows chain, WebSocket, registry, and database healthy
- the oracle keypair funding and rotation runbook is ready
- `pr402` capabilities show the intended `defaultOperatorPubkey`

