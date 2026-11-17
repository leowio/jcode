# Transient retry recovery

Provider requests retry transient network, overload, and rate-limit failures
until success or cancellation by default:

```toml
[provider]
max_retries = 0
retry_backoff_cap_secs = 30
```

`JCODE_MAX_RETRIES` and `JCODE_RETRY_BACKOFF_CAP_SECS` override these values.
Zero means unlimited; the previous `4294967295` unlimited setting remains
supported. A positive finite value limits provider request attempts, including
the initial request. It does not impose a total budget on client recovery.

The TUI retains failed turns for automatic recovery after classified transient
failures, stalled streams, and recoverable rate limits. These continuations
remain retryable until cancelled, even if an individual provider request has
an explicit finite attempt budget. Backoff is capped by the configured delay;
a valid server `Retry-After` hint can request a longer wait.

Permanent credential, billing, request, model, and policy failures stop recovery.
Explicit small budgets for deterministic recovery and provider switching remain
bounded. Native-agent outcomes that may already have changed external state
require reconciliation before replay. User cancellation drops pending recovery.

Provider retry loops roll back replay-visible partial output before restarting
an attempt. The agent's generic recovery layer covers runtimes without a native
retry loop in unlimited mode and refuses to replay observed live tool effects.
