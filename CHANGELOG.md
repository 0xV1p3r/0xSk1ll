# Changelog

All notable changes to 0xSk1ll are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0, a minor version may
break compatibility.

## [Unreleased]

### Added

- Several config directories of one tool, such as a second Claude Code account
  started with `CLAUDE_CONFIG_DIR=~/.claude_2`: a `machine.toml` entry with
  `preset = "claude"` and `home = "~/.claude_2"` is a preset instance, which
  gets the preset's skills, rules, format and mode under that directory, its
  paths derived rather than written. `sk1ll harnesses add <name> --preset
  <preset> --home <dir>` adds one, and `sk1ll harnesses` shows what each is an
  instance of. `home` on a preset's own entry moves that preset.
- A Harness has a kind: the preset it is or is an instance of. A Rule's
  `harnesses: [claude]` reaches every Harness named or of kind `claude`.
- Config directories found in the environment: `CLAUDE_CONFIG_DIR`,
  `CODEX_HOME` or `COPILOT_HOME`, read by the `sk1ll` command and the app
  (the daemon rarely has your shell's variables), name a candidate preset
  instance. First-run setup offers it as on and writes its `preset` and
  `home`; anywhere else it is only listed (`sk1ll harnesses`, `--all`,
  Settings → Harnesses) until `sk1ll harnesses enable <name>` or **Enable**
  adds it after the usual preview. No directory is ever globbed.

### Changed

- Renaming a preset or a preset instance keeps its kind: the new entry is an
  instance (`preset = "<kind>"`) instead of a custom Harness with copied
  paths, and Rules naming the kind aren't reported as still routed to the old
  name.
- Changing the paths of an opt-in preset writes `preset = "<name>"` beside
  them instead of pinning all four settings.
- Machines report each Harness to the Server with its kind
  (`{"name", "kind"}`), and join requests carry the same. A Server and its
  Machines must be upgraded together.

## [0.2.0] - 2026-11-26

### Added

- The app updates itself from the GitHub releases. It checks at startup
  (off in Settings) and shows the new release with its notes. **Update and
  restart** installs the signed `.deb`, restarts the Daemon into it and
  relaunches the app. Releases carry `latest.json` and the `.deb`'s updater
  signature.
  0.1.0 has no updater: install the 0.2.0 `.deb` by hand once, and later
  releases arrive through the app.
- `sk1ll status` in a terminal says when a newer release is out, checking at
  most once a day (`SK1LL_NO_UPDATE_CHECK=1` turns it off).
- The Server names its release on every response (`sk1ll-version`). A
  protocol mismatch is its own connection state, `incompatible`, which says
  the Server's release and which side to upgrade. The app warns when the
  Server is out of date or newer than this Machine, when a compatible Server
  runs an older release, and when the Daemon is older than the app.
  `sk1ll status` shows the Server's release.
- Keep mine for a Blocked pull in one step: `sk1ll pull --keep <items>` or
  `--keep-all`, and **Keep mine** in the app's Blocked notice, take the new
  Baseline and put this Machine's version of the clashing items back on top
  as Draft. A keep that is interrupted leaves your versions in recovery, and
  the next pull or reconcile reports each one for `sk1ll recover adopt`.
- `sk1ll harnesses disable <h> --take-out` (and `remove`), and **Take out and
  turn off** in Settings, take out what sk1ll placed in a Harness before
  changing `machine.toml`, and change nothing there if that fails.
- Sync removes a block instructions file it created once turning every rule
  off leaves it empty and untouched.
- List and clear orphan Assignment toggles in the desktop matrix or with
  `sk1ll fleet orphans` and `sk1ll fleet prune-orphans`.
- `sk1ll repair-ids` gives items that share an Item ID fresh ones in the
  Draft. Commit and Pull refuse duplicate Item IDs.
- `sk1ll harnesses rename <old> <new>` moves a Harness's Assignment
  and renames its table in `machine.toml`.

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
  can remove managed placements.
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

- Every `sk1ll` command goes through the daemon's engine: the running
  daemon's, else one the command runs in its own process (ADR 0008). Without
  a daemon, commands now sync the harnesses as the daemon would, `fleet`
  commands work, and a joined Machine connects to the server only for
  Assignment writes, renames and unmanaged Adopt. The CLI needs Unix.
- Assignment toggles use Item IDs. Renaming a Skill or Rule preserves its
  Assignment on every Machine. Item IDs are always explicit: the daemon and
  mutating commands write a fresh ID to any item that lacks one, so an item
  deleted and re-created, by hand or through sk1ll, starts from the
  defaults.
- Pull rolls forward: it replays unpushed commits without touching the
  working tree, then updates only the paths the new Baseline changes. An
  interrupted Pull is finished by the next `sk1ll pull` or Daemon refresh,
  and content it replaced goes to the recovery area. sk1ll needs Git 2.40
  or later.
- A Daemon sends its Machine's Assignment writes over the same WebSocket
  as status, so Adopt reports its Draft before enabling it.
- The skills repo no longer supports git filter drivers (Git LFS): the
  server has no LFS endpoint. A pull onto a Baseline that writes a file
  through one stays Blocked and says to remove `filter=` from
  `.gitattributes`, and the server refuses such a push. Pull, the clone and
  the first Baseline run no filter driver. Imports keep their LFS handling.

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

### Removed

- The pull's filter-driver conversion cache (`.git/sk1ll-pull-filtered/`)
  and its retries.
- The `inventory` WebSocket frame, which nothing sent since every command
  runs through an engine that reports full status.
- Push-time toggle sweeping and the Restore notification outbox.
- Assignment copying during item rename and its offline override.
- Compatibility fallbacks for older Assignment and join-code formats.

### Fixed

- Recovered content: the original of an adopted unmanaged skill can be
  adopted (back into that skill) and its diff shows, where both failed for
  want of a target. Content Adopt can't place is reported as not adoptable,
  and the app offers only Discard and a diff against nothing. An original
  holding what the copy left out (a `.git`, extended attributes) stays as
  changed content, and Recover Adopt keeps it.
- After **Overwrite** (or `sync --force`) of a hand edit, the kept copy is
  listed only as recovered content: `GET /conflicts` lists Harness
  conflicts alone, and `POST /sync` returns changed recovery entries as
  `kept`. It is marked `overwritten` and **Overwritten** in the app: it
  stays until adopted or discarded, but no longer makes the Machine a
  Conflict, in its status, the Fleet's count or the badge.
- A clean pull no longer leaves a permanent recovery entry for each file it
  replaced: an entry for a file that held exactly what checking out the old
  commit writes is a routine copy, pruned after the grace period once the
  pull is finished. A write since, other bytes (an edit inside an expanded
  `$Id$`), a mode or an extended attribute Git can't restore keeps it,
  changed.
  The app marks recovered content **Changed** or **Unchanged**.
- Adopt keeps the repo version it replaces as changed recovery content
  unless that was exactly the committed version (of a skills repo in a Git
  subdirectory too), so a Draft edit or deletion it replaces is never
  pruned, and Recover Adopt puts it back into its item. It no longer prunes its own input on the way,
  and refuses a recovered rule or instructions file that isn't UTF-8
  instead of rewriting it with replacement characters.
- The recovery grace period runs from when a displacement completed, and no
  recovery entry is pruned before it is over: `SK1LL_DISPLACED_MAX` and the
  size cap that pruned young entries are gone.
- The app's Conflicts badge counts what the Fleet table's Conflicts column
  counts: Harness conflicts and recovered content holding a change, not
  routine copies.
- A harness enabled on a joined Machine starts all off, as the Server
  assigns it. Before, the first sync placed every item in it and took them
  all out into recovery once the Server's column arrived, and the Settings
  preview listed everything as placed.
- Taking out a harness whose instructions file sk1ll created no longer
  leaves an empty file behind after an Overwrite of a hand edit to its
  block.
- CLI messages: `sk1ll status` says when the Machine is Blocked (and how
  to clear it), behind, or had a push refused; `sync` exits non-zero while
  it leaves conflicts alone and `sync --force` says which recovery entry
  kept each edit; status and sync print rules the same way
  (`rule git -> <path>`); every duplicate Item ID error suggests
  `sk1ll repair-ids`; renaming a harness onto one that exists no longer
  suggests `--replace`, which can't help there, and `harnesses rename`
  accepts `--yes`; empty `SK1LL_*` variables count as unset; a `--repo`
  that isn't there is named as missing; `sk1ll init` beside a daemon for
  another repo says so; `fleet log` prints dates and readable causes; the
  `recover` subcommands and their `<ID>` have help; a standalone commit
  without `origin` no longer tells you to push to a fleet; adopting a rule
  no longer adds a blank line after its frontmatter.
- The recovery area's index and the import cache follow `SK1LL_STATE_DIR`
  and the new `SK1LL_CACHE_DIR` (and the daemon's configuration) instead of
  always the platform's directories. The test suite no longer writes under
  the real home directory, and `scripts/check-test-home.sh`, which CI runs,
  fails if any test does.
- Restoring both sides of a case-only Skill or Rule rename on a
  case-insensitive filesystem puts the Baseline's spelling back, even when
  the directory holds an ignored file, with no-replace renames that move
  everything in it along.
- A commit can repair Item ID metadata the Baseline can't read (a manifest
  committed as invalid TOML, a rule with broken frontmatter): it is checked
  against the rest of `HEAD`, and the tree it makes is checked strictly.
- `sk1ll adopt` of a hand edit finds its target by the Item ID the
  placement recorded, as recovery's Adopt does: an edit to the harness copy
  of a skill or rule renamed since goes into the renamed item instead of
  recreating the old name. An ID that leads to no item, or to several, is
  refused with what was found.
- `sk1ll commit` refuses a tree the server would refuse for good (items
  without an Item ID of their own, files through a filter driver), with the
  server's own check and message. A push the server refuses anyway is no
  longer reported as queued or retried after a pull: the command fails, and
  status, `sk1ll fleet` and the app show *push refused* with the reason
  until a fix is committed on top.
- Rewriting a block instructions file (a rule toggled or changed) no
  longer leaves a recovery entry that is never pruned each time; only a
  forced rewrite of a block edited by hand keeps the old file, marked
  changed, until it is adopted or discarded.
- `sk1ll draft`, `sk1ll conflicts` and `sk1ll recover list` only read:
  without a daemon they no longer write missing Item IDs or sync the
  harnesses, they answer beside another engine (a daemon for another repo
  included), and their Draft, conflicts and recovery entries are planned in
  memory. On a joined Machine they read another selected repo as a
  standalone one.
- `sk1ll commit` of an item the pull is Blocked on no longer says the daemon
  pushes it when it can: the pull before the push turns such a commit back
  into Draft, and the command fails saying so and how to go on.
- A Blocked Machine that committed the clashing item could only get out
  with `git reset`: pull refused it with a hint to commit or restore, and
  restore found nothing in the Draft. Pull now turns an unpushed commit
  that conflicts with the new Baseline (and the commits after it) back into
  Draft changes, moving only the branch, and is Blocked on them as on any
  Draft clash. Restoring an item a pull is Blocked on keeps your version in
  recovery, to bring back with `sk1ll recover adopt` after the pull. Every
  Blocked message in the CLI, the push error and the app now gives that
  route.
- The daemon starts when its state file holds a pull record it can't read
  (a pending pull another version recorded without its Baseline): it drops
  that record with a warning and derives the pull's state from its journal
  and `HEAD`.
- A conflict's diff requested while the daemon was busy no longer answers
  that the conflict is gone.
- Daemon state writes use exclusive private staging files. Failed desktop
  notifications keep their retry schedule across Daemon restarts.
- The Library keeps unsaved text through a Daemon disconnect and restores
  keyboard focus when editing ends.
- Reviewed Import updates require a dedicated Daemon route. Harness Apply
  reports when either client or Daemon is too old for a fingerprint. Reviewed
  Sync verifies staged copies before publishing and requests a new preview if
  the staged bytes changed. The new preview shows completed placements and
  earlier errors. Settings can open `machine.toml` even when it fails
  to parse.
- The development GUI shows historical file contents after a working-copy save.
- Assignment edits check revisions. Copy Assignment can recover from a
  conflict within the same dialog.

- When local edits prevent an Import update, the desktop app offers to fork
  the Import or overwrite the edits after confirmation.
- Machine entries in the desktop command palette open the selected Machine.
- Desktop messages identify origin for standalone pushes, explain when Fleet
  deletions reach other Machines, and clarify how to join as the first Machine.
- A push queued behind its Machine's removal is refused with `401`, and a
  removal waits for a push in progress. A delayed WebSocket upgrade or
  revocation for a removed token can't end a rejoined Machine's session.
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
  the matrix can discard late replies from a prior membership.
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

[0.2.0]: https://github.com/0xV1p3r/0xSk1ll/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/0xV1p3r/0xSk1ll/releases/tag/v0.1.0
