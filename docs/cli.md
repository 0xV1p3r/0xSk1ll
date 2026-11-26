# Using the CLI

## The skills repo

The repo lives in `~/.local/share/sk1ll/repo` (`$XDG_DATA_HOME/sk1ll/repo`) unless you pass `--repo` or set `SK1LL_REPO`. `sk1ll` commands run inside another skills repo use that one. A new repo starts with one commit holding its starter files, so Draft starts empty. If you later add an `origin` with its own history, the first Pull through the daemon replaces that commit with the remote's baseline. It looks like this:

```
~/.local/share/sk1ll/repo/
  sk1ll.toml                 # import sources
  sk1ll.lock                 # pinned commit and content hash per import
  personal/<name>/SKILL.md   # your skills
  imported/<name>/...        # vendored imports; review updates with git diff
  instructions/<name>.md     # one rule per file, rendered in file-name order
```

`sk1ll add` accepts a GitHub repo, folder or `SKILL.md` link, `github:owner/repo`, a git URL or a local path. If the link holds several skills, `sk1ll add --list <link>` prints them and `--path` picks one. The [sk1ll skill](../skills/sk1ll/SKILL.md), added in the quick start, teaches an agent to run these commands. `sk1ll` carries a copy of it: if the link can't be fetched (no network, or no access to this repo), `sk1ll add` installs that copy as the personal skill `sk1ll` instead. The copy doesn't follow upstream. To import the real one later, delete `personal/sk1ll` and run the same `sk1ll add` again.

## Draft and baseline

The pushed state of the Fleet repo is the **Baseline**. Local commits stay on this Machine until pushed. Uncommitted changes are this machine's **draft**. Sync applies the draft to the harnesses right away, so you can try an edit before other machines get it.

```sh
sk1ll draft                          # draft items: A(dded), M(odified), D(eleted)
sk1ll diff pdf                       # one item against the baseline
sk1ll commit pdf git-flow --push     # commit only these items, then push
sk1ll commit --all -m "..."          # commit every draft item
sk1ll restore pdf                    # discard one item's draft
sk1ll pull                           # fetch the new baseline, keep the draft on top
```

Commands take a bare skill or rule name, or `skill:<name>`, `rule:<name>`, `manifest` and `other:<path>`. `pull` never merges. If the new baseline changes an item that is also in your draft, `pull` changes nothing and marks the machine Blocked. An unpushed commit that conflicts with the new baseline is turned back into draft changes first (only the branch moves; no file changes), so it blocks the pull the same way. To get past it, keep yours or take theirs:

```sh
sk1ll pull --keep pdf      # keep mine: take the new baseline, put your pdf back on top as draft
sk1ll diff pdf             # review: your whole file replaced the other machine's change to it
sk1ll commit pdf           # or merge by hand first

sk1ll restore pdf          # take theirs: your version goes to recovery (prints its entry ids)
sk1ll pull
```

`sk1ll pull --keep-all` keeps your version of every clashing item. Keep mine works per file: the other machine's change to that item is overwritten in your draft, so review the diff before you commit. While the pull also clashes with an item you didn't name, nothing changes and it stays Blocked. It refuses an item you staged a change in: unstage it or commit it first. If a step fails, your version stays in recovery and the error names the `sk1ll recover adopt <id>` that finishes by hand. An interrupted keep (a crash, a stop) puts nothing more back on its own: the next `sk1ll pull` or daemon reconcile reports each version left in recovery with its `sk1ll recover adopt <id>`, in `sk1ll status` and the app until you adopt or discard each one.

The daemon and mutating commands write a new Item ID for hand-made Skills, Rules,
and Import entries that lack one. These writes are part of the Draft and get
committed with the item. Until then an item has no toggle and follows the
defaults; `sk1ll list --json` shows its `id` as null, and `sk1ll list` says so only
while a pending Pull holds the IDs up.
If copying an item duplicates its ID, run `sk1ll repair-ids` and commit the
repaired items.

Pull requires Git 2.40 or later, and 2.42 when the new Baseline changes a `.gitattributes` file. It updates only the working-tree paths the new Baseline changes, one at a time, and leaves the Draft where it is. Replayed commits get this Machine as their committer and lose their signatures. Pull drops commits that become empty and keeps commits that started empty. Pull refuses unpushed merge commits, submodules and sparse checkouts. File changes need atomic filesystem operations on Linux. Pull checks for support before it starts changing files.

