# The daemon is a reconciler

Status: accepted and implemented: one reconcile over fingerprinted Inputs,
readers answer from a published Snapshot, and network I/O runs outside the
engine mutex.

The daemon is level-triggered. Desired state is a pure function of its Inputs:
the repo (working tree, HEAD and refs, a Pull's journal), `machine.toml` and a
pending Harness rename, the applied Assignment revision, where the Machine
stands on Pull, each Harness's placements and the recovery area. The daemon
skips the reconcile when their fingerprint is unchanged.

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

## One reconcile over fingerprinted Inputs

Every trigger runs the same reconcile: file events and the periodic tick wake
it in the background; Assignment snapshots, nudges, the server session and
local API mutations run it under the engine mutex they hold, so their reply
shows its result. It fingerprints the Inputs from file metadata alone (`lstat`:
inode, size, mtime, ctime and mode of every entry), with no file read and no git
process, and plans and applies only when they differ from the Inputs of the last
reconcile that succeeded. It keeps the order a Pull and Item IDs need: finish a
stopped Pull, recover, write missing Item IDs, then sync and report. A
reconcile that failed or reported errors records nothing, so the next wake tries
again.

Metadata can miss a change made within one tick of the filesystem's clock (a
second on some filesystems): a file written twice in that tick may keep its
size, inode and timestamps. So Inputs changed less than two seconds before they
were read are never recorded, and the next wake reconciles again. A Harness's
fingerprint covers the entries of its skills directory, the whole tree of each
placement its ownership index records, the index and sk1ll's scratch area, and
its instructions; it does not look inside unmanaged skills, which the plan does
not read either.

Planning takes the hashes of skill trees (repo sources, Harness placements) from
a cache keyed by that same metadata, with the same two-second rule. The cache
serves planning only. Applying a plan, the ownership checks and every removal
hash the filesystem as it is, so a stale entry can at worst make a plan stale,
which applying it then refuses; it never makes sync overwrite or delete content
the uncached check would have kept. Directory timestamps are left out of both
fingerprints (lock files coming and going change them constantly), so a change
to a directory's extended attributes alone is not seen until something else in
the tree changes.

## Readers use the Snapshot

Each reconcile publishes a Snapshot (the Draft, the inventory, the conflicts and
the recovery area) as one immutable value, before the events that send clients
to it; the applied Assignment is published the same way. Readers never wait for
the engine mutex. When it is free, a read first reconciles (which returns at
once when no Input changed), so a read right after an edit sees it. While an
engine operation runs, a read answers from the last Snapshot, so an edit made
outside sk1ll moments before may not show until the reconcile it wakes publishes
the next one; the `draft` and `status` events follow. Reads about something in
particular (a diff, a file, the skills browser, a preview) still read the repo
as it is.
