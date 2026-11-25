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

Keep your **skills** (`SKILL.md` folders, your own or imported from GitHub) and **rules** (instructions such as "use Conventional Commits") in one repo. `sk1ll sync` puts them into each agent tool on the machine: Claude Code, Codex, OpenCode, Gemini, GitHub Copilot, Cursor, Amp, Kiro, Cline, Roo Code or any folder you choose.

sk1ll only touches files it placed. If you edit one inside a tool, it reports a conflict and leaves your edit alone.

With several machines, a self-hosted server holds the repo and each machine's choices, and every machine picks up a push within seconds.

## Install

You need Git 2.40 or newer on `PATH`. Download the latest version from the [releases page](https://github.com/0xV1p3r/0xSk1ll/releases):

| System | Download |
| --- | --- |
| Debian, Ubuntu | `0xSk1ll_<version>_amd64.deb` (desktop app and CLI), then `sudo apt install ./0xSk1ll_<version>_amd64.deb`. Needs GTK 3 and WebKitGTK 4.1. |
| Other Linux (x86_64) | `sk1ll-<version>-x86_64-linux.tar.gz`: the CLI `sk1ll`, the app `sk1ll-gui` and the server. Copy the binaries to `~/.local/bin`. |
| Linux (ARM) | `sk1ll-<version>-aarch64-linux.tar.gz`: CLI and server, no app. |

`SHA256SUMS` on the release page lists the checksums. Check a download with `sha256sum --check --ignore-missing SHA256SUMS`.

To build from source, run `cargo install --path crates/sk1ll-cli`. sk1ll needs Linux; macOS and Windows are not supported.

## First run

Run `sk1ll setup`, or open the desktop app, which shows the same steps. It creates your repo (or joins an existing fleet), picks the tools to sync into and starts the background daemon.

Then add skills and sync:

```sh
sk1ll add https://github.com/owner/repo/tree/main/skills/pdf   # import a skill
sk1ll new rule commits                                         # write your own rule
sk1ll sync --dry-run                                           # see what would change
sk1ll sync                                                     # place everything
```

Add the [sk1ll skill](skills/sk1ll/SKILL.md) (`sk1ll add https://github.com/0xV1p3r/0xSk1ll/tree/main/skills/sk1ll`) and your agents can run these commands for you.

Changes you make are a **draft** on this machine until you commit and push them. `sk1ll draft` lists them, and `sk1ll commit --all --push` shares them with your other machines.

## Several machines

Run the server with Docker (`ghcr.io/0xv1p3r/sk1ll-server`), then join each machine with a code from `sk1ll fleet invite`:

```sh
sk1ll join https://sk1ll.example.com <code>
```

The desktop app shows every machine and lets you turn each skill and rule on or off per tool and per machine. See [Running a fleet](docs/fleet.md).

## Updates

- **Desktop app (`.deb`):** checks GitHub for a new version at startup and updates itself. Settings → App updates turns this off.
- **CLI:** `sk1ll status` says when a newer release is out. Set `SK1LL_NO_UPDATE_CHECK=1` to turn that off.
- **Imported skills:** `sk1ll update` fetches upstream changes. Preview one with `sk1ll update <name> --dry-run`.

Upgrade the server and all machines to the same release. See [CHANGELOG.md](CHANGELOG.md) for what changed.

## Documentation

- [Using the CLI](docs/cli.md): the repo layout, drafts and pulls, and every command.
- [Configuration](docs/configuration.md): tools, `machine.toml`, rules and assignment.
- [Running a fleet](docs/fleet.md): the server, joining machines and the daemon.
- [Desktop app](docs/gui.md): its screens and updates.
- [Known limitations](docs/known-limitations.md): edge cases sync's safety guarantees don't cover.
- [Development](docs/development.md): building, checks and releases. Also the [Skills API](docs/skills-api.md), [glossary](GLOSSARY.md) and [decision records](docs/adr/).

## License

[MIT](LICENSE).
