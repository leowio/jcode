# Shared OpenAI OAuth account storage

`JCODE_SHARED_OPENAI_AUTH_HOME` opts otherwise isolated Jcode runtimes into one
native OpenAI account store, `<directory>/openai-auth.json`. All OpenAI account
reads, mutations, and rotated-token writes use that store and its existing
`.openai-auth.json.lock`. Other credentials, sessions, memories, runtime caches,
and configuration retain their own `JCODE_HOME`.

The opt-in directory must be absolute, user-owned, a real directory, and mode
0700. Its account file must be an owned private regular file without symlinks
or extra hard links. Initialize this native account store once using an
explicitly authorized current account; do not continuously copy independently
rotating credentials between runtimes. Native account expiry values are Unix
milliseconds, whereas JWT `exp` values are seconds.

A second persistent `.openai-refresh.lock` covers the entire network grant,
latest-account reload, and token persistence. It uses an owned Send file guard
acquired on a blocking task, so it can remain held across the async HTTP call.
The existing cancellation-safe refresh transaction owns this guard: dropping
the caller does not release it before the response is persisted. Waiters reload
the shared account after acquiring the lock and reuse a token already refreshed
by another process. The lock wait is bounded at 120 seconds; the HTTP exchange
retains its existing deadline. Never delete a lock sidecar while runtimes live.

The shared OAuth store is authoritative when opted in: expired canonical OAuth
credentials do not fall through to old private legacy OAuth copies. Legacy
Codex files are not modified by this feature. Independent applications which
also retain the same rotating refresh token are outside this lock protocol and
should not independently refresh copied credentials.

Unix is required for the shared override. Without it, existing account storage
paths remain unchanged; Unix standalone runtimes also receive cross-process
refresh serialization within their own home.

Tests in `crates/jcode-base/src/auth/shared_openai_auth_tests.rs` run two separate
private-home processes against a local mock endpoint and require exactly one
refresh grant, canonical persistence, and no private account files. They also
reject unsafe directories, credential symlinks, and refresh-lock symlinks.
