use crate::{
    error::OracleError,
    types::{CheckResult, DeliveryEvidence, EvaluationResult, SlaDocument},
};

pub struct Evaluator;

impl Evaluator {
    /// Run all SLA compliance checks against the delivery evidence.
    pub fn evaluate(
        sla: &SlaDocument,
        evidence: &DeliveryEvidence,
    ) -> Result<EvaluationResult, OracleError> {
        let mut checks = Vec::new();

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
