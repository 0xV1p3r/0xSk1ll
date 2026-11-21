# Known limitations

Sync never deletes or overwrites content it displaces from a harness. The old
copy goes to a recovery area next to it (`.sk1ll-tmp/displaced/`). If anything
was written to that copy during the swap, it is reported as a conflict that
you can adopt or discard (`sk1ll recover`). The cases below are what that
guarantee does not cover. Each one needs exceptional timing or an unusual
filesystem. None of them was reproduced through normal editing.

## A save that lands mid-swap on a block instructions file

Some editors save by writing a temporary file and renaming it over the
original. If that rename lands after sync's last check of a block
instructions file and before sync's own rename, sync can replace the
editor's file. Recovery holds the version from before the editor's rename.

## Placement anchors and directory locks

A skill symlink's ownership follows its target. Recreating the identical
link counts as the same placement because it points to the same skill.
Copied skills and rules use hard-link anchors. If an anchor is missing
or no longer points to the file at the destination, the object is unmanaged.

The directory lock assumes a filesystem with working locks between all
writers. An NFS configuration without cross-host locking can admit two
writers. Keep the lock file in place while a process holds it.

Linux resolves staging paths through the pinned directory descriptor.
On other platforms the private staging directory must stay at the same
path while sync runs.

## A concurrent save in the Library editor

The Daemon stages the editor's new text before checking the file's fingerprint
again. If another process writes to the file between that final check and the
rename, the editor's save replaces that write. This is the same timing gap as
for block instructions files.

## A save to machine.toml at the moment sk1ll writes it

Apply first checks the file seen at preview. A harness change stages the new `machine.toml` in a file only it created,
resolves the configured path again, and checks that it still points to the
same file with the same bytes and permissions. It then renames the new file
over that target. A symlink retargeted after the check can leave the write
at the old target. A save that lands in the gap
between that last check and the rename is replaced and lost, as described
above for block instructions files. A save any earlier makes the change fail without
writing. A missing `machine.toml` is created only while nothing is there
(with a hard link, which never replaces); on a filesystem without hard links
the change fails and says so. ACLs and extended attributes of `machine.toml`
are not kept, and its owner and group only where the filesystem lets sk1ll
set them.

## Directory placement without a no-replace rename

On Linux, directories are placed with `renameat2(RENAME_NOREPLACE)`. On macOS
they use `renamex_np(RENAME_EXCL)`. On other platforms, or filesystems that
reject those calls, sync checks the destination and then renames. In that case
an *empty* directory created in the gap can be replaced. A directory with
content in it is never replaced.

## First baseline and join recovery

An unborn repo's first baseline publishes each missing file without replacing
anything, using pinned, no-follow directory traversal. If a later file is
obstructed, HEAD and the index stay unchanged. Files already published are kept
as draft, because deleting them could lose edits made after publication. Move
those files aside with the obstruction before retrying the first pull.

Standalone join uses the same no-replace directory renames as sync. The empty
directory placement window described above also applies to publishing its
prepared clone and restoring its backup on platforms without that support.
A concurrent obstruction is kept, and rollback refuses to overwrite it.
The error identifies the backup to restore after moving the obstruction aside.
On non-Unix platforms init and first-baseline file publication fail rather than
use a path traversal that could follow a concurrent symlink replacement.

## Filesystems without hard links

Skill copies and rule files need hard links for their placement anchors.
On filesystems without hard links, such as FAT, an existing object without
an anchor is unmanaged and a new placement fails before changing the
destination. Block instructions also use hard links for recovery.

## The recovery size cap

Recovery prunes backups after a 10-minute grace period if they still match
their placement fingerprints. A backup with changes or without a placement
fingerprint stays until explicit discard. Above `SK1LL_DISPLACED_MAX`
(default 1 GiB), the oldest unchanged backups can be pruned before the grace
period ends. Pruning rechecks the payload and then every entry's metadata.
A write through a still-open file descriptor after that last check can be
lost, whether pruning happens after the grace period or to meet the size
cap.

Superseded ownership indexes are saved under `.sk1ll-state/index-history/`.
Directory recovery checks them after the same grace period and keeps any
that changed. This history sits outside the size cap.

The cap is a cleanup target, not a strict ceiling. It is checked before a
new copy is added, so recovery can sit above the cap until the next prune.

## Content browsing outside Unix

On Unix, Skills file reads and tree listings pin directories and open each path
component relative to its directory descriptor, refusing symlinks. On non-Unix
platforms containment uses best-effort canonical-path checks. A concurrent
ancestor-directory symlink replacement can redirect a read or listing after
that check. Avoid browsing content being changed by an untrusted process there.

## Import checks reach whatever the server can reach

The server checks each import's upstream with `git ls-remote`. The source
comes from the pushed manifest, and only the `https`, `http`, `ssh` and `git`
transports are allowed. By default any machine that can push, whatever its
scope, can point a source at an address inside the server's own network, and
the server will send git's discovery request there.

Set `SK1LL_SERVER_IMPORT_HOSTS` to the hosts imports come from, or turn the
checks off with `SK1LL_SERVER_IMPORT_CHECKS=off` (see
[Import checks](fleet.md#import-checks)). The allowlist is matched on the host
name in the source, before DNS: an allowed name that resolves to an internal
address is still contacted, and a proxy set in the server's environment
decides where an allowed name's connection goes. While the allowlist is set, the check ignores the
server's git and ssh configuration, so url rewrites, ssh `Host` aliases and
proxy commands can't send it elsewhere, but private sources that relied on
that configuration need the real host name and a default key or the agent.
Restrict the server's outbound network too if that matters to you.

## Toggles for items no longer in the baseline

A push drops toggles only for the items it removed itself: on every machine
for a `fleet` machine or the admin, on its own assignment only for a `self`
machine (ADR 0005). Other toggles for items that aren't in the baseline stay
in their assignments, and in `GET /machines/{id}/assignment`: other machines'
toggles for items a `self` machine removed, toggles kept for a draft or
unpushed commit that has since gone, toggles for items that were already
absent before a push (servers before ADR 0005 dropped those on any push),
and toggles left behind when cleanup failed, ran out of time reading the repo
or was interrupted by a restart. They do nothing while the item is absent,
and apply again when an item of the same kind and name is pushed, even if it's a new item
that only shares the name. A `fleet` machine or the admin can clear them by
setting the toggles.

## The server's repo at startup and HEAD changes

The server's reads of its repo while serving have a deadline, an output cap
and, for each push or draft discard, a total budget (ADR 0005). The git
commands it runs while setting the repo up at startup (creating it, the
initial commit, its config and the pre-receive hook) don't: a repo stuck on
storage or a lock can hold startup until git gives up. The server decides the
baseline branch from HEAD once at startup and writes `hooks/pre-receive` over
on every start. Changing HEAD while it runs isn't supported, and pushes keep
going to the branch it started with until it restarts.

## Reviewed actions across versions

Reviewed Import updates require the Daemon's reviewed update route. Harness
Apply requires a fingerprint from Harness Preview. Restart or upgrade the
Daemon if the desktop app reports that either capability is missing.

A reviewed Sync can apply several placements. When a staged copy's digest
differs from the reviewed digest, the Daemon leaves that copy unpublished and
returns a new preview. Placements completed before the mismatch remain applied.

## Assignment edits on older Servers

Servers that omit Machine registrations do not enforce the Assignment
revision precondition. The desktop matrix warns before a bulk edit on those
Servers because it can overwrite an intervening change. Bulk Undo requires
a Server that checks revisions or the standalone Daemon.
