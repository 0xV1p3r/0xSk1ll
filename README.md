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

`sk1ll sync` places the skills and renders the rules into each agent tool on the machine. 0xSk1ll calls these tools harnesses: Claude Code, Codex, OpenCode, Gemini, or any directory you configure. It edits only files it placed and records their hashes. If you edit managed content inside a harness, sync reports a conflict and leaves your edit alone.

For several machines, a self-hosted server acts as the git remote and holds the per-machine assignment. A daemon on each machine pulls as soon as another machine pushes.

## Install

Releases ship Linux and Apple silicon macOS builds. Each [release](https://github.com/0xV1p3r/0xSk1ll/releases) has:

- `sk1ll-<version>-x86_64-linux.tar.gz` with `sk1ll`, the desktop app `sk1ll-gui`, `sk1ll-server` and the systemd unit, and an `aarch64` archive without the app,
- `sk1ll-<version>-aarch64-macos.tar.gz` with `sk1ll`, `sk1ll-server`, `0xSk1ll.app` and the launchd plist,
- a `.deb` for the desktop app that also installs `sk1ll` to `/usr/bin`,
- `SHA256SUMS`, and the server image `ghcr.io/0xv1p3r/sk1ll-server`.

```sh
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf sk1ll-0.1.0-x86_64-linux.tar.gz
install -Dm755 sk1ll-0.1.0-x86_64-linux/sk1ll -t ~/.local/bin        # and sk1ll-gui for the app
```

Or install the `.deb` with `sudo apt install ./0xSk1ll_0.1.0_amd64.deb`. `sk1ll` needs `git` on `PATH`; the app needs GTK 3 and WebKitGTK 4.1.

On macOS, verify the download with `shasum -a 256 -c SHA256SUMS --ignore-missing`. Extract the archive, copy `0xSk1ll.app` to Applications and put the CLI binaries on your `PATH` if you use them from a terminal. Git must be on `PATH`. The app is unsigned and unnotarized. If Gatekeeper blocks a copy you verified, try opening it once and then use Open Anyway in System Settings → Privacy & Security. macOS builds run in CI and have had no manual testing.

To build from source instead, run `cargo install --path crates/sk1ll-cli`, which installs `sk1ll` to `~/.cargo/bin`. The CLI builds on any platform in standalone mode. The daemon, `join` and `fleet` need Unix.

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

The committed `HEAD`, pushed to the server, is the **baseline**. Uncommitted changes are this machine's **draft**. Sync applies the draft to the harnesses right away, so you can try an edit before other machines get it.

```sh
sk1ll draft                          # draft items: A(dded), M(odified), D(eleted)
sk1ll diff pdf                       # one item against the baseline
sk1ll commit pdf git-flow --push     # commit only these items, then push
sk1ll restore pdf                    # discard one item's draft
sk1ll pull                           # fetch the new baseline, keep the draft on top
```

Commands take a bare skill or rule name, or `skill:<name>`, `rule:<name>`, `manifest` and `other:<path>`. `pull` never merges. If the new baseline changes an item that is also in your draft, `pull` changes nothing and marks the machine Blocked until you commit or restore that item.

## Commands

| Command | What it does |
|---|---|
| `setup` | Set up this machine step by step: a standalone repo, joining a fleet or starting one, harnesses and the daemon service |
| `init [path]` | Create a skills repo |
| `add <link>` | Import a skill (`--path`, `--ref`, `--name`, `--list`) |
| `update [name] [--force]` | Re-fetch imports; refuses to overwrite local edits unless forced |
| `remove <name>` | Remove an import |
| `fork <name>` | Turn an import into a personal skill you can edit |
| `edit <name>` | Open a personal skill or rule in `$VISUAL` or `$EDITOR` |
| `list [--unmanaged]` | List repo content, or skills in harnesses that sk1ll doesn't manage |
| `status` | Import drift and pending sync actions |
| `sync [--dry-run] [--force]` | Write skills and instructions into harnesses |
| `render <harness>` | Print the instruction files a harness would receive |
| `harnesses` | List this machine's harnesses |
| `assign <harness> skill\|rule <name> on\|off` | Toggle an item for one harness |
| `assign <harness> --default-skills on\|off` | Set the default for skills added later (`--default-rules` for rules) |
| `conflicts` | List harness and recovery conflicts |
| `adopt <harness> <name>` | Copy a hand edit or an unmanaged skill into the draft |
| `recover list\|adopt\|discard` | Inspect content that sync moved aside |
| `draft`, `diff`, `commit`, `push`, `restore`, `pull` | Draft workflow, see above |
| `join <server-url> <code>` | Join a fleet and clone its repo; `--admin-token` instead of a code for the first machine |
| `daemon` | Run the daemon in the foreground |
| `fleet` | Show every machine in the fleet |
| `fleet invite` | Create a single-use join code and print the join command and expiry; `--scope self` limits the new machine to managing itself |
| `fleet remove <machine>` | Remove a machine after confirming its id. Use `--yes` in scripts |
| `fleet log`, `fleet undo <entry-id>` | Filter the Assignment log or undo an entry |
| `completions <shell>` | Generate shell completions |

`sk1ll <command> --help` lists every flag.

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
