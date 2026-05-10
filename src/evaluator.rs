use crate::{
    error::OracleError,
    types::{
        CheckResult, DeliveryEvidence, EvaluationResult, SlaDocument, API_QUALITY_V1_PROFILE_ID,
    },
};

/// Trait describing an oracle's evaluation contract.
///
/// Forks that want to build a domain-specific oracle on top of the rest of the oracle-qa
/// pipeline (chain monitor, evidence fetcher, settler, Postgres ledger, operator HTTP
/// surface) should implement this trait on their own struct and wire it into the pipeline.
/// The profile id serves both as a machine-readable identifier for the rule family and as
/// a guard when SLA documents declare a `profile_id` field.
#[allow(dead_code)]
pub trait QualityOracle: Send + Sync {
    /// Stable profile identifier, e.g. `x402/oracle-qa/api-quality/v1`.
    fn profile_id(&self) -> &'static str;

    /// Evaluate a validated SLA document against validated delivery evidence.
    /// Implementations should return a deterministic `EvaluationResult` whose
    /// `resolution_reason` aligns with the `sla_escrow_api::resolution::ResolutionReason`
    /// vocabulary so on-chain verdicts remain auditable.
    fn evaluate(
        &self,
        sla: &SlaDocument,
        evidence: &DeliveryEvidence,
    ) -> Result<EvaluationResult, OracleError>;
}

/// Default reference evaluator: the API-quality profile v1.
///
/// Implements the deterministic battery of checks documented in
/// `spec/api-quality-v1/NORMATIVE.md`. Wrap in `Arc` / keep as a short-lived value per
/// job — it is trivially cloneable and holds no state beyond the strict-profile flag.
#[derive(Clone, Copy)]
pub struct Evaluator {
    strict_profile: bool,
}

impl Evaluator {
    pub fn new(strict_profile: bool) -> Self {
        Self { strict_profile }
    }

    /// Pure helper preserved for ad-hoc callers and older tests.
    #[allow(dead_code)]
    pub fn evaluate(
        sla: &SlaDocument,
        evidence: &DeliveryEvidence,
        strict_profile: bool,
    ) -> Result<EvaluationResult, OracleError> {
        Self { strict_profile }.evaluate_impl(sla, evidence)
    }

    fn evaluate_impl(
        &self,
        sla: &SlaDocument,
        evidence: &DeliveryEvidence,
    ) -> Result<EvaluationResult, OracleError> {
        let mut checks = Vec::new();

        let version_ok = sla.version == 1;
        if self.strict_profile || !version_ok {
            checks.push(CheckResult {
                name: "version".into(),
                passed: version_ok,
                detail: if version_ok {
                    "1".into()
                } else {
                    format!("expected 1, got {}", sla.version)
                },
            });
        }

        if let Some(pid) = &sla.profile_id {
            let ok = pid == API_QUALITY_V1_PROFILE_ID;
            checks.push(CheckResult {
                name: "profile_id".into(),
                passed: ok,
                detail: if ok {
                    API_QUALITY_V1_PROFILE_ID.into()
                } else {
                    format!("expected '{}', got '{}'", API_QUALITY_V1_PROFILE_ID, pid)
                },
            });
        } else if self.strict_profile {
            checks.push(CheckResult {
                name: "profile_id".into(),
                passed: false,
                detail: format!("missing; expected '{}'", API_QUALITY_V1_PROFILE_ID),
            });
        }

        // Check 1: HTTP status code range
        let status_ok = evidence.status_code >= sla.min_status_code
            && evidence.status_code <= sla.max_status_code;
        checks.push(CheckResult {
            name: "status_code".into(),
            passed: status_ok,
            detail: format!(
                "Got {} (expected {}-{})",
                evidence.status_code, sla.min_status_code, sla.max_status_code
            ),
        });

        // Check 2: Response latency
        let latency_ok = evidence.latency_ms <= sla.max_latency_ms;
        checks.push(CheckResult {
            name: "latency".into(),
            passed: latency_ok,
            detail: format!("{}ms (max {}ms)", evidence.latency_ms, sla.max_latency_ms),
        });

        // Check 3: Required fields present in response body
        if !sla.required_fields.is_empty() {
            let body_obj = evidence.response_body.as_object();
            for field in &sla.required_fields {
                let present = body_obj.map(|obj| obj.contains_key(field)).unwrap_or(false);
                checks.push(CheckResult {
                    name: format!("required_field:{}", field),
                    passed: present,
                    detail: if present {
                        "present".into()
                    } else {
                        "missing".into()
                    },
                });
            }
        }

        // Check 4: JSON Schema validation (if schema provided)
        if let Some(schema_value) = &sla.response_schema {
            match jsonschema::validator_for(schema_value) {
                Ok(validator) => {
                    let valid = validator.is_valid(&evidence.response_body);
                    checks.push(CheckResult {
                        name: "json_schema".into(),
                        passed: valid,
                        detail: if valid {
                            "valid".into()
                        } else {
                            let errors: Vec<String> = validator
                                .iter_errors(&evidence.response_body)
                                .take(3)
                                .map(|e| e.to_string())
                                .collect();
                            format!("invalid: {}", errors.join("; "))
                        },
                    });
                }
                Err(e) => {
                    checks.push(CheckResult {
                        name: "json_schema".into(),
                        passed: false,
                        detail: format!("schema compile error: {}", e),
                    });
                }
            }
        }

        // Check 5: Minimum body length (if specified)
        if let Some(min_len) = sla.min_body_length {
            let body_str = serde_json::to_string(&evidence.response_body).unwrap_or_default();
            let len_ok = body_str.len() >= min_len;
            checks.push(CheckResult {
                name: "body_length".into(),
                passed: len_ok,
                detail: format!("{} bytes (min {})", body_str.len(), min_len),
            });
        }

        let approved = checks.iter().all(|c| c.passed);

        use sla_escrow_api::resolution::ResolutionReason;

        let resolution_reason: u16 = if approved {
            ResolutionReason::None.into()
        } else {
            checks
                .iter()
                .find(|c| !c.passed)
                .map(|c| match c.name.as_str() {
                    "profile_id" => ResolutionReason::GeneralRejection,
                    "version" => ResolutionReason::GeneralRejection,
                    "status_code" => ResolutionReason::StatusCodeOutOfRange,
                    "latency" => ResolutionReason::LatencyExceeded,
                    "json_schema" => ResolutionReason::SchemaValidationFailed,
                    "body_length" => ResolutionReason::BodyTooShort,
                    name if name.starts_with("required_field:") => {
                        ResolutionReason::RequiredFieldsMissing
                    }
                    _ => ResolutionReason::GeneralRejection,
                })
                .unwrap_or(ResolutionReason::GeneralRejection)
                .into()
        };

        Ok(EvaluationResult {
            approved,
            resolution_reason,
            checks,
        })
    }
}

