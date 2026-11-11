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

```sh
cargo install --path crates/sk1ll-cli    # installs the `sk1ll` binary
```

The CLI works on any platform in standalone mode. The daemon, `join` and `fleet` need Unix.

## Quick start

```sh
sk1ll init                                                           # creates ~/skills
sk1ll add https://github.com/0xV1p3r/0xSk1ll/tree/main/skills/sk1ll  # lets agents add skills for you
sk1ll add https://github.com/owner/repo/tree/main/skills/pdf         # import one skill
sk1ll list                                                           # skills and rules in the repo
sk1ll sync --dry-run                                                 # show what sync would change
sk1ll sync                                                           # place them in every harness
```

The repo looks like this:

```
~/skills/
  sk1ll.toml                 # import sources
  sk1ll.lock                 # pinned commit and content hash per import
  personal/<name>/SKILL.md   # your skills
  imported/<name>/...        # vendored imports; review updates with git diff
  instructions/<name>.md     # one rule per file, rendered in file-name order
```

`sk1ll add` accepts a GitHub repo, folder or `SKILL.md` link, `github:owner/repo`, a git URL or a local path. If the link holds several skills, `sk1ll add --list <link>` prints them and `--path` picks one. The [sk1ll skill](skills/sk1ll/SKILL.md), added in the quick start, teaches an agent to run these commands.

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
| `join <server-url> <code>` | Join a fleet and clone its repo |
| `daemon` | Run the daemon in the foreground |
| `fleet` | Show every machine in the fleet |

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

MIT or Apache-2.0, at your option.
