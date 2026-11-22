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

- What the result is applied in is checked again as one token, the World
  taken under the mutex before releasing it: still managing, and the same
  registration (a removal or a halt for a rejoin advances the lifecycle's
  epoch), and, for what changes the harnesses, no Pull pending. A result
  from another World is not applied and saves nothing.
- Assignment writes wait for the server's confirmation unlocked, one at a
  time. A confirmation overtaken by a newer applied revision is not applied,
  and a daemon that stopped managing meanwhile applies nothing; neither does
  a Harness rename's confirmation. Adopt removes the copies an enablement
  came from only while the skill still has the enabled Item ID and no Pull
  is pending; a refused Adopt takes back only an uncommitted addition that
  still has the Item ID it gave it.
- A rejoin registers unlocked, then stores the new registration only if the
  old one is still replaceable (removed, or its token still refused); else
  it asks the server to remove the new one again. That check, storing the
  credentials and the halt are one critical section of the lifecycle, so the
  session can't connect the old registration in between; a connection after
  the halt is ignored. Assignment snapshots are
  tagged with their registration and written before the credentials, so a
  stop in between never pairs new credentials with old toggles.
- git's clone, fetch and push run unlocked, one at a time. A pull fetches,
  then moves onto the fetched Baseline under the mutex (finishing a stopped
  pull, recovery, missing Item IDs and the sync keep their order there). A
  push sends the exact commit it checked, so a pull that rebased it meanwhile
  makes the server refuse it as not fast-forward. A clone moves into place
  only once complete. Filter drivers (git-lfs's smudge) never run under the
  mutex. They run unlocked right after the fetch for every file the next pull
  writes through one: the rest of a stopped pull from its journal, else the
  move onto the fetched Baseline, with unpushed commits replayed and
  `.gitattributes` read where the pull reads them. Each conversion is keyed
  by blob, path, resolved attributes and the driver's configuration, and used
  without being checked again. A pull that misses one stops before changing
  anything. The daemon then runs the drivers unlocked and tries again, three
  times at most, then stays Blocked (or pending) and says why. The background
  reconcile does the same for a stopped pull it finishes.

## One lifecycle

The connection, a removal, the halt before a restart and where the Machine
stands against the server's Baseline (Blocked, a pull pending, the Baseline
last known) are one value with explicit transitions. The status the daemon
publishes derives from it. The state file keeps its Blocked state and
Baseline, so a restarted daemon starts Blocked and Behind before it can pull.
A pending pull is remembered with the Baseline its journal moves onto. One
whose journal is gone finished before the process could record it: it is
Clear once `HEAD` contains that Baseline, whatever was fetched since, which
the next pull takes up. A
standalone daemon's record names its repo and is dropped for any other.

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
were read are never recorded, a Harness's included, and the next wake
reconciles again. A Harness's fingerprint covers its settings (paths, mode and
instructions format), the entries of its skills directory, the whole tree of
each placement its ownership index records, the index and sk1ll's scratch
area, its instructions, and the scratch and recovery area beside a block
instructions file; it does not look inside unmanaged skills, which the plan
does not read either. The recovery area's fingerprint covers the payload of
every entry its index records, wherever it is.

What is read through a symlink is stamped through it: the link and what it
resolves to, so a skills directory, an instructions file or `machine.toml`
linked into a dotfiles repo changes the fingerprint when its target does. When
`.git` is a pointer file (a linked worktree, `--separate-git-dir`), the git
directory it names and that one's common directory are stamped instead of the
pointer alone. The working tree leaves out only the staging directories sk1ll
creates for imports and renames, directly in `personal/`, `imported/` and
`instructions/`; a skill's own file of that name is content.

Metadata still misses a few changes: a write through a shared writable mapping
after its stamps settled, a change to a directory's extended attributes or ACL
alone, a write that leaves every timestamp as it was. So a full reconcile runs
at startup and then at most every ten minutes (`SK1LL_FULL_RECONCILE_INTERVAL`)
on the next wake: it plans and applies whatever the fingerprint says and
empties the hash cache first, so every tree is read again. A change only that
pass sees is reconciled within the interval plus one periodic tick.

A standalone daemon whose repo disappears (moved or removed, or a `.git`
pointer file whose git directory is gone) publishes a Snapshot without it:
every reader of the Draft, the inventory, the conflicts and the recovery area
gets the same "no skills repo" error as any other request about the repo, and
the report is emptied. Any reconcile that fails because the repo can't be
opened does the same, with the error that stopped it, rather than leave the
last Snapshot standing.

Planning takes the hashes of skill trees (repo sources, Harness placements) from
a cache keyed by that same metadata, with the same two-second rule. The cache
serves planning only. Applying a plan, the ownership checks and every removal
hash the filesystem as it is, so a stale entry can at worst make a plan stale,
which applying it then refuses; it never makes sync overwrite or delete content
the uncached check would have kept. Directory timestamps are left out of both
fingerprints (lock files coming and going change them constantly), so a change
to a directory's extended attributes alone is not seen until something else in
the tree changes, or the next full reconcile.

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
as it is. A caller about to act on a list (`sk1ll restore --all`) asks for it
with `?fresh=true`, which waits for the engine and reconciles first.
