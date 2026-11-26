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

Rewriting a block that is still what sk1ll wrote (a rule toggled or
changed) keeps every byte outside the block and leaves no recovery entry:
the old file is held in recovery only during the swap and dropped once it
checks unchanged. A write through a descriptor still open on the old file
after that is lost. A forced rewrite of a block edited by hand keeps the
old file in recovery until you adopt or discard it.

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

## A hand edit of assignment.json at the moment a toggle is saved

A machine-local toggle is saved by comparing `assignment.json` and its
revision with the exact bytes the toggle was made from, then replacing the
file atomically. A hand edit that lands before that comparison becomes a
revision of its own and the toggle is made again on top of it. One that lands
in the gap between the comparison and the rename is replaced and lost, as
described above for block instructions files. A save into that gap needs
exceptional timing; the next toggle or reconcile does not bring it back.
Taking up a hand edit (at startup, before a toggle, in a reconcile) writes
only its revision, never the file, and a machine-local Harness rename writes
the file only if it still holds what the rename read; a later edit fails the
rename, which stays pending, rather than being replaced.

## One engine per Machine, and what keys it

The machine lock is a set of OS file locks, one for each file an engine
writes outside the repo, the harness directories and sk1ll's state
directory: the state file, `machine.toml`, `assignment.json` and the files
kept beside the state file (`daemon-assignment.json`,
`standalone-assignment.json`, `standalone-state.json`, the notifications
shown). Each lock is keyed on the file's real path (with every symlink
resolved, the file's own included). The lock files are all kept in `locks/`
in sk1ll's state directory (`SK1LL_STATE_DIR`, default
`$XDG_STATE_HOME/sk1ll`), each named by a hash of the real path it stands
for, never beside the file. A pending Harness rename is kept there too, in
`harness-rename/`, under the hash of `machine.toml`'s real path, so an
engine finds it whatever name it knows `machine.toml` by.

Two engines that share any of those files, by whatever names, are one
Machine and exclude each other, as long as they use the same state
directory: two engines run with different `SK1LL_STATE_DIR` or
`XDG_STATE_HOME` (or `HOME`) take different locks and don't see each
other's pending rename. Two state files in one directory share
`standalone-assignment.json` and `standalone-state.json`, so they are one
Machine. A symlink retargeted while an engine runs is not followed by its
lock, nor by its pending rename. Two config dirs that share none of them
are two Machines, even if their `machine.toml` files name the same harness
directories: only each harness directory's own lock keeps their syncs apart
then, and their Assignments are separate. Lock files are never removed (one
per file an engine has written, a few dozen bytes of directory entry each).
Like the directory lock, the machine lock needs a filesystem with working
locks between all writers.

A daemon that starts while a command's engine runs waits up to 30 seconds for
it, then fails to start; start it again once the command ends. A command that
changes something while the daemon manages another repo is refused rather
than sent to it, since the daemon manages one repo at a time. Previews run
beside any engine and take no lock, so one that reads while another engine
writes can see a file it is about to write and one it has written already.

## Directory placement without a no-replace rename

On Linux, directories are placed with `renameat2(RENAME_NOREPLACE)`. On other
platforms, or filesystems that reject that call, sync checks the destination and then renames. In that case
an *empty* directory created in the gap can be replaced. A directory with
content in it is never replaced. Files and symlinks fall back to a hard link
or a new symlink at the destination, which never replaces anything. The old
name is then unlinked, so a concurrent rename onto that old name in the gap
can be lost. The same fallback applies when sync detaches an incumbent into
its backup slot.

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
subsequent writes through that open file. A replaced file that still holds
just its committed version is pruned like sync's routine copies, ten
minutes after it was replaced and once the Pull is finished; a write
through a descriptor still open after that is lost. These operations need atomic
exchange and no-replace rename on the filesystems where they are used.

Pull checks for clashes item by item before it changes anything. After that,
a Draft edit to a path it has yet to update can block it. Restore the item
(`sk1ll restore` keeps your version in recovery) or clear the obstruction,
then pull again. A
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

