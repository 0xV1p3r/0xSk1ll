# Standalone assignment is machine-local

Without a fleet, assignment lives in `~/.cache/sk1ll/assignment.json` on Linux or `~/Library/Caches/sk1ll/assignment.json` on macOS, or the path selected by `SK1LL_ASSIGNMENT`. Both the direct CLI and daemon read and change this file. Changes apply immediately to this machine's harnesses. They never enter the content repo or need a commit.

This complements [ADR 0001](0001-assignment-is-server-state.md). Joined machines keep assignment at the server, with a local snapshot for offline use. Standalone Machines have no Assignment log. The desktop Assignment matrix can undo its latest bulk change while the revision still matches.

## Consequences

- The daemon watches `assignment.json` so direct CLI changes take effect too.
- A separate local snapshot stores a monotonic revision for API readers. It is not a fleet assignment snapshot.
- Joining replaces the local assignment with the server's assignment. Standalone changes are not uploaded as log entries.
