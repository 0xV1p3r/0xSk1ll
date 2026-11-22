# The CLI is a client of the engine, run in its own process without a daemon

Every `sk1ll` command that reads or changes the repo, the harnesses or the
Assignment is a request to the daemon's local API: to the running daemon when
it manages the repo, else to an engine the command starts in its own process
and drops when it ends. That engine is the daemon's engine, reconcile and
local API bound to no socket, so each operation has one implementation and
one set of guards (the engine lock, the World re-check after network I/O, the
pending Pull, Item IDs). The CLI used to carry a second implementation for
working directly, which drifted from the daemon's.

Where the Assignment lives and what the repo talks to are chosen once, by
whether the Machine joined: an `AssignmentStore` (the server, or the
machine-local file) and a `Remote` (the server, or the repo's own origin),
with one code path over them (ADR 0001, 0002, 0004).

## Considered Options

- Keep direct mode: two implementations of every change, and joined writes
  that opened their own server session.
- Require a running daemon: every command would fail on a machine that runs
  none, standalone ones included.

## Consequences

- A command run without a daemon reconciles as the daemon would: reads that
  answer from the Snapshot bring the harnesses in line first, and changes
  sync after themselves. `sync --dry-run`, `render` and `status` still only
  preview.
- The engine in a command's process runs nothing in the background. A joined
  one connects to the server only for a request that needs the session (an
  Assignment write, a Harness rename, an unmanaged Adopt) and does not pull
  when it connects; git's fetch and push need no session.
- While another process holds the repo's engine lock, a command's changes
  are refused, and so are reads that need a reconciled Snapshot (`draft`,
  `conflicts`); previews and `list` still answer.
- A joined Machine's commands work on the repo it joined; another selected
  repo is refused.
- `sk1ll init` creates a repo itself unless a daemon waits for one there: it
  makes what an engine then manages. `join` and `setup` keep their own steps.
- Fleet commands work without a daemon, through an engine for the joined repo.
- The CLI needs Unix, like the daemon.