## An interrupted keep mine

`sk1ll pull --keep` moves each kept file into recovery, one path at a time,
before its pull, and only the run itself puts them back: after its pull went
through, or, when the pull stays Blocked on an item it wasn't told to keep,
where they were. It puts them back one by one too. A run that stops part-way,
because the process exits or an error comes once a file has moved, puts
nothing more back, and nothing resumes it. An automatic resume would have to
tell what the run left at each path from what you did since, which it can't
always do: a vacant path is either a file the run hadn't put back yet or one
you deleted.

So what is where after a stop depends on how far the run got. A file it had
not moved yet is where it was, in your Draft. A file it moved is in recovery,
and its path holds the Baseline's version, or nothing. A file it had already
put back, when it stopped while settling, is back in your Draft. The next
pull or daemon reconcile finishes the pull side as any pull does, clears the
run's staging, and saves a report of each version still in recovery, with
its `sk1ll recover adopt <id>`. `sk1ll status`, every `sk1ll pull` and the
app's Draft screen show that report until you have adopted or discarded each
version in it. Its notes (staging left as it is, a deletion of yours) have
nothing to adopt: they stay until you dismiss them with `sk1ll recover
dismiss-notes` or **Dismiss notes** in the app. An index entry the pull's un-commit staged at a kept path is
reset to `HEAD`'s only where the journal names it as the run's own and it is
still exactly that. Any other staging at a kept path is left as it is, and
the report says so.

## An index change while keep mine resets a kept path

Keep mine checks a kept path's staging to tell yours from what the pull's
un-commit staged, and later resets that path's index entry to `HEAD`'s. In
between, as separate commands, it journals the path, moves its file into
recovery and saves the journal. A `git add` (or another command that changes
the index) that lands on that path anywhere from the staging check through
the reset is undone by the reset; no duration is promised for that window,
though it covers only those few steps for one path. What was staged is not lost: the
staged content is in the object store until it is garbage-collected (`git
fsck --lost-found` finds it), and the kept file itself is in recovery. Stage
it again after the keep is done.

## A deletion during keep mine's settlement

When the run puts a kept file back, it first checks that the path is not
vacant while the new Baseline has a file there (that would be a deletion of
yours, made while the keep ran). A deletion that lands between that check
and the publish, a few milliseconds, can be undone: the kept copy is put
back where you just deleted the Baseline's file. Nothing of yours is lost,
since the file put back is your kept version. Delete it again if you meant
to, or restore the item with `sk1ll restore <item>`.

## Staging the same content keep mine staged

The pull's un-commit records the exact index entry it leaves at each path of
the commits it turns back into Draft, and keep mine treats a staged path as
its own only while the entry is still exactly that. If you stage content
identical to that entry (same mode and blob) yourself, keep mine can't tell
your staging from its own, and may reset it. The content is not lost: it is
in the commit the un-commit turned back into Draft (in the reflog), and keep
mine keeps the file itself in recovery. Stage it again after the keep is
done.

## Filter drivers in the skills repo

The skills repo doesn't support git filter drivers (Git LFS and the like):
the server is plain git over HTTP and has no LFS endpoint. A pull onto a
Baseline that writes a file through one stops before it changes anything
and the Machine stays Blocked until `filter=` is gone from `.gitattributes`
(the repo's or the Machine's own `.git/info/attributes`). The server refuses
a push whose tree gives a file one, by its `.gitattributes` alone, and
`sk1ll commit` refuses to commit such a tree by the same rule. A driver needn't be configured on the
Machine for its attribute to count, since the Baseline is the same
everywhere. `git check-attr` prints `set` both for `filter` set without a
value and for a driver called `set` (likewise `unset` and `unspecified`), so
those names count as a driver only where one of that name is configured.

