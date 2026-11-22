# Standalone assignment is machine-local

Without a fleet, assignment lives in `~/.cache/sk1ll/assignment.json` on Linux or `~/Library/Caches/sk1ll/assignment.json` on macOS, or the path selected by `SK1LL_ASSIGNMENT`. The daemon's engine, in a daemon or in a CLI command's process ([ADR 0008](0008-cli-is-a-client-of-the-engine.md)), reads and changes this file, and so may a hand edit. Changes apply immediately to this machine's harnesses. They never enter the content repo or need a commit.

This complements [ADR 0001](0001-assignment-is-server-state.md). Joined machines keep assignment at the server, with a local snapshot for offline use. Standalone Machines have no Assignment log. The desktop Assignment matrix can undo its latest bulk change while the revision still matches.

## Consequences

- The daemon watches `assignment.json` so hand edits take effect too. A pending Harness rename is finished by the engine, under the machine lock, before it reconciles; reading the file never finishes one, and previews plan with it finished in memory.
- A toggle is saved by a compare-and-swap against the file and its revision, so a hand edit landing while it is made becomes a revision of its own and the toggle is made again on top of it.
- A separate local snapshot stores a monotonic revision for API readers. It is not a fleet assignment snapshot.
- Joining replaces the local assignment with the server's assignment. Standalone changes are not uploaded as log entries.