If Pull stops partway through, the next `sk1ll pull` or Daemon reconcile resumes it. Edits to paths it already updated stay as Draft on the new Baseline. While Pull is pending, `git status` shows the partially updated tree, and `sk1ll commit`, `sk1ll sync`, `sk1ll push` and `sk1ll repair-ids` refuse to run. A harness change is saved but syncs once the Pull finishes. Restoring an item a Pull is Blocked on, or that blocks a pending Pull, keeps your version in recovery. Your versions in recovery stay until you adopt or discard them. Pull's entries for the files it replaced, which hold only their committed versions, are routine copies: once the Pull is finished they are pruned after ten minutes, like sync's. One that something wrote to since (a save during the swap, a write through a file opened before the Pull), or that holds bytes or extended attributes the old commit can't give back (an edit inside an expanded `$Id$`, a mode other than checkout's, an attribute beyond its SELinux label), stays, `changed`. `sk1ll recover adopt` also keeps each file it replaces, a committed version: `sk1ll recover list` marks your version `changed` and those `unchanged`, safe to discard. `sk1ll status` counts the changed entries.

## Commands

| Command | What it does |
|---|---|
| `setup` | Set up this machine step by step: a standalone repo, joining a fleet or starting one, harnesses and the daemon service. After `init` it skips to the harnesses |
| `init [path]` | Create a skills repo |
| `add <link>` | Import a skill (`--path`, `--name`). Use `--ref` to select a branch, tag or commit and `--list` to show candidates, sizes and warnings about scripts, executables, binaries and large files |
| `update [name] [--force]` | Re-fetch imports. Local edits require `--force` |
| `update <name> --dry-run` | Preview upstream commits and changed files for one Import |
| `remove <name>` | Remove an import |
| `fork <name>` | Turn an import into a personal skill you can edit |
| `new skill\|rule <name> [--description <text>]` | Create a personal skill or rule in the draft |
| `edit <name>` | Open a personal skill or rule in `$VISUAL` or `$EDITOR` |
| `rename <item> <new-name>` | Rename a personal skill or rule in the draft. It keeps its Item ID, so its Assignment goes with it |
| `delete <item>` | Delete a personal skill or rule in the draft, after confirming. Use `--yes` in scripts |
| `list` | List repo content. Use `--json` to include Item IDs or `--unmanaged` to list unmanaged skills |
| `repair-ids` | Give each item that shares an Item ID, or has none, a fresh one in the Draft; of items sharing one, the item that holds it in the Baseline keeps it (else the first by path) |
| `status` | Import drift and pending sync actions |
| `sync [--dry-run] [--force]` | Write skills and instructions into harnesses |
| `render <harness>` | Print the instruction files a harness would receive |
| `harnesses [--all] [enable\|disable\|add\|remove]` | List the harnesses sync writes into and use `--all` to include disabled Harnesses and opt-in presets. The subcommands change `machine.toml` after previewing what stays in each Harness |
| `assign <harness> skill\|rule <name> on\|off` | Toggle a local repo item for one harness |
| `assign <harness> --default-skills on\|off` | Set the default for skills added later (`--default-rules` for rules) |
| `assign <harness> --all on\|off` | Set every Skill and Rule and both defaults. With a Fleet, this includes Baseline items the Machine has yet to pull |
| `assign ... --machine <id>` | Change another fleet Machine's Assignment instead, as the desktop app's matrix does. Needs `fleet` scope |
| `conflicts` | List harness and recovery conflicts |
| `adopt <harness> <name>` | Copy a hand edit or an unmanaged skill into the draft. A hand edit goes into the item it was placed from, found by its Item ID even if it was renamed since |
| `recover list\|adopt\|discard` | Inspect content that sync or pull moved aside: each entry's id, item, harness (or `pull`), age and whether it changed. `--paths` adds where it was and where it is kept |
| `draft`, `diff`, `commit`, `push`, `restore`, `pull` | Draft workflow, see above |
| `join <server-url> <code>` | Join a fleet and clone its repo; `--admin-token` instead of a code for the first machine |
| `daemon` | Run the daemon in the foreground |
| `fleet` | Show every machine in the fleet |
| `fleet invite` | Create a single-use join code and print the join command and expiry; `--scope self` limits the new machine to managing itself |
| `fleet remove <machine>` | Remove a machine after confirming its id. Use `--yes` in scripts |
| `harnesses rename <old> <new> [--replace]` | Rename a Harness and move its Assignment |
| `fleet orphans` | List orphan Assignment toggles |
| `fleet prune-orphans [--machine <id>]` | Clear orphan toggles within your scope |
| `fleet copy-assignment <source> <target>` | List each change (harness, item or default, before -> after), confirm, and copy a joined Machine's Assignment; a target that already matches is left alone. Use `--merge`, repeat `--map from=to`, or pass `--yes` in scripts |
| `fleet log`, `fleet undo <entry-id>` | Filter the Assignment log by Machine, Harness, author or item, or undo one entry |
| `completions <shell>` | Generate shell completions |

`sk1ll <command> --help` lists every flag. `sync` exits non-zero while it leaves a conflict alone, and `sync --force` prints the recovery id it kept each overwritten edit under. `SK1LL_*` variables set to an empty value count as unset.

