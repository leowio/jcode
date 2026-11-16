//! Last-mile transient recovery for runtimes without their own request retry
//! loop. Only the explicit unlimited mode enables this layer, so ordinary
//! finite provider budgets are never multiplied by an outer retry budget.

use crate::message::{ConnectionPhase, Message, StreamEvent, ToolDefinition};
use crate::provider::{EventStream, Provider};
use anyhow::Result;
use futures::StreamExt;
use jcode_provider_core::retry_policy::RetryPolicy;
use std::sync::Arc;
use std::time::Duration;

pub(super) async fn complete_split(
    provider: Arc<dyn Provider>,
    messages: &[Message],
    tools: &[ToolDefinition],
    system_static: &str,
    system_dynamic: &str,
    resume_session_id: Option<&str>,
) -> Result<EventStream> {
    let config = crate::config::config();
    if config.provider.max_retries != u32::MAX {
        return provider
            .complete_split(
                messages,
                tools,
                system_static,
                system_dynamic,
                resume_session_id,
            )
            .await;
    }
    let policy = RetryPolicy::new(
        config.provider.max_retries,
        Duration::from_secs(config.provider.retry_backoff_cap_secs.max(1)),
    );
    Ok(retrying_stream(
        provider,
        messages.to_vec(),
        tools.to_vec(),
        system_static.to_owned(),
        system_dynamic.to_owned(),
        resume_session_id.map(str::to_owned),
        policy,
    ))
}

enum Failure {
    Error(anyhow::Error),
    Event {
        message: String,
        retry_after_secs: Option<u64>,
    },
}

impl Failure {
    fn message(&self) -> String {
        match self {
            Self::Error(error) => format!("{error:#}"),
            Self::Event { message, .. } => message.clone(),
        }
    }

    fn retry_after(&self, policy: RetryPolicy) -> Option<Duration> {
        match self {
            Self::Error(error) => policy.retry_after_from_error(error),
            Self::Event {
                retry_after_secs, ..
            } => retry_after_secs.map(|seconds| {
                let hint = jcode_provider_core::retry_after::RetryAfter::from_delay(
                    Duration::from_secs(seconds),
                );
                if policy.is_unlimited() {
                    hint.remaining_unbounded()
                } else {
                    hint.remaining()
                }
            }),
        }
    }

    fn into_item(self) -> Result<StreamEvent> {
        match self {
            Self::Error(error) => Err(error),
            Self::Event {
                message,
                retry_after_secs,
            } => Ok(StreamEvent::Error {
                message,
                retry_after_secs,
            }),
        }
    }
}

// The stream owns its request snapshot. There is no detached task: dropping
// the returned stream cancels its current request/read/backoff immediately.
#[allow(clippy::too_many_arguments)]
fn retrying_stream(
    provider: Arc<dyn Provider>,
    messages: Vec<Message>,
    tools: Vec<ToolDefinition>,
    system_static: String,
    system_dynamic: String,
    resume_session_id: Option<String>,
    policy: RetryPolicy,
) -> EventStream {
    Box::pin(async_stream::stream! {
        let mut next_delay = None;
        for attempt in policy.attempts() {
            if attempt > 0 {
                yield Ok(StreamEvent::ConnectionPhase {
                    phase: ConnectionPhase::Retrying {
                        attempt: attempt.saturating_add(1),
                        max: policy.max_attempts(),
                    },
                });
                tokio::time::sleep(policy.delay(attempt, next_delay.take(), 1000)).await;
            }
            let mut saw_output = false;
            let mut unsafe_to_replay = false;
            let failure = match provider.complete_split(
                &messages, &tools, &system_static, &system_dynamic, resume_session_id.as_deref(),
            ).await {
                Err(error) => Failure::Error(error),
                Ok(mut inner) => {
                    // Opaque SDK/CLI runtimes can perform tools internally
                    // between output events. Unlike HTTP function definitions,
                    // their partial output cannot prove replay has no effects.
                    let opaque_live_runtime = has_opaque_live_tools(provider.as_ref());
                    let mut failure = None;
                    while let Some(item) = inner.next().await {
                        match item {
                            Err(error) => {
                                failure = Some(Failure::Error(error));
                                break;
                            }
                            Ok(StreamEvent::Error { message, retry_after_secs }) => {
                                failure = Some(Failure::Event { message, retry_after_secs });
                                break;
                            }
                            Ok(event) => {
                                // These bridges execute tools during the stream,
                                // unlike ordinary HTTP tool-call definitions.
                                let replay_visible = jcode_provider_core::attempt_tracker::stream_event_is_replay_visible(&event);
                                unsafe_to_replay |= (opaque_live_runtime && replay_visible) || matches!(
                                    event,
                                    StreamEvent::NativeToolCall { .. } | StreamEvent::ToolResult { .. }
                                );
                                if matches!(event, StreamEvent::RetryRollback { .. }) {
                                    saw_output = false;
                                } else {
                                    saw_output |= replay_visible;
                                }
                                yield Ok(event);
                            }
                        }
                    }
                    let Some(failure) = failure else { return; };
                    failure
                }
            };
            let message = failure.message();
            if !is_retryable(&message) || !policy.allows_retry_after(attempt) {
                yield failure.into_item();
                return;
            }
            if unsafe_to_replay {
                yield Err(anyhow::anyhow!(
                    "unsafe_to_replay: provider output may include live tool effects; resume manually to avoid repeating side effects. {message}"
                ));
                return;
            }
            if saw_output {
                yield Ok(StreamEvent::RetryRollback {
                    attempt: attempt.saturating_add(2),
                    max: policy.max_attempts(),
                });
            }
            next_delay = failure.retry_after(policy);
            crate::logging::warn(&format!(
                "Transient provider error; retrying request without resending the user prompt: {message}"
            ));
        }
    })
}

