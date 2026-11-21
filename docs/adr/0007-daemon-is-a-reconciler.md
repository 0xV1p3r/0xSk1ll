# The daemon is a reconciler

Status: accepted. Sync already plans from desired state and read endpoints
use the published Assignment. The full Snapshot, refresh fingerprint and
network prepare and local commit split are pending.

The daemon is level-triggered. Desired state is a pure function of its inputs.
The fingerprint includes HEAD, the remote head, the Draft tree OID, the
Assignment revision, the `machine.toml` hash and Harness fingerprints. The daemon
skips refresh when the fingerprint is unchanged.

## Consequences

- Readers use a published Snapshot.
- The daemon prepares over the network, commits locally under the engine mutex,
  then publishes the Snapshot. Network I/O runs outside the mutex.