Every git command on the skills repo (a pull's, the clone's, the first
Baseline's, and status, diff, restore and commit outside a pull) has each
filter driver git's configuration for that repo names turned off,
conditional includes included, so no driver process runs even for a pull
that started before an attribute named one; that pull finishes with the
content unconverted. The clone is made without a checkout, its drivers read
in the clone, and a Baseline that writes a file through one is refused
there too. The drivers are read from git's configuration as each command,
or a pull, starts: one added to the configuration in between, and named by
an attribute that also appears meanwhile, can still run. Your own git
commands in the repo use your configuration as it is, drivers included.
Imports keep their own handling of LFS sources.

## A registration left over by a rejoin

`POST /join` registers before it stores anything. If the Machine's old
registration works again meanwhile (the server stops refusing its token),
the new registration is not used: the Daemon asks the server to remove it
again. If the server can't be reached then, it stays in the Fleet as a
Machine that never connects, until it is removed from the Fleet.

## Extended attributes and alternate data streams

The fingerprint that decides whether placed content is unchanged includes
extended attributes (and with them, ACLs) on Linux. A change to
an attribute therefore counts as an edit, including one the system adds
by itself, such as a relabel. On Windows
alternate data streams are not covered, and on other Unix systems neither are
extended attributes. A change to either alone can be lost when sync replaces
or prunes the file.

## Filesystems without hard links

Skill copies and rule files need hard links for their placement anchors.
On filesystems without hard links, such as FAT, an existing object without
an anchor is unmanaged and a new placement fails before changing the
destination. Block instructions also use hard links for recovery.

## Recovery pruning

Recovery prunes backups 10 minutes after the displacement completed, if they
still match their placement fingerprints, whatever all backups take: there
is no size cap that prunes earlier. A backup with changes or without a
placement fingerprint stays until explicit discard. So does a Draft recovery
entry from Pull, except one for a file that held exactly what checking out
the old commit writes, with no extended attributes beyond its SELinux label:
that one is pruned like a backup, once the Pull is finished. Pruning
rechecks the payload and then every entry's metadata. A write through a
still-open file descriptor after that last check can be lost.

Superseded ownership indexes are saved under `.sk1ll-state/index-history/`.
Directory recovery checks them after the same grace period and keeps any
that changed.

Pruning never deletes a directory recursively. It unlinks only the files and
symlinks the fingerprint covered, each after checking that its metadata is
unchanged, and then removes each directory only while it is empty. A file
added, or changed, after the check stops the removal. The rest of the backup
stays in recovery, marked changed. Files already unlinked by then are gone,
and they matched the fingerprint. `sk1ll recover discard` is the one
recursive deletion, and it runs only on request.

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

The hook checks each push's Item IDs by running the `sk1ll-server` binary
that wrote it, by its path at startup. Replacing that binary while the server
runs makes the hook run the new one; removing it refuses every push until the
server restarts. A server embedded through the library (`Config::new`) has no
binary to run and doesn't check Item IDs. Commits that reach the bare repo
without a push, such as a fetch run on the server by hand, aren't checked
either: the server then refuses to list or prune orphans and to change
defaults until a checked push replaces that Baseline.

## Changes metadata cannot see

The Daemon decides whether anything changed from file metadata (inode, size,
timestamps, mode), and remembers the hashes of trees whose metadata stayed the
same. A few changes leave all of it as it was: a write through a shared
writable memory mapping after the file's timestamps settled, a change to a
directory's extended attributes or ACL alone, a write that restores every
timestamp. The Daemon notices those within ten minutes, plus one periodic
reconcile (`SK1LL_FULL_RECONCILE_INTERVAL`, `SK1LL_RECONCILE_INTERVAL`), when
a full reconcile plans everything and reads every tree again. One also runs at
startup. Sync never relies on the remembered hashes when it replaces or
removes content.

A linked worktree's or a separate git directory's changes are seen by the
periodic reconcile, not by the file watcher, which watches `.git` in the
working tree only: they can take up to one `SK1LL_RECONCILE_INTERVAL` to show.

## Reviewed sync and concurrent changes

A reviewed Sync can apply several placements. When a staged copy's digest
differs from the reviewed digest, the Daemon leaves that copy unpublished and
returns a new preview. Placements completed before the mismatch remain applied.
