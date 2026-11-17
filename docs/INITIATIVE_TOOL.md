# Native initiative tool

Set `JCODE_ENABLE_INITIATIVE=1` on the daemon process to expose the retained
native `initiative` tool to the model. New tool registries read the opt-in
independently of the shared base-tool cache. It is disabled by default.

The tool can create, list, show, resume, update, checkpoint, and focus durable
initiatives, including their milestones, progress, blockers, and next steps.
It uses the existing native goal storage; no data migration is needed.

The `/initiatives`, `/initiatives show <id>`, and `/initiatives resume` user
commands remain available independently of model tool registration. Existing
tool profiles and explicit tool-disable settings still apply.
