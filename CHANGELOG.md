# Changelog

All notable changes to 0xSk1ll are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0, a minor version may
break compatibility.

## [Unreleased]

### Added

- Create a Rule from the desktop getting-started checklist and reopen the
  checklist from the command palette.
- Copy diagnostics and ready-to-paste join commands from the desktop app.
  Join code entry points explain Fleet permissions and the dialog shows the
  default expiry.
- Restart a systemd user Daemon from the desktop app after a local API mismatch.
- CLI commands to create join codes, remove machines, and view or undo Assignment log entries.
- Shell completion generation with `sk1ll completions <shell>`.

### Changed

- CLI setup explains how to join as the first machine and shows manual sync commands when the daemon is skipped.
- `sk1ll sync --force` help lists managed instruction blocks, skill copies, and rule files.

### Fixed

- Machine entries in the desktop command palette open the selected Machine.
- Desktop messages identify origin for standalone pushes, explain when Fleet
  deletions reach other Machines, and clarify how to join as the first Machine.

## [0.1.0] - 2026-11-17

The first release, for private testing. Linux only for the desktop app, the
daemon and fleets; the CLI also builds elsewhere in standalone mode.

### Added

- One git repo for skills and rules: personal skills, imports vendored from a
  git source and pinned in `sk1ll.lock`, and project-agnostic rules rendered
  in file-name order. It lives in `~/.local/share/sk1ll/repo` unless another
  is picked.
- `sk1ll add` imports a skill from a GitHub repo, folder or `SKILL.md` link,
  `github:owner/repo`, a git URL or a local path; `--list` shows what a
  multi-skill repo holds. `update`, `remove` and `fork` manage imports.
- Sync into Claude Code, Codex, OpenCode, Gemini and custom harnesses. Sync
  edits only what it placed, reports hand edits as conflicts, and moves
  displaced content to a recovery area instead of deleting it.
- Draft and baseline: uncommitted changes apply on this machine right away;
  `draft`, `diff`, `commit`, `push`, `restore` and `pull` work per item, and
  `pull` never merges.
- Standalone operation without a fleet, with machine-local assignment.
- Fleets: a self-hosted server (Docker) as the only git remote, holding each
  machine's assignment and its log, with single-use join codes, per-machine
  tokens and revocation. A daemon on each machine pulls as soon as another
  machine pushes and keeps working offline.
- The desktop app (Tauri): Fleet, Assignment matrix, Draft, Conflicts,
  Imports, Skills, Log and Settings screens, light and dark themes, and
  editor launching with installed editor icons.
- Guided first-run setup: `sk1ll setup` and the app's setup wizard create a
  standalone repo, join a fleet or start one, choose harnesses and install
  the daemon as a systemd user service.
- The sk1ll skill, which teaches agents to add skills with the CLI. A copy
  ships inside `sk1ll`: if its link can't be fetched, `sk1ll add` installs
  that copy as a personal skill.
- Protocol versions between the server and daemons, and between the daemon
  and the CLI or app: a mismatched peer is refused with an explanation
  instead of being misread.
- `--version` on every binary, `/healthz` and the app's Diagnostics name the
  release and the commit it was built from.

[0.1.0]: https://github.com/0xV1p3r/0xSk1ll/releases/tag/v0.1.0
