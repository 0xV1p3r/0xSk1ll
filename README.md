<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/branding/wordmark-dark.svg">
    <img src="assets/branding/wordmark-light.svg" alt="0xSk1ll" width="360">
  </picture>
</p>

<p align="center">
  One git repo for your agent skills and instructions, synced into every harness on every machine.
</p>

---

0xSk1ll keeps two kinds of content in a single git repo:

- **Skills**: `SKILL.md` folders, either your own or imported from a git source and pinned to a commit.
- **Rules**: project-agnostic instruction files ("for commits, use Conventional Commits").

`sk1ll sync` places the skills and renders the rules into each agent tool on the machine. 0xSk1ll calls these tools harnesses: Claude Code, Codex, OpenCode, Gemini, GitHub Copilot, Cursor, Amp, Kiro, Cline, Roo Code, or any directory you configure. It edits only files it placed and records their hashes. If you edit managed content inside a harness, sync reports a conflict and leaves your edit alone.

For several machines, a self-hosted server acts as the git remote and holds the per-machine assignment. A daemon on each machine pulls as soon as another machine pushes.

## Install

Install Git 2.40 or newer and make sure `git` is on `PATH`.

Release builds target Linux and Apple silicon macOS. The [release page](https://github.com/0xV1p3r/0xSk1ll/releases) lists available archives:

- `sk1ll-<version>-x86_64-linux.tar.gz` with `sk1ll`, the desktop app `sk1ll-gui`, `sk1ll-server` and the systemd unit, and an `aarch64` archive without the app,
- `sk1ll-<version>-aarch64-macos.tar.gz` with `sk1ll`, `sk1ll-server`, `0xSk1ll.app` and the launchd plist,
- a `.deb` for the desktop app that also installs `sk1ll` to `/usr/bin`,
- `SHA256SUMS`, and the server image `ghcr.io/0xv1p3r/sk1ll-server`.

```sh
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf sk1ll-0.1.0-x86_64-linux.tar.gz
install -Dm755 sk1ll-0.1.0-x86_64-linux/sk1ll -t ~/.local/bin        # and sk1ll-gui for the app
```

Or install the `.deb` with `sudo apt install ./0xSk1ll_0.1.0_amd64.deb`. The app needs GTK 3 and WebKitGTK 4.1.

On macOS, verify the download with `shasum -a 256 -c SHA256SUMS --ignore-missing`. Extract the archive, copy `0xSk1ll.app` to Applications and put the CLI binaries on your `PATH` if you use them from a terminal. The app is unsigned and unnotarized. If Gatekeeper blocks a copy you verified, try opening it once and then use Open Anyway in System Settings → Privacy & Security. macOS builds run in CI and have had no manual testing.

To build from source instead, run `cargo install --path crates/sk1ll-cli`, which installs `sk1ll` to `~/.cargo/bin`. sk1ll needs Unix (Linux or macOS): without a running daemon, each command runs the daemon's engine in its own process.

## Quick start

`sk1ll setup` walks you through the first run, and the desktop app shows the same steps on first open. To do it by hand:

```sh
sk1ll init                                                           # creates ~/.local/share/sk1ll/repo
sk1ll add https://github.com/0xV1p3r/0xSk1ll/tree/main/skills/sk1ll  # lets agents add skills for you
sk1ll add https://github.com/owner/repo/tree/main/skills/pdf         # import one skill
sk1ll list                                                           # skills and rules in the repo
sk1ll sync --dry-run                                                 # show what sync would change
sk1ll sync                                                           # place them in every harness
```

The repo lives in `~/.local/share/sk1ll/repo` (`$XDG_DATA_HOME/sk1ll/repo`) on Linux and in `~/Library/Application Support/sk1ll/repo` on macOS unless you pass `--repo` or set `SK1LL_REPO`. `sk1ll` commands run inside another skills repo use that one. A new repo starts with one commit holding its starter files, so Draft starts empty. If you later add an `origin` with its own history, the first Pull through the daemon replaces that commit with the remote's baseline. It looks like this:

```
~/.local/share/sk1ll/repo/
  sk1ll.toml                 # import sources
  sk1ll.lock                 # pinned commit and content hash per import
  personal/<name>/SKILL.md   # your skills
  imported/<name>/...        # vendored imports; review updates with git diff
  instructions/<name>.md     # one rule per file, rendered in file-name order
```

`sk1ll add` accepts a GitHub repo, folder or `SKILL.md` link, `github:owner/repo`, a git URL or a local path. If the link holds several skills, `sk1ll add --list <link>` prints them and `--path` picks one. The [sk1ll skill](skills/sk1ll/SKILL.md), added in the quick start, teaches an agent to run these commands. `sk1ll` carries a copy of it: if the link can't be fetched (no network, or no access to this repo), `sk1ll add` installs that copy as the personal skill `sk1ll` instead. The copy doesn't follow upstream. To import the real one later, delete `personal/sk1ll` and run the same `sk1ll add` again.

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

Pull requires Git 2.40 or later, and 2.42 when the new Baseline changes a `.gitattributes` file. It updates only the working-tree paths the new Baseline changes, one at a time, and leaves the Draft where it is. Replayed commits get this Machine as their committer and lose their signatures. Pull drops commits that become empty and keeps commits that started empty. Pull refuses unpushed merge commits, submodules and sparse checkouts. File changes need atomic filesystem operations on Linux or macOS. Pull checks for support before it starts changing files.

If Pull stops partway through, the next `sk1ll pull` or Daemon reconcile resumes it. Edits to paths it already updated stay as Draft on the new Baseline. While Pull is pending, `git status` shows the partially updated tree, and `sk1ll commit`, `sk1ll sync`, `sk1ll push` and `sk1ll repair-ids` refuse to run. A harness change is saved but syncs once the Pull finishes. Restoring an item a Pull is Blocked on, or that blocks a pending Pull, keeps your version in recovery. Pull recovery entries stay until you adopt or discard them. Pull and `sk1ll recover adopt` also keep each file they replace, a committed version: `sk1ll recover list` marks your version `changed` and those `unchanged`, safe to discard. `sk1ll status` counts the changed entries.

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

## Documentation

- [Configuration](docs/configuration.md): harness presets, `machine.toml`, rule frontmatter, instruction formats and assignment.
- [Running a fleet](docs/fleet.md): deploying the server, joining machines and running the daemon.
- [Desktop GUI](docs/gui.md): the Tauri app and how to run it.
- [Development](docs/development.md): crate layout, the isolated dev fleet and the check suite.
- [Skills API](docs/skills-api.md): the daemon's content endpoints.
- [Known limitations](docs/known-limitations.md): edge cases sync's safety guarantees don't cover.
- [Glossary](GLOSSARY.md) and [decision records](docs/adr/).

## License

[MIT](LICENSE).
