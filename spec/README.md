# Oracle SLA specifications (`oracle-qa`)

This directory holds **published, versioned rule families** for the reference oracle implementation. Each profile is a **small closed world**: sellers instantiate parameters inside the schema; the oracle evaluates only what it implements—**simple, predictable, and auditable**.

## Design philosophy

**Simple is best, yet elegant.** A profile should be:

- **Normative** — enough precision that two independent implementations can agree on pass/fail for the same inputs.
- **Minimal** — no vocabulary beyond what evaluation requires; resist turning the SLA into a general-purpose policy language.
- **Hash-committed** — the on-chain `sla_hash` / `delivery_hash` bind **exact off-chain octets** (see each profile’s *Normative* document).

## Profiles


| Profile                          | Identifier                      | Status                                                                                                |
| -------------------------------- | ------------------------------- | ----------------------------------------------------------------------------------------------------- |
| API response quality (HTTP JSON) | `x402/oracle-qa/api-quality/v1` | **Current** — matches `[src/types.rs](../src/types.rs)` and `[src/evaluator.rs](../src/evaluator.rs)` |


Start with `**[api-quality-v1/NORMATIVE.md](api-quality-v1/NORMATIVE.md)`** and the JSON Schemas under `api-quality-v1/schema/`.

## Relation to x402 architecture

The ecosystem-level intent—**SHA-256 over committed artifacts**, oracle as bridge between off-chain evidence and on-chain `ConfirmOracle`—is described in `[ARCHITECTURE_OVERVIEW.md](../../ARCHITECTURE_OVERVIEW.md)` (*Standardizing the SLA Hash & Delivery Hash*). Profiles here **specialize** that story for machine-checkable API quality: field names and evaluation semantics are fixed so sellers and oracles share one **interoperability surface**, not one-off bespoke rules per merchant.