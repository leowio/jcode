# Account prompts with an external gateway

For an external gateway that owns account authentication and routing, Jcode can
use a named OpenAI-compatible provider profile and suppress unrelated account
prompts:

```toml
[display]
show_account_prompts = false

[provider]
default_provider = "my-gateway"
model_picker_providers = ["my-gateway"]
cross_provider_failover = "manual"
same_provider_account_failover = false

[providers.my-gateway]
type = "open-ai-compatible"
base_url = "http://127.0.0.1:2455/v1"
auth = "bearer"
api_key_env = "MY_GATEWAY_API_KEY"
env_file = "my-gateway.env"
default_model = "my-model"
requires_api_key = true

[[providers.my-gateway.models]]
id = "my-model"
```

Keep the key in the user-level env file, not the repository. The gateway still
requires its normal API-key authentication.

`display.show_account_prompts` defaults to `true` for compatibility. Setting it
to `false` hides startup account-router/login hints and unsolicited hosted-model
account nudges. It does not suppress authentication failures, disable security
checks, or remove explicit `/login`, `/account`, and `/hosted` commands. The flag
is read when the header or nudge is evaluated. An older client binary needs an
upgrade to recognize it.

Changing the default provider affects new routing decisions. An existing session
may retain its selected provider. Start a new session or explicitly select the
gateway model to migrate that session. Do not restart a shared server or erase
saved provider credentials merely to hide UI prompts.

## Separate gateway speed profiles

For gateways such as Codex LB that accept `service_tier`, use separate named
profiles instead of the native provider's speed toggle. Clone the gateway
profile, keeping its URL, credential env file, and model definitions unchanged,
and give each clone an explicit name and extra request body:

| Profile | `extra_body.service_tier` | Meaning |
| --- | --- | --- |
| `codex-lb-standard` | `"default"` | Standard, the default route |
| `codex-lb-fast` | `"priority"` | Explicit Fast request |
| `codex-lb-ultrafast` | `"ultrafast"` | Explicit Ultrafast request |

Set `provider.default_provider` to `"codex-lb-standard"`, and include these three
profile names in `provider.model_picker_providers`. Keep any configured
`agents.swarm_model` on the Standard profile unless explicitly requesting a
different worker mode. Do not include unsupported models in the Ultrafast
profile. Keep automatic cross-provider failover disabled with `"manual"`.

Choose a qualified model in `/model`, for example
`codex-lb-fast:gpt-6.1-sol`, or start a client explicitly:

```sh
jcode --provider-profile codex-lb-standard
jcode --provider-profile codex-lb-fast
jcode --provider-profile codex-lb-ultrafast
```

These names identify the requested tier, not a guaranteed upstream grant or
speed. Inspect the gateway's requested and actual service-tier fields after a
request. A successful Ultrafast-profile request whose actual tier is `default`
does not establish that Ultrafast was granted. The native provider's speed
toggle does not override these profiles' fixed extra request body.
