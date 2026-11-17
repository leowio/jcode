//! Model routing helpers for the Copilot API: reasoning efforts, endpoint
//! selection, and retryable error classification.

/// Reasoning efforts supported by Copilot's claude-sonnet-5 route,
/// per live `/models` capabilities (issue #558).
pub(crate) const SONNET5_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Efforts for `model`: catalog-advertised levels when known, else the
/// claude-sonnet-5 fallback used before the catalog is available.
pub(crate) fn copilot_model_efforts(
    catalog: &std::collections::HashMap<String, Vec<String>>,
    model: &str,
) -> Vec<String> {
    if let Some(levels) = catalog.get(model) {
        return levels.clone();
    }
    if model == "claude-sonnet-5" {
        SONNET5_EFFORTS.iter().map(|e| e.to_string()).collect()
    } else {
        Vec::new()
    }
}

pub(crate) fn copilot_model_uses_responses_api(model: &str) -> bool {
    model.trim().to_ascii_lowercase().starts_with("gpt-5.6")
}

pub(crate) fn copilot_api_path(uses_responses_api: bool) -> &'static str {
    if uses_responses_api {
        "responses"
    } else {
        "chat/completions"
    }
}

pub(crate) fn is_retryable_error(error_str: &str) -> bool {
    // Explicit protocol failures take precedence over suggestive words in an
    // untrusted response body; an invalid login/request never becomes an
    // endless retry just because the body also mentions overload or timeout.
    if let Some(rest) = error_str
        .split_once("copilot api error (http ")
        .map(|(_, rest)| rest)
    {
        let code = rest
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        match code.parse::<u16>().ok() {
            Some(408 | 429 | 500 | 502 | 503 | 504 | 529) => return true,
            Some(507) => {
                return error_str.contains("exceeded request buffer limit while retrying upstream");
            }
            Some(400..=599) => return false,
            _ => {}
        }
    }
    if [
        "authentication_error",
        "permission_error",
        "invalid_request_error",
        "insufficient_quota",
    ]
    .iter()
    .any(|kind| error_str.contains(kind))
    {
        return false;
    }

    jcode_provider_core::is_transient_transport_error(error_str)
        || error_str.contains("500 internal server error")
        || error_str.contains("502 bad gateway")
        || error_str.contains("503 service unavailable")
        || error_str.contains("504 gateway timeout")
        || error_str.contains("overloaded")
        || error_str.contains("429 too many requests")
        || error_str.contains("rate limit")
        || error_str.contains("rate_limit")
        || error_str.contains("stream error")
        || error_str.contains("stream read timeout")
}

#[cfg(test)]
mod retry_classification_tests {
    use super::is_retryable_error;

    #[test]
    fn protocol_status_takes_precedence_over_response_body() {
        for (error, expected) in [
            (
                "Copilot API error (HTTP 401 Unauthorized): rate limit overloaded timeout",
                false,
            ),
            (
                "Copilot API error (HTTP 400 Bad Request): rate limit",
                false,
            ),
            ("Copilot API error (HTTP 408 Request Timeout): {}", true),
            ("Copilot API error (HTTP 429 Too Many Requests): {}", true),
            ("Copilot API error (HTTP 503 Service Unavailable): {}", true),
            (
                "Copilot API error (HTTP 501 Not Implemented): overloaded",
                false,
            ),
        ] {
            assert_eq!(
                is_retryable_error(&error.to_ascii_lowercase()),
                expected,
                "{error}"
            );
        }
    }
}
