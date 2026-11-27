# Harnesses have kinds, and presets have instances

Status: accepted. Preset instances, kinds and matching Rules by kind are
implemented; finding instances from environment variables is not yet.

Some tools run with another config directory, for a second account:
Claude Code with `CLAUDE_CONFIG_DIR`, Codex with `CODEX_HOME`, Copilot with
`COPILOT_HOME`. Each built-in preset had one fixed home, so a second directory
could only be a custom harness with paths copied by hand. That harness missed
the Rules routed to the tool (`harnesses: [claude]` compared names), pinned
paths the preset's defaults could no longer change, and showed as custom.

A preset's paths are relative to its home. A `machine.toml` entry with
`preset = "<preset>"` and `home = "<dir>"` is an instance of that preset: it
gets the preset's settings under `home`, derived when the file is read, and
any key it sets overrides them as on the preset's own entry. `home` alone on
a preset's entry moves that preset. Every effective Harness has a **kind**:
the preset it is or is an instance of, none for a custom one. A Rule's
`harnesses:` matches a Harness's name or its kind, with no syntax of its own.

## Considered Options

- Custom harnesses with a `kind` key: the kind would match Rules, but the
  paths would still be copied, and pinned.
- A `harnesses: [kind:claude]` syntax for Rules: explicit, but every Rule
  routed to a tool would need rewriting to reach its second directory, and
  a tool named once in a Rule is meant whichever directory it runs from.
- Detecting instances by globbing for `~/.claude*`: such directories are
  often backups or other tools' data, and syncing into them unasked breaks
  the rule that an upgrade never starts writing somewhere new.

## Consequences

- An instance is synced into because its entry exists, an opt-in preset's
  too: the entry is the opt-in. A gemini instance includes gemini's opt-in
  skills directory unless it sets `skills = ""`.
- `preset` must name a preset, and only its own on a preset's entry; `home`
  needs a preset. Both are checked when `machine.toml` is read. `home`
  follows the path rules of the other paths, and the overlap checks see an
  instance's derived locations, so two instances can't share a directory.
- Renaming a preset or an instance keeps its kind: the new entry is
  `preset = "<kind>"` (with `home` when it isn't the preset's), so the Rules
  that reached it still do, and the rename doesn't report them.
- A Machine reports each Harness with its kind, and the local API's Harness
  carries `kind` and `home`. The wire format changed without a protocol
  bump, while sk1ll is pre-stable and has no other users.
- Instances are found by hand (`sk1ll harnesses add --preset --home`) and,
  later, from the environment variables above where the daemon sees them;
  never by globbing directories.
