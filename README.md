# 0xSk1ll

0xSk1ll manages personal and imported agent skills (`SKILL.md` folders) and
project-agnostic agent instructions ("for X do Y") in one git repo, and syncs them
into every harness on every machine.

```
~/skills/
  sk1ll.toml        # imports, targets (harnesses), profiles (per machine)
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
sk1ll render <target>           # preview a target's instruction files
sk1ll sync [--dry-run] [--force] [--profile work]
```

Cross-machine sync: `git pull && sk1ll sync` (e.g. in a `post-merge` hook).

## Targets

A target is a harness: a skills directory, a global instructions location, or both.

```toml
[targets.claude]
skills = "~/.claude/skills"
instructions = "~/.claude/rules"
instructions_format = "rules"

[targets.codex]
skills = "~/.codex/skills"
instructions = "~/.codex/AGENTS.md"
mode = "copy"   # place skills as marked copies instead of symlinks

[profiles.work]
exclude_skills = ["personal-*"]
instructions = ["*"]
targets = ["claude"]   # empty = all targets
```

## Instructions

Each `instructions/*.md` file is one fragment. Optional frontmatter:

```markdown
---
description: Git conventions
targets: [claude]   # only render into these targets; default: all
---
For commits: use Conventional Commits.
```

There's no cross-harness global `AGENTS.md`; each harness has its own location,
so each target picks an `instructions_format`:

| Format | Writes | Use for |
|---|---|---|
| `block` (default) | A marked block inside one file; the rest of the file is left alone. Writes follow symlinks, so dotfile-linked files work. | `~/.codex/AGENTS.md`, `~/.config/opencode/AGENTS.md`, `~/.gemini/GEMINI.md` |
| `rules` | One copied file per fragment, tracked in a hidden `.sk1ll-managed` index | Claude Code's `~/.claude/rules/`, which loads every `.md` file in it for all projects |

`rules` keeps fragment frontmatter except `targets`, so Claude's `paths:` works for
path-scoped rules. It copies rather than symlinks because Claude Code skips
symlinked rule files in some sessions (Cowork). Rule files you wrote yourself are
never touched; a name clash is reported as a conflict.

Both formats record hashes. If you edit managed content directly in the harness,
sync reports a conflict instead of overwriting it. Move the edit into
`instructions/`, or pass `--force` to overwrite it.

## Layout

- `crates/sk1ll-core`: all logic. No printing; public types are `Serialize`.
  `sync::plan` is side-effect free, and `sync::apply` re-verifies state before
  touching anything. 0xSk1ll only ever modifies symlinks into the repo, copies
  marked with `.sk1ll-managed`, its own block in instruction files, and rule
  files listed in a rules directory's `.sk1ll-managed` index.
- `crates/sk1ll-cli`: thin `clap` front-end.

## Roadmap

- `sk1ll-gui`: Tauri app; each core fn becomes a `#[tauri::command]`, and the
  plan → confirm → apply flow maps to a diff view.
- `sk1ll-daemon`: live sync. Watch `personal/`, `instructions/` and targets
  with `notify`, then re-plan/apply; periodically `git pull --rebase`,
  autocommit and push personal edits; optionally poll import refs and notify
  when updates exist.
- Imported instruction sets (same vendoring + lockfile as skills).
- Per-target frontmatter transforms (in copy mode) for harness quirks.