impl QualityOracle for Evaluator {
    fn profile_id(&self) -> &'static str {
        API_QUALITY_V1_PROFILE_ID
    }

    fn evaluate(
        &self,
        sla: &SlaDocument,
        evidence: &DeliveryEvidence,
    ) -> Result<EvaluationResult, OracleError> {
        self.evaluate_impl(sla, evidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DeliveryEvidence, SlaDocument, API_QUALITY_V1_PROFILE_ID};

    fn valid_sla() -> SlaDocument {
        SlaDocument {
            version: 1,
            profile_id: Some(API_QUALITY_V1_PROFILE_ID.into()),
            endpoint: "https://api.example.test/data".into(),
            method: "GET".into(),
            response_schema: None,
            required_fields: vec!["result".into()],
            max_latency_ms: 500,
            min_status_code: 200,
            max_status_code: 299,
            min_body_length: None,
        }
    }

    fn valid_evidence() -> DeliveryEvidence {
        DeliveryEvidence {
            status_code: 200,
            latency_ms: 42,
            response_body: serde_json::json!({ "result": "ok" }),
            response_headers: None,
            timestamp: 1_700_000_000,
        }
    }

    #[test]
    fn strict_profile_requires_profile_id() {
        let mut sla = valid_sla();
        sla.profile_id = None;

        let result = Evaluator::evaluate(&sla, &valid_evidence(), true).unwrap();

        assert!(!result.approved);
        assert!(result
            .checks
            .iter()
            .any(|check| check.name == "profile_id" && !check.passed));
    }

    #[test]
    fn strict_profile_requires_version_one() {
        let mut sla = valid_sla();
        sla.version = 2;

        let result = Evaluator::evaluate(&sla, &valid_evidence(), true).unwrap();

        assert!(!result.approved);
        assert!(result
            .checks
            .iter()
            .any(|check| check.name == "version" && !check.passed));
    }

    #[test]
    fn valid_evidence_approves() {
        let result = Evaluator::evaluate(&valid_sla(), &valid_evidence(), true).unwrap();

        assert!(result.approved);
    }

    #[test]
    fn trait_and_direct_call_match() {
        let oracle = Evaluator::new(true);
        let via_trait =
            <Evaluator as QualityOracle>::evaluate(&oracle, &valid_sla(), &valid_evidence())
                .unwrap();
        let via_direct = Evaluator::evaluate(&valid_sla(), &valid_evidence(), true).unwrap();
        assert_eq!(via_trait.approved, via_direct.approved);
        assert_eq!(via_trait.resolution_reason, via_direct.resolution_reason);
    }
}
