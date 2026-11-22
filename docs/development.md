# Development

## Crates

| Crate | Role |
|---|---|
| `sk1ll-core` | All repo, import, draft and sync logic, and the one git layer (`git`, its `plumbing`). It prints nothing, and its public types are `Serialize`. |
| `sk1ll-cli` | The `sk1ll` binary, a `clap` client of the local API: of the running daemon, else of an engine it starts in its own process ([ADR 0008](adr/0008-cli-is-a-client-of-the-engine.md)) |
| `sk1ll-daemon` | The per-machine daemon: its engine (over an `AssignmentStore` and a `Remote`), local API, in-process engine (`local`) and the client the CLI and GUI use |
| `sk1ll-server` | The fleet server: git remote, assignment, machine status |
| `sk1ll-proto` | Wire types shared by the server, daemon, CLI and GUI, with no I/O |
| `sk1ll-gui/src-tauri` | The desktop app, see [Desktop GUI](gui.md) |

`sync::plan` has no side effects. `sync::apply` checks the filesystem and the index entry again before it changes anything. Sync only modifies existing harness content that it owns.

- A skill symlink must resolve to the target recorded in the skills directory's `.sk1ll-state/index.json`.
- A copied skill's root `.sk1ll-placement` file must match the token and share an inode with its anchor in `.sk1ll-state/anchors/`.
- A rule file must share an inode with its recorded anchor in the rules directory's `.sk1ll-state/anchors/`.
- In block-format instructions, sync owns its marked block.

An existing skill symlink enters the index only when its canonical target is the repo skill currently assigned to that harness. Sync locks each managed directory while it recovers or writes placements. It records a pending placement or removal and the recovery entry before moving the incumbent. It validates the object that moved and publishes the replacement only at a vacant name. Recovery checks which object is at the destination and keeps the backup if a different object took its place. A read-only plan resolves pending ownership in memory. The index stores a full placement fingerprint, including directories and modes, separately from the source content hash. Recovery keeps a backup whose payload differs from that fingerprint or has no recorded placement.

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
cargo fmt --all --check
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
python3 crates/sk1ll-gui/tests/test_dev_fleet.py
```

The CLI's command tests run the `sk1ll` binary, each with its own `HOME` and XDG directories: every command runs the engine, which reconciles, and would otherwise find your own harnesses through the presets.

The automated tests don't drive the desktop app. Test interactive GUI flows by hand against the dev fleet.

Time sync planning with `cargo test -p sk1ll-core sync_plan_no_change -- --ignored --nocapture`.

CI (`.github/workflows/ci.yml`) runs the same checks on Linux, plus `scripts/check-version.sh` and `gh actions-lock --verify`. The arm64 macOS job builds the UI and runs the workspace build, tests and clippy.

## Workflow actions

Workflows name actions by version tag, and `.github/workflows/actions.lock` pins each one, transitive ones included, to the commit that runs. After changing a workflow's `uses:`, regenerate the lockfile with the [`gh actions-lock`](https://github.com/github/gh-actions-lock) extension (`gh extension install github/gh-actions-lock`, then `gh actions-lock`) and commit both. Dependabot's action updates relock it themselves. `Dockerfile.server` pins its base images by digest.

## Release

1. Set the version in `Cargo.toml`, `crates/sk1ll-gui/src-tauri/tauri.conf.json`, `crates/sk1ll-gui/ui/package.json` and the `SK1LL_SERVER_VERSION` default in `deploy/docker-compose.server.yml`. Then run `cargo build` so `Cargo.lock` follows.
2. Date the version's section in `CHANGELOG.md`. Bump `PROTOCOL_VERSION` in `sk1ll-proto` or `API_VERSION` in `sk1ll-daemon`'s client if the server protocol or the local API changed incompatibly.
3. Run `scripts/check-version.sh vX.Y.Z` and the checks above, then tag and push: `git tag vX.Y.Z && git push origin vX.Y.Z`.
4. `.github/workflows/release.yml` builds the Linux x86_64 and aarch64 archives, the Linux `.deb` and the macOS arm64 archive with an unsigned app bundle, and opens a draft release with `SHA256SUMS` and the changelog section. A tag that isn't `vX.Y.Z` opens a pre-release. Review it and publish it.
5. Publishing it runs `.github/workflows/server-image.yml`, which pushes the server image for the tag, and `:latest` unless it's a pre-release.

Once the repository is public, both workflows attest the build provenance of what they ship: `gh attestation verify <file> --repo 0xV1p3r/0xSk1ll`.

Builds read the commit for `--version` from git, or from `SK1LL_COMMIT` where there is no `.git` (the server image takes it as a build argument). The toolchain is pinned in `rust-toolchain.toml`.

## Docs

Domain terms are defined in [GLOSSARY.md](../GLOSSARY.md). Use them as written there, including the words to avoid. Record design decisions in [docs/adr/](adr/).

## Open questions

- Imported instruction sets, vendored and locked like skills.
- Per-harness frontmatter transforms in copy mode, for harness quirks.
