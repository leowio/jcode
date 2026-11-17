# Managed system tool catalogs

`JCODE_ENABLE_INITIATIVE=1` registers the retained native `initiative` tool for
new registries. The default remains off. This opt-in is applied outside the
process-wide base-tool cache, so constructing a registry without the flag does
not accidentally cache initiatives off for later managed construction. Native
initiatives use the existing durable goal/checkpoint store, including the
repository scope selected by `JCODE_SHARED_PROJECT_HOME`.

`JCODE_SYSTEM_MCP_ONLY=1` treats `$JCODE_HOME/mcp.json` as the complete MCP source
for that runtime. It skips one-time Codex migration, live Claude user files,
Claude per-project entries, and all project-local MCP files. This matters for a
router whose working directory is the operator home: `HOME/.jcode/mcp.json`
would otherwise be loaded as a project-local override even when its own private
`JCODE_HOME` uses a centrally generated catalog. Missing or malformed private
catalogs yield an empty catalog rather than importing a different source.
Environment expansion and unsupported-transport filtering still apply.

Both flags are explicit opt-ins; ordinary Jcode runtimes retain their existing
tool exposure and MCP configuration precedence.
