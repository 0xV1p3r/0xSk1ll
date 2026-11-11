# Configuration

## Harnesses

A harness is an agent tool on this machine with a skills directory, a global instructions location, or both. Harnesses are machine-local and never stored in the repo. `sk1ll harnesses` lists the ones this machine has.

sk1ll includes a built-in preset when its directory exists:

| Preset | Detected by | Skills | Instructions |
|---|---|---|---|
| `claude` | `~/.claude` | `~/.claude/skills` | `~/.claude/rules` (`rules` format) |
| `codex` | `~/.codex` | `~/.codex/skills` (copy mode) | `~/.codex/AGENTS.md` |
| `opencode` | `~/.config/opencode` | `~/.config/opencode/skill` | `~/.config/opencode/AGENTS.md` |
| `gemini` | `~/.gemini` | none | `~/.gemini/GEMINI.md` |

## machine.toml

`~/.config/sk1ll/machine.toml` adjusts the presets and adds harnesses. Override its location with `--machine-config` or `SK1LL_MACHINE_CONFIG`.

An entry named after a preset changes only the keys it sets, and includes the preset even when its directory wasn't detected. Any other name adds a new harness.

```toml
id = "workstation"   # machine identity; default: the hostname

[harnesses.claude]
mode = "copy"        # place skills as marked copies instead of symlinks

[harnesses.gemini]
enabled = false      # don't sync into a detected preset

[harnesses.cursor]
skills = "~/.cursor/skills"
instructions = ""    # an empty string clears a preset's path
```

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

A standalone repo keeps assignment in `~/.cache/sk1ll/assignment.json` (override with `--assignment` or `SK1LL_ASSIGNMENT`). A joined machine sends every change to the server, which keeps a log with per-entry undo. [ADR 0001](adr/0001-assignment-is-server-state.md) explains why assignment isn't stored in the repo.
