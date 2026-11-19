# Known limitations

Sync never deletes or overwrites content it displaces from a harness. The old
copy goes to a recovery area next to it (`.sk1ll-tmp/displaced/`). If anything
was written to that copy during the swap, it is reported as a conflict that
you can adopt or discard (`sk1ll recover`). The cases below are what that
guarantee does not cover. Each one needs exceptional timing or an unusual
filesystem. None of them was reproduced through normal editing.

## A save that lands mid-swap on a rules or instructions file

Some editors save by writing a temporary file and renaming it over the
original. This is not the default in vim, VS Code or emacs. JetBrains does it
on some save paths, and emacs does it with `file-precious-flag`. If such a save
renames its file into place in the millisecond gap after sync's last check and
before sync's own rename, sync's rename replaces the editor's file. The
editor's content is lost. The version before it is kept in recovery.

This only applies to rules files and block instruction files. Skills are
directories and are placed without replacing. Closing the gap needs
`RENAME_EXCHANGE`, which is Linux-only.

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

Updating a rules or block instruction file, and creating any file, relies on
`link(2)`. On filesystems without hard links, such as FAT, these operations
fail and leave the original in place. They never fall back to an unsafe write.

## The recovery size cap

Unchanged recovery copies are pruned after a 10-minute grace period. Copies
that changed are never pruned automatically. The total size is capped by
`SK1LL_DISPLACED_MAX` (default 1 GiB). Above the cap, the oldest unchanged
copies are pruned first, even inside the grace period. If a program still
holds such a copy open and writes to it after it is pruned, that write is
lost.

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
and toggles a cleanup failed, timed out or gave up reading the repo, or a
restart skipped. They do nothing while the item is absent, and apply again
when an item of the same kind and name is pushed, even if it's a new item
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
