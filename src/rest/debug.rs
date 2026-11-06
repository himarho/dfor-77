use std::fmt;

use super::{AttachmentUpload, RestOutcome, RestRequest, RestResponse};

impl fmt::Debug for AttachmentUpload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttachmentUpload")
            .field("file_name_bytes", &self.file_name.len())
            .field("file_bytes", &self.file_bytes.len())
            .finish()
    }
}

impl fmt::Debug for RestRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RestRequest")
            .field("method", &self.method)
            .field("route_template", &self.route_template)
            .field("action", &self.action)
            .field("path_bytes", &self.path.len())
            .field("body_bytes", &self.body_bytes().len())
            .field("has_json_body", &self.has_json_body())
            .field("has_audit_reason", &self.audit_reason_header.is_some())
            .field("has_nonce", &self.nonce.is_some())
            .finish()
    }
}

impl fmt::Debug for RestResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RestResponse")
            .field("http_status", &self.http_status)
            .field("discord_error_code", &self.discord_error_code)
            .field("body_bytes", &self.body.len())
            .field("header_count", &self.headers.len())
            .finish()
    }
}

impl fmt::Debug for RestOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RestOutcome")
            .field("status", &self.status)
            .field("commit_state", &self.commit_state)
            .field("request", &self.request)
            .field("http_status", &self.http_status)
            .field("discord_error_code", &self.discord_error_code)
            .field("has_created_message_id", &self.created_message_id.is_some())
            .field(
                "response_bytes",
                &self.response_body.as_ref().map(String::len),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rest::{HttpMethod, MockRestBackend, RestBackend, RestPlanner};

    #[test]
    fn request_outcome_and_mock_diagnostics_do_not_reveal_secret_paths_or_payloads() {
        let mut planner = RestPlanner::new();
        let payload = r#"{"content":"private-content","token":"a-short-secret"}"#;
        for planned in [
            planner.interaction_callback("10", "a-short-secret", payload),
            planner.raw_json_request(
                HttpMethod::Post,
                "/api/v10/webhooks/10/a-short-secret?private=query-secret",
                Some(payload),
                "reason-secret",
            ),
        ] {
            let mut outcome = planned;
            outcome.response_body = Some("response-secret".into());
            outcome.why = "reason-secret".into();
            let request = outcome.request.as_ref().unwrap();
            let mut backend = MockRestBackend::default();
            backend.execute(request).unwrap();
            let output = format!("{request:?} {outcome:#?} {backend:#?}");
            for secret in [
                "a-short-secret",
                "private-content",
                "query-secret",
                "reason-secret",
                "response-secret",
            ] {
                assert!(!output.contains(secret), "{output}");
            }
            assert!(output.contains("Post"));
            assert!(output.contains("body_bytes"));
            assert!(output.len() < 4096);
        }
    }

    #[test]
    fn binary_upload_and_response_diagnostics_remain_small_and_exclude_contents() {
        let upload = AttachmentUpload {
            file_name: "private-name".into(),
            file_bytes: vec![255; 1024 * 1024],
        };
        let mut response = RestResponse::ok();
        response.body = "response-secret".repeat(100_000);
        response
            .headers
            .insert("Set-Cookie".into(), "cookie-secret".into());
        let output = format!("{upload:#?} {response:?}");
        for secret in ["private-name", "255", "response-secret", "cookie-secret"] {
            assert!(!output.contains(secret), "{output}");
        }
        assert!(output.len() < 512);
        assert_eq!(upload.file_bytes.len(), 1024 * 1024);
        assert!(response.body.starts_with("response-secret"));
    }
}
