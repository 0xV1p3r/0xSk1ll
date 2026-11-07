# 0xSk1ll

0xSk1ll manages personal and imported agent skills (`SKILL.md` folders) and
project-agnostic agent instructions ("for X do Y") in one git repo, and syncs them
into every harness on every machine.

```
~/skills/
  sk1ll.toml        # imports (the repo holds content only)
  sk1ll.lock        # pinned commit + content hash per import
  personal/<name>/SKILL.md
  imported/<name>/...        # vendored, reviewed via git diff
  instructions/<name>.md     # instruction fragments, rendered in file-name order
```

The CLI command is `sk1ll`; the Rust crates use the `sk1ll-` prefix.

```sh
sk1ll init
sk1ll add github:owner/repo --path skills/pdf --ref main
sk1ll update [name] [--force]   # refuses to clobber local edits unless forced
sk1ll fork <name>               # imported -> personal, to edit freely
sk1ll list                      # skills + instruction fragments
sk1ll status                    # import drift + pending sync actions
sk1ll harnesses                 # this machine's harnesses (detected + machine.toml)
sk1ll assign claude skill pdf off       # per-harness toggles on this machine
sk1ll assign codex --default-skills off # default for skills added later
sk1ll render <harness>          # preview a harness's instruction files
sk1ll sync [--dry-run] [--force]
sk1ll adopt <harness> <name>    # take a harness's hand edits into the draft
```

## Baseline and draft

The repo's committed `HEAD`, pushed to `origin`, is the **baseline**. Uncommitted
changes (untracked files included) are this machine's **draft**, and sync
applies it to the harnesses right away. The draft is split into items:
`skill:<name>` (`personal/<name>/`, `imported/<name>/`), `fragment:<name>`
(`instructions/<name>.md`), `manifest` (`sk1ll.toml` + `sk1ll.lock`), and
`other:<path>` for anything else. Commands take a bare skill or fragment name,
or the `kind:name` form.

```sh
sk1ll draft                         # draft items: A(dded), M(odified), D(eleted)
sk1ll diff pdf                      # an item against the baseline
sk1ll promote pdf git-flow [-m msg] [--push]   # commit just these items
sk1ll push                          # push promoted commits to origin
sk1ll reset pdf                     # discard an item's draft
sk1ll reset --all [--yes]           # discard the whole draft (asks first)
sk1ll pull                          # fetch the new baseline, keep the draft on top
```

`promote` commits exactly the named items and leaves the rest of the draft (and
the index) alone. Without `-m`, the message is `promote <items> from <machine>`;
the author is your git identity, or the machine id if git has none. `pull`
fast-forwards (rebasing promoted, unpushed commits), and carries the draft
across. If the new baseline changes an item that is also in the draft, or a
rebase conflicts, `pull` changes nothing and reports the machine as Blocked;
promote or reset those items and pull again. Then run `sk1ll sync` (e.g. in a
`post-merge` hook).

## Harnesses

A harness is an agent tool on this machine: a skills directory, a global
instructions location, or both. Harnesses are machine-local, not in the repo.
Built-in presets are included when their directory exists:

| Preset | Detected by | Skills | Instructions |
|---|---|---|---|
| `claude` | `~/.claude` | `~/.claude/skills` | `~/.claude/rules` (`rules`) |
| `codex` | `~/.codex` | `~/.codex/skills` (copy mode) | `~/.codex/AGENTS.md` |
| `opencode` | `~/.config/opencode` | `~/.config/opencode/skill` | `~/.config/opencode/AGENTS.md` |
| `gemini` | `~/.gemini` | none | `~/.gemini/GEMINI.md` |

