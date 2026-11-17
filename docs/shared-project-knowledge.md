# Shared project knowledge

Set `JCODE_SHARED_PROJECT_HOME` to an absolute directory to share native project
memory and initiatives between otherwise isolated Jcode homes. Without it, every
existing path and scope keeps its previous behavior. Non-Git working directories
also keep private project storage.

Repository identity uses the origin URL, normalized across HTTPS/SSH and stripped
of credentials, query strings, and fragments. GitHub/GitLab repository names are
case-normalized. Repositories without an origin use the canonical Git common
directory. Linked worktrees, subdirectories, and clones with the same origin use
the same repository key. Separate origins use separate stores.

Each repository directory is `<root>/v1-<FNV-1a-64(identity)>/` and contains:

- `repository.json`: the version and credential-free identity (collision checked)
- `memory.json`: Jcode's native project memory graph
- `goals/*.json`: Jcode's native durable initiatives
- `memory.lock`, `goals.lock`: cross-process transaction locks

Global memory, global initiatives, session goal attachments, credentials, logs,
and session databases remain under each process's own `JCODE_HOME`. Session
attachments recognize a repository across worktrees while still belonging to
their original private session home.

Native project memory mutation operations lock the full read-modify-write cycle.
Reads that normalize/import/migrate the graph take the same reentrant lock;
shared graphs bypass the process-local cache. Goal create/update operations lock
ID allocation and checkpoint updates before synchronizing their memory mirror.
Do not implement external edits by independently loading and then overwriting a
snapshot: use native mutation APIs, or hold the corresponding transaction lock.

Existing private files are never moved, deleted, or implicitly merged. Before
activation, back them up and explicitly merge project graphs and initiatives
into the matching repository directory, preserving IDs/checkpoints and resolving
conflicts. The old paths are `$JCODE_HOME/memory/projects/<DefaultHasher(Path)>.json`
and `$JCODE_HOME/goals/projects/<DefaultHasher(Path)>/*.json`. Private global
knowledge is intentionally not included in that migration.

Shared-project locking is supported on Unix; other platforms reject activation
instead of silently sharing without process-safe locking.
