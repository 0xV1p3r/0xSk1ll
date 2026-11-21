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

## A concurrent save in the Library editor

The Daemon stages the editor's new text before checking the file's fingerprint
again. If another process writes to the file between that final check and the
rename, the editor's save replaces that write. This is the same timing gap as
the one for rules files above.

## A save to machine.toml at the moment sk1ll writes it

Apply first checks the file seen at preview. A harness change stages the new `machine.toml` in a file only it created,
resolves the configured path again, and checks that it still points to the
same file with the same bytes and permissions. It then renames the new file
over that target. A symlink retargeted after the check can leave the write
at the old target. A save that lands in the gap
between that last check and the rename is replaced and lost, as described
above for rules files. A save any earlier makes the change fail without
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

## A save while Pull updates a file

Pull leaves disjoint Draft files in place. Before replacing a file, it checks
that the file still holds what the old Baseline has. It then exchanges the
names with the new file, so a save renamed in at the last moment goes into
recovery. A writer with the displaced file still open keeps writing to that
same file. Adopt moves the recovered file into the Draft, including any
subsequent writes through that open file. These operations need atomic
exchange and no-replace rename on the filesystems where they are used.

Pull checks for clashes item by item before it changes anything. After that,
a Draft edit to a path it has yet to update can block it. Move any edits you
want to keep aside, restore or clear the obstruction, then pull again. A
save to a path Pull already updated stays as Draft. While Pull is pending,
the Daemon keeps the last complete Draft report and inventory, reports the
inventory as incomplete, writes no Item IDs and pauses sync. A Harness
that reads Skills through symlinks can already see the paths Pull updated.

Pull needs the repo's Git directory on the same filesystem as its working
tree. An interrupted Pull leaves `sk1ll-pull.json` there, along with the
evidence it uses to resume after a process exits. Pull recovery after power
loss is unsupported. Keep external Git commands that switch branches or
write the index out of the repo during Pull. Pull checks the branch again
before writing the index, and another Git command can still race that check.

## Filesystems without hard links

Sync relies on `link(2)` to update or create a Rule file or block instructions
file. On filesystems without hard links, such as FAT, these operations
fail and leave the original in place. They never fall back to an unsafe write.

## The recovery size cap

Unchanged harness recovery copies are pruned after a 10-minute grace period.
Changed copies and Draft recovery entries from Pull stay until you adopt or
discard them. Above `SK1LL_DISPLACED_MAX` (default 1 GiB), pruning can remove
the oldest unchanged harness copies earlier. A write through a file still
held open after it was pruned is lost. A new copy can put recovery above the
cleanup target until the next prune.

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

## Toggles for absent items

Assignment keeps toggles for absent Item IDs until someone clears them. An
offline Machine's last report can still include an item that was later
removed from the Baseline. Its toggles are eligible for pruning once the
Machine reports the item gone or the Machine is removed from the Fleet.

## The server's repo at startup and HEAD changes

The server's reads of its repo while serving have a deadline and an output
cap. The git
commands it runs while setting the repo up at startup (creating it, the
initial commit, its config and the pre-receive hook) don't: a repo stuck on
storage or a lock can hold startup until git gives up. The server decides the
baseline branch from HEAD once at startup and writes `hooks/pre-receive` over
on every start. Changing HEAD while it runs isn't supported, and pushes keep
going to the branch it started with until it restarts.

## Reviewed sync and concurrent changes

A reviewed Sync can apply several placements. When a staged copy's digest
differs from the reviewed digest, the Daemon leaves that copy unpublished and
returns a new preview. Placements completed before the mismatch remain applied.
