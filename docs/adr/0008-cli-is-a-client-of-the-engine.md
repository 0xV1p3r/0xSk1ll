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

- A command run without a daemon reconciles as the daemon would: changes
  sync after themselves.
- Previews only read: `status`, `render`, `sync --dry-run`, `list`, `diff`,
  `edit`, `harnesses`, `draft`, `conflicts` and `recover list` run a
  read-only engine that holds no lock, never reconciles and writes
  nothing, not even a missing Item ID. Sent to a running daemon, what the
  first seven ask for never reconciles either; `draft`, `conflicts` and
  `recover list` get the daemon's Snapshot. A read-only engine answers
  those from a Snapshot it plans in memory instead. They plan in memory
  with the Inputs the next reconcile would have: a machine-local Harness rename that stopped
  half-way is finished in the plan only, and a hand edit of the
  machine-local Assignment is read, not applied. Only an engine holding the
  machine lock finishes a pending rename, under its engine mutex.
- The engine in a command's process runs nothing in the background. A joined
  one connects to the server only for a request that needs the session (an
  Assignment write, a Harness rename, an unmanaged Adopt) and does not pull
  when it connects; git's fetch and push need no session.
- One engine at a time manages a Machine, whatever repo it is for. What it
  changes is mostly the Machine's: `machine.toml` and a pending Harness
  rename, the Assignment (the machine-local file and its revision, or a
  joined Machine's snapshot and its mirror), the state file and the harness
  directories with their ownership indexes. So an engine takes the machine
  lock before it reads any of that, and holds it for its life: the daemon
  until it exits, a command's engine for the command. It is one OS lock per
  file an engine writes there (the state file, `machine.toml`, the
  Assignment file, and the files kept beside the state file), each keyed on
  the file's real path, every symlink resolved: two names for one file are
  one lock, so engines that share any of these files exclude each other
  however they are configured. The lock files live in sk1ll's own state
  directory, named by a hash of that real path, never beside a user's file,
  and are taken in one order. A pending Harness rename is kept there too,
  keyed on `machine.toml`'s real path, so it is finished through whichever
  name an engine knows `machine.toml` by, and `machine.toml`'s lock covers
  it. A daemon starting
  while a command's engine runs waits for it (up to 30 seconds), so it
  loads what that command saved. `join` and `init` hold it too.
- A command that changes something while another engine manages the
  Machine is refused: a daemon for another repo says it manages that repo
  (select it, or stop the daemon); a running command's engine, or one for
  another config dir sharing these files, makes it fail with "another sk1ll
  process is managing this Machine". Previews still answer beside it.
- Core's engine lock on the repo stays, taken for whatever changes the
  repo: it is the token core's repo operations require. While another
  process holds it, a command's changes are refused; previews still answer.
- A machine-local Assignment write has no network step: it is checked,
  given the next revision, saved and applied in one step under the engine
  mutex. The save compares the file and its revision with what the write
  was made from; a hand edit that landed meanwhile is taken up as a
  revision of its own and the write made again on top, so a toggle that
  reports success is saved and never overwrites the edit. What an engine
  just loaded is never written back unless something changed: taking up a
  hand edit (at startup, or before a toggle or a reconcile) records only
  its revision, by the same compare-and-swap against the exact bytes it
  read, so a second edit landing in between is read again, not replaced.
- A joined Machine's commands work on the repo it joined; another selected
  repo is refused.
- `sk1ll init` creates a repo itself unless a daemon waits for one there: it
  makes what an engine then manages. `join` and `setup` keep their own steps.
- Fleet commands work without a daemon, through an engine for the joined repo.
- The CLI needs Unix, like the daemon.
