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
transports are allowed. Any joined machine can still point a source at an
address inside the server's own network, and the server will send git's
discovery request there. Run the server where its outbound network access is
limited to what imports need if that matters to you.
