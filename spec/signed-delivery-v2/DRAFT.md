# Signed delivery evidence — draft profile (v2 direction)

**Status:** Draft / design — **not** implemented in `oracle-qa` yet.  
**Motivation:** `api-quality/v1` evaluates **seller-attested** JSON; a stronger default reduces casual forgery without requiring the oracle to replay HTTP.

## Goals

1. Bind delivery evidence to the **seller’s** signing key (expected to match on-chain `FundPayment.seller` / merchant identity for the payment).
2. Keep evaluation **deterministic** and **hash-committed** (`delivery_hash` still = `SHA256(raw_bytes)` of the serialized delivery artifact).
3. Remain compatible with the existing pipeline shape: fetch by hash → verify bytes → evaluate.

## Proposed artifact extensions (informative)

Extend delivery JSON with optional (or required in v2) fields:

| Field | Role |
| ----- | ---- |
| `message_digest` | SHA-256 hex or base58 of a canonical message (see below). |
| `seller_pubkey` | Base58 Solana pubkey of the signer (must match escrow seller / attest policy). |
| `seller_signature` | Ed25519 signature (base64 or base58) over `message_digest`’s raw 32 bytes (or over a prefixed domain-separated message). |

**Message to sign (sketch):** `SHA256("oracle-qa/v2/delivery\0" ‖ payment_uid (32) ‖ delivery_hash (32) ‖ canonical_body_hash)` where `canonical_body_hash` commits to the semantic payload (e.g. status, latency, stable serialization of `response_body`).

## Oracle behavior (future)

1. After byte-level `SHA256` matches on-chain `delivery_hash`, parse JSON.
2. Recompute `message_digest` / verify domain separation matches policy.
3. Verify Ed25519 signature with `seller_pubkey` against the agreed message.
4. Ensure `seller_pubkey` matches the payment’s seller (read from chain or trusted manifest per deployment policy).
5. Run existing v1 checks (status, latency, schema, …).

## Versioning

- Ship as **`x402/oracle-qa/signed-delivery/v2`** (or similar) with a bumped `version` field in the SLA document.
- V1 remains available for low-friction integrations.

## References

- [`api-quality-v1/NORMATIVE.md`](../api-quality-v1/NORMATIVE.md) — trust model for v1.
- SLA-Escrow program: `DeliverySubmittedEvent`, `ConfirmOracle` (`sla-escrow` workspace).
