use super::*;

pub(super) fn should_refresh_token(status: StatusCode, body: &str) -> bool {
    if status == StatusCode::UNAUTHORIZED {
        return true;
    }
    if status == StatusCode::FORBIDDEN {
        let lower = body.to_lowercase();
        return lower.contains("token")
            || lower.contains("expired")
            || lower.contains("unauthorized");
    }
    false
}

pub(super) fn maybe_record_runtime_model_unavailable_from_stream_error(
    catalog_scope: &str,
    model: &str,
    message: &str,
) {
    let reason = classify_unavailable_model_error(StatusCode::BAD_REQUEST, message)
        .or_else(|| classify_unavailable_model_error(StatusCode::FORBIDDEN, message));

    if let Some(reason) = reason {
        jcode_base::provider::record_model_unavailable_for_scope(catalog_scope, model, &reason);
        jcode_base::logging::warn(&format!(
            "Recorded OpenAI model '{}' as unavailable from stream error: {}",
            model, reason
        ));
    }
}

pub(super) fn classify_unavailable_model_error(status: StatusCode, body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();

    let mentions_model = lower.contains("model")
        || lower.contains("slug")
        || lower.contains("engine")
        || lower.contains("deployment");
    let unavailable = lower.contains("not available")
        || lower.contains("unavailable")
        || lower.contains("does not have access")
        || lower.contains("not enabled")
        || lower.contains("not found")
        || lower.contains("unknown model")
        || lower.contains("unsupported model")
        || lower.contains("invalid model");

    if !mentions_model || !unavailable {
        return None;
    }

    if status == StatusCode::NOT_FOUND
        || status == StatusCode::FORBIDDEN
        || status == StatusCode::BAD_REQUEST
        || status == StatusCode::UNPROCESSABLE_ENTITY
    {
        let trimmed = body.trim();
        let reason = if trimmed.is_empty() {
            format!("model denied by OpenAI API (status {})", status)
        } else {
            format!(
                "model denied by OpenAI API (status {}): {}",
                status, trimmed
            )
        };
        return Some(reason);
    }

    None
}

/// Check if an error is transient and should be retried
pub(in super::super) fn is_retryable_error(error_str: &str) -> bool {
    // Explicit in-place credential rotation reconnects with a new credential.
    // Deterministic failures otherwise win over any
    // incidental timeout/overload/rate-limit wording in the same error.
    if error_str.contains("openai token refreshed, retrying")
        || error_str.contains("openai credentials changed before request, retrying")
    {
        return true;
    }
    const PERMANENT: &[&str] = &[
        "unauthorized",
        "forbidden",
        "invalid api key",
        "invalid_api_key",
        "invalid x-api-key",
        "authentication_error",
        "permission_denied",
        "invalid_grant",
        "refresh token has been revoked",
        "refresh token is revoked",
        "refresh token already used",
        "refresh_token_reused",
        "refresh token expired",
        "insufficient_quota",
        "usage_limit",
        "usage limit",
        "exceeded your current quota",
        "insufficient credits",
        "credit balance",
        "billing_hard_limit",
        "billing_not_active",
        "quota exhausted",
        "invalid_request_error",
        "invalid_function_parameters",
        "invalid schema",
        "invalid tool",
        "context_length_exceeded",
        "maximum context length",
        "context window exceeded",
        "string_above_max_length",
        "model_not_found",
        "model_not_supported",
        "unsupported model",
        "content_policy_violation",
        "safety_violation",
        "cancelled",
        "canceled",
        "interrupted by user",
    ];
    if PERMANENT.iter().any(|marker| error_str.contains(marker)) {
        return false;
    }
    if [400, 401, 403, 404, 405, 410, 413, 415, 422]
        .iter()
        .any(|status| {
            [
                "api error ",
                "http ",
                "http status ",
                "status ",
                "status: ",
                "status=",
            ]
            .iter()
            .any(|prefix| {
                let marker = format!("{prefix}{status}");
                error_str.match_indices(&marker).any(|(start, _)| {
                    error_str[start + marker.len()..]
                        .chars()
                        .next()
                        .is_none_or(|after| !after.is_ascii_digit())
                })
            })
        })
    {
        return false;
    }
    // Shared transport-layer classifier used by every other provider. This
    // covers transient TLS/network faults (connection reset/closed/refused/
    // aborted, broken pipe, timeouts, unexpected EOF, error decoding/reading,
    // TLS BadRecordMac / fatal-alert, TLS handshake EOF, DNS/route failures,
    // and HTTP/2 stream/protocol faults). Keeping the OpenAI path delegated
    // here ensures retry behavior is unified across providers (issue #338).
    jcode_provider_core::is_transient_transport_error(error_str)
        // OpenAI-specific transport wrapper.
        || error_str.contains("failed to send request to openai api")
        // Stream/decode errors specific to the OpenAI streaming runtime.
        || error_str.contains("incomplete message")
        || error_str.contains("stream disconnected before completion")
        || error_str.contains("ended before message completion marker")
        || error_str.contains("falling back from websockets to https transport")
        // Server errors (5xx)
        || error_str.contains("500 internal server error")
        || error_str.contains("502 bad gateway")
        || error_str.contains("503 service unavailable")
        || error_str.contains("504 gateway timeout")
        || error_str.contains("overloaded")
        || error_str.contains("service_unavailable")
        || error_str.contains("gateway_timeout")
        || error_str.contains("too_many_requests")
        // Rate limiting (429): transient, recovers on retry. Unified with the
        // other providers (Anthropic/Copilot) which already retry these.
        || error_str.contains("429 too many requests")
        || error_str.contains("rate limit")
        || error_str.contains("rate_limit")
        // API-level server errors
        || error_str.contains("api_error")
        || error_str.contains("server_error")
        || error_str.contains("internal server error")
        || error_str.contains("an error occurred while processing your request")
        || error_str.contains("please include the request id")
        // Auth: we just force-refreshed the OpenAI token in place and want the
        // retry loop to reconnect with the fresh credentials.
        || error_str.contains("openai token refreshed, retrying")
        || error_str.contains("openai credentials changed before request, retrying")
}
