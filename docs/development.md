# Development

## Crates

| Crate | Role |
|---|---|
| `sk1ll-core` | All repo, import, draft and sync logic. It prints nothing, and its public types are `Serialize`. |
| `sk1ll-cli` | The `sk1ll` binary, a thin `clap` front end |
| `sk1ll-daemon` | The per-machine daemon, its local API and the client the CLI and GUI use |
| `sk1ll-server` | The fleet server: git remote, assignment, machine status |
| `sk1ll-proto` | Wire types shared by the server, daemon, CLI and GUI, with no I/O |
| `sk1ll-gui/src-tauri` | The desktop app, see [Desktop GUI](gui.md) |

`sync::plan` has no side effects. `sync::apply` checks the filesystem again before it changes anything. Sync only modifies:

- skill symlinks listed in the skills directory's `.sk1ll-managed-links` index,
- skill copies marked with `.sk1ll-managed`,
- its own block in instruction files,
- rule files listed in a rules directory's `.sk1ll-managed` index.

An existing skill symlink enters the index only when its canonical target is the repo skill currently assigned to that harness.

## Isolated dev fleet

`scripts/dev-fleet.sh` starts a real server and two daemons in a temporary directory, with sample skills, a rule and a local-only draft. It builds the binaries, gives each process its own `HOME`, XDG directories and git config, and uses a temporary admin token for join codes. It never reads or writes your real skills or harness directories.

```sh
scripts/dev-fleet.sh          # prints the GUI command for its socket
scripts/dev-fleet.sh --gui    # also runs pnpm tauri dev
```

Ctrl-C stops every process in the fleet, git hooks included. It waits five seconds before sending SIGKILL, then deletes the directory. The script needs bash, curl, python3, git, setsid, ps and pkill. Set `SK1LL_DEV_PORT` to pin the server port.

## Checks

```sh
cd crates/sk1ll-gui/ui
pnpm tsc --noEmit
pnpm lint
pnpm test
pnpm build           # needed before cargo, because the GUI embeds ui/dist
cd ../../..
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
python3 crates/sk1ll-gui/tests/test_dev_fleet.py
```

The automated tests don't drive the desktop app. Test interactive GUI flows by hand against the dev fleet.

## Docs

Domain terms are defined in [GLOSSARY.md](../GLOSSARY.md). Use them as written there, including the words to avoid. Record design decisions in [docs/adr/](adr/).

## Open questions

- Imported instruction sets, vendored and locked like skills.
- Per-harness frontmatter transforms in copy mode, for harness quirks.
