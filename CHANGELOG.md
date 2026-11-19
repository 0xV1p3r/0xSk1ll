# Changelog

All notable changes to 0xSk1ll are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0, a minor version may
break compatibility.

## [Unreleased]

### Added

- Edit Personal Skill text files and Rules in the desktop Library, with a diff
  before overwriting a concurrent disk edit and a link to review the Draft.
- Choose a branch, tag or commit in Add Import. Preview upstream commits and
  changed files in Imports or with `sk1ll update <name> --dry-run`.
- Copy a joined Machine's Assignment from the CLI or Assignment matrix.
  Preview the changes, map Harnesses and undo individual changes in the log.
- Preview a Harness's rendered instructions and on-disk diff in Settings.
  **Sync now** shows a dry-run plan before applying from Settings or the
  command palette.
- Desktop notifications for Machine states that need attention, with an opt-out
  in Settings and `machine.toml`.
- macOS arm64 builds in CI and release archives with the unsigned desktop app,
  CLI and server. Setup and the desktop app can start the daemon through launchd.
- Create a Rule from the desktop getting-started checklist and reopen the
  checklist from the command palette.
- Copy diagnostics and ready-to-paste join commands from the desktop app.
- Restart a systemd user Daemon from the desktop app after a local API mismatch.
- CLI commands to create join codes, remove Machines, and view or undo
  Assignment log entries.
- Shell completion generation with `sk1ll completions <shell>`.
- Join-code scopes. A `self` code limits the new Machine to its own
  Assignment, log entries and membership. `fleet` is the default and keeps
  existing Machines' access (ADR 0005). The CLI and app show when the code
  expires, and the Fleet screen shows each Machine's scope.
- Server settings for import checks: `SK1LL_SERVER_IMPORT_CHECKS=off` and the
  host allowlist `SK1LL_SERVER_IMPORT_HOSTS`. Skipped Imports are reported with
  a reason and shown on the desktop Imports screen.
- Change Harnesses after setup in Settings or with
  `sk1ll harnesses enable|disable|add|remove`. Preview which placements
  stay behind and what the next sync will place. Changes preserve
  `machine.toml` comments and report failed placements.
- `sk1ll assign <harness> --all on|off` sets every Skill and Rule and both
  defaults, including Baseline items the joined Machine has yet to pull.
  Settings offers to turn off the items before disabling a Harness so sync
  can remove managed placements. Older Servers use the local item list and
  warn that unpulled items may stay on.
- Opt-in presets for GitHub Copilot CLI (`copilot`), Cursor (`cursor`), Amp
  (`amp`), Kiro (`kiro`), Cline (`cline`) and Roo Code (`roo`), and Gemini's
  skills directory (`~/.gemini/skills`). They sync only while
  `machine.toml` enables them (`enabled = true`; for gemini,
  `skills = "~/.gemini/skills"`), so an upgraded machine syncs only where it
  did before. First-run setup enables the ones it finds and you keep;
  elsewhere they are listed as available. An existing custom harness with
  such a name keeps its own paths.
- `sk1ll harnesses --all` also lists disabled Harnesses, available presets
  and presets that were not detected.
- Settings shows the resolved `machine.toml` path and opens it in the
  preferred editor.

### Changed

- Add Import and `sk1ll add --list` show file sizes and warn about scripts,
  executable files, binaries and files over 1 MiB.
- Machines that joined with a `self` join code get `403` for creating join
  codes or managing other Machines.
- The server accepts pushes only to the baseline branch, the one HEAD names
  when it starts; pushes to tags, other branches, refs under the branch's name
  or `refs/replace/` are refused, also by a `pre-receive` hook the server
  writes over on every start. Replacement refs already in the repo are ignored
  and logged at startup.
- The server refuses to start when the repo's HEAD is detached or names
  something outside `refs/heads/`, and says how to fix it. Changing HEAD needs
  a restart.
- CLI setup explains how to join as the first machine and shows manual sync commands when the daemon is skipped.
- `sk1ll sync --force` help lists managed instruction blocks, skill copies, and rule files.
- `machine.toml` is written through a temporary file only sk1ll created,
  and only if the file is still exactly as read (bytes, identity and
  permissions); it keeps its permissions and symlink. See known limitations
  for the instant between that check and the rename.
- Sync's own bookkeeping (rules and links indexes, the assignment cache) is
  staged the same way, never through whatever holds the temporary name.
- Harness paths may not contain `..`, and locations may not be nested in one
  another. The home directory, `/` and the repo are compared where paths
  resolve. See the configuration docs for rewriting an existing entry.
- Sync treats a rule file replaced by a symlink, or anything that isn't a
  plain file, as someone else's, even when its text is identical, and never
  blocks on a FIFO there.
- A dangling symlink at the assignment snapshot is an error, not a missing
  snapshot.

### Fixed

- Daemon state writes use exclusive private staging files. Failed desktop
  notifications keep their retry schedule across Daemon restarts.
