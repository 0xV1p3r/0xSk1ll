# 0xSk1ll

Manages agent skills and instructions in one git repo and keeps every harness on every machine in step with it.

## Content

**Skill**:
A `SKILL.md` folder, either personal or imported.

**Import**:
A skill vendored from an external git source and pinned to a commit; updated from its source rather than edited (fork it to edit).
_Avoid_: Dependency, package, remote skill

**Unmanaged skill**:
A skill in a harness's skills directory that sk1ll did not put there and does not track; it is not in the repo.
_Avoid_: Installed skill, stray, foreign skill

**Rule**:
One instruction file ("for X do Y"), kept in `instructions/`. A harness's **instructions** are all its enabled rules rendered together (into a block or as rule files).
_Avoid_: Fragment, instruction (when meaning a single file)

**Item ID**:
The stable identifier of a personal skill, Rule or Import, always explicit: it is never derived from the item's name. Assignment toggles follow it through renames. sk1ll writes a fresh one, as a Draft change, to an item that lacks one; until then the item has no toggle and follows the defaults.
_Avoid_: UUID, key

## Where content goes

**Machine**:
One computer in the fleet, with a stable identity and its own harnesses.
_Avoid_: Profile, host

**Harness**:
An agent tool on a machine (Claude Code, Codex, ...) that receives skills and/or instructions.
_Avoid_: Target

**Fleet**:
All machines that share one repo.

**Assignment**:
Which skills and rules are enabled in which harness on which machine. It is live fleet state held by the server, not part of the repo, so it has no draft.
_Avoid_: Profile, selection, enablement

**Orphan toggle**:
An explicit Assignment toggle whose Item ID is absent from the Baseline and every Machine's last report. It is inert and can be cleared from the Assignment matrix or with `sk1ll fleet prune-orphans`.

**Server**:
The self-hosted fleet server: the fleet's git remote, which also holds its assignments and machine status and tells machines when to pull. Always means this one; the daemon's endpoint is its **local API**.
_Avoid_: Hub, relay, coordinator

**Opt-in preset**:
A built-in Harness configuration that needs explicit enablement in `machine.toml` before sync uses it. Gemini’s skills directory is also opt-in.

**Registration**:
A Machine’s membership instance in the Fleet. Each join gets a new registration ID, even when the Machine ID is reused. Assignment writes can check that ID to guard against a concurrent rejoin. The daemon tags its stored Assignment snapshot with its registration and ignores one of another.

## State

**Baseline**:
The committed, pushed state of the repo; the same for every machine.
_Avoid_: Git state, main

**Draft**:
Uncommitted changes to the repo on one machine; private to that machine and live in its harnesses.
_Avoid_: System state, local state, working copy

**Snapshot**:
The daemon's published view of reconciled state, used to answer reads.

**Full reconcile**:
A reconcile that plans and reads every tree again, whatever the fingerprint of its inputs and the hash cache say: at startup, then at most every ten minutes. It bounds how long a change file metadata cannot show goes unnoticed.
_Avoid_: Resync, rescan

**Commit**:
To record selected draft items as a new commit on the fleet branch. It reaches the fleet once it is pushed.
_Avoid_: Promote, publish

**Push**:
To send commits to the server, making them the new baseline.

**Pull**:
To fetch the latest baseline and carry the draft on top; it never merges, it stops as **Blocked** instead.

**Restore**:
To discard (part of) a draft and return it to baseline.
_Avoid_: Reset, revert, discard

**Conflict**:
Content in a harness that differs from what the machine's draft says it should be, and that sk1ll won't overwrite on its own.
_Avoid_: Drift

**Adopt**:
To copy content from a harness into the machine's draft: either a hand edit made to managed content in the harness, which goes into the item it was placed from (found by the Item ID the placement recorded, whatever the item is called now), or an unmanaged skill, which becomes a personal skill.
_Avoid_: Import (reserved for external git sources), take in

**Daemon**:
The background process on each machine that keeps its harnesses matching its draft and assignment, and talks to the server for it.
_Avoid_: Agent (reserved for the AI tools that harnesses run)

**Engine**:
What carries out every change to the repo, the harnesses and the assignment, and reconciles them: inside the daemon, or inside a single `sk1ll` command when no daemon runs. One engine at a time manages a Machine, whatever repo it is for: it holds the **machine lock** for its life.
_Avoid_: Direct mode

**Preview**:
A command that shows what a sync would do or what the repo holds (`status`, `render`, `sync --dry-run`, `list`, `draft`, `conflicts`, `recover list`) and changes nothing, not even what a reconcile would bring in line. It runs beside another engine.
_Avoid_: Dry run (only for `sync --dry-run` itself)

**Join code**:
A single-use code that lets a new machine enter the fleet and receive its own credential.
_Avoid_: Invite, fleet token

**Scope**:
What a machine may manage, set by the join code it joined with: `fleet` (the whole fleet) or `self` (only its own assignment, log entries and membership).
_Avoid_: Role, permission level

## Machine status

A machine has one headline status: the most severe that applies, in the order Offline, Blocked, Conflict, Behind, Drafting, In sync. **Conflict** is defined above.

**Offline**:
The server can't reach the machine; it shows a last-seen time.

**Blocked**:
A new baseline clashes with the machine's draft, so the machine stays on its old baseline. Unpushed commits that conflict with it are turned back into Draft first, so they block the same way. Restoring the clashing items keeps their version in recovery, to adopt after the pull. A pull also stops as Blocked when the new baseline writes a file through a git filter driver (Git LFS), which the skills repo doesn't support.

**Behind**:
The machine hasn't picked up the latest baseline yet.

**Drafting**:
The machine has a draft.

**In sync**:
The machine is on the latest baseline, with no draft and no conflicts.
