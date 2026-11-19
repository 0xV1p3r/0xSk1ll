# Configuration

## Harnesses

A harness is an agent tool on this machine with a skills directory, a global instructions location, or both. Harnesses are machine-local and never stored in the repo. `sk1ll harnesses` lists the ones sync writes into. `sk1ll harnesses --all` also lists the ones turned off and the presets that are available but off.

sk1ll includes a built-in preset when its directory exists (an opt-in preset only once enabled, see below):

| Preset | Detected by | Skills | Instructions |
|---|---|---|---|
| `claude` | `~/.claude` | `~/.claude/skills` | `~/.claude/rules` (`rules` format) |
| `codex` | `~/.codex` | `~/.codex/skills` (copy mode) | `~/.codex/AGENTS.md` |
| `opencode` | `~/.config/opencode` | `~/.config/opencode/skill` | `~/.config/opencode/AGENTS.md` |
| `gemini` | `~/.gemini` | `~/.gemini/skills` (opt-in) | `~/.gemini/GEMINI.md` |
| `copilot` | `~/.copilot` | `~/.copilot/skills` (copy mode) | `~/.copilot/copilot-instructions.md` |
| `cursor` | `~/.cursor` | `~/.cursor/skills` (copy mode) | none (Cursor keeps user rules in its settings) |
| `amp` | `~/.config/amp` | `~/.config/amp/skills` (copy mode) | `~/.config/amp/AGENTS.md` |
| `kiro` | `~/.kiro` | `~/.kiro/skills` (copy mode) | `~/.kiro/steering` (`rules` format) |
| `cline` | `~/.cline` | `~/.cline/skills` (copy mode) | none |
| `roo` | `~/.roo` | `~/.roo/skills` | none |

### Opt-in presets

The presets added after 0.1.0 (`copilot`, `cursor`, `amp`, `kiro`, `cline` and `roo`) and gemini's skills directory are opt-in, so an upgrade never starts syncing somewhere new. An opt-in preset is synced into only while `machine.toml` enables it explicitly:

```toml
[harnesses.kiro]
enabled = true

[harnesses.gemini]
skills = "~/.gemini/skills"   # gemini's skills directory; without it gemini gets its instructions only
```

First-run setup (the GUI wizard, `sk1ll setup`, joining a fleet without a repo, or `sk1ll init` on a machine without a `machine.toml`) offers the opt-in presets it finds as on and writes these lines for the ones you keep. Setup skips any candidate whose destinations overlap an enabled harness or the skills repo and reports the overlap. `sk1ll init` on a machine that already has a `machine.toml` adds nothing. Anywhere else, an opt-in preset found on the machine is listed as available: `sk1ll harnesses` mentions it on stderr, and `sk1ll harnesses --all` and Settings list it with **Enable**. `sk1ll harnesses enable <name>` writes the lines above after the usual preview; for gemini, it adds the skills directory unless the entry sets `skills` already (`skills = ""` keeps gemini to its instructions).