fn has_opaque_live_tools(provider: &dyn Provider) -> bool {
    // MultiProvider's OpenRouter slot can host Grok Build; use its active
    // display label as well as the stable routing name.
    [
        provider.name().to_ascii_lowercase(),
        provider.display_name().to_ascii_lowercase(),
    ]
    .iter()
    .any(|name| {
        name.contains("cursor")
            || name.contains("grok")
            || name == "jcode"
            || name == "jcode subscription"
    })
}

fn contains_status(message: &str, status: &str) -> bool {
    message.match_indices(status).any(|(start, _)| {
        let before = message[..start].chars().next_back();
        let after = message[start + status.len()..].chars().next();
        !before.is_some_and(|c| c.is_ascii_alphanumeric())
            && !after.is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

fn is_retryable(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    if lower.trim_start().starts_with("[jcode-provider-failover]")
        || jcode_provider_core::is_definitive_usage_exhaustion(message)
        || crate::auth::refresh_state::error_is_permanent_rejection(message)
        || ["400", "401", "402", "403", "404", "413", "422"]
            .iter()
            .any(|code| contains_status(&lower, code))
        || [
            "unsafe_to_replay",
            "unauthorized",
            "permission_denied",
            "accessdenied",
            "authentication_error",
            "invalid api key",
            "invalid x-api-key",
            "invalid_grant",
            "credentials have been revoked",
            "not configured",
            "insufficient_quota",
            "insufficient credit",
            "billing",
            "payment required",
            "quota exhausted",
            "usage_limit_reached",
            "invalid_request",
            "invalid request",
            "validationexception",
            "validation error",
            "invalid schema",
            "invalid tool",
            "unsupported parameter",
            "model_not_found",
            "context length",
            "context window",
            "context_length",
            "too many tokens",
            "prompt is too long",
            "input is too long",
            "content_policy",
            "safety_violation",
            "guardrail",
            "interrupted by user",
            "cancelled",
            "canceled",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return false;
    }
    jcode_provider_core::is_transient_transport_error(&lower)
        || ["429", "500", "502", "503", "504", "529"]
            .iter()
            .any(|code| contains_status(&lower, code))
        || [
            "rate limit",
            "rate_limit",
            "ratelimit",
            "throttlingexception",
            "throttled",
            "too many requests",
            "overloaded",
            "service unavailable",
            "serviceunavailable",
            "internal server error",
            "internalserverexception",
            "server_error",
            "api_error",
            "stream disconnected before completion",
            "ended before message completion marker",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
}

#[cfg(test)]
#[path = "provider_retry_tests.rs"]
mod tests;
