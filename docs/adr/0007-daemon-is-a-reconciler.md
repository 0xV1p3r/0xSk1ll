# The daemon is a reconciler

Status: accepted. Sync plans from desired state, read endpoints use the
published Assignment, and network I/O runs outside the engine mutex. The full
Snapshot and the refresh fingerprint are pending.

The daemon is level-triggered. Desired state is a pure function of its inputs.
The fingerprint includes HEAD, the remote head, the Draft tree OID, the
Assignment revision, the `machine.toml` hash and Harness fingerprints. The daemon
skips refresh when the fingerprint is unchanged.

## Consequences

- Readers use a published Snapshot.
- The daemon prepares over the network, commits locally under the engine mutex,
  then publishes the Snapshot. Network I/O runs outside the mutex.

## Network outside the engine mutex

An operation that talks to the server checks and takes what it sends under
the engine mutex, releases it for the network, then takes it again to apply
the result against the state as it is now:

- Assignment writes wait for the server's confirmation unlocked, one at a
  time. A confirmation overtaken by a newer applied revision is not applied,
  and a daemon that stopped managing meanwhile applies nothing. Adopt
  removes the copies an enablement came from only while the skill still has
  the enabled Item ID.
- git's clone, fetch and push run unlocked, one at a time. A pull fetches,
  then moves onto the fetched Baseline under the mutex (finishing a stopped
  pull, recovery, missing Item IDs and the sync keep their order there). A
  push sends the exact commit it checked, so a pull that rebased it meanwhile
  makes the server refuse it as not fast-forward. A clone moves into place
  only once complete.

## One lifecycle

The connection, a removal, the halt before a restart and where the Machine
stands against the server's Baseline (Blocked, a pull pending, the Baseline
last known) are one value with explicit transitions. The status the daemon
publishes derives from it. The state file keeps its Blocked state and
Baseline, so a restarted daemon starts Blocked and Behind before it can pull.