The presets of 0.1.0 (`claude`, `codex`, `opencode`, and `gemini`'s instructions) are still included whenever their directory exists.

For an opt-in preset name, an entry with either path key (`skills` or `instructions`) defines a custom harness, even with `enabled = true`. Missing keys use the custom harness defaults. An entry with no path keys and `enabled = true` activates the preset. To switch a custom harness back to the preset, remove the entry and enable the preset. Changing the paths of an enabled opt-in preset pins all four settings in `machine.toml`: `skills`, `instructions`, `instructions_format`, and `mode`. Future changes to preset defaults won't apply to that entry.

Presets use copy mode where the tool's documentation doesn't say it follows skill symlinks. If you set `COPILOT_HOME`, point the `copilot` paths there in `machine.toml`. A Kiro steering file without front matter is always included. The Kiro CLI may load only files marked `inclusion: always`.

Several tools also read other tools' directories. Cursor reads `~/.claude/skills` and `~/.codex/skills`, Amp and Copilot read `~/.claude/skills`, and most of them read the shared `~/.agents/skills`. With both harnesses on, such a tool sees a skill twice. Turn one harness off, or narrow its assignment.

## machine.toml

`~/.config/sk1ll/machine.toml` on Linux and `~/Library/Application Support/sk1ll/machine.toml` on macOS adjusts the presets and adds harnesses. Override its location with `--machine-config` or `SK1LL_MACHINE_CONFIG`.

An entry named after a preset from 0.1.0 changes only the keys it sets, and includes the preset even when its directory wasn't detected. Any other name adds a new harness.

```toml
id = "workstation"   # machine identity; default: the hostname

[harnesses.claude]
mode = "copy"        # place skills as marked copies instead of symlinks

[harnesses.gemini]
enabled = false      # don't sync into a detected preset

[harnesses.codex]
instructions = ""    # an empty string clears a preset's path

[harnesses.agents]   # a custom harness: the directory several tools share
skills = "~/.agents/skills"
mode = "copy"
```

A custom harness name uses lowercase letters, digits, `-` and `_`. Paths are absolute or start with `~/`, contain no `..`, and aren't the home directory or `/`, however a symlink spells them. Each location is compared where it resolves: no location may be inside another or equal to it, in the same harness or across harnesses, and none overlaps the skills repo. sk1ll checks this whenever a change turns a harness on (enable, add, edit, or resetting a preset). Turning a harness off or removing it is always allowed, so a bad entry can still be fixed.

### Paths with `..`

sk1ll refuses a path with `..` when a change turns a harness on, because after a symlink `..` means the target's parent, not the parent as written. An entry written before keeps working until you change it. To change it, use the destination the path resolves to in the filesystem: `realpath -m <path>` prints it (on macOS, `realpath` without `-m` for a path that exists). For example, if `~/tools` is a symlink to `/opt/tools`, `~/tools/../agents/skills` resolves to `/opt/agents/skills`, not `~/agents/skills`. Write that resolved path in `machine.toml`, or pass it to `sk1ll harnesses add <name> --skills /opt/agents/skills`.

## Changing harnesses

Change harnesses at any time in Settings, or with `sk1ll harnesses`:

```sh
sk1ll harnesses --all                             # also turned-off harnesses and available presets
sk1ll harnesses disable gemini                    # stop syncing into it
sk1ll harnesses enable gemini                     # sync into it again
sk1ll harnesses add agents --skills ~/.agents/skills --mode copy
sk1ll harnesses add codex --instructions ''       # adjust a preset; options left out keep their value
sk1ll harnesses remove agents                     # drop the entry; a preset goes back to its defaults
```

Each change edits `machine.toml` in place, so comments and layout survive. Setting a value back to the preset's removes the override; a harness that was synced into stays on (with `enabled = true` if nothing else would keep it). Before writing, sk1ll shows two lists:

- **Already placed.** What sync put where the change stops syncing: a turned-off or removed harness, or a path the harness no longer uses. A change never removes anything: these items stay where they are, and sk1ll stops managing them. Copies edited by hand are marked.
- **Then sync.** What the changed harness receives next.

To take placed content out, turn the harness's items off before the change, so the normal sync removes them with all its checks (each removal goes through the recovery area, and a hand-edited copy stays):

```sh
sk1ll assign kiro --all off     # every skill and rule, and both defaults
sk1ll sync                      # not needed when the daemon runs
sk1ll harnesses disable kiro    # nothing is left behind now
```

In Settings, **Turn off this Harness's items first** in the preview does the same in the Assignment (on the server for a joined machine).

The change is then applied and the harnesses synced. It goes through the daemon when one manages the repo. The CLI asks for confirmation on a terminal. Pass `--yes` in scripts. Assignment is left unchanged, so turning a harness back on restores its toggles. A joined machine reports its new set of harnesses to the server with its next status.

`machine.toml` is read once for a change and written only if it is still exactly what sk1ll read (same file, bytes and permissions); otherwise nothing is written and the change fails (409 from the daemon), so you can retry. sk1ll writes the new text to a temporary file only it created, next to the file (a symlinked `machine.toml` stays a link, and its target gets the new text), with the file's permissions (0600 for a new file) and, where allowed, its owner and group, then renames it into place. ACLs and extended attributes are not kept. A save that lands in the instant between that last check and the rename is lost; see [Known limitations](known-limitations.md).

If the sync after the change can't place something, the change itself still stands. The CLI lists each failure and exits nonzero, and Settings lists them before closing.

An edit to `machine.toml` by hand has the same effect on sync, without the preview. Content in a dropped location just stays there.

## Rules

Each `instructions/<name>.md` file is one rule. Frontmatter is optional:

```markdown
---
description: Git conventions
harnesses: [claude]   # render only into these harnesses; default: all
---
For commits, use Conventional Commits.
```

`harnesses:` narrows a rule further on top of assignment. The older `targets:` key still works as a deprecated alias.

## Instruction formats

Harnesses don't share one global `AGENTS.md`, so each harness sets an `instructions_format`:

| Format | What sync writes | Used by |
|---|---|---|
| `block` (default) | One marked block inside a single file. Sync leaves the rest of the file alone and follows symlinks, so dotfile-managed files keep working. | `~/.codex/AGENTS.md`, `~/.config/opencode/AGENTS.md`, `~/.gemini/GEMINI.md` |
| `rules` | One copied file per rule, listed in a hidden `.sk1ll-managed` index | `~/.claude/rules/`, which Claude Code loads for every project |

The `rules` format keeps each rule's frontmatter except `harnesses`, so Claude's `paths:` key still scopes a rule to matching files. It copies instead of symlinking because Claude Code skips symlinked rule files in some sessions (Cowork). Sync never touches rule files you wrote yourself and reports a name clash as a conflict.

`sk1ll render <harness>` prints what a harness would receive.

## Conflicts and adopt

Sync records a hash for every block, rule file and copied skill it writes. If the harness copy no longer matches, sync reports a conflict instead of overwriting it. You then have two choices:

- `sk1ll adopt <harness> <name>` copies the edited skill or rule file back into the repo as a draft change and restores the rule's `harnesses:` key.
- `sk1ll sync --force` overwrites the edit.

A `block` mixes several rules, so it can't be adopted. Move that edit into `instructions/` by hand.

`adopt` also takes an unmanaged skill, one that sits in a harness's skills directory without sk1ll tracking it. sk1ll copies it to `personal/<name>/` as a draft. Identical copies in other harnesses are adopted at the same time and explicitly enabled there. Copies with different content stay unmanaged. Sync moves the original copies into recovery (see [known limitations](known-limitations.md)), and other harnesses follow their default for new skills.

`adopt` refuses a name that already exists in the repo. The exception is an identical personal copy, so you can retry an interrupted adopt. On a joined machine, adopt needs a running daemon connected to the server. The [Skills API](skills-api.md#adopt-and-fork) describes the exact sequence.

## Assignment

Assignment decides which skills and rules each harness gets. Every item starts enabled. Each harness also has a default for items added later.

```sh
sk1ll assign claude skill pdf off           # one toggle
sk1ll assign codex --default-skills off     # default for new skills
sk1ll assign codex --default-rules on       # default for new rules
```

Changing a default first records every existing item's current value as an explicit toggle, so existing items keep their state.

A standalone machine keeps assignment in `~/.cache/sk1ll/assignment.json` on Linux or `~/Library/Caches/sk1ll/assignment.json` on macOS (override with `--assignment` or `SK1LL_ASSIGNMENT`). The daemon watches this file and applies changes from either the GUI or CLI. There is no standalone assignment log or undo. [ADR 0004](adr/0004-standalone-assignment-is-machine-local.md) records this choice. A joined machine sends every change to the server, which keeps a log with per-entry undo. [ADR 0001](adr/0001-assignment-is-server-state.md) explains why assignment isn't stored in the repo.

Join codes expire after `SK1LL_SERVER_JOIN_CODE_TTL` (default `24h`), using the server duration syntax.
