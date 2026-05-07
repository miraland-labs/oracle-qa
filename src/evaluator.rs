use crate::{
    error::OracleError,
    types::{
        CheckResult, DeliveryEvidence, EvaluationResult, SlaDocument, API_QUALITY_V1_PROFILE_ID,
    },
};

pub struct Evaluator;

impl Evaluator {
    /// Run all SLA compliance checks against the delivery evidence.
    pub fn evaluate(
        sla: &SlaDocument,
        evidence: &DeliveryEvidence,
        strict_profile: bool,
    ) -> Result<EvaluationResult, OracleError> {
        let mut checks = Vec::new();

        let version_ok = sla.version == 1;
        if strict_profile || !version_ok {
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
        } else if strict_profile {
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
}