Adjust them in `~/.config/sk1ll/machine.toml` (`--machine-config` /
`SK1LL_MACHINE_CONFIG`). An entry for a preset changes only the keys it sets
(and includes the preset even if it wasn't detected); any other name adds a
harness:

```toml
id = "workstation"   # machine identity; default: hostname

[harnesses.claude]
mode = "copy"        # place skills as marked copies instead of symlinks

[harnesses.gemini]
enabled = false      # don't sync into a detected preset

[harnesses.cursor]
skills = "~/.cursor/skills"
instructions = ""    # empty clears a preset's path
```

## Assignment

Which skills and fragments each harness on this machine gets. Initially,
every item is enabled. Each harness has a default for newly appearing
items (`--default-skills`, `--default-fragments`). Changing a default records
existing items' current values as explicit toggles, so their assignment stays
the same. Assignment lives in a local cache, `~/.cache/sk1ll/assignment.json`
(`--assignment` / `SK1LL_ASSIGNMENT`), until the hub holds it (see Direction).

## Instructions

Each `instructions/*.md` file is one fragment. Optional frontmatter:

```markdown
---
description: Git conventions
harnesses: [claude]   # only render into these harnesses; default: all
---
For commits: use Conventional Commits.
```

There's no cross-harness global `AGENTS.md`; each harness has its own location,
so each harness has an `instructions_format`:

| Format | Writes | Use for |
|---|---|---|
| `block` (default) | A marked block inside one file; the rest of the file is left alone. Writes follow symlinks, so dotfile-linked files work. | `~/.codex/AGENTS.md`, `~/.config/opencode/AGENTS.md`, `~/.gemini/GEMINI.md` |
| `rules` | One copied file per fragment, tracked in a hidden `.sk1ll-managed` index | Claude Code's `~/.claude/rules/`, which loads every `.md` file in it for all projects |

A fragment's `harnesses:` restricts it further on top of assignment; `targets:`
is still accepted as a deprecated alias.

`rules` keeps fragment frontmatter except `harnesses`, so Claude's `paths:` works for
path-scoped rules. It copies rather than symlinks because Claude Code skips
symlinked rule files in some sessions (Cowork). Rule files you wrote yourself are
never touched; a name clash is reported as a conflict.

Both formats record hashes, as do copied skills. If you edit managed content
directly in the harness, sync reports a conflict instead of overwriting it.
`sk1ll adopt <harness> <name>` copies a copied skill or a rule file back into the
repo as a draft change (a rule file's `harnesses:` key is restored); `sync
--force` overwrites the edit instead. A `block` can't be adopted, since it mixes
fragments: move the edit into `instructions/` by hand.

## Layout

- `crates/sk1ll-core`: all logic. No printing; public types are `Serialize`.
  `sync::plan` is side-effect free, and `sync::apply` re-verifies state before
  touching anything. 0xSk1ll modifies skill symlinks recorded in the skills
  directory's `.sk1ll-managed-links` index, copies marked with `.sk1ll-managed`,
  its own block in instruction files, and rule files listed in a rules
  directory's `.sk1ll-managed` index. An existing skill link can enter the index
  only when its canonical target is exactly the currently assigned repo skill.
- `crates/sk1ll-cli`: thin `clap` front-end.

## Direction

The sections above describe the current CLI: harnesses are already
machine-local and profiles are gone, but assignment is still a local file and
there is no hub, daemon or GUI yet. The planned fleet model is below.
Terms are defined in [GLOSSARY.md](GLOSSARY.md), and decisions are recorded in
[docs/adr/](docs/adr/).

- **The repo holds content only**: skills, fragments, imports. The **hub**, a
  self-hosted Docker container, hosts the canonical repo, and each machine keeps
  a full clone with the hub as its only remote ([ADR 0002](docs/adr/0002-hub-is-the-git-remote.md)).
- **Baseline vs draft**: a draft is a machine's uncommitted changes. It is live
  in that machine's harnesses and never leaves the machine. Promote and reset
  work per skill or fragment. After a promote, the hub tells every machine to
  pull. If a draft clashes with the new baseline, the machine is marked Blocked.
- **Assignment** (which item is enabled in which harness on which machine) is
  live hub state, not in `sk1ll.toml`. It is made of explicit toggles plus a
  per-harness default for new skills, and the hub keeps a log with undo
  ([ADR 0001](docs/adr/0001-assignment-is-hub-state.md)).
- **`sk1ll-daemon`** runs on every machine. It watches the repo and harnesses,
  applies draft + assignment, works offline from the last assignment it knew,
  and is the only process that talks to the hub. Machines join with a
  single-use join code and get per-machine, revocable tokens.
- **`sk1ll-gui`** (Tauri v2) talks only to the local Daemon over its Unix
  socket. See [Desktop GUI](#desktop-gui) below.

## Desktop GUI

The Unix desktop app includes Fleet, Assignment matrix, Draft, Conflicts,
Imports, Log and Settings. It supports light, dark and system appearance.
Fleet reports refresh on Daemon events and every 15 seconds. The header shows
this Machine's id, headline status and Daemon/Hub connections. A stopped
Daemon gets a reconnecting screen with start instructions; revoked and
awaiting-assignment Machines get banners.

The matrix groups Harness columns by Machine, shows explicit vs inherited
Assignment and controls the defaults for new Skills and Fragments. Items
absent from the applied Baseline carry an `Unpushed` or `Draft only` badge
and can only be assigned to this Machine. Writes are optimistic, roll back on errors and show
the Daemon's message. Successful responses merge by Machine and revision, so
older refreshes cannot undo a saved toggle. Matrix snapshot reads use empty
Assignment changes, which return a revision without modifying or logging state.
The read-only `GET /items` inventory reads the applied
origin/main tree from git and includes Draft and unpushed items.

Draft includes item checkboxes, unified diffs, an editable prefilled Promote
message, push results, confirmed Reset and Pull for Blocked clashes. Reset and
Promote capture their item lists, compare byte fingerprints on refresh, and
require review again when selected content changes. Conflicts
supports Adopt/Overwrite and displaced recovery Adopt/Discard. Imports updates
into Draft; Log pages newest first with per-entry Undo. Settings lists Harnesses,
joins/rejoins, creates one-time join codes, removes Machines with typed
confirmation and shows connection info. Mutations show pending states and are
never retried by the GUI.

Layout:

- `crates/sk1ll-gui/src-tauri`: the `sk1ll-gui` workspace member, thin Client
  commands and one reconnecting SSE forwarding task.
- `crates/sk1ll-gui/ui`: React, TypeScript, Vite, Tailwind and shadcn/ui, using
  pnpm. The handwritten serde contract is in `ui/src/types.ts`.
- The root `Cargo.toml` lists `crates/sk1ll-gui/src-tauri` explicitly as a
  workspace member. Unix gating lives inside `src-tauri`.

For development, install Rust, pnpm and the Tauri v2 Unix prerequisites, including
GTK3 and WebKitGTK 4.1 on Linux, then start a joined Daemon:

```sh
systemctl --user start sk1ll-daemon  # or: sk1ll daemon
cd crates/sk1ll-gui/ui
pnpm install
pnpm tauri dev
# Override the socket when needed:
SK1LL_DAEMON_SOCKET=/absolute/path/to/daemon.sock pnpm tauri dev
```

For an isolated real Fleet with a Hub, two Machines, sample Skills, a Fragment
and a local-only Draft:

```sh
scripts/dev-fleet.sh           # prints the GUI command for its temporary socket
scripts/dev-fleet.sh --gui     # also launches pnpm tauri dev
```

The script builds Hub/Daemon binaries, isolates their HOME/XDG and git config,
and uses a temporary admin token to create join codes. Its Harness paths are
inside the temporary directory. Ctrl-C stops every process in the service
sessions, including git hooks, with a five-second grace period before SIGKILL,
and removes the Fleet. It never reads or writes the real skills or Harness directories.
Requires bash, curl, python3, git, setsid, ps and pkill. `SK1LL_DEV_PORT` optionally pins
the Hub port. `SK1LL_GUI_TRACE=1` logs command names and outcomes without payloads.

Build and check:

```sh
cd crates/sk1ll-gui/ui
pnpm tsc --noEmit
pnpm lint
pnpm test
pnpm build
pnpm tauri build --no-bundle
cd ../../..
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
python3 crates/sk1ll-gui/tests/test_dev_fleet.py
```

Build the frontend before invoking Cargo directly, since the app embeds
`ui/dist`. The linked app is `target/release/sk1ll-gui`. Automated tests cover
status precedence, Assignment state and rollback, command forwarding over a
fake Unix socket, Tauri IPC and event reconnect/backoff. Interactive desktop
flows still need manual testing against the development Fleet.

Sync's guarantees and their known gaps are in [docs/known-limitations.md](docs/known-limitations.md).

Still open, not yet designed:

- Imported instruction sets (same vendoring + lockfile as skills).
- Per-harness frontmatter transforms (in copy mode) for harness quirks.