- Harness-wide Assignment and rename-copy require a current Baseline inventory.
  A structured inventory refusal tells rename-copy to retry. Harness-wide
  changes report skipped names in standalone and direct CLI use. Settings
  shows local placement errors after saving an Assignment, and retries sync
  even when the revision is unchanged.
- The Library keeps unsaved text through a Daemon disconnect and restores
  keyboard focus when editing ends.
- Reviewed Import updates require a dedicated Daemon route. Harness Apply
  reports when either client or Daemon is too old for a fingerprint. Reviewed
  Sync verifies staged copies before publishing and requests a new preview if
  the staged bytes changed. Settings can open `machine.toml` even when it fails
  to parse.
- The development GUI shows historical file contents after a working-copy save.
- Assignment edits check revisions on supported authorities. The matrix warns
  before unguarded bulk edits, and Copy Assignment can recover from a conflict
  within the same dialog.

- When local edits prevent an Import update, the desktop app offers to fork
  the Import or overwrite the edits after confirmation.
- Starting the macOS daemon recreates its log directory and repairs generated
  LaunchAgents after the app moves. New agents include the Git directory found
  on PATH, and the desktop app shows start commands for the detected manager.
- Machine entries in the desktop command palette open the selected Machine.
- Desktop messages identify origin for standalone pushes, explain when Fleet
  deletions reach other Machines, and clarify how to join as the first Machine.
- A push drops toggles only for the items it removed, and a push by a `self`
  Machine only its own; other Machines' toggles stay and apply again when the
  items come back. Unrelated pushes no longer drop toggles for drafts missing
  from a Machine's latest status.
- A push queued behind its Machine's removal is refused with `401`, and a
  removal waits for a push in progress. A delayed WebSocket upgrade or
  revocation for a removed token can't end a rejoined Machine's session.
- If the Server cannot list either tree for a push, that push drops no
  toggles. Later pushes leave those toggles alone. Repo reads have a
  deadline and a 64 MiB output cap. Each push and Draft discard also has
  a total read budget. A cancelled or timed-out read stops its git process
  group.
- A draft discard reads the baseline branch itself instead of the HEAD cached
  after the last push, so a push whose HEAD read failed can't make it drop a
  toggle for an item in the baseline. A failed read is no longer taken for an
  empty repo. While the baseline can't be read, the server drops nothing and
  closes the session with code 1013; the daemon keeps backing off and sends
  its unacknowledged discards again.
- An empty repo whose unborn HEAD named another branch takes pushes to the
  `main` branch the server creates in it.
- Server reads of the repo ignore replacement objects, so a replacement ref
  can't change what the server sees in the baseline.
- A Machine removal whose request goes away keeps pushes out until the
  removal is stored.
- Server writes check the caller's token and scope again inside their
  transaction, so a request racing its Machine's removal or a rejoin with a
  `self` code can't use the old scope.
- Import checks with `SK1LL_SERVER_IMPORT_HOSTS` refuse sources with percent
  escapes before the path or unusual ssh user names, and run `git ls-remote`
  without the server's git and ssh configuration, so neither can lead them
  to an unlisted host.
- Daemons are told when skipped or failed import checks change, so the
  desktop Imports screen shows newly skipped Imports.
- A set but empty, blank or non-UTF-8 `SK1LL_SERVER_IMPORT_HOSTS` stops the
  server from starting instead of allowing any host.
- The `curl` join-code example in the fleet guide prints a `self` code only
  when the server confirms the scope.
- Join codes are invalid from their `expires_at` second on, matching the
  reported expiry.
- The Assignment matrix loads and retries each Machine independently.
  Bulk changes apply only at the displayed revision. Undo checks the
  revision and registration again, and skips Machines that left, rejoined
  or changed. The Server assigns a new registration ID at each join so
  the matrix can discard late replies from a prior membership. Older
  Servers without registration IDs do not offer guarded Undo.
- A batch Import tries every selected skill instead of stopping at the first
  failure, even when the Daemon was unreachable for one, shows each as added,
  not added or not attempted (after Stop), reads back what reached the Draft
  and lets you retry only the rest. A failed response counts as added only
  when the Import's source, path, ref and locked commit match, and a retry
  checks the names with the Daemon first. A name taken since fails on its own
  while the others are still added, and a clash remembered from the last
  check never disables Retry.
- An interrupted Adopt can be resumed from the Library. `GET /skills` marks
  the unmanaged variant that matches the existing personal copy as
  `resumable`; other variants stay blocked.
- Renaming a Personal Skill or Rule copies this Machine's explicit toggles
  to the new name first. The Server handles the copy for joined Machines,
  and the Daemon handles it in standalone mode. Existing destination toggles
  are preserved. If the copy fails or its outcome is uncertain, the Library
  offers **Rename without copying Assignment**. If the copy succeeds and the
  rename moves nothing, cleanup uses the copy's revision and registration.

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
