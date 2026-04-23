# API Quality Profile — Version 1 (Normative)

**Profile identifier:** `x402/oracle-qa/api-quality/v1`  
**Document status:** Normative specification for the `oracle-qa` reference implementation  
**Scope:** Off-chain SLA documents and delivery evidence for **JSON-over-HTTP** API response quality adjudication.

---

## Abstract

This profile defines a **shared, finite rule set** for binding a buyer’s expectations and a seller’s fulfillment to cryptographic hashes (`sla_hash`, `delivery_hash`) while permitting an oracle to evaluate compliance **without per-seller custom code**. Sellers parameterize a fixed schema; the oracle applies a deterministic battery of checks aligned with the `sla-escrow` resolution vocabulary.

**Keywords:** service-level objective, content addressing, JSON Schema, oracle, escrow.

---

## 1. Introduction

Machine-facing commerce requires **explicit contracts** that are both human- and program-readable. The x402 stack commits only **hashes** on-chain; the present profile specifies **what those hashes authenticate** for one class of services: **responses from HTTP APIs returning JSON bodies**.

This document is **normative** for artifacts evaluated by `oracle-qa` at profile version `1`. Implementations may extend behavior off-spec; interoperability requires conformance to Sections 3–6.

---

## 2. Terminology


| Term                  | Definition                                                                                                      |
| --------------------- | --------------------------------------------------------------------------------------------------------------- |
| **SLA document**      | UTF-8 JSON object describing the agreed quality bounds for one payment.                                         |
| **Delivery evidence** | UTF-8 JSON object attesting the seller’s measured outcome (status, latency, body snapshot).                     |
| **Raw commitment**    | The exact octet sequence hashed; **no** implied canonicalization beyond stable UTF-8 encoding of the JSON text. |
| **Profile**           | A versioned rule family (`x402/oracle-qa/api-quality/v1`); version `1` matches schema major version below.      |


---

## 3. Cryptographic binding

Let `SHA256` denote the SHA-256 function on byte strings.

**SLA commitment.** The on-chain field `sla_hash` SHALL equal `SHA256(B_sla)` where `B_sla` is the **exact** UTF-8 encoding of the SLA JSON text made retrievable at the evidence registry path keyed by that hash.

**Delivery commitment.** The on-chain field `delivery_hash` SHALL equal `SHA256(B_del)` where `B_del` is the **exact** UTF-8 encoding of the delivery evidence JSON text similarly retrievable.

**Rationale.** Hashing **serialized bytes** (not a re-parse through an arbitrary serializer) ensures sellers, buyers, and oracles agree on the committed artifact—consistent with the x402 architecture overview’s separation of **off-chain payload** from **on-chain digest**.

---

## 4. SLA document

### 4.1 Schema

The SLA document MUST validate against `[schema/sla-document.schema.json](schema/sla-document.schema.json)`.

### 4.2 Semantics of fields


| Field                                | Role                                                                                                                                                                                                                                           |
| ------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `version`                            | **MUST** be `1` for this profile. Future profile revisions may increment.                                                                                                                                                                      |
| `endpoint`, `method`                 | **Declarative** metadata: which resource the parties intend (URI string, HTTP verb). This reference oracle **does not** replay HTTP requests; it evaluates **delivery evidence** only. These fields support audit, dispute, and agent routing. |
| `min_status_code`, `max_status_code` | Inclusive bounds; evidence `status_code` MUST lie in `[min_status_code, max_status_code]`.                                                                                                                                                     |
| `max_latency_ms`                     | Upper bound on reported latency; evidence `latency_ms` MUST NOT exceed it.                                                                                                                                                                     |
| `required_fields`                    | If non-empty, each listed name MUST appear as a key in `response_body` (JSON object) in the evidence.                                                                                                                                          |
| `response_schema`                    | If present, a **JSON Schema** (draft acceptable to the evaluator); evidence `response_body` MUST validate.                                                                                                                                     |
| `min_body_length`                    | If present, the UTF-8 length of the **canonical JSON serialization** of `response_body` used by the evaluator (stringify of the `response_body` value) MUST be ≥ this minimum.                                                                 |


Omitted keys use implementation defaults: `max_latency_ms` = 5000, `min_status_code` = 200, `max_status_code` = 299, `required_fields` = `[]` (see `[SlaDocument](../../src/types.rs)`). A **minimal** SLA contains only `version`, `endpoint`, and `method` — see `[examples/sla.tiny.json](examples/sla.tiny.json)`.

---

## 5. Delivery evidence

### 5.1 Schema

The delivery evidence MUST validate against `[schema/delivery-evidence.schema.json](schema/delivery-evidence.schema.json)`.

### 5.2 Semantics


| Field              | Role                                                                    |
| ------------------ | ----------------------------------------------------------------------- |
| `status_code`      | HTTP status code observed by the seller for the fulfilled call.         |
| `latency_ms`       | Non-negative measured latency in milliseconds.                          |
| `response_body`    | Parsed JSON value returned to the client (typically an object).         |
| `response_headers` | Optional map for audit; **not** used in core checks in v1.              |
| `timestamp`        | Unix epoch seconds when evidence was recorded (informative for audits). |


---

## 6. Evaluation semantics

Given validated SLA `S` and evidence `E`, the oracle computes a finite conjunction of checks **in fixed order**; failure of any check yields **rejection** with the first applicable resolution reason.


| Order | Check           | Predicate                                                                          | Typical `ResolutionReason` (on failure) |
| ----- | --------------- | ---------------------------------------------------------------------------------- | --------------------------------------- |
| 1     | Status          | `E.status_code ∈ [S.min_status_code, S.max_status_code]`                           | Status code out of range                |
| 2     | Latency         | `E.latency_ms ≤ S.max_latency_ms`                                                  | Latency exceeded                        |
| 3     | Required fields | For each `f` in `S.required_fields`, `E.response_body` is an object containing `f` | Required fields missing                 |
| 4     | JSON Schema     | If `S.response_schema` set, `E.response_body` validates                            | Schema validation failed                |
| 5     | Body length     | If `S.min_body_length` set, `len(serialize(E.response_body)) ≥ S.min_body_length`  | Body too short                          |


If all checks pass, the verdict is **approved** with reason **none**. Implementations MUST NOT approve if `SHA256` verification of raw bytes against on-chain hashes fails earlier in the pipeline.

---

## 7. Versioning and extensibility

- **Minor documentation fixes** do not change the profile identifier.
- **Breaking** changes (new required SLA keys, changed check semantics) require a new profile path (e.g. `…/v2`) and a bumped `version` field where applicable.

Sellers SHOULD declare `x402/oracle-qa/api-quality/v1` in marketplace or discovery metadata when this profile is intended.

---

## 8. References

- x402 architecture: `[ARCHITECTURE_OVERVIEW.md](../../../ARCHITECTURE_OVERVIEW.md)` — *Standardizing the SLA Hash & Delivery Hash*.
- Implementation: `[oracle-qa` source](../../) — `types.rs`, `evaluator.rs`, `pipeline.rs`.

---

## Appendix A. Informative: alignment with ecosystem guidance

The architecture overview suggests canonical JSON for hashing; **this profile** follows **raw UTF-8 octets** of the stored JSON text as the commitment input, which avoids serializer-dependent drift and matches the reference oracle’s integrity layer. Conceptually both approaches serve the same goal: **the hash is the fingerprint of the agreement and of the proof.**