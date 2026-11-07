# 0xSk1ll

Manages agent skills and instructions in one git repo and keeps every harness on every machine in step with it.

## Content

**Skill**:
A `SKILL.md` folder, either personal or imported.

**Import**:
A skill vendored from an external git source and pinned to a commit; updated from its source rather than edited (fork it to edit).
_Avoid_: Dependency, package, remote skill

**Fragment**:
One instruction file ("for X do Y") that is rendered into a harness's global instructions.
_Avoid_: Rule, instruction (when meaning a single file)

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
Which skills and fragments are enabled in which harness on which machine. It is live fleet state held by the hub, not part of the repo, so it has no draft.
_Avoid_: Profile, selection, enablement

**Hub**:
The self-hosted server that holds the fleet's assignments and status and moves content between machines.
_Avoid_: Relay, server

## State

**Baseline**:
The committed, pushed state of the repo; the same for every machine.
_Avoid_: Git state, main

**Draft**:
Uncommitted changes to the repo on one machine; private to that machine and live in its harnesses.
_Avoid_: System state, local state, working copy

**Promote**:
To turn (part of) a draft into baseline, so that it reaches the whole fleet.
_Avoid_: Commit, publish

**Reset**:
To discard (part of) a draft and return to baseline.
_Avoid_: Revert, discard

**Conflict**:
Content in a harness that differs from what the machine's draft says it should be, and that sk1ll won't overwrite on its own.
_Avoid_: Drift

**Daemon**:
The background process on each machine that keeps its harnesses matching its draft and assignment, and talks to the hub for it.
_Avoid_: Agent (reserved for the AI tools that harnesses run)

**Join code**:
A single-use code that lets a new machine enter the fleet and receive its own credential.
_Avoid_: Invite, fleet token

## Machine status

A machine has one headline status: the most severe that applies, in the order Offline, Blocked, Conflict, Behind, Drafting, In sync. **Conflict** is defined above.

**Offline**:
The hub can't reach the machine; it shows a last-seen time.

**Blocked**:
A new baseline clashes with the machine's draft, so the machine stays on its old baseline.

**Behind**:
The machine hasn't picked up the latest baseline yet.

**Drafting**:
The machine has a draft.

**In sync**:
The machine is on the latest baseline, with no draft and no conflicts.
